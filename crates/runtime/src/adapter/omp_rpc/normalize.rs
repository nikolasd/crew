//! Raw OMP-RPC wire frames -> normalized [`AdapterEventPayload`]s.
//!
//! [`AdapterEventPayload`] (frozen by Task 1, not editable here) has no
//! single variant for "prompt accepted" vs "turn completed" -- the two
//! lifecycle points the plan's Task 6 acceptance criteria require this
//! adapter to keep genuinely distinguishable. This module represents both
//! as `role: "system"` message events, always emitted in that order for a
//! given prompt, rather than inventing a new payload variant:
//! - prompt acceptance -> `MessageChunk{role:"system",
//!   text: PROMPT_ACCEPTED_MARKER}` (the vendor's `response` frame for the
//!   `prompt` command proves the command was received/processed)
//! - turn completion -> `MessageFinal{role:"system",
//!   text: PROMPT_COMPLETED_MARKER}`, emitted either immediately after
//!   acceptance (when `data.agentInvoked == false`, a local-only prompt
//!   that never invoked a subagent) or later, from a distinct `agent_end`
//!   frame (when `data.agentInvoked == true`).
//!
//! `MessageChunk` vs `MessageFinal` is a real, structural distinction in
//! the frozen enum, so a consumer can tell the two apart without string
//! matching -- the marker text exists only for this adapter's own tests
//! and diagnostics, never as the sole distinguishing signal.

use crew_protocol::{ArtifactId, Classified, ContentClass};
use serde_json::Value;

use crew_runtime::adapter::AdapterEventPayload;

/// The visible marker text for a prompt-acceptance event.
pub const PROMPT_ACCEPTED_MARKER: &str = "omp-rpc:prompt-accepted";
/// The visible marker text for a turn-completion event.
pub const PROMPT_COMPLETED_MARKER: &str = "omp-rpc:prompt-completed";

fn visible(text: impl Into<String>) -> Classified<String> {
    Classified {
        class: ContentClass::Visible,
        value: text.into(),
    }
}

fn prompt_accepted() -> AdapterEventPayload {
    AdapterEventPayload::MessageChunk {
        role: "system".to_string(),
        text: visible(PROMPT_ACCEPTED_MARKER),
    }
}

fn prompt_completed() -> AdapterEventPayload {
    AdapterEventPayload::MessageFinal {
        role: "system".to_string(),
        text: visible(PROMPT_COMPLETED_MARKER),
    }
}

/// A vendor UI request this adapter treats as an approval. Only the two
/// decision-shaped `extension_ui_request` methods qualify --
/// `select`/`confirm`; `input`, `editor`, `cancel`, `notify`,
/// `setStatus`, `setWidget`, `setTitle`, `set_editor_text`, and
/// `open_url` are display or free-text surfaces, never approvals (see
/// `omp://rpc.md`'s complete `extension_ui_request` method list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// The `extension_ui_request` `id` an `extension_ui_response` must
    /// echo back on stdin to resolve this request.
    pub request_id: String,
    /// `"confirm"` or `"select"`.
    pub method: &'static str,
    /// The request's `title` field, or an empty string when absent.
    pub title: String,
}

/// Returns `Some` only for `method == "confirm" | "select"` on an
/// `extension_ui_request` frame carrying a string `id`. Every other
/// `extension_ui_request` (`setWidget`, `notify`, ...) -- and every
/// non-`extension_ui_request` frame -- returns `None`: this function is
/// never the sole gate on approval detection reaching a non-approval
/// frame, since [`normalize_frame`] independently drops
/// `extension_ui_request` to zero events regardless of what this
/// function reports.
#[must_use]
pub fn extension_ui_request_to_pending_approval(frame: &Value) -> Option<PendingApproval> {
    if frame.get("type").and_then(Value::as_str) != Some("extension_ui_request") {
        return None;
    }
    let request_id = frame.get("id").and_then(Value::as_str)?.to_string();
    let method = match frame.get("method").and_then(Value::as_str)? {
        "confirm" => "confirm",
        "select" => "select",
        _ => return None,
    };
    let title = frame
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(PendingApproval {
        request_id,
        method,
        title,
    })
}

/// Normalizes one already-parsed OMP-RPC frame into zero or more
/// [`AdapterEventPayload`]s, in emission order.
///
/// Frame types this adapter has no normalized representation for yet
/// (`extension_ui_request`, `available_commands_update`, the `ready`
/// handshake itself, ...) yield no events -- an unrecognized frame type is
/// recovery, not an error. `thinking_*` frames are dropped here, before
/// ever constructing an [`AdapterEventPayload`], per the shared adapter
/// contract's redaction-boundary discipline (never rely solely on the
/// sink's own defensive drop).
#[must_use]
pub fn normalize_frame(frame: &Value) -> Vec<AdapterEventPayload> {
    let Some(frame_type) = frame.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    match frame_type {
        "response" => normalize_response(frame),
        "agent_end" => vec![prompt_completed()],
        "message_start" | "message_update" => frame
            .get("text")
            .and_then(Value::as_str)
            .map(|text| AdapterEventPayload::MessageChunk {
                role: role_of(frame),
                text: visible(text),
            })
            .into_iter()
            .collect(),
        "message_end" => frame
            .get("text")
            .and_then(Value::as_str)
            .map(|text| AdapterEventPayload::MessageFinal {
                role: role_of(frame),
                text: visible(text),
            })
            .into_iter()
            .collect(),
        "thinking_start" | "thinking_delta" | "thinking_end" => Vec::new(),
        "toolcall_start" | "tool_execution_start" => tool_started(frame).into_iter().collect(),
        "toolcall_delta" => tool_progress(frame).into_iter().collect(),
        // A completed tool call yields its result, and additionally an
        // artifact when the call actually mutated a file.
        "toolcall_end" | "tool_execution_end" => tool_result(frame)
            .into_iter()
            .chain(artifact_produced(frame))
            .collect(),
        "subagent_started" | "subagent_lifecycle" | "subagent_event" => {
            subagent_observed(frame).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

fn role_of(frame: &Value) -> String {
    frame
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant")
        .to_string()
}

fn normalize_response(frame: &Value) -> Vec<AdapterEventPayload> {
    let command = frame.get("command").and_then(Value::as_str).unwrap_or("");
    let success = frame
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let data = frame.get("data").cloned().unwrap_or(Value::Null);

    if !success {
        let detail = frame
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("OMP-RPC command failed")
            .to_string();
        return vec![AdapterEventPayload::ProtocolHealthChanged {
            healthy: false,
            detail: visible(detail),
        }];
    }

    match command {
        "prompt" => {
            let mut events = vec![prompt_accepted()];
            let agent_invoked = data
                .get("agentInvoked")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !agent_invoked {
                events.push(prompt_completed());
            }
            events
        }
        "get_state" => data
            .get("sessionId")
            .and_then(Value::as_str)
            .map(|id| AdapterEventPayload::VendorSessionEstablished {
                vendor_session_id: id.to_string(),
            })
            .into_iter()
            .collect(),
        "get_session_stats" => {
            let tokens = data.get("tokens");
            let input_tokens = tokens.and_then(|t| t.get("input")).and_then(Value::as_u64);
            let output_tokens = tokens.and_then(|t| t.get("output")).and_then(Value::as_u64);
            match (input_tokens, output_tokens) {
                (Some(input_tokens), Some(output_tokens)) => {
                    vec![AdapterEventPayload::UsageReported {
                        input_tokens,
                        output_tokens,
                        cost_usd: data.get("cost").and_then(Value::as_f64),
                    }]
                }
                _ => Vec::new(),
            }
        }
        "get_subagents" => data
            .get("subagents")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(subagent_observed).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The tool name, across both observed frame shapes.
///
/// `omp 17.2.7`'s real `tool_execution_*` frames carry `toolName`, while
/// the `toolcall_*` frames carry `name`. Both are routed here, so both
/// spellings are read rather than silently falling back to `"tool"` --
/// which is what a real `edit` call used to normalize to.
fn tool_name_of(frame: &Value) -> String {
    frame
        .get("toolName")
        .or_else(|| frame.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string()
}

fn tool_started(frame: &Value) -> Option<AdapterEventPayload> {
    let tool_call_id = frame.get("toolCallId").and_then(Value::as_str)?.to_string();
    Some(AdapterEventPayload::ToolStarted {
        tool_call_id,
        name: tool_name_of(frame),
    })
}

fn tool_progress(frame: &Value) -> Option<AdapterEventPayload> {
    let tool_call_id = frame.get("toolCallId").and_then(Value::as_str)?.to_string();
    let detail = frame
        .get("delta")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(AdapterEventPayload::ToolProgress {
        tool_call_id,
        name: tool_name_of(frame),
        detail: visible(detail),
    })
}

/// Whether a completed tool call failed.
///
/// `tool_execution_end` reports `isError: true`; `toolcall_end` reports
/// `ok: false`. Absent both, a completed call is treated as successful --
/// but `isError` must be honored, or a rejected `edit` would be reported
/// as a success (observed against `omp 17.2.7`, whose failed edits carry
/// `isError: true` and no `ok` field at all).
fn tool_ok_of(frame: &Value) -> bool {
    if let Some(is_error) = frame.get("isError").and_then(Value::as_bool) {
        return !is_error;
    }
    frame.get("ok").and_then(Value::as_bool).unwrap_or(true)
}

/// The human-readable outcome, across both shapes. `toolcall_end` carries
/// `result` as a plain string; `tool_execution_end` carries it as an
/// object whose `content` is an MCP-style text-block array.
fn tool_detail_of(frame: &Value) -> String {
    let Some(result) = frame.get("result") else {
        return String::new();
    };
    if let Some(text) = result.as_str() {
        return text.to_string();
    }
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn tool_result(frame: &Value) -> Option<AdapterEventPayload> {
    let tool_call_id = frame.get("toolCallId").and_then(Value::as_str)?.to_string();
    Some(AdapterEventPayload::ToolResult {
        tool_call_id,
        name: tool_name_of(frame),
        ok: tool_ok_of(frame),
        detail: visible(tool_detail_of(frame)),
    })
}

/// An artifact for a tool call that actually mutated a file.
///
/// The discriminator was captured from a real `omp --mode rpc 17.2.7`
/// session (a local `lm-studio` model told to rewrite a file): a
/// successful mutation reports
///
/// ```text
/// {"type":"tool_execution_end","toolName":"edit","isError":false,
///  "result":{"details":{"op":"update","path":"greeting.txt",
///                       "diff":"-1|hello\n+1|goodbye", ...}}}
/// ```
///
/// while a *rejected* edit carries `isError: true` and `details: {}`, and
/// a `read` carries `details` with no `op`/`path` at all. So the presence
/// of both `op` and `path` under `result.details`, on a non-error frame,
/// is what distinguishes a mutation from every other tool call --
/// deliberately narrow, so a read or a failed edit never fabricates an
/// artifact.
///
/// `artifact_kind` is the literal `"fileChange"` rather than the frame's
/// `op` (`"update"`, ...): `op` describes the operation, not the kind of
/// artifact, and Codex emits `"fileChange"` for the same event
/// (`codex/normalize.rs`), so the two adapters stay comparable.
fn artifact_produced(frame: &Value) -> Option<AdapterEventPayload> {
    if !tool_ok_of(frame) {
        return None;
    }
    let details = frame.get("result")?.get("details")?;
    details.get("op").and_then(Value::as_str)?;
    details.get("path").and_then(Value::as_str)?;
    Some(AdapterEventPayload::ArtifactProduced {
        artifact_id: ArtifactId::new(),
        artifact_kind: "fileChange".to_string(),
    })
}

/// A vendor-reported subagent, in whatever shape it was observed (either
/// its own top-level event frame, or one entry of a `get_subagents`
/// response's `data.subagents` array). Emitted even though this adapter
/// always declares `nested: none` -- emission never upgrades a declared
/// capability (see `event_sink.rs`'s own doc comment).
fn subagent_observed(frame: &Value) -> Option<AdapterEventPayload> {
    let vendor_child_id = frame
        .get("id")
        .or_else(|| frame.get("subagentId"))
        .and_then(Value::as_str)?
        .to_string();
    let vendor_parent_ref = frame
        .get("parentId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(AdapterEventPayload::NestedWorkerObserved {
        vendor_child_id,
        vendor_parent_ref,
    })
}
