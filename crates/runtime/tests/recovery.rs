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

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use crew_protocol::{
    ProjectId, Run, RunFlags, RunState, TaskId, TaskRef, Timestamp, WorkerId, WorkerProfileRef,
};
use crew_runtime::adapter::tui::TuiTimings;
use crew_runtime::adapter::{
    AdapterKind, AdapterMode as ProfileAdapterMode, AdapterRegistry, ClaudeStartupOptions,
    FixtureAuthorization, ResumeSupport, StartupOptions, TuiSupport, WorkerProfile,
};
use crew_runtime::config::NestedViolationAction;
use crew_runtime::config::crew::{
    AdapterConfig, AdapterMode as CrewAdapterMode, CloseOnExit, PermissionMode,
};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::display::{DisplayRegistry, HiddenDisplay};
use crew_runtime::doctor::{Doctor, DoctorResult};
use crew_runtime::domain::DomainRepository;
use crew_runtime::policy::ViolationService;
use crew_runtime::recovery::{
    DEFAULT_STALE_RUN_THRESHOLD, RecoveredOutcome, RecoveryConfig, RecoveryCoordinator,
};
use crew_runtime::supervisor::EscalationTimings;
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

/// WP29 gap: a daemon *restart* must not lose the durable transcript. This
/// drops the live in-memory handle and re-opens the same on-disk journal —
/// the exact persistence boundary a real `crewd stop` -> `crewd serve` crosses
/// on restart — then asserts the journaled events for a run are still present
/// and the run itself is still persisted.
#[tokio::test]
async fn a_journaled_transcript_survives_a_database_restart() {
    let (state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (_task, _worker, run_id) = seed_run(&db, project_id, "working").await;

    // The working-state transition journals at least one event: that is the
    // run's durable transcript tail.
    let before = journal_count(&db, run_id, "working").await;
    assert!(before > 0, "expected journaled events before restart");
    assert_eq!(
        run_state(&db, run_id).await,
        "working",
        "run persisted before restart"
    );

    // Simulate the daemon restart: release the live handle, reopen the same
    // journal from disk.
    drop(db);
    let db = DatabaseHandle::start(state_dir.path().join("runtime.db"))
        .await
        .expect("reopen journal after restart");

    let after = journal_count(&db, run_id, "working").await;
    assert_eq!(after, before, "journaled transcript must survive a restart");
    assert_eq!(
        run_state(&db, run_id).await,
        "working",
        "run must survive a restart"
    );
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

// ------------------------------------------------------------ WP15: resume first

/// The vendor session id the fake `claude` script below always establishes
/// (mirrors `tests/tui_claude_registry.rs`'s own fixture).
const SESSION_ID: &str = "11111111-1111-4111-8111-000000000099";

fn claude_tui_profile_json() -> String {
    let profile = WorkerProfile {
        id: crew_runtime::adapter::ProfileId::new(),
        adapter: AdapterKind::Claude.wire_name().to_string(),
        model: String::new(),
        permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
        startup_options: StartupOptions::Claude(ClaudeStartupOptions {
            mode: ProfileAdapterMode::Tui,
            ..ClaudeStartupOptions::default()
        }),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    };
    serde_json::to_string(&profile).expect("WorkerProfile is a plain serializable type")
}

/// A Claude profile's resolved JSON with a specific override for the
/// `mode` key inside `startupOptions.claude`: `Some(literal)` sets it to
/// that exact string; `None` removes the key entirely, simulating a
/// genuine pre-WP13 journal entry that predates the `mode` field ever
/// existing -- it must still deserialize (per `AdapterMode`'s own
/// `#[default] Headless`), not fail to parse.
fn claude_profile_json_with_mode(mode_override: Option<&str>) -> String {
    let profile = WorkerProfile {
        id: crew_runtime::adapter::ProfileId::new(),
        adapter: AdapterKind::Claude.wire_name().to_string(),
        model: String::new(),
        permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
        // The `mode` set here is a placeholder, overwritten below -- the
        // struct itself has no way to express "no mode key at all".
        startup_options: StartupOptions::Claude(ClaudeStartupOptions {
            mode: ProfileAdapterMode::Tui,
            ..ClaudeStartupOptions::default()
        }),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    };
    let mut value = serde_json::to_value(&profile).expect("WorkerProfile serializes");
    let claude_options = value["startupOptions"]["claude"]
        .as_object_mut()
        .expect("claude startup options is a JSON object");
    match mode_override {
        Some(mode) => {
            claude_options.insert("mode".to_string(), serde_json::json!(mode));
        }
        None => {
            claude_options.remove("mode");
        }
    }
    serde_json::to_string(&value).expect("profile value serializes")
}

/// A minimal registry wired ONLY for resume support -- no `TuiSupport` at
/// all. Sufficient (and deliberately minimal) for the retired-headless-mode
/// test below: the rejection fires from `mode` alone, before any adapter
/// construction, transcript lookup, or vendor spawn is ever attempted.
async fn resume_only_registry(
    db: &Arc<DatabaseHandle>,
    project_id: ProjectId,
) -> (
    Arc<AdapterRegistry>,
    tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
) {
    let (events_tx, _rx) = tokio::sync::broadcast::channel(256);
    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        std::env::temp_dir(),
        None,
        vec![],
    );
    registry.set_resume_support(Arc::new(ResumeSupport {
        db: Arc::clone(db),
        project_id,
        violation_service: Arc::new(ViolationService::new(
            Arc::clone(db),
            project_id,
            events_tx.clone(),
            None,
            NestedViolationAction::default(),
            crew_runtime::security::redaction::Redactor::new(),
        )),
        events_tx: events_tx.clone(),
    }));
    (Arc::new(registry), events_tx)
}

/// crew-v2 gap-closure WP-C ruling 1 -- the pre-drop journal compatibility
/// test, the heart of this WP: a run whose stored profile OMITS `mode`
/// entirely (a genuine pre-WP13 journal entry, defaulting to `Headless`
/// per `AdapterMode`'s own `#[default]`), and one that says `"mode":
/// "headless"` explicitly, must BOTH terminalize on boot recovery with the
/// honest retired-mode reason -- NOT "profile unreadable", NOT a
/// Claude-shaped transcript-path failure. (A pre-WP-C build would have
/// produced exactly one of those confusing symptoms instead:
/// `evaluate_resume_eligibility`'s old Headless branch asked a
/// since-deleted headless adapter for its declared capabilities.)
#[tokio::test]
async fn a_pre_mode_field_and_an_explicit_headless_journal_both_terminalize_with_the_retired_mode_reason()
 {
    for (label, mode_override) in [
        ("mode omitted entirely (pre-WP13 journal)", None),
        ("mode: \"headless\" explicit", Some("headless")),
    ] {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let profile_json = claude_profile_json_with_mode(mode_override);
        let (_task_id, _worker_id, run_id) =
            seed_run_with_profile(&db, project_id, "working", Some(profile_json)).await;
        set_resume_state(&db, run_id, Some("some-vendor-session".to_string())).await;

        let db = Arc::new(db);
        let (registry, events_tx) = resume_only_registry(&db, project_id).await;
        let coordinator = RecoveryCoordinator::with_resume(
            Arc::clone(&db),
            project_id,
            RecoveryConfig::default(),
            registry,
            events_tx,
        );

        let result = coordinator.recover().await.expect("sweep");
        assert_eq!(result.recovered_runs.len(), 1, "{label}: {result:?}");
        let recovered = &result.recovered_runs[0];
        assert_eq!(
            recovered.outcome,
            RecoveredOutcome::Terminalized,
            "{label}: {recovered:?}"
        );
        assert!(recovered.success, "{label}: {recovered:?}");
        assert_eq!(
            run_state(&db, run_id).await,
            "failed",
            "{label}: the run must terminalize, not stay stuck"
        );

        let reason = recovered.error.as_deref().unwrap_or_default();
        assert!(
            reason.contains("headless") && reason.contains("retired"),
            "{label}: expected the honest retired-mode reason, got: {reason:?}"
        );
        let reason_lower = reason.to_lowercase();
        assert!(
            !reason_lower.contains("unreadable"),
            "{label}: must not be a \"profile unreadable\"-shaped failure: {reason:?}"
        );
        assert!(
            !reason_lower.contains("transcript"),
            "{label}: must not be a Claude-shaped transcript-path failure: {reason:?}"
        );

        db.shutdown().await.expect("shutdown database");
    }
}

/// Seeds one task + worker + run exactly like [`seed_run`], but stores the
/// given resolved profile JSON on the worker -- what `resume_run` re-derives
/// the adapter kind and mode from.
async fn seed_run_with_profile(
    db: &DatabaseHandle,
    project_id: ProjectId,
    initial_state: &str,
    resolved_profile_json: Option<String>,
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
                adapter: "claude".into(),
                model: "test".into(),
                permission_envelope: serde_json::json!({}),
            },
            parent_worker_id: None,
            created_at: Timestamp::now(),
        };
        repo.create_worker_with_snapshot(&worker, resolved_profile_json)?;
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
    .expect("seed run with profile");

    if initial_state != "queued" {
        drive_to_state(db, project_id, run_id, initial_state).await;
    }

    (task_id, worker_id, run_id)
}

/// Writes the run's stored resume seam directly (`runs.vendor_session_id`
/// and `runs.transcript_cursor`). Raw SQL on purpose: these columns are only
/// ever written by an adapter event's own commit path, which a recovery test
/// does not drive.
async fn set_resume_state(
    db: &DatabaseHandle,
    run_id: crew_protocol::RunId,
    vendor_session_id: Option<String>,
) {
    let run_id_string = run_id.to_string();
    db.run_domain_op(Box::new(move |conn| {
        Ok(conn
            .execute(
                "UPDATE runs SET vendor_session_id = ?1 WHERE run_id = ?2",
                rusqlite::params![vendor_session_id, run_id_string],
            )
            .map(|_| serde_json::json!({}))?)
    }))
    .await
    .expect("write resume state");
}

/// One journaled-event count for this run, matched by a raw substring of the
/// stored `event_json` (same helper as `tests/tui_claude_registry.rs`).
async fn journal_count(db: &DatabaseHandle, run_id: crew_protocol::RunId, marker: &str) -> usize {
    let run_id = run_id.to_string();
    let marker_owned = marker.to_string();
    let value = db
        .run_domain_op(Box::new(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_json LIKE ?2",
                rusqlite::params![run_id, format!("%{marker_owned}%")],
                |row| row.get(0),
            )?;
            Ok(serde_json::json!(count))
        }))
        .await
        .expect("journal count query");
    value.as_i64().expect("count is an integer") as usize
}

/// The lowest `sequence` of a journaled event matching `marker` for this
/// run, or `None` if it never appears. Used to pin ordering between two
/// markers, not just their presence (M-3 rider, WP-A review).
async fn first_journal_sequence(
    db: &DatabaseHandle,
    run_id: crew_protocol::RunId,
    marker: &str,
) -> Option<i64> {
    let run_id = run_id.to_string();
    let marker_owned = marker.to_string();
    let value = db
        .run_domain_op(Box::new(move |conn| {
            // `MIN` over zero matching rows still returns exactly one row
            // (with a NULL aggregate), never zero rows, so this never needs
            // `.optional()`.
            let sequence: Option<i64> = conn.query_row(
                "SELECT MIN(sequence) FROM events WHERE run_id = ?1 AND event_json LIKE ?2",
                rusqlite::params![run_id, format!("%{marker_owned}%")],
                |row| row.get(0),
            )?;
            Ok(serde_json::json!(sequence))
        }))
        .await
        .expect("journal sequence query");
    value.as_i64()
}

/// Polls until the given journal marker appears (or times out), for
/// assertions about work the resumed VENDOR does asynchronously.
async fn wait_for_journalled(
    db: &Arc<DatabaseHandle>,
    run_id: crew_protocol::RunId,
    marker: &str,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if journal_count(db, run_id, marker).await >= 1 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// A fake `claude` binary that answers a fresh start AND resumes via
/// `--resume <session>` without ever touching stdin on the resume path --
/// never the real CLI (copied from `tests/tui_claude_registry.rs`'s
/// fixture, minus the nonce-discovery branch this sweep never exercises).
fn write_fake_claude_script(
    scripts_dir: &std::path::Path,
    session_dir: &std::path::Path,
) -> std::path::PathBuf {
    let script = format!(
        r#"#!/bin/sh
echo "Welcome to Claude Code!"
SESSION_ID="11111111-1111-4111-8111-000000000099"
TRANSCRIPT="{session_dir}/$SESSION_ID.jsonl"
if [ "$1" = "--resume" ]; then
  (
    sleep 0.3
    printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:30Z","message":{{"content":[{{"type":"text","text":"post-resume answer"}}]}}}}' >> "$TRANSCRIPT"
  ) &
  while IFS= read -r line; do :; done
  exit 0
fi
while IFS= read -r line; do
  :
done
"#,
        session_dir = session_dir.display(),
    );
    let path = scripts_dir.join("fake-claude.sh");
    std::fs::write(&path, script).expect("write fake claude script");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn fast_timings() -> TuiTimings {
    TuiTimings {
        readiness_quiet: Duration::from_millis(80),
        readiness_cap: Duration::from_secs(4),
        discovery_timeout: Duration::from_secs(4),
        tailer_poll: Duration::from_millis(40),
        submit_idle: Duration::from_millis(50),
        escalation: EscalationTimings {
            sigint_to_sigterm: Duration::from_millis(150),
            sigterm_to_sigkill: Duration::from_millis(150),
        },
    }
}

/// The full WP15 registry fixture: real `AdapterRegistry`, optional
/// `TuiSupport` pointed at the fake script (`false` models an adapter whose
/// TUI support is unavailable in this daemon), and `ResumeSupport` wired to
/// the same db/project/event channel. Returns the broadcast sender so the
/// coordinator can fan out its journaled envelopes.
async fn resume_registry(
    db: &Arc<DatabaseHandle>,
    dir: &TempDir,
    project_id: ProjectId,
    script_path: &std::path::Path,
    session_dir: &std::path::Path,
    with_tui_support: bool,
) -> (
    Arc<AdapterRegistry>,
    tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
) {
    let mut adapters = BTreeMap::new();
    adapters.insert(
        "claude".to_string(),
        AdapterConfig {
            enabled: true,
            bin: script_path.to_string_lossy().into_owned(),
            mode: CrewAdapterMode::Tui,
            permission_mode: PermissionMode::Default,
            model: None,
            profile: "test".to_string(),
            session_dir: Some(session_dir.to_string_lossy().into_owned()),
            extra_args: Vec::new(),
        },
    );
    let panes_dir = dir.path().join("panes");
    std::fs::create_dir_all(&panes_dir).expect("create panes dir");

    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        dir.path().to_path_buf(),
        None,
        vec![],
    );
    if with_tui_support {
        let mut display_registry = DisplayRegistry::new();
        display_registry.register(Box::new(HiddenDisplay::new(
            crew_protocol::DisplayConfig::default(),
        )));
        registry.set_tui_support(Arc::new(TuiSupport {
            display_registry: Arc::new(display_registry),
            panes_dir,
            crewd_path: dir.path().join("crewd"),
            state_dir: dir.path().to_path_buf(),
            close_on_exit: CloseOnExit::Always,
            forced_backend: None,
            adapters,
            timings: fast_timings(),
        }));
    }
    let (events_tx, _rx) = tokio::sync::broadcast::channel(256);
    registry.set_resume_support(Arc::new(ResumeSupport {
        db: Arc::clone(db),
        project_id,
        violation_service: Arc::new(ViolationService::new(
            Arc::clone(db),
            project_id,
            events_tx.clone(),
            None,
            NestedViolationAction::default(),
            crew_runtime::security::redaction::Redactor::new(),
        )),
        events_tx: events_tx.clone(),
    }));
    (Arc::new(registry), events_tx)
}

/// One fully-wired resumable scenario: a `working` run whose worker carries
/// a Claude `mode: "tui"` profile, whose vendor session id is stored, and
/// whose deterministic transcript file exists.
struct ResumableScenario {
    _state_dir: TempDir,
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    registry: Arc<AdapterRegistry>,
    events_tx: tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
    run_id: crew_protocol::RunId,
}

async fn seed_resumable_run(with_tui_support: bool) -> ResumableScenario {
    // Under /tmp, not the default temp root: a resumed TUI run attaches its
    // pane over a unix socket under this directory, and macOS's default
    // per-user temp root exceeds the platform sun_path limit.
    let state_dir = tempfile::Builder::new()
        .prefix("bat-recovery-wp15-")
        .tempdir_in("/tmp")
        .expect("create state dir");
    let db = Arc::new(
        DatabaseHandle::start(state_dir.path().join("runtime.db"))
            .await
            .unwrap(),
    );
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) =
        seed_run_with_profile(&db, project_id, "working", Some(claude_tui_profile_json())).await;

    let session_dir = state_dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    // The transcript must EXIST for eligibility; empty keeps the tailer
    // quiet until the resumed fixture appends its post-resume entry.
    std::fs::write(session_dir.join(format!("{SESSION_ID}.jsonl")), "")
        .expect("write empty transcript");
    set_resume_state(&db, run_id, Some(SESSION_ID.to_string())).await;

    let script_path = write_fake_claude_script(state_dir.path(), &session_dir);
    let (registry, events_tx) = resume_registry(
        &db,
        &state_dir,
        project_id,
        &script_path,
        &session_dir,
        with_tui_support,
    )
    .await;
    ResumableScenario {
        _state_dir: state_dir,
        db,
        project_id,
        registry,
        events_tx,
        run_id,
    }
}

/// A coordinator wired for the resume-first boot sweep.
fn resume_coordinator(scenario: &ResumableScenario) -> RecoveryCoordinator {
    RecoveryCoordinator::with_resume(
        Arc::clone(&scenario.db),
        scenario.project_id,
        RecoveryConfig::default(),
        Arc::clone(&scenario.registry),
        scenario.events_tx.clone(),
    )
}

/// The headline contract: a stuck run with a live vendor session, an
/// available adapter, and an existing transcript RESUMES -- same run, prior
/// state, no terminal edge -- and the resumed vendor actually continues
/// producing journaled output under this daemon.
#[tokio::test]
async fn a_resumable_working_run_is_resumed_and_stays_non_terminal() {
    let scenario = seed_resumable_run(true).await;
    let result = resume_coordinator(&scenario)
        .recover()
        .await
        .expect("sweep");

    assert_eq!(result.recovered_runs.len(), 1, "{result:?}");
    assert_eq!(
        result.recovered_runs[0].outcome,
        RecoveredOutcome::Resumed,
        "error: {:?}",
        result.recovered_runs[0].error
    );
    assert!(result.recovered_runs[0].success);
    assert_eq!(result.recovered_runs[0].new_state.to_string(), "working");
    assert_eq!(
        run_state(&scenario.db, scenario.run_id).await,
        "working",
        "the resumed run continues in its prior state"
    );
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_attempted\""
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_succeeded\""
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(&scenario.db, scenario.run_id, "\"code\":\"resume_failed\"").await,
        0
    );
    assert!(
        wait_for_journalled(&scenario.db, scenario.run_id, "post-resume answer").await,
        "the resumed vendor must actually continue and journal fresh output"
    );
    assert_eq!(
        scenario.registry.running_count(),
        1,
        "this daemon now owns the continued session"
    );

    let _ = scenario
        .registry
        .running_adapter(scenario.run_id)
        .unwrap()
        .dispose()
        .await;
    scenario.db.shutdown().await.expect("shutdown database");
}

/// A fake `claude` binary whose `--resume` invocation exits nonzero without
/// ever producing pty output -- unlike [`write_fake_claude_script`], this
/// one never becomes ready. `wait_for_readiness` sees the process exit
/// before readiness, so the adapter's own `fail_start` runs and journals a
/// real `ProcessExited` evidence.
fn write_fake_failing_resume_script(scripts_dir: &std::path::Path) -> std::path::PathBuf {
    let script = r#"#!/bin/sh
if [ "$1" = "--resume" ]; then
  exit 1
fi
while IFS= read -r line; do :; done
"#;
    let path = scripts_dir.join("fake-claude-fails-resume.sh");
    std::fs::write(&path, script).expect("write fake failing resume script");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Same shape as [`seed_resumable_run`], except the fake vendor is wired to
/// fail its resume attempt for real (see
/// [`write_fake_failing_resume_script`]) instead of succeeding.
async fn seed_resumable_run_with_failing_resume() -> ResumableScenario {
    let state_dir = tempfile::Builder::new()
        .prefix("bat-recovery-wp15-")
        .tempdir_in("/tmp")
        .expect("create state dir");
    let db = Arc::new(
        DatabaseHandle::start(state_dir.path().join("runtime.db"))
            .await
            .unwrap(),
    );
    let project_id = ProjectId::new();
    let (_task_id, _worker_id, run_id) =
        seed_run_with_profile(&db, project_id, "working", Some(claude_tui_profile_json())).await;

    let session_dir = state_dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    std::fs::write(session_dir.join(format!("{SESSION_ID}.jsonl")), "")
        .expect("write empty transcript");
    set_resume_state(&db, run_id, Some(SESSION_ID.to_string())).await;

    let script_path = write_fake_failing_resume_script(state_dir.path());
    let (registry, events_tx) = resume_registry(
        &db,
        &state_dir,
        project_id,
        &script_path,
        &session_dir,
        true,
    )
    .await;
    ResumableScenario {
        _state_dir: state_dir,
        db,
        project_id,
        registry,
        events_tx,
        run_id,
    }
}

/// Regression test for the recovery.rs:510 bug: `terminalize`'s idempotent
/// fallback re-read the run's state with `SELECT status FROM runs`, but the
/// column is `state` -- so every re-read errored, and the sweep always
/// reported `TransitionFailed`/`LeftUntouched` behind "state re-read also
/// failed: no such column: status", masking the real resume-failure root
/// cause.
///
/// This drives the actual production race deterministically, not by luck of
/// timing: the fake vendor exits nonzero with no pty output, so
/// `wait_for_readiness` fails, the adapter's own `fail_start` emits
/// `ProcessExited`, and `RunLifecycleSink` transitions the run to `failed`
/// for real -- awaited to completion inside `emit()` -- before `resume_run`
/// even returns its `Err` up to `resume_failed_fallback`. By the time
/// `resume_failed_fallback` calls `terminalize`, the run is ALREADY
/// terminal, so `terminalize`'s own `transition_run` call hits the illegal
/// failed->failed edge and must take the idempotent re-read branch, not
/// error out.
#[tokio::test]
async fn terminalize_takes_the_idempotent_branch_when_the_run_is_already_terminal() {
    let scenario = seed_resumable_run_with_failing_resume().await;
    let result = resume_coordinator(&scenario)
        .recover()
        .await
        .expect("sweep");

    assert_eq!(result.recovered_runs.len(), 1, "{result:?}");
    let recovered = &result.recovered_runs[0];
    assert_eq!(
        recovered.outcome,
        RecoveredOutcome::Terminalized,
        "the idempotent already-terminal branch must be taken, not TransitionFailed \
         (error: {:?})",
        recovered.error
    );
    assert!(recovered.success, "error: {:?}", recovered.error);
    assert!(
        recovered
            .error
            .as_deref()
            .is_none_or(|e| !e.contains("state re-read also failed")),
        "must not hit the broken re-read path: {:?}",
        recovered.error
    );
    assert_eq!(run_state(&scenario.db, scenario.run_id).await, "failed");
    assert_eq!(
        journal_count(&scenario.db, scenario.run_id, "\"code\":\"resume_failed\"").await,
        1
    );

    // M-3 rider (WP-A review): pin the idempotent branch itself, not just
    // its outcome. The doc comment above claims a specific event order --
    // `ProcessExited` lands (settling the run to `failed` for real) BEFORE
    // `terminalize`'s own `resume_failed` write hits the already-terminal
    // run and takes the idempotent re-read branch. Assert that ordering
    // directly via journal sequence, rather than only inferring it from the
    // absence of "state re-read also failed" and the correct end state --
    // either of those could stay true even if a future change reordered the
    // two writes for an unrelated reason.
    let process_exited_seq = first_journal_sequence(&scenario.db, scenario.run_id, "ProcessExited")
        .await
        .expect("a real ProcessExited must be journaled for this scenario's fake vendor exit");
    let resume_failed_seq =
        first_journal_sequence(&scenario.db, scenario.run_id, "\"code\":\"resume_failed\"")
            .await
            .expect("resume_failed must be journaled (asserted via count above)");
    assert!(
        process_exited_seq < resume_failed_seq,
        "ProcessExited (seq {process_exited_seq}) must precede resume_failed (seq \
         {resume_failed_seq}) in the journal -- that ordering is what forces terminalize's \
         transition_run call to hit the already-failed run and take the idempotent branch, \
         rather than the normal working->failed edge"
    );

    scenario.db.shutdown().await.expect("shutdown database");
}

/// No vendor session was ever established: the attempt is announced and
/// fails, `resume_failed` is journaled BEFORE the failed edge, and the run
/// takes the existing terminalize fallback.
#[tokio::test]
async fn a_missing_vendor_session_journals_resume_failed_then_fails_the_run() {
    let scenario = seed_resumable_run(true).await;
    set_resume_state(&scenario.db, scenario.run_id, None).await;
    let result = resume_coordinator(&scenario)
        .recover()
        .await
        .expect("sweep");

    assert_eq!(result.recovered_runs.len(), 1, "{result:?}");
    assert_eq!(
        result.recovered_runs[0].outcome,
        RecoveredOutcome::Terminalized
    );
    assert!(result.recovered_runs[0].success);
    assert_eq!(
        run_state(&scenario.db, scenario.run_id).await,
        "failed",
        "ineligibility falls through to the existing terminalize path"
    );
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_attempted\""
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(&scenario.db, scenario.run_id, "\"code\":\"resume_failed\"").await,
        1
    );
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_succeeded\""
        )
        .await,
        0
    );
    assert!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "no vendor session was ever established"
        )
        .await
            >= 1,
        "resume_failed names its reason"
    );
    scenario.db.shutdown().await.expect("shutdown database");
}

/// An unavailable adapter (here: no TUI support supplied to the registry at
/// all, even though session and transcript are fine) reads exactly like any
/// other failed resume.
#[tokio::test]
async fn an_unavailable_adapter_journals_resume_failed_then_fails_the_run() {
    let scenario = seed_resumable_run(false).await;
    let result = resume_coordinator(&scenario)
        .recover()
        .await
        .expect("sweep");

    assert_eq!(result.recovered_runs.len(), 1, "{result:?}");
    assert_eq!(
        result.recovered_runs[0].outcome,
        RecoveredOutcome::Terminalized
    );
    assert_eq!(run_state(&scenario.db, scenario.run_id).await, "failed");
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_attempted\""
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(&scenario.db, scenario.run_id, "\"code\":\"resume_failed\"").await,
        1
    );
    assert!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "has no TUI support in this daemon"
        )
        .await
            >= 1,
        "resume_failed names the unavailability"
    );
    scenario.db.shutdown().await.expect("shutdown database");
}

/// Runs recover independently in one mixed sweep: the resumable one
/// continues, the ineligible one terminalizes, neither blocks the other.
#[tokio::test]
async fn a_mixed_sweep_recovers_each_run_independently() {
    let scenario = seed_resumable_run(true).await;
    let (_t2, _w2, ineligible) =
        seed_run_with_profile(&scenario.db, scenario.project_id, "working", None).await;

    let result = resume_coordinator(&scenario)
        .recover()
        .await
        .expect("sweep");
    assert_eq!(result.recovered_runs.len(), 2, "{result:?}");

    let resumed = result
        .recovered_runs
        .iter()
        .find(|r| r.run_id == scenario.run_id)
        .expect("resumable run swept");
    assert_eq!(resumed.outcome, RecoveredOutcome::Resumed);

    let terminalized = result
        .recovered_runs
        .iter()
        .find(|r| r.run_id == ineligible)
        .expect("ineligible run swept");
    assert_eq!(terminalized.outcome, RecoveredOutcome::Terminalized);

    assert_eq!(run_state(&scenario.db, scenario.run_id).await, "working");
    assert_eq!(run_state(&scenario.db, ineligible).await, "failed");

    let _ = scenario
        .registry
        .running_adapter(scenario.run_id)
        .unwrap()
        .dispose()
        .await;
    scenario.db.shutdown().await.expect("shutdown database");
}

/// Idempotence: a second boot over the same journal adds nothing. The
/// resumed run is skipped because THIS process already owns its adapter;
/// the terminalized one is skipped because it is terminal; every
/// `resume_attempted` count stays at one.
#[tokio::test]
async fn a_second_boot_does_not_double_journal() {
    let scenario = seed_resumable_run(true).await;
    let (_t2, _w2, ineligible) =
        seed_run_with_profile(&scenario.db, scenario.project_id, "working", None).await;
    let coordinator = resume_coordinator(&scenario);

    let first = coordinator.recover().await.expect("first sweep");
    let second = coordinator.recover().await.expect("second sweep");
    drop(coordinator);

    assert_eq!(first.recovered_runs.len(), 2);
    assert!(
        second.recovered_runs.is_empty(),
        "a second boot must find nothing left to decide: {:?}",
        second.recovered_runs
    );
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_attempted\""
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_succeeded\""
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(&scenario.db, ineligible, "\"code\":\"resume_attempted\"").await,
        1
    );
    assert_eq!(run_state(&scenario.db, scenario.run_id).await, "working");
    assert_eq!(run_state(&scenario.db, ineligible).await, "failed");

    let _ = scenario
        .registry
        .running_adapter(scenario.run_id)
        .unwrap()
        .dispose()
        .await;
    scenario.db.shutdown().await.expect("shutdown database");
}

/// `waitingUser` runs are resume CANDIDATES even under the default config
/// -- but when the resume fails they keep today's conservative skip (never
/// terminalized), and a second boot does not re-attempt them either.
#[tokio::test]
async fn a_waiting_user_resume_failure_keeps_the_conservative_skip_and_is_not_retried() {
    let scenario = seed_resumable_run(false).await;
    // seed_resumable_run left the run in `working`; one more legal edge.
    let to = RunState::try_from("waitingUser").expect("waitingUser is a valid state");
    let run_id = scenario.run_id;
    let project_id = scenario.project_id;
    scenario
        .db
        .run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.transition_run(run_id, &to, None)
                .map(|_| serde_json::json!({}))
        }))
        .await
        .expect("drive to waitingUser");
    let coordinator = resume_coordinator(&scenario);

    let first = coordinator.recover().await.expect("first sweep");
    drop(coordinator);

    assert_eq!(first.recovered_runs.len(), 1, "{first:?}");
    assert_eq!(
        first.recovered_runs[0].outcome,
        RecoveredOutcome::LeftUntouched
    );
    assert!(!first.recovered_runs[0].success);
    assert_eq!(
        run_state(&scenario.db, scenario.run_id).await,
        "waitingUser",
        "the conservative default survives a failed waiting-run resume"
    );
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_attempted\""
        )
        .await,
        1
    );
    assert_eq!(
        journal_count(&scenario.db, scenario.run_id, "\"code\":\"resume_failed\"").await,
        1
    );

    // A second boot finds the previous failure already decided: nothing new.
    let scenario_ref = &scenario;
    let second = resume_coordinator(scenario_ref)
        .recover()
        .await
        .expect("second sweep");
    assert!(second.recovered_runs.is_empty(), "{second:?}");
    assert_eq!(
        journal_count(
            &scenario.db,
            scenario.run_id,
            "\"code\":\"resume_attempted\""
        )
        .await,
        1
    );
    assert_eq!(
        run_state(&scenario.db, scenario.run_id).await,
        "waitingUser"
    );
    scenario.db.shutdown().await.expect("shutdown database");
}
