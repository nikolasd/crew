//! Raw Claude `stream-json` frames -> [`AdapterEventPayload`]s.
//!
//! [`ClaudeNormalizer`] is stateful across the lines of one session only
//! for the cross-line correlation the wire format genuinely requires: a
//! `tool_use_id -> name` map (a `tool_result` block never repeats the
//! tool's name) and the set of `parent_tool_use_id`s already reported via
//! [`AdapterEventPayload::NestedWorkerObserved`] (so a still-active
//! subagent's later frames don't re-report it).
//!
//! **Thinking blocks are discarded here, before any [`AdapterEvent`] is
//! ever constructed** -- stronger than [`AdapterEventSink`]'s own
//! defensive `None`-on-drop backstop for non-`Visible` content: a
//! `thinking` content block simply never produces a [`ClaudeEvent::Emit`]
//! at all, so no `Classified<String>` carrying its text is ever built in
//! the first place. See `crates/runtime/tests/claude_adapter.rs`'s
//! `thinking_only_message_produces_no_events_at_all`.
//!
//! Vendor `PermissionRequest` hook lifecycle (Claude's only
//! `--include-hook-events`-surfaced approval signal) is normalized into
//! [`ClaudeEvent::ApprovalRequested`]/[`ClaudeEvent::ApprovalResolved`],
//! never into an [`AdapterEvent`]: wiring these into
//! `crate::approval::ApprovalService` end-to-end is out of this
//! milestone's scope (see the Worker Adapters plan's shared context) --
//! the adapter surfaces them only to its own internal
//! `pending_approvals` state, which `snapshot()` reports and
//! `respond_to_approval()` remains unable to resolve (`approvals:
//! observable`, not `controllable`).

use std::collections::{HashMap, HashSet};

use crew_protocol::{Classified, ContentClass};
use crew_runtime::adapter::{AdapterError, AdapterEventPayload};
use serde_json::Value;

use super::protocol::{
    RawChatMessage, RawContentBlock, RawFrame, RawHookLifecycle, RawResult, RawStreamEvent,
};

/// One decoded effect of a single Claude `stream-json` line.
#[derive(Debug, Clone)]
pub enum ClaudeEvent {
    /// Ready to be wrapped into an `AdapterEvent` (with the run's
    /// `run_id`/`task_id`/`worker_id`) and pushed through the sink.
    Emit(AdapterEventPayload),
    /// A `PermissionRequest` hook started -- see the module doc.
    ApprovalRequested {
        approval_id: String,
        hook_name: String,
    },
    /// The matching `PermissionRequest` hook resolved. `decision` is
    /// `"allow"`/`"deny"` when the hook's own JSON output parses that
    /// far (`hookSpecificOutput.decision.behavior`); otherwise the raw
    /// hook outcome (`"success"`/`"error"`/`"cancelled"`).
    ApprovalResolved {
        approval_id: String,
        decision: String,
    },
}

/// Stateful line-by-line normalizer for one Claude `stream-json` session.
#[derive(Debug, Default)]
pub struct ClaudeNormalizer {
    tool_names: HashMap<String, String>,
    reported_subagents: HashSet<String>,
}

impl ClaudeNormalizer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses and normalizes one raw `stream-json` line.
    ///
    /// # Errors
    /// Returns [`AdapterError::protocol`] for a line that fails to parse
    /// at all (see [`RawFrame::parse`]); a structurally valid but
    /// unrecognized `type`/`subtype` is never an error.
    pub fn normalize_line(
        &mut self,
        adapter: &str,
        line: &[u8],
    ) -> Result<Vec<ClaudeEvent>, AdapterError> {
        let frame = RawFrame::parse(line)
            .map_err(|err| AdapterError::protocol(adapter, "normalize", err.to_string()))?;
        Ok(self.normalize(frame))
    }

    fn normalize(&mut self, frame: RawFrame) -> Vec<ClaudeEvent> {
        match frame {
            RawFrame::SystemInit(init) => vec![ClaudeEvent::Emit(
                AdapterEventPayload::VendorSessionEstablished {
                    vendor_session_id: init.session_id,
                },
            )],
            RawFrame::HookStarted(hook) if hook.hook_event == "PermissionRequest" => {
                vec![ClaudeEvent::ApprovalRequested {
                    approval_id: hook.hook_id,
                    hook_name: hook.hook_name,
                }]
            }
            RawFrame::HookResponse(hook) if hook.hook_event == "PermissionRequest" => {
                vec![ClaudeEvent::ApprovalResolved {
                    approval_id: hook.hook_id.clone(),
                    decision: parse_hook_decision(&hook),
                }]
            }
            RawFrame::HookStarted(_) | RawFrame::HookResponse(_) | RawFrame::HookProgress(_) => {
                // A non-`PermissionRequest` hook, or an intermediate
                // `hook_progress` frame -- neither carries an
                // approval-relevant signal this adapter normalizes.
                Vec::new()
            }
            RawFrame::StreamEvent(event) => self.normalize_stream_event(event),
            RawFrame::Assistant(message) => self.normalize_chat_message("assistant", message),
            RawFrame::User(message) => self.normalize_chat_message("user", message),
            RawFrame::Result(result) => normalize_result(result),
            RawFrame::Unrecognized => Vec::new(),
        }
    }

    fn normalize_stream_event(&mut self, event: RawStreamEvent) -> Vec<ClaudeEvent> {
        let Some(delta) = event.event.delta else {
            return Vec::new();
        };
        if delta.kind != "text_delta" {
            return Vec::new();
        }
        let Some(text) = delta.text else {
            return Vec::new();
        };
        vec![ClaudeEvent::Emit(AdapterEventPayload::MessageChunk {
            role: "assistant".to_string(),
            text: Classified {
                class: ContentClass::Visible,
                value: text,
            },
        })]
    }

    fn normalize_chat_message(
        &mut self,
        base_role: &str,
        message: RawChatMessage,
    ) -> Vec<ClaudeEvent> {
        let mut events = Vec::new();

        if let Some(parent_id) = message.parent_tool_use_id.as_deref()
            && self.reported_subagents.insert(parent_id.to_string())
        {
            events.push(ClaudeEvent::Emit(
                AdapterEventPayload::NestedWorkerObserved {
                    vendor_child_id: parent_id.to_string(),
                    vendor_parent_ref: message.session_id.clone(),
                },
            ));
        }

        let role = match message.parent_tool_use_id.as_deref() {
            Some(parent_id) => format!("{base_role}:subagent:{parent_id}"),
            None => base_role.to_string(),
        };

        for block in message.message.content {
            match block {
                RawContentBlock::Text { text } => {
                    events.push(ClaudeEvent::Emit(AdapterEventPayload::MessageFinal {
                        role: role.clone(),
                        text: Classified {
                            class: ContentClass::Visible,
                            value: text,
                        },
                    }));
                }
                // Discarded before ever reaching the sink -- see the
                // module doc.
                RawContentBlock::Thinking { .. } => {}
                RawContentBlock::ToolUse { id, name, .. } => {
                    self.tool_names.insert(id.clone(), name.clone());
                    events.push(ClaudeEvent::Emit(AdapterEventPayload::ToolStarted {
                        tool_call_id: id,
                        name,
                    }));
                }
                RawContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let name = self
                        .tool_names
                        .get(&tool_use_id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    events.push(ClaudeEvent::Emit(AdapterEventPayload::ToolResult {
                        tool_call_id: tool_use_id,
                        name,
                        ok: !is_error,
                        detail: Classified {
                            class: ContentClass::Visible,
                            value: tool_result_text(&content),
                        },
                    }));
                }
                RawContentBlock::Unrecognized => {}
            }
        }

        events
    }
}

/// A `tool_result` block's `content` is either a plain string or an
/// array of Anthropic content blocks (almost always `{"type":"text",
/// "text": "..."}`); this joins every text sub-block into one detail
/// string.
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Best-effort decision extraction from a `hook_response` frame's own
/// `output` (the hook script's raw `HookJSONOutput` JSON text). For a
/// `PermissionRequest` hook this is `{"hookSpecificOutput":
/// {"hookEventName":"PermissionRequest","decision":{"behavior":"allow"|
/// "deny", ...}}}`; when `output` doesn't parse that far (a hook that
/// didn't return a decision object, or simply didn't run to completion),
/// this falls back to the frame's own `outcome`
/// (`"success"`/`"error"`/`"cancelled"`) rather than guessing.
fn parse_hook_decision(hook: &RawHookLifecycle) -> String {
    if let Some(output) = &hook.output
        && let Ok(parsed) = serde_json::from_str::<Value>(output)
        && let Some(behavior) = parsed
            .get("hookSpecificOutput")
            .and_then(|h| h.get("decision"))
            .and_then(|d| d.get("behavior"))
            .and_then(Value::as_str)
    {
        return behavior.to_string();
    }
    hook.outcome
        .clone()
        .unwrap_or_else(|| "unknown".to_string())
}

fn normalize_result(result: RawResult) -> Vec<ClaudeEvent> {
    let mut events = vec![ClaudeEvent::Emit(AdapterEventPayload::UsageReported {
        input_tokens: result.usage.input_tokens,
        output_tokens: result.usage.output_tokens,
        cost_usd: Some(result.total_cost_usd),
    })];
    // A vendor-reported failure (`is_error: true`, subtype
    // `error_max_turns`/`error_during_execution`/...) must surface as an
    // explicit unhealthy-protocol event naming the subtype, not be
    // silently reduced to a usage report -- the same shape the Copilot
    // adapter emits for a failed stop reason (R12).
    if result.is_error == Some(true) {
        let subtype = result.subtype.as_deref().unwrap_or("unreported");
        events.push(ClaudeEvent::Emit(
            AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: Classified {
                    class: ContentClass::Visible,
                    value: format!("claude result reported an error: {subtype}"),
                },
            },
        ));
    }
    if let Some(text) = result.result {
        events.push(ClaudeEvent::Emit(AdapterEventPayload::MessageFinal {
            role: "result".to_string(),
            text: Classified {
                class: ContentClass::Visible,
                value: text,
            },
        }));
    }
    events
}
