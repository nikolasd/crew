//! Regression tests for R54: `ViolationService::decide` must not admit two
//! concurrent, contradictory decisions for the same policy violation.
//!
//! `DatabaseHandle::run_domain_op` (`crates/runtime/src/db/actor.rs`) sends a
//! whole boxed closure to a single-owner `std::thread` over a *bounded*
//! `tokio::sync::mpsc` channel (capacity 32) and awaits a `oneshot` reply.
//! That actor thread is a strictly FIFO single consumer: it never
//! interleaves the *inside* of two closures, only whole closures with each
//! other. A service method that spans more than one `run_domain_op` round
//! trip is therefore not a transaction -- a decision made from the result
//! of an earlier round trip can be stale by the time a later one runs.
//!
//! The first two tests below drive two `decide` calls through
//! `tokio::join!(biased; ...)` in a single task, never `tokio::spawn`.
//! Plain (non-`biased`) `join!` rotates which branch it polls first on
//! every poll of the combined future -- a fairness mechanism documented on
//! the macro itself -- so it does *not* guarantee argument order beyond
//! the very first poll; an earlier version of this file wrongly assumed it
//! did. `biased;` pins polling to declaration order on every poll, so the
//! first-declared future always enqueues its next `run_domain_op` command
//! before the second is even polled, which makes the actor's enqueue --
//! and thus processing -- order deterministic and reproducible across
//! runs: the first-declared `decide` call always reaches the guarded
//! write before the second.
//!
//! The underlying invariant these two tests defend does not actually
//! depend on `biased`: because both calls share one task and the actor is
//! a strictly FIFO single consumer, their `run_domain_op` sends can never
//! be simultaneous or unordered from the actor's point of view, whichever
//! call happens to be enqueued first -- so the guarded
//! `UPDATE ... WHERE resolution IS NULL` in `resolve_policy_violation`
//! always admits exactly one writer. `biased` is used to make *which* call
//! wins reproducible and easy to reason about, not to make "exactly one
//! wins" true -- that already follows from the guard plus the FIFO actor.
//! `tokio::spawn` would remove even the ordering `biased` gives: each
//! `decide` call would run on its own independently scheduled task, free
//! to enqueue its commands in whatever order the executor picks. Every
//! assertion in these two tests still derives its expectation from
//! whichever call actually returned `Ok`, rather than assuming which one
//! wins, as a second line of defense. Checked empirically too: run 20x
//! with `--exact`, no flakes observed, both before and after `biased` was
//! added.
//!
//! The fourth test, `releasing_a_violation_whose_run_has_already_settled_is_refused`,
//! deliberately does *not* join! `decide` against the run-settling
//! transition, even though that would look like the more direct test of
//! "the run settles mid-decide". An adversarial review of an earlier draft
//! found a residual gap in that shape: `decide`'s first round trip (the
//! snapshot) could in principle have its actor reply arrive so fast that
//! the *entire* `decide` future resolves inside one poll, before the
//! run-settling future is ever touched -- vanishingly unlikely (the window
//! is a handful of CPU instructions against real SQLite work measured in
//! microseconds, and it did not reproduce in dozens of runs), but a real
//! timing dependency rather than a scheduling guarantee, and the review
//! was right to reject resting a determinism claim on it. This test
//! instead settles the run first, sequentially, then calls `decide` --
//! zero timing dependency, and it still proves exactly the thing R54
//! changed: `PolicyViolationSnapshot` no longer carries `run_state`, so
//! the guard's own live read of `runs.state` inside
//! `resolve_policy_violation`'s transaction is the *only* thing left that
//! can refuse this release, and this test shows that read is correct on
//! its own terms.

use std::sync::Arc;

use crew_protocol::{
    PolicyViolationId, ProjectId, Run, RunFlags, RunId, RunState, TaskId, TaskRef, Timestamp,
    Worker, WorkerId, WorkerProfileRef,
};
use crew_runtime::config::NestedViolationAction;
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::{DomainRepository, RunFlag};
use crew_runtime::policy::{DecideOutcome, ViolationError, ViolationService};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::broadcast;

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

/// Seeds one task/worker/run, drives the run to `working`, quarantines it,
/// and records one unresolved nested-worker violation against it. Returns
/// the ids a test needs to decide and probe.
async fn seed_quarantined_violation(
    db: &DatabaseHandle,
    project_id: ProjectId,
) -> (PolicyViolationId, RunId, TaskId, WorkerId) {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = RunId::new();

    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.upsert_task(
            task_id,
            &TaskRef {
                owner_client_instance_id: "omp-1".into(),
                revision: 1,
            },
        )?;
        let worker = Worker {
            worker_id,
            profile_ref: WorkerProfileRef {
                id: worker_id,
                fingerprint: "sha256:fake".into(),
                adapter: "fake".into(),
                model: "test".into(),
                permission_envelope: json!({}),
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
        Ok(json!({}))
    }))
    .await
    .expect("seed task/worker/run");

    for state in ["starting", "working"] {
        let to = RunState::try_from(state).expect("valid state");
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.transition_run(run_id, &to, None).map(|_| json!({}))
        }))
        .await
        .unwrap_or_else(|e| panic!("drive to {state} failed: {e}"));
    }

    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.set_run_flag(run_id, RunFlag::PolicyQuarantined, true)
            .map(|_| json!({}))
    }))
    .await
    .expect("quarantine the run");

    let violation_id = PolicyViolationId::new();
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.record_policy_violation(
            violation_id,
            run_id,
            task_id,
            worker_id,
            "nested_worker_denied",
            7,
            "sha256:fp",
            Some("child-1"),
            Some("parent-1"),
            "quarantine",
        )
        .map(|_| json!({}))
    }))
    .await
    .expect("record the violation");

    (violation_id, run_id, task_id, worker_id)
}

/// A `ViolationService` with no adapter driver: the cancel path only
/// transitions the run, so the run's projected state is the observable
/// that proves whether the cancel side effect fired.
fn service(db: Arc<DatabaseHandle>, project_id: ProjectId) -> ViolationService {
    ViolationService::new(
        db,
        project_id,
        broadcast::channel(64).0,
        None,
        NestedViolationAction::Quarantine,
    )
}

/// A single `run_domain_op` transitioning `run_id` straight to `cancelled`
/// -- the same edge `ViolationService::cancel_and_transition` uses in
/// production -- to simulate the run settling out from under a concurrent
/// `decide("release")`.
async fn cancel_the_run(db: &DatabaseHandle, project_id: ProjectId, run_id: RunId) {
    let to = RunState::try_from("cancelled").expect("cancelled is a valid state");
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.transition_run(run_id, &to, None).map(|_| json!({}))
    }))
    .await
    .expect("cancel the run");
}

async fn violation_resolution(
    db: &DatabaseHandle,
    violation_id: PolicyViolationId,
) -> Option<String> {
    db.run_domain_op(Box::new(move |conn| {
        let resolution: Option<String> = conn.query_row(
            "SELECT resolution FROM policy_violations WHERE violation_id = ?1",
            [violation_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(json!(resolution))
    }))
    .await
    .expect("read violation resolution")
    .as_str()
    .map(str::to_string)
}

async fn run_state(db: &DatabaseHandle, run_id: RunId) -> String {
    db.run_domain_op(Box::new(move |conn| {
        let state: String = conn.query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            [run_id.to_string()],
            |r| r.get(0),
        )?;
        Ok(json!(state))
    }))
    .await
    .expect("read run state")
    .as_str()
    .expect("state is a string")
    .to_string()
}

async fn run_quarantined(db: &DatabaseHandle, run_id: RunId) -> bool {
    db.run_domain_op(Box::new(move |conn| {
        let quarantined: i64 = conn.query_row(
            "SELECT flags_policy_quarantined FROM runs WHERE run_id = ?1",
            [run_id.to_string()],
            |r| r.get(0),
        )?;
        Ok(json!(quarantined))
    }))
    .await
    .expect("read quarantine flag")
    .as_i64()
    .expect("flag is an integer")
        != 0
}

async fn decided_event_count(db: &DatabaseHandle) -> i64 {
    db.run_domain_op(Box::new(|conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_json LIKE '%policyViolationDecided%'",
            [],
            |r| r.get(0),
        )?;
        Ok(json!(count))
    }))
    .await
    .expect("count decided events")
    .as_i64()
    .expect("count is an integer")
}

#[tokio::test]
async fn concurrent_release_and_cancel_admit_exactly_one_decision() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (violation_id, run_id, ..) = seed_quarantined_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    let (release, cancel) = tokio::join!(
        biased;
        svc.decide(violation_id, "omp-1", "release"),
        svc.decide(violation_id, "omp-1", "cancel"),
    );

    // Exactly one call decided the violation; the other was refused as a
    // conflicting concurrent decision.
    let decided = [
        matches!(release, Ok(DecideOutcome::Decided)),
        matches!(cancel, Ok(DecideOutcome::Decided)),
    ];
    assert_eq!(
        decided.iter().filter(|d| **d).count(),
        1,
        "exactly one of release/cancel must be the decision: release={release:?} cancel={cancel:?}"
    );
    let conflicted = [
        matches!(release, Err(ViolationError::Conflict { .. })),
        matches!(cancel, Err(ViolationError::Conflict { .. })),
    ];
    assert_eq!(
        conflicted.iter().filter(|c| **c).count(),
        1,
        "the losing call must see Conflict: release={release:?} cancel={cancel:?}"
    );
    assert_eq!(
        decided_event_count(&db).await,
        1,
        "exactly one PolicyViolationDecided event must be journaled, never two"
    );

    // Only the winner's side effect is visible -- derived from which call
    // actually won, so this assertion does not depend on join! argument
    // order (though the analysis above shows release always wins here).
    if release.is_ok() {
        assert_eq!(
            violation_resolution(&db, violation_id).await,
            Some("release".to_string())
        );
        assert_eq!(run_state(&db, run_id).await, "working");
        assert!(
            !run_quarantined(&db, run_id).await,
            "release must clear quarantine"
        );
    } else {
        assert_eq!(
            violation_resolution(&db, violation_id).await,
            Some("cancel".to_string())
        );
        assert_eq!(run_state(&db, run_id).await, "cancelled");
        assert!(
            run_quarantined(&db, run_id).await,
            "cancel must not touch quarantine"
        );
    }
}

#[tokio::test]
async fn concurrent_identical_releases_journal_one_event_and_report_already_decided() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (violation_id, run_id, ..) = seed_quarantined_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    let (first, second) = tokio::join!(
        biased;
        svc.decide(violation_id, "omp-1", "release"),
        svc.decide(violation_id, "omp-1", "release"),
    );

    let outcomes = [
        first.expect("first release call must succeed"),
        second.expect("second release call must succeed"),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == DecideOutcome::Decided)
            .count(),
        1,
        "exactly one call must be the new decision: {outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == DecideOutcome::AlreadyDecided)
            .count(),
        1,
        "the other call must observe an idempotent replay: {outcomes:?}"
    );
    assert_eq!(
        decided_event_count(&db).await,
        1,
        "an idempotent replay must not journal a second event"
    );
    assert_eq!(
        violation_resolution(&db, violation_id).await,
        Some("release".to_string())
    );
    assert!(!run_quarantined(&db, run_id).await);
    assert_eq!(run_state(&db, run_id).await, "working");
}

#[tokio::test]
async fn deciding_the_same_resolution_twice_sequentially_stays_idempotent() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (violation_id, run_id, ..) = seed_quarantined_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    let first = svc.decide(violation_id, "omp-1", "release").await;
    let second = svc.decide(violation_id, "omp-1", "release").await;

    assert!(
        matches!(first, Ok(DecideOutcome::Decided)),
        "the first decide must be the new decision: {first:?}"
    );
    assert!(
        matches!(second, Ok(DecideOutcome::AlreadyDecided)),
        "the sequential replay must be idempotent, not an error: {second:?}"
    );
    assert_eq!(
        decided_event_count(&db).await,
        1,
        "a sequential replay must not journal a second event"
    );
    assert!(!run_quarantined(&db, run_id).await);
}

#[tokio::test]
async fn releasing_a_violation_whose_run_has_already_settled_is_refused() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (violation_id, run_id, ..) = seed_quarantined_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    // Settle the run out from under the violation *before* calling decide --
    // sequentially, with no join! and no dependence on actor-reply timing.
    // `PolicyViolationSnapshot` no longer carries `run_state` (R54): the
    // only thing that can still catch this is the guard's own live read of
    // `runs.state` inside `resolve_policy_violation`'s transaction. This
    // proves that read is correct on its own terms, deterministically,
    // rather than relying on a race actually landing mid-decide.
    cancel_the_run(&db, project_id, run_id).await;

    let release = svc.decide(violation_id, "omp-1", "release").await;

    assert!(
        matches!(release, Err(ViolationError::RunSettled { .. })),
        "a release on an already-settled run must be refused: {release:?}"
    );
    assert_eq!(
        violation_resolution(&db, violation_id).await,
        None,
        "the guard must roll back the UPDATE together with the appended event"
    );
    assert_eq!(
        decided_event_count(&db).await,
        0,
        "no PolicyViolationDecided event may survive a refused release"
    );
    assert_eq!(run_state(&db, run_id).await, "cancelled");
    assert!(
        run_quarantined(&db, run_id).await,
        "quarantine must still be set -- release was never applied"
    );
}
