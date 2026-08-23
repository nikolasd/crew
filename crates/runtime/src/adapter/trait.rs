//! The `Adapter` trait: the object-safe contract every worker adapter
//! (Claude, Codex, Copilot, OMP-RPC, and the `fake-worker`-backed fixture
//! adapter used by tests/conformance) implements.
//!
//! Mirrors the design spec's "Runtime adapter contract" exactly:
//! `probe/start/resume/send/respondToApproval/cancel/snapshot/dispose`,
//! plus the event sink adapters push normalized telemetry through
//! (`super::event_sink::AdapterEventSink`, passed into `start`/`resume`
//! rather than being a trait method itself). No method has a default
//! body: every adapter must decide explicitly what each operation means
//! for it, even if that decision is "return `capability_unsupported`" --
//! nothing here silently no-ops.

use std::sync::Arc;

use crew_protocol::{RunId, TaskId, WorkerId};

use super::AdapterFuture;
use super::capability::AdapterCapabilities;
use super::event_sink::AdapterEventSink;

/// The result of a no-model-call probe: observed version/auth-readiness
/// facts and the capabilities the adapter is prepared to declare. `probe`
/// must never invoke a model -- only version/help/auth-readiness commands.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// The installed vendor CLI/tool version, if determinable.
    pub version: Option<String>,
    /// Whether the adapter observed the vendor CLI is ready to run without
    /// an interactive login flow.
    pub auth_ready: bool,
    /// The capabilities this adapter is prepared to declare, pending
    /// conformance proof.
    pub capabilities: AdapterCapabilities,
    /// Whether an ambient native capability (skills, plugins, hooks, MCP
    /// servers discovered by the vendor CLI itself) could not be fully
    /// enumerated programmatically. Never `false` unless every capability
    /// claimed was actually observed.
    pub inventory_incomplete: bool,
}

/// A resumable vendor session/thread reference (Claude session id, Codex
/// thread id, Copilot ACP `sessionId`, or an OMP session file/id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorSessionRef(pub String);

/// The immutable request to start one run with one worker.
#[derive(Debug, Clone)]
pub struct StartSpec {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub worker_id: WorkerId,
    /// The initial assignment text (the task's goal/instructions).
    pub prompt: String,
    /// Set when this start is actually a resume of a prior vendor session.
    pub resume: Option<VendorSessionRef>,
}

/// A message delivered to an already-started (or resuming) adapter.
#[derive(Debug, Clone)]
pub enum AdapterMessage {
    Steer { text: String },
    FollowUp { text: String },
    Answer { text: String },
    PeerMessage { text: String },
}

/// The scope of a cancellation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelScope {
    /// Cancel only the active turn; the worker/session may continue.
    Turn,
    /// Cancel the entire worker (its process/session).
    Worker,
    /// Cancel the worker and every child it has spawned.
    Subtree,
}

/// A point-in-time snapshot of an adapter's state, children, usage, and
/// artifacts, as returned by [`Adapter::snapshot`].
#[derive(Debug, Clone, Default)]
pub struct AdapterSnapshot {
    pub state_summary: String,
    pub children: Vec<String>,
    pub usage: Option<serde_json::Value>,
    pub artifacts: Vec<serde_json::Value>,
}

/// The object-safe contract every worker adapter implements.
///
/// Adapters call `sink.emit(..)` (the [`AdapterEventSink`] passed into
/// [`Adapter::start`]/[`Adapter::resume`]) to push normalized events --
/// they never write `crate::domain::DomainRepository` directly.
pub trait Adapter: Send + Sync {
    /// The adapter kind, e.g. `"claude"`, `"codex"`, `"copilot"`,
    /// `"ompRpc"`, or `"fixture"`/`"fake"` for tests. Used verbatim in
    /// every [`super::error::AdapterError::adapter`].
    fn kind(&self) -> &str;

    /// The capabilities this adapter instance declares.
    fn capabilities(&self) -> AdapterCapabilities;

    /// A no-model-call probe of version, auth readiness, and observed
    /// capabilities.
    fn probe(&self) -> AdapterFuture<'_, ProbeResult>;

    /// Starts (or, if `spec.resume` is set, resumes) a supervised vendor
    /// process/session for `spec`, pushing every subsequent normalized
    /// event through `sink`.
    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()>;

    /// Restores a previously-established vendor session without a fresh
    /// [`StartSpec`] (e.g. after a runtime restart for a
    /// `vendor-resumable` adapter).
    fn resume(
        &self,
        session: VendorSessionRef,
        sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()>;

    /// Delivers a follow-up/steer/answer/peer-message to an already
    /// started adapter.
    fn send(&self, message: AdapterMessage) -> AdapterFuture<'_, ()>;

    /// Resolves a pending approval request the adapter itself reported.
    fn respond_to_approval(&self, approval_id: &str, decision: &str) -> AdapterFuture<'_, ()>;

    /// Requests cancellation at the given scope.
    fn cancel(&self, scope: CancelScope) -> AdapterFuture<'_, ()>;

    /// A point-in-time snapshot of state, children, usage, and artifacts.
    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot>;

    /// Releases every protocol/process resource this adapter instance
    /// holds. Idempotent.
    fn dispose(&self) -> AdapterFuture<'_, ()>;
}
