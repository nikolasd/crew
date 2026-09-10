//! The worker-timeout sweep: turns the shared [`ActivityClock`]'s
//! liveness state into durable [`RuntimeEvent::WorkerTimeout`] facts.
//!
//! Two deadlines per run:
//!
//! * **Inactivity** — from the run's last journaled vendor event. Fires
//!   once per quiet stretch; new activity re-arms it.
//! * **Total** — from the run's clock start. Independent of activity; no
//!   amount of chatter disarms it.
//!
//! The runtime never kills on timeout — it journals and broadcasts the
//! fact, and the leader decides what to do via `run/timeoutAck`.
//! Every journal decision re-checks liveness inside the database actor
//! closure (`record_worker_timeout_if_live`), so a run that settled
//! between the clock snapshot and the write never receives a timeout fact.
//!
//! This sweep never transitions a run's state on its own -- a parked run
//! whose leader is still connected is never settled here, however long
//! it stays silent. It used to: an inactivity backstop settled a
//! leader-abandoned turn to `lost` (ADR-0027 wave 3). The maintainer
//! ruled that out -- silence alone is never evidence the leader gave up,
//! only the leader's connection actually being gone is -- so settlement
//! now happens exclusively through `crate::ipc::leader_registry`'s
//! disconnect grace window (see ADR-0036). `lost` keeps its narrower,
//! original meaning (ADR-0023): the evidence itself is missing or
//! ambiguous, not "the leader went quiet."

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
                // `due_timeouts` (the only producer of `kind` here) only
                // ever returns `Inactivity`/`Total` -- it has no notion of
                // a leader's connection state at all. `LeaderGone` is
                // journaled from the leader-grace-window teardown
                // directly (`service::orchestration::settle_leader_gone`),
                // never through this worker-liveness sweep. This arm
                // should be unreachable on that invariant, but this loop
                // runs inside a long-lived background task, not a test --
                // panicking here would silently take the whole sweep down
                // rather than fail loudly where someone would notice. Log
                // and skip this one kind instead, so a future producer
                // added here in error degrades to a missed fact, not a
                // dead daemon.
                TimeoutKind::LeaderGone => {
                    tracing::error!(
                        run_id = %run_id,
                        "due_timeouts produced LeaderGone, which it should never be able to \
                         observe -- skipping this kind rather than journaling a nonsensical \
                         since_ms"
                    );
                    continue;
                }
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
                    // A parked run whose leader is still alive (connected,
                    // regardless of how long it stays silent) is never
                    // settled here -- only the fact above is journaled.
                    // Settlement now happens exclusively through the
                    // leader-disconnect grace window
                    // (`ipc::leader_registry`,
                    // `OrchestrationService::settle_leader_gone`): the
                    // inactivity backstop that used to settle a
                    // leader-abandoned turn to `lost` after this same
                    // `Inactivity` fact (ADR-0027 wave 3) was removed
                    // once the maintainer ruled that the runtime must
                    // never decide a leader gave up based on silence
                    // alone -- only on its connection actually being
                    // gone. See ADR-0036.
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
