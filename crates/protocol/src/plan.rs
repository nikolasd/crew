//! Result contracts for the crew v2 leader/subtask plan flow
//! (`plan/propose`, `plan/decide`, `plan/get`) and the run-timeout
//! acknowledgement (`run/timeoutAck`).
//!
//! Only the wire-facing result shapes are defined here: the actual
//! propose/decide/get/timeoutAck orchestration lands in a later work
//! package (crew v2 gap closure WP17/WP21). Until then the daemon refuses
//! every one of these methods with a "not yet implemented" JSON-RPC error
//! (`crate::service::orchestration`); this module exists so extension code
//! can be written against a stable wire contract in the meantime.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::event::PlanSpec;
use crate::ids::RunId;

/// Result of `plan/propose`: the sequence of the `PlanProposed` event it
/// persisted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PlanProposeResult {
    pub run_id: RunId,
    #[ts(type = "number")]
    pub sequence: u64,
}

/// Result of `plan/decide`: the sequence of the `PlanDecided` event it
/// persisted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PlanDecideResult {
    pub run_id: RunId,
    #[ts(type = "number")]
    pub sequence: u64,
}

/// Result of `plan/get`: the most recently proposed plan for a run and its
/// decision, if any. `plan: None` means no plan has been proposed yet;
/// `approved: None` means a plan exists but has not yet been decided.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PlanGetResult {
    pub run_id: RunId,
    pub plan: Option<PlanSpec>,
    pub approved: Option<bool>,
}

/// Result of `run/timeoutAck`: the sequence of the event the leader's
/// timeout acknowledgement persisted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RunTimeoutAckResult {
    pub run_id: RunId,
    #[ts(type = "number")]
    pub sequence: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_get_result_is_camel_case_and_allows_null_plan() {
        let result = PlanGetResult {
            run_id: RunId::new(),
            plan: None,
            approved: None,
        };
        let value = serde_json::to_value(&result).unwrap();
        assert!(value["plan"].is_null());
        assert!(value["approved"].is_null());
        assert!(value.get("runId").is_some());
    }

    #[test]
    fn plan_propose_result_rejects_unknown_field() {
        let value = serde_json::json!({
            "runId": RunId::new().to_string(),
            "sequence": 1,
            "unexpected": true,
        });
        assert!(serde_json::from_value::<PlanProposeResult>(value).is_err());
    }
}
