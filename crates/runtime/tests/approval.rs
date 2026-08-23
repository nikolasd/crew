//! Integration tests for the correlated approval flow: ownership
//! enforcement, idempotency, settled-run rejection, callback semantics,
//! and reconciliation-driven ownership rebinding.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use batman_protocol::{
    ApprovalId, ApprovalRequest, DecidedBy, ProjectId, RunId, TaskId, Timestamp, WorkerId,
};
use batman_runtime::approval::{
    ApprovalCallback, ApprovalService, CallbackFuture, NoopApprovalCallback,
};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::ipc::{PeerCredentialReader, PeerCredentials, Server, ServerConfig};
use batman_runtime::paths::RuntimePaths;
use batman_runtime::service::FakeRunDriver;
use nix::unistd::Uid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{broadcast, oneshot};
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

fn matching_reader() -> Arc<dyn PeerCredentialReader> {
    Arc::new(FakeReader {
        uid: Some(Uid::current().as_raw()),
    })
}

/// An [`ApprovalCallback`] that always fails, for testing the
/// `protocolUnhealthy` path.
struct FailingCallback;

impl ApprovalCallback for FailingCallback {
    fn acknowledge(&self, _approval_id: ApprovalId, _decision: &str) -> CallbackFuture<'static> {
        Box::pin(async { Err("adapter unreachable".to_string()) })
    }
}

/// An [`ApprovalCallback`] that records every call, for asserting a
/// duplicate identical decision never re-invokes the adapter.
struct CountingCallback {
    calls: Arc<std::sync::atomic::AtomicU32>,
}

impl ApprovalCallback for CountingCallback {
    fn acknowledge(&self, _approval_id: ApprovalId, _decision: &str) -> CallbackFuture<'static> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
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
            .prefix("bat-ap-s-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = tempfile::Builder::new()
            .prefix("bat-ap-r-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();

        let paths = RuntimePaths::resolve(state.path(), repo.path()).unwrap();
        let db = Arc::new(DatabaseHandle::start(paths.database.clone()).await.unwrap());

        let mut config = ServerConfig {
            credential_reader: matching_reader(),
            run_driver: Some(Arc::new(FakeRunDriver)),
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

        for _ in 0..50 {
            if UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

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

    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        self.recv().await
    }
}

async fn omp_client(harness: &Harness, instance_id: &str) -> Client {
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "client": { "name": "@nikolasd/crew", "version": "0.1.0" },
                "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
                "repository": { "canonicalPath": harness.owned_dir, "vcsRoot": harness.owned_dir },
                "auth": { "role": "ompExtension", "instanceId": instance_id, "agentDirectory": harness.owned_dir },
                "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
                "lastSequence": null
            }
        }))
        .await;
    let init = client.recv().await;
    assert!(init.get("error").is_none(), "initialize failed: {init:?}");
    client
}

/// Creates a task/worker/run through `omp`, drives the run to `working`
/// (FakeRunDriver reaches it automatically), then directly invokes
/// [`ApprovalService::request`] against the harness's own database to
/// simulate "an adapter reports it needs approval".
async fn seed_pending_approval(
    harness: &Harness,
    omp: &mut Client,
    owner_instance_id: &str,
) -> (ApprovalId, RunId, TaskId) {
    let task = omp
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": owner_instance_id, "revision": 1 }),
        )
        .await;
    let task_id = TaskId::parse(task["result"]["taskId"].as_str().unwrap()).unwrap();
    let worker = omp
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = WorkerId::parse(worker["result"]["workerId"].as_str().unwrap()).unwrap();
    let submit = omp
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id.to_string(), "workerId": worker_id.to_string() }),
        )
        .await;
    let run_id = RunId::parse(submit["result"]["runId"].as_str().unwrap()).unwrap();

    let db = Arc::new(
        DatabaseHandle::start(harness.database.clone())
            .await
            .unwrap(),
    );
    let (events_tx, _events_rx) = broadcast::channel(64);
    let service = ApprovalService::new(
        db,
        harness.project_id,
        Arc::new(NoopApprovalCallback),
        events_tx,
    );
    let approval_id = ApprovalId::new();
    service
        .request(ApprovalRequest {
            approval_id,
            run_id,
            task_id,
            action: "write file".to_string(),
            arguments: json!({ "path": "/tmp/x" }),
            human_required: true,
            policy_reason: "write requires human approval".to_string(),
            created_at: Timestamp::now(),
            decided_at: None,
            decision: None,
            decided_by: None,
            reason: None,
        })
        .await
        .unwrap();

    (approval_id, run_id, task_id)
}

// -------------------------------------------------------------- ownership

#[tokio::test]
async fn only_the_owning_instance_can_decide() {
    let harness = Harness::start(|_| {}).await;
    let mut owner = omp_client(&harness, "omp-owner").await;
    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut owner, "omp-owner").await;

    let mut other = omp_client(&harness, "omp-other").await;
    let denied = other
        .call(
            2,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert_eq!(
        denied["error"]["code"], -32602,
        "non-owner must be rejected: {denied:?}"
    );

    let allowed = owner
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert!(
        allowed.get("error").is_none(),
        "owner must be allowed: {allowed:?}"
    );
    assert_eq!(allowed["result"]["outcome"], "decided");
}

#[tokio::test]
async fn display_and_worker_mcp_cannot_reach_approval_decide() {
    let harness = Harness::start(|_| {}).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "client": { "name": "display", "version": "0.1.0" },
                "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
                "repository": { "canonicalPath": harness.owned_dir, "vcsRoot": harness.owned_dir },
                "auth": { "role": "display", "instanceId": "display-1" },
                "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
                "lastSequence": null
            }
        }))
        .await;
    client.recv().await;
    let result = client.call(2, "approval/decide", json!({ "approvalId": ApprovalId::new().to_string(), "decision": "approve", "reason": "x", "decidedBy": "human" })).await;
    assert_eq!(
        result["error"]["code"], -32601,
        "display must get METHOD_NOT_FOUND: {result:?}"
    );
}

// -------------------------------------------------------------- idempotency

#[tokio::test]
async fn duplicate_identical_decision_is_idempotent_and_never_recalls_the_adapter() {
    let harness = Harness::start(|_| {}).await;
    let mut owner = omp_client(&harness, "omp-owner").await;
    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut owner, "omp-owner").await;

    let first = owner
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert_eq!(first["result"]["outcome"], "decided");

    let second = owner
        .call(
            6,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert!(
        second.get("error").is_none(),
        "identical repeat must succeed: {second:?}"
    );
    assert_eq!(second["result"]["outcome"], "alreadyDecided");
}

#[tokio::test]
async fn conflicting_second_decision_fails() {
    let harness = Harness::start(|_| {}).await;
    let mut owner = omp_client(&harness, "omp-owner").await;
    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut owner, "omp-owner").await;

    let first = owner
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert_eq!(first["result"]["outcome"], "decided");

    let conflicting = owner.call(6, "approval/decide", json!({ "approvalId": approval_id.to_string(), "decision": "deny", "reason": "changed my mind", "decidedBy": "human" })).await;
    assert_eq!(conflicting["error"]["code"], -32602, "{conflicting:?}");
}

// ---------------------------------------------------------- settled runs

#[tokio::test]
async fn a_decision_cannot_target_a_settled_run() {
    let harness = Harness::start(|_| {}).await;
    let mut owner = omp_client(&harness, "omp-owner").await;
    let (approval_id, run_id, _task_id) =
        seed_pending_approval(&harness, &mut owner, "omp-owner").await;

    // Settle the run out from under the pending approval: waitingUser has
    // no direct cancel path via run/cancel (only working/starting do), so
    // decide the approval first to return it to working, then cancel.
    // To exercise "settled" specifically, transition directly through the
    // domain repository.
    let db = Arc::new(
        DatabaseHandle::start(harness.database.clone())
            .await
            .unwrap(),
    );
    {
        use batman_runtime::domain::DomainRepository;
        let mut conn = rusqlite::Connection::open(&harness.database).unwrap();
        let mut repo = DomainRepository::new(&mut conn, harness.project_id);
        let failed = batman_protocol::RunState::try_from("failed").unwrap();
        repo.transition_run(run_id, &failed, None)
            .expect("force-fail the run for this test");
    }
    drop(db);

    let result = owner
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert_eq!(
        result["error"]["code"], -32602,
        "settled run must reject the decision: {result:?}"
    );
}

// ---------------------------------------------------------- callback path

#[tokio::test]
async fn a_failed_callback_keeps_the_decision_and_marks_protocol_unhealthy() {
    let harness = Harness::start(|config| {
        config.approval_callback = Arc::new(FailingCallback);
    })
    .await;
    let mut owner = omp_client(&harness, "omp-owner").await;
    let (approval_id, run_id, _task_id) =
        seed_pending_approval(&harness, &mut owner, "omp-owner").await;

    let result = owner
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert!(result.get("error").is_none(), "{result:?}");
    assert_eq!(result["result"]["outcome"], "decidedCallbackFailed");

    // The decision itself was kept, and it never asks again: re-deciding
    // identically is a no-op.
    let repeat = owner
        .call(
            6,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert_eq!(repeat["result"]["outcome"], "alreadyDecided");

    let get = owner
        .call(7, "run/get", json!({ "runId": run_id.to_string() }))
        .await;
    assert_eq!(get["result"]["flags"]["protocolUnhealthy"], true);
    // The run was NOT transitioned back to working on a failed callback.
    assert_eq!(get["result"]["state"], "waitingUser");
}

#[tokio::test]
async fn a_successful_callback_returns_the_run_to_working() {
    let harness = Harness::start(|_| {}).await;
    let mut owner = omp_client(&harness, "omp-owner").await;
    let (approval_id, run_id, _task_id) =
        seed_pending_approval(&harness, &mut owner, "omp-owner").await;

    let result = owner
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert_eq!(result["result"]["outcome"], "decided");

    let get = owner
        .call(6, "run/get", json!({ "runId": run_id.to_string() }))
        .await;
    assert_eq!(get["result"]["state"], "working");
    assert_eq!(get["result"]["flags"]["protocolUnhealthy"], false);
}

#[tokio::test]
async fn identical_repeat_decision_never_re_invokes_the_adapter_callback() {
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counting = Arc::new(CountingCallback {
        calls: calls.clone(),
    });

    let harness = Harness::start(|_| {}).await;
    let mut owner = omp_client(&harness, "omp-owner").await;
    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut owner, "omp-owner").await;

    // Decide directly through a dedicated ApprovalService using the
    // counting callback, bypassing the RPC layer's own Noop-configured
    // service, to isolate the "no re-invocation" assertion.
    let db = Arc::new(
        DatabaseHandle::start(harness.database.clone())
            .await
            .unwrap(),
    );
    let (events_tx, _events_rx) = broadcast::channel(64);
    let service = ApprovalService::new(db, harness.project_id, counting, events_tx);
    let first = service
        .decide(approval_id, "omp-owner", "approve", "ok", DecidedBy::Human)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        first,
        batman_runtime::approval::DecideOutcome::Decided
    ));

    let second = service
        .decide(approval_id, "omp-owner", "approve", "ok", DecidedBy::Human)
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the callback must not be invoked again"
    );
    assert!(matches!(
        second,
        batman_runtime::approval::DecideOutcome::AlreadyDecided
    ));
}

// --------------------------------------------------------- restart/reconcile

#[tokio::test]
async fn pending_request_survives_reconcile_and_only_the_rebound_owner_can_decide() {
    let harness = Harness::start(|_| {}).await;
    let mut original_owner = omp_client(&harness, "omp-1").await;
    let (approval_id, _run_id, task_id) =
        seed_pending_approval(&harness, &mut original_owner, "omp-1").await;

    // A new OMP client instance connects and reconciles ownership of the
    // task at the matching revision.
    let mut new_owner = omp_client(&harness, "omp-2").await;
    let reconcile = new_owner
        .call(
            2,
            "reconcile/omp",
            json!({ "taskId": task_id.to_string(), "revision": 1 }),
        )
        .await;
    assert!(reconcile.get("error").is_none(), "{reconcile:?}");

    // The request remains pending.
    let list = new_owner.call(3, "approval/list", json!({})).await;
    let approvals = list["result"]["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0]["decision"], Value::Null);

    // The stale disconnected owner can no longer decide.
    let stale_attempt = original_owner
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert_eq!(stale_attempt["error"]["code"], -32602, "{stale_attempt:?}");

    // Only the rebound owner can decide.
    let rebound_attempt = new_owner
        .call(
            4,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert!(
        rebound_attempt.get("error").is_none(),
        "{rebound_attempt:?}"
    );
    assert_eq!(rebound_attempt["result"]["outcome"], "decided");
}

#[tokio::test]
async fn a_human_required_approval_is_rejected_when_decided_by_the_model() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut client, "omp-1").await;

    // Decide with decidedBy: "model" — should be rejected for human_required approval.
    let result = client
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "model" }),
        )
        .await;
    assert_eq!(
        result["error"]["code"], -32602,
        "model decision on human_required approval must be rejected: {result:?}"
    );
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("requires a human decision"),
        "error message must mention human requirement: {result:?}"
    );

    // The approval is still undecided.
    let list = client.call(6, "approval/list", json!({})).await;
    let approvals = list["result"]["approvals"].as_array().unwrap();
    assert_eq!(approvals.len(), 1);
    assert!(
        approvals[0]["decision"].is_null(),
        "approval must still be undecided: {list:?}"
    );
}

#[tokio::test]
async fn a_human_required_approval_is_accepted_when_decided_by_a_human() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut client, "omp-1").await;

    // Decide with decidedBy: "human" — should succeed.
    let result = client
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "human" }),
        )
        .await;
    assert!(
        result.get("error").is_none(),
        "human decision on human_required approval must succeed: {result:?}"
    );
    assert_eq!(result["result"]["outcome"], "decided");
}

/// R92: `decided_by` has been persisted since MIGRATION_7 and `reason`
/// since MIGRATION_9, both carried on `ApprovalDecided` events -- but
/// `approval/list` projected neither, so decision provenance was readable
/// only via `events/replay` or `crewd audit export`. The list must
/// carry both for a decided approval and neither key for a pending one.
#[tokio::test]
async fn approval_list_projects_decision_provenance() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let (approval_id, run_id, _task_id) =
        seed_pending_approval(&harness, &mut client, "omp-1").await;

    // Pending: provenance keys PRESENT and null (the projection emits
    // them like decidedAt/decision) -- a serde Index would answer Null
    // for a missing key too, so pin presence explicitly.
    let pending = client
        .call(5, "approval/list", json!({ "runId": run_id.to_string() }))
        .await;
    let row = &pending["result"]["approvals"].as_array().unwrap()[0];
    let obj = row.as_object().unwrap();
    assert!(
        obj.contains_key("decidedBy") && obj["decidedBy"].is_null(),
        "decidedBy must be present and null while pending: {row:?}"
    );
    assert!(
        obj.contains_key("reason") && obj["reason"].is_null(),
        "reason must be present and null while pending: {row:?}"
    );

    let decided = client
        .call(
            6,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "reviewed the diff by hand", "decidedBy": "human" }),
        )
        .await;
    assert!(decided.get("error").is_none(), "{decided:?}");

    let list = client
        .call(7, "approval/list", json!({ "runId": run_id.to_string() }))
        .await;
    let row = &list["result"]["approvals"].as_array().unwrap()[0];
    assert_eq!(
        row["decidedBy"], "human",
        "the decider must be readable from approval/list, not only the \
         journal: {row:?}"
    );
    assert_eq!(
        row["reason"], "reviewed the diff by hand",
        "the rationale must be readable from approval/list: {row:?}"
    );
    // The wire row round-trips through the canonical deny_unknown_fields
    // type, so a renamed field fails here at test time.
    serde_json::from_value::<batman_protocol::ApprovalRequest>(row.clone())
        .expect("the list row must deserialize as the canonical ApprovalRequest");
}

#[tokio::test]
async fn an_approval_not_human_required_is_decidable_by_the_model() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut client, "omp-1").await;

    // Override the human_required flag to false in the database.
    let db = Arc::new(
        DatabaseHandle::start(harness.database.clone())
            .await
            .unwrap(),
    );
    db.run_domain_op(Box::new(move |conn| {
        let approval_id_str = approval_id.to_string();
        conn.execute(
            "UPDATE approvals SET human_required = 0 WHERE approval_id = ?1",
            rusqlite::params![approval_id_str],
        )?;
        Ok(json!({}))
    }))
    .await
    .unwrap();

    // Decide with decidedBy: "model" — should succeed since human_required is false.
    let result = client
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok", "decidedBy": "model" }),
        )
        .await;
    assert!(
        result.get("error").is_none(),
        "model decision on non-human_required approval must succeed: {result:?}"
    );
    assert_eq!(result["result"]["outcome"], "decided");
}

#[tokio::test]
async fn an_approval_without_decided_by_defaults_to_model_and_fails_human_required() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;

    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut client, "omp-1").await;

    // Omit decidedBy entirely — defaults to Model, which must be rejected for human_required.
    let result = client
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "ok" }),
        )
        .await;
    assert_eq!(
        result["error"]["code"], -32602,
        "missing decidedBy defaults to model, which must be rejected: {result:?}"
    );
}

#[tokio::test]
async fn a_decision_persists_the_bare_decided_by_token_and_the_reason() {
    // R34: `decided_by` was written via serde_json::to_string, storing the
    // JSON-quoted token `"human"` -- `WHERE decided_by = 'human'` matched
    // zero rows forever. R59: `reason` was accepted end-to-end and then
    // discarded (`let _ = reason;`) -- permanent audit-trail loss.
    let harness = Harness::start(|_| {}).await;
    let mut owner = omp_client(&harness, "omp-owner").await;
    let (approval_id, _run_id, _task_id) =
        seed_pending_approval(&harness, &mut owner, "omp-owner").await;

    let result = owner
        .call(
            5,
            "approval/decide",
            json!({ "approvalId": approval_id.to_string(), "decision": "approve", "reason": "looks good", "decidedBy": "human" }),
        )
        .await;
    assert_eq!(result["result"]["outcome"], "decided");

    let conn = rusqlite::Connection::open(&harness.database).unwrap();
    let (decided_by, reason): (String, Option<String>) = conn
        .query_row(
            "SELECT decided_by, reason FROM approvals WHERE approval_id = ?1",
            rusqlite::params![approval_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        decided_by, "human",
        "decided_by must be the bare token, not a JSON-quoted string"
    );
    assert_eq!(reason.as_deref(), Some("looks good"));
}
