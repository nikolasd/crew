//! Regression test for R73: `ApprovalService::decide`'s callback-failure
//! path (`crates/runtime/src/approval/service.rs`) used to write back the
//! *whole* `RunFlags` struct it read into `ApprovalSnapshot` before the
//! decision write and the vendor callback await. If anything else -- most
//! plausibly `ViolationService::apply_action`'s read-modify-write shape
//! (`crates/runtime/src/policy/violation.rs`, e.g. `quarantine`) --
//! mutated a different flag on the same run during that callback window,
//! `decide`'s write-back would silently revert it: a lost update, not a
//! conflict either side detects. The fix: `decide`'s callback-failure path
//! now calls `DomainRepository::set_run_flag(run_id, RunFlag::ProtocolUnhealthy,
//! true)`, which reads the run's current flags, flips this one, and writes
//! the row back all inside that one call, so nothing can go stale between
//! its read and its write.
//!
//! `FailingCallback::acknowledge` below plays the innocent case: it fails
//! without touching flags, so `decide`'s write is the *only* writer and
//! `protocolUnhealthy` lands correctly. `QuarantineDuringCallback::acknowledge`
//! plays the concurrent-mutation case: while `decide` awaits it, it performs
//! the same guarded `set_run_flag` call `ViolationService::quarantine`
//! performs in production, flipping `policy_quarantined`, and then fails,
//! so `decide`'s subsequent guarded write of `protocol_unhealthy` has
//! something it must not clobber.
//!
//! The first test also subscribes to the service's broadcast channel and
//! asserts the emitted `RunFlagsChanged` envelope, not just the database
//! row: `set_run_flag` builds that event from the flags it read and
//! flipped, so a reorder that constructed the event before applying the
//! flip would leave the row correct while broadcasting the stale,
//! pre-change struct -- the replay contract this project exists to
//! guarantee would then be silently wrong while every other assertion in
//! this file stayed green.

use std::sync::Arc;

use crew_protocol::{
    ApprovalId, ApprovalRequest, DecidedBy, EventEnvelope, ProjectId, Run, RunFlags, RunId,
    RunState, RuntimeEvent, TaskId, TaskRef, Timestamp, Worker, WorkerId, WorkerProfileRef,
};
use crew_runtime::approval::{ApprovalCallback, ApprovalService, CallbackFuture, DecideOutcome};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::{DomainRepository, RunFlag};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::broadcast;

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

/// Seeds one task/worker/run, drives the run to `working`, and creates one
/// pending, human-required approval against it -- identical in shape to
/// `approval_decide_race.rs`'s helper of the same name.
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

/// An [`ApprovalService`] wired to the given callback and broadcast sender.
/// Pass `broadcast::channel(64).0` directly for tests that never inspect
/// the broadcast stream (the production shape for an unattached console);
/// pass a sender whose receiver you kept via `.subscribe()` for tests that
/// assert what was broadcast.
fn service(
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    callback: Arc<dyn ApprovalCallback>,
    events_tx: broadcast::Sender<EventEnvelope>,
) -> ApprovalService {
    ApprovalService::new(db, project_id, callback, events_tx)
}

/// Reads a run's current flags directly, independent of `ApprovalService`'s
/// own (potentially stale) `ApprovalSnapshot` -- this is the ground truth
/// each test asserts against.
async fn read_run_flags(db: &DatabaseHandle, run_id: RunId) -> RunFlags {
    let value = db
        .run_domain_op(Box::new(move |conn| {
            conn.query_row(
                "SELECT flags_degraded_control, flags_needs_reconciliation, flags_protocol_unhealthy,
                        flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
                        flags_turn_settled
                 FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| {
                    Ok(json!({
                        "degradedControl": row.get::<_, i64>(0)? != 0,
                        "needsReconciliation": row.get::<_, i64>(1)? != 0,
                        "protocolUnhealthy": row.get::<_, i64>(2)? != 0,
                        "policyQuarantined": row.get::<_, i64>(3)? != 0,
                        "workspaceDirty": row.get::<_, i64>(4)? != 0,
                        "childrenActive": row.get::<_, i64>(5)? != 0,
                        "turnSettled": row.get::<_, i64>(6)? != 0,
                    }))
                },
            )
            .map_err(Into::into)
        }))
        .await
        .expect("read run flags");

    RunFlags {
        degraded_control: value["degradedControl"].as_bool().unwrap_or(false),
        needs_reconciliation: value["needsReconciliation"].as_bool().unwrap_or(false),
        protocol_unhealthy: value["protocolUnhealthy"].as_bool().unwrap_or(false),
        policy_quarantined: value["policyQuarantined"].as_bool().unwrap_or(false),
        workspace_dirty: value["workspaceDirty"].as_bool().unwrap_or(false),
        children_active: value["childrenActive"].as_bool().unwrap_or(false),
        turn_settled: value["turnSettled"].as_bool().unwrap_or(false),
    }
}

/// Fails every callback without touching the run at all -- the baseline
/// case where `decide`'s write-back of its pre-callback snapshot is the
/// only writer and therefore correct.
struct FailingCallback;

impl ApprovalCallback for FailingCallback {
    fn acknowledge(&self, _approval_id: ApprovalId, _decision: &str) -> CallbackFuture<'static> {
        Box::pin(async { Err("adapter unreachable".to_string()) })
    }
}

/// Fails every callback, but first performs the exact guarded flag flip
/// `ViolationService::quarantine` performs post-R73
/// (`crates/runtime/src/policy/violation.rs`): call
/// `DomainRepository::set_run_flag(run_id, RunFlag::PolicyQuarantined, true)`,
/// which reads the run's current flags, flips this one, and writes the
/// whole row back, all inside that one call. This is the same shape
/// `ViolationService::apply_action` can run concurrently with an
/// in-flight `decide` -- both go through the same single-consumer
/// database actor, so this mutation is guaranteed to land, and to land
/// strictly between `decide`'s pre-callback snapshot read and its
/// post-callback-failure write-back, because it runs *inside* the callback
/// await `decide` is blocked on.
struct QuarantineDuringCallback {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    run_id: RunId,
}

impl ApprovalCallback for QuarantineDuringCallback {
    fn acknowledge(&self, _approval_id: ApprovalId, _decision: &str) -> CallbackFuture<'static> {
        let db = Arc::clone(&self.db);
        let project_id = self.project_id;
        let run_id = self.run_id;
        Box::pin(async move {
            db.run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.set_run_flag(run_id, RunFlag::PolicyQuarantined, true)
                    .map(|_| json!({}))
            }))
            .await
            .expect("apply quarantine inside the callback window");

            Err("adapter unreachable".to_string())
        })
    }
}

#[tokio::test]
async fn a_flag_set_during_the_callback_window_survives_a_callback_failure() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, _task_id) = seed_pending_approval(&db, project_id).await;

    let (events_tx, mut events_rx) = broadcast::channel(64);
    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(QuarantineDuringCallback {
            db: Arc::clone(&db),
            project_id,
            run_id,
        }),
        events_tx,
    );

    let outcome = svc
        .decide(approval_id, "omp-1", "approve", "ok", DecidedBy::Human)
        .await;

    assert!(
        matches!(outcome, Ok(DecideOutcome::DecidedCallbackFailed)),
        "a failing callback must still record the decision: {outcome:?}"
    );

    let flags = read_run_flags(&db, run_id).await;
    assert!(
        flags.policy_quarantined,
        "policy_quarantined was set by a concurrent writer inside the callback \
         window and must survive decide's callback-failure write-back, but it \
         was reverted to false: {flags:?}"
    );
    assert!(
        flags.protocol_unhealthy,
        "the callback failure must still mark the run protocol_unhealthy: {flags:?}"
    );

    // The database row is only half the contract: the `RunFlagsChanged`
    // event decide's callback-failure write broadcasts must carry the same
    // post-change struct. `set_run_flag` builds that event from the flags
    // it read and flipped (`repository.rs`) before returning; a reorder
    // that constructed the event first and applied the flip after would
    // leave this row correct while broadcasting the stale, pre-change
    // struct -- and every assertion above would stay green.
    let mut run_flags_events = Vec::new();
    while let Ok(envelope) = events_rx.try_recv() {
        if let RuntimeEvent::RunFlagsEvent { flags, .. } = envelope.event {
            run_flags_events.push(flags);
        }
    }
    let broadcast_flags = run_flags_events
        .last()
        .expect("decide's callback-failure write broadcasts a RunFlagsChanged event");
    assert!(
        broadcast_flags.protocol_unhealthy,
        "the broadcast RunFlagsChanged payload must carry protocol_unhealthy: {broadcast_flags:?}"
    );
    assert!(
        broadcast_flags.policy_quarantined,
        "the broadcast RunFlagsChanged payload must carry the concurrent writer's \
         policy_quarantined, not a stale pre-change struct: {broadcast_flags:?}"
    );
}

#[tokio::test]
async fn the_unhealthy_flag_is_applied_when_no_concurrent_mutation_happens() {
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, _task_id) = seed_pending_approval(&db, project_id).await;

    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(FailingCallback),
        broadcast::channel(64).0,
    );

    let outcome = svc
        .decide(approval_id, "omp-1", "approve", "ok", DecidedBy::Human)
        .await;

    assert!(
        matches!(outcome, Ok(DecideOutcome::DecidedCallbackFailed)),
        "a failing callback must still record the decision: {outcome:?}"
    );

    let flags = read_run_flags(&db, run_id).await;
    assert!(
        flags.protocol_unhealthy,
        "a failing callback must mark the run protocol_unhealthy: {flags:?}"
    );
    assert!(
        !flags.degraded_control,
        "no other flag should change: {flags:?}"
    );
    assert!(
        !flags.needs_reconciliation,
        "no other flag should change: {flags:?}"
    );
    assert!(
        !flags.policy_quarantined,
        "no other flag should change: {flags:?}"
    );
    assert!(
        !flags.workspace_dirty,
        "no other flag should change: {flags:?}"
    );
    assert!(
        !flags.children_active,
        "no other flag should change: {flags:?}"
    );
}
