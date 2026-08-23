//! Daemon lifecycle: single-instance locking, detached/foreground serving,
//! idle shutdown, graceful stop, and the client side of `status`.
//!
//! `crewd serve` takes an advisory `flock(2)` on a persistent lock file
//! recording the runtime's identity, then serves the socket protocol until it
//! is signalled, an accepted in-band `runtime/shutdown` arrives (refused
//! with `-32602` while any run is live or another connection is open,
//! unless `force: true` -- R82; the SIGTERM operator path stays
//! deliberately unarbitrated), or it has been idle (no connections, no
//! active runs) for the configured interval. On any of
//! those it journals a stop record, then -- and only then -- removes the
//! socket and releases the lock, so the socket's disappearance is proof the
//! journal shut down first.
//!
//! Two servers racing for one repository resolve deterministically: the kernel
//! grants the exclusive `flock` to exactly one; the loser reads the live
//! lock's metadata and reports [`ServeError::AlreadyRunning`]. Staleness is
//! implicit -- a crashed daemon has its `flock` released by the kernel, so the
//! next starter simply acquires it. The lock file is never deleted on the
//! contended path, so there is no remove-then-recreate window in which two
//! daemons could own the same socket and database.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use batman_protocol::{
    BinarySource, Classified, ContentClass, DiagnosticLevel, EventEnvelope, RuntimeEvent,
};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::VERSION;
use crate::adapter::mcp_config::AdapterMcpConfig;
use crate::adapter::registry::AdapterRegistry;
use crate::coordination::{ScopeTokenStore, ScopeTokenVerifier};
use crate::db::DatabaseHandle;
use crate::ipc::{self, Server, ServerConfig};
use crate::paths::{PathError, RuntimePaths};
use crate::security::redaction::{RawEventKind, RawRuntimeEvent, Redactor};
use crate::security::{SecurityError, ensure_private_dir, ensure_private_file};

pub use crate::ipc::should_idle_shutdown;
use crate::policy::PolicyEvaluator;

/// Options for [`serve`].
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// The Crew state root directory. Already resolved by the CLI layer.
    pub state_dir: PathBuf,
    /// The repository this runtime serves.
    pub repo: PathBuf,
    /// Idle interval in seconds; `None` never idle-exits.
    pub idle_seconds: Option<u64>,
    /// Foreground mode logs structured records to stderr; detached mode logs
    /// them to `runtime.log`.
    pub foreground: bool,
    /// Where this binary was loaded from, reported by `runtime/status`.
    pub binary_source: BinarySource,
    /// Path to the org-level configuration file.
    pub org_config: Option<PathBuf>,
    /// Path to the repo-level configuration file.
    pub repo_config: Option<PathBuf>,
    /// Path to the user-level configuration file.
    pub user_config: Option<PathBuf>,
}

/// The machine-readable identity of an already-running runtime, printed by
/// the CLI to stdout when a `serve` loses the single-instance race.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlreadyRunning {
    /// Always `"already_running"`.
    pub status: &'static str,
    /// The pid of the runtime that holds the lock.
    pub pid: i32,
    /// The project id the live runtime serves.
    pub project_id: String,
    /// The live runtime's socket path.
    pub socket: String,
}

/// Errors from [`serve`].
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// Another runtime already holds the lock for this repository.
    #[error("a runtime is already running for this repository (pid {})", .0.pid)]
    AlreadyRunning(AlreadyRunning),
    /// Securing the state directory failed.
    #[error(transparent)]
    Security(#[from] SecurityError),
    /// Resolving the repository paths failed.
    #[error(transparent)]
    Path(#[from] PathError),
    /// Binding or serving the socket failed.
    #[error(transparent)]
    Ipc(#[from] ipc::IpcError),
    /// The durable database could not be opened or written.
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    /// A filesystem operation on the lock or log failed.
    #[error("lifecycle I/O error at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Resolving the effective policy failed (org/repo/user config parse/merge error).
    #[error("config error: {0}")]
    ConfigError(String),
}
/// or idle. Acquires the single-instance lock first; performs a graceful,
/// journal-before-socket-removal shutdown on exit.
///
/// # Errors
/// Returns [`ServeError::AlreadyRunning`] if a live runtime already holds the
/// lock, or another [`ServeError`] on a filesystem, database, or socket
/// failure.
pub async fn serve(opts: &ServeOptions) -> Result<(), ServeError> {
    ensure_private_dir(&opts.state_dir)?;
    let paths = RuntimePaths::resolve(&opts.state_dir, &opts.repo)?;

    // Win the lock (or report the live holder) before touching any state.
    // The over-long-socket-path guard lives in `Server::bind`; if it (or any
    // later step) fails, `lock` drops and the lock file is released.
    let lock = acquire_lock(&paths)?;

    init_logging(opts.foreground, &paths.log)?;
    tracing::info!(
        project_id = %paths.project_id,
        pid = std::process::id(),
        socket = %paths.socket.display(),
        detached = !opts.foreground,
        "runtime_started"
    );

    let db = Arc::new(DatabaseHandle::start(paths.database.clone()).await?);

    // Crash recovery: transition any run left stuck in a non-terminal state
    // by an unclean prior shutdown, before anything else touches the
    // database or the socket accepts a single connection. Every run this
    // sweep can see predates this process, so there is no live run to race.
    let recovery =
        crate::recovery::RecoveryCoordinator::with_defaults(Arc::clone(&db), paths.project_id);
    match recovery.recover().await {
        Ok(result) if result.recovered_count > 0 => {
            for recovered in &result.recovered_runs {
                if recovered.success {
                    tracing::info!(
                        run_id = %recovered.run_id,
                        worker_id = %recovered.worker_id,
                        from_state = %recovered.previous_state,
                        to_state = %recovered.new_state,
                        last_activity = %recovered.last_activity,
                        "crash_recovery_transitioned_run"
                    );
                } else {
                    tracing::warn!(
                        run_id = %recovered.run_id,
                        worker_id = %recovered.worker_id,
                        from_state = %recovered.previous_state,
                        error = recovered.error.as_deref().unwrap_or("unknown"),
                        "crash_recovery_failed_to_transition_run"
                    );
                }
            }
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(error = %err, "crash_recovery_sweep_failed");
        }
    }

    // Use the config paths from ServeOptions (passed through from CLI).
    // The layers are retained alongside the merged policy so `run/submit`
    // can re-merge a run's own `policyOverrides` onto them.
    let layers = crate::config::LayeredConfig::load(
        opts.org_config.as_deref(),
        opts.repo_config.as_deref(),
        opts.user_config.as_deref(),
    )
    .map_err(|e| ServeError::ConfigError(e.to_string()))?;
    let policy = layers
        .merge(None)
        .map_err(|e| ServeError::ConfigError(e.to_string()))?;
    let layers = Arc::new(layers);

    // Org security patterns fail *closed*. An org configures these to keep
    // specific secrets out of a durable journal it cannot retroactively
    // scrub; silently starting with built-in rules only would journal
    // exactly the text the org asked never to be written, and would do so
    // behind a warning nobody reads. Refusing to start is recoverable
    // (fix the pattern); a leaked secret in an append-only journal is not.
    let redactor = Redactor::with_org_rules(&policy.org_security_patterns).map_err(|e| {
        ServeError::ConfigError(format!(
            "org security patterns failed to compile ({e}); refusing to start rather than \
             journaling text the org's redaction rules were meant to remove"
        ))
    })?;

    let started = redactor.sanitize(RawRuntimeEvent {
        timestamp: batman_protocol::Timestamp::now(),
        project_id: paths.project_id,
        run_id: None,
        kind: RawEventKind::RuntimeStarted,
    });
    db.append_event(started).await?;

    let repo_root = std::fs::canonicalize(&opts.repo).unwrap_or_else(|_| opts.repo.clone());

    // The credential store every worker-MCP subprocess's scope token is
    // verified against. Without this, `ServerConfig::default()`'s
    // `RejectAllWorkerVerifier` would reject every worker-MCP reconnect
    // even when an adapter below successfully embeds one via `mcp`.
    let scope_tokens = Arc::new(ScopeTokenStore::new());

    // `AdapterMcpConfig` needs this runtime's own verified binary path to
    // tell a supervised vendor process which `crewd coordination-mcp`
    // to spawn. `current_exe()` can fail (e.g. the executable was removed
    // after this process started); when it does, workers still start --
    // just without worker-coordination MCP tools -- rather than failing
    // the whole daemon. Never guessed: only a real resolved path is used.
    let mcp = match std::env::current_exe() {
        Ok(crewd_path) => Some(AdapterMcpConfig {
            scope_tokens: Arc::clone(&scope_tokens),
            project_id: paths.project_id,
            crewd_path,
            state_dir: paths.root.clone(),
            repository: repo_root.clone(),
        }),
        Err(err) => {
            let unavailable = redactor.sanitize(RawRuntimeEvent {
                timestamp: batman_protocol::Timestamp::now(),
                project_id: paths.project_id,
                run_id: None,
                kind: RawEventKind::Diagnostic {
                    level: DiagnosticLevel::Warning,
                    code: "worker_mcp_unavailable".to_string(),
                    fragments: vec![Classified {
                        class: ContentClass::Visible,
                        value: format!(
                            "could not resolve the running crewd binary's own path ({err}); \
                             workers will start without worker-coordination MCP tools"
                        ),
                    }],
                },
            });
            db.append_event(unavailable).await?;
            None
        }
    };
    let org_security_patterns = policy.org_security_patterns.clone();
    let nested_violation_action = policy.rollout_gates.nested_violation_action;
    let policy = Arc::new(policy);
    let registry = Arc::new(AdapterRegistry::new(
        Arc::new(
            PolicyEvaluator::new((*policy).clone()).with_daily_spend(Arc::new(
                crate::policy::JournalDailySpend::new(paths.database.clone(), paths.project_id),
            )),
        ),
        repo_root.clone(),
        mcp,
        org_security_patterns,
    ));
    let config = ServerConfig {
        binary_source: opts.binary_source,
        run_driver: Some(Arc::clone(&registry) as Arc<dyn crate::service::RunDriver>),
        repository: repo_root.clone(),
        worker_verifier: Arc::new(ScopeTokenVerifier::new(Arc::clone(&scope_tokens))),
        nested_violation_action,
        policy: Some((Arc::clone(&layers), Arc::clone(&policy))),
        ..ServerConfig::default()
    };
    let server = Server::bind(
        paths.socket.clone(),
        Arc::clone(&db),
        paths.project_id,
        config,
    )
    .await?
    .with_idle(opts.idle_seconds.map(Duration::from_secs));

    // Retrofit the real, server-owned `CoordinationBroker` into the
    // registry constructed above -- necessarily before `Server::bind`,
    // since it is threaded in via `ServerConfig::run_driver` -- so
    // OMP-RPC adapters' in-process host-tool bridge answers against the
    // same broker instance `coordination/*` RPC dispatch uses. See
    // `AdapterRegistry::set_broker`'s own doc comment for why this is a
    // post-construction setter rather than a constructor argument.
    registry.set_broker(server.coordination_broker());

    // Settle messages a crash left in `recorded`/`sent`. Like the
    // recovery sweep, this runs after bind but before the first
    // connection is accepted, so no live run can race it. One-shot, not
    // periodic: a running daemon settles its own messages.
    match server
        .coordination_broker()
        .sweep_unacknowledged_as_unknown()
        .await
    {
        Ok(0) => {}
        Ok(count) => tracing::info!(count, "unacknowledged_messages_settled_as_unknown"),
        Err(err) => tracing::warn!(error = %err.message, "message_settlement_sweep_failed"),
    }

    // Retention: prune once at startup, then every 24 hours. A one-shot
    // alone leaves a long-lived daemon's journal growing without bound,
    // which is the whole point of a retention period. A prune failure is
    // never fatal -- an oversized journal is recoverable, a daemon that
    // refuses to start is not.
    let retention = crate::audit::Retention::new(policy.retention.clone());
    if let Err(err) = retention.prune(&db).await {
        tracing::warn!(error = %err, "retention_prune_failed");
    }
    let retention_db = Arc::clone(&db);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        ticker.tick().await; // fires immediately; the startup prune above already ran
        loop {
            ticker.tick().await;
            if let Err(err) = retention.prune(&retention_db).await {
                tracing::warn!(error = %err, "retention_prune_failed");
            }
        }
    });

    server.serve(shutdown_signal()).await?;

    // Graceful shutdown: journal the stop record durably FIRST, then close the
    // database, and only then remove the socket and release the lock. The
    // socket's disappearance is therefore proof the journal shut down first.
    let stopping = redactor.sanitize(RawRuntimeEvent {
        timestamp: batman_protocol::Timestamp::now(),
        project_id: paths.project_id,
        run_id: None,
        kind: RawEventKind::RuntimeStopping,
    });
    let _ = db.append_event(stopping).await;
    // Reliably drain-and-close the database actor: `shutdown` takes `&self`, so
    // it runs even though `db` is an `Arc` still cloned into any in-flight
    // connection tasks. Only this clean path -- the actor thread actually
    // joined -- emits `db_actor_closed`, so the log line is proof the journal
    // shut down before the socket is removed below.
    match db.shutdown().await {
        Ok(()) => tracing::info!("db_actor_closed"),
        Err(err) => tracing::warn!(error = %err, "db actor shutdown did not complete cleanly"),
    }
    let _ = std::fs::remove_file(&paths.socket);
    drop(lock);

    tracing::info!("runtime_stopped");
    Ok(())
}

/// Resolves when the process receives `SIGINT` or `SIGTERM`.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(err) => {
            tracing::warn!(error = %err, "failed to install SIGTERM handler; only SIGINT will stop the runtime");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

// ------------------------------------------------------------------- lock

/// The JSON contents of `runtime.lock`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LockContents {
    pid: i32,
    /// A per-process instance token: a fresh identity for this daemon, so a
    /// recycled pid alone cannot be mistaken for the same runtime.
    instance_token: String,
    runtime_version: String,
    project_id: String,
    socket_path: String,
}

/// Holds the kernel `flock` for the runtime's lifetime. The advisory lock is
/// released automatically when the wrapped file descriptor is dropped (either
/// explicitly at the end of [`serve`] or by the kernel on process death), so
/// there is no lock *file* to remove -- it stays on disk and a stale lock is
/// simply one whose `flock` is once again acquirable.
struct LockGuard {
    _lock: Flock<std::fs::File>,
}

/// Acquires the single-instance lock by taking an exclusive, non-blocking
/// advisory `flock(2)` on the persistent lock file. On success we own the
/// runtime for the file descriptor's lifetime and (over)write the identity
/// metadata under the lock. On contention (`EWOULDBLOCK`) a live owner already
/// holds the lock, so we read its metadata and report
/// [`ServeError::AlreadyRunning`].
fn acquire_lock(paths: &RuntimePaths) -> Result<LockGuard, ServeError> {
    // Open (creating if absent) the persistent lock file WITHOUT O_EXCL: the
    // file's existence no longer conveys ownership -- the flock does. Rust's
    // std sets O_CLOEXEC on the descriptor; we request it explicitly too.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC)
        .open(&paths.lock)
        .map_err(|source| ServeError::Io {
            path: paths.lock.clone(),
            source,
        })?;

    let mut locked = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(locked) => locked,
        Err((_file, Errno::EWOULDBLOCK)) => {
            // A live owner holds the flock. It wrote its metadata under the
            // lock; read it (with a brief retry for the tiny window between the
            // owner acquiring the lock and finishing its write) for the report.
            let existing = read_lock_with_retry(&paths.lock).ok_or_else(|| ServeError::Io {
                path: paths.lock.clone(),
                source: std::io::Error::other(
                    "runtime lock is held but its metadata could not be read",
                ),
            })?;
            return Err(ServeError::AlreadyRunning(AlreadyRunning {
                status: "already_running",
                pid: existing.pid,
                project_id: existing.project_id,
                socket: existing.socket_path,
            }));
        }
        Err((_file, errno)) => {
            return Err(ServeError::Io {
                path: paths.lock.clone(),
                source: std::io::Error::from(errno),
            });
        }
    };

    // We own the lock. Truncate any stale metadata left by a crashed owner and
    // write our identity, then fsync so a concurrent loser reading under our
    // held flock sees a complete document.
    let contents = LockContents {
        pid: std::process::id() as i32,
        instance_token: uuid::Uuid::now_v7().to_string(),
        runtime_version: VERSION.to_string(),
        project_id: paths.project_id.to_string(),
        socket_path: paths.socket.display().to_string(),
    };
    let bytes = serde_json::to_vec(&contents).expect("LockContents serializes");
    write_lock_metadata(&mut locked, &bytes).map_err(|source| ServeError::Io {
        path: paths.lock.clone(),
        source,
    })?;

    Ok(LockGuard { _lock: locked })
}

/// Truncates the lock file to empty and writes `bytes` from its start, then
/// fsyncs. `file` is positioned at offset 0 by the fresh open, so no seek is
/// needed after truncation.
fn write_lock_metadata(file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Reads and parses the lock file, or `None` if it is absent or unparseable.
fn read_lock(path: &Path) -> Option<LockContents> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Like [`read_lock`], but tolerates the tiny window between a concurrent
/// holder taking the flock and finishing its metadata write by retrying for up
/// to ~500ms. Returns `None` only if the lock is still unparseable after that
/// -- i.e. genuinely empty or corrupt, not merely mid-write.
fn read_lock_with_retry(path: &Path) -> Option<LockContents> {
    for attempt in 0..20 {
        if let Some(contents) = read_lock(path) {
            return Some(contents);
        }
        if attempt < 19 {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    None
}

/// Whether the lock file currently has no live owner, judged by attempting to
/// take the exclusive advisory lock non-blockingly. Acquirable (or the file is
/// absent) means no live daemon holds it; `EWOULDBLOCK` means one does. The
/// probe lock is released immediately on return.
fn lock_file_is_free(path: &Path) -> bool {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(file) => file,
        // No lock file (or it vanished): nothing is running.
        Err(_) => return true,
    };
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        // Acquired: no live owner. Dropping the guard releases it at once.
        Ok(_guard) => true,
        // A live owner holds the flock.
        Err((_file, _errno)) => false,
    }
}

// ----------------------------------------------------------------- status

/// Options for [`status`].
#[derive(Debug, Clone)]
pub struct StatusOptions {
    pub state_dir: PathBuf,
    pub repo: PathBuf,
    /// Bounded retry window, in seconds, for startup races. `None` attempts
    /// exactly once.
    pub wait_seconds: Option<u64>,
}

/// Errors from [`status`].
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Security(#[from] SecurityError),
}

/// Connects to the runtime, initializes, and returns its `runtime/status`
/// result as JSON. Retries connecting until `wait_seconds` elapses to absorb
/// startup races; if the runtime never answers, returns `{"running": false}`.
///
/// # Errors
/// Returns [`StatusError`] only if the state paths cannot be resolved.
pub async fn status(opts: &StatusOptions) -> Result<Value, StatusError> {
    ensure_private_dir(&opts.state_dir)?;
    let paths = RuntimePaths::resolve(&opts.state_dir, &opts.repo)?;
    let repo_str = std::fs::canonicalize(&opts.repo)
        .unwrap_or_else(|_| opts.repo.clone())
        .display()
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(opts.wait_seconds.unwrap_or(0));
    loop {
        match query_status(&paths.socket, &repo_str).await {
            Ok(value) => return Ok(value),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(_) => return Ok(json!({ "running": false })),
        }
    }
}

/// One connect/initialize/`runtime/status` round-trip against `socket`.
async fn query_status(socket: &Path, repo_str: &str) -> Result<Value, anyhow::Error> {
    let stream = UnixStream::connect(socket).await?;
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    let init = json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "initialize",
        "params": {
            "client": { "name": "crewd", "version": VERSION },
            "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
            "repository": { "canonicalPath": repo_str, "vcsRoot": repo_str },
            "auth": { "role": "display", "instanceId": "crewd-status" },
            "capabilities": { "eventReplay": false, "maxFrameBytes": 65536 },
            "lastSequence": null
        }
    });
    send_frame(&mut write, &init).await?;
    let init_response = read_frame(&mut reader).await?;
    if init_response.get("error").is_some() {
        anyhow::bail!("initialize failed: {init_response}");
    }

    let request = json!({ "jsonrpc": "2.0", "id": "2", "method": "runtime/status" });
    send_frame(&mut write, &request).await?;
    let response = read_frame(&mut reader).await?;
    if let Some(error) = response.get("error") {
        anyhow::bail!("runtime/status failed: {error}");
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("runtime/status response had no result"))
}

async fn send_frame(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    value: &Value,
) -> Result<(), std::io::Error> {
    let mut line = serde_json::to_string(value).expect("request value serializes");
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await
}

async fn read_frame(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<Value, anyhow::Error> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        anyhow::bail!("runtime closed the connection before responding");
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

// ---------------------------------------------------------------- monitor

/// Options for [`monitor`].
#[derive(Debug, Clone)]
pub struct MonitorOptions {
    pub state_dir: PathBuf,
    pub repo: PathBuf,
    /// Renders only the run matching this id (its full, un-truncated wire
    /// form). `None` renders every run in the project.
    pub run_id: Option<String>,
}

/// Errors from [`monitor`].
#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Security(#[from] SecurityError),
    #[error("{0}")]
    Protocol(String),
}

/// One run's replayable monitor state: lifecycle state and the latest
/// human-readable activity description. Mirrors the embedded TypeScript
/// monitor's own `MonitorRow`/`reduceEvent`
/// (`packages/extension/src/monitor/model.ts`), reduced to the fields
/// this plain-text renderer uses.
#[derive(Debug, Clone, Default)]
struct MonitorRow {
    state: Option<String>,
    latest_activity: Option<String>,
}

/// The wire (camelCase) string a `RuntimeEventKind`-shaped value
/// serializes to, e.g. `"messageRecorded"` -- used identically to how the
/// embedded TypeScript monitor reads `event.payload.kind` directly off
/// the wire, without hand-matching every variant here.
fn wire_str<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// Applies one envelope to `rows`, returning the rendered line for its
/// run if the envelope carries a run id and contributes a state or
/// activity update. Extends the embedded TypeScript monitor's own
/// `eventPatch` (which predates the adapter/workspace event kinds) with
/// the mappings this daemon-side monitor can additionally see.
fn apply_and_render(
    rows: &mut HashMap<String, MonitorRow>,
    envelope: &EventEnvelope,
) -> Option<String> {
    let run_id = envelope.run_id.as_ref()?.to_string();
    let (state, activity): (Option<String>, Option<String>) = match &envelope.event {
        RuntimeEvent::RunEvent { state, .. } => (Some(state.clone()), Some(format!("run {state}"))),
        RuntimeEvent::MessageEvent {
            kind,
            delivery_state,
            ..
        } => (None, Some(format!("{} {delivery_state}", wire_str(kind)))),
        RuntimeEvent::ApprovalEvent { kind, action, .. } => (
            None,
            Some(
                if matches!(kind, batman_protocol::RuntimeEventKind::ApprovalRequested) {
                    format!("approval requested: {action}")
                } else {
                    "approval decided".to_string()
                },
            ),
        ),
        RuntimeEvent::ChildEvent { kind, .. } => (
            None,
            Some(
                match kind {
                    batman_protocol::RuntimeEventKind::ChildWorkerRequested => {
                        "child worker requested"
                    }
                    batman_protocol::RuntimeEventKind::ChildWorkerAccepted => {
                        "child worker accepted"
                    }
                    _ => "child worker request denied",
                }
                .to_string(),
            ),
        ),
        RuntimeEvent::AdapterMessageEvent { .. } => (None, Some("adapter message".to_string())),
        RuntimeEvent::AdapterToolEvent { kind, .. } => {
            (None, Some(format!("tool {}", wire_str(kind))))
        }
        RuntimeEvent::AdapterUsageEvent { .. } => (None, Some("usage reported".to_string())),
        RuntimeEvent::AdapterArtifactEvent { .. } => (None, Some("artifact produced".to_string())),
        RuntimeEvent::AdapterNestedWorkerEvent { .. } => {
            (None, Some("nested worker observed".to_string()))
        }
        RuntimeEvent::AdapterProtocolHealthEvent {
            healthy, detail, ..
        } => {
            // R12/R42/R57 invest in a precise detail (the vendor's error
            // subtype, the raw stop reason); surface it instead of a
            // constant label (R91).
            let label = if *healthy {
                "protocol healthy"
            } else {
                "protocol unhealthy"
            };
            (
                None,
                Some(match detail {
                    Some(detail) => format!("{label}: {detail}"),
                    None => label.to_string(),
                }),
            )
        }
        RuntimeEvent::WorkspaceEvent { kind, .. } => {
            (None, Some(format!("workspace {}", wire_str(kind))))
        }
        _ => return None,
    };

    let row = rows.entry(run_id.clone()).or_default();
    if let Some(state) = state {
        row.state = Some(state);
    }
    if let Some(activity) = activity {
        row.latest_activity = Some(activity);
    }

    let short_id = &run_id[..run_id.len().min(8)];
    let state_display = row.state.as_deref().unwrap_or("unknown");
    Some(match &row.latest_activity {
        Some(activity) => format!("{short_id} · {state_display} · {activity}"),
        None => format!("{short_id} · {state_display}"),
    })
}

/// Connects to the runtime as a `display` principal, replays every event
/// from sequence 0, renders one line per contributing envelope, then
/// follows new events live until interrupted (`SIGINT`/`SIGTERM`). A
/// transient disconnect reconnects and replays from the highest sequence
/// already rendered plus one, so no visible line is duplicated.
///
/// # Errors
/// Returns [`MonitorError`] if the state paths cannot be resolved or the
/// very first connection/handshake fails; once at least one connection has
/// succeeded, a later disconnect retries rather than returning `Err`.
pub async fn monitor(opts: &MonitorOptions) -> Result<(), MonitorError> {
    ensure_private_dir(&opts.state_dir)?;
    let paths = RuntimePaths::resolve(&opts.state_dir, &opts.repo)?;
    let repo_str = std::fs::canonicalize(&opts.repo)
        .unwrap_or_else(|_| opts.repo.clone())
        .display()
        .to_string();

    let mut rows: HashMap<String, MonitorRow> = HashMap::new();
    let mut last_sequence: u64 = 0;
    let mut connected_once = false;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        let connect_result = tokio::select! {
            result = connect_and_catch_up(&paths.socket, &repo_str, last_sequence, opts.run_id.as_deref(), &mut rows) => result,
            () = &mut shutdown => return Ok(()),
        };
        let (mut reader, _writer) = match connect_result {
            Ok((reader, writer, replayed_through)) => {
                connected_once = true;
                last_sequence = last_sequence.max(replayed_through);
                (reader, writer)
            }
            Err(_) if connected_once => {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(500)) => continue,
                    () = &mut shutdown => return Ok(()),
                }
            }
            Err(err) => return Err(MonitorError::Protocol(err.to_string())),
        };

        loop {
            tokio::select! {
                frame = read_frame(&mut reader) => {
                    match frame {
                        Ok(notification) => {
                            if notification.get("method").and_then(Value::as_str) != Some("events/event") {
                                continue;
                            }
                            let Some(params) = notification.get("params") else { continue };
                            let Ok(envelope) = serde_json::from_value::<EventEnvelope>(params.clone()) else { continue };
                            last_sequence = last_sequence.max(envelope.sequence);
                            if run_filter_matches(&envelope, opts.run_id.as_deref())
                                && let Some(line) = apply_and_render(&mut rows, &envelope)
                            {
                                println!("{line}");
                            }
                        }
                        Err(_) => break,
                    }
                }
                () = &mut shutdown => return Ok(()),
            }
        }
    }
}

/// Whether `envelope` should be rendered given an optional `--run-id`
/// filter: every envelope when unset, only the matching run's when set.
fn run_filter_matches(envelope: &EventEnvelope, filter: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(wanted) => envelope
            .run_id
            .as_ref()
            .is_some_and(|id| id.to_string() == wanted),
    }
}

/// One connect/initialize/`events/replay`/`events/subscribe` sequence:
/// replays every event after `after_sequence`, rendering each
/// run-filter-matching, state/activity-contributing one, then subscribes
/// for live delivery. Returns the open reader/writer halves and the
/// highest sequence replayed, for the caller's live-read loop and
/// reconnect checkpoint respectively.
async fn connect_and_catch_up(
    socket: &Path,
    repo_str: &str,
    after_sequence: u64,
    run_id_filter: Option<&str>,
    rows: &mut HashMap<String, MonitorRow>,
) -> Result<
    (
        BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
        u64,
    ),
    anyhow::Error,
> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);

    let init = json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "initialize",
        "params": {
            "client": { "name": "crewd", "version": VERSION },
            "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
            "repository": { "canonicalPath": repo_str, "vcsRoot": repo_str },
            "auth": { "role": "display", "instanceId": "crewd-monitor" },
            "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
            "lastSequence": null
        }
    });
    send_frame(&mut write, &init)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let init_response = read_frame(&mut reader).await?;
    if init_response.get("error").is_some() {
        anyhow::bail!("initialize failed: {init_response}");
    }

    let replay_request = json!({
        "jsonrpc": "2.0",
        "id": "2",
        "method": "events/replay",
        "params": { "afterSequence": after_sequence }
    });
    send_frame(&mut write, &replay_request)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let replay_response = read_frame(&mut reader).await?;
    if let Some(error) = replay_response.get("error") {
        anyhow::bail!("events/replay failed: {error}");
    }
    let mut highest_sequence = after_sequence;
    let empty = Vec::new();
    let envelopes = replay_response
        .get("result")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for raw in envelopes {
        let Ok(envelope) = serde_json::from_value::<EventEnvelope>(raw.clone()) else {
            continue;
        };
        highest_sequence = highest_sequence.max(envelope.sequence);
        if run_filter_matches(&envelope, run_id_filter)
            && let Some(line) = apply_and_render(rows, &envelope)
        {
            println!("{line}");
        }
    }

    let subscribe_request = json!({
        "jsonrpc": "2.0",
        "id": "3",
        "method": "events/subscribe",
        "params": {}
    });
    send_frame(&mut write, &subscribe_request)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let subscribe_response = read_frame(&mut reader).await?;
    if let Some(error) = subscribe_response.get("error") {
        anyhow::bail!("events/subscribe failed: {error}");
    }

    Ok((reader, write, highest_sequence))
}

// ------------------------------------------------------------------- stop

/// Options for [`stop`].
#[derive(Debug, Clone)]
pub struct StopOptions {
    pub state_dir: PathBuf,
    pub repo: PathBuf,
}

/// The outcome of a [`stop`] request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// No live runtime was found to stop.
    NotRunning,
    /// A live runtime was signalled and its socket was removed.
    Stopped,
}

/// Errors from [`stop`].
#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Security(#[from] SecurityError),
    #[error("failed to signal the runtime (pid {pid}): {source}")]
    Signal { pid: i32, source: nix::errno::Errno },
    #[error("timed out waiting for the runtime to shut down and remove its socket")]
    Timeout,
}

/// Gracefully stops the runtime for a repository: validates the lock holder is
/// live, sends `SIGTERM`, and waits for the socket to disappear (which the
/// daemon does only after its journal shutdown completes).
///
/// Deliberately unarbitrated, unlike the in-band `runtime/shutdown` RPC
/// (R82): this is the operator path -- whoever can signal the process can
/// stop it -- while the RPC path refuses when other work is live unless
/// forced.
///
/// # Errors
/// Returns [`StopError`] if the paths cannot be resolved, the signal cannot be
/// delivered, or the runtime does not shut down within the wait window.
pub async fn stop(opts: &StopOptions) -> Result<StopOutcome, StopError> {
    ensure_private_dir(&opts.state_dir)?;
    let paths = RuntimePaths::resolve(&opts.state_dir, &opts.repo)?;

    let Some(lock) = read_lock(&paths.lock) else {
        return Ok(StopOutcome::NotRunning);
    };

    // Validate liveness via the advisory lock before signalling: if we can
    // take the flock ourselves, no live daemon holds it, so the pid recorded
    // in the (now stale) metadata may have been recycled -- never signal it.
    // A held flock proves the owner process that wrote this metadata is still
    // alive, closing the recycled-pid hole a bare `kill(pid, 0)` left open.
    if lock_file_is_free(&paths.lock) {
        return Ok(StopOutcome::NotRunning);
    }

    signal::kill(Pid::from_raw(lock.pid), Signal::SIGTERM).map_err(|source| StopError::Signal {
        pid: lock.pid,
        source,
    })?;

    // Wait for the daemon to journal its stop and remove the socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !paths.socket.exists() {
            return Ok(StopOutcome::Stopped);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(StopError::Timeout)
}

/// Whether a live daemon currently holds this repository's runtime lock.
///
/// The advisory flock is the liveness proof, exactly as [`stop`] uses it:
/// the socket file alone is not one -- an unclean crash (SIGKILL, machine
/// crash) leaves `runtime.sock` on disk, and only the graceful shutdown
/// path removes it. Used by `crewd lease release` to refuse out-of-band
/// writes only when a daemon is genuinely serving (R86 review W1).
#[must_use]
pub fn runtime_is_live(lock_path: &Path) -> bool {
    read_lock(lock_path).is_some() && !lock_file_is_free(lock_path)
}

// ---------------------------------------------------------------- logging

/// A [`tracing_subscriber`] `MakeWriter` over a shared append-mode file.
#[derive(Clone)]
struct FileWriter {
    file: Arc<std::fs::File>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileWriter {
    type Writer = FileHandle;
    fn make_writer(&'a self) -> Self::Writer {
        FileHandle(Arc::clone(&self.file))
    }
}

struct FileHandle(Arc<std::fs::File>);

impl std::io::Write for FileHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

/// Installs the process's structured (JSON) tracing subscriber: to stderr in
/// foreground mode, to a private `runtime.log` when detached.
fn init_logging(foreground: bool, log_path: &Path) -> Result<(), ServeError> {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if foreground {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_writer(std::io::stderr)
            .with_env_filter(filter)
            .try_init();
    } else {
        ensure_private_file(log_path)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|source| ServeError::Io {
                path: log_path.to_path_buf(),
                source,
            })?;
        let writer = FileWriter {
            file: Arc::new(file),
        };
        let _ = tracing_subscriber::fmt()
            .json()
            .with_writer(writer)
            .with_env_filter(filter)
            .try_init();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use batman_protocol::{
        EventSource, ProjectId, RunId, RuntimeEvent, TaskId, Timestamp, WorkerId,
    };

    fn health_envelope(healthy: bool, detail: Option<&str>) -> EventEnvelope {
        let run_id = RunId::new();
        EventEnvelope {
            sequence: 1,
            timestamp: Timestamp::now(),
            project_id: ProjectId::new(),
            task_id: None,
            worker_id: None,
            run_id: Some(run_id),
            parent_worker_id: None,
            source: EventSource::Runtime,
            event: RuntimeEvent::AdapterProtocolHealthEvent {
                run_id,
                task_id: TaskId::new(),
                worker_id: WorkerId::new(),
                healthy,
                detail: detail.map(str::to_string),
            },
            vendor_event_ref: None,
        }
    }

    /// R91: the status row must render the journaled detail -- the
    /// vendor's error subtype / raw stop reason -- not a constant label.
    /// Mirrors the two cases model.test.ts pins for the embedded monitor.
    #[test]
    fn status_row_renders_the_protocol_health_detail() {
        let mut rows = HashMap::new();
        let line = apply_and_render(
            &mut rows,
            &health_envelope(false, Some("error result: rate_limited")),
        )
        .expect("a health event contributes an activity");
        assert!(
            line.contains("protocol unhealthy: error result: rate_limited"),
            "the detail must reach the operator, not a constant label: {line}"
        );

        let healthy = apply_and_render(&mut rows, &health_envelope(true, None))
            .expect("a healthy event contributes an activity");
        assert!(
            healthy.contains("protocol healthy"),
            "the healthy edge renders its bare label: {healthy}"
        );
    }
}
