//! Integration tests for `batman-xtask package-set`: the together-checks no
//! single `package` invocation can make.
//!
//! Every test asserts on the *specific* error text, so a test cannot pass by
//! failing for the wrong reason -- which for a release gate is the difference
//! between refusing a bad set and refusing every set.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const XTASK: &str = env!("CARGO_BIN_EXE_batman-xtask");

/// The real workspace root, so fixtures reuse the shipped `targets.json`.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask is nested two directories below the workspace root")
        .to_path_buf()
}

fn targets() -> Vec<String> {
    let raw = fs::read_to_string(workspace_root().join("release/targets.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    parsed["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["leaf"].as_str().unwrap().to_string())
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// A workspace root whose `packages/extension/package.json` declares
/// `version`, plus the real `release/targets.json`.
fn fixture_root(version: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let extension_dir = root.path().join("packages/extension");
    fs::create_dir_all(&extension_dir).unwrap();
    fs::write(
        extension_dir.join("package.json"),
        format!(r#"{{"name": "@nikolasd/crew", "version": "{version}"}}"#),
    )
    .unwrap();
    let release_dir = root.path().join("release");
    fs::create_dir_all(&release_dir).unwrap();
    fs::copy(
        workspace_root().join("release/targets.json"),
        release_dir.join("targets.json"),
    )
    .unwrap();
    root
}

/// Options for corrupting exactly one property of one leaf.
#[derive(Default)]
struct Corrupt {
    /// Omit this target's directory entirely.
    missing: Option<String>,
    /// Give this target a different `version`.
    wrong_version: Option<String>,
    /// Name the binary something other than `crewd`.
    wrong_binary_name: Option<String>,
    /// Drop the executable bit.
    not_executable: Option<String>,
    /// Give this target a different `schemaFingerprint`.
    wrong_fingerprint: Option<String>,
    /// Leave the manifest checksum describing different bytes.
    bad_checksum: Option<String>,
}

/// Assembles a full set of leaf packages under `<dir>/crew-<target>/`.
fn assemble(version: &str, corrupt: &Corrupt) -> tempfile::TempDir {
    let input = tempfile::tempdir().unwrap();
    for target in targets() {
        if corrupt.missing.as_deref() == Some(target.as_str()) {
            continue;
        }
        let leaf = input.path().join(format!("crew-{target}"));
        let bin_dir = leaf.join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let bytes = format!("crewd-for-{target}").into_bytes();
        let bin_name = if corrupt.wrong_binary_name.as_deref() == Some(target.as_str()) {
            "crewd-renamed"
        } else {
            "crewd"
        };
        let bin_path = bin_dir.join(bin_name);
        fs::write(&bin_path, &bytes).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if corrupt.not_executable.as_deref() == Some(target.as_str()) {
                0o644
            } else {
                0o755
            };
            fs::set_permissions(&bin_path, fs::Permissions::from_mode(mode)).unwrap();
        }

        let declared_sha = if corrupt.bad_checksum.as_deref() == Some(target.as_str()) {
            "0".repeat(64)
        } else {
            sha256_hex(&bytes)
        };
        let declared_version = if corrupt.wrong_version.as_deref() == Some(target.as_str()) {
            "9.9.9".to_string()
        } else {
            version.to_string()
        };
        let fingerprint = if corrupt.wrong_fingerprint.as_deref() == Some(target.as_str()) {
            "f".repeat(64)
        } else {
            "a".repeat(64)
        };

        let manifest = serde_json::json!({
            "name": format!("@nikolasd/crew-{target}"),
            "version": declared_version,
            "target": target,
            "sha256": declared_sha,
            "sizeBytes": bytes.len(),
            "rustVersion": "rustc 1.97.1 (fixture)",
            "sourceCommit": "0123456789abcdef0123456789abcdef01234567",
            "protocolRange": "1.0-1.0",
            "schemaFingerprint": fingerprint,
            "os": "macos-latest",
            "cpu": "arm64",
            "buildTimestamp": "1970-01-01T00:00:00Z",
        });
        fs::write(
            leaf.join("manifest.json"),
            format!("{}\n", serde_json::to_string_pretty(&manifest).unwrap()),
        )
        .unwrap();
    }
    input
}

/// Runs `package-set` and returns `(success, combined output)`.
fn run_package_set(root: &Path, version: &str, input: &Path, output: &Path) -> (bool, String) {
    let out = Command::new(XTASK)
        .args(["package-set", "--version", version])
        .arg("--input")
        .arg(input)
        .arg("--output")
        .arg(output)
        .current_dir(root)
        .output()
        .expect("running package-set");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), combined)
}

/// `package-set` resolves the workspace root from its own compile-time path,
/// so these tests drive the REAL `release/targets.json` and the real
/// extension version. Reads it so the fixtures agree.
fn real_version() -> String {
    let raw = fs::read_to_string(workspace_root().join("packages/extension/package.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    parsed["version"].as_str().unwrap().to_string()
}

#[test]
fn a_complete_consistent_set_is_accepted_and_writes_the_release_manifest() {
    let version = real_version();
    let input = assemble(&version, &Corrupt::default());
    let output = tempfile::tempdir().unwrap();

    let (ok, out) = run_package_set(&workspace_root(), &version, input.path(), output.path());
    assert!(ok, "a valid set must be accepted: {out}");

    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(output.path().join("release-manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["version"].as_str(), Some(version.as_str()));
    assert_eq!(
        manifest["leaves"].as_array().map(Vec::len),
        Some(targets().len()),
        "every target must appear in the aggregate manifest"
    );
    // The aggregate carries the fields every leaf agreed on.
    assert_eq!(
        manifest["schemaFingerprint"].as_str(),
        Some("a".repeat(64).as_str())
    );
    assert_eq!(
        manifest["buildTimestamp"].as_str(),
        Some("1970-01-01T00:00:00Z")
    );
}

#[test]
fn a_missing_target_is_rejected_by_name() {
    let version = real_version();
    let missing = targets().last().unwrap().clone();
    let input = assemble(
        &version,
        &Corrupt {
            missing: Some(missing.clone()),
            ..Default::default()
        },
    );
    let output = tempfile::tempdir().unwrap();

    let (ok, out) = run_package_set(&workspace_root(), &version, input.path(), output.path());
    assert!(!ok, "a set missing a target must be refused");
    assert!(
        out.contains(&missing),
        "the error must name the missing target: {out}"
    );
    assert!(
        out.contains("missing from the assembled set"),
        "unexpected error: {out}"
    );
}

#[test]
fn a_version_mismatch_between_the_flag_and_the_extension_is_rejected() {
    let version = real_version();
    let input = assemble(&version, &Corrupt::default());
    let output = tempfile::tempdir().unwrap();

    let (ok, out) = run_package_set(&workspace_root(), "9.9.9", input.path(), output.path());
    assert!(
        !ok,
        "a --version that disagrees with the extension must be refused"
    );
    assert!(
        out.contains("does not match packages/extension/package.json"),
        "unexpected error: {out}"
    );
}

#[test]
fn a_leaf_declaring_a_different_version_is_rejected() {
    let version = real_version();
    let odd = targets()[1].clone();
    let input = assemble(
        &version,
        &Corrupt {
            wrong_version: Some(odd.clone()),
            ..Default::default()
        },
    );
    let output = tempfile::tempdir().unwrap();

    let (ok, out) = run_package_set(&workspace_root(), &version, input.path(), output.path());
    assert!(!ok, "a leaf from another release must be refused");
    assert!(
        out.contains(&odd),
        "the error must name the offending leaf: {out}"
    );
    assert!(out.contains("declares version"), "unexpected error: {out}");
}

#[test]
fn a_binary_with_the_wrong_name_is_rejected() {
    let version = real_version();
    let odd = targets()[0].clone();
    let input = assemble(
        &version,
        &Corrupt {
            wrong_binary_name: Some(odd.clone()),
            ..Default::default()
        },
    );
    let output = tempfile::tempdir().unwrap();

    let (ok, out) = run_package_set(&workspace_root(), &version, input.path(), output.path());
    assert!(
        !ok,
        "the loader only resolves bin/crewd, so any other name must be refused"
    );
    assert!(out.contains("no bin/crewd"), "unexpected error: {out}");
}

#[cfg(unix)]
#[test]
fn a_binary_without_the_executable_bit_is_rejected() {
    let version = real_version();
    let odd = targets()[2].clone();
    let input = assemble(
        &version,
        &Corrupt {
            not_executable: Some(odd.clone()),
            ..Default::default()
        },
    );
    let output = tempfile::tempdir().unwrap();

    let (ok, out) = run_package_set(&workspace_root(), &version, input.path(), output.path());
    assert!(
        !ok,
        "a non-executable crewd would fail at launch, not at package time"
    );
    assert!(out.contains("not executable"), "unexpected error: {out}");
}

#[test]
fn a_schema_fingerprint_mismatch_across_leaves_is_rejected() {
    let version = real_version();
    let odd = targets()[3].clone();
    let input = assemble(
        &version,
        &Corrupt {
            wrong_fingerprint: Some(odd.clone()),
            ..Default::default()
        },
    );
    let output = tempfile::tempdir().unwrap();

    let (ok, out) = run_package_set(&workspace_root(), &version, input.path(), output.path());
    assert!(
        !ok,
        "leaves built against different protocol surfaces must not ship together"
    );
    assert!(
        out.contains("schema fingerprint mismatch"),
        "unexpected error: {out}"
    );
}

#[test]
fn a_checksum_that_does_not_match_the_binary_is_rejected() {
    let version = real_version();
    let odd = targets()[0].clone();
    let input = assemble(
        &version,
        &Corrupt {
            bad_checksum: Some(odd.clone()),
            ..Default::default()
        },
    );
    let output = tempfile::tempdir().unwrap();

    let (ok, out) = run_package_set(&workspace_root(), &version, input.path(), output.path());
    assert!(!ok, "a substituted binary must be refused");
    assert!(out.contains("checksum mismatch"), "unexpected error: {out}");
}

#[test]
fn windows_and_musl_are_rejected_by_absence_from_targets_json() {
    // `package` (single leaf) is where a target name is validated. Windows and
    // musl are unsupported by not appearing in release/targets.json at all,
    // and the error names the supported set rather than guessing a fallback.
    let root = fixture_root(&real_version());
    let binary = root.path().join("crewd-built");
    fs::write(&binary, b"bytes").unwrap();

    for unsupported in ["windows-x64", "linux-x64-musl", "linux-arm64-musl"] {
        let out = Command::new(XTASK)
            .args(["package", "--target", unsupported])
            .arg("--binary")
            .arg(&binary)
            .current_dir(root.path())
            .output()
            .expect("running package");
        let combined = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(!out.status.success(), "{unsupported} must be refused");
        assert!(
            combined.contains("unsupported target"),
            "the error must name the condition for {unsupported}: {combined}"
        );
        for supported in targets() {
            assert!(
                combined.contains(&supported),
                "the error must list supported target {supported}: {combined}"
            );
        }
    }
}
