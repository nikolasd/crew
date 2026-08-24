//! Integration tests for crash recovery.
//!
//! Exercises the real [`RecoveryCoordinator`] against a real, migrated
//! [`DatabaseHandle`] (never a hand-rolled schema): seeds a task/worker/run
//! through the real `DomainRepository` API via `run_domain_op`, drives each
//! run into the non-terminal state under test, then calls `recover()` and
//! asserts the resulting terminal state.
//!
//! The startup sweep has no age filter, so no test needs to age a run:
//! every seeded run is already "stuck" the moment it exists. The two
//! doctor-facing tests at the end are the only ones that manipulate
//! timestamps, and they do it by back-dating `runs.created_at` /
//! `events.timestamp` directly -- no production API back-dates, and the
//! doctor's silence report is the only consumer left that reads age at all.
//!
//! Tests run with `--test-threads=1` since they manipulate real database
//! state through the same actor a concurrent test's `DatabaseHandle` would
//! also spawn a thread for; keeping DB files per-test (via `TempDir`)
//! already isolates them, but the crate-wide convention is one thread.

use std::sync::Arc;

use crew_protocol::{
    ProjectId, Run, RunFlags, RunState, TaskId, TaskRef, Timestamp, WorkerId, WorkerProfileRef,
};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::doctor::{Doctor, DoctorResult};
use crew_runtime::domain::DomainRepository;
use crew_runtime::recovery::{DEFAULT_STALE_RUN_THRESHOLD, RecoveryConfig, RecoveryCoordinator};
use tempfile::TempDir;

/// Seeds one task + one worker + one run in `initial_state` against a real,
/// migrated database, and returns the run's identifiers for the caller to
/// drive further.
async fn seed_run(
    db: &DatabaseHandle,
    project_id: ProjectId,
    initial_state: &str,
) -> (TaskId, WorkerId, crew_protocol::RunId) {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = crew_protocol::RunId::new();

    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.upsert_task(
            task_id,
            &TaskRef {
                owner_client_instance_id: "omp-1".into(),
                revision: 1,
            },
        )?;
        let worker = crew_protocol::Worker {
            worker_id,
            profile_ref: WorkerProfileRef {
                id: worker_id,
                fingerprint: "sha256:fake".into(),
                adapter: "fake".into(),
                model: "test".into(),
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

    if initial_state != "queued" {
        drive_to_state(db, project_id, run_id, initial_state).await;
    }

    (task_id, worker_id, run_id)
}

/// Walks `run_id` through the legal edges from `queued` up to `target`.
async fn drive_to_state(
    db: &DatabaseHandle,
    project_id: ProjectId,
    run_id: crew_protocol::RunId,
    target: &str,
) {
    let path: &[&str] = match target {
        "starting" => &["starting"],
        "working" => &["starting", "working"],
        "waitingUser" => &["starting", "working", "waitingUser"],
        "waitingPeer" => &["starting", "working", "waitingPeer"],
        "paused" => &["starting", "working", "paused"],
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
        .unwrap_or_else(|e| panic!("drive to {state} failed: {e}"));
    }
}

/// Reads a run's current projected state directly, for assertions.
async fn run_state(db: &DatabaseHandle, run_id: crew_protocol::RunId) -> String {
    db.run_domain_op(Box::new(move |conn| {
        let state: String = conn.query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            [run_id.to_string()],
            |r| r.get(0),
        )?;
        Ok(serde_json::json!(state))
    }))
    .await
    .expect("read run state")
    .as_str()
    .expect("state is a string")
    .to_string()
}

/// A `RecoveryConfig` with the given opt-in flags. There is no threshold to
/// tune: the startup sweep takes every non-terminal run, so a test's seeded
/// run is already "stuck" the moment it exists.
fn config(recover_paused: bool, recover_waiting: bool) -> RecoveryConfig {
    RecoveryConfig {
        recover_paused,
        recover_waiting,
    }
}

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

#[tokio::test]
async fn recovery_returns_empty_when_no_stuck_runs() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let coordinator = RecoveryCoordinator::with_defaults(Arc::new(db), project_id);
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 0);
    assert!(result.recovered_runs.is_empty());
}

#[tokio::test]
async fn recovery_config_default_values() {
    let config = RecoveryConfig::default();
    assert!(!config.recover_paused);
    assert!(!config.recover_waiting);
}

// --------------------------------------------------------- kill-point tests

/// Kill-point: intent recorded (`queued`) but never started -- no evidence
/// the vendor process was ever spawned. Recovers to `failed`.
#[tokio::test]
async fn stuck_queued_run_recovers_to_failed() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "queued").await;

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(false, false));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 1);
    assert!(result.recovered_runs[0].success);
    assert_eq!(run_state(&db, run_id).await, "failed");
}

/// Kill-point: identity allocation in progress (`starting`) when the
/// process died -- the vendor child may or may not have spawned; without
/// process/PID evidence this sweep cannot tell, so it recovers to `failed`
/// (the invariant this sweep guarantees is "no false success/`succeeded`",
/// not "no false negative on a possibly-still-running process" -- that is
/// `RecoveryCoordinator`'s own PID/executable verification, out of this
/// module's scope per the Hardening plan's kill-point matrix).
#[tokio::test]
async fn stuck_starting_run_recovers_to_failed() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "starting").await;

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(false, false));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 1);
    assert_eq!(run_state(&db, run_id).await, "failed");
}

/// Kill-point: mid-run (`working`, covers child spawn and vendor
/// acknowledgement -- both project onto this one state in the current
/// schema) when the process died. Recovers to `failed`.
#[tokio::test]
async fn stuck_working_run_recovers_to_failed() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "working").await;

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(false, false));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 1);
    assert_eq!(run_state(&db, run_id).await, "failed");
}

/// Kill-point: waiting on a peer worker's acknowledgement (`waitingPeer`)
/// when the process died. With `recover_waiting: true`, recovers to
/// `cancelled` -- never `failed`, since the run was legitimately paused on
/// external input, not evidence of a failure.
#[tokio::test]
async fn stuck_waiting_peer_run_recovers_to_cancelled_when_opted_in() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "waitingPeer").await;

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(false, true));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 1);
    assert_eq!(run_state(&db, run_id).await, "cancelled");
}

/// Kill-point: event append pending, surfaced here as waiting on user
/// approval (`waitingUser`) when the process died. With `recover_waiting:
/// false` (the default), the run is left untouched -- recovering it would
/// silently cancel work a human may still be about to approve.
#[tokio::test]
async fn stuck_waiting_user_run_is_untouched_when_not_opted_in() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "waitingUser").await;

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(false, false));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(
        result.recovered_count, 0,
        "waitingUser must stay untouched by default"
    );
    assert_eq!(run_state(&db, run_id).await, "waitingUser");
}

/// Kill-point: projection update pending, surfaced here as `paused` when
/// the process died. With `recover_paused: true`, recovers to `cancelled`.
#[tokio::test]
async fn stuck_paused_run_recovers_to_cancelled_when_opted_in() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "paused").await;

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(true, false));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 1);
    assert_eq!(run_state(&db, run_id).await, "cancelled");
}

/// A `paused` run is protected (never recovered) unless `recover_paused`
/// explicitly opts in -- the same invariant as `waitingUser`/`waitingPeer`,
/// proven separately since `paused` is reachable from `working` alone
/// (unlike the waiting states) and has its own config flag.
#[tokio::test]
async fn stuck_paused_run_is_untouched_when_not_opted_in() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "paused").await;

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(false, false));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 0);
    assert_eq!(run_state(&db, run_id).await, "paused");
}

/// R51: the realistic crash is "the daemon died and a supervisor restarted it
/// seconds later," so the startup sweep must recover a run whose last event is
/// seconds old. Under the old five-minute staleness cutoff this exact run --
/// the common case -- was skipped by the only sweep that would ever run
/// against that crash, and stayed `working` forever with no live process.
#[tokio::test]
async fn a_run_whose_last_event_is_seconds_old_is_recovered_at_startup() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "working").await;
    // No ageing of any kind: last activity is "now", which is exactly the
    // crash-then-immediate-restart case.

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::with_defaults(Arc::clone(&db), project_id);
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 1);
    assert_eq!(run_state(&db, run_id).await, "failed");
}

/// A run already in a terminal state is never touched by recovery -- it has
/// no outgoing edges and recovery must never attempt (and fail) a
/// transition out of one.
#[tokio::test]
async fn terminal_run_is_never_touched() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "working").await;
    let failed = RunState::try_from("failed").unwrap();
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.transition_run(run_id, &failed, None)
            .map(|_| serde_json::json!({}))
    }))
    .await
    .unwrap();

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(true, true));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 0);
    assert_eq!(run_state(&db, run_id).await, "failed");
}

/// Multiple independently-stuck runs are each recovered in one sweep, to
/// their own state-appropriate targets.
#[tokio::test]
async fn multiple_stuck_runs_are_all_recovered_independently() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_t1, _w1, queued_run) = seed_run(&db, project_id, "queued").await;
    let (_t2, _w2, working_run) = seed_run(&db, project_id, "working").await;
    let (_t3, _w3, paused_run) = seed_run(&db, project_id, "paused").await;

    let db = Arc::new(db);
    let coordinator = RecoveryCoordinator::new(Arc::clone(&db), project_id, config(true, true));
    let result = coordinator.recover().await.unwrap();

    assert_eq!(result.recovered_count, 3);
    assert_eq!(run_state(&db, queued_run).await, "failed");
    assert_eq!(run_state(&db, working_run).await, "failed");
    assert_eq!(run_state(&db, paused_run).await, "cancelled");
}

// ------------------------------------------- doctor's silence-threshold report

/// Back-dates a run's last activity -- both `runs.created_at` and every event
/// it journaled -- past `DEFAULT_STALE_RUN_THRESHOLD`. Raw SQL on purpose: no
/// production API back-dates a timestamp, and the doctor's report is the only
/// consumer left that reads age at all.
async fn backdate_past_stale_threshold(db: &DatabaseHandle, run_id: crew_protocol::RunId) {
    let old = (time::OffsetDateTime::now_utc()
        - time::Duration::seconds(
            i64::try_from(DEFAULT_STALE_RUN_THRESHOLD.as_secs()).unwrap() + 60,
        ))
    .format(&time::format_description::well_known::Rfc3339)
    .unwrap();
    db.run_domain_op(Box::new(move |conn| {
        conn.execute(
            "UPDATE runs SET created_at = ?1 WHERE run_id = ?2",
            rusqlite::params![old, run_id.to_string()],
        )?;
        conn.execute(
            "UPDATE events SET timestamp = ?1 WHERE run_id = ?2",
            rusqlite::params![old, run_id.to_string()],
        )?;
        Ok(serde_json::json!({}))
    }))
    .await
    .expect("backdate run activity");
}

/// A `Doctor` reading the same database and project the seeded runs live in.
/// No policy: `configuration_valid` then reports `skipped:`, which these
/// tests never inspect -- only the `stale_runs` entry.
fn doctor_over(db: &Arc<DatabaseHandle>, state_dir: &TempDir, project_id: ProjectId) -> Doctor {
    Doctor::new(
        Some(Arc::clone(db)),
        Some(state_dir.path().to_path_buf()),
        None,
    )
    .with_runtime_context(
        state_dir.path().join("runtime.sock"),
        state_dir.path().to_path_buf(),
        project_id,
    )
}

fn error_for<'a>(result: &'a DoctorResult, check_name: &str) -> Option<&'a str> {
    result
        .failed_checks
        .iter()
        .find(|c| c.check_name == check_name)
        .map(|c| c.error.as_str())
}

/// The doctor's `stale_runs` report runs against a *live* daemon, where a
/// quiet run is not a dead run, so it must keep the silence threshold the
/// startup sweep no longer has: a run whose last event is seconds old must
/// not be named.
#[tokio::test]
async fn the_doctors_stale_run_report_ignores_a_run_that_is_merely_recent() {
    let (state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, _run_id) = seed_run(&db, project_id, "working").await;

    let db = Arc::new(db);
    let doctor = doctor_over(&db, &state_dir, project_id);
    let result = doctor.check().await.unwrap();

    assert!(
        result.passed_checks.iter().any(|name| name == "stale_runs"),
        "a merely-recent run must not be reported stale: {:?}",
        result.failed_checks
    );
}

/// And the threshold is not merely a constant: a run back-dated past it is
/// named in the report, by id.
#[tokio::test]
async fn the_doctors_stale_run_report_names_a_run_silent_past_the_threshold() {
    let (state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) = seed_run(&db, project_id, "working").await;
    backdate_past_stale_threshold(&db, run_id).await;

    let db = Arc::new(db);
    let doctor = doctor_over(&db, &state_dir, project_id);
    let result = doctor.check().await.unwrap();

    let error = error_for(&result, "stale_runs").unwrap_or_else(|| {
        panic!(
            "expected the stale_runs check to fail: {:?}",
            result.failed_checks
        )
    });
    assert!(
        error.contains(&run_id.to_string()),
        "the report must name the offending run: {error}"
    );
}
