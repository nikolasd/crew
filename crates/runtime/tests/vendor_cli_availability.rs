//! The availability probe's two contracts: a run whose vendor CLI is not
//! installed is denied at authorization time (never at spawn time), and
//! `CREW_DISABLE_VENDOR_CLI` stays *permissive* rather than turning into
//! a denial.
//!
//! This file deliberately contains exactly **one** test. Both contracts
//! are decided by process-global state -- the `CREW_DISABLE_VENDOR_CLI`
//! variable and `PATH` -- which `std::env::set_var` may only mutate
//! soundly when no other thread is running (edition 2024 makes it
//! `unsafe` for precisely this reason). `cargo test` gives every
//! integration-test file its own binary and runs `#[test]` functions in
//! that binary concurrently, so a single test function is the only shape
//! that makes the mutation sound. The three phases below are therefore
//! sequenced inside one function rather than split into three tests.
//!
//! Never invokes a model: the probe is a `--version` handshake only.

use std::path::PathBuf;
use std::sync::Arc;

use batman_protocol::{ProjectId, RunId, TaskId, WorkerId};
use batman_runtime::adapter::{
    AdapterKind, AdapterRegistry, ClaudeStartupOptions, FixtureAuthorization, StartupOptions,
    WorkerProfile,
};
use batman_runtime::conformance::{DISABLE_VENDOR_CLI_ENV, probe_availability};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::policy::ViolationService;
use batman_runtime::service::{RunDriver, RunDriverContext};

/// A Claude profile. Claude is probed by resolving `claude` on `PATH`, so
/// emptying `PATH` is what makes its CLI "not installed" for the probe --
/// `WorkerProfile` carries no binary-path field to point at a missing
/// file.
fn claude_profile() -> WorkerProfile {
    WorkerProfile {
        id: batman_runtime::adapter::ProfileId::new(),
        adapter: "claude".to_string(),
        model: "sonnet".to_string(),
        permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
        startup_options: StartupOptions::Claude(ClaudeStartupOptions::default()),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    }
}

async fn seed_worker_and_run(
    db: &Arc<DatabaseHandle>,
    project_id: ProjectId,
    profile: &WorkerProfile,
) -> (RunId, TaskId, WorkerId) {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = RunId::new();
    let profile_row_id = WorkerId::new().to_string();
    let resolved_profile_json = serde_json::to_string(profile).unwrap();
    db.run_domain_op(Box::new(move |conn| {
        conn.execute(
            "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?4)",
            rusqlite::params![task_id.to_string(), project_id.to_string(), "test-owner", "2026-01-01T00:00:00Z"],
        )?;
        conn.execute(
            "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
             VALUES (?1, 'sha256:test', 'claude', 'sonnet', '{}')",
            rusqlite::params![profile_row_id],
        )?;
        conn.execute(
            "INSERT INTO workers (worker_id, project_id, profile_id, resolved_profile_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                worker_id.to_string(),
                project_id.to_string(),
                profile_row_id,
                resolved_profile_json,
                "2026-01-01T00:00:00Z",
            ],
        )?;
        conn.execute(
            "INSERT INTO runs (run_id, task_id, worker_id, state, created_at)
             VALUES (?1, ?2, ?3, 'queued', ?4)",
            rusqlite::params![run_id.to_string(), task_id.to_string(), worker_id.to_string(), "2026-01-01T00:00:00Z"],
        )?;
        Ok(serde_json::Value::Null)
    }))
    .await
    .unwrap();
    (run_id, task_id, worker_id)
}

fn ctx(
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
) -> RunDriverContext {
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(100);
    let violation_service = Arc::new(ViolationService::new(
        Arc::clone(&db),
        project_id,
        events_tx.clone(),
        None,
        batman_runtime::config::NestedViolationAction::default(),
    ));
    RunDriverContext {
        db,
        project_id,
        run_id,
        task_id,
        worker_id,
        events_tx,
        prompt: None,
        violation_service,
        workspace_path: None,
        policy: None,
        display: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_uninstalled_vendor_cli_is_denied_at_authorization_and_the_kill_switch_stays_permissive()
{
    // --- Phase 1: the kill switch is permissive, and is not cached ----
    //
    // Codex carries this phase because phases 2 and 3 use Claude, and the
    // probe caches per adapter kind for 60s -- reusing one kind across
    // phases would let a cached verdict answer a later phase.
    //
    // SAFETY: this binary holds exactly one test (see the module doc), and
    // `current_thread` keeps its async work on this same thread, so no
    // other thread can observe the environment mid-mutation.
    unsafe { std::env::set_var(DISABLE_VENDOR_CLI_ENV, "1") };

    let skipped = probe_availability(AdapterKind::Codex).await;
    assert!(
        skipped.was_skipped(),
        "the switch must SKIP rather than deny: a denial would make every run in \
         CI unauthorized, and a pass would fabricate proof, detail was {:?}",
        skipped.detail
    );
    assert!(
        skipped.detail.contains(DISABLE_VENDOR_CLI_ENV),
        "the skip must say why it skipped: {:?}",
        skipped.detail
    );

    // Clearing the switch must produce a real observation rather than the
    // cached skip -- the skip is deliberately never cached, so that
    // installing a CLI (or unsetting the switch) is picked up without
    // restarting the daemon.
    unsafe { std::env::remove_var(DISABLE_VENDOR_CLI_ENV) };
    let observed = probe_availability(AdapterKind::Codex).await;
    assert!(
        !observed.detail.contains(DISABLE_VENDOR_CLI_ENV),
        "a skipped probe must not be cached; got the skip again: {:?}",
        observed.detail
    );

    // --- Phase 2: the probe actually observes the machine -------------
    //
    // `PATH` is emptied, so `claude` cannot be resolved. A probe that
    // reported success here would be reporting the fixture's declared
    // capabilities as though they were the installed binary's.
    let real_path = std::env::var("PATH").unwrap_or_default();
    unsafe { std::env::set_var("PATH", "") };

    let unavailable = probe_availability(AdapterKind::Claude).await;
    assert!(
        !unavailable.proved(),
        "an unresolvable vendor CLI must fail the probe: {:?}",
        unavailable.detail
    );

    // --- Phase 3: run_one denies before spawning anything -------------
    let dir = tempfile::Builder::new()
        .prefix("bat-avail-")
        .tempdir_in("/tmp")
        .unwrap();
    let db = Arc::new(
        DatabaseHandle::start(dir.path().join("state.db"))
            .await
            .unwrap(),
    );
    let project_id = ProjectId::new();
    let profile = claude_profile();
    let (run_id, task_id, worker_id) = seed_worker_and_run(&db, project_id, &profile).await;

    // `allow: true` isolates the probe as the cause: policy permits this
    // run, so a denial can only have come from availability.
    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        PathBuf::from("/tmp"),
        None,
        vec![],
    );

    let err = registry
        .start(ctx(db, project_id, run_id, task_id, worker_id))
        .await
        .expect_err("an unavailable vendor CLI must prevent start");

    unsafe { std::env::set_var("PATH", &real_path) };

    assert!(
        err.contains("adapter claude is unavailable"),
        "expected an availability denial naming the adapter, got: {err}"
    );
    // The denial happened at authorization time, so no adapter was ever
    // constructed and no reservation survives.
    assert_eq!(
        registry.running_count(),
        0,
        "a probe denial must not leave a started adapter behind"
    );
}
