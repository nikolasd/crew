//! The canonical JSON Schema document for the BATMAN wire protocol.
//!
//! [`ProtocolDocument`] exists solely to give `schemars` a single root that
//! transitively references every exported request, result, and event type,
//! so one invocation produces a schema with everything reachable from the
//! wire protocol in `$defs`.
//!
//! [`render_schema`] is the sole renderer. `batman-xtask generate` writes
//! its output to `packages/protocol-ts/schema/batman.schema.json`, and
//! `crewd doctor`'s `schema_compatibility` check compares the committed
//! file against it -- both must derive the schema the same way or the check
//! would report drift that does not exist.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ApplyResult, ArtifactFetchResult, ArtifactListResult, DisplayBackend, DisplayConfig,
    DisplayStatus, EventEnvelope, InitializeParams, InitializeResult, InspectResult,
    JsonRpcErrorResponse, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    PolicyViolationListResult, RunResultResult, RuntimeEvent, RuntimeStatus, WorkspaceInfo,
};

/// Root schema document referencing every exported request/result/event
/// type, so that a single `schemars` invocation produces one JSON Schema
/// with everything reachable from the wire protocol in `$defs`.
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
