//! Build tooling for the `crew` workspace.
//!
//! `cargo run -p batman-xtask -- generate` regenerates the canonical JSON
//! Schema document and TypeScript bindings from `batman-protocol`, the sole
//! source of truth for every Crew wire type. `--check` verifies the
//! committed outputs are up to date and that Rust crate versions match the
//! npm source of truth, without modifying anything.
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use batman_protocol::{
    ApplyRequest, ApplyResult, ApprovalId, Artifact, ArtifactFetchRequest, ArtifactFetchResult,
    ArtifactId, ArtifactKind, ArtifactListRequest, ArtifactListResult, BatmanMethod, BinarySource,
    Classified, ClientAuth, ClientCapabilities, ClientInfo, ClientPrincipalSummary, ClientRole,
    ContentClass, DiagnosticLevel, DisplayBackend, DisplayConfig, DisplayStatus, EventEnvelope,
    EventSource, InitializeParams, InitializeResult, InspectRequest, InspectResult, JsonRpcError,
    JsonRpcErrorResponse, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LeaseRequest,
    MessageId, MessageKind, OperationId, PolicyViolationListResult, PolicyViolationSummary,
    ProjectId, ProtocolVersion, ReleaseRequest, RepositoryIdentity, RequestId, RunId,
    RunResultResult, RunUsage, RuntimeCapabilities, RuntimeEvent, RuntimeInfo, RuntimeStatus,
    TaskId, Timestamp, VersionRange, WorkerId, WorkspaceInfo,
};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ts_rs::TS;

#[derive(clap::Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate the canonical JSON Schema document and TypeScript bindings
    /// from `batman-protocol`.
    Generate {
        /// Verify the committed outputs are up to date without modifying
        /// them. Exits non-zero if generation would produce different
        /// output.
        #[arg(long)]
        check: bool,
    },
    /// Assemble a platform leaf package: copy `--binary` into the matching
    /// `packages/crew-<target>/bin/crewd` and emit its `manifest.json`.
    Package {
        /// One of the four supported target triples: `darwin-arm64`,
        /// `darwin-x64`, `linux-arm64-gnu`, `linux-x64-gnu`.
        #[arg(long)]
        target: String,
        /// Path to the built `crewd` binary to package.
        #[arg(long)]
        binary: PathBuf,
    },
    /// Validate an assembled set of leaf packages together and emit one
    /// aggregate `release-manifest.json`.
    PackageSet {
        /// The release version every leaf must declare.
        #[arg(long)]
        version: String,
        /// Directory containing one `crew-<target>/` per supported target.
        #[arg(long)]
        input: PathBuf,
        /// Directory to write `release-manifest.json` into.
        #[arg(long)]
        output: PathBuf,
    },
}

/// One entry of `release/targets.json`, the single source of truth for the
/// targets this workspace ships. Previously duplicated between a constant
/// here and `release.yml`'s build matrix; both now read the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TargetEntry {
    leaf: String,
    rust: String,
    os: String,
    cpu: String,
    libc: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TargetsFile {
    targets: Vec<TargetEntry>,
}

/// Reads `release/targets.json`. Windows and musl stay unsupported by
/// absence: a target missing from this file is rejected, never inferred.
fn read_targets(root: &Path) -> Result<Vec<TargetEntry>> {
    let path = root.join("release/targets.json");
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: TargetsFile =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if parsed.targets.is_empty() {
        bail!("{} lists no targets", path.display());
    }
    Ok(parsed.targets)
}

/// Resolves one target by its leaf name, or fails naming the supported set.
fn find_target(targets: &[TargetEntry], leaf: &str) -> Result<TargetEntry> {
    targets
        .iter()
        .find(|t| t.leaf == leaf)
        .cloned()
        .with_context(|| {
            format!(
                "unsupported target {leaf:?}; supported targets are: {}",
                targets
                    .iter()
                    .map(|t| t.leaf.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// The deterministic checksum/provenance payload written to each leaf
/// package's `manifest.json`. Field order here is the JSON key order: serde
/// serializes struct fields in declaration order, so this is stable across
/// runs without needing a `preserve_order` feature. Every field is derived
/// from the source tree or the binary itself -- never from wall-clock time
/// -- so packaging the same binary twice produces byte-identical output.
/// Unsigned: the release plan signs this payload later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LeafManifest {
    name: String,
    version: String,
    target: String,
    sha256: String,
    #[serde(rename = "sizeBytes")]
    size_bytes: u64,
    /// `rustc --version`, trimmed.
    #[serde(rename = "rustVersion")]
    rust_version: String,
    /// `git rev-parse HEAD`.
    #[serde(rename = "sourceCommit")]
    source_commit: String,
    /// The wire protocol range this build speaks, e.g. `"1.0-1.0"`.
    #[serde(rename = "protocolRange")]
    protocol_range: String,
    /// SHA-256 of the committed schema document, so a leaf can be matched
    /// to the exact protocol surface it was generated against.
    #[serde(rename = "schemaFingerprint")]
    schema_fingerprint: String,
    /// From `release/targets.json`.
    os: String,
    /// From `release/targets.json`.
    cpu: String,
    /// `SOURCE_DATE_EPOCH` as RFC3339. **Never** `now()`: this struct
    /// promises byte-identical output for the same binary, and a wall-clock
    /// value would break that. When the variable is unset this is the Unix
    /// epoch (`1970-01-01T00:00:00Z`), which is a deliberate, reproducible
    /// placeholder rather than a build time.
    #[serde(rename = "buildTimestamp")]
    build_timestamp: String,
}

fn main() -> Result<()> {
    let args = <Args as clap::Parser>::parse();
    match args.command {
        Command::Generate { check } => run_generate(check),
        Command::Package { target, binary } => package_leaf(&workspace_root(), &target, &binary),
        Command::PackageSet {
            version,
            input,
            output,
        } => package_set(&workspace_root(), &version, &input, &output),
    }
}

/// The workspace root, resolved from the location of this crate at compile
/// time so generation behaves the same regardless of the process's current
/// directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask is nested two directories below the workspace root")
        .to_path_buf()
}

/// Renders the canonical protocol schema. The renderer lives in
/// `batman-protocol` because `crewd doctor`'s `schema_compatibility`
/// check compares against the same bytes; a second copy here would let the
/// two drift.
fn render_schema() -> Result<Vec<u8>> {
    batman_protocol::render_schema().context("serializing schema to JSON")
}

/// Exports TypeScript bindings for the explicit allowlist of wire types
/// below, alongside all of their transitive dependencies. This list — not
/// `#[ts(export)]` — decides what is generated: a type carrying the derive
/// but absent from this list (and unreferenced by anything in it) emits
/// nothing (R60's root cause). Idempotent and order independent: `ts-rs`
/// merges declarations into their target files sorted by type name
/// regardless of call order.
fn export_bindings(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

    macro_rules! export {
        ($($ty:ty),+ $(,)?) => {
            $(<$ty as TS>::export_all_to(dir).with_context(|| {
                format!("exporting {} bindings to {}", stringify!($ty), dir.display())
            })?;)+
        };
    }

    export!(
        ApprovalId,
        ArtifactId,
        MessageId,
        OperationId,
        ProjectId,
        RunId,
        RunResultResult,
        RunUsage,
        TaskId,
        WorkerId,
        BatmanMethod,
        BinarySource,
        ClientAuth,
        ClientCapabilities,
        ClientInfo,
        ClientPrincipalSummary,
        ClientRole,
        InitializeParams,
        InitializeResult,
        JsonRpcError,
        JsonRpcErrorResponse,
        JsonRpcNotification<ts_rs::Dummy>,
        JsonRpcRequest<ts_rs::Dummy>,
        JsonRpcResponse<ts_rs::Dummy>,
        RepositoryIdentity,
        RequestId,
        RuntimeCapabilities,
        RuntimeInfo,
        RuntimeStatus,
        ProtocolVersion,
        VersionRange,
        Classified<ts_rs::Dummy>,
        ContentClass,
        DiagnosticLevel,
        EventEnvelope,
        EventSource,
        RuntimeEvent,
        Timestamp,
        DisplayBackend,
        DisplayConfig,
        DisplayStatus,
        Artifact,
        ArtifactKind,
        ArtifactListRequest,
        ArtifactListResult,
        ArtifactFetchRequest,
        ArtifactFetchResult,
        InspectResult,
        ApplyResult,
        LeaseRequest,
        InspectRequest,
        ApplyRequest,
        ReleaseRequest,
        WorkspaceInfo,
        PolicyViolationSummary,
        PolicyViolationListResult,
        MessageKind,
    );

    Ok(())
}

/// Removes every `*.ts` file directly inside `dir`, so that types renamed or
/// removed from `batman-protocol` don't leave stale bindings behind.
/// `dir` is treated as fully owned by the generator.
fn clear_ts_files(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "ts") {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

/// Returns the sorted set of `*.ts` file names directly inside `dir`.
fn sorted_ts_file_names(dir: &Path) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    if dir.exists() {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "ts") {
                names.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    Ok(names.into_iter().collect())
}

/// Byte-compares two files, producing a clear error naming the drifted file.
fn compare_files(fresh: &Path, committed: &Path, what: &str) -> Result<()> {
    let fresh_bytes = fs::read(fresh)
        .with_context(|| format!("reading freshly generated {}", fresh.display()))?;
    let committed_bytes = fs::read(committed).with_context(|| {
        format!(
            "reading committed {what} at {} (has `bun run generate` been run and committed?)",
            committed.display()
        )
    })?;
    if fresh_bytes != committed_bytes {
        bail!(
            "generated output drift detected: committed {what} at {} does not match freshly \
             generated output; run `bun run generate` and commit the result",
            committed.display()
        );
    }
    Ok(())
}

/// Byte-compares every generated `*.ts` file in `fresh_dir` against
/// `committed_dir`, requiring the exact same set of files.
fn compare_dirs(fresh_dir: &Path, committed_dir: &Path) -> Result<()> {
    let fresh_names = sorted_ts_file_names(fresh_dir)?;
    let committed_names = sorted_ts_file_names(committed_dir)?;

    if fresh_names != committed_names {
        bail!(
            "generated output drift detected: committed {} contains {:?}, but generation now \
             produces {:?}; run `bun run generate` and commit the result",
            committed_dir.display(),
            committed_names,
            fresh_names,
        );
    }

    for name in &fresh_names {
        compare_files(
            &fresh_dir.join(name),
            &committed_dir.join(name),
            "TypeScript binding",
        )?;
    }

    Ok(())
}

/// Verifies the protocol-ts barrel (`src/index.ts`) re-exports every
/// generated binding. R17 made "every generated file is re-exported" the
/// barrel's contract; this turns that convention into a build failure,
/// so a type that exists on the wire is always importable from the barrel.
fn check_barrel_completeness(root: &Path, generated_dir: &Path) -> Result<()> {
    let barrel_path = root.join("packages/protocol-ts/src/index.ts");
    let barrel = fs::read_to_string(&barrel_path)
        .with_context(|| format!("reading {}", barrel_path.display()))?;
    let mut missing = Vec::new();
    for name in sorted_ts_file_names(generated_dir)? {
        let type_name = name.trim_end_matches(".ts");
        let expected = format!("export type * from \"./generated/{type_name}\";");
        if !barrel.contains(&expected) {
            missing.push(type_name.to_string());
        }
    }
    if !missing.is_empty() {
        bail!(
            "barrel drift: {} is missing re-exports for generated bindings {:?}; \
             add `export type * from \"./generated/<Name>\";` for each",
            barrel_path.display(),
            missing,
        );
    }
    Ok(())
}

fn run_generate(check: bool) -> Result<()> {
    let root = workspace_root();
    let schema_path = root.join("packages/protocol-ts/schema/batman.schema.json");
    let generated_dir = root.join("packages/protocol-ts/src/generated");

    let schema_bytes = render_schema()?;

    if check {
        let temp = tempfile::tempdir().context("creating temporary directory for --check")?;

        let temp_schema_path = temp.path().join("batman.schema.json");
        fs::write(&temp_schema_path, &schema_bytes)
            .with_context(|| format!("writing {}", temp_schema_path.display()))?;

        let temp_generated_dir = temp.path().join("generated");
        export_bindings(&temp_generated_dir)?;

        compare_files(&temp_schema_path, &schema_path, "schema")?;
        compare_dirs(&temp_generated_dir, &generated_dir)?;
        check_barrel_completeness(&root, &temp_generated_dir)?;

        // Verify Rust crate versions match the npm source of truth.
        // If they drift, `crewd --version` (which reports CARGO_PKG_VERSION
        // from the runtime crate) would disagree with the npm package version,
        // and the leaf manifest version (read from the extension package.json)
        // would lie about what the binary actually reports.
        check_version_coherence(&root)?;

        println!(
            "generate --check: schema, TypeScript bindings, and version coherence are up to date"
        );
        return Ok(());
    }

    if let Some(parent) = schema_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&schema_path, &schema_bytes)
        .with_context(|| format!("writing {}", schema_path.display()))?;

    fs::create_dir_all(&generated_dir)
        .with_context(|| format!("creating {}", generated_dir.display()))?;
    clear_ts_files(&generated_dir)?;
    export_bindings(&generated_dir)?;

    println!(
        "generate: wrote {} and TypeScript bindings to {}",
        schema_path.display(),
        generated_dir.display()
    );
    Ok(())
}

/// This leaf package's `name` field for a given target triple, e.g.
/// `@nikolasd/crew-darwin-arm64` for `darwin-arm64`.
fn leaf_package_name(target: &str) -> String {
    format!("@nikolasd/crew-{target}")
}

/// The leaf package directory for a given target triple, rooted at
/// `packages/crew-<target>` under the workspace root.
fn leaf_package_dir(root: &Path, target: &str) -> PathBuf {
    root.join("packages").join(format!("crew-{target}"))
}

/// Reads the `version` field out of `packages/extension/package.json`; every
/// leaf manifest's `version` must equal it so `resolveCrewd` (the
/// TypeScript loader) can require an exact match before running a packaged
/// binary.
fn read_extension_version(root: &Path) -> Result<String> {
    let package_json_path = root.join("packages/extension/package.json");
    let raw = fs::read_to_string(&package_json_path)
        .with_context(|| format!("reading {}", package_json_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", package_json_path.display()))?;
    value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .with_context(|| {
            format!(
                "{} has no string `version` field",
                package_json_path.display()
            )
        })
}

/// Reads the `version` field out of a `Cargo.toml` file.
///
/// Minimal parser: scans for `version = "..."` in the file. No `toml` crate
/// dependency — Cargo.toml version lines are simple and stable.
fn read_cargo_version(path: &Path) -> Result<String> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("version =") {
            let value = trimmed
                .strip_prefix("version =")
                .and_then(|v| v.trim().strip_prefix('"'))
                .and_then(|v| v.strip_suffix('"'))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}: malformed version line: {trimmed:?}; expected `version = \"x.y.z\"`",
                        path.display()
                    )
                })?;
            return Ok(value.to_string());
        }
    }
    bail!("{} has no `version` field", path.display())
}

/// Verifies every Rust crate's version matches the npm source of truth
/// (`packages/extension/package.json`).
///
/// If a Rust crate drifts, `crewd --version` (which reports `CARGO_PKG_VERSION`
/// from the runtime crate) would disagree with the npm package version, and the
/// leaf manifest version (read from the extension package.json, not the binary)
/// would lie about what the shipped binary actually reports.
fn check_version_coherence(root: &Path) -> Result<()> {
    let expected = read_extension_version(root)?;

    // The runtime crate is critical: `crewd --version` reports its
    // `CARGO_PKG_VERSION`, so a drift here means the binary lies about
    // its version relative to the npm package that ships it.
    let runtime_path = root.join("crates/runtime/Cargo.toml");
    let runtime_version = read_cargo_version(&runtime_path)?;
    if runtime_version != expected {
        bail!(
            "version drift: crates/runtime/Cargo.toml declares version {:?} but \
             packages/extension/package.json declares {:?}; \
             `crewd --version` would report {:?} while the npm package ships as {:?}; \
             update crates/runtime/Cargo.toml to match",
            runtime_version,
            expected,
            runtime_version,
            expected,
        );
    }

    // The protocol crate's version surfaces in `clientInfo.version` when
    // the Codex adapter reports back to the vendor CLI.
    let protocol_path = root.join("crates/protocol/Cargo.toml");
    let protocol_version = read_cargo_version(&protocol_path)?;
    if protocol_version != expected {
        bail!(
            "version drift: crates/protocol/Cargo.toml declares version {:?} but \
             packages/extension/package.json declares {:?}; \
             update crates/protocol/Cargo.toml to match",
            protocol_version,
            expected,
        );
    }

    // The OMP marketplace catalog is what users actually install from; a
    // stale version here ships silently because nothing else reads it.
    // Both `metadata.version` and the `crew` plugin entry's `version`
    // must equal the extension version (R64).
    let marketplace_path = root.join(".claude-plugin/marketplace.json");
    let raw = fs::read_to_string(&marketplace_path)
        .with_context(|| format!("reading {}", marketplace_path.display()))?;
    let marketplace: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", marketplace_path.display()))?;
    let metadata_version = marketplace
        .pointer("/metadata/version")
        .and_then(serde_json::Value::as_str)
        .with_context(|| {
            format!(
                "{} has no string `metadata.version` field",
                marketplace_path.display()
            )
        })?;
    if metadata_version != expected {
        bail!(
            "version drift: {} declares metadata.version {:?} but \
             packages/extension/package.json declares {:?}; \
             update .claude-plugin/marketplace.json to match",
            marketplace_path.display(),
            metadata_version,
            expected,
        );
    }
    let plugin_version = marketplace
        .pointer("/plugins")
        .and_then(serde_json::Value::as_array)
        .and_then(|plugins| {
            plugins
                .iter()
                .find(|p| p.get("name").and_then(serde_json::Value::as_str) == Some("crew"))
        })
        .and_then(|p| p.get("version"))
        .and_then(serde_json::Value::as_str)
        .with_context(|| {
            format!(
                "{} has no `crew` plugin entry with a string `version` field",
                marketplace_path.display()
            )
        })?;
    if plugin_version != expected {
        bail!(
            "version drift: {}'s `crew` plugin entry declares version {:?} but \
             packages/extension/package.json declares {:?}; \
             update .claude-plugin/marketplace.json to match",
            marketplace_path.display(),
            plugin_version,
            expected,
        );
    }

    Ok(())
}

/// Copies `binary` into the leaf package directory matching `target` as
/// `bin/crewd` (mode `0755` on Unix) and writes its deterministic
/// `manifest.json` (SHA-256 + size + target + version provenance).
///
/// `root` is the workspace root, parameterized so this is independently
/// testable against a temporary fixture root rather than the real workspace.
fn package_leaf(root: &Path, target: &str, binary: &Path) -> Result<()> {
    let targets = read_targets(root)?;
    let entry = find_target(&targets, target)?;

    let leaf_dir = leaf_package_dir(root, target);
    fs::create_dir_all(&leaf_dir).with_context(|| format!("creating {}", leaf_dir.display()))?;

    let bin_dir = leaf_dir.join("bin");
    fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    let bin_path = bin_dir.join("crewd");

    let bytes =
        fs::read(binary).with_context(|| format!("reading binary at {}", binary.display()))?;
    fs::write(&bin_path, &bytes).with_context(|| format!("writing {}", bin_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("setting permissions on {}", bin_path.display()))?;
    }

    let manifest = LeafManifest {
        name: leaf_package_name(target),
        version: read_extension_version(root)?,
        target: target.to_string(),
        sha256: sha256_hex(&bytes),
        size_bytes: bytes.len() as u64,
        rust_version: rust_version()?,
        source_commit: source_commit(root)?,
        protocol_range: batman_protocol::supported_range_text(),
        schema_fingerprint: schema_fingerprint(root)?,
        os: entry.os,
        cpu: entry.cpu,
        build_timestamp: build_timestamp()?,
    };

    let mut manifest_json =
        serde_json::to_string_pretty(&manifest).context("serializing leaf manifest")?;
    manifest_json.push('\n');

    let manifest_path = leaf_dir.join("manifest.json");
    fs::write(&manifest_path, manifest_json.as_bytes())
        .with_context(|| format!("writing {}", manifest_path.display()))?;

    println!(
        "package: wrote {} and {}",
        bin_path.display(),
        manifest_path.display()
    );
    Ok(())
}

/// Hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Runs `program` with `args` and returns its trimmed stdout.
fn capture(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .with_context(|| format!("running {program} {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The compiler that produced this build, e.g. `rustc 1.97.1 (...)`.
fn rust_version() -> Result<String> {
    capture("rustc", &["--version"], None)
}

/// The exact source commit a leaf was built from.
fn source_commit(root: &Path) -> Result<String> {
    capture("git", &["rev-parse", "HEAD"], Some(root))
}

/// SHA-256 of the committed schema document. Ties a leaf to the protocol
/// surface it was generated against, so `package-set` can refuse a set
/// assembled from mismatched builds.
fn schema_fingerprint(root: &Path) -> Result<String> {
    let path = root.join("packages/protocol-ts/schema/batman.schema.json");
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

/// `SOURCE_DATE_EPOCH` rendered as RFC3339, or the Unix epoch when unset.
///
/// Never `now()`: a wall-clock value would make two packagings of the same
/// binary differ, which is the one property `LeafManifest` promises.
fn build_timestamp() -> Result<String> {
    let epoch: i64 = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(raw) => raw
            .trim()
            .parse()
            .with_context(|| format!("SOURCE_DATE_EPOCH is not an integer: {raw:?}"))?,
        Err(_) => 0,
    };
    // Formatted by hand rather than pulling in a date crate: the only input
    // is a Unix timestamp, and xtask has no other need for time handling.
    let days = epoch.div_euclid(86_400);
    let secs_of_day = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    ))
}

/// Days-since-epoch to `(year, month, day)`. Howard Hinnant's `civil_from_days`
/// algorithm, which is exact for the whole proleptic Gregorian range.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The aggregate manifest describing one complete release: the fields every
/// leaf must agree on, plus each leaf's own manifest verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseManifest {
    version: String,
    #[serde(rename = "sourceCommit")]
    source_commit: String,
    #[serde(rename = "schemaFingerprint")]
    schema_fingerprint: String,
    #[serde(rename = "buildTimestamp")]
    build_timestamp: String,
    leaves: Vec<LeafManifest>,
}

/// Validates an assembled set of leaf packages *together* -- the checks no
/// single `package` invocation can make -- and writes one aggregate
/// `release-manifest.json`.
///
/// Every failure is a distinct, specific error: a release that ships a
/// mismatched set is far worse than one that refuses to build.
fn package_set(root: &Path, version: &str, input: &Path, output: &Path) -> Result<()> {
    let targets = read_targets(root)?;
    let expected_version = read_extension_version(root)?;
    if version != expected_version {
        bail!(
            "--version {version:?} does not match packages/extension/package.json version \
             {expected_version:?}"
        );
    }

    let mut leaves: Vec<LeafManifest> = Vec::with_capacity(targets.len());
    for entry in &targets {
        let leaf_dir = input.join(format!("crew-{}", entry.leaf));
        if !leaf_dir.is_dir() {
            bail!(
                "target {:?} is missing from the assembled set: {} does not exist",
                entry.leaf,
                leaf_dir.display()
            );
        }

        let manifest_path = leaf_dir.join("manifest.json");
        let raw = fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let manifest: LeafManifest = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", manifest_path.display()))?;

        if manifest.version != version {
            bail!(
                "leaf {:?} declares version {:?} but the release is {version:?}",
                entry.leaf,
                manifest.version
            );
        }
        if manifest.target != entry.leaf {
            bail!(
                "leaf directory crew-{} contains a manifest for target {:?}",
                entry.leaf,
                manifest.target
            );
        }

        // The binary must be named `crewd` exactly: the TypeScript loader
        // resolves `<leaf>/bin/crewd` and nothing else.
        let bin_path = leaf_dir.join("bin").join("crewd");
        if !bin_path.is_file() {
            bail!(
                "leaf {:?} has no bin/crewd (found nothing at {})",
                entry.leaf,
                bin_path.display()
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&bin_path)
                .with_context(|| format!("reading metadata for {}", bin_path.display()))?
                .permissions()
                .mode();
            if mode & 0o111 == 0 {
                bail!(
                    "leaf {:?} bin/crewd is not executable (mode {:o})",
                    entry.leaf,
                    mode & 0o777
                );
            }
        }

        let bytes =
            fs::read(&bin_path).with_context(|| format!("reading {}", bin_path.display()))?;
        let actual = sha256_hex(&bytes);
        if actual != manifest.sha256 {
            bail!(
                "leaf {:?} checksum mismatch: manifest declares {} but bin/crewd hashes to {}",
                entry.leaf,
                manifest.sha256,
                actual
            );
        }

        leaves.push(manifest);
    }

    // Every leaf must have been generated against the same protocol surface.
    // A set spanning two schema fingerprints would ship binaries that
    // disagree about the wire format.
    let first = &leaves[0];
    for leaf in &leaves[1..] {
        if leaf.schema_fingerprint != first.schema_fingerprint {
            bail!(
                "schema fingerprint mismatch: leaf {:?} has {} but leaf {:?} has {}",
                first.target,
                first.schema_fingerprint,
                leaf.target,
                leaf.schema_fingerprint
            );
        }
    }

    let release = ReleaseManifest {
        version: version.to_string(),
        source_commit: first.source_commit.clone(),
        schema_fingerprint: first.schema_fingerprint.clone(),
        build_timestamp: first.build_timestamp.clone(),
        leaves,
    };

    fs::create_dir_all(output).with_context(|| format!("creating {}", output.display()))?;
    let mut json =
        serde_json::to_string_pretty(&release).context("serializing release manifest")?;
    json.push('\n');
    let path = output.join("release-manifest.json");
    fs::write(&path, json.as_bytes()).with_context(|| format!("writing {}", path.display()))?;

    println!(
        "package-set: validated {} leaves and wrote {}",
        release.leaves.len(),
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod package_tests {
    use super::*;

    /// Builds a fixture workspace root with everything `package_leaf` reads:
    /// `packages/extension/package.json` declaring `version`, an empty
    /// `packages/crew-<target>` leaf directory, a `release/targets.json`
    /// copied from the real one, a committed schema document to fingerprint,
    /// and an initialized git repository so `git rev-parse HEAD` resolves.
    fn fixture_root(version: &str, target: &str) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("creating fixture workspace root");

        let extension_dir = root.path().join("packages/extension");
        fs::create_dir_all(&extension_dir).expect("creating fixture extension dir");
        fs::write(
            extension_dir.join("package.json"),
            format!(r#"{{"name": "@nikolasd/crew", "version": "{version}"}}"#),
        )
        .expect("writing fixture extension package.json");

        let leaf_dir = root.path().join("packages").join(format!("crew-{target}"));
        fs::create_dir_all(&leaf_dir).expect("creating fixture leaf dir");

        // The real targets file, so a fixture can never drift from the
        // target set the workspace actually ships.
        let release_dir = root.path().join("release");
        fs::create_dir_all(&release_dir).expect("creating fixture release dir");
        let real_targets = workspace_root().join("release/targets.json");
        fs::copy(&real_targets, release_dir.join("targets.json"))
            .expect("copying release/targets.json into the fixture");

        let schema_dir = root.path().join("packages/protocol-ts/schema");
        fs::create_dir_all(&schema_dir).expect("creating fixture schema dir");
        fs::write(
            schema_dir.join("batman.schema.json"),
            b"{\"fixture\":true}\n",
        )
        .expect("writing fixture schema");

        // `source_commit` shells out to git, so the fixture needs a repo with
        // one commit. Committer identity is set locally so the test does not
        // depend on the machine's global git config.
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("running git in the fixture");
            assert!(status.success(), "git {args:?} failed in the fixture");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "fixture@test.invalid"]);
        git(&["config", "user.name", "Fixture"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "fixture", "--no-gpg-sign"]);

        root
    }

    #[test]
    fn package_leaf_rejects_unsupported_targets() {
        let root = fixture_root("0.1.0", "darwin-arm64");
        let binary = root.path().join("crewd-built");
        fs::write(&binary, b"binary-bytes").unwrap();

        let err = package_leaf(root.path(), "windows-x64", &binary).unwrap_err();
        assert!(err.to_string().contains("unsupported target"));
    }

    #[test]
    fn package_leaf_creates_missing_leaf_directory() {
        let target = "darwin-arm64";
        let root = fixture_root("0.1.0", target);
        let leaf_dir = leaf_package_dir(root.path(), target);
        fs::remove_dir_all(&leaf_dir).expect("removing fixture leaf dir to simulate a fresh clone");
        assert!(!leaf_dir.is_dir());

        let binary = root.path().join("crewd-built");
        fs::write(&binary, b"binary-bytes-for-missing-leaf-dir-test").unwrap();

        package_leaf(root.path(), target, &binary)
            .expect("package_leaf should create the missing leaf directory rather than bail");

        assert!(leaf_dir.is_dir());
        let bin_path = leaf_dir.join("bin").join("crewd");
        assert!(bin_path.is_file());
    }

    #[test]
    fn package_leaf_writes_binary_and_manifest() {
        let target = "darwin-arm64";
        let root = fixture_root("0.1.0", target);
        let binary = root.path().join("crewd-built");
        let bytes = b"pretend-this-is-a-crewd-binary";
        fs::write(&binary, bytes).unwrap();

        package_leaf(root.path(), target, &binary).expect("package_leaf should succeed");

        let leaf_dir = leaf_package_dir(root.path(), target);
        let bin_path = leaf_dir.join("bin").join("crewd");
        assert_eq!(fs::read(&bin_path).unwrap(), bytes);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&bin_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        let manifest_path = leaf_dir.join("manifest.json");
        let manifest_bytes = fs::read(&manifest_path).unwrap();
        assert!(manifest_bytes.ends_with(b"\n"));

        let manifest: LeafManifest = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest.name, "@nikolasd/crew-darwin-arm64");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.target, target);
        assert_eq!(manifest.size_bytes, bytes.len() as u64);

        let mut hasher = Sha256::new();
        hasher.update(bytes);
        assert_eq!(manifest.sha256, hex::encode(hasher.finalize()));
    }

    #[test]
    fn package_leaf_manifest_is_byte_identical_across_runs() {
        let target = "linux-x64-gnu";
        let root = fixture_root("0.1.0", target);
        let binary = root.path().join("crewd-built");
        fs::write(&binary, b"deterministic-fixture-bytes").unwrap();

        package_leaf(root.path(), target, &binary).unwrap();
        let manifest_path = leaf_package_dir(root.path(), target).join("manifest.json");
        let first = fs::read(&manifest_path).unwrap();

        package_leaf(root.path(), target, &binary).unwrap();
        let second = fs::read(&manifest_path).unwrap();

        assert_eq!(
            first, second,
            "packaging the same binary twice must be byte-identical"
        );
    }
}

#[cfg(test)]
mod version_coherence_tests {
    use super::*;

    /// Builds a fixture root with everything `check_version_coherence`
    /// reads: the extension package.json (the source of truth), both
    /// Cargo.tomls, and the marketplace catalog.
    fn coherence_fixture(
        extension: &str,
        runtime: &str,
        protocol: &str,
        metadata: &str,
        plugin: &str,
    ) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("creating fixture root");
        let write = |rel: &str, contents: String| {
            let path = root.path().join(rel);
            fs::create_dir_all(path.parent().unwrap()).expect("creating fixture dir");
            fs::write(&path, contents).expect("writing fixture file");
        };
        write(
            "packages/extension/package.json",
            format!(r#"{{"name": "@nikolasd/crew", "version": "{extension}"}}"#),
        );
        write(
            "crates/runtime/Cargo.toml",
            format!("[package]\nname = \"batman-runtime\"\nversion = \"{runtime}\"\n"),
        );
        write(
            "crates/protocol/Cargo.toml",
            format!("[package]\nname = \"batman-protocol\"\nversion = \"{protocol}\"\n"),
        );
        write(
            ".claude-plugin/marketplace.json",
            format!(
                r#"{{"name": "crew", "metadata": {{"version": "{metadata}"}}, "plugins": [{{"name": "crew", "version": "{plugin}"}}]}}"#
            ),
        );
        root
    }

    #[test]
    fn coherent_versions_pass() {
        let root = coherence_fixture("0.4.0", "0.4.0", "0.4.0", "0.4.0", "0.4.0");
        check_version_coherence(root.path()).expect("all-equal versions must pass");
    }

    #[test]
    fn marketplace_metadata_drift_fails_naming_the_file() {
        let root = coherence_fixture("0.4.0", "0.4.0", "0.4.0", "0.3.0", "0.4.0");
        let err = check_version_coherence(root.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("marketplace.json"), "error was: {err}");
        assert!(err.contains("metadata.version"), "error was: {err}");
    }

    #[test]
    fn marketplace_plugin_entry_drift_fails_naming_the_entry() {
        let root = coherence_fixture("0.4.0", "0.4.0", "0.4.0", "0.4.0", "0.3.0");
        let err = check_version_coherence(root.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("marketplace.json"), "error was: {err}");
        assert!(err.contains("plugin entry"), "error was: {err}");
    }
}
