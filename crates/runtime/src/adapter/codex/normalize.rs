//! Normalizes raw Codex app-server JSON-RPC notifications/requests into
//! [`AdapterEventPayload`]s (for server notifications) and
//! [`PendingApproval`]s (for server-issued approval requests).
//!
//! Grounded against the real `codex app-server generate-json-schema`
//! output on the installed 0.145.0 binary (see
//! `fixtures/adapters/codex/schema-version.json`): method names, the
//! `ThreadItem` variant tags (`agentMessage`, `reasoning`,
//! `commandExecution`, `fileChange`, ...), and the `execCommandApproval`/
//! `applyPatchApproval` server-request shapes below all come from that
//! generated schema, not guesswork.
//!
//! `reasoning` thread items carry the model's hidden chain-of-thought
//! (the `content`/`summary` string arrays) -- this module drops them
//! before any [`AdapterEvent`](super::super::event_sink::AdapterEvent) is
//! ever constructed, per the redaction-boundary discipline: thinking
//! content must never reach the sink at all, not merely be defensively
//! nulled out there.

use crate::adapter::AdapterEventPayload;

use crew_protocol::{ArtifactId, Classified, ContentClass};

use serde_json::Value;

/// A vendor-issued approval request this adapter observed but has not yet
/// resolved. Never routed through [`AdapterEventSink`](super::super::event_sink::AdapterEventSink) --
/// approvals are reported through [`crate::adapter::Adapter::snapshot`]
/// until the `ApprovalService` RPC seam is adapter-reachable (see the
/// Worker Adapters plan's approval note).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    /// The JSON-RPC request id the eventual response must echo.
    pub request_id: Value,
    /// `"execCommand"` or `"applyPatch"`.
    pub kind: &'static str,
    /// The vendor's own correlation id for this approval (`callId`).
    pub call_id: String,
    /// A short, non-secret summary safe to surface in a snapshot (never
    /// echoes raw command/patch content verbatim, to stay clear of any
    /// secret-shaped argument).
    pub summary: String,
}

fn visible(text: impl Into<String>) -> Classified<String> {
    Classified {
        class: ContentClass::Visible,
        value: text.into(),
    }
}

fn item_type(item: &Value) -> Option<&str> {
    item.get("type")?.as_str()
}

fn item_id(item: &Value) -> String {
    item.get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Normalizes one Codex app-server server notification (`method`+`params`
/// from a raw JSON-RPC notification frame) into an [`AdapterEventPayload`],
/// or `None` when the notification carries nothing this adapter's declared
/// events cover (including every `reasoning`-typed item, which is always
/// dropped rather than emitted).
#[must_use]
pub fn notification_to_event(method: &str, params: &Value) -> Option<AdapterEventPayload> {
    match method {
        "thread/started" => {
            let vendor_session_id = params.get("thread")?.get("id")?.as_str()?.to_string();
            Some(AdapterEventPayload::VendorSessionEstablished { vendor_session_id })
        }
        "item/started" => {
            let item = params.get("item")?;
            match item_type(item)? {
                "commandExecution" => Some(AdapterEventPayload::ToolStarted {
                    tool_call_id: item_id(item),
                    name: "commandExecution".to_string(),
                }),
                _ => None,
            }
        }
        "item/completed" => {
            let item = params.get("item")?;
            match item_type(item)? {
                "agentMessage" => {
                    let text = item.get("text")?.as_str()?.to_string();
                    Some(AdapterEventPayload::MessageFinal {
                        role: "assistant".to_string(),
                        text: visible(text),
                    })
                }
                // Hidden chain-of-thought: dropped before it ever reaches
                // an AdapterEvent, per this module's own doc comment.
                "reasoning" => None,
                "commandExecution" => {
                    let status = item.get("status").and_then(Value::as_str).unwrap_or("");
                    let detail = item
                        .get("aggregatedOutput")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Some(AdapterEventPayload::ToolResult {
                        tool_call_id: item_id(item),
                        name: "commandExecution".to_string(),
                        ok: status == "completed",
                        detail: visible(detail),
                    })
                }
                "fileChange" => Some(AdapterEventPayload::ArtifactProduced {
                    artifact_id: ArtifactId::new(),
                    artifact_kind: "fileChange".to_string(),
                }),
                _ => None,
            }
        }
        "item/agentMessage/delta" => {
            let delta = params.get("delta")?.as_str()?.to_string();
            Some(AdapterEventPayload::MessageChunk {
                role: "assistant".to_string(),
                text: visible(delta),
            })
        }
        "thread/tokenUsage/updated" => {
            let total = params.get("tokenUsage")?.get("total")?;
            let input_tokens = total.get("inputTokens")?.as_u64()?;
            let output_tokens = total.get("outputTokens")?.as_u64()?;
            Some(AdapterEventPayload::UsageReported {
                input_tokens,
                output_tokens,
                cost_usd: None,
            })
        }
        // A vendor-side turn failure -- an expired credential, an
        // exhausted quota (`usageLimitExceeded`), a refused request. Codex
        // reports it as a notification and then completes the turn with
        // `status: "failed"`, so without this arm the failure is invisible:
        // the turn simply never produces a final message and every caller
        // sees an unexplained timeout. Observed verbatim against
        // `codex-cli 0.146.0`.
        "error" => {
            let error = params.get("error")?;
            let message = error.get("message").and_then(Value::as_str)?;
            let code = error
                .get("codexErrorInfo")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Some(AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: visible(format!("{code}: {message}")),
            })
        }
        // `turn/completed` closes out a turn whose content was already
        // fully represented by the `item/started`/`item/completed`
        // lifecycle events above -- there is no dedicated "turn
        // completed" AdapterEventPayload variant (see this adapter's
        // final summary for why one was not added), so nothing further
        // is emitted here.
        _ => None,
    }
}

/// Normalizes one Codex app-server server *request* (a JSON-RPC request
/// with an `id` sent server -> client) into a [`PendingApproval`], or
/// `None` for any request this adapter does not treat as an approval.
#[must_use]
pub fn server_request_to_pending_approval(
    id: &Value,
    method: &str,
    params: &Value,
) -> Option<PendingApproval> {
    match method {
        "execCommandApproval" => {
            let call_id = params.get("callId")?.as_str()?.to_string();
            let command = params
                .get("command")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            Some(PendingApproval {
                request_id: id.clone(),
                kind: "execCommand",
                call_id,
                summary: format!("exec command approval requested ({} arg(s))", {
                    command.split_whitespace().count()
                }),
            })
        }
        "applyPatchApproval" => {
            let call_id = params.get("callId")?.as_str()?.to_string();
            let file_count = params
                .get("fileChanges")
                .and_then(Value::as_object)
                .map(|m| m.len())
                .unwrap_or(0);
            Some(PendingApproval {
                request_id: id.clone(),
                kind: "applyPatch",
                call_id,
                summary: format!("apply-patch approval requested ({file_count} file(s))"),
            })
        }
        _ => None,
    }
}

/// Maps this adapter's plain `decision` string (as passed to
/// [`crate::adapter::Adapter::respond_to_approval`]) to the wire shape
/// `ExecCommandApprovalResponse`/`ApplyPatchApprovalResponse` both use for
/// their `decision` field (`ReviewDecision`, verified against the real
/// generated schema).
///
/// # Errors
/// Returns an error detail string if `decision` is not one of
/// `"approve"`/`"deny"`.
pub fn decision_to_review_decision(decision: &str) -> Result<Value, String> {
    match decision {
        "approve" => Ok(Value::String("approved".to_string())),
        "deny" => Ok(serde_json::json!({ "denied": { "rejection": "denied by operator" } })),
        other => Err(format!("unknown approval decision {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_item_never_emits_an_event() {
        let params = serde_json::json!({
            "threadId": "t", "turnId": "u", "completedAtMs": 1,
            "item": {"id": "r1", "type": "reasoning", "content": ["secret cot"], "summary": []}
        });
        assert!(notification_to_event("item/completed", &params).is_none());
    }

    #[test]
    fn decision_mapping_rejects_unknown_values() {
        assert!(decision_to_review_decision("maybe").is_err());
        assert!(decision_to_review_decision("approve").is_ok());
        assert!(decision_to_review_decision("deny").is_ok());
    }
}
