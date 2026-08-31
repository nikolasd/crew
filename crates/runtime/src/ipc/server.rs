//! The runtime socket server: binds the per-repository Unix domain socket,
//! enforces the same-user peer-credential boundary on every accepted
//! connection before any JSON is parsed, and hands each accepted connection
//! to [`super::connection`].

use std::future::Future;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crew_protocol::ProjectId;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast};

use crate::db::DatabaseHandle;

use super::{IpcError, PeerCredentials, ServerConfig};

/// State shared by every connection served by a [`Server`].
pub(crate) struct Shared {
    pub(crate) db: Arc<DatabaseHandle>,
    pub(crate) config: ServerConfig,
    pub(crate) project_id: ProjectId,
    pub(crate) started_at: Instant,
    pub(crate) events_tx: broadcast::Sender<crew_protocol::EventEnvelope>,
    /// Number of connections currently admitted and being served. Used to
    /// decide whether the runtime is idle.
    pub(crate) active_connections: Arc<AtomicUsize>,
    /// Fired by an in-band `runtime/shutdown` request to trigger a graceful
    /// shutdown of the accept loop.
    pub(crate) shutdown: Arc<Notify>,
    /// Routes every orchestration method to the domain repository.
    pub(crate) orchestration: Arc<crate::service::OrchestrationService>,
    /// Routes every worker-safe `coordination/*` method to the domain
    /// repository.
    pub(crate) coordination: Arc<crate::coordination::CoordinationBroker>,
    /// The mid-run nested-worker policy violation service, shared with
    /// [`Self::orchestration`] and exposed post-bind for the adapter
    /// registry's resume support bundle (see `Server::violation_service`).
    pub(crate) violation: Arc<crate::policy::ViolationService>,
    /// The dashboard's live URL (token included), set at most once, after
    /// `bind` -- like [`Self::coordination`]/the violation service, the
    /// dashboard only exists post-bind (it subscribes to this same
    /// `events_tx`). `None` until (and unless) a dashboard bind succeeds;
    /// read by `runtime/status` (CREW-35).
    pub(crate) dashboard_url: std::sync::OnceLock<String>,
    /// The directory the socket and SQLite journal both live in (siblings
    /// under `RuntimePaths::resolve`'s own root) -- named in the remedy for
    /// a journal that fails to deserialize on replay (CREW-52), and in
    /// `runtime/status`'s `state_root` (D25).
    pub(crate) state_dir: PathBuf,
    /// The Unix domain socket path this server is actually bound to.
    /// `runtime/status`'s `socket_path` (D25): a two-daemon mixup (two
    /// `crewd` processes against different state roots, neither aware of
    /// the other) previously had no way to see which one a client was
    /// actually talking to.
    pub(crate) socket_path: PathBuf,
}

impl Shared {
    /// The number of runs the injected driver is actively driving (R87):
    /// the adapter registry's live-adapter count in production, `0` when
    /// no driver is wired. Consumed by `runtime/status` and the
    /// idle-shutdown decision, so both always agree. Two deliberate
    /// properties: a queued run with no adapter does not suppress idle
    /// shutdown (its submitting client's connection does, and boot
    /// recovery owns orphaned rows); and a run that settles without a
    /// `ProcessExited` (terminal-degraded profiles) stays counted, which
    /// fails safe -- the daemon refuses idle self-termination and
    /// unforced in-band shutdown rather than killing something it cannot
    /// see.
    pub(crate) fn active_run_count(&self) -> usize {
        self.config
            .run_driver
            .as_ref()
            .map_or(0, |driver| driver.active_run_count())
    }
}

/// Per-connection context derived from the accepted peer's credentials.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnContext {
    /// Whether the runtime verified the peer's OS credentials.
    pub(crate) peer_credentials_verified: bool,
    /// The peer process's pid, if known (used for worker-MCP ancestry).
    pub(crate) peer_pid: Option<i32>,
}

/// The runtime socket server. Bind once, then [`Server::serve`] until a
/// shutdown signal.
pub struct Server {
    listener: UnixListener,
    socket: PathBuf,
    shared: Arc<Shared>,
    owner_only_verified: bool,
    idle: Option<Duration>,
}

/// The longest a Unix domain socket path may be, including its NUL
/// terminator, before `bind(2)` truncates or rejects it. macOS `sun_path` is
/// 104 bytes; Linux allows 108.
#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_MAX: usize = 108;

/// Whether `socket` fits within the platform `sun_path` bound (leaving room
/// for the NUL terminator). Guards against a silently truncated bind.
#[must_use]
pub fn socket_path_within_limit(socket: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    socket.as_os_str().as_bytes().len() < SUN_PATH_MAX
}

impl Server {
    /// Binds the runtime socket at `socket`, removing any stale socket file
    /// left by a previous run, and tightening the socket to mode `0600`.
    ///
    /// # Errors
    /// Returns [`IpcError`] if the socket cannot be bound or secured.
    pub async fn bind(
        socket: PathBuf,
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        config: ServerConfig,
    ) -> Result<Self, IpcError> {
        // Guard the platform `sun_path` bound before attempting to bind: an
        // over-long path would otherwise be silently truncated.
        if !socket_path_within_limit(&socket) {
            use std::os::unix::ffi::OsStrExt;
            return Err(IpcError::SocketPathTooLong {
                path: socket.clone(),
                len: socket.as_os_str().as_bytes().len(),
                limit: SUN_PATH_MAX,
            });
        }

        // Remove a stale socket file from a previous run so bind() succeeds.
        match std::fs::remove_file(&socket) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(IpcError::Io(err)),
        }

        let listener = UnixListener::bind(&socket).map_err(|source| IpcError::Bind {
            path: socket.clone(),
            source,
        })?;

        // Tighten the socket file itself to owner-only. The parent directory
        // is already mode 0700 (see RuntimePaths::resolve), but defense in
        // depth: the socket node should not be group/other accessible.
        let mut perms = std::fs::metadata(&socket)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&socket, perms)?;

        let owner_only_verified = config
            .owner_only_override
            .unwrap_or_else(|| check_owner_only(&socket, config.euid));

        let (events_tx, _events_rx) = broadcast::channel(64);

        // Create workspace services.
        let lease_db_path = socket
            .parent()
            .map(|p| p.join("workspace-leases.db"))
            .unwrap_or_else(|| std::path::PathBuf::from("/tmp/workspace-leases.db"));
        let lease_service = Arc::new(
            crate::workspace::LeaseService::open(project_id, &lease_db_path)
                .expect("failed to open lease service database"),
        );
        let artifact_store = config
            .artifact_store
            .clone()
            .unwrap_or_else(|| Arc::new(crate::workspace::ArtifactStore::new()));

        // The mid-run nested-worker policy violation service (Hardening
        // plan Task 1). Constructed here (not in `lifecycle::serve()`
        // alongside `AdapterRegistry`) because it needs the real
        // `events_tx`, which only exists once `Server::bind` creates it
        // above -- the same ordering constraint documented on
        // `Server::coordination_broker`. `config.run_driver` is already
        // available (constructed by the caller before `bind`), so this
        // has no construction-order cycle with `AdapterRegistry`.
        let violation_service = {
            // WP26: the service's cancellation intents and acknowledgements
            // are durable journal text, so they get the full configured
            // Redactor -- built-in rules plus the compiled
            // `security.patterns` -- never a built-ins-only instance. The
            // same patterns already failed closed at startup (lifecycle),
            // so a compile error here can only be a bug.
            let org_patterns = config
                .policy
                .as_ref()
                .map(|(_, policy)| policy.org_security_patterns.clone())
                .unwrap_or_default();
            let redactor = crate::security::redaction::Redactor::with_org_rules(&org_patterns)
                .map_err(IpcError::Configuration)?;
            Arc::new(crate::policy::ViolationService::new(
                db.clone(),
                project_id,
                events_tx.clone(),
                config.run_driver.clone(),
                config.nested_violation_action,
                redactor,
            ))
        };

        let pane_reopen = config.pane_reopen.as_ref().map(|support| {
            (
                std::sync::Arc::new(crate::display::PaneCoordinator::new(
                    std::sync::Arc::clone(&support.display_registry),
                    db.clone(),
                    project_id,
                    events_tx.clone(),
                    support.crewd_path.clone(),
                    support.state_dir.clone(),
                    config.repository.clone(),
                )),
                support.panes_dir.clone(),
            )
        });

        let mut orchestration = crate::service::OrchestrationService::new(
            db.clone(),
            project_id,
            config.run_driver.clone(),
            config.approval_callback.clone(),
            violation_service.clone(),
            events_tx.clone(),
            lease_service.clone(),
            artifact_store.clone(),
            config.repository.clone(),
            config.turn_budget_default,
            config
                .activity_clock
                .clone()
                .unwrap_or_else(|| std::sync::Arc::new(crate::adapter::ActivityClock::new())),
        );
        if let Some((config_paths, policy)) = config.policy.clone() {
            orchestration = orchestration.with_policy(config_paths, policy);
        }
        if let Some(retention) = config.retention.clone() {
            orchestration = orchestration.with_retention(retention);
        }
        if let Some((coordinator, panes_dir)) = pane_reopen {
            orchestration = orchestration.with_pane_reopen(coordinator, panes_dir);
        }
        let orchestration = Arc::new(orchestration);
        // Worker-supplied `coordination/*` text is durable journal text, so
        // the broker gets the full configured Redactor -- built-in rules
        // plus the compiled `security.patterns` -- never a built-ins-only
        // instance, for exactly the reason the ViolationService above does.
        // The same patterns already failed closed at startup (lifecycle),
        // so a compile error here can only be a bug.
        let broker_org_patterns = config
            .policy
            .as_ref()
            .map(|(_, policy)| policy.org_security_patterns.clone())
            .unwrap_or_default();
        let broker_redactor = Arc::new(
            crate::security::redaction::Redactor::with_org_rules(&broker_org_patterns)
                .map_err(IpcError::Configuration)?,
        );
        let coordination = Arc::new(crate::coordination::CoordinationBroker::new(
            db.clone(),
            project_id,
            events_tx.clone(),
            lease_service,
            artifact_store,
            broker_redactor,
        ));

        let shared = Arc::new(Shared {
            db,
            config,
            project_id,
            started_at: Instant::now(),
            events_tx,
            active_connections: Arc::new(AtomicUsize::new(0)),
            shutdown: Arc::new(Notify::new()),
            orchestration,
            coordination,
            violation: Arc::clone(&violation_service),
            dashboard_url: std::sync::OnceLock::new(),
            state_dir: socket
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| socket.clone()),
            socket_path: socket.clone(),
        });

        Ok(Self {
            listener,
            socket,
            shared,
            owner_only_verified,
            idle: None,
        })
    }

    /// The bound socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Sets the idle interval: the server exits once no connection has been
    /// admitted and no run is active for this long. `None` (the default)
    /// never idle-exits.
    #[must_use]
    pub fn with_idle(mut self, idle: Option<Duration>) -> Self {
        self.idle = idle;
        self
    }

    /// A handle that, when notified, triggers a graceful shutdown of the
    /// accept loop -- used to serve an in-band `runtime/shutdown` request.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shared.shutdown)
    }

    /// The [`crate::coordination::CoordinationBroker`] this server binds
    /// `coordination/*` dispatch to, exposed so a caller that constructed
    /// an [`crate::adapter::registry::AdapterRegistry`] before `bind`
    /// (necessarily -- it is threaded in via [`ServerConfig::run_driver`])
    /// can retrofit that registry's own OMP-RPC in-process host-tool
    /// bridge with the SAME broker instance after the fact, rather than
    /// each holding an independent (and independently rate-limited)
    /// broker.
    #[must_use]
    pub fn coordination_broker(&self) -> Arc<crate::coordination::CoordinationBroker> {
        Arc::clone(&self.shared.coordination)
    }

    /// The live event broadcast every committed mutation fans out to.
    /// Exposed (like [`Server::coordination_broker`]) for projections
    /// constructed after `bind` -- the dashboard's SSE route subscribes to
    /// exactly this sender, so it sees the same envelopes as
    /// `events/subscribe` clients.
    #[must_use]
    pub fn events_sender(&self) -> broadcast::Sender<crew_protocol::EventEnvelope> {
        self.shared.events_tx.clone()
    }

    /// Records the dashboard's live URL (token included) for `runtime/status`
    /// to report, once the dashboard has bound successfully -- post-bind for
    /// the same reason as [`Self::coordination_broker`]/[`Self::events_sender`]:
    /// the dashboard only exists after `Server::bind` has already produced
    /// this server's own event sender. A no-op if called more than once
    /// (there is exactly one dashboard per daemon run).
    pub fn set_dashboard_url(&self, url: String) {
        let _ = self.shared.dashboard_url.set(url);
    }

    /// The policy-violation service this server constructed at bind time,
    /// exposed (like [`Server::coordination_broker`] and for the same
    /// construction-order reason: it only exists once `bind` has created
    /// the live event broadcast) so the adapter registry's resume support
    /// bundle shares the SAME instance orchestration dispatch uses, never
    /// a second service with independent quarantine state.
    #[must_use]
    pub fn violation_service(&self) -> Arc<crate::policy::ViolationService> {
        Arc::clone(&self.shared.violation)
    }

    /// Accepts and serves connections until `shutdown` resolves.
    ///
    /// Each accepted connection has its peer credentials read *before* any
    /// bytes are consumed; a connection whose peer uid differs from the
    /// runtime's is dropped immediately, before parsing.
    ///
    /// # Errors
    /// Returns [`IpcError`] only on a fatal accept-loop error; ordinary
    /// per-connection failures are logged and the loop continues.
    pub async fn serve<F>(self, shutdown: F) -> Result<(), IpcError>
    where
        F: Future<Output = ()> + Send,
    {
        let Server {
            listener,
            shared,
            owner_only_verified,
            idle,
            socket: _,
        } = self;

        tokio::pin!(shutdown);

        let in_band_shutdown = Arc::clone(&shared.shutdown);
        // The server is idle from the moment it starts serving with no
        // connected clients; the interval below observes when that changes.
        let mut idle_since: Option<Instant> = idle.map(|_| Instant::now());
        let mut ticker = idle.map(|_| tokio::time::interval(Duration::from_millis(100)));

        loop {
            tokio::select! {
                () = &mut shutdown => break,
                () = in_band_shutdown.notified() => break,
                _ = async { ticker.as_mut().expect("ticker present when idle set").tick().await },
                    if ticker.is_some() =>
                {
                    let limit = idle.expect("idle set when ticker present");
                    let connections = shared.active_connections.load(Ordering::Relaxed);
                    let runs = shared.active_run_count();
                    if connections == 0 && runs == 0 {
                        match idle_since {
                            Some(since) => {
                                if should_idle_shutdown(connections, runs, since.elapsed(), limit) {
                                    break;
                                }
                            }
                            None => idle_since = Some(Instant::now()),
                        }
                    } else {
                        idle_since = None;
                    }
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            idle_since = None;
                            Self::admit(stream, &shared, owner_only_verified);
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "failed to accept runtime socket connection");
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Applies the same-user peer-credential boundary and, if the connection
    /// is admitted, spawns its handler. Rejected connections are dropped here
    /// -- before a single byte of JSON is read.
    fn admit(stream: UnixStream, shared: &Arc<Shared>, owner_only_verified: bool) {
        let creds: PeerCredentials = shared.config.credential_reader.read(&stream);
        let euid = shared.config.euid;

        let (admitted, peer_verified) = match creds.uid {
            Some(uid) if uid == euid => (true, true),
            Some(uid) => {
                tracing::warn!(
                    peer_uid = uid,
                    expected = euid,
                    "rejecting connection from a different uid before parsing"
                );
                (false, true)
            }
            None => {
                // Peer credentials unavailable: fail closed unless the
                // owner-only directory/socket permission check passed.
                if owner_only_verified {
                    (true, false)
                } else {
                    tracing::warn!(
                        "rejecting connection: peer credentials unavailable and owner-only check failed"
                    );
                    (false, false)
                }
            }
        };

        if !admitted {
            drop(stream);
            return;
        }

        let ctx = ConnContext {
            peer_credentials_verified: peer_verified,
            peer_pid: creds.pid,
        };
        let shared = Arc::clone(shared);
        shared.active_connections.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let connections = Arc::clone(&shared.active_connections);
            super::connection::handle(stream, ctx, shared).await;
            connections.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

/// Decides whether the runtime should idle-exit: only when no connection is
/// live, no run is active, and the idle interval has fully elapsed. The
/// connection and run counts are ANDed, so either a live client or an active
/// run suppresses shutdown.
#[must_use]
pub fn should_idle_shutdown(
    active_connections: usize,
    active_runs: usize,
    idle_elapsed: Duration,
    idle_limit: Duration,
) -> bool {
    active_connections == 0 && active_runs == 0 && idle_elapsed >= idle_limit
}

/// Checks that `socket`'s parent directory is owned by `euid` and accessible
/// only by its owner (no group/other permission bits).
fn check_owner_only(socket: &Path, euid: u32) -> bool {
    let dir = socket.parent().unwrap_or_else(|| Path::new("/"));
    match std::fs::metadata(dir) {
        Ok(meta) => meta.uid() == euid && (meta.mode() & 0o077) == 0,
        Err(_) => false,
    }
}

/// Reads the connected peer's OS credentials via the kernel. Returns whatever
/// the platform can provide; fields the platform cannot report are `None`.
#[cfg(target_os = "macos")]
pub(crate) fn read_system_peer_credentials(stream: &UnixStream) -> PeerCredentials {
    use std::os::fd::{AsRawFd, BorrowedFd};

    use nix::sys::socket::{getsockopt, sockopt};

    let fd = stream.as_raw_fd();
    // SAFETY: `stream` outlives this borrow; the fd is valid for the call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let uid = getsockopt(&borrowed, sockopt::LocalPeerCred)
        .ok()
        .map(|cred| cred.uid());
    let pid = getsockopt(&borrowed, sockopt::LocalPeerPid).ok();
    PeerCredentials { uid, pid }
}

/// Reads the connected peer's OS credentials via the kernel. Returns whatever
/// the platform can provide; fields the platform cannot report are `None`.
#[cfg(target_os = "linux")]
pub(crate) fn read_system_peer_credentials(stream: &UnixStream) -> PeerCredentials {
    use std::os::fd::{AsRawFd, BorrowedFd};

    use nix::sys::socket::{getsockopt, sockopt};

    let fd = stream.as_raw_fd();
    // SAFETY: `stream` outlives this borrow; the fd is valid for the call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    match getsockopt(&borrowed, sockopt::PeerCredentials) {
        Ok(cred) => PeerCredentials {
            uid: Some(cred.uid()),
            pid: Some(cred.pid()),
        },
        Err(_) => PeerCredentials::default(),
    }
}

/// Reads the connected peer's OS credentials via the kernel. On platforms
/// without a supported peer-credential mechanism, reports nothing.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn read_system_peer_credentials(_stream: &UnixStream) -> PeerCredentials {
    PeerCredentials::default()
}
