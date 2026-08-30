//! Regression tests for R70: `ApprovalService::decide` must not admit two
//! concurrent decisions for the same approval.
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
//! the very first poll; an earlier version of the analogous R54 file
//! (`policy_violation.rs`) wrongly assumed it did. `biased;` pins polling
//! to declaration order on every poll, so the first-declared future always
//! enqueues its next `run_domain_op` command before the second is even
//! polled, which makes the actor's enqueue -- and thus processing -- order
//! deterministic and reproducible across runs: the first-declared `decide`
//! call always reaches the guarded write before the second.
//!
//! The underlying invariant these two tests defend does not actually depend
//! on `biased`: because both calls share one task and the actor is a
//! strictly FIFO single consumer, their `run_domain_op` sends can never be
//! simultaneous or unordered from the actor's point of view, whichever call
//! happens to be enqueued first -- so the guarded
//! `UPDATE ... WHERE decision IS NULL` in `decide_approval` always admits
//! exactly one writer. `biased` is used to make *which* call wins
//! reproducible and easy to reason about, not to make "exactly one wins"
//! true -- that already follows from the guard plus the FIFO actor.
//! `tokio::spawn` would remove even the ordering `biased` gives: each
//! `decide` call would run on its own independently scheduled task, free to
//! enqueue its commands in whatever order the executor picks. Every
//! assertion in these two tests still derives its expectation from whichever
//! call actually returned `Ok`, rather than assuming which one wins, as a
//! second line of defense.
//!
//! Only `humanRequired` is still checked caller-side by `decide` -- it
//! reads a field a decision write never mutates, so it cannot go stale.
//! Task ownership used to be a second caller-side pre-check here, but it
//! moved into `decide_approval`'s guarded write for R71 (see
//! `approval_owner_race.rs`), because a `reconcile/omp` rebind landing
//! between a caller-side snapshot read and the write could otherwise leave
//! a stale owner's decision with nothing left to refuse it. That move is
//! not part of what these two tests race: the seeded approval is
//! `humanRequired: true` and both racers identify as `"omp-1"`, which owns
//! the task, so the ownership guard admits both racers and the decision
//! guard -- the `UPDATE ... WHERE decision IS NULL` these tests exist to
//! defend -- is what must arbitrate between them.
//!
//! The fourth test, `deciding_an_approval_whose_run_has_already_settled_is_refused`,
//! deliberately does *not* join! `decide` against the run-settling
//! transition, even though that would look like the more direct test of
//! "the run settles mid-decide". An adversarial review of the analogous R54
//! test (`releasing_a_violation_whose_run_has_already_settled_is_refused`)
//! found a residual timing gap in that shape: `decide`'s first round trip
//! (the snapshot) could in principle have its actor reply arrive so fast that
//! the *entire* `decide` future resolves inside one poll, before the
//! run-settling future is ever touched -- vanishingly unlikely (the window
//! is a handful of CPU instructions against real SQLite work measured in
//! microseconds), but a real timing dependency rather than a scheduling
//! guarantee. This test instead settles the run first, sequentially, then
//! calls `decide` -- zero timing dependency, and it still proves exactly the
//! thing R70 changed: `ApprovalSnapshot` no longer carries `run_state` (or
//! `decision`), so the guard's own live read of `runs.state` inside
//! `decide_approval`'s transaction is the *only* thing left that can refuse
//! this decision, and this test shows that read is correct on its own terms.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crew_protocol::{
    ApprovalId, ApprovalRequest, DecidedBy, ProjectId, Run, RunFlags, RunId, RunState, TaskId,
    TaskRef, Timestamp, Worker, WorkerId, WorkerProfileRef,
};
use crew_runtime::approval::{
    ApprovalCallback, ApprovalError, ApprovalService, CallbackFuture, DecideOutcome,
};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::DomainRepository;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::broadcast;

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

/// An [`ApprovalCallback`] that counts invocations and always succeeds, for
/// asserting that a losing (or refused) decision never reaches the adapter.
struct CountingCallback {
    calls: Arc<AtomicU32>,
}

impl ApprovalCallback for CountingCallback {
    fn acknowledge(&self, _approval_id: ApprovalId, _decision: &str) -> CallbackFuture<'static> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// Seeds one task/worker/run, drives the run to `working`, and creates one
/// pending, human-required approval against it. `create_approval` leaves the
/// run in `waitingUser`, the state a decision targets. Returns the ids a
/// test needs to decide and probe.
async fn seed_pending_approval(
    db: &DatabaseHandle,
    project_id: ProjectId,
) -> (ApprovalId, RunId, TaskId) {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = RunId::new();
    let approval_id = ApprovalId::new();

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

    let request = ApprovalRequest {
        approval_id,
        run_id,
        task_id,
        action: "write file".into(),
        arguments: json!({ "path": "/tmp/x" }),
        human_required: true,
        policy_reason: "write requires human approval".into(),
        created_at: Timestamp::now(),
        decided_at: None,
        decision: None,
        decided_by: None,
        reason: None,
    };
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.create_approval(&request).map(|_| json!({}))
    }))
    .await
    .expect("create the approval");

    (approval_id, run_id, task_id)
}

/// An [`ApprovalService`] wired to the given callback; the broadcast sender
/// has no subscribers, which is the production shape for an unattached
/// console.
fn service(
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    callback: Arc<dyn ApprovalCallback>,
) -> ApprovalService {
    ApprovalService::new(db, project_id, callback, broadcast::channel(64).0)
}

/// A single `run_domain_op` transitioning `run_id` straight to `failed` --
/// the same direct force-fail `tests/approval.rs` uses -- to settle the run
/// out from under a pending approval.
async fn fail_the_run(db: &DatabaseHandle, project_id: ProjectId, run_id: RunId) {
    let to = RunState::try_from("failed").expect("failed is a valid state");
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.transition_run(run_id, &to, None).map(|_| json!({}))
    }))
    .await
    .expect("fail the run");
}

async fn approval_decision(db: &DatabaseHandle, approval_id: ApprovalId) -> Option<String> {
    db.run_domain_op(Box::new(move |conn| {
        let decision: Option<String> = conn.query_row(
            "SELECT decision FROM approvals WHERE approval_id = ?1",
            [approval_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(json!(decision))
    }))
    .await
    .expect("read approval decision")
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

async fn decided_event_count(db: &DatabaseHandle) -> i64 {
    db.run_domain_op(Box::new(|conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_json LIKE '%approvalDecided%'",
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
async fn concurrent_approve_and_deny_admit_exactly_one_decision() {
    let no_reason = crew_protocol::Redacted::assert_runtime_authored("no");
    let ok_reason = crew_protocol::Redacted::assert_runtime_authored("ok");
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, ..) = seed_pending_approval(&db, project_id).await;
    let calls = Arc::new(AtomicU32::new(0));
    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        }),
    );

    let (approve, deny) = tokio::join!(
        biased;
        svc.decide(approval_id, "omp-1", "approve", &ok_reason, DecidedBy::Human),
        svc.decide(approval_id, "omp-1", "deny", &no_reason, DecidedBy::Human),
    );

    // Exactly one call decided the approval; the other was refused as a
    // conflicting concurrent decision.
    let decided = [
        matches!(approve, Ok(DecideOutcome::Decided)),
        matches!(deny, Ok(DecideOutcome::Decided)),
    ];
    assert_eq!(
        decided.iter().filter(|d| **d).count(),
        1,
        "exactly one of approve/deny must be the decision: approve={approve:?} deny={deny:?}"
    );
    let conflicted = [
        matches!(approve, Err(ApprovalError::Conflict { .. })),
        matches!(deny, Err(ApprovalError::Conflict { .. })),
    ];
    assert_eq!(
        conflicted.iter().filter(|c| **c).count(),
        1,
        "the losing call must see Conflict: approve={approve:?} deny={deny:?}"
    );
    assert_eq!(
        decided_event_count(&db).await,
        1,
        "exactly one approvalDecided event must be journaled, never two"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only the deciding call may reach the adapter callback"
    );

    // Only the winner's decision is on record -- derived from which call
    // actually won, so this assertion does not depend on join! argument
    // order (though the analysis above shows approve always wins here).
    if approve.is_ok() {
        assert_eq!(
            approval_decision(&db, approval_id).await,
            Some("approve".to_string())
        );
    } else {
        assert_eq!(
            approval_decision(&db, approval_id).await,
            Some("deny".to_string())
        );
    }
    assert_eq!(
        run_state(&db, run_id).await,
        "working",
        "the winner's successful callback must return the run to working"
    );
}

#[tokio::test]
async fn concurrent_identical_approvals_journal_one_event_and_invoke_the_callback_once() {
    let ok_reason = crew_protocol::Redacted::assert_runtime_authored("ok");
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, ..) = seed_pending_approval(&db, project_id).await;
    let calls = Arc::new(AtomicU32::new(0));
    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        }),
    );

    let (first, second) = tokio::join!(
        biased;
        svc.decide(approval_id, "omp-1", "approve", &ok_reason, DecidedBy::Human),
        svc.decide(approval_id, "omp-1", "approve", &ok_reason, DecidedBy::Human),
    );

    let outcomes = [
        first.expect("first approve call must succeed"),
        second.expect("second approve call must succeed"),
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
        calls.load(Ordering::SeqCst),
        1,
        "a losing identical replay must never re-invoke the adapter"
    );
    assert_eq!(
        approval_decision(&db, approval_id).await,
        Some("approve".to_string())
    );
    assert_eq!(run_state(&db, run_id).await, "working");
}

#[tokio::test]
async fn deciding_the_same_decision_twice_sequentially_stays_idempotent() {
    let ok_reason = crew_protocol::Redacted::assert_runtime_authored("ok");
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, ..) = seed_pending_approval(&db, project_id).await;
    let calls = Arc::new(AtomicU32::new(0));
    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        }),
    );

    let first = svc
        .decide(
            approval_id,
            "omp-1",
            "approve",
            &ok_reason,
            DecidedBy::Human,
        )
        .await;
    let second = svc
        .decide(
            approval_id,
            "omp-1",
            "approve",
            &ok_reason,
            DecidedBy::Human,
        )
        .await;

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
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a sequential replay must not re-invoke the adapter"
    );
    assert_eq!(run_state(&db, run_id).await, "working");
}

#[tokio::test]
async fn deciding_an_approval_whose_run_has_already_settled_is_refused() {
    let ok_reason = crew_protocol::Redacted::assert_runtime_authored("ok");
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, ..) = seed_pending_approval(&db, project_id).await;
    let calls = Arc::new(AtomicU32::new(0));
    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        }),
    );

    // Settle the run out from under the approval *before* calling decide --
    // sequentially, with no join! and no dependence on actor-reply timing.
    // `ApprovalSnapshot` no longer carries `run_state` (R70): the only thing
    // that can still catch this is the guard's own live read of `runs.state`
    // inside `decide_approval`'s transaction. This proves that read is
    // correct on its own terms, deterministically, rather than relying on a
    // race actually landing mid-decide.
    fail_the_run(&db, project_id, run_id).await;

    let outcome = svc
        .decide(
            approval_id,
            "omp-1",
            "approve",
            &ok_reason,
            DecidedBy::Human,
        )
        .await;

    assert!(
        matches!(outcome, Err(ApprovalError::RunSettled { .. })),
        "a decision on an already-settled run must be refused: {outcome:?}"
    );
    assert_eq!(
        approval_decision(&db, approval_id).await,
        None,
        "the guard must roll back the UPDATE together with the appended event"
    );
    assert_eq!(
        decided_event_count(&db).await,
        0,
        "no approvalDecided event may survive a refused decision"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a refused decision must never reach the adapter callback"
    );
    assert_eq!(run_state(&db, run_id).await, "failed");
}
