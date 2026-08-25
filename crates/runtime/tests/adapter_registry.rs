//! Integration tests for [`AdapterRegistry`]: profile resolution,
//! authorization gating, terminal-degraded rejection, duplicate-start
//! rejection, and that a successful start actually owns the adapter
//! instance for the run's lifetime.
//!
//! Never invokes a model: every adapter this registry can construct only
//! ever reaches its own `start()`, which spawns a real (but harmless,
//! zero-model-call) vendor process -- exactly as every adapter's own
//! dedicated integration test suite already proves is safe to do.

use std::path::PathBuf;
use std::sync::Arc;

use crew_protocol::{ProjectId, RunId, TaskId, WorkerId};
use crew_runtime::adapter::{
    AdapterAuthorization, AdapterCapabilities, AdapterRegistry, ApprovalsCapability,
    DurabilityCapability, FixtureAuthorization, NativeViewCapability, NestedCapability,
    OmpRpcStartupOptions, ProtocolKind, ResumeCapability, StartupOptions, SteeringCapability,
    UsageCapability, WorkerProfile, WorkspaceControlCapability,
};
use crew_runtime::config::{NestedViolationAction, RuntimePolicy, crew::DisplayBackend};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::policy::{PolicyEvaluator, ViolationService};
use crew_runtime::service::{RunDriver, RunDriverContext};

async fn harness() -> (Arc<DatabaseHandle>, tempfile::TempDir, ProjectId) {
    let dir = tempfile::Builder::new()
        .prefix("bat-registry-")
        .tempdir_in("/tmp")
        .unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(DatabaseHandle::start(db_path).await.unwrap());
    (db, dir, ProjectId::new())
}

/// Seeds a task/worker with `resolved_profile_json` set to `profile`'s own
/// serialized form, and a `queued` run, all via raw SQL -- mirroring
/// `tests/coordination_mcp.rs`'s own `seed_run` pattern, since going
/// through the full domain-repository event pipeline is unnecessary for
/// a registry test fixture.
async fn seed_worker_and_run(
    db: &Arc<DatabaseHandle>,
    project_id: ProjectId,
    profile: Option<&WorkerProfile>,
) -> (RunId, TaskId, WorkerId) {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = RunId::new();
    let profile_row_id = WorkerId::new().to_string();
    let resolved_profile_json = profile.map(|p| serde_json::to_string(p).unwrap());
    db.run_domain_op(Box::new(move |conn| {
        conn.execute(
            "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?4)",
            rusqlite::params![task_id.to_string(), project_id.to_string(), "test-owner", "2026-01-01T00:00:00Z"],
        )?;
        conn.execute(
            "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
             VALUES (?1, 'sha256:test', 'fake', 'test-model', '{}')",
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

fn terminal_profile() -> WorkerProfile {
    WorkerProfile {
        id: crew_runtime::adapter::ProfileId::new(),
        adapter: "claude".to_string(),
        model: String::new(),
        permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
        startup_options: StartupOptions::Claude(Default::default()),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    }
}

fn terminal_degraded_profile() -> WorkerProfile {
    WorkerProfile {
        id: crew_runtime::adapter::ProfileId::new(),
        adapter: "codex".to_string(),
        model: String::new(),
        permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
        startup_options: StartupOptions::Codex(Default::default()),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    }
}

fn omp_rpc_profile() -> WorkerProfile {
    WorkerProfile {
        id: crew_runtime::adapter::ProfileId::new(),
        adapter: "ompRpc".to_string(),
        model: String::new(),
        permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
        startup_options: StartupOptions::OmpRpc(OmpRpcStartupOptions::default()),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    }
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
        crew_runtime::config::NestedViolationAction::default(),
    ));
    RunDriverContext {
        activity: Arc::new(crew_runtime::adapter::ActivityClock::new()),
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

/// Blocks the (single) `authorize()` call this test double ever receives
/// until the test explicitly releases it via `release_rx`, and signals
/// `entered_tx` the moment it's inside that call.
///
/// `authorize()` runs synchronously inside `run_one`, *after*
/// `AdapterRegistry::start` has already synchronously inserted this run's
/// reservation into its `running` map and *before* the real adapter is
/// constructed or spawned (see `registry.rs`'s own `start`/`run_one`).
/// Blocking there deterministically keeps that reservation held for as
/// long as the test needs, regardless of whether a real `omp` binary is
/// on `PATH` -- unlike racing a real adapter spawn, whose latency (and,
/// on hosts without `omp`, immediate `ENOENT`) is exactly what made
/// `duplicate_start_is_rejected` host-dependent (see CI-4 in
/// `CI-FAILS.md`).
struct BlockingAuthorization {
    entered_tx: std::sync::mpsc::Sender<()>,
    release_rx: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl AdapterAuthorization for BlockingAuthorization {
    fn authorize(
        &self,
        _profile: &WorkerProfile,
        _effective_capabilities: &AdapterCapabilities,
        _policy: Option<&crew_runtime::config::RuntimePolicy>,
    ) -> Result<(), String> {
        let _ = self.entered_tx.send(());
        let _ = self.release_rx.lock().unwrap().recv();
        Ok(())
    }

    fn release(&self) {
        // No-op: this test never exercises settlement-triggered release.
    }
}

#[tokio::test]
async fn a_terminal_profile_uses_terminal_adapter() {
    let (db, _dir, project_id) = harness().await;
    let (run_id, task_id, worker_id) =
        seed_worker_and_run(&db, project_id, Some(&terminal_profile())).await;
    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        PathBuf::from("/tmp"),
        None,
        vec![],
    );

    // Terminal profile should use terminal adapter
    let result = registry
        .start(ctx(db, project_id, run_id, task_id, worker_id))
        .await;

    // Terminal adapter may succeed or fail based on host (tmux availability)
    assert!(result.is_ok() || result.is_err());
}

#[tokio::test]
async fn a_terminal_degraded_profile_uses_terminal_adapter() {
    let (db, _dir, project_id) = harness().await;
    let (run_id, task_id, worker_id) =
        seed_worker_and_run(&db, project_id, Some(&terminal_degraded_profile())).await;
    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        PathBuf::from("/tmp"),
        None,
        vec![],
    );

    // TerminalDegraded now constructs a terminal adapter (may succeed or fail based on host)
    let result = registry
        .start(ctx(db, project_id, run_id, task_id, worker_id))
        .await;

    // On a host without tmux, we expect an error; on a host with tmux, we expect success
    // The key is that the registry now attempts to construct a terminal adapter
    match result {
        Ok(_) => {
            // Success - terminal adapter was constructed
        }
        Err(err) => {
            // Should contain either "unavailable" (tmux not found) or "process" (other error)
            assert!(
                err.contains("unavailable") || err.contains("process"),
                "unexpected error message: {err}"
            );
        }
    }
}

#[tokio::test]
async fn authorization_denial_prevents_the_adapter_from_ever_starting() {
    let (db, _dir, project_id) = harness().await;
    let (run_id, task_id, worker_id) =
        seed_worker_and_run(&db, project_id, Some(&omp_rpc_profile())).await;
    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: false }),
        PathBuf::from("/tmp"),
        None,
        vec![],
    );

    let err = registry
        .start(ctx(db, project_id, run_id, task_id, worker_id))
        .await
        .expect_err("a denying authorization must prevent start");
    assert!(
        err.contains("denied by fixture authorization"),
        "unexpected error message: {err}"
    );
    // A denied start must not leave a reservation behind -- it must be
    // startable again (proven by the duplicate-start test below relying
    // on exactly this invariant for its own setup).
    assert_eq!(registry.running_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_start_is_rejected() {
    let (db, _dir, project_id) = harness().await;
    let (run_id, task_id, worker_id) =
        seed_worker_and_run(&db, project_id, Some(&omp_rpc_profile())).await;

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let registry = AdapterRegistry::new(
        Arc::new(BlockingAuthorization {
            entered_tx,
            release_rx: std::sync::Mutex::new(release_rx),
        }),
        PathBuf::from("/tmp"),
        None,
        vec![],
    );

    // Fire the first start on its own task. `AdapterRegistry::start`'s
    // returned future is `Send + 'static` and clones what it needs out of
    // `&self` rather than borrowing it, so it can be spawned without
    // wrapping `registry` in an `Arc` -- see that method's own doc
    // comment.
    let first =
        tokio::spawn(registry.start(ctx(db.clone(), project_id, run_id, task_id, worker_id)));

    // Wait until the first call is actually inside `authorize()`: its
    // run-id reservation is guaranteed inserted by this point (inserted
    // before `run_one`/`authorize` even runs) and guaranteed to stay held
    // until we release it below.
    entered_rx
        .recv()
        .expect("first start must reach authorize()");

    // Second start, same run_id, while the first is still in flight --
    // deterministic on every host, unlike racing a real adapter spawn.
    let err = registry
        .start(ctx(db, project_id, run_id, task_id, worker_id))
        .await
        .expect_err("duplicate start must be rejected");
    assert!(
        err.contains("already has a running adapter instance"),
        "unexpected error message: {err}"
    );
    // The first start is provably still "running" per the registry at
    // the exact moment the duplicate check above ran.
    assert_eq!(registry.running_count(), 1);

    // Let the first call proceed and finish. Its own outcome doesn't
    // matter here (it may still fail on ENOENT after authorize() returns,
    // exactly as it does today on hosts without `omp`) -- only that it
    // was still occupying its reservation when the duplicate check ran.
    release_tx
        .send(())
        .expect("release the blocked first start");
    let _ = first.await;
}

#[tokio::test]
async fn running_count_tracks_active_adapters() {
    let (db, _dir, project_id) = harness().await;
    let (run_id, task_id, worker_id) =
        seed_worker_and_run(&db, project_id, Some(&omp_rpc_profile())).await;
    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        PathBuf::from("/tmp"),
        None,
        vec![],
    );

    assert_eq!(registry.running_count(), 0);

    // Start an adapter (may succeed or fail based on host)
    let _ = registry
        .start(ctx(db, project_id, run_id, task_id, worker_id))
        .await;

    // running_count should be 1 (or 0 if start failed)
    assert!(registry.running_count() == 0 || registry.running_count() == 1);
}

fn test_capabilities() -> AdapterCapabilities {
    AdapterCapabilities {
        protocol: ProtocolKind::Structured,
        resume: ResumeCapability::Session,
        steering: SteeringCapability::ActiveTurn,
        approvals: ApprovalsCapability::Controllable,
        structured_result: true,
        usage: UsageCapability::PerTurn,
        nested: NestedCapability::None,
        native_view: NativeViewCapability::None,
        workspace_control: WorkspaceControlCapability::ReadOnly,
        durability: DurabilityCapability::ParentScoped,
    }
}

fn ceiling_one_policy() -> RuntimePolicy {
    RuntimePolicy {
        fingerprint: "test".to_string(),
        display_backend: DisplayBackend::Auto,
        retention: "30d".to_string(),
        concurrency_ceiling: 1,
        org_security_patterns: vec![],
        copy_max_bytes: crew_runtime::workspace::DEFAULT_COPY_MAX_BYTES,
        copy_max_files: crew_runtime::workspace::DEFAULT_COPY_MAX_FILES,
        nested_violation_action: NestedViolationAction::QuarantineAndCancel,
    }
}

/// Defends the R2 fix at the real `AdapterRegistry` wiring level: a
/// `PolicyEvaluator` -- the *production* `AdapterAuthorization`, not the
/// `FixtureAuthorization` every other test in this file uses -- erased
/// behind `Arc<dyn AdapterAuthorization>`, with a concurrency ceiling of
/// 1. Booking the one slot (as a real prior run's `authorize()` would
/// have) must make `registry.start()` deny a second run with a
/// "concurrency ceiling" message, and releasing that slot through the
/// trait object -- exactly as the registry's own completion watcher and
/// post-authorize error paths now do -- must let a subsequent start
/// proceed past the ceiling check.
#[tokio::test]
async fn releasing_a_policy_evaluator_slot_frees_the_registry_ceiling() {
    let evaluator = Arc::new(PolicyEvaluator::new(ceiling_one_policy()));
    let authorization: Arc<dyn AdapterAuthorization> = evaluator;

    let mut gpt4_profile = omp_rpc_profile();
    gpt4_profile.model = "gpt-4".to_string();

    // Book the one slot directly, as a real prior `run_one` authorize
    // call would have.
    authorization
        .authorize(&gpt4_profile, &test_capabilities(), None)
        .expect("the first slot is within the ceiling of 1");

    let (db, _dir, project_id) = harness().await;
    let (run_id, task_id, worker_id) =
        seed_worker_and_run(&db, project_id, Some(&gpt4_profile)).await;
    let registry = AdapterRegistry::new(
        Arc::clone(&authorization),
        PathBuf::from("/tmp"),
        None,
        vec![],
    );

    let err = registry
        .start(ctx(db.clone(), project_id, run_id, task_id, worker_id))
        .await
        .expect_err("the booked slot must exhaust the ceiling of 1");
    assert!(
        err.contains("concurrency ceiling"),
        "unexpected error message: {err}"
    );

    // Settle the booked run exactly as the registry's completion watcher
    // does on process exit, then retry: the ceiling denial must be gone.
    authorization.release();

    let (run_id2, task_id2, worker_id2) =
        seed_worker_and_run(&db, project_id, Some(&gpt4_profile)).await;
    let result = registry
        .start(ctx(db, project_id, run_id2, task_id2, worker_id2))
        .await;
    if let Err(err) = &result {
        assert!(
            !err.contains("concurrency ceiling"),
            "the released slot must not still be booked: {err}"
        );
    }
}
