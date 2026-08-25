//! The worker-timeout sweep (WP19): turns the shared [`ActivityClock`]'s
//! liveness state into durable [`RuntimeEvent::WorkerTimeout`] facts.
//!
//! Two deadlines per run, per spec §7.5:
//!
//! * **Inactivity** — from the run's last journaled vendor event. Fires
//!   once per quiet stretch; new activity re-arms it.
//! * **Total** — from the run's clock start. Independent of activity; no
//!   amount of chatter disarms it.
//!
//! The runtime never kills on timeout — it journals and broadcasts the
//! fact, and the leader decides what to do (WP21's `run/timeoutAck`).
//! Every journal decision re-checks liveness inside the database actor
//! closure (`record_worker_timeout_if_live`), so a run that settled
//! between the clock snapshot and the write never receives a timeout fact.

use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crew_protocol::{EventEnvelope, ProjectId, TimeoutKind};

use crate::adapter::{ActivityClock, due_timeouts, millis_since};
use crate::db::DatabaseHandle;
use crate::domain::{DomainRepository, broadcast_committed, embed_envelope};

use serde_json::json;

/// One sweep pass: snapshot the clocks, decide which deadlines are due,
/// journal each due fact for runs that are still live, mark them journaled.
///
/// # Errors
/// Never fails the daemon: individual failures are logged and left to the
/// next tick (the clock flags stay unset, so nothing is silently dropped).
pub async fn sweep_once(
    db: &DatabaseHandle,
    project_id: ProjectId,
    events_tx: &broadcast::Sender<EventEnvelope>,
    clock: &ActivityClock,
    inactivity_timeout: Duration,
    total_timeout: Duration,
) {
    let now = Instant::now();
    let snapshot = clock.snapshot();
    for (run_id, activity) in snapshot {
        let kinds = due_timeouts(&activity, inactivity_timeout, total_timeout, now);
        if kinds.is_empty() {
            continue;
        }
        for kind in kinds {
            let since_ms = match kind {
                TimeoutKind::Inactivity => millis_since(activity.last_activity, now),
                TimeoutKind::Total => millis_since(activity.started_at, now),
            };
            let outcome = db
                .run_domain_op(Box::new({
                    move |conn| {
                        // The database actor's op boundary carries plain
                        // `Value`s: encode "run not live" as JSON null.
                        Ok(DomainRepository::new(conn, project_id)
                            .record_worker_timeout_if_live(run_id, kind, since_ms)?
                            .map(|committed| {
                                embed_envelope(
                                    json!({ "sequence": committed.sequence }),
                                    &committed.envelope,
                                )
                            })
                            .unwrap_or(serde_json::Value::Null))
                    }
                }))
                .await;
            match outcome {
                Ok(mut value) if !value.is_null() => {
                    broadcast_committed(events_tx, &mut value);
                    tracing::info!(
                        run_id = %run_id,
                        kind = ?kind,
                        since_ms,
                        "worker_timeout_reported"
                    );
                    // Mark only after a successful commit: a failed append
                    // leaves the flag unset so the next tick retries.
                    clock.mark_journaled(&run_id, kind);
                }
                Ok(_) => {
                    // The run is terminal or gone: drop its clock so this
                    // entry stops costing the sweep anything.
                    clock.forget(&run_id);
                }
                Err(err) => {
                    tracing::warn!(
                        run_id = %run_id,
                        kind = ?kind,
                        error = %err,
                        "worker_timeout_journal_failed"
                    );
                }
            }
        }
    }
}
