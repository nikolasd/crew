//! The `crewd` command-line interface: `serve`, `status`, `stop`,
//! `version`, `schema`, `monitor`, `audit`, `doctor`, and
//! `coordination-mcp`. This layer only
//! parses arguments, resolves the state root (via [`StateRoot::resolve`],
//! the same precedence the OMP extension applies) when `--state-dir` is
//! omitted, and maps [`crate::lifecycle`] outcomes to process exit codes;
//! all behaviour lives in the library.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crew_runtime::{StateRoot, VERSION};
/// Selector for `crewd conformance --live`: which control plane to exercise
/// against the real vendor CLI. `Tui` is the default because every reserved
/// adapter now defaults to TUI mode; `Headless` reaches each adapter's own
/// kept headless live report (distinct `claude`-style labels).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ConformanceModeArg {
    Tui,
    Headless,
}
/// The committed fixture-mode baseline loaded from
/// `fixtures/conformance/fixture-mode-baseline.json`. Maps each adapter to
/// the scenario names that are expected to fail in fixture mode.
#[derive(serde::Deserialize)]
struct FixtureBaseline {
    #[serde(rename = "expectedFailures")]
    expected_failures: std::collections::HashMap<String, Vec<String>>,
}

/// The Crew runtime daemon.
#[derive(Parser)]
#[command(name = "crewd", version, about = "The Crew runtime daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the runtime socket protocol for a repository.
    Serve {
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository this runtime instance serves.
        #[arg(long)]
        repo: PathBuf,
        /// Exit after this many seconds with no connections and no active
        /// runs. Omit to run until signalled.
        #[arg(long)]
        idle_seconds: Option<u64>,
        /// Run in the foreground, logging structured records to stderr rather
        /// than to `runtime.log`.
        #[arg(long)]
        foreground: bool,
        /// A crew config layer file, lowest precedence first. Repeatable;
        /// a later occurrence's values win a field-by-field deep merge
        /// (`security.patterns` is additive instead). A path that does not
        /// exist is treated as an absent layer, not an error.
        #[arg(long = "config")]
        config: Vec<PathBuf>,
    },
    /// Print the runtime's `runtime/status` snapshot as JSON.
    Status {
        /// Retry connecting for up to this many seconds (startup races).
        #[arg(long)]
        wait_seconds: Option<u64>,
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository whose runtime to query.
        #[arg(long)]
        repo: PathBuf,
    },
    /// Gracefully stop the runtime serving a repository.
    Stop {
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository whose runtime to stop.
        #[arg(long)]
        repo: PathBuf,
    },
    /// Display runtime events for one or all runs.
    Monitor {
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository whose events to display.
        #[arg(long)]
        repo: PathBuf,
        /// Render only the run matching this id (full, un-truncated form).
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Print the runtime version.
    Version,
    /// Print the canonical JSON Schema document to stdout.
    Schema,
    /// Inspect or scaffold the crew.json configuration layers.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Audit commands for managing event retention and export.
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    /// Run diagnostic checks on the runtime state and configuration.
    Doctor {
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository to diagnose.
        #[arg(long)]
        repo: PathBuf,
        /// Output as JSON (machine-readable).
        #[arg(long)]
        json: bool,
        /// A crew config layer file, lowest precedence first. Repeatable;
        /// same semantics as `serve --config`.
        #[arg(long = "config")]
        config: Vec<PathBuf>,
    },
    /// Serve the worker-coordination MCP proxy for one run over stdio.
    CoordinationMcp {
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository this run belongs to.
        #[arg(long)]
        repo: PathBuf,
        /// The run this MCP proxy is scoped to.
        #[arg(long)]
        run_id: String,
    },
    /// Probe the display backend status.
    Display {
        #[command(subcommand)]
        probe: DisplayCommand,
    },
    /// Run conformance tests for one or all adapters.
    Conformance {
        /// Adapter name: claude, codex, copilot, ompRpc, or all.
        #[arg(long)]
        adapter: String,
        /// Use fixture mode (no real model calls).
        #[arg(long, default_value_t = false)]
        fixture: bool,
        /// Use live mode (real vendor CLI), gated per adapter.
        #[arg(long, default_value_t = false)]
        live: bool,
        /// Live mode only: which control plane to exercise. `tui` (default)
        /// spawns the real vendor binary on a PTY; `headless` runs each
        /// adapter's kept headless live report.
        #[arg(long, value_enum, default_value_t = ConformanceModeArg::Tui)]
        mode: ConformanceModeArg,
        /// Output file path for the conformance report.
        #[arg(long)]
        output: PathBuf,
    },
    /// List registered adapters with declared vs effective capabilities.
    Adapters {
        /// Output as JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Inspect or force-release workspace leases for a repository.
    Lease {
        #[command(subcommand)]
        command: LeaseCommand,
    },
    /// Attach to a running worker's pseudo-terminal.
    Attach {
        /// The run to attach to.
        run_id: String,
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository this run belongs to. Required unless --socket
        /// is given.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Connect directly to this socket instead of resolving one from
        /// --repo/--state-dir/<run-id> (mainly for tests).
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Write a starter crew.json (and its schema) for a user or a repository.
    Init {
        /// Write to `~/.omp/crew.json` instead of the repository layer.
        #[arg(long)]
        global: bool,
        /// The repository whose `.omp/crew.json` to write. Defaults to the
        /// current directory. Ignored with `--global`.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Overwrite an existing crew.json. Without this, an existing file
        /// is left untouched and the command fails.
        #[arg(long)]
        force: bool,
    },
    /// Print a configuration document to stdout. With no flag, prints the
    /// effective merged config -- what this repository is actually running.
    Print {
        /// The full built-in default snapshot (what `config init` writes).
        #[arg(long, conflicts_with_all = ["schema", "effective"])]
        defaults: bool,
        /// The JSON Schema editors validate and autocomplete crew.json from.
        #[arg(long, conflicts_with_all = ["defaults", "effective"])]
        schema: bool,
        /// The merged result of the layers that actually apply (the default).
        #[arg(long, conflicts_with_all = ["defaults", "schema"])]
        effective: bool,
        /// The repository whose layers `--effective` merges. Defaults to
        /// the current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// List the config layer files in precedence order and whether each exists.
    Path {
        /// The repository whose project layer to report. Defaults to the
        /// current directory.
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum LeaseCommand {
    /// Force-release a workspace lease by id.
    ///
    /// The operator remedy for a lease whose owning session correlation
    /// was never persisted (extension crashed before the recording
    /// upsert): such a lease is unreleasable over RPC, because
    /// `workspace/release` is owner-gated and a new session is a
    /// different principal. `crewd doctor` reports these as stale.
    Release {
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository whose lease to release.
        #[arg(long)]
        repo: PathBuf,
        /// The lease to release (full id, as `crewd doctor` prints it).
        #[arg(long)]
        lease_id: String,
        /// Confirm releasing a lease that is still `active` -- this strips
        /// a run's workspace claim.
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum AuditCommand {
    /// Export events to a JSONL file.
    Export {
        /// The Crew state root. Defaults to the resolved state root.
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The repository whose events to export.
        #[arg(long)]
        repo: PathBuf,
        /// Export events from this timestamp (ISO 8601).
        #[arg(long)]
        from: Option<String>,
        /// Export events up to this timestamp (ISO 8601).
        #[arg(long)]
        to: Option<String>,
        /// The output file path. Required -- the export always writes to
        /// this file, never to stdout.
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum DisplayCommand {
    /// Probe the display backend status.
    Probe {
        /// Backend to probe: herdr, tmux, osWindow, or hidden.
        backend: String,
        /// Output as JSON.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

/// The CLI's entry point, called from `main.rs`.
pub async fn run() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            state_dir,
            repo,
            idle_seconds,
            foreground,
            config,
        } => run_serve(state_dir, repo, idle_seconds, foreground, config).await,
        Command::Status {
            wait_seconds,
            state_dir,
            repo,
        } => run_status(wait_seconds, state_dir, repo).await,
        Command::Stop { state_dir, repo } => run_stop(state_dir, repo).await,
        Command::Monitor {
            state_dir,
            repo,
            run_id,
        } => run_monitor(state_dir, repo, run_id).await,
        Command::Version => {
            println!("crewd {VERSION}");
            ExitCode::SUCCESS
        }
        Command::Schema => run_schema().await,
        Command::Config { command } => run_config(command),
        Command::Audit {
            command:
                AuditCommand::Export {
                    state_dir,
                    repo,
                    from,
                    to,
                    output,
                },
        } => run_audit_export(state_dir, repo, from, to, output).await,
        Command::Lease {
            command:
                LeaseCommand::Release {
                    state_dir,
                    repo,
                    lease_id,
                    yes,
                },
        } => run_lease_release(state_dir, repo, lease_id, yes).await,
        Command::Doctor {
            state_dir,
            repo,
            json,
            config,
        } => run_doctor(state_dir, repo, json, config).await,
        Command::CoordinationMcp {
            state_dir,
            repo,
            run_id,
        } => run_coordination_mcp(state_dir, repo, run_id).await,
        Command::Display {
            probe: DisplayCommand::Probe { backend, json },
        } => run_display_probe(backend, json).await,
        Command::Conformance {
            adapter,
            fixture,
            live,
            mode,
            output,
        } => run_conformance(adapter, fixture, live, mode, output).await,
        Command::Adapters { json } => run_adapters(json).await,
        Command::Attach {
            run_id,
            state_dir,
            repo,
            socket,
        } => run_attach(run_id, state_dir, repo, socket).await,
    }
}

/// Reads the launcher's `CREW_BINARY_SOURCE` hint. The extension sets it
/// when it spawns the daemon (`packages/extension/src/runtime.ts`); a
/// hand-run `crewd` leaves it unset, which is `Unknown` rather than an
/// error -- the field is diagnostic, never load-bearing.
fn binary_source_from_env() -> crew_protocol::BinarySource {
    use crew_protocol::BinarySource;
    match std::env::var("CREW_BINARY_SOURCE").as_deref() {
        Ok("override") => BinarySource::Override,
        Ok("package") => BinarySource::Package,
        _ => BinarySource::Unknown,
    }
}

/// Runs `crewd serve`: acquires the single-instance lock, starts the IPC
/// server, and serves until signalled, idle-shutdown, or in-band stop.
async fn run_serve(
    state_dir: Option<PathBuf>,
    repo: PathBuf,
    idle_seconds: Option<u64>,
    foreground: bool,
    config: Vec<PathBuf>,
) -> ExitCode {
    use crew_runtime::lifecycle::{self, ServeOptions};

    let state_dir = match resolve_state_dir(state_dir) {
        Ok(dir) => dir,
        Err(err) => return fail(&err),
    };

    let options = ServeOptions {
        state_dir,
        repo,
        idle_seconds,
        foreground,
        binary_source: binary_source_from_env(),
        config_paths: config,
    };

    match lifecycle::serve(&options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(lifecycle::ServeError::AlreadyRunning(already)) => {
            // Machine-readable identity of the live runtime, on stdout.
            println!(
                "{}",
                serde_json::to_string(&already).expect("AlreadyRunning serializes")
            );
            // EX_TEMPFAIL (73): a peer already holds the lock.
            ExitCode::from(73)
        }
        Err(err) => fail(&err),
    }
}

/// Runs `crewd status`: connects to the runtime, queries `runtime/status`,
/// and prints the result as JSON.
async fn run_status(
    wait_seconds: Option<u64>,
    state_dir: Option<PathBuf>,
    repo: PathBuf,
) -> ExitCode {
    use crew_runtime::lifecycle::{self, StatusOptions};

    let state_dir = match resolve_state_dir(state_dir) {
        Ok(dir) => dir,
        Err(err) => return fail(&err),
    };

    let options = StatusOptions {
        state_dir,
        repo,
        wait_seconds,
    };

    match lifecycle::status(&options).await {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string(&value).expect("status serializes")
            );
            ExitCode::SUCCESS
        }
        Err(err) => fail(&err),
    }
}

/// Runs `crewd stop`: signals a live runtime and waits for it to shut down.
async fn run_stop(state_dir: Option<PathBuf>, repo: PathBuf) -> ExitCode {
    use crew_runtime::lifecycle::{self, StopOptions};

    let state_dir = match resolve_state_dir(state_dir) {
        Ok(dir) => dir,
        Err(err) => return fail(&err),
    };

    let options = StopOptions { state_dir, repo };

    match lifecycle::stop(&options).await {
        Ok(crew_runtime::lifecycle::StopOutcome::Stopped) => {
            println!("runtime stopped");
            ExitCode::SUCCESS
        }
        Ok(crew_runtime::lifecycle::StopOutcome::NotRunning) => {
            println!("no runtime running for this repository");
            ExitCode::from(1)
        }
        Err(err) => fail(&err),
    }
}

/// Runs `crewd lease release`: force-releases a workspace lease by id,
/// directly against the lease database -- no daemon required. Prints the
/// released lease's materialized path (when one exists) so the operator
/// can remove a leaked worktree the runtime will no longer tear down.
///
/// Guarded (R86 review E1-E3): refused while a runtime serves this
/// repository (its socket exists; the daemon's monitors could never see
/// this out-of-band write); an `active` lease needs `--yes`; the intent
/// is persisted to the audited `operations` table before the release
/// runs; the release itself journals `LeaseReleased` (and `CleanupFailed`
/// when the worktree teardown fails, in which case the row moves to
/// `cleanupFailed` so the doctor keeps reporting the leaked directory).
async fn run_lease_release(
    state_dir: Option<PathBuf>,
    repo: PathBuf,
    lease_id: String,
    yes: bool,
) -> ExitCode {
    use crew_protocol::{IsolationKind, OperationId, Timestamp, WorkspaceState};
    use crew_runtime::domain::DomainRepository;
    use crew_runtime::paths::RuntimePaths;
    use crew_runtime::security::redaction::Redactor;
    use crew_runtime::workspace::{LeaseError, LeaseService, WorkspaceMaterializer};
    use std::path::Path;

    let state_root = match resolve_state_dir(state_dir) {
        Ok(dir) => dir,
        Err(err) => return fail(&err),
    };
    let paths = match RuntimePaths::resolve(&state_root, &repo) {
        Ok(paths) => paths,
        Err(err) => return fail(&err),
    };

    // A live daemon owns this state: its subscribers would never see an
    // out-of-band mutation (invariant 7), and the lease may belong to an
    // in-flight run it is supervising. Liveness is proven by the advisory
    // flock, the same probe `crewd stop` uses -- NOT by the socket
    // file, which an unclean crash (the exact case this command exists
    // for) leaves behind.
    if crew_runtime::lifecycle::runtime_is_live(&paths.lock) {
        eprintln!(
            "a runtime is serving this repository; release the lease over RPC \
             (workspace/release) or run `crewd stop` first"
        );
        return ExitCode::from(1);
    }

    let leases = match LeaseService::open(paths.project_id, &paths.root.join("workspace-leases.db"))
    {
        Ok(service) => service,
        Err(err) => return fail(&err),
    };

    // Read the lease first: the operator learns the materialized path, and
    // the `active` guard below needs the state before any write.
    let info = match leases.get(lease_id.clone()) {
        Ok(info) => info,
        Err(LeaseError::NotFound { lease_id }) => {
            eprintln!("no lease {lease_id} exists for this repository");
            return ExitCode::from(1);
        }
        Err(err) => {
            eprintln!("could not read lease {lease_id} before release: {err}");
            return ExitCode::from(1);
        }
    };
    if info.state == WorkspaceState::Released {
        eprintln!("lease {lease_id} was already released");
        return ExitCode::from(2);
    }
    if info.state == WorkspaceState::Active && !yes {
        eprintln!(
            "lease {lease_id} is active -- releasing it strips a run's workspace \
             claim; pass --yes to confirm"
        );
        return ExitCode::from(1);
    }

    // Intent before side effect (invariant 4), into the same audited
    // `operations` table every other out-of-band mutation uses.
    let db = match crew_runtime::db::DatabaseHandle::start(paths.database.clone()).await {
        Ok(handle) => std::sync::Arc::new(handle),
        Err(err) => return fail(&err),
    };
    // The intent is durable journal text: sanitize it with the full
    // configured Redactor (built-ins plus `security.patterns` from the
    // user and repo crew.json layers, same precedence `serve` uses) when
    // a config is readable (WP26). This command is crash recovery, so a
    // broken or absent config degrades to built-in rules with a visible
    // warning instead of blocking the operator's cleanup.
    let mut layer_files: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        layer_files.push(PathBuf::from(home).join(".omp").join("crew.json"));
    }
    layer_files.push(repo.join(".omp").join("crew.json"));
    layer_files.retain(|p| p.is_file());
    let layer_refs: Vec<&Path> = layer_files.iter().map(PathBuf::as_path).collect();
    let redactor = match crew_runtime::config::crew::load_layers(&layer_refs, None) {
        Ok(config) => Redactor::with_org_rules(&config.security.patterns).unwrap_or_else(|e| {
            eprintln!("warning: invalid security.patterns ({e}); using built-in redaction only");
            Redactor::new()
        }),
        Err(e) => {
            eprintln!("warning: could not read crew.json ({e}); using built-in redaction only");
            Redactor::new()
        }
    };
    let operation_id = OperationId::new();
    let intent = redactor.sanitize_json(&serde_json::json!({
        "kind": "cli.lease.release",
        "leaseId": lease_id,
        "runId": info.run_id.to_string(),
        "state": info.state,
    }));
    if let Err(err) = db
        .record_operation_intent(operation_id, "cli.lease.release", intent, Timestamp::now())
        .await
    {
        let _ = db.shutdown().await;
        return fail(&err);
    }

    let released = match leases.release(lease_id.clone()) {
        Ok(()) => true,
        Err(LeaseError::AlreadyReleased { .. }) => true,
        Err(err) => {
            let _ = db.shutdown().await;
            return fail(&err);
        }
    };

    // Tear the materialized directory down exactly as `abandon_lease`
    // does; a shared lease's path is the repository itself and is never
    // touched. A teardown failure moves the row to `cleanupFailed` so
    // `stale()` keeps reporting the leaked directory (review E3).
    let teardown_error = if info.isolation_kind != IsolationKind::Shared
        && !info.path.is_empty()
        && std::path::Path::new(&info.path).exists()
    {
        WorkspaceMaterializer::new(paths.project_id, repo.clone())
            .map_err(|e| e.to_string())
            .and_then(|m| {
                m.teardown(std::path::Path::new(&info.path), info.isolation_kind)
                    .map_err(|e| e.to_string())
            })
            .err()
    } else {
        None
    };
    if let Some(message) = &teardown_error {
        let _ = leases.mark_cleanup_failed(lease_id.clone());
        eprintln!(
            "worktree teardown failed; the doctor will keep reporting {}: {message}",
            info.path
        );
    }

    // Journal what happened (invariant 7's commit half; no daemon is
    // serving, so there are no live subscribers to broadcast to).
    let project_id = paths.project_id;
    let run_id = info.run_id;
    let event_lease_id = lease_id.clone();
    let cleanup_error = teardown_error.clone();
    let journaled = db
        .run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            if let Some(error) = cleanup_error {
                repo.record_workspace_event(
                    crew_protocol::WorkspaceEvent::CleanupFailed {
                        lease_id: event_lease_id.clone(),
                        error,
                    },
                    run_id,
                    event_lease_id.clone(),
                )?;
            }
            repo.record_workspace_event(
                crew_protocol::WorkspaceEvent::LeaseReleased {
                    lease_id: event_lease_id.clone(),
                    run_id,
                },
                run_id,
                event_lease_id,
            )
            .map(|_| serde_json::json!({}))
        }))
        .await;
    if let Err(err) = journaled {
        let _ = db.shutdown().await;
        return fail(&err);
    }

    let ack = redactor.sanitize_json(&serde_json::json!({
        "released": released,
        "cleanupFailed": teardown_error.is_some(),
    }));
    if let Err(err) = db.acknowledge_operation(operation_id, ack).await {
        let _ = db.shutdown().await;
        return fail(&err);
    }
    if let Err(err) = db.shutdown().await {
        return fail(&err);
    }

    println!("lease {lease_id} released");
    if teardown_error.is_none()
        && info.isolation_kind == IsolationKind::Shared
        && !info.path.is_empty()
        && std::path::Path::new(&info.path).exists()
    {
        println!(
            "its shared workspace is the repository itself and was left in place: {}",
            info.path
        );
    }
    ExitCode::SUCCESS
}

/// Runs `crewd monitor`: connects to the runtime, replays events, and
/// renders them as plain-text lines until interrupted.
async fn run_monitor(
    state_dir: Option<PathBuf>,
    repo: PathBuf,
    run_id: Option<String>,
) -> ExitCode {
    use crew_runtime::lifecycle::{self, MonitorOptions};

    let state_dir = match resolve_state_dir(state_dir) {
        Ok(dir) => dir,
        Err(err) => return fail(&err),
    };

    let options = MonitorOptions {
        state_dir,
        repo,
        run_id,
    };

    match lifecycle::monitor(&options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail(&err),
    }
}

/// Runs `crewd schema`: prints the canonical JSON Schema document.
/// The config layer files that apply to `repo`, lowest precedence first.
/// Mirrors the extension's `resolveCrewConfigPaths` exactly -- the two
/// must never disagree about which files a daemon is launched with.
fn config_layer_paths(repo: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".omp").join("crew.json"));
    }
    paths.push(repo.join(".omp").join("crew.json"));
    paths
}

fn resolve_repo(repo: Option<PathBuf>) -> PathBuf {
    repo.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn run_config(command: ConfigCommand) -> ExitCode {
    match command {
        ConfigCommand::Init {
            global,
            repo,
            force,
        } => run_config_init(global, repo, force),
        ConfigCommand::Print {
            defaults,
            schema,
            effective: _,
            repo,
        } => run_config_print(defaults, schema, repo),
        ConfigCommand::Path { repo } => run_config_path(repo),
    }
}

/// Writes a starter `crew.json` plus its schema, side by side so the
/// snapshot's relative `$schema` reference resolves in an editor.
///
/// Refuses to overwrite an existing crew.json without `--force`: the file
/// is the operator's, and silently replacing hand-tuned configuration
/// would be the worst possible failure mode for a convenience command.
fn run_config_init(global: bool, repo: Option<PathBuf>, force: bool) -> ExitCode {
    let dir = if global {
        match std::env::var("HOME") {
            Ok(home) => PathBuf::from(home).join(".omp"),
            Err(_) => {
                eprintln!("error: --global needs HOME to be set");
                return ExitCode::FAILURE;
            }
        }
    } else {
        resolve_repo(repo).join(".omp")
    };

    let config_path = dir.join("crew.json");
    if config_path.exists() && !force {
        eprintln!(
            "error: {} already exists; pass --force to overwrite it",
            config_path.display()
        );
        return ExitCode::FAILURE;
    }

    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!("error: creating {}: {err}", dir.display());
        return ExitCode::FAILURE;
    }

    let schema_path = dir.join(crew_runtime::config::crew::SCHEMA_FILE_NAME);
    if let Err(err) = std::fs::write(
        &schema_path,
        crew_runtime::config::crew::render_config_schema(),
    ) {
        eprintln!("error: writing {}: {err}", schema_path.display());
        return ExitCode::FAILURE;
    }
    if let Err(err) = std::fs::write(
        &config_path,
        crew_runtime::config::crew::render_default_document(),
    ) {
        eprintln!("error: writing {}: {err}", config_path.display());
        return ExitCode::FAILURE;
    }

    println!("wrote {}", config_path.display());
    println!("wrote {}", schema_path.display());
    println!(
        "\nThis is a full snapshot of today's built-in defaults, so every key in it\n\
         now overrides the daemon rather than tracking it. Delete any key you do not\n\
         intend to pin; `crewd doctor` reports the ones that have since diverged."
    );
    ExitCode::SUCCESS
}

fn run_config_print(defaults: bool, schema: bool, repo: Option<PathBuf>) -> ExitCode {
    if defaults {
        print!(
            "{}",
            String::from_utf8_lossy(&crew_runtime::config::crew::render_default_document())
        );
        return ExitCode::SUCCESS;
    }
    if schema {
        print!(
            "{}",
            String::from_utf8_lossy(&crew_runtime::config::crew::render_config_schema())
        );
        return ExitCode::SUCCESS;
    }

    let repo = resolve_repo(repo);
    let paths = config_layer_paths(&repo);
    let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    match crew_runtime::config::crew::load_layers(&refs, None) {
        Ok(cfg) => {
            let mut text = match serde_json::to_string_pretty(&cfg) {
                Ok(text) => text,
                Err(err) => {
                    eprintln!("error: serializing effective config: {err}");
                    return ExitCode::FAILURE;
                }
            };
            text.push('\n');
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Reports which layer files exist, in the order they merge, plus every
/// key an existing layer pins away from the current built-in default.
fn run_config_path(repo: Option<PathBuf>) -> ExitCode {
    let repo = resolve_repo(repo);
    let paths = config_layer_paths(&repo);

    println!("crew.json layers, lowest precedence first:");
    for path in &paths {
        let marker = if path.exists() { "present" } else { "absent " };
        println!("  [{marker}] {}", path.display());
    }
    if !paths.iter().any(|p| p.exists()) {
        println!("\nNo layer files; the daemon runs on built-in defaults.");
        println!("Create one with `crewd config init` (or `--global`).");
    }
    ExitCode::SUCCESS
}

async fn run_schema() -> ExitCode {
    // Read the schema file from the protocol package.
    let schema_path = std::path::Path::new("packages/protocol-ts/schema/crew.schema.json");
    match std::fs::read_to_string(schema_path) {
        Ok(schema) => {
            print!("{schema}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("failed to read schema file: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Runs `crewd audit export`: exports events to a JSONL file.
async fn run_audit_export(
    state_dir: Option<PathBuf>,
    repo: PathBuf,
    from: Option<String>,
    to: Option<String>,
    output: PathBuf,
) -> ExitCode {
    // A failed resolve must not silently fall back to some default
    // directory: the export would then report success against a state
    // root that may not exist, or isn't the one actually served.
    let state_root = match resolve_state_dir(state_dir) {
        Ok(dir) => dir,
        Err(err) => return fail(&err),
    };

    // `--state-dir` is the state ROOT here, exactly like every other
    // subcommand -- `RuntimePaths::resolve` derives the per-repository
    // runtime directory from it plus `--repo`, rather than treating
    // `--state-dir` itself as that directory.
    let paths = match crew_runtime::paths::RuntimePaths::resolve(&state_root, &repo) {
        Ok(paths) => paths,
        Err(err) => return fail(&format!("failed to resolve runtime paths: {err}")),
    };

    // `DatabaseHandle::start` migrates a missing file into an empty,
    // freshly-initialized database rather than erroring -- which would
    // make this export silently "succeed" with zero events against a repo
    // that was never actually served. Refuse before that happens.
    if !paths.database.exists() {
        return fail(&format!(
            "no database at {}; this repository has never been served under this state root",
            paths.database.display()
        ));
    }

    let db = match crew_runtime::db::DatabaseHandle::start(paths.database.clone()).await {
        Ok(handle) => handle,
        Err(err) => return fail(&format!("failed to open database: {err}")),
    };

    let mut export = crew_runtime::audit::Export::new(
        repo.to_string_lossy().to_string(),
        paths.root.to_string_lossy().to_string(),
        output.to_string_lossy().to_string(),
    );
    export.from = from;
    export.to = to;

    match export.export(&db).await {
        Ok(count) => {
            println!("exported {count} events to {}", output.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("export failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves the state directory when `--state-dir` is omitted, using the
/// real process environment and `$HOME`. Delegates to
/// [`resolve_state_dir_with`] -- see that function for the actual
/// precedence; this wrapper only exists to keep the process-global env
/// read out of the (deterministically testable) resolution logic itself,
/// the same split [`StateRoot::resolve`]/`resolve_with` already applies.
fn resolve_state_dir(state_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    resolve_state_dir_with(state_dir, &env, home.as_deref())
}

/// Resolves the state directory when `--state-dir` is omitted, via
/// [`StateRoot::resolve`] -- the same `CREW_STATE_DIR` ->
/// `$XDG_STATE_HOME/omp/crew` -> `$HOME/.omp/crew` precedence the OMP
/// extension's `resolveStateRoot` applies when it spawns `crewd serve`. A
/// bare `crewd status`/`stop`/`serve`/... with no flag therefore resolves
/// to the exact directory the extension would have used, instead of a
/// separate, easy-to-forget default -- CLI and extension now share one
/// resolution algorithm rather than two.
fn resolve_state_dir_with(
    state_dir: Option<PathBuf>,
    env: &std::collections::HashMap<String, String>,
    home: Option<&std::path::Path>,
) -> Result<PathBuf, String> {
    if let Some(dir) = state_dir {
        return Ok(dir);
    }

    let home = home.ok_or_else(|| {
        "cannot resolve a default state directory: $HOME is not set; use --state-dir to specify one".to_string()
    })?;

    StateRoot::resolve(env, home)
        .map(|root| root.path().to_path_buf())
        .map_err(|err| {
            format!(
                "failed to resolve a default state directory: {err}; use --state-dir to specify one"
            )
        })
}

/// Runs `crewd doctor`: runs the diagnostic check catalog against the
/// same paths `serve` uses, so it diagnoses the state a daemon actually
/// writes rather than a directory only `doctor` believes in.
async fn run_doctor(
    state_dir: Option<PathBuf>,
    repo: PathBuf,
    json: bool,
    config: Vec<PathBuf>,
) -> ExitCode {
    use crew_runtime::doctor::Doctor;
    use crew_runtime::paths::RuntimePaths;

    /// Reports a fatal condition in the caller's chosen format. Only
    /// conditions that prevent the catalog from running reach here; an
    /// individual check's failure is part of the result.
    fn abort(json: bool, message: &str) -> ExitCode {
        if json {
            println!(
                "{}",
                serde_json::json!({ "healthy": false, "error": message })
            );
        } else {
            eprintln!("{message}");
        }
        ExitCode::FAILURE
    }

    let state_root = match resolve_state_dir(state_dir) {
        Ok(dir) => dir,
        Err(err) => return abort(json, &err),
    };

    let paths = match RuntimePaths::resolve(&state_root, &repo) {
        Ok(paths) => paths,
        Err(err) => return abort(json, &format!("failed to resolve runtime paths: {err}")),
    };

    let db = match crew_runtime::db::DatabaseHandle::start(paths.database.clone()).await {
        Ok(handle) => Some(std::sync::Arc::new(handle)),
        Err(err) => return abort(json, &format!("failed to open database: {err}")),
    };

    // `--repo` names the repository being diagnosed, never a config file.
    // Config layers are explicit flags, exactly as they are for `serve`.
    let path_refs: Vec<&std::path::Path> = config.iter().map(PathBuf::as_path).collect();
    let policy = match crew_runtime::config::resolve_policy(&path_refs, None) {
        Ok(policy) => Some(policy),
        Err(err) => return abort(json, &format!("failed to load config: {err}")),
    };

    // The notes report on the layers that actually apply here. Explicit
    // `--config` flags win when given; otherwise fall back to the implicit
    // user/project pair the extension launches the daemon with, so a plain
    // `crewd doctor` still tells the operator which files are in play.
    let note_layers = if config.is_empty() {
        config_layer_paths(&repo)
    } else {
        config.clone()
    };

    let doctor = Doctor::new(db, Some(paths.root.clone()), policy)
        .with_runtime_context(paths.socket.clone(), repo, paths.project_id)
        .with_config_layers(note_layers);

    match doctor.check().await {
        Ok(result) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&result).expect("DoctorResult serializes")
                );
            } else {
                println!(
                    "doctor check: {}",
                    if result.healthy { "healthy" } else { "failed" }
                );
                if !result.failed_checks.is_empty() {
                    eprintln!("failed checks:");
                    for check in &result.failed_checks {
                        eprintln!("  - {:?}", check);
                    }
                }
                // Notes go to stdout, not stderr: they are observations
                // about a healthy runtime, not diagnostics of a broken one.
                for note in &result.notes {
                    println!("note ({}): {}", note.check_name, note.detail);
                }
            }
            ExitCode::from(if result.healthy { 0 } else { 1 })
        }
        Err(err) => {
            eprintln!("doctor check failed: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Runs `crewd coordination-mcp`: proxies MCP `initialize`/`tools/list`/
/// `tools/call` on stdio to the worker coordination tools over the
/// runtime socket, authenticated with `CREW_WORKER_SCOPE_TOKEN` read
/// from (and removed from) this process's own inherited environment. All
/// protocol/auth behavior lives in `crew_runtime::coordination::mcp`;
/// this function only resolves CLI arguments into that call.
async fn run_coordination_mcp(
    state_dir: Option<PathBuf>,
    repo: PathBuf,
    run_id: String,
) -> ExitCode {
    use crew_protocol::RunId;
    use crew_runtime::coordination::mcp::{self, ProcessEnvironment};

    let state_dir = match resolve_state_dir(state_dir) {
        Ok(dir) => dir,
        Err(err) => return fail(&err),
    };
    let run_id = match RunId::parse(&run_id) {
        Ok(id) => id,
        Err(err) => return fail(&err),
    };

    match mcp::run(&state_dir, &repo, run_id, &ProcessEnvironment).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail(&err),
    }
}

/// Runs `crewd display probe`: probes one display backend's status and
/// prints it as JSON or human-readable text. Never activates the backend;
/// this only reads availability/version, exactly like `DisplayBackendTrait::status`.
async fn run_display_probe(backend: String, json: bool) -> ExitCode {
    use crew_protocol::{DisplayBackend as ProtoBackend, DisplayConfig};
    use crew_runtime::display::{
        DisplayBackendTrait, HerdrDisplay, HiddenDisplay, OsWindowDisplay, TmuxDisplay,
    };

    let display: Box<dyn DisplayBackendTrait> = match backend.as_str() {
        "herdr" => Box::new(HerdrDisplay::new(DisplayConfig {
            backend: ProtoBackend::Herdr,
            width: None,
            height: None,
        })),
        "tmux" => Box::new(TmuxDisplay::new(DisplayConfig {
            backend: ProtoBackend::Tmux,
            width: None,
            height: None,
        })),
        "osWindow" => Box::new(OsWindowDisplay::new(DisplayConfig {
            backend: ProtoBackend::OsWindow,
            width: None,
            height: None,
        })),
        "hidden" => Box::new(HiddenDisplay::new(DisplayConfig {
            backend: ProtoBackend::Hidden,
            width: None,
            height: None,
        })),
        other => {
            return fail(&format!(
                "unknown display backend `{other}`; expected one of herdr, tmux, osWindow, or hidden"
            ));
        }
    };

    let status = display.status();
    let version = display.version();

    if json {
        let mut value = serde_json::to_value(&status).expect("DisplayStatus serializes");
        if let Some(obj) = value.as_object_mut() {
            obj.insert("version".to_string(), serde_json::json!(version));
        }
        println!("{value}");
    } else {
        println!("backend: {}", display.backend_name());
        println!("available: {}", status.available);
        println!("active: {}", status.active);
        if let Some(v) = version {
            println!("version: {v}");
        }
        if let Some((w, h)) = status.dimensions {
            println!("dimensions: {w}x{h}");
        }
    }
    ExitCode::SUCCESS
}

/// Runs `crewd conformance`: runs one or all adapters' fixture or live
/// conformance suite and writes the resulting report(s) to `output` as a
/// JSON array, printing the exact same JSON to stdout. Exactly one of
/// `fixture`/`live` must be set. Live mode exercises the TUI control plane
/// by default (`--mode tui`); `--mode headless` reaches each adapter's kept
/// headless live report. When `CREW_DISABLE_VENDOR_CLI=1` is set, the
/// adapter's `live_report()` returns `Err`, which this command reports as a
/// `{adapter, mode:"live", passed:false, error}` entry, never a hard process
/// failure.
async fn run_conformance(
    adapter: String,
    fixture: bool,
    live: bool,
    mode: ConformanceModeArg,
    output: PathBuf,
) -> ExitCode {
    use crew_runtime::adapter::{AdapterKind, AdapterMode};
    use crew_runtime::conformance::{
        ConformanceReport, run_fixture_conformance, run_live_conformance,
    };

    if fixture == live {
        return fail(&format!(
            "conformance requires exactly one of --fixture or --live (got fixture={fixture}, live={live})"
        ));
    }

    let kinds: Vec<AdapterKind> = if adapter == "all" {
        vec![
            AdapterKind::Claude,
            AdapterKind::Codex,
            AdapterKind::Copilot,
            AdapterKind::OmpRpc,
        ]
    } else {
        match AdapterKind::from_wire_name(&adapter) {
            Some(kind) => vec![kind],
            None => {
                return fail(&format!(
                    "unknown adapter `{adapter}`; expected one of claude, codex, copilot, ompRpc, or all"
                ));
            }
        }
    };

    // crew-v2 gap-closure WP-C (WP-B M-1 rider): `--mode headless` is a
    // typed rejection now, for both `--fixture` and `--live` -- never
    // silently accepted-and-discarded. Before this WP, `--fixture`
    // ignored `--mode` entirely (always headless-sourced) and `--live
    // --mode headless` silently reached each adapter's own headless
    // `live_report`; both dispatch targets are deleted along with the
    // headless control plane itself (spec §4.6).
    if matches!(mode, ConformanceModeArg::Headless) {
        return fail(
            &"mode: \"headless\" is retired in crew v2 (spec §4.6) -- the headless control \
              plane has no adapter implementation to dispatch to; use --mode tui (the default)",
        );
    }

    let mut reports: Vec<serde_json::Value> = Vec::with_capacity(kinds.len());
    let mut typed_reports: Vec<ConformanceReport> = Vec::new();
    for kind in kinds {
        if fixture {
            let report = run_fixture_conformance(kind, AdapterMode::Tui).await;
            typed_reports.push(report);
        } else {
            match run_live_conformance(kind, matches!(mode, ConformanceModeArg::Tui)).await {
                Ok(report) => typed_reports.push(report),
                Err(err) => {
                    // Live failures are reported as entries, never a hard
                    // process failure.
                    reports.push(serde_json::json!({
                        "adapter": kind.wire_name(),
                        "mode": "live",
                        "passed": false,
                        "error": err,
                    }));
                    continue;
                }
            }
        }
    }
    // Convert typed reports to JSON.
    for report in &typed_reports {
        reports.push(serde_json::to_value(report).expect("ConformanceReport serializes"));
    }

    let rendered = serde_json::to_string_pretty(&reports).expect("reports serialize");
    if let Err(err) = std::fs::write(&output, &rendered) {
        return fail(&format!("failed to write {}: {err}", output.display()));
    }
    println!("{rendered}");

    // In fixture mode, gate against the committed baseline.
    if fixture {
        let baseline_path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/conformance/fixture-mode-baseline.json"
        ));
        let baseline_text = match std::fs::read_to_string(&baseline_path) {
            Ok(t) => t,
            Err(err) => {
                return fail(&format!(
                    "cannot read fixture-mode baseline {}: {err}",
                    baseline_path.display()
                ));
            }
        };
        let baseline: FixtureBaseline = match serde_json::from_str(&baseline_text) {
            Ok(b) => b,
            Err(err) => {
                return fail(&format!(
                    "fixture-mode baseline {} is malformed: {err}",
                    baseline_path.display()
                ));
            }
        };
        for report in &typed_reports {
            let adapter = &report.adapter;
            let expected_failures: Vec<String> = baseline
                .expected_failures
                .get(adapter.as_str())
                .cloned()
                .unwrap_or_default();
            let unproven: Vec<String> = report
                .scenarios
                .iter()
                .filter(|s| !s.proved())
                .map(|s| s.name.to_string())
                .collect();
            // Check for unproven scenarios (failed or skipped -- neither is
            // proof) not in the baseline.
            let unexpected: Vec<&String> = unproven
                .iter()
                .filter(|name| !expected_failures.contains(name))
                .collect();
            if !unexpected.is_empty() {
                return fail(&format!(
                    "adapter {} has scenario(s) not proven and not in the fixture-mode baseline: {}",
                    adapter,
                    unexpected
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            // Check for baseline entries that now pass (rotting baseline).
            let now_passing: Vec<&String> = expected_failures
                .iter()
                .filter(|name| !unproven.contains(name))
                .collect();
            if !now_passing.is_empty() {
                return fail(&format!(
                    "adapter {} baseline scenario(s) now pass — remove from baseline: {}",
                    adapter,
                    now_passing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    ExitCode::SUCCESS
}

/// Runs `crewd adapters`: runs every reserved adapter kind's fixture
/// conformance suite and prints the resulting reports (the only source of
/// truth for OMP-facing effective capabilities) as JSON or human-readable
/// text.
async fn run_adapters(json: bool) -> ExitCode {
    use crew_runtime::adapter::{AdapterKind, AdapterMode};
    use crew_runtime::conformance::run_fixture_conformance;

    let kinds = [
        AdapterKind::Claude,
        AdapterKind::Codex,
        AdapterKind::Copilot,
        AdapterKind::OmpRpc,
    ];
    let mut reports = Vec::with_capacity(kinds.len());
    for kind in kinds {
        // crew-v2 gap-closure WP-C: TUI now, not Headless -- the headless
        // control plane is retired (spec §4.6) and its adapters deleted.
        // Report labels are now the `*-tui` ones (`claude-tui`, ...),
        // matching every other TUI-sourced fixture report.
        reports.push(run_fixture_conformance(kind, AdapterMode::Tui).await);
    }

    if json {
        println!(
            "{}",
            serde_json::to_string(&reports).expect("reports serialize")
        );
    } else {
        for report in &reports {
            println!(
                "{}: mode={:?} passed={} scenarios={}",
                report.adapter,
                report.mode,
                report.passed,
                report.scenarios.len()
            );
        }
    }
    ExitCode::SUCCESS
}

/// The byte a viewer types to detach from `crewd attach`: Ctrl+], the
/// same convention `telnet` and several other raw-terminal clients use.
const ATTACH_DETACH_BYTE: u8 = 0x1D;

/// Runs `crewd attach`: connects to a worker's attach socket, puts this
/// process's stdin into raw mode, and pumps bytes bidirectionally between
/// the terminal and the socket until the socket closes or the viewer
/// types Ctrl+]. Resolves the socket via `RuntimePaths` from `--repo`/
/// `--state-dir`/`run-id`, unless `--socket` names one directly.
///
/// The byte-pumping itself (`crew_runtime::display::attach::pump`) is
/// unit-tested against in-memory pipes; only the raw-mode terminal setup
/// below is untested -- it is a thin wrapper with no logic beyond calling
/// libc through `nix`, restored via an RAII guard on every exit path.
async fn run_attach(
    run_id: String,
    state_dir: Option<PathBuf>,
    repo: Option<PathBuf>,
    socket_override: Option<PathBuf>,
) -> ExitCode {
    use crew_protocol::RunId;
    use crew_runtime::display::attach;
    use crew_runtime::paths::RuntimePaths;

    // CREW-18: resolved alongside the socket path, only on the `--repo`
    // path -- `--socket` (mainly for tests, per `run_attach`'s own doc)
    // carries no repository/project context to look a run's worker and
    // adapter up from, so it just skips the title; a raw socket path is
    // never surfaced to an interactive user's terminal anyway.
    let mut pane_title: Option<String> = None;

    let socket_path = if let Some(socket) = socket_override {
        socket
    } else {
        let Some(repo) = repo else {
            return fail(&"crewd attach requires --repo (or an explicit --socket)");
        };
        let run_id = match RunId::parse(&run_id) {
            Ok(id) => id,
            Err(err) => return fail(&err),
        };
        let state_dir = match resolve_state_dir(state_dir) {
            Ok(dir) => dir,
            Err(err) => return fail(&err),
        };
        let paths = match RuntimePaths::resolve(&state_dir, &repo) {
            Ok(paths) => paths,
            Err(err) => return fail(&err),
        };
        pane_title = resolve_pane_title(&paths, run_id).await;
        paths.pane_socket(&run_id)
    };

    let socket = match attach::connect(&socket_path).await {
        Ok(socket) => socket,
        Err(err) => return fail(&err),
    };

    println!("crewd attach: connected. Press Ctrl+] to detach.");

    // Set once, here, before the pump starts -- never re-asserted. The
    // vendor process's own later title sequences (if it emits any) simply
    // overwrite this one and win, exactly like any other program sharing
    // a terminal would.
    if let Some(title) = &pane_title {
        print!("{}", attach::osc_set_title(title));
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    let guard = match RawModeGuard::enable() {
        Ok(guard) => guard,
        Err(err) => return fail(&format!("failed to enable raw terminal mode: {err}")),
    };
    let outcome = attach::pump(
        socket,
        tokio::io::stdin(),
        tokio::io::stdout(),
        ATTACH_DETACH_BYTE,
    )
    .await;
    drop(guard);

    match outcome {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => fail(&err),
    }
}

/// Best-effort: the pane title for `run_id`, or `None` if the database is
/// unreachable or the run/worker/profile join comes up empty -- a title
/// is a nicety, never worth refusing to attach over.
async fn resolve_pane_title(
    paths: &crew_runtime::paths::RuntimePaths,
    run_id: crew_protocol::RunId,
) -> Option<String> {
    let db = crew_runtime::db::DatabaseHandle::start(paths.database.clone())
        .await
        .ok()?;
    let project_id = paths.project_id;
    let run_id_string = run_id.to_string();
    let row = db
        .run_domain_op(Box::new(move |conn| {
            conn.query_row(
                "SELECT r.worker_id, p.adapter
                 FROM runs r
                 JOIN workers w ON w.worker_id = r.worker_id
                 JOIN worker_profiles p ON p.id = w.profile_id
                 WHERE r.run_id = ?1 AND w.project_id = ?2",
                rusqlite::params![run_id_string, project_id.to_string()],
                |row| {
                    Ok(serde_json::json!({
                        "workerId": row.get::<_, String>(0)?,
                        "adapter": row.get::<_, String>(1)?,
                    }))
                },
            )
            .map_err(crew_runtime::domain::DomainError::from)
        }))
        .await
        .ok()?;
    let worker_id = row["workerId"].as_str()?;
    let adapter = row["adapter"].as_str()?;
    Some(crew_runtime::display::attach::pane_title(
        worker_id, adapter,
    ))
}

/// Puts this process's stdin into raw mode (no line buffering, no echo,
/// no signal-generating control characters) for the duration of `crewd
/// attach`, restoring the original terminal settings on drop -- on
/// every exit path, including an early return or a panic unwind.
struct RawModeGuard {
    original: nix::sys::termios::Termios,
}

impl RawModeGuard {
    fn enable() -> nix::Result<Self> {
        use nix::sys::termios::{SetArg, cfmakeraw, tcgetattr, tcsetattr};
        let original = tcgetattr(std::io::stdin())?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(std::io::stdin(), SetArg::TCSANOW, &raw)?;
        Ok(Self { original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        use nix::sys::termios::{SetArg, tcsetattr};
        let _ = tcsetattr(std::io::stdin(), SetArg::TCSANOW, &self.original);
    }
}

/// Prints an error to stderr and returns `ExitCode::FAILURE`.
fn fail(err: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("{err}");
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An explicit `--state-dir` always wins outright, before `env`/`home`
    /// are even consulted.
    #[test]
    fn resolve_state_dir_prefers_an_explicit_flag() {
        let env = std::collections::HashMap::new();
        let explicit = PathBuf::from("/explicit/state");
        assert_eq!(
            resolve_state_dir_with(
                Some(explicit.clone()),
                &env,
                Some(std::path::Path::new("/home/alice"))
            ),
            Ok(explicit)
        );
    }

    /// A bare invocation with no `--state-dir` must resolve to exactly the
    /// same directory the OMP extension's `resolveStateRoot` would have
    /// used -- this is the whole point of routing through
    /// [`StateRoot::resolve`] instead of a separate CLI-only default.
    #[test]
    fn resolve_state_dir_with_no_flag_matches_the_extensions_default() {
        let env = std::collections::HashMap::new();
        let home = std::path::Path::new("/home/alice");
        assert_eq!(
            resolve_state_dir_with(None, &env, Some(home)),
            Ok(PathBuf::from("/home/alice/.omp/crew"))
        );
    }

    /// `CREW_STATE_DIR` still wins outright over the `$HOME`-derived
    /// default, exactly as [`StateRoot::resolve`] documents.
    #[test]
    fn resolve_state_dir_with_no_flag_honors_crew_state_dir() {
        let mut env = std::collections::HashMap::new();
        env.insert("CREW_STATE_DIR".to_string(), "/var/lib/crew".to_string());
        let home = std::path::Path::new("/home/alice");
        assert_eq!(
            resolve_state_dir_with(None, &env, Some(home)),
            Ok(PathBuf::from("/var/lib/crew"))
        );
    }

    /// No flag and no `$HOME` must fail closed with a clear message, not
    /// panic or guess a directory.
    #[test]
    fn resolve_state_dir_with_no_flag_and_no_home_fails_closed() {
        let env = std::collections::HashMap::new();
        let err = resolve_state_dir_with(None, &env, None).unwrap_err();
        assert!(
            err.contains("$HOME is not set"),
            "unexpected message: {err}"
        );
        assert!(err.contains("--state-dir"), "unexpected message: {err}");
    }

    /// `run_audit_export` must resolve the database the same way every
    /// other subcommand does: `RuntimePaths::resolve(state_root, repo)`,
    /// not a direct join of `runtime.db` onto `--state-dir`.
    #[tokio::test]
    async fn audit_export_resolves_the_db_via_runtime_paths_like_every_other_subcommand() {
        let state_root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();

        // Seed events at the location `RuntimePaths::resolve` computes for
        // this repo -- the correct per-repository database -- not at
        // `<state-root>/runtime.db`, which is what the old buggy code read.
        let paths =
            crew_runtime::paths::RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();
        {
            let db = crew_runtime::db::DatabaseHandle::start(paths.database.clone())
                .await
                .unwrap();
            let redactor = crew_runtime::security::redaction::Redactor::new();
            for text in ["first", "second", "third"] {
                let event = crew_runtime::security::redaction::RawRuntimeEvent {
                    timestamp: crew_protocol::Timestamp::now(),
                    project_id: crew_protocol::ProjectId::new(),
                    run_id: None,
                    kind: crew_runtime::security::redaction::RawEventKind::Diagnostic {
                        level: crew_protocol::DiagnosticLevel::Info,
                        code: "fixture".to_string(),
                        fragments: vec![crew_protocol::Classified {
                            class: crew_protocol::ContentClass::Visible,
                            value: text.to_string(),
                        }],
                    },
                };
                db.append_event(redactor.sanitize(event)).await.unwrap();
            }
        }

        let output = state_root.path().join("audit.jsonl");
        let code = run_audit_export(
            Some(state_root.path().to_path_buf()),
            repo.path().to_path_buf(),
            None,
            None,
            output.clone(),
        )
        .await;

        assert_eq!(code, ExitCode::SUCCESS);
        let lines: Vec<_> = std::fs::read_to_string(&output)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 3, "expected all 3 seeded events exported");
    }

    /// A `--state-dir`/`--repo` pair whose resolved database was never
    /// created must be refused -- opening it would silently migrate an
    /// empty database into existence and export zero events with no
    /// indication anything was wrong.
    #[tokio::test]
    async fn audit_export_refuses_a_nonexistent_database_without_creating_one() {
        let state_root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let output = state_root.path().join("audit.jsonl");

        let code = run_audit_export(
            Some(state_root.path().to_path_buf()),
            repo.path().to_path_buf(),
            None,
            None,
            output.clone(),
        )
        .await;

        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            !output.exists(),
            "a refused export must not write an output file"
        );
        let paths =
            crew_runtime::paths::RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();
        assert!(
            !paths.database.exists(),
            "a refused export must not have silently created the database either"
        );
    }

    /// CREW-18: `resolve_pane_title` joins a run to its worker's adapter
    /// exactly the way `pane/reopen`'s own handler does, then formats it
    /// through `attach::pane_title` -- this pins the join, not the
    /// formatting (already covered directly in `attach.rs`'s own tests).
    #[tokio::test]
    async fn resolve_pane_title_joins_run_to_worker_and_adapter() {
        let state_root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let paths =
            crew_runtime::paths::RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();

        let run_id = crew_protocol::RunId::new();
        let worker_id = crew_protocol::WorkerId::new();
        let profile_row_id = crew_protocol::WorkerId::new().to_string();
        {
            let db = crew_runtime::db::DatabaseHandle::start(paths.database.clone())
                .await
                .unwrap();
            let project_id = paths.project_id;
            let (run_id, worker_id, profile_row_id) = (
                run_id.to_string(),
                worker_id.to_string(),
                profile_row_id.clone(),
            );
            db.run_domain_op(Box::new(move |conn| {
                conn.execute(
                    "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
                     VALUES ('11111111-1111-7111-8111-111111111111', ?1, 'test-owner', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                    rusqlite::params![project_id.to_string()],
                )?;
                conn.execute(
                    "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
                     VALUES (?1, 'sha256:test', 'claude', 'test-model', '{}')",
                    rusqlite::params![profile_row_id],
                )?;
                conn.execute(
                    "INSERT INTO workers (worker_id, project_id, profile_id, created_at)
                     VALUES (?1, ?2, ?3, '2026-01-01T00:00:00Z')",
                    rusqlite::params![worker_id, project_id.to_string(), profile_row_id],
                )?;
                conn.execute(
                    "INSERT INTO runs (run_id, task_id, worker_id, state, created_at)
                     VALUES (?1, '11111111-1111-7111-8111-111111111111', ?2, 'queued', '2026-01-01T00:00:00Z')",
                    rusqlite::params![run_id, worker_id],
                )?;
                Ok(serde_json::Value::Null)
            }))
            .await
            .unwrap();
            db.shutdown().await.unwrap();
        }

        let title = resolve_pane_title(&paths, run_id)
            .await
            .expect("the seeded run/worker/profile join must resolve");
        assert_eq!(
            title,
            crew_runtime::display::attach::pane_title(&worker_id.to_string(), "claude")
        );
    }

    /// An unknown run resolves to no title -- best-effort, never a reason
    /// to refuse the attach itself.
    #[tokio::test]
    async fn resolve_pane_title_is_none_for_an_unknown_run() {
        let state_root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let paths =
            crew_runtime::paths::RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();
        {
            // Touch the database into existence without seeding anything.
            crew_runtime::db::DatabaseHandle::start(paths.database.clone())
                .await
                .unwrap()
                .shutdown()
                .await
                .unwrap();
        }

        let title = resolve_pane_title(&paths, crew_protocol::RunId::new()).await;
        assert!(title.is_none());
    }
}
