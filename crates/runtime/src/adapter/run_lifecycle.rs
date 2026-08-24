//! Applies the durable `RunState` edges the adapter layer has evidence for.
//!
//! Wraps a run's [`AdapterEventSink`] and, *after* the inner sink has
//! journaled each event, commits (and broadcasts) the lifecycle edge that
//! event is evidence of:
//!
//! | evidence | edge |
//! |---|---|
//! | `ProcessStarted` | `queued -> starting` |
//! | any other payload except `ProcessExited` | up to `working` |
//! | `ProcessExited { exit_code: Some(0), signal: None }` | `-> succeeded` |
//! | `ProcessExited` with a non-zero code or a signal | `-> failed` |
//! | `ProcessExited` with no code and no signal | `-> lost` |
//!
//! Four properties this shape depends on:
//!
//! * **Edges are walked, never jumped.** Codex emits no `ProcessStarted` at
//!   all (its `spawn_client` observes the pid but journals nothing), and
//!   `queued -> working`, `starting -> succeeded`, `waitingUser -> succeeded`
//!   are all illegal in `RunState::can_transition_to`. So a target is reached
//!   by committing each legal hop in turn, which is also what keeps
//!   `runs.started_at` correct: `DomainRepository::transition_run` stamps it
//!   only on the `starting` edge.
//! * **Forward only.** `working` is applied only from `queued`/`starting`, so
//!   vendor output arriving while a run sits in `waitingUser` (an approval) or
//!   `paused` never clobbers that state.
//! * **A terminal state always wins.** A run cancelled through `run/cancel`
//!   (which commits `cancelled` before killing the process) is already
//!   terminal when its exit arrives; every walk stops on a terminal state, and
//!   `transition_run` itself rejects the edge and appends nothing even if a
//!   concurrent commit wins the race.
//! * **No edge without durable evidence.** A failed inner `emit` (the
//!   sanitize/journal/broadcast step never actually committed anything) never
//!   reaches any `observe_*` call: `RunLifecycleSink::emit` gates every
//!   lifecycle observation on the inner sink's `Result` being `Ok`, so a run
//!   never advances on evidence that was never actually journaled.
//!
//! The terminal edge lives here rather than in `registry::watch_settlement`
//! so it is durable *before* `SettlementSink` fires the settlement signal that
//! releases the run's concurrency slot: no other run can be authorized while
//! this one still reads non-terminal. See ADR-0023 for the mapping and for why
//! an unobservable exit is `lost` rather than a guessed `succeeded`/`failed`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crew_protocol::{EventEnvelope, ProjectId, RunId, RunState};
use serde_json::json;
use tokio::sync::broadcast;

use crate::db::DatabaseHandle;
use crate::domain::{DomainError, DomainRepository, embed_envelope, take_envelope};
use crate::service::query::run_state_op;

use super::AdapterFuture;
use super::event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink};

/// The evidence-driven lifecycle edges for one run. Each run's
/// [`RunLifecycleSink`] owns exactly one of these.
pub(crate) struct RunLifecycle {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    events_tx: broadcast::Sender<EventEnvelope>,
    run_id: RunId,
}

impl RunLifecycle {
    /// Reads the run's currently-stored state. Returns `None` (with a
    /// warning) when the read fails or the stored value is not a known state --
    /// callers treat `None` as "cannot act on this event", never as a state.
    async fn current(&self) -> Option<RunState> {
        let value = match self.db.run_domain_op(run_state_op(self.run_id)).await {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    run_id = %self.run_id,
                    "failed to read run state for a lifecycle edge"
                );
                return None;
            }
        };
        let Some(stored) = value.get("state").and_then(serde_json::Value::as_str) else {
            tracing::warn!(run_id = %self.run_id, "run-state read returned no state");
            return None;
        };
        match RunState::try_from(stored) {
            Ok(state) => Some(state),
            Err(err) => {
                tracing::warn!(run_id = %self.run_id, %err, "stored run state is not a known state");
                None
            }
        }
    }

    /// Commits one `transition_run` edge, embedding its `EventEnvelope` in the
    /// domain result and broadcasting it to live `events/subscribe` listeners --
    /// the same commit-then-broadcast sequence every `OrchestrationService`
    /// mutation uses (a mutation that appends without broadcasting silently
    /// breaks the monitor).
    async fn commit(&self, to: &RunState) -> Result<(), DomainError> {
        let project_id = self.project_id;
        let run_id = self.run_id;
        let to_owned = to.clone();
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.transition_run(run_id, &to_owned, None)
                    .map(|committed| {
                        embed_envelope(
                            json!({ "sequence": committed.sequence }),
                            &committed.envelope,
                        )
                    })
            }))
            .await?;
        if let Some(envelope) = take_envelope(&mut result) {
            let _ = self.events_tx.send(envelope);
        }
        Ok(())
    }

    /// Commits the legal hops from the run's current state toward `target` --
    /// at most three, since `queued -> starting -> working -> terminal` is the
    /// longest legal path. Stops on a terminal state (a terminal state always
    /// wins) and gives up with a warning when the state cannot be read or no
    /// hop is legal.
    async fn walk_to(&self, target: &RunState) {
        for _ in 0..3 {
            let Some(current) = self.current().await else {
                return;
            };
            if current == *target || current.is_terminal() {
                return;
            }
            let Some(next) = next_hop(&current, target) else {
                tracing::warn!(
                    run_id = %self.run_id,
                    from = %current,
                    to = %target,
                    "no legal run-state hop toward the target; giving up"
                );
                return;
            };
            if let Err(err) = self.commit(&next).await {
                tracing::warn!(
                    error = %err,
                    run_id = %self.run_id,
                    from = %current,
                    to = %next,
                    "failed to commit run-state edge"
                );
                return;
            }
        }
        tracing::warn!(
            run_id = %self.run_id,
            to = %target,
            "run-state walk exhausted without reaching the target"
        );
    }

    /// `ProcessStarted` evidence: the vendor process is up. Only a run still
    /// in `queued` moves -- `queued -> starting` is the one legal edge here,
    /// and the one that stamps `runs.started_at`.
    pub(crate) async fn observe_process_started(&self) {
        let Some(current) = self.current().await else {
            return;
        };
        if current == state("queued")
            && let Err(err) = self.commit(&state("starting")).await
        {
            tracing::warn!(
                error = %err,
                run_id = %self.run_id,
                from = %current,
                to = "starting",
                "failed to move a queued run to starting"
            );
        }
    }

    /// Any non-exit payload (a vendor session, message, or tool activity): the
    /// run is doing work, provided it had not already moved past `working`.
    /// Returns `false` only when the current state could not be read, so the
    /// caller re-asks on the next event instead of giving up on a transient
    /// database error.
    pub(crate) async fn observe_vendor_activity(&self) -> bool {
        let Some(current) = self.current().await else {
            return false;
        };
        match current.to_string().as_str() {
            "queued" | "starting" => {
                self.walk_to(&state("working")).await;
                true
            }
            // At-or-past `working` (`waitingUser`, `waitingPeer`, `paused`) or
            // terminal: vendor output must never clobber those states.
            _ => true,
        }
    }

    /// `ProcessExited` evidence: the vendor process is gone, so the run
    /// terminalizes. The walk keeps the hop-by-hop edges legal without
    /// touching the protocol's transition table (`queued -> lost` and
    /// `waitingUser -> succeeded` are not direct edges).
    pub(crate) async fn observe_process_exited(
        &self,
        exit_code: Option<i32>,
        signal: Option<&str>,
    ) {
        self.walk_to(&terminal_state_for(exit_code, signal)).await;
    }
}

/// Wraps a run's [`AdapterEventSink`] so the run's journaled evidence also
/// drives its durable [`RunState`].
pub struct RunLifecycleSink {
    inner: Arc<dyn AdapterEventSink>,
    lifecycle: RunLifecycle,
    /// Set once `observe_vendor_activity` has acted (or the run is already
    /// past `working`): a chatty run then pays no state read for the rest of
    /// its lifetime.
    working_observed: AtomicBool,
}

impl RunLifecycleSink {
    /// Wraps `inner` so this run's journaled evidence also drives its
    /// durable `RunState`.
    #[must_use]
    pub fn wrap(
        inner: Arc<dyn AdapterEventSink>,
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
        run_id: RunId,
    ) -> Arc<dyn AdapterEventSink> {
        Arc::new(Self {
            inner,
            lifecycle: RunLifecycle {
                db,
                project_id,
                events_tx,
                run_id,
            },
            working_observed: AtomicBool::new(false),
        })
    }
}

impl AdapterEventSink for RunLifecycleSink {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
        // Match by reference: `event` is moved into the inner `emit`
        // below, so a by-value match would consume the payload before
        // the inner sink needs it.
        let exit = match &event.payload {
            AdapterEventPayload::ProcessExited { exit_code, signal } => {
                Some((*exit_code, signal.clone()))
            }
            _ => None,
        };
        let process_started = matches!(&event.payload, AdapterEventPayload::ProcessStarted { .. });
        Box::pin(async move {
            let result = self.inner.emit(event).await;
            if result.is_ok() {
                if let Some((exit_code, signal)) = exit {
                    self.lifecycle
                        .observe_process_exited(exit_code, signal.as_deref())
                        .await;
                } else if process_started {
                    self.lifecycle.observe_process_started().await;
                } else if !self.working_observed.load(Ordering::Relaxed)
                    && self.lifecycle.observe_vendor_activity().await
                {
                    self.working_observed.store(true, Ordering::Relaxed);
                }
            }
            result
        })
    }
}

/// The one legal hop from `from` toward `target`: the target itself when the
/// edge is legal, otherwise the intermediate state the lifecycle table forces
/// (runs always pass through `starting`, then `working`, before a terminal
/// state).
fn next_hop(from: &RunState, target: &RunState) -> Option<RunState> {
    if from.can_transition_to(target) {
        return Some(target.clone());
    }
    match from.to_string().as_str() {
        "queued" => Some(state("starting")),
        "starting" | "waitingUser" | "waitingPeer" | "paused" => Some(state("working")),
        // Terminal (or unknown) states have no outgoing edges.
        _ => None,
    }
}

/// The terminal state an exit status is evidence of. A signalled death is
/// `failed` even when a code is present (the code is not trustworthy once a
/// signal is); a clean zero exit is `succeeded`; any other code is `failed`;
/// and an exit whose status the supervisor could not observe at all is
/// `lost` -- ADR-0023 names the uncertainty rather than guessing.
fn terminal_state_for(exit_code: Option<i32>, signal: Option<&str>) -> RunState {
    if signal.is_some() {
        return state("failed");
    }
    match exit_code {
        Some(0) => state("succeeded"),
        Some(_) => state("failed"),
        None => state("lost"),
    }
}

/// Constructs a [`RunState`] from one of the protocol's own table literals;
/// none of these can fail to parse.
fn state(name: &str) -> RunState {
    RunState::try_from(name).expect("run-state literal from the protocol's own table")
}
#[cfg(test)]
mod tests {
    use crew_protocol::{
        Classified, ContentClass, ProjectId, Run, RunFlags, RunId, RuntimeEvent, RuntimeEventKind,
        TaskId, TaskRef, Timestamp, Worker, WorkerId, WorkerProfileRef,
    };
    use tempfile::TempDir;

    use crate::adapter::AdapterError;

    use super::*;

    /// The inner sink these tests wrap: accepts every event, journals
    /// nothing, and resolves with the fixed sequence `0` -- so every state
    /// change these tests observe is the work of the lifecycle edge itself.
    struct StubSink;
    impl AdapterEventSink for StubSink {
        fn emit(&self, _event: AdapterEvent) -> AdapterFuture<'_, u64> {
            Box::pin(async { Ok(0) })
        }
    }

    /// The inner sink `a_failed_inner_emit_never_applies_a_lifecycle_edge`
    /// wraps: every event fails as if the sanitize/journal/broadcast step
    /// never actually committed anything, so no evidence was ever durable.
    struct FailingSink;

    impl AdapterEventSink for FailingSink {
        fn emit(&self, _event: AdapterEvent) -> AdapterFuture<'_, u64> {
            Box::pin(async {
                Err(AdapterError::process(
                    "stub",
                    "emit",
                    "journal write failed",
                ))
            })
        }
    }

    /// A real, migrated database on a throwaway file: the same pattern
    /// `registry.rs`'s settlement tests use (per-test `TempDir`, explicit
    /// `shutdown` so the actor thread never outlives the test).
    async fn open_db() -> (TempDir, Arc<DatabaseHandle>) {
        let dir = tempfile::Builder::new()
            .prefix("bat-run-lifecycle-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            DatabaseHandle::start(db_path)
                .await
                .expect("start database"),
        );
        (dir, db)
    }

    /// Seeds one task + worker + `queued` run through the real
    /// `DomainRepository` API (copied from `tests/recovery.rs`'s
    /// `seed_run`), returning the identifiers the tests then drive.
    async fn seed_run(db: &DatabaseHandle, project_id: ProjectId) -> (TaskId, WorkerId, RunId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &TaskRef {
                    owner_client_instance_id: "omp-1".to_string(),
                    revision: 1,
                },
            )?;
            let worker = Worker {
                worker_id,
                profile_ref: WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".to_string(),
                    adapter: "fake".to_string(),
                    model: "test".to_string(),
                    permission_envelope: serde_json::json!({}),
                },
                parent_worker_id: None,
                created_at: Timestamp::now(),
            };
            repo.create_worker(&worker)?;
            let run = Run {
                run_id,
                task_id,
                worker_id,
                state: RunState::try_from("queued").expect("queued is a valid state"),
                flags: RunFlags::default(),
                vendor_session_id: None,
                started_at: None,
                completed_at: None,
            };
            repo.submit_run(&run, None, None)?;
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed run");
        (task_id, worker_id, run_id)
    }

    /// Drives `run_id` through the legal edges from `queued` up to `target`,
    /// directly through `DomainRepository` (bypassing the sink on purpose:
    /// the tests pin what the sink does from a given starting state).
    async fn drive_to_state(
        db: &DatabaseHandle,
        project_id: ProjectId,
        run_id: RunId,
        target: &str,
    ) {
        let path: &[&str] = match target {
            "working" => &["starting", "working"],
            "waitingUser" => &["starting", "working", "waitingUser"],
            "cancelled" => &["cancelled"],
            other => panic!("no drive path defined for {other}"),
        };
        for state in path {
            let to = RunState::try_from(*state).expect("valid state");
            db.run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.transition_run(run_id, &to, None)
                    .map(|_| serde_json::json!({}))
            }))
            .await
            .unwrap_or_else(|err| panic!("drive to {state} failed: {err}"));
        }
    }

    /// Reads a run's current projected state.
    async fn run_state(db: &DatabaseHandle, run_id: RunId) -> String {
        db.run_domain_op(Box::new(move |conn| {
            let state: String = conn.query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            Ok(serde_json::json!(state))
        }))
        .await
        .expect("read run state")
        .as_str()
        .expect("state is a string")
        .to_string()
    }

    /// Every journaled run-state event for `run_id`, in sequence order: the
    /// `state` each `RunEvent` recorded, so the exact walk the sink committed
    /// is readable back out of the durable journal.
    async fn run_states(db: &DatabaseHandle, run_id: RunId) -> Vec<String> {
        let raw: Vec<String> = db
            .run_domain_op(Box::new(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
                let rows: Vec<String> = stmt
                    .query_map([run_id.to_string()], |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok(serde_json::json!(rows))
            }))
            .await
            .expect("read journaled events")
            .as_array()
            .expect("rows are an array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        raw.into_iter()
            .filter_map(|raw| {
                let event: RuntimeEvent =
                    serde_json::from_str(&raw).expect("parse a journaled event");
                match event {
                    RuntimeEvent::RunEvent { state, .. } => Some(state),
                    _ => None,
                }
            })
            .collect()
    }

    /// Whether `runs.started_at` / `runs.completed_at` is set.
    async fn run_timestamp_set(db: &DatabaseHandle, run_id: RunId, column: &'static str) -> bool {
        let value = db
            .run_domain_op(Box::new(move |conn| {
                let timestamp: Option<String> = conn
                    .query_row(
                        &format!("SELECT {column} FROM runs WHERE run_id = ?1"),
                        [run_id.to_string()],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                Ok(serde_json::json!(timestamp.is_some()))
            }))
            .await
            .expect("read run timestamp");
        value.as_bool().unwrap_or(false)
    }

    #[tokio::test]
    async fn process_started_moves_a_queued_run_to_starting() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessStarted { pid: 1234 },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "starting");
        assert_eq!(
            run_states(&db, run_id).await,
            vec!["queued".to_string(), "starting".to_string()]
        );
        assert!(
            run_timestamp_set(&db, run_id, "started_at").await,
            "the starting edge stamps runs.started_at"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn the_first_vendor_event_walks_a_queued_run_through_starting_into_working() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, mut rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::VendorSessionEstablished {
                vendor_session_id: "vs-1".to_string(),
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "working");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string()
            ]
        );

        // The event-broadcast invariant: every edge the walk committed is
        // broadcast, in commit order, before `emit` resolves.
        let first = rx.recv().await.expect("first broadcast envelope");
        match &first.event {
            RuntimeEvent::RunEvent { kind, state, .. } => {
                assert_eq!(*kind, RuntimeEventKind::RunStarting);
                assert_eq!(state.as_str(), "starting");
            }
            other => panic!("expected the starting edge, got {other:?}"),
        }
        let second = rx.recv().await.expect("second broadcast envelope");
        match &second.event {
            RuntimeEvent::RunEvent { kind, state, .. } => {
                assert_eq!(*kind, RuntimeEventKind::RunWorking);
                assert_eq!(state.as_str(), "working");
            }
            other => panic!("expected the working edge, got {other:?}"),
        }
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_zero_exit_settles_the_run_as_succeeded() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "succeeded");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "succeeded".to_string()
            ]
        );
        assert!(
            run_timestamp_set(&db, run_id, "completed_at").await,
            "a terminal edge stamps runs.completed_at"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_nonzero_exit_settles_the_run_as_failed() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(7),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "failed");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "failed".to_string()
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_signalled_exit_settles_the_run_as_failed() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: None,
                signal: Some("SIGKILL".to_string()),
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "failed");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "failed".to_string()
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn an_exit_with_no_observable_status_settles_the_run_as_lost() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: None,
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        // `queued -> lost` is illegal, so the walk proves its shape by
        // committing the forced intermediate hop first.
        assert_eq!(run_state(&db, run_id).await, "lost");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "lost".to_string()
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn an_exit_after_cancellation_leaves_the_cancelled_run_untouched() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "cancelled").await;
        let before = run_states(&db, run_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "cancelled");
        assert_eq!(
            run_states(&db, run_id).await,
            before,
            "a terminal state always wins: no further RunEvent may be appended"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn an_exit_while_waiting_on_a_user_routes_through_working_to_succeeded() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "waitingUser").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        // `waitingUser -> succeeded` is illegal, so the walk commits the
        // forced intermediate hop before the terminal edge.
        assert_eq!(run_state(&db, run_id).await, "succeeded");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "waitingUser".to_string(),
                "working".to_string(),
                "succeeded".to_string()
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn vendor_output_never_reopens_working_on_a_run_that_started_waiting() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "waitingUser").await;
        let before = run_states(&db, run_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink =
            RunLifecycleSink::wrap(Arc::new(StubSink), Arc::clone(&db), project_id, tx, run_id);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::MessageFinal {
                role: "assistant".to_string(),
                text: Classified {
                    class: ContentClass::Visible,
                    value: "all done".to_string(),
                },
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(
            run_state(&db, run_id).await,
            "waitingUser",
            "vendor output must never clobber an approval wait"
        );
        assert_eq!(
            run_states(&db, run_id).await,
            before,
            "no RunEvent may be appended for output that is at-or-past working"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_failed_inner_emit_never_applies_a_lifecycle_edge() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(FailingSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
        );

        let err = sink
            .emit(AdapterEvent {
                run_id,
                task_id,
                worker_id,
                payload: AdapterEventPayload::ProcessStarted { pid: 1234 },
                cursor: None,
            })
            .await
            .expect_err("the inner sink's failure must propagate");
        assert_eq!(
            err.to_string(),
            "adapter stub operation emit failed (process): journal write failed"
        );

        // No edge without durable evidence: a `ProcessStarted` whose journal
        // write never actually committed must never move the run, because
        // nothing durable backs the `starting` edge it would otherwise apply.
        assert_eq!(
            run_state(&db, run_id).await,
            "queued",
            "a run must not advance on evidence its own sink failed to journal"
        );
        assert_eq!(
            run_states(&db, run_id).await,
            vec!["queued".to_string()],
            "no RunEvent may be appended for an emit the inner sink rejected"
        );
        db.shutdown().await.expect("shutdown database");
    }
}
