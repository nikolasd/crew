//! Regression tests for R75: `ViolationService`'s own quarantine
//! idempotency check used to be a check-then-act race, on the very flag
//! R73 (`run_flags_lost_update.rs`) hardened the *write* side of, and
//! R72/R54 (`violation_owner_race.rs`, `policy_violation.rs`) hardened the
//! ownership/conflict side of. Found during R73's adversarial review
//! (`agent://R73Adversary`, finding W2), not fixed by it: R73's mechanism
//! is the flag write itself never losing a concurrent mutation of a
//! *different* flag; this file's races are about whether
//! `ViolationService` decides to write `policyQuarantined` at all.
//!
//! Two shapes, both fixed across `crates/runtime/src/policy/violation.rs`
//! and `crates/runtime/src/domain/repository.rs`:
//!
//! 1. `record_nested_worker`/`record_cost_ceiling` used to read
//!    `flags.policy_quarantined` in their own round trip (the deleted
//!    `ViolationService::load_run_state_and_flags`) *before* the
//!    violation was even journaled, and cache the result as
//!    `already_actioned` for `apply_action` to consume a round trip
//!    later -- a stale snapshot a concurrent release's clear could land
//!    inside of. Fixed by moving that read into
//!    `DomainRepository::record_policy_violation` itself, immediately
//!    before the same call's `INSERT`; `ViolationService::load_policy_fingerprint`
//!    now loads only the policy fingerprint.
//! 2. `ViolationService::decide`'s release path used to resolve the
//!    violation (`DomainRepository::resolve_policy_violation`, one
//!    commit) and then unconditionally unquarantine the run (the
//!    deleted `set_quarantined(run_id, false)`, a *second*, independent
//!    commit) -- a violation recorded and actioned in the gap between
//!    those two commits got its quarantine silently reverted by a
//!    release that targeted a different, unrelated violation. Fixed by
//!    `DomainRepository::release_quarantine`, which counts still-
//!    unresolved violations on the run -- inside the same closure as its
//!    own read and write -- and refuses to clear the flag if any remain.
//!
//! Three of the four tests below drive two service calls through
//! `tokio::join!(biased; ...)`, following `violation_owner_race.rs`'s
//! argument: `DatabaseHandle::run_domain_op` is a strictly FIFO
//! single-consumer actor (`crates/runtime/src/db/actor.rs`), and `biased`
//! polls the same declared future first on *every* poll of the combined
//! future. Each service method here is a straight, unbranching chain of
//! `run_domain_op` round trips with no vendor callback and no external
//! wait, so the future declared first in the `biased` join always has its
//! round trip `k` enqueued -- and thus, by strict FIFO, completed -- before
//! the other future's round trip `k`, for every `k`: whenever the
//! second-declared future's round trip `k` reply is ready, the
//! first-declared future's round trip `k` reply must already be ready too
//! (it was enqueued, and therefore processed, earlier), so `biased`'s
//! first-then-second poll order deterministically advances the first future
//! one round trip ahead of the second at every step. No `sleep`, no
//! `tokio::spawn`, no flake surface. Verified with 5 consecutive `--exact`
//! runs of this file, no flakes observed. The fourth test is non-
//! concurrent: `a_plain_release_with_no_concurrent_violation_clears_quarantine`
//! guards the ordinary case a fix must not regress -- releasing a
//! violation with nothing racing it must still clear quarantine.
//!
//! Which hunk each test falsifies (`agent://ReviewR75` A6). The three
//! pre-existing tests all falsify the *same* hunk,
//! `DomainRepository::release_quarantine`'s unresolved-violation count
//! guard; restoring the pre-fix unconditional clear in its place breaks
//! all three:
//!
//! - `a_release_landing_mid_record_does_not_suppress_the_fresh_quarantine`
//!   and `a_release_does_not_unquarantine_a_violation_recorded_after_its_resolve`
//!   both falsify on the count guard itself: with the unconditional clear
//!   restored, the release always wipes the fresh violation's quarantine.
//! - `a_plain_release_with_no_concurrent_violation_clears_quarantine`
//!   falsifies the guard's *breadth*: drop `resolution IS NULL` from the
//!   count, or move the count ahead of the resolve, and the sole released
//!   violation counts itself and the release wrongly refuses to clear.
//!
//! None of the three falsifies the record-side fix (item 1 above) --
//! `a_record_landing_after_a_release_reapplies_quarantine_from_its_own_read`,
//! added below, closes that gap: hoisting
//! `record_policy_violation`'s `flags_policy_quarantined`/`state` read
//! back out into an earlier, separate round trip (the pre-fix shape)
//! reproduces the staleness and this test's final assertions fail.

use std::sync::Arc;

use crew_protocol::{
    PolicyViolationId, ProjectId, Run, RunFlags, RunId, RunState, TaskId, TaskRef, Timestamp,
    Worker, WorkerId, WorkerProfileRef,
};
use crew_runtime::config::NestedViolationAction;
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::{DomainRepository, RunFlag};
use crew_runtime::policy::{DecideOutcome, ViolationService};
use rusqlite::OptionalExtension;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::broadcast;

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

/// Seeds one task (owned by `"omp-1"`)/worker/run and drives the run to
/// `working`, flags untouched (all default `false`). Shared by both
/// quarantine states the tests below need.
async fn seed_task_worker_run(
    db: &DatabaseHandle,
    project_id: ProjectId,
) -> (RunId, TaskId, WorkerId) {
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

    (run_id, task_id, worker_id)
}

/// Journals one unresolved `nested_worker_denied` violation against
/// `run_id`, bypassing `ViolationService` entirely -- this is the
/// pre-seeded violation each test's `decide("release")` targets, distinct
/// from the fresh one a concurrent `record_nested_worker` call journals.
async fn record_raw_violation(
    db: &DatabaseHandle,
    project_id: ProjectId,
    violation_id: PolicyViolationId,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
) {
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
            Some("child-old"),
            Some("parent-old"),
            "quarantine",
        )
        .map(|_| json!({}))
    }))
    .await
    .expect("record the pre-seeded violation");
}

/// Seeds a task/worker/run already quarantined, with one unresolved
/// violation against it -- the state a plain release acts on, and the state
/// Interleaving A's release call needs (a release of an *older* violation
/// while the run is quarantined for that same reason).
async fn seed_quarantined_violation(
    db: &DatabaseHandle,
    project_id: ProjectId,
) -> (PolicyViolationId, RunId, TaskId, WorkerId) {
    let (run_id, task_id, worker_id) = seed_task_worker_run(db, project_id).await;

    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.set_run_flag(run_id, RunFlag::PolicyQuarantined, true)
            .map(|_| json!({}))
    }))
    .await
    .expect("quarantine the run");

    let violation_id = PolicyViolationId::new();
    record_raw_violation(db, project_id, violation_id, run_id, task_id, worker_id).await;

    (violation_id, run_id, task_id, worker_id)
}

/// Seeds a task/worker/run with one unresolved violation but *no*
/// quarantine yet -- the state Interleaving B needs: a release has
/// something to resolve, but nothing has forced the flag `true`, so a
/// concurrent fresh violation's own `apply_action` is the one deciding
/// whether to quarantine the run.
async fn seed_unquarantined_violation(
    db: &DatabaseHandle,
    project_id: ProjectId,
) -> (PolicyViolationId, RunId, TaskId, WorkerId) {
    let (run_id, task_id, worker_id) = seed_task_worker_run(db, project_id).await;
    let violation_id = PolicyViolationId::new();
    record_raw_violation(db, project_id, violation_id, run_id, task_id, worker_id).await;
    (violation_id, run_id, task_id, worker_id)
}

/// A `ViolationService` with no adapter driver; the broadcast sender has no
/// subscribers, which is the production shape for an unattached console.
fn service(db: Arc<DatabaseHandle>, project_id: ProjectId) -> ViolationService {
    ViolationService::new(
        db,
        project_id,
        broadcast::channel(64).0,
        None,
        NestedViolationAction::Quarantine,
    )
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

/// The one remaining unresolved violation for `run_id` -- used to find the
/// *fresh* violation a racing `record_nested_worker` call journals, once
/// the pre-seeded one has been resolved by a concurrent release.
async fn unresolved_violation_id(db: &DatabaseHandle, run_id: RunId) -> Option<PolicyViolationId> {
    let id = db
        .run_domain_op(Box::new(move |conn| {
            let id: Option<String> = conn
                .query_row(
                    "SELECT violation_id FROM policy_violations
                     WHERE run_id = ?1 AND resolution IS NULL",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(json!(id))
        }))
        .await
        .expect("read unresolved violation id");
    id.as_str()
        .map(|s| PolicyViolationId::parse(s).expect("stored id is a valid uuid"))
}

#[tokio::test]
async fn a_release_landing_mid_record_does_not_suppress_the_fresh_quarantine() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (old_violation_id, run_id, task_id, worker_id) =
        seed_quarantined_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    // `record_nested_worker` is declared first: `biased` FIFO guarantees
    // its round trips land one ahead of `decide`'s at every step --
    // `load_policy_fingerprint` (round 1) before `decide`'s snapshot, then
    // `record_policy_violation`'s own flag read (round 2, the fix this
    // test defends) before `decide`'s resolve. That a-read happens
    // strictly before `decide`'s `release_quarantine` round trip (which
    // only runs after the resolve), so it always reads
    // `policy_quarantined = true` from the seed -- `already_actioned` is
    // `true` regardless of anything the release goes on to do, and
    // `apply_action` makes no further round trip at all.
    let (record_result, release_result) = tokio::join!(
        biased;
        svc.record_nested_worker(run_id, task_id, worker_id, "child-new", "parent-new", 42),
        svc.decide_and_release_status(old_violation_id, "omp-1", "release"),
    );

    record_result.expect("recording the fresh violation must succeed");
    let (outcome, quarantine_cleared) =
        release_result.expect("releasing the old violation must succeed");
    assert_eq!(outcome, DecideOutcome::Decided);
    assert_eq!(
        quarantine_cleared,
        Some(false),
        "the concurrent fresh violation's own INSERT lands before the \
         release's unresolved-count read, so the release must report it \
         did not clear the flag it targeted"
    );

    let fresh_violation_id = unresolved_violation_id(&db, run_id)
        .await
        .expect("the fresh violation must be journaled");
    assert_ne!(
        fresh_violation_id, old_violation_id,
        "the fresh violation must be a distinct record from the released one"
    );
    assert_eq!(
        violation_resolution(&db, fresh_violation_id).await,
        None,
        "the fresh violation must remain unresolved"
    );

    assert!(
        run_quarantined(&db, run_id).await,
        "a fresh, still-open nested-worker violation on this run must leave \
         it quarantined even though an *older*, unrelated violation was \
         released concurrently -- `already_actioned`'s stale snapshot must \
         not suppress the fresh violation's own quarantine just because a \
         different violation's release cleared the flag afterwards"
    );
}

#[tokio::test]
async fn a_release_does_not_unquarantine_a_violation_recorded_after_its_resolve() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (old_violation_id, run_id, task_id, worker_id) =
        seed_unquarantined_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    // `record_nested_worker` is declared first again, for the same
    // structural reason, but the run starts *unquarantined*, so this
    // interleaving now exercises `release_quarantine`'s guard rather than
    // `record_policy_violation`'s read: round 1 is `load_policy_fingerprint`
    // vs `decide`'s snapshot; round 2 is `record_policy_violation`'s own
    // flag read (`policy_quarantined = false` from the seed --
    // `already_actioned = false`) and `INSERT`, landing strictly before
    // `decide`'s resolve; round 3 is the fresh violation's own `quarantine`
    // call landing strictly before `decide`'s `release_quarantine`. By the
    // time `release_quarantine` runs, the flag is already `true` again and
    // the fresh, still-unresolved violation makes its unresolved-count
    // guard refuse to clear it.
    let (record_result, release_result) = tokio::join!(
        biased;
        svc.record_nested_worker(run_id, task_id, worker_id, "child-new", "parent-new", 99),
        svc.decide(old_violation_id, "omp-1", "release"),
    );

    record_result.expect("recording the fresh violation must succeed");
    assert_eq!(
        release_result.expect("releasing the old violation must succeed"),
        DecideOutcome::Decided,
    );

    let fresh_violation_id = unresolved_violation_id(&db, run_id)
        .await
        .expect("the fresh violation must be journaled");
    assert_ne!(
        fresh_violation_id, old_violation_id,
        "the fresh violation must be a distinct record from the released one"
    );
    assert_eq!(
        violation_resolution(&db, fresh_violation_id).await,
        None,
        "the fresh violation must remain unresolved"
    );

    assert!(
        run_quarantined(&db, run_id).await,
        "a fresh violation's own quarantine commit must survive a release \
         that targets a *different*, already-resolved violation -- \
         `release_quarantine`'s unresolved-violation guard must see the \
         fresh violation and refuse to clear the flag, not clobber a \
         quarantine journaled for a reason recorded after the release's \
         own resolve already committed"
    );
}

#[tokio::test]
async fn a_plain_release_with_no_concurrent_violation_clears_quarantine() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (violation_id, run_id, ..) = seed_quarantined_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    let (outcome, quarantine_cleared) = svc
        .decide_and_release_status(violation_id, "omp-1", "release")
        .await
        .expect("releasing the only violation must succeed");

    assert_eq!(outcome, DecideOutcome::Decided);
    assert_eq!(
        quarantine_cleared,
        Some(true),
        "releasing the only unresolved violation on the run must report \
         that it cleared the flag"
    );
    assert_eq!(
        violation_resolution(&db, violation_id).await,
        Some("release".to_string())
    );
    assert!(
        !run_quarantined(&db, run_id).await,
        "a plain release with nothing racing it must still clear quarantine"
    );
}

/// Falsifies the record-side fix (`agent://ReviewR75` W1): `decide` is
/// declared first this time, and `record_nested_worker` is wrapped in one
/// leading no-op round trip, so `biased` FIFO forces the enqueue order
/// decide-snapshot, no-op, decide-resolve, record-fingerprint,
/// decide-release (clears -- the fresh violation has not landed yet),
/// record's own flag read (sees the flag already cleared), record's own
/// re-quarantine.
///
/// If `DomainRepository::record_policy_violation`'s
/// `flags_policy_quarantined`/`state` read were hoisted back out into an
/// earlier, separate round trip *before* `load_policy_fingerprint` --
/// the pre-R75 shape -- it would pair with `decide`'s `resolve` round
/// trip rather than its `release`, land *before* the release's clear
/// instead of after it, and read the seed's `true`. `already_actioned`
/// would then come back `true` and the fresh violation would never get
/// its own quarantine applied. Confirmed by making exactly that temporary
/// edit (a standalone query for the two columns, run one round trip ahead
/// of the fingerprint load, with `apply_action` driven from its result
/// instead of `record_policy_violation`'s own): `run_quarantined` came
/// back `false` at the end of this test, failing the assertion below;
/// restoring the read to its guarded-closure location made the test pass
/// again.
#[tokio::test]
async fn a_record_landing_after_a_release_reapplies_quarantine_from_its_own_read() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (old_violation_id, run_id, task_id, worker_id) =
        seed_quarantined_violation(&db, project_id).await;
    let svc = service(Arc::clone(&db), project_id);

    // The leading no-op round trip delays `record_nested_worker`'s first
    // real round trip (`load_policy_fingerprint`) by exactly one command
    // relative to `decide`, which is declared first below -- that one-
    // command offset is what lands `decide`'s every round trip strictly
    // ahead of `record_nested_worker`'s corresponding one, including the
    // release's clear landing before `record_policy_violation`'s own read.
    let record = async {
        db.run_domain_op(Box::new(|_conn| Ok(json!({}))))
            .await
            .expect("leading no-op round trip");
        svc.record_nested_worker(run_id, task_id, worker_id, "child-new", "parent-new", 7)
            .await
    };

    let (release_result, record_result) = tokio::join!(
        biased;
        svc.decide(old_violation_id, "omp-1", "release"),
        record,
    );

    assert_eq!(
        release_result.expect("releasing the pre-seeded violation must succeed"),
        DecideOutcome::Decided,
    );
    record_result.expect("recording the fresh violation must succeed");

    let fresh_violation_id = unresolved_violation_id(&db, run_id)
        .await
        .expect("the fresh violation must be journaled and still unresolved");
    assert_ne!(
        fresh_violation_id, old_violation_id,
        "the fresh violation must be a distinct record from the released one"
    );

    assert!(
        run_quarantined(&db, run_id).await,
        "a fresh nested-worker violation landing just after a release cleared \
         the flag must re-quarantine the run from its own read -- it must \
         not observe the just-cleared flag and skip quarantining because a \
         stale `already_actioned` was captured a round trip too early"
    );
}
