//! Event retention and pruning.
//!
//! Prunes the EVENTS of terminal (or unassociated) runs past the
//! configured retention policies: an age cutoff (`period`) and a recency
//! cap (`max_runs` -- the newest `max_runs` terminal runs by last
//! journaled sequence keep their events; older terminal runs are pruned).
//! Run ROWS are never deleted, so `/crew runs` history keeps its shape;
//! active runs are never touched by either policy.

use crate::DatabaseHandle;

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

        let mut deleted_events: u64 = 0;
        let mut runs_pruned: u64 = 0;

        // Age policy: ONE bounded DELETE per [`DatabaseHandle::run_domain_op`]
        // call, yielding the DB actor between batches so concurrent reads,
        // writes, and RPC traffic never queue behind an entire prune pass
        // (WP26).
        loop {
            let deleted = age_prune_batch(db_handle, &cutoff_text).await?;
            deleted_events += deleted;
            if deleted == 0 {
                break;
            }
        }

        // Recency policy (`max_runs`): pick the excess terminal runs in one
        // read op, then delete each run's events in batch ops -- again one
        // op per batch, yielding between every call.
        for run_id in excess_terminal_runs(db_handle, max_runs).await? {
            let mut run_deleted: u64 = 0;
            loop {
                let deleted = run_prune_batch(db_handle, &run_id).await?;
                run_deleted += deleted;
                if deleted == 0 {
                    break;
                }
            }
            deleted_events += run_deleted;
            if run_deleted > 0 {
                runs_pruned += 1;
            }
        }

        Ok(PruneReport {
            deleted_events,
            runs_pruned,
        })
    }
}

/// Rows deleted by one batch statement. Small enough that any single
/// op's lock window stays short; large enough that a prune pass is a
/// handful of ops, not thousands.
const PRUNE_BATCH: i64 = 1000;

const AGE_BATCH_SQL: &str = "DELETE FROM events
  WHERE sequence IN (
    SELECT sequence FROM events
    WHERE timestamp < ?1
      AND (run_id IS NULL OR run_id IN (
        SELECT run_id FROM runs
        WHERE state IN ('succeeded', 'failed', 'cancelled', 'lost')
      ))
    LIMIT ?2
  )";

const RUN_BATCH_SQL: &str = "DELETE FROM events
  WHERE run_id = ?1 AND sequence IN (
    SELECT sequence FROM events WHERE run_id = ?1 LIMIT ?2
  )";

const EXCESS_RUNS_SQL: &str = "SELECT r.run_id FROM runs r
  WHERE r.state IN ('succeeded', 'failed', 'cancelled', 'lost')
  ORDER BY COALESCE(
    (SELECT MAX(e.sequence) FROM events e WHERE e.run_id = r.run_id), 0
  ) DESC, r.run_id ASC
  LIMIT -1 OFFSET ?1";

/// Deletes ONE age-policy batch (at most [`PRUNE_BATCH`] rows) in its own
/// domain op and reports how many rows went.
async fn age_prune_batch(db_handle: &DatabaseHandle, cutoff_text: &str) -> Result<u64, String> {
    let cutoff_text = cutoff_text.to_owned();
    let value = db_handle
        .run_domain_op(Box::new(move |conn| {
            let deleted =
                conn.execute(AGE_BATCH_SQL, rusqlite::params![cutoff_text, PRUNE_BATCH])?;
            Ok(serde_json::json!({ "deleted": deleted }))
        }))
        .await
        .map_err(|e| format!("failed to execute prune operation: {e}"))?;
    Ok(value["deleted"].as_u64().unwrap_or(0))
}

/// Deletes at most [`PRUNE_BATCH`] of one run's events in its own domain op.
async fn run_prune_batch(db_handle: &DatabaseHandle, run_id: &str) -> Result<u64, String> {
    let run_id = run_id.to_owned();
    let value = db_handle
        .run_domain_op(Box::new(move |conn| {
            let deleted = conn.execute(RUN_BATCH_SQL, rusqlite::params![run_id, PRUNE_BATCH])?;
            Ok(serde_json::json!({ "deleted": deleted }))
        }))
        .await
        .map_err(|e| format!("failed to execute prune operation: {e}"))?;
    Ok(value["deleted"].as_u64().unwrap_or(0))
}

/// Reads the terminal runs past the `max_runs` recency cap in one read-only
/// domain op: every terminal run whose last journaled sequence sorts below
/// the newest `max_runs`.
async fn excess_terminal_runs(
    db_handle: &DatabaseHandle,
    max_runs: u32,
) -> Result<Vec<String>, String> {
    let value = db_handle
        .run_domain_op(Box::new(move |conn| {
            let mut stmt = conn.prepare(EXCESS_RUNS_SQL)?;
            let rows = stmt
                .query_map(rusqlite::params![i64::from(max_runs)], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(serde_json::json!({ "runIds": rows }))
        }))
        .await
        .map_err(|e| format!("failed to execute prune operation: {e}"))?;
    let ids = value["runIds"].as_array().cloned().unwrap_or_default();
    Ok(ids
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect())
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

    /// Seeds `n` unassociated journal rows stamped with `timestamp`.
    async fn seed_unassociated_events(db: &DatabaseHandle, n: usize, timestamp: String) {
        db.run_domain_op(Box::new(move |conn| {
            for _ in 0..n {
                conn.execute(
                    "INSERT INTO events (sequence, timestamp, project_id, run_id, event_json)
                     SELECT COALESCE(MAX(sequence), 0) + 1, ?1, 'p-test', NULL, '{}'
                     FROM events",
                    rusqlite::params![timestamp],
                )?;
            }
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed events");
    }

    async fn test_db() -> DatabaseHandle {
        let dir = tempfile::TempDir::new().unwrap();
        DatabaseHandle::start(dir.path().join("journal.db"))
            .await
            .unwrap()
    }

    /// WP26: one call == one domain op == at most [`PRUNE_BATCH`] rows.
    /// A 2500-row backlog therefore takes three deleting ops and a fourth
    /// confirming-empty op -- each yielding the DB actor between calls --
    /// instead of one op holding the actor across the whole loop.
    #[tokio::test]
    async fn age_prune_deletes_one_bounded_batch_per_domain_op() {
        let db = test_db().await;
        seed_unassociated_events(&db, 2_500, "2020-01-01T00:00:00Z".to_owned()).await;

        assert_eq!(
            age_prune_batch(&db, "2026-01-01T00:00:00Z").await.unwrap(),
            u64::try_from(PRUNE_BATCH).unwrap()
        );
        assert_eq!(
            age_prune_batch(&db, "2026-01-01T00:00:00Z").await.unwrap(),
            u64::try_from(PRUNE_BATCH).unwrap()
        );
        assert_eq!(
            age_prune_batch(&db, "2026-01-01T00:00:00Z").await.unwrap(),
            500
        );
        assert_eq!(
            age_prune_batch(&db, "2026-01-01T00:00:00Z").await.unwrap(),
            0,
            "the final op confirms emptiness rather than spinning forever"
        );
    }

    /// Seeds one succeeded run through the real projection path and returns
    /// its id. The lifecycle edges are not what these tests exercise; the
    /// row is walked straight to a terminal state.
    async fn seed_terminal_run(
        db: &DatabaseHandle,
        project_id: crew_protocol::ProjectId,
    ) -> crew_protocol::RunId {
        let task_id = crew_protocol::TaskId::new();
        let worker_id = crew_protocol::WorkerId::new();
        let run_id = crew_protocol::RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = crate::domain::DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &crew_protocol::TaskRef {
                    owner_client_instance_id: "omp-1".into(),
                    revision: 1,
                },
            )?;
            repo.create_worker(&crew_protocol::Worker {
                worker_id,
                profile_ref: crew_protocol::WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".into(),
                    adapter: "fake".into(),
                    model: "test".into(),
                    permission_envelope: serde_json::json!({}),
                },
                parent_worker_id: None,
                created_at: crew_protocol::Timestamp::now(),
            })?;
            repo.submit_run(
                &crew_protocol::Run {
                    run_id,
                    task_id,
                    worker_id,
                    state: crew_protocol::RunState::try_from("queued").expect("queued is valid"),
                    flags: crew_protocol::RunFlags::default(),
                    vendor_session_id: None,
                    started_at: None,
                    completed_at: None,
                },
                None,
                None,
            )?;
            conn.execute(
                "UPDATE runs SET state = 'succeeded' WHERE run_id = ?1",
                rusqlite::params![run_id.to_string()],
            )?;
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed terminal run");
        run_id
    }

    async fn count_events_for(db: &DatabaseHandle, run_id: &str) -> i64 {
        let run_id = run_id.to_owned();
        db.run_domain_op(Box::new(move |conn| {
            Ok(serde_json::json!({
                "n": conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE run_id = ?1",
                    [run_id],
                    |row| row.get::<_, i64>(0),
                )?,
            }))
        }))
        .await
        .unwrap()["n"]
            .as_i64()
            .unwrap()
    }

    /// End-to-end: the recency cap keeps the newest `max_runs` terminal
    /// runs by last journaled sequence and prunes the rest.
    #[tokio::test]
    async fn prune_keeps_the_newest_max_runs_terminal_runs() {
        let db = test_db().await;
        let project_id = crew_protocol::ProjectId::new();
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();

        // Journaled in this order: `old` takes lower sequences than `new`,
        // so the recency cap with max_runs=1 keeps only `new`. Fresh
        // timestamps keep the age policy out of the assertion entirely.
        let old = seed_terminal_run(&db, project_id).await;
        let new = seed_terminal_run(&db, project_id).await;
        for run_id in [old, new] {
            for _ in 0..3 {
                let run = run_id.to_string();
                let ts = now.clone();
                db.run_domain_op(Box::new(move |conn| {
                    conn.execute(
                        "INSERT INTO events (sequence, timestamp, project_id, run_id, event_json)
                         SELECT COALESCE(MAX(sequence), 0) + 1, ?1, 'p-test', ?2, '{}'
                         FROM events",
                        rusqlite::params![ts, run],
                    )?;
                    Ok(serde_json::json!({}))
                }))
                .await
                .unwrap();
            }
        }
        // Each run carries its own `RunQueued` journal row (from
        // `submit_run`) plus the 3 seeded events.
        let report = Retention::new("30d", 1).prune(&db).await.unwrap();
        assert_eq!(report.runs_pruned, 1, "only the older sibling: {report:?}");
        assert_eq!(report.deleted_events, 4);
        assert_eq!(count_events_for(&db, &old.to_string()).await, 0);
        assert_eq!(count_events_for(&db, &new.to_string()).await, 4);
    }
}
