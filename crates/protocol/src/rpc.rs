//! JSON-RPC 2.0 envelopes and the `initialize` handshake types.
//!
//! BATMAN speaks JSON-RPC 2.0 over NDJSON. This module owns the generic
//! request/response/error envelopes plus the concrete payloads exchanged
//! during the `initialize` handshake.

use crate::BatmanMethod;
use crate::ids::{ProjectId, RunId, TaskId, WorkerId};
use crate::version::{ProtocolVersion, VersionRange};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Identifies a repository on disk, independent of any particular runtime
/// instance.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RepositoryIdentity {
    pub canonical_path: String,
    pub vcs_root: String,
}

/// Identifies the connecting client implementation (name + version), for
/// diagnostics only.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// The role a connecting client authenticates as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ClientRole {
    OmpExtension,
    WorkerMcp,
    Display,
}

/// Authentication payload presented by a connecting client. The `role` tag
/// determines which shape the remaining fields take.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(
    tag = "role",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[ts(export)]
pub enum ClientAuth {
    OmpExtension {
        instance_id: String,
        agent_directory: String,
    },
    WorkerMcp {
        instance_id: String,
        scope_token: String,
    },
    Display {
        instance_id: String,
    },
}

/// Capabilities a connecting client declares support for.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ClientCapabilities {
    pub event_replay: bool,
    pub max_frame_bytes: u32,
}

/// Parameters for the `initialize` request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct InitializeParams {
    pub client: ClientInfo,
    pub supported: VersionRange,
    pub repository: RepositoryIdentity,
    pub auth: ClientAuth,
    pub capabilities: ClientCapabilities,
    #[ts(type = "number | null")]
    pub last_sequence: Option<u64>,
}

/// Identifies the runtime implementation (name + version), for diagnostics
/// only.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RuntimeInfo {
    pub name: String,
    pub version: String,
}

/// Capabilities the runtime grants for the negotiated session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RuntimeCapabilities {
    /// The negotiated maximum NDJSON frame size, in bytes.
    pub max_frame_bytes: u32,
    /// Whether the runtime was able to verify the peer's OS credentials for
    /// this connection. `false` on platforms where peer credential lookup is
    /// unavailable.
    pub peer_credentials_verified: bool,
}

/// A summary of the authenticated client, echoed back so the client can
/// confirm how the runtime identified it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ClientPrincipalSummary {
    pub role: ClientRole,
    pub instance_id: String,
    /// The run this connection is scoped to. `None` for every role except
    /// `workerMcp`, whose scope-token binding determines it -- never a
    /// value the client can request or override.
    pub scoped_run_id: Option<RunId>,
    /// The task this connection is scoped to, alongside `scopedRunId`.
    pub scoped_task_id: Option<TaskId>,
    /// The worker this connection is scoped to, alongside `scopedRunId`.
    /// A `workerMcp` client uses this (never a self-declared value) as
    /// the authoritative sender identity for `coordination/send`.
    pub scoped_worker_id: Option<WorkerId>,
}

// BatmanMethod is re-exported from method.rs.

/// Where the running `crewd` binary was loaded from. `override` means a
/// developer override path, `package` a bundled/installed binary, and
/// `unknown` that the source could not be determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum BinarySource {
    Override,
    Package,
    Unknown,
}

/// Result of a `runtime/status` request: a snapshot of the runtime's health
/// and identity. Kept intentionally small at foundation scope; later tasks
/// extend it with richer run/queue detail.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RuntimeStatus {
    /// Whether the runtime is accepting connections and serving requests.
    pub running: bool,
    /// The protocol version the runtime negotiated for this session.
    pub protocol: ProtocolVersion,
    /// The canonical project id this runtime serves.
    pub project_id: ProjectId,
    /// Number of runs the runtime's adapter registry is actively driving.
    pub active_runs: u32,
    /// The durable database schema version currently applied.
    pub schema_version: u32,
    /// Whether the negotiated protocol is within the runtime's supported
    /// range (a self-check that always holds for a live, negotiated session).
    pub protocol_healthy: bool,
    /// Seconds the runtime has been up since it started serving.
    #[ts(type = "number")]
    pub uptime_seconds: u64,
    /// Where the running binary was loaded from.
    pub binary_source: BinarySource,
}

/// Result of a successful `initialize` request.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct InitializeResult {
    pub runtime: RuntimeInfo,
    pub negotiated: ProtocolVersion,
    pub project_id: ProjectId,
    pub principal: ClientPrincipalSummary,
    pub allowed_methods: Vec<BatmanMethod>,
    pub capabilities: RuntimeCapabilities,
    #[ts(type = "number")]
    pub next_sequence: u64,
}

/// JSON-RPC 2.0 error codes reserved by the specification.
pub mod error_code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    /// BATMAN application-defined error codes, in the reserved
    /// `-32000..-32099` range.
    pub const NOT_INITIALIZED: i32 = -32001;
    pub const INCOMPATIBLE_VERSION: i32 = -32002;
    pub const CAPABILITY_UNSUPPORTED: i32 = -32003;
    pub const PROFILE_REQUIRED: i32 = -32007;
    pub const SEQUENCE_GONE: i32 = -32004;
    pub const ADAPTER_UNAVAILABLE: i32 = -32005;
    pub const RATE_LIMITED: i32 = -32006;
    /// Reserved application error range per JSON-RPC 2.0: an invalid
    /// lifecycle-state transition was requested.
    pub const ILLEGAL_TRANSITION: i32 = -32100;
    /// The addressed run is quarantined pending `policy/violation/decide`
    /// -- messages, artifact publication, and workspace apply are blocked
    /// until the owning `ompExtension` client resolves the violation.
    pub const POLICY_QUARANTINED: i32 = -32101;
}

/// The fixed `jsonrpc` version string used on every envelope.
pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request identifier, either a number or a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(untagged)]
#[ts(export)]
pub enum RequestId {
    Number(#[ts(type = "number")] i64),
    String(String),
}

/// A JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct JsonRpcRequest<P> {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

impl<P> JsonRpcRequest<P> {
    #[must_use]
    pub fn new(id: RequestId, method: impl Into<String>, params: Option<P>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 success response envelope.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct JsonRpcResponse<R> {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: R,
}

impl<R> JsonRpcResponse<R> {
    #[must_use]
    pub fn new(id: RequestId, result: R) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        }
    }
}

/// A JSON-RPC 2.0 error object. `data` carries optional, already-sanitized
/// diagnostic detail; it is omitted from the wire form when absent.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "unknown")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 error response envelope. `id` is `None` when the request
/// identifier could not be determined (for example, on a parse error).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: Option<RequestId>,
    pub error: JsonRpcError,
}

impl JsonRpcErrorResponse {
    #[must_use]
    pub fn new(id: Option<RequestId>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            error: JsonRpcError {
                code,
                message: message.into(),
                data: None,
            },
        }
    }

    /// Like [`JsonRpcErrorResponse::new`], but attaches already-sanitized
    /// diagnostic `data` to the error object.
    #[must_use]
    pub fn with_data(
        id: Option<RequestId>,
        code: i32,
        message: impl Into<String>,
        data: serde_json::Value,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            error: JsonRpcError {
                code,
                message: message.into(),
                data: Some(data),
            },
        }
    }
}

/// A JSON-RPC 2.0 notification envelope: a method call with no `id`, for
/// which no response is expected. BATMAN uses these to push runtime events to
/// subscribed clients via the `events/event` method.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct JsonRpcNotification<P> {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<P>,
}

impl<P> JsonRpcNotification<P> {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<P>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

/// The method name carried by an `events/event` notification.
pub const EVENTS_EVENT_METHOD: &str = "events/event";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batman_method_serializes_as_literal_method_name() {
        assert_eq!(
            serde_json::to_value(BatmanMethod::RuntimeStatus).unwrap(),
            "runtime/status"
        );
        assert_eq!(
            serde_json::to_value(BatmanMethod::EventsSubscribe).unwrap(),
            "events/subscribe"
        );
        let parsed: BatmanMethod = serde_json::from_str("\"runtime/shutdown\"").unwrap();
        assert_eq!(parsed, BatmanMethod::RuntimeShutdown);
    }

    #[test]
    fn error_codes_match_spec() {
        assert_eq!(error_code::PARSE_ERROR, -32700);
        assert_eq!(error_code::INVALID_REQUEST, -32600);
        assert_eq!(error_code::METHOD_NOT_FOUND, -32601);
        assert_eq!(error_code::INVALID_PARAMS, -32602);
        assert_eq!(error_code::INTERNAL_ERROR, -32603);
        assert_eq!(error_code::NOT_INITIALIZED, -32001);
        assert_eq!(error_code::INCOMPATIBLE_VERSION, -32002);
        assert_eq!(error_code::CAPABILITY_UNSUPPORTED, -32003);
        assert_eq!(error_code::SEQUENCE_GONE, -32004);
    }

    #[test]
    fn request_envelope_is_camel_case_and_strict() {
        let request = JsonRpcRequest::new(
            RequestId::Number(1),
            "initialize",
            Some(serde_json::json!({ "foo": "bar" })),
        );
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], "initialize");

        let with_unknown =
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":null,"unknown":true}"#;
        assert!(serde_json::from_str::<JsonRpcRequest<serde_json::Value>>(with_unknown).is_err());
    }

    #[test]
    fn client_principal_summary_is_camel_case() {
        let summary = ClientPrincipalSummary {
            role: ClientRole::OmpExtension,
            instance_id: "omp-1".into(),
            scoped_run_id: None,
            scoped_task_id: None,
            scoped_worker_id: None,
        };
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["role"], "ompExtension");
        assert_eq!(value["instanceId"], "omp-1");
        assert!(value["scopedRunId"].is_null());
    }

    #[test]
    fn client_principal_summary_carries_the_worker_mcp_scope() {
        let run_id = RunId::new();
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let summary = ClientPrincipalSummary {
            role: ClientRole::WorkerMcp,
            instance_id: "worker-1".into(),
            scoped_run_id: Some(run_id),
            scoped_task_id: Some(task_id),
            scoped_worker_id: Some(worker_id),
        };
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["scopedRunId"], run_id.to_string());
        assert_eq!(value["scopedTaskId"], task_id.to_string());
        assert_eq!(value["scopedWorkerId"], worker_id.to_string());
    }

    #[test]
    fn error_response_allows_null_id() {
        let response = JsonRpcErrorResponse::new(None, error_code::PARSE_ERROR, "bad json");
        let value = serde_json::to_value(&response).unwrap();
        assert!(value["id"].is_null());
        assert_eq!(value["error"]["code"], -32700);
    }

    #[test]
    fn request_id_round_trips_number_and_string() {
        assert_eq!(serde_json::to_value(RequestId::Number(7)).unwrap(), 7);
        assert_eq!(
            serde_json::to_value(RequestId::String("abc".into())).unwrap(),
            "abc"
        );
        let number: RequestId = serde_json::from_str("7").unwrap();
        assert_eq!(number, RequestId::Number(7));
        let string: RequestId = serde_json::from_str("\"abc\"").unwrap();
        assert_eq!(string, RequestId::String("abc".into()));
    }
}
