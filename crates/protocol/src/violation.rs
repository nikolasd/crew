//! Policy-violation listing contracts (R80).
//!
//! `policy/violation/decide` reports `quarantineCleared: false` when a
//! different violation on the same run is still open; this listing is how
//! an operator finds *which* one without diffing `PolicyViolationRecorded`
//! against `PolicyViolationDecided` in the raw event stream.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::PolicyViolationId;

/// One recorded policy violation, projected exactly from the
/// `policy_violations` table: an undecided row (`resolution` null) on a
/// quarantined run is the one holding the quarantine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PolicyViolationSummary {
    pub violation_id: PolicyViolationId,
    pub run_id: String,
    pub task_id: String,
    pub worker_id: String,
    /// The vendor-reported child id, when the violation had a vendor
    /// child at all (a cost ceiling does not).
    pub vendor_child_id: Option<String>,
    pub vendor_parent_ref: Option<String>,
    /// The action policy applied when the violation was recorded
    /// (`quarantine`, `cancel`, `quarantineAndCancel`).
    pub action: String,
    pub created_at: String,
    /// Set once decided via `policy/violation/decide`.
    pub resolved_at: Option<String>,
    /// `"release"` or `"cancel"` once decided; absent while open.
    pub resolution: Option<String>,
    pub resolved_by: Option<String>,
}

/// Result of `policy/violation/list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PolicyViolationListResult {
    pub violations: Vec<PolicyViolationSummary>,
}
