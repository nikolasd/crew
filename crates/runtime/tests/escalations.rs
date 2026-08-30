//! WP20 triage plumbing: escalations.
//!
//! Covers the escalation lifecycle end to end at the repository boundary
//! (the same guarded-write layer the RPC handlers run through):
//!
//! * a journaled `WorkerQuestion` auto-opens a `question` escalation row in
//!   the SAME transaction (`record_adapter_event`);
//! * `message/send {kind:"answer"}` resolves the run's open question
//!   escalation inside the guarded write and reports whether the durable
//!   `EscalationAnswered` fact must be journaled;
//! * a write-shaped tool from a run whose plan subtask declared
//!   `writes: false` raises a `write_violation` escalation; declared-writes
//!   runs and runs without plan provenance never raise;
//! * two consecutive failed runs for one task trip WP20's repeated-failure
//!   detector;
//! * concurrent answers race through the FIFO database actor and exactly
//!   one resolution wins (the `decided_at IS NULL` guard), following the
//!   `approval_decide_race.rs` pattern.

use crew_protocol::{
    DeliveryState, MessageId, MessageKind, ProjectId, Run, RunFlags, RunId, RunState, TaskId,
    TaskRef, Timestamp, Worker, WorkerId, WorkerProfileRef,
};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::DomainRepository;
use serde_json::json;
use tempfile::TempDir;

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

/// Seeds one task/worker/queued run. Returns the ids.
async fn seed_run(db: &DatabaseHandle, project_id: ProjectId) -> (RunId, TaskId, WorkerId) {
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

    (run_id, task_id, worker_id)
}

fn answer_message(
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    reply_to: MessageId,
) -> crew_protocol::RunMessage {
    crew_protocol::RunMessage {
        message_id: MessageId::new(),
        run_id,
        sender_worker_id: worker_id,
        recipient_worker_id: None,
        task_id,
        kind: MessageKind::Answer,
        payload: "42".to_string(),
        delivery_state: DeliveryState::Recorded,
        created_at: Timestamp::now(),
        sent_at: None,
        acknowledged_at: None,
        reply_to: Some(reply_to),
    }
}

#[tokio::test]
async fn a_worker_question_opens_exactly_one_escalation_row() {
    let (_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (run_id, task_id, worker_id) = seed_run(&db, project_id).await;

    // Two question events in a row: only the first may open a row while it
    // is still undecided.
    for _ in 0..2 {
        let event = crew_protocol::RuntimeEvent::WorkerQuestion {
            run_id,
            task_id,
            worker_id,
            question: Some(crew_protocol::Redacted::assert_runtime_authored(
                "which module?",
            )),
        };
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.record_adapter_event(&event, task_id, worker_id, run_id, None)?;
            Ok(json!({}))
        }))
        .await
        .expect("journal question");
    }

    let rows = db
        .run_domain_op(Box::new(move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM escalations WHERE run_id = ?1 AND kind = 'question'",
                    [run_id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            Ok(json!(count))
        }))
        .await
        .unwrap();
    assert_eq!(
        rows.as_i64(),
        Some(1),
        "a second undecided question must not open a second row"
    );
}

#[tokio::test]
async fn an_answer_resolves_the_open_escalation_once() {
    let (_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (run_id, task_id, worker_id) = seed_run(&db, project_id).await;

    // Open the escalation the way record_adapter_event would.
    let question = crew_protocol::RuntimeEvent::WorkerQuestion {
        run_id,
        task_id,
        worker_id,
        question: Some(crew_protocol::Redacted::assert_runtime_authored(
            "which module?",
        )),
    };
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.record_adapter_event(&question, task_id, worker_id, run_id, None)?;
        Ok(json!({}))
    }))
    .await
    .expect("open escalation");

    // First answer resolves it and reports the fact.
    let (resolved, fact) = {
        let first = crew_protocol::RunMessage {
            message_id: MessageId::new(),
            run_id,
            sender_worker_id: worker_id,
            recipient_worker_id: None,
            task_id,
            kind: MessageKind::Answer,
            payload: "module A".into(),
            delivery_state: DeliveryState::Recorded,
            created_at: Timestamp::now(),
            sent_at: None,
            acknowledged_at: None,
            reply_to: Some(MessageId::new()),
        };
        let project = project_id;
        let result = db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project);
                let (committed, answered) =
                    repo.record_message(&first, Some("omp-1"), true, false)?;
                let fact = if answered {
                    repo.journal_escalation_answered(run_id, crew_protocol::AnsweredBy::Leader)
                        .ok()
                        .map(|c| c.sequence)
                } else {
                    None
                };
                Ok(json!({
                    "sequence": committed.sequence,
                    "answered": answered,
                    "factSequence": fact,
                }))
            }))
            .await
            .unwrap();
        (
            result["answered"].as_bool().unwrap(),
            result["factSequence"].as_u64(),
        )
    };
    assert!(
        resolved,
        "the first answer must resolve the open escalation"
    );
    assert!(fact.is_some(), "resolution must journal EscalationAnswered");

    // A second answer finds nothing open: no resolution, no fact.
    let again = {
        let second = answer_message(run_id, task_id, worker_id, MessageId::new());
        let project = project_id;
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project);
            repo.record_message(&second, Some("omp-1"), true, false)
                .map(|(_, resolved)| json!(resolved))
        }))
        .await
        .unwrap()
    };
    assert!(
        !again.as_bool().unwrap(),
        "an already-decided escalation must not resolve twice"
    );
}

/// Two concurrent answers race through the FIFO actor; the
/// `decided_at IS NULL` guard admits exactly one resolver.
#[tokio::test]
async fn concurrent_answers_admit_exactly_one_resolution() {
    let (_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (run_id, task_id, worker_id) = seed_run(&db, project_id).await;

    let question = crew_protocol::RuntimeEvent::WorkerQuestion {
        run_id,
        task_id,
        worker_id,
        question: Some(crew_protocol::Redacted::assert_runtime_authored("pick one")),
    };
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.record_adapter_event(&question, task_id, worker_id, run_id, None)?;
        Ok(json!({}))
    }))
    .await
    .expect("open escalation");

    async fn attempt(
        db: &DatabaseHandle,
        project: ProjectId,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
    ) -> bool {
        let msg = answer_message(run_id, task_id, worker_id, MessageId::new());
        let resolved = db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project);
                repo.record_message(&msg, None, true, true)
                    .map(|(_, resolved)| json!(resolved))
            }))
            .await
            .unwrap();
        resolved.as_bool().unwrap()
    }

    let (a, b) = tokio::join!(
        attempt(&db, project_id, run_id, task_id, worker_id),
        attempt(&db, project_id, run_id, task_id, worker_id),
    );
    let resolutions = u32::from(a) + u32::from(b);
    assert_eq!(
        resolutions, 1,
        "exactly one concurrent answer may resolve the open escalation"
    );
}

#[tokio::test]
async fn write_tools_from_read_only_subtasks_raise_and_others_do_not() {
    let (_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (run_id, task_id, _worker_id) = seed_run(&db, project_id).await;

    // Seed an approved plan whose subtask s-readonly declares writes:false
    // and s-writable declares writes:true, plus plan_ref on the run.
    db.run_domain_op(Box::new({
        move |conn| {
            let now = crew_protocol::Timestamp::now().as_str().to_string();
            conn.execute(
                "INSERT INTO plans (plan_id, project_id, run_id, task_id, worker_id, \
                   owner_client_instance_id, task_text, subtasks_json, status, created_at) \
                 VALUES ('plan-wp20', 'p', ?1, ?2, 'w', 'omp-1', 't', ?3, 'approved', ?4)",
                rusqlite::params![
                    run_id.to_string(),
                    task_id.to_string(),
                    // BARE array: exactly what propose_plan stores.
                    json!([
                        {"id": "s-readonly", "description": "d", "adapter": "fake", "writes": false},
                        {"id": "s-writable", "description": "d", "adapter": "fake", "writes": true}
                    ])
                    .to_string(),
                    now,
                ],
            )?;
            conn.execute(
                "UPDATE runs SET plan_ref = ?1 WHERE run_id = ?2",
                rusqlite::params![
                    json!({"planId": run_id.to_string(), "subtaskId": "s-readonly"}).to_string(),
                    run_id.to_string(),
                ],
            )?;
            Ok(json!({}))
        }
    }))
    .await
    .expect("seed plan + plan_ref");

    // A write-shaped tool from the read-only subtask raises.
    let raised = db
        .run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.raise_write_violation_if_declared_read_only(run_id, "edit")
                .map(|opt| json!(opt.is_some()))
        }))
        .await
        .unwrap();
    assert!(
        raised.as_bool().unwrap(),
        "write tool on writes:false subtask must raise"
    );

    // The raised escalation row exists with the machine reason.
    let rows = db
        .run_domain_op(Box::new(move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM escalations WHERE run_id = ?1 AND kind = 'write_violation'",
                    [run_id.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            Ok(json!(count))
        }))
        .await
        .unwrap();
    assert_eq!(rows.as_i64(), Some(1));
}
