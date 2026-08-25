//! Event retention and pruning.
//!
//! Prunes the EVENTS of terminal (or unassociated) runs past the
//! configured retention policies: an age cutoff (`period`) and a recency
//! cap (`max_runs` -- the newest `max_runs` terminal runs by last
//! journaled sequence keep their events; older terminal runs are pruned).
//! Run ROWS are never deleted, so `/crew runs` history keeps its shape;
//! active runs are never touched by either policy.

use crate::DatabaseHandle;
use crate::db::DomainClosure;

/// What one prune pass removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneReport {
    /// Journal rows deleted by BOTH policies combined.
    pub deleted_events: u64,
    /// Distinct terminal runs whose events were removed by the `max_runs`
    /// recency cap alone (they had events to delete). Age-based deletions
    /// are not attributed to runs.
    pub runs_pruned: u64,
}

/// Retention policy for event pruning.
#[derive(Debug, Clone)]
pub struct Retention {
    pub period: String,
    pub max_runs: u32,
}

impl Retention {
    #[must_use]
    pub fn new(period: impl Into<String>, max_runs: u32) -> Self {
        Self {
            period: period.into(),
            max_runs,
        }
    }

    pub async fn prune(&self, db_handle: &DatabaseHandle) -> Result<PruneReport, String> {
        let period = parse_period(&self.period)?;

        // Calculate the cutoff timestamp as RFC3339 text matching how `timestamp` is stored
        let cutoff_text = time::OffsetDateTime::now_utc()
            .checked_sub(time::Duration::seconds(period as i64))
            .ok_or_else(|| "retention period exceeds system time".to_string())?
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| format!("failed to format cutoff timestamp: {e}"))?;

        let max_runs = self.max_runs;

        // Both policies run inside ONE domain op for now: the actor
        // serializes domain ops anyway, and the deletes are bounded per
        // statement. (WP26 splits the passes across separate ops so other
        // work can interleave between batches.)
        let closure = Box::new(
            move |conn: &mut rusqlite::Connection| -> Result<serde_json::Value, crate::domain::DomainError> {
                let mut deleted_events: u64 = 0;
                let mut runs_pruned: u64 = 0;

                // Age policy: events older than the cutoff for terminal
                // runs (or no run at all). Bounded batches keep any single
                // statement's lock window short.
                loop {
                    let deleted = conn.execute(
                        "DELETE FROM events
                         WHERE sequence IN (
                           SELECT sequence FROM events
                           WHERE timestamp < ?1
                             AND (run_id IS NULL OR run_id IN (
                               SELECT run_id FROM runs
                               WHERE state IN ('succeeded', 'failed', 'cancelled', 'lost')
                             ))
                           LIMIT 1000
                         )",
                        rusqlite::params![cutoff_text.as_str()],
                    )?;
                    deleted_events += deleted as u64;
                    if deleted == 0 {
                        break;
                    }
                }

                // Recency policy (`max_runs`): the newest `max_runs`
                // terminal runs BY LAST JOURNALED SEQUENCE keep their
                // events; every older terminal run is pruned. Runs with no
                // events at all sort oldest and cost nothing to prune.
                {
                    let excess: Vec<String> = {
                        let mut stmt = conn.prepare(
                            "SELECT r.run_id FROM runs r
                             WHERE r.state IN ('succeeded', 'failed', 'cancelled', 'lost')
                             ORDER BY COALESCE(
                               (SELECT MAX(e.sequence) FROM events e WHERE e.run_id = r.run_id), 0
                             ) DESC, r.run_id ASC
                             LIMIT -1 OFFSET ?1",
                        )?;
                        let rows = stmt.query_map(
                            rusqlite::params![i64::from(max_runs)],
                            |row| row.get::<_, String>(0),
                        )?;
                        rows.collect::<Result<Vec<_>, _>>()?
                    };
                    for run_id in excess {
                        let mut run_deleted: u64 = 0;
                        loop {
                            let deleted = conn.execute(
                                "DELETE FROM events WHERE run_id = ?1 AND sequence IN (
                                   SELECT sequence FROM events WHERE run_id = ?1 LIMIT 1000
                                 )",
                                rusqlite::params![run_id],
                            )?;
                            run_deleted += deleted as u64;
                            if deleted == 0 {
                                break;
                            }
                        }
                        deleted_events += run_deleted;
                        if run_deleted > 0 {
                            runs_pruned += 1;
                        }
                    }
                }

                Ok(serde_json::json!({
                    "deletedEvents": deleted_events,
                    "runsPruned": runs_pruned,
                }))
            },
        ) as DomainClosure;

        let value = db_handle
            .run_domain_op(closure)
            .await
            .map_err(|e| format!("failed to execute prune operation: {e}"))?;

        Ok(PruneReport {
            deleted_events: value["deletedEvents"].as_u64().unwrap_or(0),
            runs_pruned: value["runsPruned"].as_u64().unwrap_or(0),
        })
    }
}

pub(crate) fn parse_period(period: &str) -> Result<u64, String> {
    let period = period.trim();
    if let Some(days) = period.strip_suffix("d") {
        days.parse::<u64>()
            .map(|d| d * 24 * 60 * 60)
            .map_err(|e| format!("invalid period: {e}"))
    } else if let Some(months) = period.strip_suffix("mo") {
        months
            .parse::<u64>()
            .map(|m| m * 30 * 24 * 60 * 60)
            .map_err(|e| format!("invalid period: {e}"))
    } else if let Some(years) = period.strip_suffix("y") {
        years
            .parse::<u64>()
            .map(|y| y * 365 * 24 * 60 * 60)
            .map_err(|e| format!("invalid period: {e}"))
    } else {
        Err(format!(
            "invalid retention period {period:?}: expected a number followed by 'd', 'mo', or 'y'"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retention_new() {
        let retention = Retention::new("30d", 20);
        assert_eq!(retention.period, "30d");
        assert_eq!(retention.max_runs, 20);
    }

    #[test]
    fn test_parse_period_units() {
        assert_eq!(parse_period("7d").unwrap(), 7 * 24 * 60 * 60);
        assert_eq!(parse_period("2mo").unwrap(), 2 * 30 * 24 * 60 * 60);
        assert_eq!(parse_period("1y").unwrap(), 365 * 24 * 60 * 60);
        assert!(parse_period("30").is_err());
        assert!(parse_period("w").is_err());
    }
}
