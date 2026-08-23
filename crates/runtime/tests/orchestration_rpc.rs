//! Integration tests for the orchestration JSON-RPC methods.
//!
//! Drives the real [`batman_runtime::ipc::Server`] over a Unix domain
//! socket, exercising `task/upsert|get`, `worker/create|list|get`,
//! `run/submit|list|get|retry|cancel`, `message/send|list`,
//! `approval/list|decide`, and `reconcile/omp`.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use batman_protocol::{ProjectId, RunId, TaskId, WorkerId};
use batman_runtime::adapter::{
    Adapter, AdapterEvent, AdapterEventPayload, AdapterEventSink, CancelScope, OmpRpcAdapter,
    OmpRpcAdapterOptions, OmpRpcStartupOptions, ProfileId, StartSpec, StartupOptions,
    WorkerProfile,
};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::domain::DomainRepository;
use batman_runtime::ipc::{PeerCredentialReader, PeerCredentials, Server, ServerConfig};
use batman_runtime::paths::RuntimePaths;
use batman_runtime::service::{AdapterFuture, FakeRunDriver, RunDriver, RunDriverContext};
use batman_runtime::workspace::ArtifactStore;
use nix::unistd::Uid;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// ------------------------------------------------------------------ fakes

struct FakeReader {
    uid: Option<u32>,
}

impl PeerCredentialReader for FakeReader {
    fn read(&self, _stream: &UnixStream) -> PeerCredentials {
        PeerCredentials {
            uid: self.uid,
            pid: Some(4242),
        }
    }
}

fn current_uid() -> u32 {
    Uid::current().as_raw()
}

fn matching_reader() -> Arc<dyn PeerCredentialReader> {
    Arc::new(FakeReader {
        uid: Some(current_uid()),
    })
}

/// A [`RunDriver`] that records every `send_follow_up` call verbatim and
/// always succeeds, so tests can assert exactly what a driver-backed run
/// received without needing a real (or fake-but-opinionated) adapter.
/// `start` never transitions run state -- these tests only exercise
/// `message/send`, not run lifecycle.
#[derive(Default)]
struct RecordingRunDriver {
    follow_ups: parking_lot::Mutex<Vec<(RunId, TaskId, WorkerId, String)>>,
}

impl RunDriver for RecordingRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, _ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn send_follow_up(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        self.follow_ups
            .lock()
            .push((run_id, task_id, worker_id, prompt));
        Box::pin(async { Ok(()) })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
        None
    }

    fn cancel_run(
        &self,
        _run_id: RunId,
        _scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        Box::pin(async { Ok(batman_runtime::service::CancelOutcome::Cancelled) })
    }
}

/// A [`RunDriver`] whose `start` always fails, so tests can exercise the
/// lease-and-workspace cleanup path that runs when a run never actually
/// starts (the row was already fully allocated -- and, for isolated modes,
/// materialized -- before this call).
#[derive(Default)]
struct FailingRunDriver;

impl RunDriver for FailingRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, _ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Err("boom: adapter never came up".to_string()) })
    }

    fn send_follow_up(
        &self,
        _run_id: RunId,
        _task_id: TaskId,
        _worker_id: WorkerId,
        _prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
        None
    }

    fn cancel_run(
        &self,
        _run_id: RunId,
        _scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        Box::pin(async { Ok(batman_runtime::service::CancelOutcome::Cancelled) })
    }
}

/// Turns `dir` into a real git repository with one commit, so `gitWorktree`
/// isolation can actually materialize (`git worktree add` needs a base
/// commit to check out). `Harness::start` only creates an empty `.git`
/// directory, which is enough to make `git rev-parse HEAD` fail --
/// sufficient for materialize-failure tests, but not for a test that needs
/// materialization to *succeed* so a later failure (e.g. `driver.start`)
/// can be isolated on its own.
fn init_real_git_repo(dir: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test User"],
    ] {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(&args)
            .status()
            .expect("git command runs");
        assert!(status.success(), "git {args:?} failed");
    }
    std::fs::write(dir.join("README.md"), "# test\n").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-q", "-m", "init"]] {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(&args)
            .status()
            .expect("git command runs");
        assert!(status.success(), "git {args:?} failed");
    }
}
/// Tracks whether cancel_run was called and with what scope.
/// Used to verify item 33's wiring: OrchestrationService calls cancel_run on the live adapter.
#[derive(Default)]
struct CancelTrackingRunDriver {
    cancel_calls: parking_lot::Mutex<Vec<(RunId, CancelScope)>>,
    follow_ups: parking_lot::Mutex<Vec<(RunId, TaskId, WorkerId, String)>>,
}

impl CancelTrackingRunDriver {
    fn cancel_calls(&self) -> Vec<(RunId, CancelScope)> {
        self.cancel_calls.lock().clone()
    }
}

impl RunDriver for CancelTrackingRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, _ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn send_follow_up(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        self.follow_ups
            .lock()
            .push((run_id, task_id, worker_id, prompt));
        Box::pin(async { Ok(()) })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
        None
    }

    fn cancel_run(
        &self,
        run_id: RunId,
        scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        self.cancel_calls.lock().push((run_id, scope));
        Box::pin(async { Ok(batman_runtime::service::CancelOutcome::Cancelled) })
    }
}

#[tokio::test]
async fn run_cancel_calls_adapter_cancel_run_with_worker_scope() {
    let driver = Arc::new(CancelTrackingRunDriver::default());

    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    // Create a task, worker, and run
    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();

    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "run/submit failed: {submit:?}"
    );

    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    // Now cancel the run
    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "run/cancel failed: {cancel:?}"
    );

    // Verify cancel_run was called with CancelScope::Worker
    let calls = driver.cancel_calls();
    assert_eq!(calls.len(), 1, "expected exactly one cancel_run call");
    let (called_run_id, scope) = &calls[0];
    assert_eq!(
        called_run_id.to_string(),
        run_id,
        "cancel_run called with wrong run_id"
    );
    assert_eq!(
        *scope,
        CancelScope::Worker,
        "cancel_run called with wrong CancelScope"
    );
}

// ------------------------------------------------------- policy violation

/// Simulates what a real adapter/`AdapterRegistry` does when its
/// effective `nested` capability is not `Managed`: emits a
/// `NestedWorkerObserved` event through a real
/// [`batman_runtime::adapter::DomainAdapterEventSink`] constructed with
/// `nested_not_managed: true`, exercising the full
/// `ViolationService::record` pipeline (Hardening plan Task 1) without
/// spawning any vendor process. Captures its own `RunDriverContext` so a
/// test can trigger a *second* `NestedWorkerObserved` later (for the
/// idempotency case), reusing the same `db`/`events_tx`/`violation_service`
/// a real second observation on the same run would.
#[derive(Default)]
struct ViolationTriggeringRunDriver {
    cancel_calls: parking_lot::Mutex<Vec<(RunId, CancelScope)>>,
    captured: parking_lot::Mutex<Option<RunDriverContext>>,
}

impl ViolationTriggeringRunDriver {
    fn cancel_calls(&self) -> Vec<(RunId, CancelScope)> {
        self.cancel_calls.lock().clone()
    }

    async fn emit_nested_worker_observed(
        &self,
        vendor_child_id: &str,
        vendor_parent_ref: &str,
    ) -> Result<u64, String> {
        let ctx = self
            .captured
            .lock()
            .clone()
            .expect("start() must run before emit_nested_worker_observed");
        let sink = batman_runtime::adapter::DomainAdapterEventSink::new(
            ctx.db.clone(),
            ctx.project_id,
            ctx.events_tx.clone(),
            vec![],
            true,
            Arc::clone(&ctx.violation_service),
            None,
        )
        .expect("built-in patterns always compile");
        sink.emit(AdapterEvent {
            run_id: ctx.run_id,
            task_id: ctx.task_id,
            worker_id: ctx.worker_id,
            payload: AdapterEventPayload::NestedWorkerObserved {
                vendor_child_id: vendor_child_id.to_string(),
                vendor_parent_ref: vendor_parent_ref.to_string(),
            },
        })
        .await
        .map_err(|e| e.to_string())
    }
}

impl RunDriver for ViolationTriggeringRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        *self.captured.lock() = Some(ctx.clone());
        Box::pin(async move {
            let sink = batman_runtime::adapter::DomainAdapterEventSink::new(
                ctx.db.clone(),
                ctx.project_id,
                ctx.events_tx.clone(),
                vec![],
                true,
                Arc::clone(&ctx.violation_service),
                None,
            )
            .expect("built-in patterns always compile");
            sink.emit(AdapterEvent {
                run_id: ctx.run_id,
                task_id: ctx.task_id,
                worker_id: ctx.worker_id,
                payload: AdapterEventPayload::NestedWorkerObserved {
                    vendor_child_id: "child-vendor-1".to_string(),
                    vendor_parent_ref: "parent-vendor-1".to_string(),
                },
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn send_follow_up(
        &self,
        _run_id: RunId,
        _task_id: TaskId,
        _worker_id: WorkerId,
        _prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
        None
    }

    fn cancel_run(
        &self,
        run_id: RunId,
        scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        self.cancel_calls.lock().push((run_id, scope));
        Box::pin(async { Ok(batman_runtime::service::CancelOutcome::Cancelled) })
    }
}

/// Like [`ViolationTriggeringRunDriver`], but its `cancel_run` outcome is
/// configurable: `Err` simulates a real kill failure, `Ok(NoRunningAdapter)`
/// simulates a run whose adapter already exited (R13).
struct ConfigurableCancelViolationDriver {
    outcome: Result<batman_runtime::service::CancelOutcome, String>,
}

impl RunDriver for ConfigurableCancelViolationDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async move {
            let sink = batman_runtime::adapter::DomainAdapterEventSink::new(
                ctx.db.clone(),
                ctx.project_id,
                ctx.events_tx.clone(),
                vec![],
                true,
                Arc::clone(&ctx.violation_service),
                None,
            )
            .expect("built-in patterns always compile");
            sink.emit(AdapterEvent {
                run_id: ctx.run_id,
                task_id: ctx.task_id,
                worker_id: ctx.worker_id,
                payload: AdapterEventPayload::NestedWorkerObserved {
                    vendor_child_id: "child-vendor-1".to_string(),
                    vendor_parent_ref: "parent-vendor-1".to_string(),
                },
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn send_follow_up(
        &self,
        _run_id: RunId,
        _task_id: TaskId,
        _worker_id: WorkerId,
        _prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
        None
    }

    fn cancel_run(
        &self,
        _run_id: RunId,
        _scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        let outcome = self.outcome.clone();
        Box::pin(async move { outcome })
    }
}

/// R13: a policy cancellation whose adapter kill actually FAILS must not
/// record a clean success -- the run is journaled `cancelled` (the intent
/// stands) but `degradedControl` must be raised so `run/get` and the
/// monitor see that the control plane could not act on this run.
#[tokio::test]
async fn a_failed_policy_kill_raises_degraded_control() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(ConfigurableCancelViolationDriver {
            outcome: Err("kill failed: permission denied".to_string()),
        }) as Arc<dyn RunDriver>);
        // Default action is QuarantineAndCancel.
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let (_, _, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    let get = client.call(5, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(get["result"]["state"], "cancelled", "{get:?}");
    assert_eq!(
        get["result"]["flags"]["degradedControl"], true,
        "a failed kill must raise degradedControl: {get:?}"
    );
}

/// R13's counter-case: an absent adapter is a clean outcome, not a kill
/// failure -- `degradedControl` must stay false.
#[tokio::test]
async fn a_policy_cancel_with_no_running_adapter_is_clean() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(ConfigurableCancelViolationDriver {
            outcome: Ok(batman_runtime::service::CancelOutcome::NoRunningAdapter),
        }) as Arc<dyn RunDriver>);
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let (_, _, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    let get = client.call(5, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(get["result"]["state"], "cancelled", "{get:?}");
    assert_eq!(
        get["result"]["flags"]["degradedControl"], false,
        "an absent adapter is not a kill failure: {get:?}"
    );
}

/// Submits a task/worker/run with `driver` as the injected `RunDriver`
/// (which emits the first `NestedWorkerObserved` from inside `start()`),
/// returning `(task_id, worker_id, run_id)`.
async fn submit_run_with_driver(client: &mut Client, owner: &str) -> (String, String, String) {
    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": owner, "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "run/submit failed: {submit:?}"
    );
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();
    (task_id, worker_id, run_id)
}

#[tokio::test]
async fn nested_worker_observed_quarantines_run_and_blocks_message_send_until_released() {
    let driver = Arc::new(ViolationTriggeringRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
        c.nested_violation_action = batman_runtime::config::NestedViolationAction::Quarantine;
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let (task_id, worker_id, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    // The pure-Quarantine action never cancels: the run stays non-terminal.
    let get = client.call(5, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get["result"]["flags"]["policyQuarantined"], true,
        "run must be quarantined after NestedWorkerObserved: {get:?}"
    );
    assert_ne!(
        get["result"]["state"], "cancelled",
        "Quarantine-only action must never cancel the run"
    );
    assert!(
        driver.cancel_calls().is_empty(),
        "Quarantine-only action must never call cancel_run"
    );

    // message/send is blocked while quarantined.
    let blocked_send = client
        .call(
            6,
            "message/send",
            json!({
                "runId": run_id,
                "senderWorkerId": worker_id,
                "taskId": task_id,
                "kind": "question",
                "payload": "should be blocked"
            }),
        )
        .await;
    assert_eq!(
        blocked_send["error"]["code"], -32101,
        "expected POLICY_QUARANTINED: {blocked_send:?}"
    );

    // Find the recorded violation's id from the replayed event.
    let replay = client
        .call(7, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let events = replay["result"].as_array().unwrap();
    let recorded = events
        .iter()
        .find(|e| e["event"]["type"] == "policyViolationRecorded")
        .expect("a policyViolationRecorded event must be journaled");
    let violation_id =
        recorded["event"]["payload"]["kind"]["policyViolationRecorded"]["violation_id"]
            .as_str()
            .expect("violation_id must be present on the recorded event")
            .to_string();

    // The owning client releases the quarantine.
    let decide = client
        .call(
            8,
            "policy/violation/decide",
            json!({ "violationId": violation_id, "resolution": "release" }),
        )
        .await;
    assert!(decide.get("error").is_none(), "decide failed: {decide:?}");
    assert_eq!(decide["result"]["outcome"], "decided");

    let get_after = client.call(9, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get_after["result"]["flags"]["policyQuarantined"], false,
        "release must clear the quarantine flag: {get_after:?}"
    );

    // message/send now succeeds.
    let unblocked_send = client
        .call(
            10,
            "message/send",
            json!({
                "runId": run_id,
                "senderWorkerId": worker_id,
                "taskId": task_id,
                "kind": "question",
                "payload": "should now succeed"
            }),
        )
        .await;
    assert!(
        unblocked_send.get("error").is_none(),
        "message/send must succeed once released: {unblocked_send:?}"
    );
}

#[tokio::test]
async fn policy_violation_decide_is_forbidden_for_a_non_owning_client() {
    let driver = Arc::new(ViolationTriggeringRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
        c.nested_violation_action = batman_runtime::config::NestedViolationAction::Quarantine;
    })
    .await;
    let mut owner_client = omp_client(&harness, "omp-owner").await;
    let (_, _, run_id) = submit_run_with_driver(&mut owner_client, "omp-owner").await;

    let replay = owner_client
        .call(5, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let events = replay["result"].as_array().unwrap();
    let recorded = events
        .iter()
        .find(|e| e["event"]["type"] == "policyViolationRecorded")
        .expect("a policyViolationRecorded event must be journaled");
    let violation_id =
        recorded["event"]["payload"]["kind"]["policyViolationRecorded"]["violation_id"]
            .as_str()
            .unwrap()
            .to_string();

    let mut other_client = omp_client(&harness, "omp-other").await;
    let decide = other_client
        .call(
            2,
            "policy/violation/decide",
            json!({ "violationId": violation_id, "resolution": "release" }),
        )
        .await;
    assert_eq!(
        decide["error"]["code"], -32602,
        "a non-owning client must be rejected: {decide:?}"
    );

    // The quarantine must remain untouched by the rejected attempt.
    let get = owner_client
        .call(6, "run/get", json!({ "runId": run_id }))
        .await;
    assert_eq!(get["result"]["flags"]["policyQuarantined"], true);
}

#[tokio::test]
async fn policy_violation_decide_release_is_refused_on_an_already_terminal_run() {
    let driver = Arc::new(ViolationTriggeringRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
        // Default action is QuarantineAndCancel: the violation itself
        // cancels the run.
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let (_, _, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    let get = client.call(5, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get["result"]["state"], "cancelled",
        "QuarantineAndCancel must cancel the run: {get:?}"
    );
    assert_eq!(
        driver.cancel_calls().len(),
        1,
        "cancel_run must be called exactly once"
    );

    let replay = client
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let events = replay["result"].as_array().unwrap();
    let recorded = events
        .iter()
        .find(|e| e["event"]["type"] == "policyViolationRecorded")
        .expect("a policyViolationRecorded event must be journaled");
    let violation_id =
        recorded["event"]["payload"]["kind"]["policyViolationRecorded"]["violation_id"]
            .as_str()
            .unwrap()
            .to_string();

    // Releasing quarantine on an already-terminal (cancelled) run must
    // never revive it.
    let decide = client
        .call(
            7,
            "policy/violation/decide",
            json!({ "violationId": violation_id, "resolution": "release" }),
        )
        .await;
    assert!(
        decide.get("error").is_some(),
        "releasing quarantine on a terminal run must be refused: {decide:?}"
    );

    let get_after = client.call(8, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get_after["result"]["state"], "cancelled",
        "the refused release must never revive the run"
    );
}
// ------------------------------------------------------------------ cost ceiling

/// Simulates a run whose adapter reports usage crossing the merged policy's
/// per-run cost ceiling: emits one `UsageReported` through a real
/// [`batman_runtime::adapter::DomainAdapterEventSink`] built with
/// `nested_not_managed: false` (so only the ceiling can trigger a violation)
/// and `cost_ceiling_per_run_usd: Some(1.0)`, exercising the whole
/// `ViolationService::record_cost_ceiling` pipeline with no vendor process.
#[derive(Default)]
struct CostCeilingRunDriver {
    cancel_calls: parking_lot::Mutex<Vec<(RunId, CancelScope)>>,
    captured: parking_lot::Mutex<Option<RunDriverContext>>,
}

impl CostCeilingRunDriver {
    fn cancel_calls(&self) -> Vec<(RunId, CancelScope)> {
        self.cancel_calls.lock().clone()
    }
}

impl RunDriver for CostCeilingRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        *self.captured.lock() = Some(ctx.clone());
        Box::pin(async move {
            let sink = batman_runtime::adapter::DomainAdapterEventSink::new(
                ctx.db.clone(),
                ctx.project_id,
                ctx.events_tx.clone(),
                vec![],
                false,
                Arc::clone(&ctx.violation_service),
                Some(1.0),
            )
            .expect("built-in patterns always compile");
            sink.emit(AdapterEvent {
                run_id: ctx.run_id,
                task_id: ctx.task_id,
                worker_id: ctx.worker_id,
                payload: AdapterEventPayload::UsageReported {
                    input_tokens: 1_000,
                    output_tokens: 2_000,
                    cost_usd: Some(2.5),
                },
            })
            .await
            .map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn send_follow_up(
        &self,
        _run_id: RunId,
        _task_id: TaskId,
        _worker_id: WorkerId,
        _prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
        None
    }

    fn cancel_run(
        &self,
        run_id: RunId,
        scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        self.cancel_calls.lock().push((run_id, scope));
        Box::pin(async { Ok(batman_runtime::service::CancelOutcome::Cancelled) })
    }
}

/// A run whose adapter reports $2.50 against a $1.00 per-run ceiling
/// records a `cost_ceiling_exceeded` violation, quarantines the run,
/// and allows the owning client to release the quarantine via
/// `policy/violation/decide` — proving the full `record_cost_ceiling`
/// pipeline (R48).
#[tokio::test]
async fn crossing_the_per_run_cost_ceiling_records_an_actionable_violation() {
    let driver = Arc::new(CostCeilingRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
        c.nested_violation_action = batman_runtime::config::NestedViolationAction::Quarantine;
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let (_, _, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    // The pure-Quarantine action never cancels: the run stays non-terminal.
    let get = client.call(5, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get["result"]["flags"]["policyQuarantined"], true,
        "run must be quarantined after cost ceiling breach: {get:?}"
    );
    assert_ne!(
        get["result"]["state"], "cancelled",
        "Quarantine-only action must never cancel the run"
    );
    assert!(
        driver.cancel_calls().is_empty(),
        "Quarantine-only action must never call cancel_run"
    );

    // Find the recorded cost-ceiling violation from the replayed events.
    let replay = client
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let events = replay["result"].as_array().unwrap();
    let recorded = events
        .iter()
        .find(|e| e["event"]["type"] == "policyViolationRecorded")
        .expect("a policyViolationRecorded event must be journaled");
    let kind = &recorded["event"]["payload"]["kind"]["policyViolationRecorded"];
    assert_eq!(
        kind["code"], "cost_ceiling_exceeded",
        "the recorded violation must carry the cost ceiling code: {kind:?}"
    );
    assert!(
        kind["vendor_child_id"].is_null(),
        "a cost-ceiling violation has no vendor child: {kind:?}"
    );
    assert!(
        kind["vendor_parent_ref"].is_null(),
        "a cost-ceiling violation has no vendor parent: {kind:?}"
    );
    let violation_id = kind["violation_id"]
        .as_str()
        .expect("violation_id must be present")
        .to_string();

    // The owning client releases the quarantine — this step reads the
    // policy_violations projection row, so it proves the row was persisted.
    let decide = client
        .call(
            7,
            "policy/violation/decide",
            json!({ "violationId": violation_id, "resolution": "release" }),
        )
        .await;
    assert!(decide.get("error").is_none(), "decide failed: {decide:?}");
    assert_eq!(decide["result"]["outcome"], "decided");

    let get_after = client.call(8, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get_after["result"]["flags"]["policyQuarantined"], false,
        "release must clear the quarantine flag: {get_after:?}"
    );
}

/// A run's `policyFingerprint` is an immutable snapshot of the merge it
/// was authorized under: a later run carrying `policyOverrides` gets its
/// own fingerprint and never rewrites an existing run's.
#[tokio::test]
async fn per_run_policy_overrides_snapshot_only_their_own_run() {
    let org = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(org.path(), "max_workers: 8\n").unwrap();
    let layers = Arc::new(
        batman_runtime::config::LayeredConfig::load(Some(org.path()), None, None).unwrap(),
    );
    let startup = Arc::new(layers.merge(None).unwrap());
    let startup_fingerprint = startup.fingerprint.clone();

    let harness = Harness::start({
        let layers = Arc::clone(&layers);
        let startup = Arc::clone(&startup);
        move |c| {
            c.run_driver = Some(Arc::new(FakeRunDriver) as Arc<dyn RunDriver>);
            c.policy = Some((layers, startup));
        }
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let plain = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    let plain_run = plain["result"]["runId"].as_str().unwrap().to_string();

    let overridden = client
        .call(
            5,
            "run/submit",
            json!({
                "taskId": task_id,
                "workerId": worker_id,
                "policyOverrides": { "max_workers": 2 },
            }),
        )
        .await;
    assert!(
        overridden.get("error").is_none(),
        "an override that violates no lock must be accepted: {overridden:?}"
    );
    let overridden_run = overridden["result"]["runId"].as_str().unwrap().to_string();

    let plain_get = client
        .call(6, "run/get", json!({ "runId": plain_run }))
        .await;
    let overridden_get = client
        .call(7, "run/get", json!({ "runId": overridden_run }))
        .await;

    assert_eq!(
        plain_get["result"]["policyFingerprint"], startup_fingerprint,
        "a run without overrides is snapshotted under the startup merge"
    );
    let overridden_fingerprint = overridden_get["result"]["policyFingerprint"]
        .as_str()
        .expect("an overridden run must carry its own fingerprint");
    assert_ne!(
        overridden_fingerprint, startup_fingerprint,
        "an override must produce a distinct merge fingerprint"
    );

    // The snapshot is immutable: the second submit did not rewrite the
    // first run's row.
    let plain_again = client
        .call(8, "run/get", json!({ "runId": plain_run }))
        .await;
    assert_eq!(
        plain_again["result"]["policyFingerprint"], startup_fingerprint,
        "one run's overrides must never change another run's snapshot"
    );
}

/// A run whose display preference resolves to an available backend
/// journals exactly one `DisplayPaneAttached`, and reports the winning
/// backend on the submit response so the caller needs no second call.
#[tokio::test]
async fn run_submit_journals_the_display_pane_it_attached() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver) as Arc<dyn RunDriver>);
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    // `terminal` is the one backend that is always available, so this
    // resolves identically on a developer machine and in headless CI.
    let submit = client
        .call(
            4,
            "run/submit",
            json!({
                "taskId": task_id,
                "workerId": worker_id,
                "displayPreference": { "ordered": ["terminal"], "placement": "embedded" },
            }),
        )
        .await;
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();
    assert_eq!(submit["result"]["display"]["selected"], "terminal");
    assert_eq!(submit["result"]["display"]["attempts"], json!(["terminal"]));

    let replay = client
        .call(5, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let attached = pane_events(&replay, "displayPaneAttached");
    assert_eq!(attached.len(), 1, "exactly one attach: {attached:?}");
    assert_eq!(attached[0]["runId"], run_id);
    assert_eq!(attached[0]["backend"], "terminal");
    assert_eq!(
        attached[0]["paneRef"], "",
        "resolution never activates a backend, so there is no vendor pane id yet"
    );
}

/// Every `displayEvent` payload of `kind` in a replay response.
fn pane_events<'a>(replay: &'a Value, kind: &str) -> Vec<&'a Value> {
    replay["result"]
        .as_array()
        .expect("events/replay returns an array")
        .iter()
        .map(|e| &e["event"])
        .filter(|e| e["type"] == "displayEvent" && e["payload"]["kind"] == kind)
        .map(|e| &e["payload"])
        .collect()
}

/// The ordered `WorkspaceEvent` tag values (`"leaseRequested"`,
/// `"leaseAcquired"`, `"leaseReleased"`, `"cleanupFailed"`, ...) journaled
/// for `run_id` in a replay response, in emission order.
fn workspace_event_kinds_for_run(replay: &Value, run_id: &str) -> Vec<String> {
    replay["result"]
        .as_array()
        .expect("events/replay returns an array")
        .iter()
        .map(|e| &e["event"])
        .filter(|e| e["type"] == "workspaceEvent" && e["payload"]["runId"] == run_id)
        .map(|e| e["payload"]["kind"]["type"].as_str().unwrap().to_string())
        .collect()
}

/// An attach is journaled if and only if a backend was actually
/// selected. Whether `herdr` happens to be installed decides which branch
/// runs, so the test asserts the correspondence rather than the outcome.
#[tokio::test]
async fn a_pane_is_journaled_exactly_when_a_backend_was_selected() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver) as Arc<dyn RunDriver>);
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({
                "taskId": task_id,
                "workerId": worker_id,
                // Only `herdr`, with no fallback: on a machine without
                // it this resolves to nothing at all.
                "displayPreference": { "ordered": ["herdr"], "placement": "tab" },
            }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "a headless run still submits: {submit:?}"
    );
    let selected = submit["result"]["display"]["selected"].clone();

    let replay = client
        .call(5, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let attached = pane_events(&replay, "displayPaneAttached");
    match selected.as_str() {
        Some(backend) => {
            assert_eq!(attached.len(), 1, "a selection journals one attach");
            assert_eq!(attached[0]["backend"], backend);
        }
        None => assert!(
            attached.is_empty(),
            "a headless run must journal no pane at all: {attached:?}"
        ),
    }
}

#[tokio::test]
async fn second_nested_worker_observed_on_an_already_actioned_run_never_double_cancels() {
    let driver = Arc::new(ViolationTriggeringRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
        // Default QuarantineAndCancel: the first observation cancels the
        // run (terminal), so the idempotency guard is state-based here.
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let (_, _, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    assert_eq!(
        driver.cancel_calls().len(),
        1,
        "first observation must cancel once"
    );

    // A second, independent NestedWorkerObserved on the same (now
    // terminal) run -- e.g. the adapter reports a further unexpected
    // child before it is torn down.
    driver
        .emit_nested_worker_observed("child-vendor-2", "parent-vendor-2")
        .await
        .expect("a second NestedWorkerObserved must still be journaled");

    // Still exactly one cancel_run call: the second observation must not
    // create a second cancellation intent or call cancel_run again.
    assert_eq!(
        driver.cancel_calls().len(),
        1,
        "an already-actioned run must not be cancelled twice"
    );

    // But both observations are durably recorded (Option B: still
    // journal, just skip re-applying the action) -- OMP can see that a
    // run was hit by more than one unexpected child.
    let replay = client
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let events = replay["result"].as_array().unwrap();
    let recorded_count = events
        .iter()
        .filter(|e| e["event"]["type"] == "policyViolationRecorded")
        .count();
    assert_eq!(
        recorded_count, 2,
        "both NestedWorkerObserved events must produce a durable policyViolationRecorded"
    );

    let get = client.call(7, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(get["result"]["state"], "cancelled");
}

/// R80: `policy/violation/list` is the discovery surface for which
/// violation still holds a quarantine -- two recorded violations, one
/// decided, must both list with their real decision state.
#[tokio::test]
async fn policy_violation_list_reports_decision_state_for_every_violation() {
    let driver = Arc::new(ViolationTriggeringRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
        c.nested_violation_action = batman_runtime::config::NestedViolationAction::Quarantine;
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let (_, _, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    // A second violation on the same quarantined run.
    driver
        .emit_nested_worker_observed("child-vendor-2", "parent-vendor-2")
        .await
        .expect("the second observation journals");

    let list = client
        .call(5, "policy/violation/list", json!({ "runId": run_id }))
        .await;
    assert!(list.get("error").is_none(), "{list:?}");
    let violations = list["result"]["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(
        violations.iter().all(|v| v["resolution"].is_null()),
        "both start undecided: {violations:?}"
    );
    // Total both-ways check: the query op's hand-built rows must
    // round-trip through the canonical deny_unknown_fields type the
    // extension Ajv-validates against -- a renamed PolicyViolationSummary
    // field fails here at test time, not as a live ValidationError.
    serde_json::from_value::<batman_protocol::PolicyViolationListResult>(list["result"].clone())
        .expect("the wire shape must deserialize as the canonical PolicyViolationListResult");
    // Pin the exact wire key set: the query op's hand-built rows must stay
    // byte-compatible with the canonical PolicyViolationSummary the
    // extension Ajv-validates against (additionalProperties: false).
    let mut keys: Vec<&str> = violations[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "action",
            "createdAt",
            "resolution",
            "resolvedAt",
            "resolvedBy",
            "runId",
            "taskId",
            "vendorChildId",
            "vendorParentRef",
            "violationId",
            "workerId"
        ]
    );

    // Decide one; the list must now show exactly one open.
    let decided_id = violations[0]["violationId"].as_str().unwrap().to_string();
    let decide = client
        .call(
            6,
            "policy/violation/decide",
            json!({ "violationId": decided_id, "resolution": "release" }),
        )
        .await;
    assert_eq!(decide["result"]["outcome"], "decided", "{decide:?}");
    assert_eq!(
        decide["result"]["quarantineCleared"], false,
        "another violation is still open: {decide:?}"
    );

    let list = client
        .call(7, "policy/violation/list", json!({ "runId": run_id }))
        .await;
    let violations = list["result"]["violations"].as_array().unwrap();
    let open: Vec<_> = violations
        .iter()
        .filter(|v| v["resolution"].is_null())
        .collect();
    let decided: Vec<_> = violations
        .iter()
        .filter(|v| !v["resolution"].is_null())
        .collect();
    assert_eq!(open.len(), 1, "{violations:?}");
    assert_eq!(decided.len(), 1, "{violations:?}");
    assert_eq!(decided[0]["violationId"], decided_id.as_str());
    assert_eq!(decided[0]["resolution"], "release");
    assert_eq!(decided[0]["resolvedBy"], "omp-1");
}

/// R78: the quarantine gate for `workspace/apply` lives inside the same
/// domain op as the ownership gate AND inside the `ApplyStarted` append's
/// own transaction. A quarantined run's apply must be refused `-32101`
/// with no `applyStarted` in the journal -- flipping either
/// `enforce_quarantine` at the apply callsite or the guarded append back
/// to the plain variant fails this test.
#[tokio::test]
async fn workspace_apply_on_a_quarantined_run_is_refused_and_journals_no_apply_start() {
    let driver = Arc::new(ViolationTriggeringRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
        c.nested_violation_action = batman_runtime::config::NestedViolationAction::Quarantine;
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let (_, _, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    // Quarantined by the observation emitted from start(); the run is
    // still live (Quarantine, not QuarantineAndCancel), so it can hold a
    // lease.
    let acquire = client
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "write", "requestedIsolation": "shared" }),
        )
        .await;
    assert!(
        acquire.get("error").is_none(),
        "acquire is not quarantine-gated: {acquire:?}"
    );
    let lease_id = acquire["result"]["leaseId"].as_str().unwrap().to_string();
    let base_revision = acquire["result"]["baseRevision"]
        .as_str()
        .unwrap()
        .to_string();

    let apply = client
        .call(
            6,
            "workspace/apply",
            json!({
                "leaseId": lease_id,
                "strategy": "applyPatch",
                "artifactId": batman_protocol::ArtifactId::new().to_string(),
                "expectedTargetRevision": base_revision,
            }),
        )
        .await;
    assert_eq!(
        apply["error"]["code"], -32101,
        "a quarantined run's apply must be refused POLICY_QUARANTINED: {apply:?}"
    );

    let inspect = client
        .call(7, "workspace/inspect", json!({ "leaseId": lease_id }))
        .await;
    assert_eq!(
        inspect["error"]["code"], -32101,
        "a quarantined run's inspect must be refused POLICY_QUARANTINED: {inspect:?}"
    );

    let replay = client
        .call(8, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert!(
        workspace_event_kinds_for_run(&replay, &run_id)
            .iter()
            .all(|k| k != "applyStarted"),
        "a refused apply must journal no ApplyStarted: {replay:?}"
    );
}

/// R79: two CONCURRENT cancelling observations -- both journaled before
/// either transition commits, so both read `already_actioned = false` --
/// must both report success. The loser's transition fails because the
/// winner terminalized the run; that is the idempotent success the doc
/// comment always promised, acknowledged as `superseded` in the audited
/// `operations` table rather than surfacing as an error.
///
/// `current_thread` flavor on purpose: `join!` then alternates the two
/// emit futures at every await, so their database-actor submissions
/// interleave A-journal, B-journal, A-intent, B-intent, A-transition,
/// B-transition (FIFO actor queue) -- deterministically producing the
/// race. On the multi-thread runtime the observations can serialize
/// (B journals after A's transition), which is the *other*, pre-existing
/// idempotency path already covered by
/// `second_nested_worker_observed_on_an_already_actioned_run_never_double_cancels`.
#[tokio::test(flavor = "current_thread")]
async fn concurrent_cancelling_violations_are_both_idempotent_successes() {
    /// Captures the driver context in `start` WITHOUT emitting, so the
    /// test controls exactly when each observation fires.
    #[derive(Default)]
    struct CapturingDriver {
        captured: parking_lot::Mutex<Option<RunDriverContext>>,
        cancel_calls: parking_lot::Mutex<usize>,
    }

    impl RunDriver for CapturingDriver {
        fn active_run_count(&self) -> usize {
            0
        }

        fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
            *self.captured.lock() = Some(ctx);
            Box::pin(async { Ok(()) })
        }

        fn send_follow_up(
            &self,
            _run_id: RunId,
            _task_id: TaskId,
            _worker_id: WorkerId,
            _prompt: String,
        ) -> AdapterFuture<'static, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
            None
        }

        fn cancel_run(
            &self,
            _run_id: RunId,
            _scope: CancelScope,
        ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>>
        {
            *self.cancel_calls.lock() += 1;
            Box::pin(async { Ok(batman_runtime::service::CancelOutcome::Cancelled) })
        }
    }

    let driver = Arc::new(CapturingDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
        // Default action is QuarantineAndCancel.
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let (_, _, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    let ctx = driver
        .captured
        .lock()
        .clone()
        .expect("start captured the context");
    let emit = |child: &'static str, parent: &'static str| {
        let ctx = ctx.clone();
        async move {
            let sink = batman_runtime::adapter::DomainAdapterEventSink::new(
                ctx.db.clone(),
                ctx.project_id,
                ctx.events_tx.clone(),
                vec![],
                true,
                Arc::clone(&ctx.violation_service),
                None,
            )
            .expect("built-in patterns always compile");
            sink.emit(AdapterEvent {
                run_id: ctx.run_id,
                task_id: ctx.task_id,
                worker_id: ctx.worker_id,
                payload: AdapterEventPayload::NestedWorkerObserved {
                    vendor_child_id: child.to_string(),
                    vendor_parent_ref: parent.to_string(),
                },
            })
            .await
        }
    };

    // Both observations in flight together: each journals its violation
    // (reading already_actioned = false) before either transition commits.
    let (first, second) = tokio::join!(emit("child-a", "parent-a"), emit("child-b", "parent-b"));
    // NOTE: the sink swallows record_nested_worker errors into a warn log
    // (event_sink.rs), so these two expects cannot fail for R79's reason;
    // the real idempotency proof is the two-outcome operations-table
    // assertion below, which fails when the loser's ack never lands.
    first.expect("the sink never errors");
    second.expect("the sink never errors");

    // Exactly one cancelled transition.
    let get = client.call(6, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(get["result"]["state"], "cancelled");

    // The audited trail is honest: two policyViolationCancel intents, one
    // acknowledged cancelled, one superseded.
    let conn = rusqlite::Connection::open(&harness.database).unwrap();
    let acks: Vec<Option<String>> = conn
        .prepare("SELECT acknowledgement_json FROM operations WHERE kind = 'policyViolationCancel' ORDER BY requested_at")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(acks.len(), 2, "both observations persist an audited intent");
    let outcomes: Vec<String> = acks
        .iter()
        .map(|a| {
            let ack = a.as_ref().expect("every intent is acknowledged");
            serde_json::from_str::<serde_json::Value>(ack).unwrap()["outcome"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let mut sorted = outcomes.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        ["cancelled", "superseded"],
        "one intent cancelled, one superseded: {outcomes:?}"
    );
}

// --------------------------------------------------------------- harness
struct Harness {
    socket: PathBuf,
    owned_dir: PathBuf,
    database: PathBuf,
    project_id: ProjectId,
    _state: tempfile::TempDir,
    _repo: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Harness {
    async fn start(config_fn: impl FnOnce(&mut ServerConfig)) -> Self {
        let state = tempfile::Builder::new()
            .prefix("bat-os-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = tempfile::Builder::new()
            .prefix("bat-or-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();

        let paths = RuntimePaths::resolve(state.path(), repo.path()).unwrap();
        let db = Arc::new(DatabaseHandle::start(paths.database.clone()).await.unwrap());

        let mut config = ServerConfig {
            credential_reader: matching_reader(),
            repository: repo.path().to_path_buf(),
            ..Default::default()
        };
        config_fn(&mut config);

        let server = Server::bind(paths.socket.clone(), db, paths.project_id, config)
            .await
            .unwrap();
        let socket = server.socket_path().to_path_buf();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let join = tokio::spawn(async move {
            let _ = server
                .serve(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let owned_dir = std::fs::canonicalize(repo.path()).unwrap();

        Self {
            socket,
            owned_dir,
            database: paths.database.clone(),
            project_id: paths.project_id,
            _state: state,
            _repo: repo,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

// ---------------------------------------------------------------- client

struct Client {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer,
        }
    }

    async fn send(&mut self, value: &Value) {
        let line = serde_json::to_string(value).unwrap();
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim_end()).unwrap()
    }

    async fn initialize(
        &mut self,
        role: &str,
        instance_id: &str,
        agent_dir: Option<&str>,
    ) -> Value {
        let auth = match role {
            "ompExtension" => json!({
                "role": "ompExtension",
                "instanceId": instance_id,
                "agentDirectory": agent_dir.unwrap()
            }),
            "display" => json!({ "role": "display", "instanceId": instance_id }),
            other => panic!("unsupported role in test helper: {other}"),
        };
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "client": { "name": "@nikolasd/crew", "version": "0.1.0" },
                "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
                "repository": { "canonicalPath": agent_dir.unwrap_or("/tmp"), "vcsRoot": agent_dir.unwrap_or("/tmp") },
                "auth": auth,
                "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
                "lastSequence": null
            }
        }))
        .await;
        self.recv().await
    }

    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        self.recv().await
    }
}

async fn omp_client(harness: &Harness, instance_id: &str) -> Client {
    let mut client = Client::connect(&harness.socket).await;
    let init = client
        .initialize(
            "ompExtension",
            instance_id,
            Some(harness.owned_dir.to_str().unwrap()),
        )
        .await;
    assert!(init.get("error").is_none(), "initialize failed: {init:?}");
    client
}

// -------------------------------------------------------------- task CRUD

#[tokio::test]
async fn task_upsert_then_get_round_trips() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let upsert = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    assert!(upsert.get("error").is_none(), "upsert failed: {upsert:?}");
    let task_id = upsert["result"]["taskId"].as_str().unwrap().to_string();
    assert!(upsert["result"]["sequence"].as_u64().is_some());

    let get = client
        .call(3, "task/get", json!({ "taskId": task_id }))
        .await;
    assert_eq!(get["result"]["ownerClientInstanceId"], "omp-1");
    assert_eq!(get["result"]["revision"], 1);
}

#[tokio::test]
async fn events_replay_round_trips_committed_mutation_events() {
    // Regression test: `events/replay` must deserialize every stored event.
    // The domain repository previously persisted the full `EventEnvelope`
    // (embedding `sequence`, `timestamp`, ...) under `event_json`, but
    // `replay()` expects that column to hold only the bare `RuntimeEvent`
    // -- reconstructing the envelope from the `events` table's own
    // `sequence`/`timestamp`/`project_id`/`run_id` columns. The mismatch
    // made every replay fail once any mutation had committed.
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let upsert = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    assert!(upsert.get("error").is_none(), "upsert failed: {upsert:?}");
    let task_id = upsert["result"]["taskId"].as_str().unwrap().to_string();

    let replay = client
        .call(3, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert!(
        replay.get("error").is_none(),
        "events/replay failed: {replay:?}"
    );
    let events = replay["result"]
        .as_array()
        .expect("events/replay returns an array");
    assert!(
        !events.is_empty(),
        "the committed task/upsert event must be replayable"
    );
    let task_event = events
        .iter()
        .find(|e| e["event"]["type"] == "taskEvent")
        .expect("a taskEvent must be present in the replayed events");
    assert_eq!(task_event["event"]["payload"]["taskId"], task_id);
    assert_eq!(
        task_event["event"]["payload"]["ownerClientInstanceId"],
        "omp-1"
    );
}

#[tokio::test]
async fn events_subscribe_delivers_live_notifications_for_orchestration_mutations() {
    // Regression test: every orchestration/coordination mutation must
    // broadcast its committed envelope to live `events/subscribe`
    // listeners. The broadcast channel previously had no publisher at
    // all, so a monitor connected before a mutation never observed it
    // without reconnecting (which re-triggers `events/replay`).
    let harness = Harness::start(|_| {}).await;
    let mut subscriber = omp_client(&harness, "omp-sub").await;
    let sub = subscriber.call(2, "events/subscribe", json!({})).await;
    assert!(
        sub.get("error").is_none(),
        "events/subscribe failed: {sub:?}"
    );
    assert_eq!(sub["result"]["active"], true);

    let mut mutator = omp_client(&harness, "omp-mut").await;
    let upsert = mutator
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-mut", "revision": 1 }),
        )
        .await;
    assert!(upsert.get("error").is_none(), "upsert failed: {upsert:?}");
    let task_id = upsert["result"]["taskId"].as_str().unwrap().to_string();

    let notification = subscriber.recv().await;
    assert_eq!(notification["method"], "events/event");
    assert_eq!(notification["params"]["event"]["type"], "taskEvent");
    assert_eq!(
        notification["params"]["event"]["payload"]["taskId"],
        task_id
    );
}

#[tokio::test]
async fn task_upsert_is_idempotent_for_same_revision() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let first = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 5 }),
        )
        .await;
    let task_id = first["result"]["taskId"].as_str().unwrap().to_string();

    let second = client
        .call(
            3,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-1", "revision": 5 }),
        )
        .await;
    assert!(
        second.get("error").is_none(),
        "same-revision upsert must succeed: {second:?}"
    );

    let get = client
        .call(4, "task/get", json!({ "taskId": task_id }))
        .await;
    assert_eq!(get["result"]["revision"], 5);
}

#[tokio::test]
async fn task_upsert_rejects_lower_revision() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let first = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 5 }),
        )
        .await;
    let task_id = first["result"]["taskId"].as_str().unwrap().to_string();

    let lower = client
        .call(
            3,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-1", "revision": 3 }),
        )
        .await;
    assert_eq!(
        lower["error"]["code"], -32602,
        "lower revision must be INVALID_PARAMS: {lower:?}"
    );
}

// ------------------------------------------------------------ worker CRUD

#[tokio::test]
async fn worker_create_then_list_and_get() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let create = client
        .call(
            2,
            "worker/create",
            json!({ "fingerprint": "sha256:fake", "adapter": "fake", "model": "test-model" }),
        )
        .await;
    assert!(
        create.get("error").is_none(),
        "worker/create failed: {create:?}"
    );
    let worker_id = create["result"]["workerId"].as_str().unwrap().to_string();
    assert!(create["result"]["sequence"].as_u64().is_some());

    let list = client.call(3, "worker/list", json!({})).await;
    let workers = list["result"]["workers"].as_array().unwrap();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0]["workerId"], worker_id);

    let get = client
        .call(4, "worker/get", json!({ "workerId": worker_id }))
        .await;
    assert_eq!(get["result"]["profileRef"]["adapter"], "fake");
}

// --------------------------------------------------------------- run flow

#[tokio::test]
async fn run_submit_without_driver_reports_adapter_unavailable_but_preserves_queued_run() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    assert_eq!(submit["error"]["message"], "adapter_unavailable");

    // The run itself is still queued, not silently dropped.
    let list = client
        .call(5, "run/list", json!({ "taskId": task_id }))
        .await;
    let runs = list["result"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["state"], "queued");
}

#[tokio::test]
async fn run_submit_with_fake_driver_reaches_working_and_retry_creates_new_run() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "run/submit failed: {submit:?}"
    );
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    let get = client.call(5, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get["result"]["state"], "working",
        "fake driver must reach working: {get:?}"
    );

    // Cancel the run to reach a terminal state before retrying.
    let cancel = client
        .call(6, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "run/cancel failed: {cancel:?}"
    );

    let retry = client
        .call(
            7,
            "run/retry",
            json!({ "priorRunId": run_id, "workerId": worker_id }),
        )
        .await;
    assert!(retry.get("error").is_none(), "run/retry failed: {retry:?}");
    let new_run_id = retry["result"]["runId"].as_str().unwrap();
    assert_ne!(new_run_id, run_id, "retry must create a distinct RunId");
    assert_eq!(
        retry["result"]["taskId"], task_id,
        "retry must retain the same TaskId"
    );

    // The retried run must also reach working, proving retry actually starts the adapter.
    let get_retry = client
        .call(8, "run/get", json!({ "runId": new_run_id }))
        .await;
    assert_eq!(
        get_retry["result"]["state"], "working",
        "retried run must reach working: {get_retry:?}"
    );
}

/// A [`RunDriver`] that captures every [`RunDriverContext`] passed to `start()`
/// and delegates the actual state transitions to a [`FakeRunDriver`].
#[derive(Default)]
struct StartCapturingRunDriver {
    started: parking_lot::Mutex<Vec<RunDriverContext>>,
    inner: FakeRunDriver,
}

impl RunDriver for StartCapturingRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        self.started.lock().push(ctx.clone());
        self.inner.start(ctx)
    }

    fn send_follow_up(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        self.inner
            .send_follow_up(run_id, task_id, worker_id, prompt)
    }

    fn running_adapter(&self, run_id: RunId) -> Option<Arc<dyn Adapter>> {
        self.inner.running_adapter(run_id)
    }

    fn cancel_run(
        &self,
        run_id: RunId,
        scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        self.inner.cancel_run(run_id, scope)
    }
}

/// A [`RunDriver`] whose kill genuinely fails: `cancel_run` returns `Err`,
/// which since R13 always means a live vendor process the kill failed
/// against -- never an absent adapter.
#[derive(Default)]
struct KillFailingRunDriver {
    inner: FakeRunDriver,
}

impl RunDriver for KillFailingRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        self.inner.start(ctx)
    }

    fn send_follow_up(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        self.inner
            .send_follow_up(run_id, task_id, worker_id, prompt)
    }

    fn running_adapter(&self, run_id: RunId) -> Option<Arc<dyn Adapter>> {
        self.inner.running_adapter(run_id)
    }

    fn cancel_run(
        &self,
        _run_id: RunId,
        _scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        Box::pin(async { Err("SIGKILL delivery failed".to_string()) })
    }
}

/// RED (R93): since R13, `cancel_run`'s `Err` always means a live vendor
/// process a kill actually failed against -- the policy-violation path
/// raises `flags.degradedControl` on that condition, but `run/cancel`
/// only warns and reports unqualified success. The kill failure must
/// become visible to `run/get` and the monitor via the same guarded,
/// journaled, broadcast `degradedControl` flag write.
#[tokio::test]
async fn run_cancel_raises_degraded_control_when_the_kill_fails() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(KillFailingRunDriver::default()));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut client, "omp-1").await;

    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "the cancel transition itself succeeded and must report so: {cancel:?}"
    );

    let get = client.call(6, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get["result"]["flags"]["degradedControl"], true,
        "a genuine kill failure must be visible as degradedControl, \
         not only a warn log: {get:?}"
    );

    let replay = client
        .call(7, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let has_flag_event = replay["result"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event"]["payload"]["flags"]["degradedControl"] == true);
    assert!(
        has_flag_event,
        "the flag write must be journaled, not projection-only: {replay:?}"
    );
}

#[tokio::test]
async fn retry_starts_the_adapter_with_the_supplied_prompt() {
    let driver = Arc::new(StartCapturingRunDriver::default());

    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    // Submit with prompt "first".
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id, "prompt": "first" }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "run/submit failed: {submit:?}"
    );
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    // Cancel to reach terminal state.
    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "run/cancel failed: {cancel:?}"
    );

    // Retry with prompt "second".
    let retry = client
        .call(
            6,
            "run/retry",
            json!({ "priorRunId": run_id, "workerId": worker_id, "prompt": "second" }),
        )
        .await;
    assert!(retry.get("error").is_none(), "run/retry failed: {retry:?}");
    let new_run_id = retry["result"]["runId"].as_str().unwrap().to_string();

    // Assert the driver was started twice, and the second start carried the retry prompt.
    let started = driver.started.lock();
    assert_eq!(started.len(), 2, "driver should have started twice");
    assert_eq!(
        started[0].prompt.as_deref(),
        Some("first"),
        "first run should have had prompt 'first'"
    );
    assert_eq!(
        started[1].prompt.as_deref(),
        Some("second"),
        "retried run should have had prompt 'second'"
    );
    assert_eq!(
        started[1].run_id.to_string(),
        new_run_id,
        "retried run should have the new run id"
    );
}

#[tokio::test]
async fn retry_without_a_driver_reports_adapter_unavailable_and_preserves_the_queued_run() {
    // Harness with no driver injected.
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    // Submit fails with adapter_unavailable, but the queued run is preserved.
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    assert_eq!(submit["error"]["message"], "adapter_unavailable");

    // Get the run id from the list.
    let list = client
        .call(5, "run/list", json!({ "taskId": task_id }))
        .await;
    let runs = list["result"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["state"], "queued");
    let run_id = runs[0]["runId"].as_str().unwrap().to_string();

    // Manually transition to cancelled so we have a terminal state to retry from.
    let cancel = client
        .call(6, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "run/cancel failed: {cancel:?}"
    );

    // Retry without a driver should also fail with adapter_unavailable.
    let retry = client
        .call(
            7,
            "run/retry",
            json!({ "priorRunId": run_id, "workerId": worker_id }),
        )
        .await;
    assert_eq!(retry["error"]["message"], "adapter_unavailable");

    // The retried run is still queued (preserved).
    let list2 = client
        .call(8, "run/list", json!({ "taskId": task_id }))
        .await;
    let runs2 = list2["result"]["runs"].as_array().unwrap();
    assert_eq!(runs2.len(), 2, "should have original and retried runs");
    assert_eq!(runs2[1]["state"], "queued", "retried run should be queued");
}

#[tokio::test]
async fn run_cancel_on_settled_run_is_illegal_transition() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    // First cancel succeeds (working -> cancelled).
    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(cancel.get("error").is_none());

    // Second cancel on an already-terminal run is illegal.
    let second_cancel = client
        .call(6, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert_eq!(
        second_cancel["error"]["code"], -32100,
        "expected ILLEGAL_TRANSITION: {second_cancel:?}"
    );
}

#[tokio::test]
async fn run_submit_rejects_task_outside_project() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let worker = client
        .call(
            2,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    // A well-formed but nonexistent taskId.
    let fake_task_id = "018f0000-0000-7000-8000-000000000000";
    let submit = client
        .call(
            3,
            "run/submit",
            json!({ "taskId": fake_task_id, "workerId": worker_id }),
        )
        .await;
    assert_eq!(
        submit["error"]["code"], -32602,
        "expected INVALID_PARAMS for unknown task: {submit:?}"
    );
}

// -------------------------------------------------------- message/approval

#[tokio::test]
async fn message_send_then_list() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    let send = client
        .call(
            5,
            "message/send",
            json!({
                "runId": run_id,
                "senderWorkerId": worker_id,
                "taskId": task_id,
                "kind": "question",
                "payload": "what should I do next?"
            }),
        )
        .await;
    assert!(send.get("error").is_none(), "message/send failed: {send:?}");

    let list = client
        .call(6, "message/list", json!({ "runId": run_id }))
        .await;
    let messages = list["result"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["kind"], "question");
    assert_eq!(messages[0]["payload"], "what should I do next?");
}

#[tokio::test]
async fn message_send_on_a_run_with_no_running_adapter_still_succeeds_and_journals_a_diagnostic() {
    // FakeRunDriver's own `send_follow_up` always errors -- exactly the
    // shape of the real AdapterRegistry's `RegistryError::NoRunningAdapter`
    // for a `queued` run that has not reached a live adapter instance yet.
    // The RPC must still succeed and the message must still be recorded.
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    let send = client
        .call(
            5,
            "message/send",
            json!({
                "runId": run_id,
                "senderWorkerId": worker_id,
                "taskId": task_id,
                "kind": "question",
                "payload": "should I proceed?"
            }),
        )
        .await;
    assert!(
        send.get("error").is_none(),
        "message/send must succeed despite delivery failure: {send:?}"
    );

    let list = client
        .call(6, "message/list", json!({ "runId": run_id }))
        .await;
    let messages = list["result"]["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        1,
        "the message must still be durably recorded"
    );

    let replay = client
        .call(7, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let events = replay["result"]
        .as_array()
        .expect("events/replay returns an array");
    let diagnostic = events
        .iter()
        .find(|e| e["event"]["type"] == "diagnostic")
        .expect("a diagnostic event must be journaled for the failed follow-up delivery");
    assert_eq!(
        diagnostic["event"]["payload"]["code"],
        "follow_up_delivery_failed"
    );
    assert_eq!(diagnostic["event"]["payload"]["level"], "warning");
    assert_eq!(
        diagnostic["runId"], run_id,
        "the diagnostic must be scoped to the run"
    );
}

#[tokio::test]
async fn message_send_on_a_driver_backed_run_reaches_send_follow_up_exactly_once() {
    let driver = Arc::new(RecordingRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    let send = client
        .call(
            5,
            "message/send",
            json!({
                "runId": run_id,
                "senderWorkerId": worker_id,
                "taskId": task_id,
                "kind": "question",
                "payload": "verbatim payload text"
            }),
        )
        .await;
    assert!(send.get("error").is_none(), "message/send failed: {send:?}");

    let follow_ups = driver.follow_ups.lock();
    assert_eq!(
        follow_ups.len(),
        1,
        "send_follow_up must be called exactly once"
    );
    let (recorded_run, recorded_task, recorded_worker, recorded_payload) = &follow_ups[0];
    assert_eq!(recorded_run.to_string(), run_id);
    assert_eq!(recorded_task.to_string(), task_id);
    assert_eq!(recorded_worker.to_string(), worker_id);
    assert_eq!(recorded_payload, "verbatim payload text");
}

/// A message whose `send_follow_up` genuinely succeeds must advance past
/// `recorded` -- leaving it there forever regardless of outcome is the
/// deliveryState inversion this test guards against.
#[tokio::test]
async fn message_send_on_successful_delivery_advances_delivery_state_to_sent() {
    let driver = Arc::new(RecordingRunDriver::default());
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    let send = client
        .call(
            5,
            "message/send",
            json!({
                "runId": run_id,
                "senderWorkerId": worker_id,
                "taskId": task_id,
                "kind": "question",
                "payload": "should this be delivered?"
            }),
        )
        .await;
    assert!(send.get("error").is_none(), "message/send failed: {send:?}");

    let list = client
        .call(6, "message/list", json!({ "runId": run_id }))
        .await;
    let messages = list["result"]["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0]["deliveryState"], "sent",
        "a message whose delivery genuinely succeeded must advance past \
         recorded: {messages:?}"
    );
    assert!(
        messages[0]["sentAt"].is_string(),
        "sentAt must be stamped once delivery succeeds: {messages:?}"
    );
}

// --------------------------------------------------------------- sequence

#[tokio::test]
async fn every_mutation_returns_a_strictly_increasing_sequence() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let first = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let second = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;

    let seq1 = first["result"]["sequence"].as_u64().unwrap();
    let seq2 = second["result"]["sequence"].as_u64().unwrap();
    assert!(
        seq2 > seq1,
        "sequence numbers must strictly increase: {seq1} then {seq2}"
    );
}

// ----------------------------------------------------------- role gating

#[tokio::test]
async fn display_principal_cannot_call_orchestration_mutation_methods() {
    let harness = Harness::start(|_| {}).await;
    let mut client = Client::connect(&harness.socket).await;
    client.initialize("display", "display-1", None).await;

    let attempt = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    assert_eq!(
        attempt["error"]["code"], -32601,
        "display must get METHOD_NOT_FOUND: {attempt:?}"
    );
}

// ------------------------------------------------------------- reconcile

#[tokio::test]
async fn reconcile_omp_rebinds_task_ownership_on_matching_revision() {
    let harness = Harness::start(|_| {}).await;
    let mut first_client = omp_client(&harness, "omp-1").await;

    let created = first_client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 7 }),
        )
        .await;
    let task_id = created["result"]["taskId"].as_str().unwrap().to_string();

    // A second OMP instance connects and cannot mutate the task without reconciling.
    let mut second_client = omp_client(&harness, "omp-2").await;
    let reconcile = second_client
        .call(
            2,
            "reconcile/omp",
            json!({ "taskId": task_id, "revision": 7 }),
        )
        .await;
    assert!(
        reconcile.get("error").is_none(),
        "reconcile/omp failed: {reconcile:?}"
    );
    assert_eq!(reconcile["result"]["newOwnerClientInstanceId"], "omp-2");

    let get = second_client
        .call(3, "task/get", json!({ "taskId": task_id }))
        .await;
    assert_eq!(get["result"]["ownerClientInstanceId"], "omp-2");
}

/// The resume path survives a rebind (R74): a rebind matches the stored
/// revision but does not consume it, so a later `task/upsert` presenting
/// the same revision -- how the extension re-registers a task after a
/// restart -- still succeeds, while a lower revision stays refused by the
/// guarded write.
#[tokio::test]
async fn task_upsert_at_the_same_revision_still_succeeds_after_a_reconcile() {
    let harness = Harness::start(|_| {}).await;
    let mut first_client = omp_client(&harness, "omp-1").await;

    let created = first_client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 7 }),
        )
        .await;
    let task_id = created["result"]["taskId"].as_str().unwrap().to_string();

    let mut second_client = omp_client(&harness, "omp-2").await;
    let reconcile = second_client
        .call(
            2,
            "reconcile/omp",
            json!({ "taskId": task_id, "revision": 7 }),
        )
        .await;
    assert!(
        reconcile.get("error").is_none(),
        "reconcile/omp failed: {reconcile:?}"
    );

    let get = second_client
        .call(3, "task/get", json!({ "taskId": task_id }))
        .await;
    assert_eq!(
        get["result"]["revision"], 7,
        "a rebind must not consume the stored revision: {get:?}"
    );

    let resumed = second_client
        .call(
            4,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-2", "revision": 7 }),
        )
        .await;
    assert!(
        resumed.get("error").is_none(),
        "resuming at the same revision after a reconcile must succeed: {resumed:?}"
    );

    let stale = second_client
        .call(
            5,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-2", "revision": 6 }),
        )
        .await;
    assert_eq!(
        stale["error"]["code"], -32602,
        "a lower revision must stay refused by the guarded write: {stale:?}"
    );
    assert_eq!(
        stale["error"]["message"], "revision 6 is lower than stored revision 7",
        "the legacy message text is the pinned contract: {stale:?}"
    );
}

#[tokio::test]
async fn reconcile_omp_rejects_mismatched_revision() {
    let harness = Harness::start(|_| {}).await;
    let mut first_client = omp_client(&harness, "omp-1").await;

    let created = first_client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 7 }),
        )
        .await;
    let task_id = created["result"]["taskId"].as_str().unwrap().to_string();

    let mut second_client = omp_client(&harness, "omp-2").await;
    let reconcile = second_client
        .call(
            2,
            "reconcile/omp",
            json!({ "taskId": task_id, "revision": 99 }),
        )
        .await;
    assert_eq!(
        reconcile["error"]["code"], -32602,
        "mismatched revision must be INVALID_PARAMS: {reconcile:?}"
    );
}

/// R76: `task/upsert`'s guarded write has no ownership predicate -- it
/// only enforces revision monotonicity (R74), never that the caller's
/// `ownerClientInstanceId` matches whoever currently owns the row. A
/// second OMP-extension client that never reconciled can therefore
/// present the *stored* revision together with its own instance id and
/// seize an in-flight task straight out from under its rightful owner,
/// bypassing `reconcile/omp` entirely -- `task/upsert { taskId,
/// ownerClientInstanceId: "omp-2", revision: <stored> }`. RED: today this
/// upsert succeeds and rewrites `ownerClientInstanceId` to "omp-2"; it
/// must instead be refused with `-32602`, leaving ownership untouched.
///
/// The companion assertion proves the legitimate route still works:
/// after `reconcile/omp` (which itself assigns the new owner under
/// revision arbitration) an upsert *by the new owner* at the stored
/// revision succeeds. That half already passes today -- not because
/// ownership is enforced, but because nothing is enforced -- and it must
/// keep passing once R76's guard lands.
#[tokio::test]
async fn task_upsert_cannot_seize_ownership_from_another_instance() {
    let harness = Harness::start(|_| {}).await;
    let mut first_client = omp_client(&harness, "omp-1").await;

    let created = first_client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 7 }),
        )
        .await;
    let task_id = created["result"]["taskId"].as_str().unwrap().to_string();

    // omp-2 never reconciles. It presents the stored revision (not a
    // higher one) with its own instance id.
    let mut second_client = omp_client(&harness, "omp-2").await;
    let seizure = second_client
        .call(
            2,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-2", "revision": 7 }),
        )
        .await;
    assert_eq!(
        seizure["error"]["code"], -32602,
        "an upsert by a non-owner presenting the stored revision must be refused: {seizure:?}"
    );

    // Higher revision + non-owner: the variant that also clears R74's
    // `>=` guard, so the owner clause alone must refuse it.
    let seizure_higher = second_client
        .call(
            6,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-2", "revision": 8 }),
        )
        .await;
    assert_eq!(
        seizure_higher["error"]["code"], -32602,
        "a higher-revision upsert by a non-owner must be refused by the owner clause: {seizure_higher:?}"
    );
    assert_eq!(
        seizure_higher["error"]["message"],
        format!("task {task_id} is not owned by omp-2"),
        "the refusal must classify ownership, not revision: {seizure_higher:?}"
    );

    // Lower revision + non-owner: RevisionTooLow wins the classification
    // (deliberate precedence -- an owner-agnostic staleness report keeps
    // R74's byte-pinned message stable).
    let seizure_lower = second_client
        .call(
            7,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-2", "revision": 6 }),
        )
        .await;
    assert_eq!(
        seizure_lower["error"]["message"], "revision 6 is lower than stored revision 7",
        "a stale non-owner upsert reports staleness first: {seizure_lower:?}"
    );

    // Param validation: an owner id that differs from the connected
    // principal is refused before the guarded write is ever reached.
    let spoofed = second_client
        .call(
            8,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-9", "revision": 7 }),
        )
        .await;
    assert_eq!(spoofed["error"]["code"], -32602, "{spoofed:?}");
    assert!(
        spoofed["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("must match the connected instance"),
        "presenting someone else's owner id is param-invalid: {spoofed:?}"
    );

    let get = first_client
        .call(3, "task/get", json!({ "taskId": task_id }))
        .await;
    assert_eq!(
        get["result"]["ownerClientInstanceId"], "omp-1",
        "a refused upsert must not have rewritten ownership: {get:?}"
    );
    assert_eq!(get["result"]["revision"], 7);

    // Legitimate path: reconcile first, then the new owner may upsert at
    // the stored revision.
    let reconcile = second_client
        .call(
            3,
            "reconcile/omp",
            json!({ "taskId": task_id, "revision": 7 }),
        )
        .await;
    assert!(
        reconcile.get("error").is_none(),
        "reconcile/omp failed: {reconcile:?}"
    );
    assert_eq!(reconcile["result"]["newOwnerClientInstanceId"], "omp-2");

    let resumed = second_client
        .call(
            4,
            "task/upsert",
            json!({ "taskId": task_id, "ownerClientInstanceId": "omp-2", "revision": 7 }),
        )
        .await;
    assert!(
        resumed.get("error").is_none(),
        "an upsert by the actual (post-reconcile) owner at the stored revision must succeed: {resumed:?}"
    );
}
#[tokio::test]
async fn workspace_acquire_returns_lease_for_valid_run() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    // Create a task, worker, and run to get a run_id
    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();

    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "run/submit failed: {submit:?}"
    );

    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    // Now acquire a workspace for that run
    let acquire = client
        .call(
            5,
            "workspace/acquire",
            json!({
                "runId": run_id,
                "mode": "readOnly",
                "requestedIsolation": "shared"
            }),
        )
        .await;

    assert!(
        acquire.get("error").is_none(),
        "workspace/acquire failed: {acquire:?}"
    );
    assert!(
        acquire["result"]["leaseId"].as_str().is_some(),
        "leaseId missing in response: {acquire:?}"
    );
    assert_eq!(
        acquire["result"]["runId"].as_str().unwrap(),
        &run_id,
        "runId mismatch in response"
    );
}

/// An unrecognized `workspaceMode` is rejected rather than silently
/// downgraded to the shared repository. The silent fallback was the real
/// hazard: a typo (`"isolatd"`) would run a write-capable agent directly
/// against the user's working tree while the caller believed it was
/// isolated.
#[tokio::test]
async fn run_submit_rejects_an_unrecognized_workspace_mode() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id, "workspaceMode": "isolatd" }),
        )
        .await;

    let message = submit["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("workspaceMode"),
        "a typo'd mode must be refused by name, got: {submit:?}"
    );
    assert_eq!(
        submit["error"]["code"].as_i64(),
        Some(i64::from(batman_protocol::error_code::INVALID_PARAMS)),
        "an unrecognized mode is the caller's error: {submit:?}"
    );

    // `shared` remains accepted and is the documented default's spelling.
    let shared = client
        .call(
            5,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id, "workspaceMode": "shared" }),
        )
        .await;
    assert!(
        shared.get("error").is_none(),
        "shared must remain accepted: {shared:?}"
    );
}

// -------------------------------------------- lease leak on failed run start (R41 / R50)

/// R50: `materialize()` failing after `LeaseService::acquire` succeeded must
/// release the lease, not leak it. The harness repository has an empty
/// `.git` directory with no commits, so `gitWorktree` isolation's
/// `git rev-parse HEAD` fails inside `materialize()` -- exactly the failure
/// shape this test exercises.
#[tokio::test]
async fn start_queued_run_releases_the_lease_when_materialize_fails() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id, "workspaceMode": "isolated" }),
        )
        .await;
    assert!(
        submit.get("error").is_some(),
        "materialize must fail against a repository with no commits: {submit:?}"
    );

    let list = client
        .call(5, "run/list", json!({ "taskId": task_id }))
        .await;
    let runs = list["result"]["runs"].as_array().unwrap();
    assert_eq!(
        runs.len(),
        1,
        "the queued run row must be preserved, not rolled back: {runs:?}"
    );
    assert_eq!(runs[0]["state"], "queued");
    let run_id = runs[0]["runId"].as_str().unwrap().to_string();

    let replay = client
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let kinds = workspace_event_kinds_for_run(&replay, &run_id);
    assert_eq!(
        kinds,
        vec!["leaseRequested", "leaseReleased"],
        "a materialize failure must release the lease it just requested, \
         not leak it, and must never claim leaseAcquired: {kinds:?}"
    );

    let lease_db = harness.socket.parent().unwrap().join("workspace-leases.db");
    let conn = rusqlite::Connection::open(&lease_db).unwrap();
    let (state, path, released_at): (String, String, Option<String>) = conn
        .query_row(
            "SELECT state, path, released_at FROM workspace_leases WHERE run_id = ?1",
            rusqlite::params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        state, "released",
        "the row must not be left allocating/active -- that is the leak"
    );
    assert_eq!(
        path, "",
        "materialize() never produced a real path to record"
    );
    assert!(
        released_at.is_some(),
        "a genuinely released lease must record released_at"
    );
}

/// R50, second call site: `workspace/acquire`'s own `materialize()` failure
/// must release the lease exactly like `start_queued_run`'s.
#[tokio::test]
async fn workspace_acquire_releases_the_lease_when_materialize_fails() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    // No `workspaceMode`: `run/submit` never touches the lease service, so
    // the run's own workspace/acquire call below starts from a clean slate.
    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "run/submit failed: {submit:?}"
    );
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    let acquire = client
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "write", "requestedIsolation": "gitWorktree" }),
        )
        .await;
    assert!(
        acquire.get("error").is_some(),
        "materialize must fail against a repository with no commits: {acquire:?}"
    );

    let replay = client
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let kinds = workspace_event_kinds_for_run(&replay, &run_id);
    assert_eq!(
        kinds,
        vec!["leaseRequested", "leaseReleased"],
        "workspace/acquire's materialize failure must release the lease, not leak it: {kinds:?}"
    );

    let lease_db = harness.socket.parent().unwrap().join("workspace-leases.db");
    let conn = rusqlite::Connection::open(&lease_db).unwrap();
    let (state, path, released_at): (String, String, Option<String>) = conn
        .query_row(
            "SELECT state, path, released_at FROM workspace_leases WHERE run_id = ?1",
            rusqlite::params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        state, "released",
        "the row must not be left allocating/active"
    );
    assert_eq!(
        path, "",
        "materialize() never produced a real path to record"
    );
    assert!(
        released_at.is_some(),
        "a genuinely released lease must record released_at"
    );
}

/// R41: a driver that fails `start` after the workspace was already
/// materialized and the lease activated must still release the lease and
/// tear down the worktree it just created, not leak both.
#[tokio::test]
async fn start_queued_run_releases_the_lease_and_worktree_when_driver_start_fails() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FailingRunDriver));
    })
    .await;
    init_real_git_repo(&harness.owned_dir);
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id, "workspaceMode": "isolated" }),
        )
        .await;
    let message = submit["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("boom: adapter never came up"),
        "the failure must come from FailingRunDriver::start, not an earlier \
         materialize/activate failure, so this test actually exercises the \
         post-activation cleanup path: {submit:?}"
    );

    let list = client
        .call(5, "run/list", json!({ "taskId": task_id }))
        .await;
    let runs = list["result"]["runs"].as_array().unwrap();
    assert_eq!(
        runs.len(),
        1,
        "the queued run row must be preserved: {runs:?}"
    );
    assert_eq!(runs[0]["state"], "queued");
    let run_id = runs[0]["runId"].as_str().unwrap().to_string();

    let replay = client
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let kinds = workspace_event_kinds_for_run(&replay, &run_id);
    assert_eq!(
        kinds,
        vec!["leaseRequested", "leaseAcquired", "leaseReleased"],
        "materialize/activate succeeded before driver.start failed, so the \
         cleanup must answer with the same leaseReleased every other \
         abandonment past that point emits: {kinds:?}"
    );

    let lease_db = harness.socket.parent().unwrap().join("workspace-leases.db");
    let conn = rusqlite::Connection::open(&lease_db).unwrap();
    let (state, path, released_at): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT state, path, released_at FROM workspace_leases WHERE run_id = ?1",
            rusqlite::params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        state, "released",
        "the lease must be released, not left active with no owner"
    );
    assert!(
        released_at.is_some(),
        "a genuinely released lease must record released_at"
    );
    let worktree_path = path.expect("an activated lease has a real path");
    assert!(
        !std::path::Path::new(&worktree_path).exists(),
        "the worktree materialized before driver.start failed must be torn down: {worktree_path}"
    );
}
// ---------------------------------------------------------------- item 33: real adapter cancel

/// Locates the `fake-worker` binary, building it if necessary. Each
/// `tests/*.rs` file is a separate compilation unit, so this cannot be
/// shared with `tests/supervisor.rs`'s or `tests/omp_rpc_adapter.rs`'s own
/// copies of this same helper.
fn fake_worker_path() -> PathBuf {
    static PATH: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(build_fake_worker_once);
    PATH.clone()
}

fn build_fake_worker_once() -> PathBuf {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("parent of runtime crate");
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--quiet", "-p", "fake-worker"])
        .current_dir(workspace_root)
        .status()
        .expect("cargo build -p fake-worker must be runnable");
    assert!(status.success(), "cargo build -p fake-worker failed");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let profile_dir = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let binary = target_dir.join(profile_dir).join("fake-worker");
    assert!(
        binary.is_file(),
        "expected fake-worker binary at {}",
        binary.display()
    );
    binary
}

/// Collects every `AdapterEvent` a real adapter emits, so the test can
/// read back the OS pid `OmpRpcAdapter::start` reports via
/// `AdapterEventPayload::ProcessStarted` -- the adapter itself exposes no
/// pid accessor, this is the only observable path to it.
#[derive(Default)]
struct TestSink {
    events: tokio::sync::Mutex<Vec<AdapterEvent>>,
}

impl TestSink {
    async fn process_started_pid(&self) -> Option<u32> {
        self.events
            .lock()
            .await
            .iter()
            .find_map(|event| match &event.payload {
                AdapterEventPayload::ProcessStarted { pid } => Some(*pid),
                _ => None,
            })
    }
}

impl AdapterEventSink for TestSink {
    fn emit(&self, event: AdapterEvent) -> batman_runtime::adapter::AdapterFuture<'_, u64> {
        Box::pin(async move {
            let mut events = self.events.lock().await;
            events.push(event);
            Ok(events.len() as u64)
        })
    }
}

/// A `RunDriver` that constructs a real `OmpRpcAdapter` (via
/// `OmpRpcAdapter::with_binary`, pointed at the `fake-worker` fixture) and
/// stores it, delegating `running_adapter`/`cancel_run` to the adapter's
/// own methods exactly as `AdapterRegistry` does -- exercising the real
/// `Adapter::cancel` implementation (`run_pump`'s
/// `client.process_mut().terminate().await`), not a hand-rolled stand-in.
struct RealAdapterRunDriver {
    adapter: parking_lot::Mutex<Option<Arc<OmpRpcAdapter>>>,
    sink: Arc<TestSink>,
}

impl Default for RealAdapterRunDriver {
    fn default() -> Self {
        Self {
            adapter: parking_lot::Mutex::new(None),
            sink: Arc::new(TestSink::default()),
        }
    }
}

impl RealAdapterRunDriver {
    /// The fake-worker's real OS pid, once `OmpRpcAdapter::start` has
    /// emitted `ProcessStarted` (always true by the time `start` returns).
    async fn pid(&self) -> Option<u32> {
        self.sink.process_started_pid().await
    }
}

impl RunDriver for RealAdapterRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        let adapter = Arc::new(OmpRpcAdapter::with_binary(
            fake_worker_path().to_string_lossy().into_owned(),
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
            },
            OmpRpcAdapterOptions::default(),
            None,
        ));
        *self.adapter.lock() = Some(Arc::clone(&adapter));
        let sink = Arc::clone(&self.sink) as Arc<dyn AdapterEventSink>;
        Box::pin(async move {
            adapter
                .start(
                    StartSpec {
                        run_id: ctx.run_id,
                        task_id: ctx.task_id,
                        worker_id: ctx.worker_id,
                        prompt: ctx.prompt.clone().unwrap_or_default(),
                        resume: None,
                    },
                    sink,
                )
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn send_follow_up(
        &self,
        _run_id: RunId,
        _task_id: TaskId,
        _worker_id: WorkerId,
        _prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Err("not supported".to_string()) })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
        self.adapter
            .lock()
            .as_ref()
            .map(|a| Arc::clone(a) as Arc<dyn Adapter>)
    }

    fn cancel_run(
        &self,
        _run_id: RunId,
        scope: CancelScope,
    ) -> AdapterFuture<'static, Result<batman_runtime::service::CancelOutcome, String>> {
        let adapter = Arc::clone(self.adapter.lock().as_ref().unwrap());
        Box::pin(async move {
            adapter
                .cancel(scope)
                .await
                .map(|()| batman_runtime::service::CancelOutcome::Cancelled)
                .map_err(|e| e.to_string())
        })
    }
}

/// Closes item 33's remaining gap: proves `run/cancel` reaches the *real*
/// `AdapterRegistry`-equivalent chain -- `RunDriver::cancel_run` ->
/// `OmpRpcAdapter::cancel()` -> `run_pump`'s
/// `client.process_mut().terminate().await` -- and the OS-level vendor
/// subprocess actually dies, not merely that the run's database state
/// becomes `"cancelled"`.
///
/// Uses `OmpRpcAdapter::with_binary` pointed at the `fake-worker` fixture
/// (its `--mode rpc` argv, always sent verbatim by `OmpRpcAdapter::start`,
/// is aliased to fake-worker's `omp-rpc-host-tool` mode -- see
/// `fake-worker/src/main.rs`'s `Mode::OmpRpcHostTool`), so this exercises
/// the adapter's real, production `cancel()` implementation end to end.
///
/// This does not itself prove SIGKILL escalation (fake-worker's
/// `omp-rpc-host-tool` mode does not ignore SIGINT/SIGTERM, so
/// `ManagedProcess::terminate` is expected to succeed on its first
/// signal) -- that coverage remains `supervisor.rs`'s `ignore-term` test.
#[tokio::test]
async fn run_cancel_reaches_real_omprpc_adapter_and_kills_process() {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let driver = Arc::new(RealAdapterRunDriver::default());

    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::clone(&driver) as Arc<dyn RunDriver>);
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();

    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "run/submit failed: {submit:?}"
    );
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    // OmpRpcAdapter::start emits ProcessStarted (carrying the fake-worker's
    // real OS pid) synchronously, before start() ever returns -- so it must
    // already be recorded by the time run/submit's RPC response arrives.
    let pid = driver
        .pid()
        .await
        .expect("OmpRpcAdapter must have emitted ProcessStarted with a real pid");
    let os_pid = Pid::from_raw(pid as i32);
    assert!(
        kill(os_pid, None).is_ok(),
        "fake-worker process (pid {pid}) must be alive right after run/submit"
    );

    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "run/cancel failed: {cancel:?}"
    );

    // OmpRpcAdapter::cancel() only queues Outbound::Terminate on
    // run_pump's channel and returns immediately -- it does not itself
    // await ManagedProcess::terminate() completing. Poll for the process
    // to actually die, bounded well past escalation's worst case at
    // production EscalationTimings::default() (5s + 5s).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if kill(os_pid, None).is_err() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "process (pid {pid}) must be dead after run/cancel reaches the real \
             OmpRpcAdapter::cancel() -> run_pump's ManagedProcess::terminate()"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// -------------------------------------------------------- artifact isolation

/// Regression test for R10: artifact APIs must scope results by the
/// caller's task ownership. Two OMP-extension clients connecting to the
/// same daemon, each owning a different task, should never see each
/// other's artifacts.
#[tokio::test]
async fn artifact_isolation_enforces_task_ownership_scoping() {
    let store = Arc::new(ArtifactStore::new());

    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
        c.artifact_store = Some(Arc::clone(&store));
    })
    .await;

    // Client A creates a task, worker, and run
    let mut client_a = omp_client(&harness, "omp-A").await;
    let task_a = client_a
        .call(
            10,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-A", "revision": 1 }),
        )
        .await;
    let task_a_id = task_a["result"]["taskId"].as_str().unwrap().to_string();

    let worker_a = client_a
        .call(
            11,
            "worker/create",
            json!({ "fingerprint": "sha256:a", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_a_id = worker_a["result"]["workerId"].as_str().unwrap().to_string();

    let submit_a = client_a
        .call(
            12,
            "run/submit",
            json!({ "taskId": task_a_id, "workerId": worker_a_id }),
        )
        .await;
    assert!(
        submit_a.get("error").is_none(),
        "submit A failed: {submit_a:?}"
    );
    let run_a_id = submit_a["result"]["runId"].as_str().unwrap().to_string();

    // Client B creates a task, worker, and run
    let mut client_b = omp_client(&harness, "omp-B").await;
    let task_b = client_b
        .call(
            20,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-B", "revision": 1 }),
        )
        .await;
    let task_b_id = task_b["result"]["taskId"].as_str().unwrap().to_string();

    let worker_b = client_b
        .call(
            21,
            "worker/create",
            json!({ "fingerprint": "sha256:b", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_b_id = worker_b["result"]["workerId"].as_str().unwrap().to_string();

    let submit_b = client_b
        .call(
            22,
            "run/submit",
            json!({ "taskId": task_b_id, "workerId": worker_b_id }),
        )
        .await;
    assert!(
        submit_b.get("error").is_none(),
        "submit B failed: {submit_b:?}"
    );
    let run_b_id = submit_b["result"]["runId"].as_str().unwrap().to_string();

    // Seed artifacts for each run
    let artifact_a = seed_artifact(&store, "artifact-from-A\n", Some(run_a_id.clone())).await;
    let artifact_b = seed_artifact(&store, "artifact-from-B\n", Some(run_b_id.clone())).await;

    // Client A lists artifacts — should only see their own
    let list_a = client_a.call(30, "artifact/list", json!({})).await;
    let artifacts_a: Vec<String> = list_a["result"]["artifacts"]
        .as_array()
        .expect("artifacts is an array")
        .iter()
        .filter_map(|a| {
            a.get("artifactId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert!(
        artifacts_a.contains(&artifact_a.to_string()),
        "Client A should see their own artifact, got: {artifacts_a:?}"
    );
    assert!(
        !artifacts_a.contains(&artifact_b.to_string()),
        "Client A should NOT see Client B's artifact, got: {artifacts_a:?}"
    );
    // Pin the result envelope and each `Artifact` element's exact wire key
    // set (R55 review W3): the handler serializes the canonical
    // `ArtifactListResult`, and a serde attribute change would silently
    // change the wire under a green suite.
    let mut list_keys: Vec<&str> = list_a["result"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    list_keys.sort_unstable();
    assert_eq!(list_keys, ["artifacts"]);
    let first = list_a["result"]["artifacts"]
        .as_array()
        .unwrap()
        .first()
        .expect("client A has at least one artifact");
    let mut artifact_keys: Vec<&str> = first
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    artifact_keys.sort_unstable();
    assert_eq!(
        artifact_keys,
        [
            "artifactId",
            "byteLength",
            "kind",
            "mediaType",
            "runId",
            "sha256",
            "storagePath"
        ]
    );

    // Client B lists artifacts — should only see their own
    let list_b = client_b.call(31, "artifact/list", json!({})).await;
    let artifacts_b: Vec<String> = list_b["result"]["artifacts"]
        .as_array()
        .expect("artifacts is an array")
        .iter()
        .filter_map(|a| {
            a.get("artifactId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert!(
        artifacts_b.contains(&artifact_b.to_string()),
        "Client B should see their own artifact, got: {artifacts_b:?}"
    );
    assert!(
        !artifacts_b.contains(&artifact_a.to_string()),
        "Client B should NOT see Client A's artifact, got: {artifacts_b:?}"
    );

    // Client A tries to fetch B's artifact — should fail
    let fetch_cross = client_a
        .call(
            32,
            "artifact/fetch",
            json!({ "artifactId": artifact_b.to_string() }),
        )
        .await;
    assert!(
        fetch_cross.get("error").is_some(),
        "Client A should NOT be able to fetch Client B's artifact, got: {fetch_cross:?}"
    );
}

/// Seeds an artifact in the store with the given body and run_id.
async fn seed_artifact(
    store: &ArtifactStore,
    body: &str,
    run_id: Option<String>,
) -> batman_protocol::ArtifactId {
    let content = body.as_bytes().to_vec();
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let artifact = batman_protocol::Artifact {
        artifact_id: batman_protocol::ArtifactId::new(),
        kind: batman_protocol::ArtifactKind::Patch,
        sha256: format!("{:x}", hasher.finalize()),
        byte_length: content.len() as u64,
        media_type: "application/x-git-diff".to_string(),
        storage_path: format!("patches/{}.patch", content.len()),
        run_id,
    };
    store.store(artifact, content).await.unwrap()
}

// -------------------------------------------------------- R77: run-lifecycle authority
//
// R76 closed `task/upsert`'s ownership hole, but the same review (its W2
// finding) found that ownership gates *decisions* -- `approval/decide`,
// `policy/violation/decide`, `reconcile/omp` -- and never the run
// lifecycle itself. `OrchestrationService::dispatch` calls
// `run_submit`, `run_retry`, `run_cancel`, `message_send`,
// `workspace_acquire`, and `coordination_child_decide` with `params`
// only -- no `principal` -- so any connected `ompExtension` instance can
// mutate a run or task it does not own, without ever seizing it via
// `task/upsert`. Each RED test below proves one such mutation succeeds
// today against another instance's task/run and must instead be refused
// `-32602`, leaving state and the journal untouched.

/// Total number of durable events replayed for this project from the
/// beginning. A refused mutation must leave this unchanged: nothing may
/// reach the journal from an unauthorized branch.
async fn event_count(client: &mut Client, id: i64) -> usize {
    client
        .call(id, "events/replay", json!({ "afterSequence": 0 }))
        .await["result"]
        .as_array()
        .expect("events/replay returns an array")
        .len()
}

/// Seeds a pending `coordination/child/decide`-able request directly
/// through [`DomainRepository::request_child`], bypassing
/// `coordination/requestChild` entirely: that RPC is dispatched through
/// a distinct `workerMcp`-scoped path (`CoordinationBroker`, gated by a
/// minted `ScopeTokenStore` token -- see `crates/runtime/tests/coordination.rs`'s
/// `seed_scoped_run`) that this file's `Harness` does not wire up.
/// `request_child` is `pub` and is exactly what that path calls once a
/// worker's scope token has been verified, so this reaches the same
/// `waitingPeer` state and `ChildWorkerRequested` journal entry a real
/// request would, without duplicating a second harness here.
async fn seed_pending_child_request(harness: &Harness, run_id: RunId, reason: &str) {
    let db = Arc::new(
        DatabaseHandle::start(harness.database.clone())
            .await
            .unwrap(),
    );
    let project_id = harness.project_id;
    let reason = reason.to_string();
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.request_child(run_id, &reason).map(|_| Value::Null)
    }))
    .await
    .expect("seeding a pending child request must succeed");
}

/// RED: `run_submit` (`orchestration.rs`'s `dispatch` calls
/// `self.run_submit(params).await` with no `principal`) never checks
/// that the caller owns `taskId`. A second, unrelated instance can
/// submit -- and, with a driver installed, actually start -- a run
/// against a task it does not own.
#[tokio::test]
async fn run_submit_against_another_instances_task_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let task = owner
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();

    let mut attacker = omp_client(&harness, "omp-2").await;
    let attacker_worker = attacker
        .call(
            2,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let attacker_worker_id = attacker_worker["result"]["workerId"]
        .as_str()
        .unwrap()
        .to_string();

    let before = event_count(&mut owner, 3).await;

    let submit = attacker
        .call(
            3,
            "run/submit",
            json!({ "taskId": task_id, "workerId": attacker_worker_id }),
        )
        .await;
    assert_eq!(
        submit["error"]["code"], -32602,
        "run/submit against another instance's task must be refused: {submit:?}"
    );

    let runs = owner
        .call(4, "run/list", json!({ "taskId": task_id }))
        .await;
    assert_eq!(
        runs["result"]["runs"].as_array().unwrap().len(),
        0,
        "a refused submit must not have created a run: {runs:?}"
    );

    let after = event_count(&mut owner, 5).await;
    assert_eq!(
        after, before,
        "a refused submit must journal nothing: before {before}, after {after}"
    );
}

/// RED: `run_cancel` (`dispatch` calls `self.run_cancel(params).await`
/// with no `principal`) never checks that the caller owns the run's
/// task. A second, unrelated instance can cancel a run it does not own.
#[tokio::test]
async fn run_cancel_against_another_instances_run_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let mut attacker = omp_client(&harness, "omp-2").await;
    let before = event_count(&mut owner, 5).await;

    let cancel = attacker
        .call(2, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert_eq!(
        cancel["error"]["code"], -32602,
        "run/cancel by a non-owner must be refused: {cancel:?}"
    );

    let get = owner.call(6, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get["result"]["state"], "working",
        "a refused cancel must leave the run's state untouched: {get:?}"
    );

    let after = event_count(&mut owner, 7).await;
    assert_eq!(
        after, before,
        "a refused cancel must journal nothing: before {before}, after {after}"
    );

    // W3 precedence pin: the owner now genuinely cancels, reaching a
    // terminal state -- the observable contract of the reordering in
    // `transition_run` (R77) is that a *non-owner's* cancel of that
    // terminal run still classifies as `NotOwner` (-32602), not
    // `ILLEGAL_TRANSITION` (-32100), because authorization is now
    // checked before `check_transition` runs. Contrast the *owner's*
    // own second cancel of a settled run
    // (`run_cancel_on_settled_run_is_illegal_transition`), which is
    // still -32100: that caller clears ownership, so validity is what's
    // left to fail.
    let owner_cancel = owner
        .call(8, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        owner_cancel.get("error").is_none(),
        "owner's own cancel must succeed, reaching a terminal state: {owner_cancel:?}"
    );

    let cancel_terminal = attacker
        .call(3, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert_eq!(
        cancel_terminal["error"]["code"], -32602,
        "a non-owner's cancel of an already-terminal run must classify as NotOwner, not ILLEGAL_TRANSITION: {cancel_terminal:?}"
    );
}

/// RED: `run_retry` (`dispatch` calls `self.run_retry(params).await`
/// with no `principal`) never checks that the caller owns the prior
/// run's task. A second, unrelated instance can retry a terminal run
/// belonging to another instance's task under its own `WorkerId`.
#[tokio::test]
async fn run_retry_against_another_instances_task_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    // Owner cancels its own run to reach a terminal state -- legitimate,
    // exercises none of R77's guarded paths.
    let cancel = owner
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "owner's own cancel failed: {cancel:?}"
    );

    let mut attacker = omp_client(&harness, "omp-2").await;
    let attacker_worker = attacker
        .call(
            2,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let attacker_worker_id = attacker_worker["result"]["workerId"]
        .as_str()
        .unwrap()
        .to_string();

    let before = event_count(&mut owner, 6).await;

    let retry = attacker
        .call(
            3,
            "run/retry",
            json!({ "priorRunId": run_id, "workerId": attacker_worker_id }),
        )
        .await;
    assert_eq!(
        retry["error"]["code"], -32602,
        "run/retry against another instance's task must be refused: {retry:?}"
    );

    let runs = owner
        .call(7, "run/list", json!({ "taskId": task_id }))
        .await;
    assert_eq!(
        runs["result"]["runs"].as_array().unwrap().len(),
        1,
        "a refused retry must not have created a new run: {runs:?}"
    );

    let after = event_count(&mut owner, 8).await;
    assert_eq!(
        after, before,
        "a refused retry must journal nothing: before {before}, after {after}"
    );
}

/// RED: `message_send` (`dispatch` calls `self.message_send(params).await`
/// with no `principal`) never checks that the caller owns the run's
/// task. A second, unrelated instance can inject a message into another
/// instance's run, purportedly sent by that run's own worker.
#[tokio::test]
async fn message_send_against_another_instances_run_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (task_id, worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let mut attacker = omp_client(&harness, "omp-2").await;
    let before = event_count(&mut owner, 5).await;

    let send = attacker
        .call(
            2,
            "message/send",
            json!({
                "runId": run_id,
                "senderWorkerId": worker_id,
                "taskId": task_id,
                "kind": "question",
                "payload": "I am not the owner",
            }),
        )
        .await;
    assert_eq!(
        send["error"]["code"], -32602,
        "message/send against another instance's run must be refused: {send:?}"
    );

    let list = owner
        .call(6, "message/list", json!({ "runId": run_id }))
        .await;
    assert_eq!(
        list["result"]["messages"].as_array().unwrap().len(),
        0,
        "a refused send must not have recorded a message: {list:?}"
    );

    let after = event_count(&mut owner, 7).await;
    assert_eq!(
        after, before,
        "a refused send must journal nothing: before {before}, after {after}"
    );
}

/// RED: `workspace_acquire` (`dispatch` calls
/// `self.workspace_acquire(params).await` with no `principal`) never
/// checks that the caller owns the run's task. A second, unrelated
/// instance can acquire a workspace lease scoped to another instance's
/// run.
#[tokio::test]
async fn workspace_acquire_against_another_instances_run_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let mut attacker = omp_client(&harness, "omp-2").await;

    let acquire = attacker
        .call(
            2,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "readOnly", "requestedIsolation": "shared" }),
        )
        .await;
    assert_eq!(
        acquire["error"]["code"], -32602,
        "workspace/acquire against another instance's run must be refused: {acquire:?}"
    );

    let replay = owner
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert!(
        workspace_event_kinds_for_run(&replay, &run_id).is_empty(),
        "a refused acquire must not have journaled any workspace event: {replay:?}"
    );

    // W2: the journal assertion above cannot distinguish "checked before
    // acquiring" from "checked after acquiring, with the leaked lease
    // never journaled" -- leases live in `LeaseService`'s own DB, not
    // the runs DB the journal assertion reads. `run/get` reads the lease
    // DB directly (`lease_service.active_for_run`, orchestration.rs
    // :924-926), so a leaked lease would show up here even though it
    // never reached the journal.
    let get = owner.call(7, "run/get", json!({ "runId": run_id })).await;
    assert!(
        get["result"]["workspacePath"].is_null(),
        "a refused acquire must not have allocated a lease either: {get:?}"
    );
}

/// RED: `coordination_child_decide` (`dispatch` calls
/// `self.coordination_child_decide(params).await` with no `principal`)
/// never checks that the caller owns the parent run's task. A second,
/// unrelated instance can answer a `coordination/requestChild` raised on
/// another instance's run.
#[tokio::test]
async fn coordination_child_decide_against_another_instances_run_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id_str) = submit_run_with_driver(&mut owner, "omp-1").await;
    let run_id = RunId::parse(&run_id_str).unwrap();
    seed_pending_child_request(&harness, run_id, "need help").await;

    let mut attacker = omp_client(&harness, "omp-2").await;
    let before = event_count(&mut owner, 6).await;

    let decide = attacker
        .call(
            2,
            "coordination/child/decide",
            json!({ "parentRunId": run_id_str, "decision": "deny", "reason": "not yours" }),
        )
        .await;
    assert_eq!(
        decide["error"]["code"], -32602,
        "coordination/child/decide against another instance's run must be refused: {decide:?}"
    );

    let get = owner
        .call(7, "run/get", json!({ "runId": run_id_str }))
        .await;
    assert_eq!(
        get["result"]["state"], "waitingPeer",
        "a refused decide must leave the run awaiting its own owner's decision: {get:?}"
    );

    let after = event_count(&mut owner, 8).await;
    assert_eq!(
        after, before,
        "a refused decide must journal nothing: before {before}, after {after}"
    );

    // W1: the deny arm above never exercises the accept arm's ownership
    // check (`decide_child`'s guarded write, reached via
    // `coordination_child_decide`'s `"accept"` branch,
    // orchestration.rs:2078) -- deleting that check alone would leave
    // this test green without it. An attacker's "accept" with its own,
    // fabricated child ids must be refused identically to "deny".
    let accept = attacker
        .call(
            3,
            "coordination/child/decide",
            json!({
                "parentRunId": run_id_str,
                "decision": "accept",
                "childTaskId": TaskId::new().to_string(),
                "childWorkerId": WorkerId::new().to_string(),
                "childRunId": RunId::new().to_string(),
            }),
        )
        .await;
    assert_eq!(
        accept["error"]["code"], -32602,
        "coordination/child/decide accept against another instance's run must be refused: {accept:?}"
    );

    let get_after_accept = owner
        .call(9, "run/get", json!({ "runId": run_id_str }))
        .await;
    assert_eq!(
        get_after_accept["result"]["state"], "waitingPeer",
        "a refused accept must leave the run awaiting its own owner's decision: {get_after_accept:?}"
    );

    let after_accept = event_count(&mut owner, 10).await;
    assert_eq!(
        after_accept, before,
        "a refused accept must journal nothing: before {before}, after {after_accept}"
    );
}

/// GREEN guard for R81/W2: no test anywhere exercised a successful
/// `coordination/child/decide` "accept" -- the R77 GREEN chain below
/// only covers "deny", and the attacker "accept" case above is refused
/// before `decide_child`'s event is ever built. Swapping `Accept` for
/// `ChildWorkerRequestDenied`, or dropping the three `Some(..)` child
/// ids, in `DomainRepository::decide_child` would leave the whole suite
/// green without this pinning the accept arm's actual output.
#[tokio::test]
async fn owner_accepting_a_child_request_journals_the_child_ids_and_returns_the_parent_to_working()
{
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id_str) = submit_run_with_driver(&mut owner, "omp-1").await;
    let run_id = RunId::parse(&run_id_str).unwrap();
    seed_pending_child_request(&harness, run_id, "need help").await;

    let child_task_id = TaskId::new().to_string();
    let child_worker_id = WorkerId::new().to_string();
    let child_run_id = RunId::new().to_string();
    let accept = owner
        .call(
            6,
            "coordination/child/decide",
            json!({
                "parentRunId": run_id_str,
                "decision": "accept",
                "childTaskId": child_task_id,
                "childWorkerId": child_worker_id,
                "childRunId": child_run_id,
            }),
        )
        .await;
    assert!(
        accept.get("error").is_none(),
        "owner's own coordination/child/decide accept failed: {accept:?}"
    );

    let get = owner
        .call(7, "run/get", json!({ "runId": run_id_str }))
        .await;
    assert_eq!(
        get["result"]["state"], "working",
        "an accepted child request must return the parent run to working: {get:?}"
    );

    let replay = owner
        .call(8, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let events = replay["result"].as_array().unwrap();
    let recorded = events
        .iter()
        .map(|e| &e["event"])
        .rfind(|e| e["type"] == "childEvent" && e["payload"]["parentRunId"] == run_id_str)
        .expect("an accepted decide must journal a childEvent")
        .clone();
    // `seed_pending_child_request` above already journaled one
    // `childEvent` (the request itself, kind `childWorkerRequested`, with
    // no child ids yet). The accept must journal its OWN kind (R83): a
    // consumer must never have to infer "accepted" from whether the child
    // ids happen to be populated.
    assert_eq!(
        recorded["payload"]["kind"], "childWorkerAccepted",
        "an accept must journal childWorkerAccepted, distinguishable from the request: \
         {recorded:?}"
    );
    assert_eq!(recorded["payload"]["childTaskId"], child_task_id);
    assert_eq!(recorded["payload"]["childWorkerId"], child_worker_id);
    assert_eq!(recorded["payload"]["childRunId"], child_run_id);
}

/// GREEN guard for R77: every guarded mutation above must still succeed
/// when the caller genuinely owns the task/run it targets, so the
/// eventual fix (threading `principal` and arbitrating task ownership
/// inside each guarded write) cannot pass by universally refusing.
/// Chains all six methods on one owning connection: submit (via
/// `submit_run_with_driver`), workspace/acquire, message/send,
/// coordination/child/decide, run/cancel, and run/retry.
#[tokio::test]
async fn owner_can_perform_every_guarded_run_lifecycle_mutation_on_its_own_task() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (task_id, worker_id, run_id_str) = submit_run_with_driver(&mut owner, "omp-1").await;
    let run_id = RunId::parse(&run_id_str).unwrap();

    let acquire = owner
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id_str, "mode": "readOnly", "requestedIsolation": "shared" }),
        )
        .await;
    assert!(
        acquire.get("error").is_none(),
        "owner's own workspace/acquire failed: {acquire:?}"
    );

    let send = owner
        .call(
            6,
            "message/send",
            json!({
                "runId": run_id_str,
                "senderWorkerId": worker_id,
                "taskId": task_id,
                "kind": "question",
                "payload": "status?",
            }),
        )
        .await;
    assert!(
        send.get("error").is_none(),
        "owner's own message/send failed: {send:?}"
    );

    seed_pending_child_request(&harness, run_id, "need help").await;
    let decide = owner
        .call(
            7,
            "coordination/child/decide",
            json!({ "parentRunId": run_id_str, "decision": "deny", "reason": "not needed" }),
        )
        .await;
    assert!(
        decide.get("error").is_none(),
        "owner's own coordination/child/decide failed: {decide:?}"
    );
    let get_after_decide = owner
        .call(8, "run/get", json!({ "runId": run_id_str }))
        .await;
    assert_eq!(
        get_after_decide["result"]["state"], "working",
        "a decided run must return to working: {get_after_decide:?}"
    );

    let cancel = owner
        .call(9, "run/cancel", json!({ "runId": run_id_str }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "owner's own run/cancel failed: {cancel:?}"
    );

    let retry = owner
        .call(
            10,
            "run/retry",
            json!({ "priorRunId": run_id_str, "workerId": worker_id }),
        )
        .await;
    assert!(
        retry.get("error").is_none(),
        "owner's own run/retry failed: {retry:?}"
    );
    let new_run_id = retry["result"]["runId"].as_str().unwrap().to_string();
    assert_ne!(new_run_id, run_id_str, "retry must create a distinct RunId");
}

// -------------------------------------------------------- R81: workspace lease authority
//
// ReviewR77's N1 finding: `workspace_acquire` (R77, above) is owner-gated,
// but its four siblings -- `workspace_get`, `workspace_release`,
// `workspace_inspect`, `workspace_apply` (`orchestration.rs` ~1403, ~1421,
// ~1476, ~1533) -- take no `principal` and resolve their target purely
// from a caller-supplied `leaseId` via `LeaseService::get`. A non-owning
// `ompExtension` instance that learns another instance's `leaseId`
// (trivially available: `events/replay` is itself ungated, and the
// `LeaseRequested`/`LeaseAcquired` events it returns carry the `leaseId`
// in the clear) can release/tear down, inspect, or apply into a
// workspace it never acquired.
//
// `workspace_get` closes no confidentiality disclosure by being gated --
// every field its response carries (`leaseId`, `runId`, `mode`,
// `isolationKind`, `path`, `baseRevision`, `state`) is already
// reproducible from that same already-ungated `events/replay` (see
// `OrchestrationService::workspace_get`'s doc comment). It is gated
// anyway, through the same `Self::require_lease_owner` the three
// mutating siblings below share, and ReviewR81's W4 flagged it as the
// one lease-scoped gate with no test in either direction: deleting it
// left the whole suite green. The RED test below pins the gate itself,
// not a new disclosure; owner-success for all four methods is pinned in
// `workspace_release_by_the_owning_instance_succeeds` below.

/// RED: `workspace_get` resolves its response purely from a
/// caller-supplied `leaseId`, with no check that `principal` owns the
/// run it belongs to (ReviewR81 W4).
#[tokio::test]
async fn workspace_get_against_another_instances_lease_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let acquire = owner
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "readOnly", "requestedIsolation": "shared" }),
        )
        .await;
    assert!(
        acquire.get("error").is_none(),
        "owner's own workspace/acquire failed: {acquire:?}"
    );
    let lease_id = acquire["result"]["leaseId"].as_str().unwrap().to_string();

    let mut attacker = omp_client(&harness, "omp-2").await;
    let get = attacker
        .call(2, "workspace/get", json!({ "leaseId": lease_id }))
        .await;
    assert_eq!(
        get["error"]["code"], -32602,
        "workspace/get against another instance's lease must be refused: {get:?}"
    );
    assert!(
        get.get("result").is_none(),
        "a refused get must not disclose the lease's path/mode/state: {get:?}"
    );
    // The refusal message must be byte-identical to the unknown-lease
    // refusal (see the test below), so message text is not an existence
    // oracle and never leaks the owning task/instance ids (R84 review E1).
    assert_eq!(
        get["error"]["message"], "leaseId is not a lease on a run you own",
        "{get:?}"
    );
}

/// R84: an unknown `leaseId` must be the same caller error (`-32602`) as
/// an unowned one -- reporting `-32603` both misclassified the error and
/// let a caller distinguish "exists but not yours" from "does not exist"
/// by error code.
#[tokio::test]
async fn workspace_get_with_an_unknown_lease_id_is_invalid_params_not_internal() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;

    let get = client
        .call(
            2,
            "workspace/get",
            json!({ "leaseId": uuid::Uuid::now_v7().to_string() }),
        )
        .await;
    assert_eq!(
        get["error"]["code"], -32602,
        "an unknown leaseId is a caller error, not an internal one: {get:?}"
    );
    // Byte-identical to the unowned-lease refusal above: neither the code
    // nor the message distinguishes "exists but not yours" from "does not
    // exist" (R84 review E1).
    assert_eq!(
        get["error"]["message"], "leaseId is not a lease on a run you own",
        "{get:?}"
    );
}

/// RED: `workspace_release` (`dispatch` calls
/// `self.workspace_release(params).await` with no `principal`) never
/// checks that the caller owns the lease's run. A second, unrelated
/// instance can release -- and, for isolated leases, tear down the
/// worktree of -- a lease it never acquired.
#[tokio::test]
async fn workspace_release_against_another_instances_lease_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let acquire = owner
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "readOnly", "requestedIsolation": "shared" }),
        )
        .await;
    assert!(
        acquire.get("error").is_none(),
        "owner's own workspace/acquire failed: {acquire:?}"
    );
    let lease_id = acquire["result"]["leaseId"].as_str().unwrap().to_string();

    let mut attacker = omp_client(&harness, "omp-2").await;
    let release = attacker
        .call(2, "workspace/release", json!({ "leaseId": lease_id }))
        .await;
    assert_eq!(
        release["error"]["code"], -32602,
        "workspace/release against another instance's lease must be refused: {release:?}"
    );

    let get = owner.call(6, "run/get", json!({ "runId": run_id })).await;
    assert!(
        get["result"]["workspacePath"].as_str().is_some(),
        "a refused release must leave the lease active: {get:?}"
    );

    let replay = owner
        .call(7, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert!(
        !workspace_event_kinds_for_run(&replay, &run_id).contains(&"leaseReleased".to_string()),
        "a refused release must not journal LeaseReleased: {replay:?}"
    );
}

/// RED (R89): the request side speaks the closed vocabulary
/// shared|isolated|copy, but the response side collapses `copy` to
/// `"isolated"` in both `run/submit`'s echo and `run/get`'s lease
/// projection. A caller submitting `workspaceMode: "copy"` must read
/// back `"copy"` from both.
#[tokio::test]
async fn a_copy_workspace_echoes_copy_not_isolated() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    init_real_git_repo(&harness.owned_dir);
    let mut client = omp_client(&harness, "omp-1").await;

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id, "workspaceMode": "copy" }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "copy submit against a real git repo must succeed: {submit:?}"
    );
    assert_eq!(
        submit["result"]["workspaceMode"], "copy",
        "run/submit must echo the mode the caller asked for, not collapse \
         copy to isolated: {submit:?}"
    );
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    let get = client.call(5, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        get["result"]["workspaceMode"], "copy",
        "run/get must project the lease's real isolation kind: {get:?}"
    );
}

/// GREEN guard for R81: the owner using its own lease must still
/// succeed once ownership arbitration is threaded into
/// `workspace_get`, `workspace_inspect`, and `workspace_release` -- a
/// universal-refusal fix must not pass any of them either (W4).
#[tokio::test]
async fn workspace_release_by_the_owning_instance_succeeds() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    init_real_git_repo(&harness.owned_dir);
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let acquire = owner
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "readOnly", "requestedIsolation": "shared" }),
        )
        .await;
    assert!(
        acquire.get("error").is_none(),
        "owner's own workspace/acquire failed: {acquire:?}"
    );
    let lease_id = acquire["result"]["leaseId"].as_str().unwrap().to_string();

    let get = owner
        .call(6, "workspace/get", json!({ "leaseId": lease_id }))
        .await;
    assert!(
        get.get("error").is_none(),
        "owner's own workspace/get failed: {get:?}"
    );
    assert_eq!(get["result"]["leaseId"], lease_id);
    assert_eq!(get["result"]["state"], "active");
    // Pin the exact wire key set (R55 review W3): the handler serializes the
    // canonical `WorkspaceInfo`, and a `skip_serializing_if`/rename/added
    // field would silently change the wire and fail the extension's
    // additionalProperties:false validation with CI otherwise green.
    let mut get_keys: Vec<&str> = get["result"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    get_keys.sort_unstable();
    assert_eq!(
        get_keys,
        [
            "baseRevision",
            "isolationKind",
            "leaseId",
            "mode",
            "path",
            "runId",
            "state"
        ]
    );

    let inspect = owner
        .call(7, "workspace/inspect", json!({ "leaseId": lease_id }))
        .await;
    assert!(
        inspect.get("error").is_none(),
        "owner's own workspace/inspect failed: {inspect:?}"
    );
    assert_eq!(inspect["result"]["commitCount"], 1);
    assert_eq!(inspect["result"]["dirtyFileCount"], 0);
    // Pin `InspectResult`'s exact wire key set (R55 review W3).
    let mut inspect_keys: Vec<&str> = inspect["result"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    inspect_keys.sort_unstable();
    assert_eq!(
        inspect_keys,
        [
            "baseRevision",
            "commitCount",
            "commitIds",
            "currentRevision",
            "dirtyFileCount",
            "leaseId",
            "patchArtifactId",
            "untrackedFileCount"
        ]
    );

    let release = owner
        .call(8, "workspace/release", json!({ "leaseId": lease_id }))
        .await;
    assert!(
        release.get("error").is_none(),
        "owner's own workspace/release failed: {release:?}"
    );
    assert_eq!(release["result"]["released"], true);

    let replay = owner
        .call(9, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert_eq!(
        workspace_event_kinds_for_run(&replay, &run_id)
            .last()
            .map(String::as_str),
        Some("leaseReleased"),
        "owner's own release must journal LeaseReleased: {replay:?}"
    );
}

/// RED: `workspace_inspect` (`dispatch` calls
/// `self.workspace_inspect(params).await` with no `principal`) never
/// checks that the caller owns the lease's run. A second, unrelated
/// instance can inspect another instance's real workspace -- reading
/// back commit history and dirty/untracked counts, and materializing a
/// fetchable patch artifact of its contents -- for a lease it never
/// acquired.
#[tokio::test]
async fn workspace_inspect_against_another_instances_lease_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    init_real_git_repo(&harness.owned_dir);
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let acquire = owner
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "readOnly", "requestedIsolation": "shared" }),
        )
        .await;
    assert!(
        acquire.get("error").is_none(),
        "owner's own workspace/acquire failed: {acquire:?}"
    );
    let lease_id = acquire["result"]["leaseId"].as_str().unwrap().to_string();

    let mut attacker = omp_client(&harness, "omp-2").await;
    let inspect = attacker
        .call(2, "workspace/inspect", json!({ "leaseId": lease_id }))
        .await;
    assert_eq!(
        inspect["error"]["code"], -32602,
        "workspace/inspect against another instance's lease must be refused: {inspect:?}"
    );
    assert!(
        inspect.get("result").is_none(),
        "a refused inspect must not return a patch artifact or revision evidence: {inspect:?}"
    );

    let replay = owner
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert!(
        workspace_event_kinds_for_run(&replay, &run_id)
            .iter()
            .all(|k| k != "workspaceInspected" && k != "artifactPublished"),
        "a refused inspect must not journal WorkspaceInspected/ArtifactPublished: {replay:?}"
    );
}

/// RED: `workspace_apply` (`dispatch` calls
/// `self.workspace_apply(params).await` with no `principal`) never
/// checks that the caller owns the lease's run. Ownership must be
/// arbitrated immediately after `lease_service.get` and before any
/// artifact resolution, so a non-owner is refused `-32602` before ever
/// learning whether the artifact it named exists. A bogus, never-seeded
/// artifact ref is deliberately used here: seeding a real one is no
/// harder (`seed_artifact` above), but would not distinguish "refused for
/// ownership" from "refused because the artifact could not be resolved
/// yet" -- the two errors this test's message assertion tells apart.
/// Pre-R81, absent any ownership check, the handler ran the (since
/// deleted) caller-side quarantine pre-check, journaled `ApplyStarted`,
/// and only then failed resolving the artifact -- a mutation reached the
/// journal from an unauthorized caller before the request was refused at
/// all. The gate now refuses first; the quarantine read lives inside the
/// same domain op (R78).
#[tokio::test]
async fn workspace_apply_against_another_instances_lease_is_refused() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let acquire = owner
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "write", "requestedIsolation": "shared" }),
        )
        .await;
    assert!(
        acquire.get("error").is_none(),
        "owner's own workspace/acquire failed: {acquire:?}"
    );
    let lease_id = acquire["result"]["leaseId"].as_str().unwrap().to_string();
    let base_revision = acquire["result"]["baseRevision"]
        .as_str()
        .unwrap()
        .to_string();

    let mut attacker = omp_client(&harness, "omp-2").await;
    let bogus_artifact_id = batman_protocol::ArtifactId::new().to_string();
    let apply = attacker
        .call(
            2,
            "workspace/apply",
            json!({
                "leaseId": lease_id,
                "strategy": "applyPatch",
                "artifactId": bogus_artifact_id,
                "expectedTargetRevision": base_revision,
            }),
        )
        .await;
    assert_eq!(
        apply["error"]["code"], -32602,
        "workspace/apply against another instance's lease must be refused before artifact \
         resolution: {apply:?}"
    );
    assert_eq!(
        apply["error"]["message"], "leaseId is not a lease on a run you own",
        "the refusal must be the uniform lease refusal (R84 review E1) -- an ownership \
         refusal that named the owning task would leak it, and artifact-not-found would \
         prove artifact resolution ran before the gate: {apply:?}"
    );

    let replay = owner
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert!(
        workspace_event_kinds_for_run(&replay, &run_id)
            .iter()
            .all(|k| k != "applyStarted"),
        "a refused apply must not journal ApplyStarted: {replay:?}"
    );
}

/// GREEN guard for R81: the owner applying against its own lease must
/// still pass the ownership gate and reach artifact resolution -- the
/// `-32603` from `WorkspaceApplier` failing on a bogus, never-seeded
/// artifact is the proof a wrongly-universal refusal would instead
/// surface as `-32602` before this point is ever reached (W4).
#[tokio::test]
async fn workspace_apply_by_the_owning_instance_reaches_artifact_resolution() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut owner = omp_client(&harness, "omp-1").await;
    let (_task_id, _worker_id, run_id) = submit_run_with_driver(&mut owner, "omp-1").await;

    let acquire = owner
        .call(
            5,
            "workspace/acquire",
            json!({ "runId": run_id, "mode": "write", "requestedIsolation": "shared" }),
        )
        .await;
    assert!(
        acquire.get("error").is_none(),
        "owner's own workspace/acquire failed: {acquire:?}"
    );
    let lease_id = acquire["result"]["leaseId"].as_str().unwrap().to_string();
    let base_revision = acquire["result"]["baseRevision"]
        .as_str()
        .unwrap()
        .to_string();

    let bogus_artifact_id = batman_protocol::ArtifactId::new().to_string();
    let apply = owner
        .call(
            6,
            "workspace/apply",
            json!({
                "leaseId": lease_id,
                "strategy": "applyPatch",
                "artifactId": bogus_artifact_id,
                "expectedTargetRevision": base_revision,
            }),
        )
        .await;
    assert_eq!(
        apply["error"]["code"], -32603,
        "the owner's own apply must pass the ownership gate and fail on artifact resolution, \
         not ownership: {apply:?}"
    );

    let replay = owner
        .call(7, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert!(
        workspace_event_kinds_for_run(&replay, &run_id).contains(&"applyStarted".to_string()),
        "an owner's apply attempt must journal ApplyStarted before artifact resolution fails, \
         proving the gate did not refuse it: {replay:?}"
    );
}
