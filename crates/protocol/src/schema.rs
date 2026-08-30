//! The canonical JSON Schema document for the Crew wire protocol.
//!
//! [`ProtocolDocument`] exists solely to give `schemars` a single root that
//! transitively references every exported request, result, and event type,
//! so one invocation produces a schema with everything reachable from the
//! wire protocol in `$defs`.
//!
//! [`render_schema`] is the sole renderer. `crew-xtask generate` writes
//! its output to `packages/protocol-ts/schema/crew.schema.json`, and
//! `crewd doctor`'s `schema_compatibility` check compares the committed
//! file against it -- both must derive the schema the same way or the check
//! would report drift that does not exist.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ApplyResult, ArtifactFetchResult, ArtifactListResult, DisplayBackend, DisplayConfig,
    DisplayStatus, EventEnvelope, InitializeParams, InitializeResult, InspectResult,
    JsonRpcErrorResponse, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, MessageListResult,
    PaneReopenResult, PlanDecideResult, PlanGetResult, PlanProposeResult,
    PolicyViolationListResult, RetentionCleanResult, RunResultResult, RunTimeoutAckResult,
    RuntimeEvent, RuntimeStatus, WorkspaceInfo,
};

/// Root schema document referencing every exported request/result/event
/// type, so that a single `schemars` invocation produces one JSON Schema
/// with everything reachable from the wire protocol in `$defs`.
//
// CREW-44: this struct and `crates/xtask/src/main.rs`'s TS export
// allowlist (`export_bindings`'s `export!` call) are two independent
// lists that must agree on every *wire-message* type (not on bare
// id/enum/param types -- see that file's `NOT_WIRE_MESSAGE_ROOTS`, which
// is where those belong instead). CREW-43 found `RunMessage` and
// `MessageListResult` on the TS side with no field here at all, and
// nothing caught it until a human noticed. Adding a message type to the
// TS export list without a matching field here (or vice versa) now fails
// `generate --check` via `check_export_list_is_schema_reachable` in that
// same xtask file -- if you add a wire-message type to one list, add it
// to the other too.
//
// Deliberately `//`, not `///`: a doc comment here is generated schema
// content (schemars emits it as this root's `description`), not private
// engineering commentary -- CREW-44 is a code-organization note for future
// editors of this file, and belongs out of band from what every schema
// consumer reads as the file's own description.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolDocument {
    initialize_params: InitializeParams,
    initialize_result: InitializeResult,
    event_envelope: EventEnvelope,
    runtime_event: RuntimeEvent,
    display_backend: DisplayBackend,
    display_config: DisplayConfig,
    display_status: DisplayStatus,
    json_rpc_request: JsonRpcRequest<serde_json::Value>,
    json_rpc_response: JsonRpcResponse<serde_json::Value>,
    json_rpc_error_response: JsonRpcErrorResponse,
    json_rpc_notification: JsonRpcNotification<serde_json::Value>,
    runtime_status: RuntimeStatus,
    artifact_list_result: ArtifactListResult,
    artifact_fetch_result: ArtifactFetchResult,
    inspect_result: InspectResult,
    apply_result: ApplyResult,
    workspace_info: WorkspaceInfo,
    policy_violation_list_result: PolicyViolationListResult,
    /// `run/result` result payload.
    run_result_result: RunResultResult,
    /// `plan/propose` result payload.
    plan_propose_result: PlanProposeResult,
    /// `plan/decide` result payload.
    plan_decide_result: PlanDecideResult,
    /// `plan/get` result payload.
    plan_get_result: PlanGetResult,
    /// `run/timeoutAck` result payload.
    run_timeout_ack_result: RunTimeoutAckResult,
    /// `retention/clean` result payload.
    retention_clean_result: RetentionCleanResult,
    /// `pane/reopen` result payload.
    pane_reopen_result: PaneReopenResult,
    /// `message/list` result payload.
    message_list_result: MessageListResult,
}

/// Renders the [`ProtocolDocument`] schema as pretty JSON with a trailing
/// newline -- byte-for-byte what the committed schema file must contain.
///
/// # Errors
/// Returns the `serde_json` error if the schema fails to serialize, which
/// can only happen if a `JsonSchema` derive produces a non-serializable
/// value.
pub fn render_schema() -> Result<Vec<u8>, serde_json::Error> {
    let schema = schemars::schema_for!(ProtocolDocument);
    let mut text = serde_json::to_string_pretty(&schema)?;
    text.push('\n');
    Ok(text.into_bytes())
}
