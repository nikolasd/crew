//! Approval request and decision types.
//!
//! When an adapter reports an approval, the runtime atomically creates
//! the request, transitions the working run to `waitingUser`, and emits
//! one correlated event. On decision, the runtime records the decision
//! before invoking the adapter callback.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::Timestamp;
use crate::ids::{ApprovalId, RunId, TaskId};

/// An approval request raised by the runtime when an adapter needs
/// human or policy input.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ApprovalRequest {
    /// The approval request identifier (UUIDv7).
    #[serde(rename = "approvalId")]
    pub approval_id: ApprovalId,
    /// The run that triggered the approval.
    #[serde(rename = "runId")]
    pub run_id: RunId,
    /// The task this approval relates to.
    #[serde(rename = "taskId")]
    pub task_id: TaskId,
    /// The action the adapter is requesting approval for.
    pub action: String,
    /// Arguments after redaction (never raw secrets).
    #[ts(type = "object")]
    pub arguments: serde_json::Value,
    /// Whether human approval is required.
    #[serde(rename = "humanRequired")]
    pub human_required: bool,
    /// The policy reason for this approval.
    pub policy_reason: String,
    /// When the request was created (UTC RFC 3339).
    #[serde(rename = "createdAt")]
    pub created_at: Timestamp,
    /// When the request was decided (UTC RFC 3339), if applicable.
    #[serde(rename = "decidedAt", skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<Timestamp>,
    /// The decision made, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    /// Who produced the decision (`"human"` or `"model"`), when decided.
    // R92: persisted since MIGRATION_7, carried on `ApprovalDecided`
    // events, and projected by `approval/list`.
    /// Who decided this approval, when that provenance was recorded.
    #[serde(rename = "decidedBy", skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<DecidedBy>,
    /// The decision rationale, when one was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A decision on an approval request: approve or deny.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ApprovalDecision {
    /// Either `"approve"` or `"deny"`.
    pub decision: String,
    /// The reason for this decision.
    pub reason: String,
}

/// Who produced an approval decision. Sent by `approval/decide` and
/// enforced by the runtime: an approval created with
/// `human_required: true` may only be decided by `human`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DecidedBy {
    /// A human answered an interactive dialog.
    Human,
    /// The calling model supplied the decision itself.
    Model,
}

impl DecidedBy {
    /// The bare wire token (`human`/`model`) -- exactly the string the
    /// serde `rename_all = "camelCase"` produces, without JSON quoting.
    /// Used wherever the token is persisted as a scalar column value
    /// (R34): `serde_json::to_string` would store `"human"` with quotes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Model => "model",
        }
    }
}
