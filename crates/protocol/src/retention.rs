//! Retention surface types (`retention/clean`, spec §8.2 `/crew clean`).
//!
//! Retention prunes only the EVENTS of terminal (or unassociated) runs --
//! the run rows themselves stay so `/crew runs` history keeps its shape.
//! Active runs are never touched.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Result of `retention/clean`: what one on-demand prune pass removed.
///
/// `deleted_events` counts journal rows removed by BOTH policies (age
/// cutoff and `maxRuns` recency cap). `runs_pruned` counts distinct
/// terminal runs beyond `retention.maxRuns` whose events were removed by
/// the recency cap alone; age-based deletions are not attributed to runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RetentionCleanResult {
    #[ts(type = "number")]
    pub deleted_events: u64,
    #[ts(type = "number")]
    pub runs_pruned: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_clean_result_is_camel_case() {
        let result = RetentionCleanResult {
            deleted_events: 12,
            runs_pruned: 3,
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["deletedEvents"], 12);
        assert_eq!(value["runsPruned"], 3);
        let parsed: RetentionCleanResult = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, result);
    }
}
