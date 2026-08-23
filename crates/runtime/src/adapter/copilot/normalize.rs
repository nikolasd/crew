//! Normalizes a raw ACP v1 `session/update` payload (the `update` field of
//! a `SessionNotification`) into zero or more [`AdapterEventPayload`]s.
//!
//! **`agent_thought_chunk` is dropped here, before anything ever reaches
//! an `AdapterEvent`** -- this is the adapter's *primary* Thinking-content
//! filter (the sink's own drop-to-`None` on a non-`Visible` `Classified`
//! value is only a defensive backstop, per `event_sink.rs`'s module doc).
//! An unrecognized `sessionUpdate` discriminator (e.g. `plan`,
//! `available_commands_update`, `current_mode_update` -- real ACP v1
//! variants this adapter does not yet map to a canonical `AdapterEvent`)
//! normalizes to no events rather than a guessed shape.

use crew_protocol::{Classified, ContentClass};
use serde_json::Value;

use crew_runtime::adapter::AdapterEventPayload;

/// Normalizes one ACP `session/update` `update` object into the
/// canonical `AdapterEvent` payloads it represents.
#[must_use]
pub fn copilot_normalize_session_update(update: &Value) -> Vec<AdapterEventPayload> {
    match update.get("sessionUpdate").and_then(Value::as_str) {
        Some("agent_thought_chunk") => Vec::new(),
        Some(kind @ ("user_message_chunk" | "agent_message_chunk")) => {
            let role = if kind == "user_message_chunk" {
                "user"
            } else {
                "assistant"
            };
            let Some(text) = update.get("content").and_then(content_block_text) else {
                return Vec::new();
            };
            vec![AdapterEventPayload::MessageChunk {
                role: role.to_string(),
                text: Classified {
                    class: ContentClass::Visible,
                    value: text,
                },
            }]
        }
        Some("tool_call") => {
            let Some(tool_call_id) = update.get("toolCallId").and_then(Value::as_str) else {
                return Vec::new();
            };
            let name = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(tool_call_id)
                .to_string();
            vec![AdapterEventPayload::ToolStarted {
                tool_call_id: tool_call_id.to_string(),
                name,
            }]
        }
        Some("tool_call_update") => {
            let Some(tool_call_id) = update.get("toolCallId").and_then(Value::as_str) else {
                return Vec::new();
            };
            let name = update
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(tool_call_id)
                .to_string();
            let detail = Classified {
                class: ContentClass::Visible,
                value: tool_call_content_text(update.get("content").and_then(Value::as_array)),
            };
            match update.get("status").and_then(Value::as_str) {
                Some("completed") => vec![AdapterEventPayload::ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    name,
                    ok: true,
                    detail,
                }],
                Some("failed") => vec![AdapterEventPayload::ToolResult {
                    tool_call_id: tool_call_id.to_string(),
                    name,
                    ok: false,
                    detail,
                }],
                _ => vec![AdapterEventPayload::ToolProgress {
                    tool_call_id: tool_call_id.to_string(),
                    name,
                    detail,
                }],
            }
        }
        _ => Vec::new(),
    }
}

/// Extracts display text from an ACP `ContentBlock`. Non-text blocks
/// (image/audio/resource/resource_link) never leak their raw payload --
/// only a short, static placeholder naming the block's `type`.
fn content_block_text(block: &Value) -> Option<String> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block.get("text").and_then(Value::as_str).map(str::to_owned),
        Some(other) => Some(format!("[{other} content]")),
        None => None,
    }
}

/// Joins an ACP `ToolCallContent[]` array into one display string: plain
/// content blocks render their text, diffs render only the affected path
/// (never the old/new file text, which may be arbitrarily large or
/// sensitive), and embedded terminals render a static placeholder.
fn tool_call_content_text(items: Option<&Vec<Value>>) -> String {
    let Some(items) = items else {
        return String::new();
    };
    items
        .iter()
        .filter_map(|item| match item.get("type").and_then(Value::as_str) {
            Some("content") => item.get("content").and_then(content_block_text),
            Some("diff") => {
                let path = item
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown path>");
                Some(format!("diff: {path}"))
            }
            Some("terminal") => Some("[terminal output]".to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The runtime consequence of one ACP v1 turn `stopReason`.
pub struct StopOutcome {
    /// Events to emit before the turn returns. Empty for a clean end.
    pub events: Vec<AdapterEventPayload>,
    /// `Some(detail)` when the turn must be reported as a failure rather
    /// than a completion. `None` for `end_turn` and for `cancelled`,
    /// which is a requested outcome, not a fault.
    pub failure: Option<String>,
}

/// Maps an ACP v1 `session/prompt` `stopReason` onto the events and
/// failure disposition it represents.
///
/// Matching is case- and separator-insensitive (`end_turn`, `endTurn`,
/// and `EndTurn` are one reason) because ACP v1 specifies snake_case but
/// the installed Copilot CLI's exact casing is not pinned by any
/// committed fixture. An unrecognized reason is a failure carrying the
/// raw string: a silent success on an outcome this adapter does not
/// understand is exactly the defect being fixed.
///
/// `cancelled` emits the health event but does **not** fail, deliberately:
/// `run/cancel` already drives the run to `cancelled`, and returning `Err`
/// from `start` would race that into `failed` instead.
#[must_use]
pub fn copilot_normalize_stop_reason(stop_reason: &str) -> StopOutcome {
    let normalized = stop_reason.replace(['_', '-'], "").to_ascii_lowercase();
    match normalized.as_str() {
        "endturn" => StopOutcome {
            events: Vec::new(),
            failure: None,
        },
        "cancelled" | "canceled" => StopOutcome {
            events: vec![AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: visible("cancelled: the turn ended before completing".to_string()),
            }],
            failure: None,
        },
        "refusal" => StopOutcome {
            events: vec![AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: visible("refusal: the model refused the request".to_string()),
            }],
            failure: Some(format!(
                "copilot turn ended with stopReason \"{stop_reason}\""
            )),
        },
        "maxtokens" => StopOutcome {
            events: vec![AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: visible("maxTokens: the turn exhausted its token budget".to_string()),
            }],
            failure: Some("copilot turn ended with stopReason \"max_tokens\"".to_string()),
        },
        "maxturnrequests" => StopOutcome {
            events: vec![AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: visible("maxTurnRequests: the turn hit its request ceiling".to_string()),
            }],
            failure: Some("copilot turn ended with stopReason \"max_turn_requests\"".to_string()),
        },
        _other => StopOutcome {
            events: vec![AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: visible(format!("unknownStopReason: {stop_reason}")),
            }],
            failure: Some(format!(
                "copilot turn ended with an unrecognized stopReason {stop_reason:?}"
            )),
        },
    }
}

fn visible(value: String) -> Classified<String> {
    Classified {
        class: ContentClass::Visible,
        value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_turn_is_a_clean_completion_with_no_events() {
        let outcome = copilot_normalize_stop_reason("end_turn");
        assert!(outcome.events.is_empty());
        assert!(outcome.failure.is_none());
    }

    #[test]
    fn a_refusal_fails_the_turn_and_reports_unhealthy_protocol() {
        let outcome = copilot_normalize_stop_reason("refusal");
        assert_eq!(outcome.events.len(), 1);
        let event = &outcome.events[0];
        if let AdapterEventPayload::ProtocolHealthChanged { healthy, detail } = event {
            assert!(!*healthy);
            assert!(detail.value.contains("refusal"));
        } else {
            panic!("expected ProtocolHealthChanged");
        }
        assert!(outcome.failure.is_some());
    }

    #[test]
    fn a_cancellation_reports_unhealthy_but_does_not_fail_the_turn() {
        let outcome = copilot_normalize_stop_reason("cancelled");
        assert_eq!(outcome.events.len(), 1);
        if let AdapterEventPayload::ProtocolHealthChanged { healthy, detail } = &outcome.events[0] {
            assert!(!*healthy);
            assert!(detail.value.contains("cancelled"));
        } else {
            panic!("expected ProtocolHealthChanged");
        }
        assert!(outcome.failure.is_none());
    }

    #[test]
    fn a_cancellation_with_us_spelling_also_succeeds() {
        let outcome = copilot_normalize_stop_reason("canceled");
        assert!(outcome.failure.is_none());
    }

    #[test]
    fn token_exhaustion_fails_the_turn() {
        let outcome = copilot_normalize_stop_reason("max_tokens");
        assert!(outcome.failure.is_some());
    }

    #[test]
    fn a_max_turn_request_limit_fails_the_turn() {
        let outcome = copilot_normalize_stop_reason("max_turn_requests");
        assert!(outcome.failure.is_some());
    }

    #[test]
    fn an_unrecognized_stop_reason_fails_the_turn_and_echoes_the_raw_value() {
        let outcome = copilot_normalize_stop_reason("some_bizarre_reason");
        assert!(outcome.failure.is_some());
        assert!(
            outcome
                .failure
                .as_ref()
                .unwrap()
                .contains("some_bizarre_reason")
        );
    }

    #[test]
    fn an_unrecognized_stop_reason_reports_the_raw_vendor_token_in_the_health_detail() {
        // The detail must carry the vendor's exact token, not the lowercased,
        // separator-stripped match binding -- an operator greps vendor docs
        // and logs for the raw spelling (R42).
        let outcome = copilot_normalize_stop_reason("Some_Bizarre-Reason");
        assert_eq!(outcome.events.len(), 1);
        let AdapterEventPayload::ProtocolHealthChanged { detail, .. } = &outcome.events[0] else {
            panic!("expected ProtocolHealthChanged");
        };
        assert!(
            detail.value.contains("Some_Bizarre-Reason"),
            "detail was: {:?}",
            detail.value
        );
    }

    #[test]
    fn stop_reason_matching_ignores_case_and_separators() {
        let outcomes = ["endTurn", "end_turn", "END-TURN"].map(copilot_normalize_stop_reason);
        for outcome in outcomes {
            assert!(
                outcome.events.is_empty(),
                "expected no events for end_turn variant"
            );
            assert!(
                outcome.failure.is_none(),
                "expected no failure for end_turn variant"
            );
        }
    }
}
