//! Integration tests for the evidence-driven run state machine
//! (`crates/runtime/src/adapter/run_lifecycle.rs`): a real spawned OMP-RPC
//! worker process (`fake-worker --mode rpc`) driven through the *production*
//! sink chain — `DomainAdapterEventSink` wrapped in `RunLifecycleSink` — with
//! the run's durable row observed through a real, migrated `DatabaseHandle`
//! and the real `DomainRepository`. This is the end-to-end complement of
//! `run_lifecycle.rs`'s inline unit tests, which pin the evidence→edge mapping
//! at the sink level against a stubbed inner sink: the `working` edge here can
//! only have been committed by the lifecycle sink reacting to frames the
//! normalizer produced from a live child process.
//!
//! The `fake-worker --mode rpc` alias is the grounded OMP-RPC wire shape (see
//! `batman_runtime::adapter::omp_rpc::client`'s module doc), and it answers
//! the host-tool exchange with a `None` broker, exactly as
//! `a_host_tool_call_during_the_prompt_turn_never_deadlocks_start` in
//! `omp_rpc_adapter.rs` already proves: its `prompt` response (the
//! prompt-acceptance `MessageChunk`) is the first non-exit payload the sink
//! sees, which is what walks a queued run `queued -> starting -> working`.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use batman_protocol::{
    ProjectId, Run, RunFlags, RunId, RunState, RuntimeEvent, TaskId, TaskRef, Timestamp, Worker,
    WorkerId, WorkerProfileRef,
};
use batman_runtime::adapter::{
    Adapter, DomainAdapterEventSink, OmpRpcAdapter, OmpRpcAdapterOptions, OmpRpcStartupOptions,
    ProfileId, RunLifecycleSink, StartSpec, StartupOptions, WorkerProfile,
};
use batman_runtime::config::NestedViolationAction;
use batman_runtime::db::DatabaseHandle;
use batman_runtime::domain::DomainRepository;
use batman_runtime::policy::ViolationService;
use batman_runtime::recovery::RecoveryCoordinator;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::{Instant, MissedTickBehavior};

/// The `fake-worker` binary that stands in for `omp` (see
/// `omp_rpc_adapter.rs`'s own copy — each `tests/*.rs` file is its own
/// compilation unit, so this helper is intentionally duplicated).
fn fake_worker_path() -> PathBuf {
    static PATH: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(build_fake_worker_once);
    PATH.clone()
}

fn build_fake_worker_once() -> PathBuf {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/runtime/../.. is the workspace root")
        .to_path_buf();
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--quiet", "-p", "fake-worker"])
        .current_dir(&workspace_root)
        .status()
        .expect("cargo build -p fake-worker must be runnable");
    assert!(status.success(), "cargo build -p fake-worker failed");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let binary = target_dir.join(profile_dir).join("fake-worker");
    assert!(
        binary.is_file(),
        "expected fake-worker binary at {}",
        binary.display()
    );
    binary
}

/// The same OMP-RPC worker profile `omp_rpc_adapter.rs` builds — `lm-studio/x`
/// is a deliberately inert model selector: `fake-worker` never resolves it,
/// and no local model server is required.
fn omp_rpc_test_profile() -> WorkerProfile {
    WorkerProfile {
        id: ProfileId::new(),
        adapter: "ompRpc".to_string(),
        model: "lm-studio/x".to_string(),
        permission_envelope: serde_json::json!({}),
        startup_options: StartupOptions::OmpRpc(OmpRpcStartupOptions {
            profile: None,
            host_tools: None,
        }),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    }
}

/// A real, migrated database on a throwaway file: the same pattern
/// `tests/recovery.rs`'s `open_db` uses (per-test `TempDir`, explicit
/// `shutdown` so the database actor thread never outlives the test).
async fn open_db() -> (TempDir, Arc<DatabaseHandle>) {
    let dir = tempfile::Builder::new()
        .prefix("bat-run-lifecycle-e2e-")
        .tempdir_in("/tmp")
        .expect("create temp dir");
    let db_path = dir.path().join("state.db");
    let db = Arc::new(
        DatabaseHandle::start(db_path)
            .await
            .expect("start database"),
    );
    (dir, db)
}

/// Seeds one task + worker + `queued` run through the real `DomainRepository`
/// API (the same shape as `tests/recovery.rs`'s `seed_run`), returning the
/// run's identifiers for the caller to drive further.
async fn seed_run(db: &DatabaseHandle, project_id: ProjectId) -> (TaskId, WorkerId, RunId) {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = RunId::new();
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.upsert_task(
            task_id,
            &TaskRef {
                owner_client_instance_id: "omp-1".to_string(),
                revision: 1,
            },
        )?;
        let worker = Worker {
            worker_id,
            profile_ref: WorkerProfileRef {
                id: worker_id,
                fingerprint: "sha256:fake".to_string(),
                adapter: "ompRpc".to_string(),
                model: "test".to_string(),
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
    (task_id, worker_id, run_id)
}

/// The production sink chain minus the settlement layer: `DomainAdapterEventSink`
/// (sanitize + journal + broadcast) wrapped in `RunLifecycleSink` (the evidence-driven
/// `RunState` edges under test). `SettlementSink` is deliberately absent — it only
/// observes the terminal edge this suite proves `RunLifecycleSink` commits.
fn production_sink_chain(
    db: &Arc<DatabaseHandle>,
    project_id: ProjectId,
    events_tx: broadcast::Sender<batman_protocol::EventEnvelope>,
    run_id: RunId,
) -> Arc<dyn batman_runtime::adapter::AdapterEventSink> {
    let violation = Arc::new(ViolationService::new(
        Arc::clone(db),
        project_id,
        events_tx.clone(),
        None,
        NestedViolationAction::default(),
    ));
    let domain_sink = Arc::new(
        DomainAdapterEventSink::new(
            Arc::clone(db),
            project_id,
            events_tx.clone(),
            Vec::new(),
            false,
            violation,
        )
        .expect("built-in patterns always compile"),
    );
    RunLifecycleSink::wrap(domain_sink, Arc::clone(db), project_id, events_tx, run_id)
}

/// Reads a run's current projected state directly, for assertions.
async fn run_state(db: &DatabaseHandle, run_id: RunId) -> String {
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

/// Every journaled run-state event for `run_id`, in sequence order: the
/// `state` each `RunEvent` recorded, so the exact walk the sink committed is
/// readable back out of the durable journal.
async fn run_states(db: &DatabaseHandle, run_id: RunId) -> Vec<String> {
    let raw: Vec<String> = db
        .run_domain_op(Box::new(move |conn| {
            let mut stmt =
                conn.prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
            let rows: Vec<String> = stmt
                .query_map([run_id.to_string()], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            Ok(serde_json::json!(rows))
        }))
        .await
        .expect("read journaled events")
        .as_array()
        .expect("rows are an array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_string)
        .collect();
    raw.into_iter()
        .filter_map(|raw| {
            let event: RuntimeEvent = serde_json::from_str(&raw).expect("parse a journaled event");
            match event {
                RuntimeEvent::RunEvent { state, .. } => Some(state),
                _ => None,
            }
        })
        .collect()
}

/// Whether `runs.started_at` / `runs.completed_at` is set.
async fn run_timestamp_set(db: &DatabaseHandle, run_id: RunId, column: &'static str) -> bool {
    let value = db
        .run_domain_op(Box::new(move |conn| {
            let timestamp: Option<String> = conn
                .query_row(
                    &format!("SELECT {column} FROM runs WHERE run_id = ?1"),
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            Ok(serde_json::json!(timestamp.is_some()))
        }))
        .await
        .expect("read run timestamp");
    value.as_bool().unwrap_or(false)
}

/// Runs `is_done` against a fresh read of `state` on a 50 ms tick until it
/// returns `true`, then returns that state. This is the test's only source of
/// "the run reached X": it reads the durable row, never an in-memory snapshot.
async fn poll_state(db: &DatabaseHandle, run_id: RunId, is_done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last = run_state(db, run_id).await;
    loop {
        if is_done(&last) {
            return last;
        }
        interval.tick().await;
        if Instant::now() >= deadline {
            break;
        }
        last = run_state(db, run_id).await;
    }
    panic!("run did not reach a terminal state within 10s; last observed: {last}");
}

// ----------------------------------------------------------------- tests

/// `fake-worker --mode rpc` is the grounded OMP-RPC wire shape: the run must
/// walk `queued -> starting -> working` as its *real* process emits frames —
/// `starting` from the supervised process actually spawning, `working` from
/// the first normalized payload (the prompt-acceptance `MessageChunk` the
/// normalizer builds from the fake's `prompt` response) — and no edge at all
/// may be committed by anything but the lifecycle sink reacting to that
/// evidence (a `None` broker means the `host_tool_call` bridge never produces
/// a normalized payload on its own, so the prompt response is the first
/// non-exit payload the sink sees).
#[tokio::test]
async fn a_real_worker_process_walks_its_run_from_queued_into_working() {
    let (_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
    let (events_tx, _events_rx) = broadcast::channel(64);
    let sink = production_sink_chain(&db, project_id, events_tx, run_id);

    let adapter = OmpRpcAdapter::with_binary(
        fake_worker_path().to_string_lossy().into_owned(),
        omp_rpc_test_profile(),
        OmpRpcAdapterOptions::default(),
        None,
    );
    let spec = StartSpec {
        run_id,
        task_id,
        worker_id,
        prompt: "hello".to_string(),
        resume: None,
    };
    let start = tokio::time::timeout(Duration::from_secs(5), adapter.start(spec, sink)).await;
    assert!(
        matches!(start, Ok(Ok(()))),
        "start() must succeed against the fake worker: {start:?}"
    );

    // The durable row is the only thing under test — poll it until the
    // lifecycle sink's `working` edge lands.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut state = run_state(&db, run_id).await;
    while state != "working" {
        interval.tick().await;
        assert!(
            Instant::now() < deadline,
            "run never reached working; last observed state: {state}"
        );
        state = run_state(&db, run_id).await;
    }

    assert_eq!(
        run_states(&db, run_id).await,
        vec![
            "queued".to_string(),
            "starting".to_string(),
            "working".to_string(),
        ],
        "the walked edges must be exactly queued -> starting -> working"
    );

    adapter.dispose().await.expect("dispose the fake worker");
    db.shutdown().await.expect("shutdown database");
}

/// Once the run is `working`, disposing the adapter must terminalize the run
/// (exactly one terminal `RunEvent`, whatever terminal state the supervisor
/// observed — the mapping itself is pinned by `run_lifecycle.rs`'s unit
/// tests, not re-pinned here). And the `RecoveryCoordinator`'s startup sweep
/// must leave it alone: a run the state machine already terminalized is
/// recovered by no one.
#[tokio::test]
async fn a_real_worker_process_exit_settles_its_run() {
    let (dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
    let (events_tx, _events_rx) = broadcast::channel(64);
    let sink = production_sink_chain(&db, project_id, events_tx, run_id);

    let adapter = OmpRpcAdapter::with_binary(
        fake_worker_path().to_string_lossy().into_owned(),
        omp_rpc_test_profile(),
        OmpRpcAdapterOptions::default(),
        None,
    );
    let spec = StartSpec {
        run_id,
        task_id,
        worker_id,
        prompt: "hello".to_string(),
        resume: None,
    };
    let start = tokio::time::timeout(Duration::from_secs(5), adapter.start(spec, sink)).await;
    assert!(
        matches!(start, Ok(Ok(()))),
        "start() must succeed against the fake worker: {start:?}"
    );

    // Wait for the `working` edge, then kill the process.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut state = run_state(&db, run_id).await;
    while state != "working" {
        interval.tick().await;
        assert!(
            Instant::now() < deadline,
            "run never reached working; last observed state: {state}"
        );
        state = run_state(&db, run_id).await;
    }

    adapter.dispose().await.expect("dispose the fake worker");

    let final_state = poll_state(&db, run_id, |s| {
        RunState::try_from(s).is_ok_and(|r| r.is_terminal())
    })
    .await;
    assert!(
        RunState::try_from(final_state.as_str()).is_ok_and(|r| r.is_terminal()),
        "the run must read a terminal state after dispose, got {final_state}"
    );

    let states = run_states(&db, run_id).await;
    let terminals: Vec<&String> = states
        .iter()
        .filter(|s| RunState::try_from(s.as_str()).is_ok_and(|r| r.is_terminal()))
        .collect();
    assert_eq!(
        terminals.len(),
        1,
        "exactly one terminal RunEvent may be appended; journaled states: {states:?}"
    );
    assert!(
        run_timestamp_set(&db, run_id, "completed_at").await,
        "a terminal edge stamps runs.completed_at"
    );

    // The startup sweep must not touch a run the state machine already
    // terminalized (R51's sweep takes every *non-terminal* run).
    let coordinator = RecoveryCoordinator::with_defaults(Arc::clone(&db), project_id);
    let result = coordinator.recover().await.expect("recovery must succeed");
    assert!(
        result.recovered_runs.is_empty(),
        "the recovery sweep must leave a terminalized run untouched: {result:?}"
    );
    assert_eq!(
        run_state(&db, run_id).await,
        final_state,
        "the sweep must not change an already-terminal run"
    );

    db.shutdown().await.expect("shutdown database");
    drop(dir);
}
