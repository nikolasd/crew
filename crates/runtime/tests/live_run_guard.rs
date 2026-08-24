//! R94: `require_live_run` is a caller-side pre-check in its own
//! `run_domain_op` round trip, so a run settling between that check and
//! the broker's write used to journal a message against a terminal run
//! -- despite the broker's doc promising a live-token connection can
//! "never ... mutate ... state for a run that is no longer active".
//! `record_message` now takes `enforce_live` and re-reads the run's state
//! inside the same guarded transaction as its `INSERT` (R78's
//! `enforce_quarantine` pattern), and `request_child` re-runs its
//! transition check inside its own guarded write, so a racing settle can
//! no longer be overwritten with `waitingPeer`.

use crew_protocol::{
    DeliveryState, MessageId, MessageKind, ProjectId, Run, RunFlags, RunId, RunMessage, RunState,
    TaskId, TaskRef, Timestamp, Worker, WorkerId, WorkerProfileRef,
};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::{DomainError, DomainRepository};
use serde_json::json;
use tempfile::TempDir;

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

/// Seeds one task/worker/run and drives the run through the supplied
/// state chain.
async fn seed_run(
    db: &DatabaseHandle,
    project_id: ProjectId,
    states: &'static [&'static str],
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
        repo.create_worker(&Worker {
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
        })?;
        repo.submit_run(
            &Run {
                run_id,
                task_id,
                worker_id,
                state: RunState::try_from("queued").expect("queued is valid"),
                flags: RunFlags::default(),
                vendor_session_id: None,
                started_at: None,
                completed_at: None,
            },
            None,
            None,
        )?;
        Ok(json!({}))
    }))
    .await
    .expect("seed task/worker/run");

    for state in states {
        let to = RunState::try_from(*state).expect("valid state");
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.transition_run(run_id, &to, None).map(|_| json!({}))
        }))
        .await
        .unwrap_or_else(|e| panic!("drive to {state} failed: {e}"));
    }

    (run_id, task_id, worker_id)
}

fn message(run_id: RunId, task_id: TaskId, worker_id: WorkerId) -> RunMessage {
    RunMessage {
        message_id: MessageId::new(),
        run_id,
        sender_worker_id: worker_id,
        recipient_worker_id: None,
        task_id,
        kind: MessageKind::PeerMessage,
        payload: "hello".to_string(),
        delivery_state: DeliveryState::Recorded,
        created_at: Timestamp::now(),
        sent_at: None,
        acknowledged_at: None,
        reply_to: None,
    }
}

#[tokio::test]
async fn an_enforced_write_against_a_settled_run_is_refused_in_transaction() {
    let (_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (run_id, task_id, worker_id) =
        seed_run(&db, project_id, &["starting", "working", "succeeded"]).await;

    let msg = message(run_id, task_id, worker_id);
    let err = db
        .run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.record_message(&msg, None, false, true)
                .map(|_| json!({}))
        }))
        .await
        .expect_err("a live-enforced write against a settled run must refuse");
    assert!(
        matches!(err, DomainError::RunSettled { .. }),
        "the refusal must be RunSettled, not an internal error: {err:?}"
    );

    // Nothing was journaled: no MessageRecorded event for this run.
    let recorded = db
        .run_domain_op(Box::new(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| json!({ "count": count }))
            .map_err(crew_runtime::domain::DomainError::from)
        }))
        .await
        .expect("count messages");
    assert_eq!(recorded["count"], 0, "a refused write journals nothing");
}

#[tokio::test]
async fn an_unenforced_write_still_records_against_a_settled_run() {
    // `message/send` (ompExtension) deliberately passes
    // `enforce_live: false` -- the parameter gates only the broker's
    // worker-MCP writes, whose doc promises liveness.
    let (_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (run_id, task_id, worker_id) =
        seed_run(&db, project_id, &["starting", "working", "succeeded"]).await;

    let msg = message(run_id, task_id, worker_id);
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.record_message(&msg, None, false, false)
            .map(|_| json!({}))
    }))
    .await
    .expect("an unenforced write records regardless of run state");
}

#[tokio::test]
async fn request_child_re_checks_the_transition_inside_its_guarded_write() {
    // The pre-check refuses the ordinary case; this pins the in-tx
    // re-check's refusal shape so the guarded write -- the only read that
    // can observe an interleaved settle -- stays load-bearing.
    let (_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (run_id, _task_id, _worker_id) =
        seed_run(&db, project_id, &["starting", "working", "succeeded"]).await;

    let err = db
        .run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.request_child(run_id, "spawn a helper")
                .map(|_| json!({}))
        }))
        .await
        .expect_err("a settled parent must refuse a child request");
    assert!(
        matches!(err, DomainError::Transition(_)),
        "the refusal must be a transition error: {err:?}"
    );
}
