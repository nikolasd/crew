//! The initialized JSON-RPC runtime socket protocol.
//!
//! Crew's runtime speaks JSON-RPC 2.0 over NDJSON on a per-repository Unix
//! domain socket. This module owns the server side: binding the socket,
//! enforcing the operating-system security boundary (same-user peer
//! credentials, checked before a single byte of JSON is parsed), the bounded
//! `initialize` handshake with protocol-version and frame-size negotiation,
//! role-scoped method dispatch built from the authenticated
//! [`ClientPrincipal`], and durable event replay.
//!
//! Each accepted connection is split into one reader task and one serialized
//! writer task (see [`connection`]); a database transaction is never held
//! across socket I/O -- the [`crate::db::DatabaseHandle`] actor already
//! guarantees that.

mod connection;
mod server;

use std::sync::Arc;

use batman_protocol::{BatmanMethod, ClientRole, RunId, TaskId, VersionRange, WorkerId};
use tokio::net::UnixStream;

pub use server::{Server, should_idle_shutdown, socket_path_within_limit};

/// The lowest frame size, in bytes, a client may negotiate. Offers below this
/// are rejected with `INVALID_PARAMS`: a client that cannot buffer 64 KiB
/// cannot participate in the protocol.
pub const PROTOCOL_MIN_FRAME_BYTES: u32 = 64 * 1024;

/// The default (and hard ceiling) runtime maximum frame size: 4 MiB. This is
/// also the bootstrap hard limit applied before `initialize` completes.
pub const DEFAULT_MAX_FRAME_BYTES: u32 = 4 * 1024 * 1024;

/// The single protocol version this runtime implements. Re-exported from
/// `batman-protocol`, which owns it: `batman-xtask` records the same value
/// as release provenance, and a second definition here could ship a
/// manifest claiming a version the runtime does not speak.
pub use batman_protocol::PROTOCOL_VERSION as RUNTIME_PROTOCOL_VERSION;

/// The durable database schema version reported by `runtime/status`.
pub const SCHEMA_VERSION: u32 = 1;

/// The inclusive range of protocol versions the runtime supports.
#[must_use]
pub const fn runtime_supported_versions() -> VersionRange {
    batman_protocol::supported_versions()
}

/// The peer's operating-system credentials for an accepted connection, as
/// reported by a [`PeerCredentialReader`]. Either field may be `None` on a
/// platform (or in a test) that cannot report it.
#[derive(Debug, Clone, Copy, Default)]
pub struct PeerCredentials {
    /// The peer process's effective user id, if known.
    pub uid: Option<u32>,
    /// The peer process's pid, if known.
    pub pid: Option<i32>,
}

/// Reads the operating-system credentials of a connected peer. Injectable so
/// tests can simulate a matching UID, a mismatched UID, or a platform that
/// cannot report peer credentials at all.
pub trait PeerCredentialReader: Send + Sync {
    /// Reads the peer credentials for `stream`.
    fn read(&self, stream: &UnixStream) -> PeerCredentials;
}

/// The default reader, which queries the kernel for the connected peer's
/// credentials (`getsockopt(LOCAL_PEERCRED)`/`SO_PEERCRED`).
pub struct SystemPeerCredentialReader;

impl PeerCredentialReader for SystemPeerCredentialReader {
    fn read(&self, stream: &UnixStream) -> PeerCredentials {
        server::read_system_peer_credentials(stream)
    }
}

/// A run the worker-MCP credential verifier bound a reconnect credential to.
#[derive(Debug, Clone, Copy)]
pub struct ScopedRun {
    /// The supervised run the credential is scoped to.
    pub run_id: RunId,
    /// The task that run belongs to, from the same scope-token record.
    pub task_id: TaskId,
    /// The worker that run belongs to, from the same scope-token record --
    /// the authoritative sender identity for anything this connection
    /// sends, never a value the client supplies itself.
    pub worker_id: WorkerId,
}

/// Why a worker-MCP reconnect credential was rejected.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// No credential store is installed yet (the foundation default). Every
    /// worker-MCP initialization is rejected until the coordination plan
    /// installs one.
    #[error("no worker credential store is installed")]
    NoCredentialStore,
    /// The presented scope token is not valid or not live.
    #[error("invalid or expired scope token")]
    InvalidToken,
    /// The connecting peer is outside the supervised vendor-process ancestry
    /// the credential is bound to.
    #[error("peer is outside the supervised vendor-process ancestry")]
    OutsideAncestry,
    /// The run the credential is scoped to is no longer live.
    #[error("the scoped run is no longer live")]
    RunNotLive,
}

/// Verifies a worker-MCP reconnect credential against a live, run-bound
/// credential store, consulting the peer's process identity/ancestry.
///
/// Injectable until the coordination plan installs the real credential store;
/// the foundation default ([`RejectAllWorkerVerifier`]) rejects every
/// worker-MCP initialization.
pub trait WorkerCredentialVerifier: Send + Sync {
    /// Verifies `scope_token` presented by a peer with pid `peer_pid`,
    /// returning the [`ScopedRun`] the credential is bound to on success.
    ///
    /// # Errors
    /// Returns [`VerifyError`] if the credential is missing, invalid, not
    /// live, or the peer is outside the supervised ancestry.
    fn verify(&self, scope_token: &str, peer_pid: Option<i32>) -> Result<ScopedRun, VerifyError>;
}

/// The foundation default verifier: rejects every worker-MCP initialization,
/// because no credential store exists yet.
pub struct RejectAllWorkerVerifier;

impl WorkerCredentialVerifier for RejectAllWorkerVerifier {
    fn verify(&self, _scope_token: &str, _peer_pid: Option<i32>) -> Result<ScopedRun, VerifyError> {
        Err(VerifyError::NoCredentialStore)
    }
}

/// Configuration for a [`Server`]. All fields have sensible foundation
/// defaults; tests override the injectable readers/verifiers and the frame
/// bounds.
pub struct ServerConfig {
    /// The runtime's maximum NDJSON frame size, in bytes. Also the bootstrap
    /// hard limit before initialization. Capped at [`DEFAULT_MAX_FRAME_BYTES`].
    pub runtime_max_frame_bytes: u32,
    /// The effective uid the runtime runs as; a connection whose peer uid
    /// differs is dropped before any JSON is parsed.
    pub euid: u32,
    /// Reads peer OS credentials for each accepted connection.
    pub credential_reader: Arc<dyn PeerCredentialReader>,
    /// Verifies worker-MCP reconnect credentials.
    pub worker_verifier: Arc<dyn WorkerCredentialVerifier>,
    /// Test hook: forces the owner-only directory/socket permission check
    /// result instead of computing it at bind time. `None` computes it.
    pub owner_only_override: Option<bool>,
    /// Where the running binary was loaded from, reported by `runtime/status`.
    pub binary_source: batman_protocol::BinarySource,
    /// The injected adapter-start seam for `run/submit`. `None` means no
    /// adapter registry is wired up; `run/submit` then preserves the queued
    /// run and reports `adapter_unavailable` rather than pretending the run
    /// started.
    pub run_driver: Option<std::sync::Arc<dyn crate::service::RunDriver>>,
    /// The repository root this server serves.
    pub repository: std::path::PathBuf,
    /// The adapter-callback seam invoked after `approval/decide` records a
    /// decision. Defaults to [`crate::approval::NoopApprovalCallback`],
    /// which acknowledges immediately.
    pub approval_callback: std::sync::Arc<dyn crate::approval::ApprovalCallback>,
    /// How to handle a mid-run nested-worker policy violation
    /// (Hardening plan Task 1) -- `quarantine`, `cancel`, or
    /// `quarantineAndCancel` (the default). Applied by
    /// [`crate::policy::ViolationService::record`].
    pub nested_violation_action: crate::config::NestedViolationAction,
    /// The merged startup policy and the layers it came from. `Some` in the
    /// daemon (`crate::lifecycle::serve`); `None` in tests and embeddings,
    /// which then authorize every run under the authorizer's own startup
    /// policy and ignore per-run `policyOverrides`.
    pub policy: Option<(
        std::sync::Arc<crate::config::LayeredConfig>,
        std::sync::Arc<crate::config::RuntimePolicy>,
    )>,
    /// Test hook: inject a shared artifact store so the test can seed
    /// artifacts before exercising isolation gates. `None` creates a fresh
    /// store (production default).
    pub artifact_store: Option<std::sync::Arc<crate::workspace::ArtifactStore>>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            runtime_max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            euid: nix::unistd::Uid::effective().as_raw(),
            credential_reader: Arc::new(SystemPeerCredentialReader),
            worker_verifier: Arc::new(RejectAllWorkerVerifier),
            run_driver: None,
            repository: std::path::PathBuf::new(),
            owner_only_override: None,
            binary_source: batman_protocol::BinarySource::Unknown,
            approval_callback: Arc::new(crate::approval::NoopApprovalCallback),
            nested_violation_action: crate::config::NestedViolationAction::default(),
            policy: None,
            artifact_store: None,
        }
    }
}

/// The authenticated identity of a connected client, derived solely from its
/// [`batman_protocol::ClientAuth`] and the same-user socket boundary -- never
/// from client-supplied method or tool names.
#[derive(Debug, Clone)]
pub struct ClientPrincipal {
    /// The role the client authenticated as.
    pub role: ClientRole,
    /// The client's self-declared instance id, for diagnostics/routing.
    pub instance_id: String,
    /// The supervised run this principal is bound to, for `workerMcp`.
    pub scoped_run_id: Option<RunId>,
    /// The task bound alongside `scoped_run_id`, for `workerMcp`.
    pub scoped_task_id: Option<TaskId>,
    /// The worker bound alongside `scoped_run_id`, for `workerMcp` -- the
    /// only sender identity `coordination/send` trusts for this
    /// connection, regardless of what a request's `senderWorkerId`
    /// parameter claims.
    pub scoped_worker_id: Option<WorkerId>,
}

impl ClientPrincipal {
    /// The methods this principal is allowed to call, at foundation scope.
    /// Least-privilege within the same-user boundary; later protocol tasks
    /// extend these tables explicitly.
    #[must_use]
    pub fn allowed_methods(&self) -> Vec<BatmanMethod> {
        use BatmanMethod::{
            ApprovalDecide, ApprovalList, ArtifactFetch, ArtifactList, CoordinationArtifactFetch,
            CoordinationArtifactList, CoordinationAskPolicy, CoordinationChildDecide,
            CoordinationChildList, CoordinationPeerWorkspace, CoordinationPeers,
            CoordinationPublishArtifact, CoordinationReportBlocked, CoordinationRequestChild,
            CoordinationSend, CoordinationTask, EventsReplay, EventsSubscribe, MessageList,
            MessageSend, PolicyViolationDecide, PolicyViolationList, ProfileRegister, ReconcileOmp,
            RunCancel, RunGet, RunList, RunResult, RunRetry, RunSubmit, RuntimeShutdown,
            RuntimeStatus, TaskGet, TaskUpsert, WorkerCreate, WorkerGet, WorkerList,
            WorkspaceAcquire, WorkspaceApply, WorkspaceGet, WorkspaceInspect, WorkspaceRelease,
        };
        match self.role {
            ClientRole::OmpExtension => vec![
                RuntimeStatus,
                EventsSubscribe,
                EventsReplay,
                RuntimeShutdown,
                TaskUpsert,
                TaskGet,
                WorkerCreate,
                WorkerList,
                WorkerGet,
                RunSubmit,
                RunList,
                RunGet,
                RunResult,
                RunRetry,
                RunCancel,
                MessageSend,
                MessageList,
                ApprovalList,
                ApprovalDecide,
                CoordinationChildList,
                CoordinationChildDecide,
                ReconcileOmp,
                ProfileRegister,
                PolicyViolationDecide,
                PolicyViolationList,
                WorkspaceAcquire,
                WorkspaceGet,
                WorkspaceRelease,
                WorkspaceInspect,
                WorkspaceApply,
                ArtifactList,
                ArtifactFetch,
            ],
            ClientRole::Display => {
                vec![
                    RuntimeStatus,
                    EventsSubscribe,
                    EventsReplay,
                    TaskGet,
                    WorkerList,
                    WorkerGet,
                    RunList,
                    RunGet,
                    RunResult,
                    MessageList,
                    ApprovalList,
                    CoordinationChildList,
                    PolicyViolationList,
                ]
            }
            ClientRole::WorkerMcp => vec![
                RuntimeStatus,
                CoordinationTask,
                CoordinationPeers,
                CoordinationSend,
                CoordinationRequestChild,
                CoordinationPublishArtifact,
                CoordinationReportBlocked,
                CoordinationAskPolicy,
                CoordinationChildList,
                CoordinationPeerWorkspace,
                CoordinationArtifactList,
                CoordinationArtifactFetch,
            ],
        }
    }
}

/// Errors binding or serving the runtime socket.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// The Unix domain socket could not be bound at `path`.
    #[error("failed to bind runtime socket at {path:?}: {source}")]
    Bind {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The socket path exceeds the platform `sun_path` limit, so binding it
    /// would silently truncate the path. Callers must root state under a
    /// shorter directory.
    #[error("runtime socket path {path:?} is {len} bytes, exceeding the platform limit of {limit}")]
    SocketPathTooLong {
        path: std::path::PathBuf,
        len: usize,
        limit: usize,
    },
    /// A filesystem operation on the socket (removing a stale socket,
    /// tightening permissions) failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Securing the state directory failed.
    #[error(transparent)]
    Security(#[from] crate::security::SecurityError),
    /// A path could not be resolved.
    #[error(transparent)]
    Path(#[from] crate::paths::PathError),
    /// The durable database could not be opened or written.
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
}
