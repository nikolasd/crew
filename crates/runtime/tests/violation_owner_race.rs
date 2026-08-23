//! Regression tests for R72: `ViolationService::decide` used to check task
//! ownership as a caller-side pre-check against a snapshot loaded before
//! the guarded write, so a `reconcile/omp` ownership rebind landing in the
//! window between that snapshot and the write left a stale owner's
//! decision with nothing left to refuse it. This is the same shape R71
//! fixed on the approval path (see `approval_owner_race.rs`'s header for
//! the full actor-FIFO argument).
//!
//! Summarized here only as far as this file's construction needs it:
//! `DatabaseHandle::run_domain_op` sends whole boxed closures to a
//! single-owner thread over a FIFO channel, one `oneshot` reply per
//! command. The actor never interleaves the *inside* of two closures, only
//! whole closures with each other, in the order their commands were
//! enqueued.
//!
//! Before the fix, `ViolationService::decide` was *two* round trips: a
//! snapshot read (`policy_violation_snapshot`, which included
//! `tasks.owner_client_instance_id` for the caller-side ownership
//! pre-check), then, once that pre-check passed synchronously,
//! `resolve_policy_violation` (the guarded write, which never re-checked
//! ownership). `DomainRepository::reconcile_ownership` -- the same method
//! `reconcile/omp` calls -- is *one* round trip: an unguarded
//! `UPDATE tasks SET owner_client_instance_id = ...`.
//!
//! The first test below drives `svc.decide` (as the *original* owner) and
//! a direct call to `reconcile_ownership` (rebinding to a *new* owner)
//! through `tokio::join!(biased; ...)`, `decide` declared first, to pin a
//! reproducible interleaving: `biased` polls `decide` before the rebind on
//! every poll, so `decide`'s snapshot round trip was always enqueued --
//! and thus processed -- before the rebind's `UPDATE`, and the rebind
//! always committed before `decide`'s second command
//! (`resolve_policy_violation`) could possibly be sent (that second
//! `send()` cannot occur before the snapshot's reply wakes `decide` for
//! another poll, which cannot happen before the rebind's synchronous
//! first-poll `send()`). Against the pre-fix code, that pinned
//! interleaving is exactly what made this test fail RED: the stale
//! owner's caller-side pre-check read the original owner and passed, and
//! the unguarded write had no ownership check left to refuse it with, so
//! the decision was accepted (`Ok(DecideOutcome::Decided)`) instead of
//! refused. That committed RED failure is what pinned the interleaving
//! above as real and reproducible, not hypothetical.
//!
//! Ownership is now arbitrated exclusively inside
//! `resolve_policy_violation`'s guarded transaction (R72), mirroring
//! R71's `decide_approval`: the write itself re-reads
//! `tasks.owner_client_instance_id` and refuses a caller that no longer
//! owns the task. That makes the `biased` enqueue ordering this file
//! still uses no longer load-bearing for correctness -- it now exists
//! only to keep the interleaving deterministic and easy to reason about.
//! Ownership is arbitrated at write time, so the assertion below holds
//! under every ordering the rebind and the decide can occur in, not only
//! the one `biased` pins.
//!
//! The second test proves the fix does not over-reject: a rebind followed,
//! sequentially, by a decide from the *new* owner must still succeed, with
//! exactly one `PolicyViolationDecided` event. It asserts only the
//! resolution contract -- not `Run.flags.policyQuarantined`, which
//! `decide("release")` also clears via a follow-up round trip after the
//! guarded write commits; that flag is R73's territory, not this file's.
//!
//! The third test proves ownership outranks idempotent replay: the guarded
//! write checks `tasks.owner_client_instance_id` before it checks whether
//! a resolution is already on record, so a former owner replaying its own,
//! now-recorded resolution after a rebind is refused with
//! `ViolationError::Forbidden`, not accepted as
//! `Ok(DecideOutcome::AlreadyDecided)`.

use std::sync::Arc;

use crew_protocol::{
    PolicyViolationId, ProjectId, Run, RunFlags, RunId, RunState, TaskId, TaskRef, Timestamp,
    Worker, WorkerId, WorkerProfileRef,
};
use crew_runtime::config::NestedViolationAction;
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::DomainRepository;
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

/// Seeds one task (owned by `"omp-1"`)/worker/run, drives the run to
/// `working`, and records one unresolved nested-worker violation against
/// it. Returns the ids a test needs to decide, rebind, and probe.
async fn seed_pending_violation(
    db: &DatabaseHandle,
    project_id: ProjectId,
) -> (PolicyViolationId, RunId, TaskId) {
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

    (violation_id, run_id, task_id)
}

/// A [`ViolationService`] with no adapter driver; the broadcast sender has
/// no subscribers, which is the production shape for an unattached
/// console.
fn service(db: Arc<DatabaseHandle>, project_id: ProjectId) -> ViolationService {
    ViolationService::new(
        db,
        project_id,
        broadcast::channel(64).0,
        None,
        NestedViolationAction::Quarantine,
    )
}

/// A single `run_domain_op` round trip calling
/// [`DomainRepository::reconcile_ownership`] -- the same repo method
/// `reconcile/omp` calls (`service/orchestration.rs::reconcile_omp`) --
/// directly, presenting the task's current stored revision so its guarded
/// revision match (R74) admits the rebind.
async fn rebind_owner(
    db: &DatabaseHandle,
    project_id: ProjectId,
    task_id: TaskId,
    new_owner: &str,
    revision: u64,
) {
    let new_owner = new_owner.to_string();
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.reconcile_ownership(task_id, &new_owner, revision)
            .map(|_| json!({}))
    }))
    .await
    .expect("rebind task ownership");
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
async fn a_stale_owner_is_refused_by_the_guarded_write_after_a_rebind() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (violation_id, _run_id, task_id) = seed_pending_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    // `decide` (as the original owner "omp-1") is declared first, so its
    // snapshot round trip is enqueued -- and thus processed -- before the
    // rebind's `UPDATE`; the rebind then commits before `decide`'s second
    // round trip (`resolve_policy_violation`) can possibly be sent, per the
    // enqueue-order argument in this file's header. Deterministic, no
    // dependence on real timing.
    let (decide_result, _rebind) = tokio::join!(
        biased;
        svc.decide(violation_id, "omp-1", "release"),
        rebind_owner(&db, project_id, task_id, "omp-2", 1),
    );

    assert!(
        matches!(decide_result, Err(ViolationError::Forbidden { .. })),
        "a stale owner must be refused once the guarded write observes the rebind, not accepted: {decide_result:?}"
    );
    assert_eq!(
        violation_resolution(&db, violation_id).await,
        None,
        "a refused decision must never be recorded"
    );
    assert_eq!(
        decided_event_count(&db).await,
        0,
        "no policyViolationDecided event may survive a refused decision"
    );
}

#[tokio::test]
async fn the_new_owner_can_resolve_after_a_rebind() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (violation_id, _run_id, task_id) = seed_pending_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    // Rebind sequentially, fully committed before decide is even called --
    // zero timing dependency -- so this test guards the fix against
    // over-rejection: a legitimate new owner must still be able to
    // resolve after a rebind.
    rebind_owner(&db, project_id, task_id, "omp-2", 1).await;

    let outcome = svc.decide(violation_id, "omp-2", "release").await;

    assert!(
        matches!(outcome, Ok(DecideOutcome::Decided)),
        "the rebound owner must be able to resolve: {outcome:?}"
    );
    assert_eq!(
        violation_resolution(&db, violation_id).await,
        Some("release".to_string())
    );
    assert_eq!(
        decided_event_count(&db).await,
        1,
        "exactly one policyViolationDecided event must be journaled"
    );
}

#[tokio::test]
async fn a_former_owner_replaying_its_identical_resolution_is_refused() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (violation_id, _run_id, task_id) = seed_pending_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    let outcome = svc.decide(violation_id, "omp-1", "release").await;
    assert!(
        matches!(outcome, Ok(DecideOutcome::Decided)),
        "the original owner must be able to resolve: {outcome:?}"
    );

    rebind_owner(&db, project_id, task_id, "omp-2", 1).await;

    // The guarded write checks `tasks.owner_client_instance_id` before it
    // checks whether a resolution is already on record (repository.rs's
    // `resolve_policy_violation`), so a former owner replaying its own,
    // now-recorded resolution is refused with `Forbidden` -- ownership
    // outranks idempotent replay, it is not treated as a no-op repeat of
    // an identical decision.
    let replay = svc.decide(violation_id, "omp-1", "release").await;

    assert!(
        matches!(replay, Err(ViolationError::Forbidden { .. })),
        "a former owner replaying its own identical resolution must be refused by ownership, not accepted as an idempotent replay: {replay:?}"
    );
    assert_eq!(
        violation_resolution(&db, violation_id).await,
        Some("release".to_string()),
        "the original resolution must remain on record"
    );
    assert_eq!(
        decided_event_count(&db).await,
        1,
        "the refused replay must not journal a second event"
    );
}
