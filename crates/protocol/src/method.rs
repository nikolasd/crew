//! Extended JSON-RPC methods for the orchestration extension.
//!
//! Foundation scope implements `initialize`, `runtime/status`,
//! `events/subscribe`, `events/replay`, and `runtime/shutdown`.
//! The orchestration extension adds task, worker, run, message, approval,
//! coordination, and reconciliation methods.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// All JSON-RPC methods implemented by the Crew runtime, including
/// orchestration extension methods.
///
/// Serialized as the literal method name string used on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub enum CrewMethod {
    // Foundation methods
    #[serde(rename = "initialize")]
    Initialize,
    #[serde(rename = "runtime/status")]
    RuntimeStatus,
    #[serde(rename = "events/subscribe")]
    EventsSubscribe,
    #[serde(rename = "events/replay")]
    EventsReplay,
    // R82. The out-of-band `crewd stop`/SIGTERM path is deliberately
    // unarbitrated.
    /// Gracefully stops the daemon. Refused with `-32602` while any run is
    /// live or another connection is being served, unless
    /// `params.force == true` (the deliberate, logged operator escape
    /// hatch).
    #[serde(rename = "runtime/shutdown")]
    RuntimeShutdown,

    // Orchestration: task
    #[serde(rename = "task/upsert")]
    TaskUpsert,
    #[serde(rename = "task/get")]
    TaskGet,

    // Orchestration: worker
    #[serde(rename = "worker/create")]
    WorkerCreate,
    #[serde(rename = "worker/list")]
    WorkerList,
    #[serde(rename = "worker/get")]
    WorkerGet,

    // Orchestration: run
    #[serde(rename = "run/submit")]
    RunSubmit,
    #[serde(rename = "run/list")]
    RunList,
    #[serde(rename = "run/get")]
    RunGet,
    #[serde(rename = "run/retry")]
    RunRetry,
    #[serde(rename = "run/cancel")]
    RunCancel,
    // ADR-0027.
    /// The leader closes a run it considers done. A TUI vendor never exits,
    /// so a run is a conversation the leader ends -- this is that ending,
    /// distinct from `run/cancel`'s abort.
    #[serde(rename = "run/finish")]
    RunFinish,
    #[serde(rename = "run/result")]
    RunResult,

    // Orchestration: message
    #[serde(rename = "message/send")]
    MessageSend,
    #[serde(rename = "message/list")]
    MessageList,

    // Orchestration: approval
    #[serde(rename = "approval/list")]
    ApprovalList,
    #[serde(rename = "approval/decide")]
    ApprovalDecide,

    // Orchestration: coordination (child lifecycle)
    #[serde(rename = "coordination/child/list")]
    CoordinationChildList,
    #[serde(rename = "coordination/child/decide")]
    CoordinationChildDecide,

    // Orchestration: coordination (worker-safe broker surface)
    #[serde(rename = "coordination/task")]
    CoordinationTask,
    #[serde(rename = "coordination/peers")]
    CoordinationPeers,
    #[serde(rename = "coordination/send")]
    CoordinationSend,
    #[serde(rename = "coordination/requestChild")]
    CoordinationRequestChild,
    #[serde(rename = "coordination/publishArtifact")]
    CoordinationPublishArtifact,
    #[serde(rename = "coordination/reportBlocked")]
    CoordinationReportBlocked,
    #[serde(rename = "coordination/askPolicy")]
    CoordinationAskPolicy,
    #[serde(rename = "coordination/peerWorkspace")]
    CoordinationPeerWorkspace,
    #[serde(rename = "coordination/artifactList")]
    CoordinationArtifactList,
    #[serde(rename = "coordination/artifactFetch")]
    CoordinationArtifactFetch,

    // Orchestration: reconcile OMP-native agents
    #[serde(rename = "reconcile/omp")]
    ReconcileOmp,

    // Orchestration: adapter worker profiles (Worker Adapters milestone)
    #[serde(rename = "profile/register")]
    ProfileRegister,

    // Workspaces: lease and artifact operations
    #[serde(rename = "workspace/acquire")]
    WorkspaceAcquire,
    #[serde(rename = "workspace/get")]
    WorkspaceGet,
    #[serde(rename = "workspace/release")]
    WorkspaceRelease,
    #[serde(rename = "workspace/inspect")]
    WorkspaceInspect,
    #[serde(rename = "workspace/apply")]
    WorkspaceApply,
    #[serde(rename = "artifact/list")]
    ArtifactList,
    #[serde(rename = "artifact/fetch")]
    ArtifactFetch,

    // Policy: violation resolution
    #[serde(rename = "policy/violation/decide")]
    PolicyViolationDecide,
    // R80.
    /// Lists a project's recorded policy violations with their decision
    /// state, so an operator can find which violation still holds a
    /// quarantine without diffing the raw event stream.
    #[serde(rename = "policy/violation/list")]
    PolicyViolationList,

    // Orchestration: leader/subtask plan lifecycle (crew v2). Role-gated
    // to `ompExtension` only (`crate::ipc::ClientPrincipal::allowed_methods`
    // in the runtime crate). The daemon accepts these methods and refuses
    // them with a "not yet implemented" JSON-RPC error until a later work
    // package (WP17/WP21) lands their real handlers.
    /// Proposes a decomposition of a run into subtasks, persisting a
    /// `PlanProposed` event pending `plan/decide`.
    #[serde(rename = "plan/propose")]
    PlanPropose,
    /// Approves or rejects a previously proposed plan, persisting a
    /// `PlanDecided` event.
    #[serde(rename = "plan/decide")]
    PlanDecide,
    /// Fetches the most recently proposed plan for a run and its decision,
    /// if any.
    #[serde(rename = "plan/get")]
    PlanGet,
    /// The leader acknowledges a `WorkerTimeout` event, resolving how the
    /// run proceeds.
    #[serde(rename = "run/timeoutAck")]
    RunTimeoutAck,

    // Operator/maintenance surface (`/crew clean`, `/crew reopen`).
    /// Runs the retention prune once, on demand: removes the events of
    /// terminal (or unassociated) runs past the configured age cutoff and
    /// beyond the `retention.maxRuns` recency cap. Never touches active
    /// runs or run rows.
    #[serde(rename = "retention/clean")]
    RetentionClean,
    /// Re-creates a live run's display pane around its still-running
    /// attach socket (the pane was closed by the user or a backend).
    #[serde(rename = "pane/reopen")]
    PaneReopen,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_and_timeout_ack_methods_serialize_as_literal_method_names() {
        let cases = [
            (CrewMethod::PlanPropose, "plan/propose"),
            (CrewMethod::PlanDecide, "plan/decide"),
            (CrewMethod::PlanGet, "plan/get"),
            (CrewMethod::RunTimeoutAck, "run/timeoutAck"),
            (CrewMethod::RetentionClean, "retention/clean"),
            (CrewMethod::PaneReopen, "pane/reopen"),
        ];
        for (method, wire_name) in cases {
            assert_eq!(serde_json::to_value(method).unwrap(), wire_name);
            let parsed: CrewMethod = serde_json::from_value(serde_json::json!(wire_name)).unwrap();
            assert_eq!(parsed, method);
        }
    }
}
