//! The Model Context Protocol (MCP) surface `coordination-mcp` speaks over
//! stdio: tool schemas, and the pure translation between one MCP
//! `tools/call` and the [`super::CoordinationBroker`] JSON-RPC method it
//! maps to.
//!
//! Every tool here corresponds to exactly one worker-safe
//! `coordination/*` method already scoped and authorized by the
//! connection's authenticated `workerMcp` principal (see
//! `crate::ipc::connection::dispatch_coordination`) -- this module never
//! authorizes anything itself; it only shapes the wire messages on
//! either side of that boundary. In particular, no tool's input schema
//! ever exposes `runId`/`senderWorkerId`/`taskId`: the proxy fills those
//! in from its own bound scope, exactly as the socket dispatch layer
//! trusts only that bound scope, never a caller-supplied identity.

use batman_protocol::COORDINATION_PAYLOAD_MAX_BYTES;
use serde_json::{Value, json};

/// The conservative *character*-count upper bound this module's JSON
/// Schemas declare for a free-text field (`payload`, `reason`,
/// `question`, `description`). JSON Schema's `maxLength` counts Unicode
/// characters, not UTF-8 bytes, and a non-ASCII string can need up to 4
/// bytes per character -- so this is deliberately equal to, not merely
/// derived from, [`COORDINATION_PAYLOAD_MAX_BYTES`]: any string within
/// this many *characters* might still exceed the byte bound, which is
/// why [`translate_tool_call`] enforces the real byte length itself
/// (see [`reject_if_over_byte_budget`]) rather than trusting this schema
/// hint alone.
const FREE_TEXT_MAX_CHARS: usize = COORDINATION_PAYLOAD_MAX_BYTES;

/// The upper bound this module's JSON Schemas declare for an
/// id/reference-shaped field (`recipientWorkerId`, `replyTo`,
/// `artifactRef`) -- generous for a URI-shaped artifact reference, but
/// far below [`FREE_TEXT_MAX_CHARS`] since these are never free text.
const ID_MAX_CHARS: usize = 4096;

/// One MCP tool this server advertises: a name, a human-readable
/// description, its arguments' JSON Schema, and its result's JSON
/// Schema (MCP's optional `outputSchema`, mirrored into
/// `structuredContent` alongside the always-present text `content`
/// block -- see [`tool_result_from_success`]).
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub output_schema: Value,
}

/// Why a `tools/call` request could not be translated into a
/// `coordination/*` method call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolCallError {
    #[error("unknown tool {0:?}")]
    UnknownTool(String),
    #[error("{0}")]
    InvalidArguments(String),
}

/// Every tool this server advertises, in the fixed order this module's
/// tests and `tools/list` responses use.
#[must_use]
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "crew_task",
            description: "Read the worker-safe view of the task this run belongs to.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "taskId": { "type": "string" },
                    "ownerClientInstanceId": { "type": "string" },
                    "revision": { "type": "integer" },
                },
                "required": ["taskId", "ownerClientInstanceId", "revision"],
            }),
        },
        ToolSpec {
            name: "crew_peers",
            description: "List sibling workers on the same task as this run, including each peer's run id for use with crew_peer_workspace.",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "peers": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "workerId": { "type": "string" },
                                "adapter": { "type": "string" },
                                "runId": { "type": "string" },
                            },
                            "required": ["workerId", "adapter", "runId"],
                        },
                    },
                },
                "required": ["peers"],
            }),
        },
        ToolSpec {
            name: "crew_peer_workspace",
            description: "Discover the workspace path of a peer agent on the same task, by the peer's run id (from crew_peers). Fails if the peer has no active workspace lease.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "peerRunId": { "type": "string", "maxLength": ID_MAX_CHARS },
                },
                "required": ["peerRunId"],
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "mode": { "type": "string" },
                    "isolationKind": { "type": "string" },
                    "state": { "type": "string" },
                },
                "required": ["path", "mode", "isolationKind", "state"],
            }),
        },
        ToolSpec {
            name: "crew_artifact_list",
            description: "List artifacts published by any agent on this task. Never exposes artifacts from other tasks.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["patch", "commitList", "conflictReport", "workspaceManifest"],
                    },
                },
                "required": [],
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "artifacts": { "type": "array" },
                },
                "required": ["artifacts"],
            }),
        },
        ToolSpec {
            name: "crew_artifact_fetch",
            description: "Read one bounded chunk of an artifact on this task. Follow nextOffset until complete is true; the chunk size is fixed by the runtime.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "artifactId": { "type": "string", "maxLength": ID_MAX_CHARS },
                    "offset": { "type": "integer", "minimum": 0 },
                },
                "required": ["artifactId"],
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "artifact": { "type": "object" },
                    "contentBase64": { "type": "string" },
                    "nextOffset": { "type": ["integer", "null"] },
                    "complete": { "type": "boolean" },
                },
                "required": ["artifact", "contentBase64", "complete"],
            }),
        },
        ToolSpec {
            name: "crew_send",
            description: "Send a correlated, journaled message to a peer worker or to OMP.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": [
                            "assign", "steer", "followUp", "question", "answer",
                            "peerMessage", "approvalDecision", "cancel", "shutdown",
                        ],
                    },
                    "payload": { "type": "string", "maxLength": FREE_TEXT_MAX_CHARS },
                    "recipientWorkerId": { "type": "string", "maxLength": ID_MAX_CHARS },
                    "replyTo": { "type": "string", "maxLength": ID_MAX_CHARS },
                },
                "required": ["kind", "payload"],
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "messageId": { "type": "string" },
                    "deliveryState": { "type": "string" },
                    "recordedSequence": { "type": "integer" },
                    "sentSequence": { "type": "integer" },
                },
                "required": ["messageId", "deliveryState"],
            }),
        },
        ToolSpec {
            name: "crew_request_child",
            description: "Ask OMP to authorize a child worker. Never creates a task or worker itself.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "maxLength": FREE_TEXT_MAX_CHARS },
                },
                "required": ["reason"],
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": { "sequence": { "type": "integer" } },
                "required": ["sequence"],
            }),
        },
        ToolSpec {
            name: "crew_publish_artifact",
            description: "Record a reference to an artifact produced by this run.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "artifactRef": { "type": "string", "maxLength": ID_MAX_CHARS },
                    "description": { "type": "string", "maxLength": FREE_TEXT_MAX_CHARS },
                },
                "required": ["artifactRef"],
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "sequence": { "type": "integer" },
                    "artifactRef": { "type": "string" },
                },
                "required": ["sequence", "artifactRef"],
            }),
        },
        ToolSpec {
            name: "crew_report_blocked",
            description: "Report this run is blocked (e.g. on a peer answer) without changing ownership.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "maxLength": FREE_TEXT_MAX_CHARS },
                },
                "required": ["reason"],
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "messageId": { "type": "string" },
                    "deliveryState": { "type": "string" },
                },
                "required": ["messageId", "deliveryState"],
            }),
        },
        ToolSpec {
            name: "crew_ask_policy",
            description: "Ask OMP a policy question (e.g. \"may I write to this path?\") without deciding it locally.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "maxLength": FREE_TEXT_MAX_CHARS },
                },
                "required": ["question"],
                "additionalProperties": false,
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "messageId": { "type": "string" },
                    "deliveryState": { "type": "string" },
                },
                "required": ["messageId", "deliveryState"],
            }),
        },
    ]
}

/// Looks up one tool's output schema by name. Only called with a name
/// [`translate_tool_call`] already accepted, so the tool is always found.
fn output_schema_for(name: &str) -> Value {
    tool_specs()
        .into_iter()
        .find(|spec| spec.name == name)
        .map(|spec| spec.output_schema)
        .unwrap_or_else(|| json!({ "type": "object" }))
}

/// A minimal structural check against a JSON Schema fragment: object
/// `type`/`required`/`properties` (recursing into `array`'s `items`),
/// and a coarse `type` match per declared property. Not a general
/// JSON Schema validator -- this project's own schemas are generated
/// and AJV-checked on the TypeScript side (`bun run check`); this is
/// the equivalent guard for the hand-written schemas in this module,
/// catching a broker result shape that silently drifted from what this
/// module advertises before it ever reaches a caller as
/// `structuredContent`.
fn matches_schema_shape(value: &Value, schema: &Value) -> Result<(), String> {
    let declared_type = schema.get("type").and_then(Value::as_str);
    match declared_type {
        Some("object") => {
            let object = value
                .as_object()
                .ok_or_else(|| format!("expected an object, got {value}"))?;
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for field in required {
                    let field = field.as_str().unwrap_or_default();
                    if !object.contains_key(field) {
                        return Err(format!("missing required field {field:?} in {value}"));
                    }
                }
            }
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (field, field_schema) in properties {
                    if let Some(field_value) = object.get(field) {
                        matches_schema_shape(field_value, field_schema)
                            .map_err(|err| format!("field {field:?}: {err}"))?;
                    }
                }
            }
            Ok(())
        }
        Some("array") => {
            let items = value
                .as_array()
                .ok_or_else(|| format!("expected an array, got {value}"))?;
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    matches_schema_shape(item, item_schema)?;
                }
            }
            Ok(())
        }
        Some("string") if !value.is_string() => Err(format!("expected a string, got {value}")),
        Some("integer") if !value.is_i64() && !value.is_u64() => {
            Err(format!("expected an integer, got {value}"))
        }
        Some("boolean") if !value.is_boolean() => Err(format!("expected a boolean, got {value}")),
        _ => Ok(()),
    }
}

/// The run/task/worker identity a live `coordination-mcp` connection is
/// bound to -- exactly [`crate::ipc::ScopedRun`], but named for this
/// module's own use so it never needs to depend on `crate::ipc` types
/// directly in its public signature.
#[derive(Debug, Clone, Copy)]
pub struct BoundScope {
    pub run_id: batman_protocol::RunId,
    pub task_id: batman_protocol::TaskId,
    pub worker_id: batman_protocol::WorkerId,
}

/// Translates one MCP `tools/call` (`name` plus its `arguments` object)
/// into the `(method, params)` pair to send over the authenticated
/// `workerMcp` socket connection, filling in `scope`'s bound identity
/// wherever the underlying `coordination/*` method needs it -- a tool
/// argument can never supply or override `runId`, `senderWorkerId`, or
/// `taskId`.
///
/// # Errors
/// Returns [`ToolCallError::UnknownTool`] for a name outside
/// [`tool_specs`], or [`ToolCallError::InvalidArguments`] if a required
/// argument is missing or the wrong type.
pub fn translate_tool_call(
    name: &str,
    arguments: &Value,
    scope: BoundScope,
) -> Result<(&'static str, Value), ToolCallError> {
    reject_unknown_properties(name, arguments)?;
    let run_id = scope.run_id.to_string();
    match name {
        "crew_task" => Ok(("coordination/task", json!({ "runId": run_id }))),
        "crew_peers" => Ok(("coordination/peers", json!({ "runId": run_id }))),
        "crew_send" => {
            let kind = required_str(arguments, "kind")?;
            let payload = required_bounded_str(arguments, "payload", FREE_TEXT_MAX_CHARS)?;
            let mut params = json!({
                "runId": run_id,
                "senderWorkerId": scope.worker_id.to_string(),
                "taskId": scope.task_id.to_string(),
                "kind": kind,
                "payload": payload,
            });
            copy_optional_bounded_str(arguments, "recipientWorkerId", ID_MAX_CHARS, &mut params)?;
            copy_optional_bounded_str(arguments, "replyTo", ID_MAX_CHARS, &mut params)?;
            Ok(("coordination/send", params))
        }
        "crew_request_child" => {
            let reason = required_bounded_str(arguments, "reason", FREE_TEXT_MAX_CHARS)?;
            Ok((
                "coordination/requestChild",
                json!({ "runId": run_id, "reason": reason }),
            ))
        }
        "crew_publish_artifact" => {
            let artifact_ref = required_bounded_str(arguments, "artifactRef", ID_MAX_CHARS)?;
            let mut params = json!({ "runId": run_id, "artifactRef": artifact_ref });
            copy_optional_bounded_str(arguments, "description", FREE_TEXT_MAX_CHARS, &mut params)?;
            Ok(("coordination/publishArtifact", params))
        }
        "crew_report_blocked" => {
            let reason = required_bounded_str(arguments, "reason", FREE_TEXT_MAX_CHARS)?;
            Ok((
                "coordination/reportBlocked",
                json!({ "runId": run_id, "reason": reason }),
            ))
        }
        "crew_ask_policy" => {
            let question = required_bounded_str(arguments, "question", FREE_TEXT_MAX_CHARS)?;
            Ok((
                "coordination/askPolicy",
                json!({ "runId": run_id, "question": question }),
            ))
        }
        "crew_peer_workspace" => {
            let peer_run_id = required_bounded_str(arguments, "peerRunId", ID_MAX_CHARS)?;
            Ok((
                "coordination/peerWorkspace",
                json!({ "runId": run_id, "peerRunId": peer_run_id }),
            ))
        }
        "crew_artifact_list" => {
            let kind = arguments.get("kind").and_then(Value::as_str);
            Ok((
                "coordination/artifactList",
                match kind {
                    Some(kind) => json!({ "runId": run_id, "kind": kind }),
                    None => json!({ "runId": run_id }),
                },
            ))
        }
        "crew_artifact_fetch" => {
            let artifact_id = required_bounded_str(arguments, "artifactId", ID_MAX_CHARS)?;
            let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0);
            Ok((
                "coordination/artifactFetch",
                json!({ "runId": run_id, "artifactId": artifact_id, "offset": offset }),
            ))
        }
        other => Err(ToolCallError::UnknownTool(other.to_string())),
    }
}

/// Rejects `arguments` outright if it carries any key `name`'s declared
/// input schema doesn't list -- enforcing every schema's
/// `additionalProperties: false` in code, not just in the advertised
/// document. In particular this is what actually stops a smuggled
/// `runId`/`senderWorkerId`/`taskId`/etc.: silently ignoring an unknown
/// field (rather than rejecting the whole call) would let a caller
/// believe such a field took effect when it never does.
fn reject_unknown_properties(name: &str, arguments: &Value) -> Result<(), ToolCallError> {
    let Some(object) = arguments.as_object() else {
        return Err(ToolCallError::InvalidArguments(
            "arguments must be an object".to_string(),
        ));
    };
    let Some(spec) = tool_specs().into_iter().find(|spec| spec.name == name) else {
        // An unknown tool name is `ToolCallError::UnknownTool`, decided
        // by the caller (`translate_tool_call`'s own match); nothing to
        // reject here.
        return Ok(());
    };
    let allowed = spec
        .input_schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .keys()
                .cloned()
                .collect::<std::collections::HashSet<_>>()
        })
        .unwrap_or_default();
    for key in object.keys() {
        if !allowed.contains(key) {
            return Err(ToolCallError::InvalidArguments(format!(
                "{name} does not accept an argument named {key:?}"
            )));
        }
    }
    Ok(())
}

fn required_str<'a>(arguments: &'a Value, field: &str) -> Result<&'a str, ToolCallError> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolCallError::InvalidArguments(format!("{field} is required")))
}

/// Like [`required_str`], but rejects a value whose UTF-8 byte length
/// exceeds `max_bytes` -- the actual enforced bound. `maxLength` in this
/// module's JSON Schemas is a *character*-count hint for well-behaved
/// clients; this is what a hostile or buggy one cannot bypass.
fn required_bounded_str<'a>(
    arguments: &'a Value,
    field: &str,
    max_bytes: usize,
) -> Result<&'a str, ToolCallError> {
    let value = required_str(arguments, field)?;
    reject_if_over_byte_budget(field, value, max_bytes)?;
    Ok(value)
}

fn copy_optional_bounded_str(
    arguments: &Value,
    field: &str,
    max_bytes: usize,
    into: &mut Value,
) -> Result<(), ToolCallError> {
    if let Some(value) = arguments.get(field).and_then(Value::as_str) {
        reject_if_over_byte_budget(field, value, max_bytes)?;
        into[field] = Value::String(value.to_string());
    }
    Ok(())
}

fn reject_if_over_byte_budget(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ToolCallError> {
    let len = value.len();
    if len > max_bytes {
        return Err(ToolCallError::InvalidArguments(format!(
            "{field} of {len} bytes exceeds the {max_bytes}-byte maximum"
        )));
    }
    Ok(())
}

/// Wraps a `coordination/*` JSON-RPC result into the MCP `tools/call`
/// result shape: a text content block carrying the result as compact
/// JSON (for a client that only reads `content`) plus `structuredContent`
/// holding the same value typed (for one that reads it directly),
/// `isError: false`.
///
/// # Errors
/// Returns [`ToolCallError::InvalidArguments`] if `result` does not
/// match `name`'s declared [`ToolSpec::output_schema`] -- a broker
/// result shape drifted from what this module advertises must never
/// reach a caller silently mislabeled as conforming.
pub fn tool_result_from_success(name: &str, result: &Value) -> Result<Value, ToolCallError> {
    matches_schema_shape(result, &output_schema_for(name))
        .map_err(|err| ToolCallError::InvalidArguments(format!("{name} result: {err}")))?;
    Ok(json!({
        "content": [{ "type": "text", "text": result.to_string() }],
        "structuredContent": result,
        "isError": false,
    }))
}

/// Wraps a `coordination/*` JSON-RPC error (or a local translation
/// failure) into the MCP `tools/call` result shape: a single text
/// content block carrying the message, `isError: true`. MCP reports
/// tool failures as a normal result with `isError: true`, not a
/// protocol-level JSON-RPC error -- only a malformed request (unknown
/// method, bad params) uses a JSON-RPC error at the MCP layer itself.
#[must_use]
pub fn tool_result_from_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use batman_protocol::{RunId, TaskId, WorkerId};

    fn scope() -> BoundScope {
        BoundScope {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
        }
    }

    #[test]
    fn every_tool_spec_has_a_crew_prefixed_name_and_object_schema() {
        let specs = tool_specs();
        assert_eq!(specs.len(), 10);
        for spec in &specs {
            assert!(spec.name.starts_with("crew_"), "{}", spec.name);
            assert_eq!(spec.input_schema["type"], "object");
            assert!(!spec.description.is_empty());
        }
    }

    #[test]
    fn no_tool_input_schema_accepts_run_task_or_worker_identity() {
        for spec in tool_specs() {
            let properties = spec.input_schema["properties"]
                .as_object()
                .expect("every tool schema declares its properties object");
            for forbidden in ["runId", "senderWorkerId", "taskId"] {
                assert!(
                    !properties.contains_key(forbidden),
                    "{} must never accept {forbidden} as a tool argument",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn crew_task_and_peers_take_no_arguments_and_bind_the_scoped_run() {
        let scope = scope();
        let (method, params) = translate_tool_call("crew_task", &json!({}), scope).unwrap();
        assert_eq!(method, "coordination/task");
        assert_eq!(params["runId"], scope.run_id.to_string());
        assert_eq!(params.as_object().unwrap().len(), 1);

        let (method, params) = translate_tool_call("crew_peers", &json!({}), scope).unwrap();
        assert_eq!(method, "coordination/peers");
        assert_eq!(params["runId"], scope.run_id.to_string());
    }

    #[test]
    fn crew_send_binds_sender_and_task_from_the_bound_scope() {
        let scope = scope();
        let (method, params) = translate_tool_call(
            "crew_send",
            &json!({ "kind": "question", "payload": "hi" }),
            scope,
        )
        .unwrap();
        assert_eq!(method, "coordination/send");
        assert_eq!(params["runId"], scope.run_id.to_string());
        assert_eq!(params["senderWorkerId"], scope.worker_id.to_string());
        assert_eq!(params["taskId"], scope.task_id.to_string());
        assert_eq!(params["kind"], "question");
        assert_eq!(params["payload"], "hi");
        assert!(params.get("recipientWorkerId").is_none());
        assert!(params.get("replyTo").is_none());
    }

    #[test]
    fn crew_send_rejects_a_smuggled_sender_worker_id_argument_outright() {
        let scope = scope();
        let spoofed_sender = WorkerId::new();
        let arguments = json!({
            "kind": "question",
            "payload": "hi",
            // additionalProperties: false means this whole call must be
            // rejected -- silently ignoring the extra field would let a
            // caller believe it took effect when it never does.
            "senderWorkerId": spoofed_sender.to_string(),
        });
        let err = translate_tool_call("crew_send", &arguments, scope).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidArguments(_)));
    }

    #[test]
    fn every_tool_rejects_a_runid_or_task_id_argument_outright() {
        for (name, mut arguments) in [
            ("crew_task", json!({})),
            ("crew_peers", json!({})),
            ("crew_send", json!({ "kind": "question", "payload": "hi" })),
            ("crew_request_child", json!({ "reason": "x" })),
            ("crew_publish_artifact", json!({ "artifactRef": "x" })),
            ("crew_report_blocked", json!({ "reason": "x" })),
            ("crew_ask_policy", json!({ "question": "x" })),
        ] {
            arguments["runId"] = json!(RunId::new().to_string());
            let err = translate_tool_call(name, &arguments, scope()).unwrap_err();
            assert!(matches!(err, ToolCallError::InvalidArguments(_)), "{name}");
        }
    }

    #[test]
    fn crew_send_carries_optional_recipient_and_reply_to_when_present() {
        let scope = scope();
        let recipient = WorkerId::new();
        let arguments = json!({
            "kind": "peerMessage",
            "payload": "hello",
            "recipientWorkerId": recipient.to_string(),
            "replyTo": "some-message-id",
        });
        let (_, params) = translate_tool_call("crew_send", &arguments, scope).unwrap();
        assert_eq!(params["recipientWorkerId"], recipient.to_string());
        assert_eq!(params["replyTo"], "some-message-id");
    }

    #[test]
    fn crew_send_missing_payload_is_invalid_arguments_not_a_panic() {
        let scope = scope();
        let err =
            translate_tool_call("crew_send", &json!({ "kind": "question" }), scope).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidArguments(_)));
    }

    #[test]
    fn crew_send_rejects_a_payload_over_the_byte_budget_even_with_multi_byte_characters() {
        let scope = scope();
        // A 4-byte-per-character string well under `FREE_TEXT_MAX_CHARS`
        // (a character count) can still overflow the real byte budget --
        // this is exactly the gap a `maxLength`-only check would miss.
        let over_budget = "\u{1f600}".repeat(FREE_TEXT_MAX_CHARS / 4 + 1);
        assert!(over_budget.chars().count() <= FREE_TEXT_MAX_CHARS);
        assert!(over_budget.len() > FREE_TEXT_MAX_CHARS);
        let arguments = json!({ "kind": "question", "payload": over_budget });
        let err = translate_tool_call("crew_send", &arguments, scope).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidArguments(_)));

        let at_budget = "a".repeat(FREE_TEXT_MAX_CHARS);
        let arguments = json!({ "kind": "question", "payload": at_budget });
        assert!(translate_tool_call("crew_send", &arguments, scope).is_ok());
    }

    #[test]
    fn crew_publish_artifact_rejects_an_oversized_artifact_ref() {
        let scope = scope();
        let too_long = "a".repeat(ID_MAX_CHARS + 1);
        let arguments = json!({ "artifactRef": too_long });
        let err = translate_tool_call("crew_publish_artifact", &arguments, scope).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidArguments(_)));
    }

    #[test]
    fn crew_request_child_report_blocked_and_ask_policy_map_their_one_field() {
        let scope = scope();

        let (method, params) = translate_tool_call(
            "crew_request_child",
            &json!({ "reason": "need help" }),
            scope,
        )
        .unwrap();
        assert_eq!(method, "coordination/requestChild");
        assert_eq!(params["reason"], "need help");
        assert_eq!(params["runId"], scope.run_id.to_string());

        let (method, params) = translate_tool_call(
            "crew_report_blocked",
            &json!({ "reason": "waiting on peer" }),
            scope,
        )
        .unwrap();
        assert_eq!(method, "coordination/reportBlocked");
        assert_eq!(params["reason"], "waiting on peer");

        let (method, params) = translate_tool_call(
            "crew_ask_policy",
            &json!({ "question": "may I write here?" }),
            scope,
        )
        .unwrap();
        assert_eq!(method, "coordination/askPolicy");
        assert_eq!(params["question"], "may I write here?");
    }

    #[test]
    fn crew_publish_artifact_carries_optional_description() {
        let scope = scope();
        let (method, params) = translate_tool_call(
            "crew_publish_artifact",
            &json!({ "artifactRef": "artifact://abc" }),
            scope,
        )
        .unwrap();
        assert_eq!(method, "coordination/publishArtifact");
        assert_eq!(params["artifactRef"], "artifact://abc");
        assert!(params.get("description").is_none());

        let (_, params) = translate_tool_call(
            "crew_publish_artifact",
            &json!({ "artifactRef": "artifact://abc", "description": "the diff" }),
            scope,
        )
        .unwrap();
        assert_eq!(params["description"], "the diff");
    }

    #[test]
    fn unknown_tool_name_is_rejected() {
        let err = translate_tool_call("not_a_real_tool", &json!({}), scope()).unwrap_err();
        assert_eq!(
            err,
            ToolCallError::UnknownTool("not_a_real_tool".to_string())
        );
    }

    #[test]
    fn success_result_carries_content_and_structured_content_when_it_matches_its_schema() {
        let result = json!({ "taskId": "t-1", "ownerClientInstanceId": "omp-1", "revision": 1 });
        let success = tool_result_from_success("crew_task", &result).unwrap();
        assert_eq!(success["isError"], false);
        assert_eq!(success["content"][0]["type"], "text");
        assert!(
            success["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("t-1")
        );
        assert_eq!(success["structuredContent"], result);

        let failure = tool_result_from_error("boom");
        assert_eq!(failure["isError"], true);
        assert_eq!(failure["content"][0]["text"], "boom");
    }

    #[test]
    fn success_result_is_rejected_when_the_broker_result_drifts_from_the_advertised_schema() {
        // Missing every required field of crew_task's output schema.
        let drifted = json!({ "unexpected": true });
        let err = tool_result_from_success("crew_task", &drifted).unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidArguments(_)));
    }

    #[test]
    fn every_tool_output_schema_is_an_object_schema() {
        for spec in tool_specs() {
            assert_eq!(spec.output_schema["type"], "object", "{}", spec.name);
            assert!(spec.output_schema["required"].is_array(), "{}", spec.name);
        }
    }
}
