//! Integration tests for the `run/result` JSON-RPC method: reads a
//! terminal run's final journaled text and folded usage totals.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use batman_protocol::{Classified, ContentClass, ProjectId, RunId, TaskId, WorkerId};
use batman_runtime::adapter::{
    Adapter, AdapterEvent, AdapterEventPayload, AdapterEventSink, CancelScope,
};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::ipc::{PeerCredentialReader, PeerCredentials, Server, ServerConfig};
use batman_runtime::paths::RuntimePaths;
use batman_runtime::service::{AdapterFuture, FakeRunDriver, RunDriver, RunDriverContext};
use nix::unistd::Uid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// --------------------------------------------------------------- harness
struct Harness {
    socket: PathBuf,
    owned_dir: PathBuf,
    _database: PathBuf,
    _project_id: ProjectId,
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
            _database: paths.database.clone(),
            _project_id: paths.project_id,
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

// ---------------------------------------------------------- seeding driver

/// A run driver that journals a fixed list of adapter events through the
/// real DomainAdapterEventSink (redaction + commit + broadcast), then
/// reports started. `sink.emit(...).await` completes inside `start`, so
/// every seeded event is durable before `run/submit` replies.
struct SeedingRunDriver {
    events: std::sync::Mutex<Option<Vec<AdapterEventPayload>>>,
    security_patterns: Vec<String>,
}

impl SeedingRunDriver {
    fn new(events: Vec<AdapterEventPayload>, security_patterns: Vec<String>) -> Self {
        Self {
            events: std::sync::Mutex::new(Some(events)),
            security_patterns,
        }
    }
}

impl RunDriver for SeedingRunDriver {
    fn active_run_count(&self) -> usize {
        0
    }

    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        let security_patterns = self.security_patterns.clone();
        let events = self.events.lock().unwrap().take().unwrap_or_default();
        Box::pin(async move {
            let sink = batman_runtime::adapter::DomainAdapterEventSink::new(
                ctx.db.clone(),
                ctx.project_id,
                ctx.events_tx.clone(),
                security_patterns,
                false, // nested_not_managed: don't trip a violation while seeding
                Arc::clone(&ctx.violation_service),
                None, // no cost ceiling: seeding usage must not quarantine the run
            )
            .expect("seed patterns always compile");
            for payload in events {
                sink.emit(AdapterEvent {
                    run_id: ctx.run_id,
                    task_id: ctx.task_id,
                    worker_id: ctx.worker_id,
                    payload,
                })
                .await
                .map_err(|e| e.to_string())?;
            }
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
        Box::pin(async move { Ok(batman_runtime::service::CancelOutcome::NoRunningAdapter) })
    }
}

/// Submits a task/worker/run through the legacy `adapter: "fake"` path,
/// returning the new run's `runId`. Ids start at 2 because `initialize`
/// consumed 1.
async fn submit_run(client: &mut Client) -> String {
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
    submit["result"]["runId"].as_str().unwrap().to_string()
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn run_result_refuses_a_run_that_is_not_terminal() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let run_id = submit_run(&mut client).await; // helper from Step 1; fake driver reaches `working`
    let resp = client
        .call(5, "run/result", json!({ "runId": run_id }))
        .await;
    assert_eq!(
        resp["error"]["code"], -32602,
        "non-terminal run must refuse: {resp:?}"
    );
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not finished"),
        "refusal must name the reason: {resp:?}"
    );
}

#[tokio::test]
async fn run_result_refuses_an_unknown_run_id() {
    let harness = Harness::start(|_| {}).await;
    let mut client = omp_client(&harness, "omp-1").await;
    let resp = client
        .call(
            2,
            "run/result",
            json!({ "runId": "00000000-0000-4000-8000-000000000000" }),
        )
        .await;
    assert_eq!(resp["error"]["code"], -32602, "{resp:?}");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not found"),
        "{resp:?}"
    );
}

#[tokio::test]
async fn run_result_returns_final_text_usage_and_state_for_a_terminal_run() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(SeedingRunDriver::new(
            vec![
                AdapterEventPayload::MessageChunk {
                    role: "assistant".to_string(),
                    text: Classified {
                        class: ContentClass::Visible,
                        value: "thinking...".to_string(),
                    },
                },
                AdapterEventPayload::MessageFinal {
                    role: "result".to_string(),
                    text: Classified {
                        class: ContentClass::Visible,
                        value: "all done: pomegranate".to_string(),
                    },
                },
                AdapterEventPayload::UsageReported {
                    input_tokens: 1_000,
                    output_tokens: 2_000,
                    cost_usd: Some(2.5),
                },
            ],
            vec![],
        )));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let run_id = submit_run(&mut client).await;
    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "cancel must succeed: {cancel:?}"
    );
    let resp = client
        .call(6, "run/result", json!({ "runId": run_id }))
        .await;
    assert!(
        resp.get("error").is_none(),
        "run/result must succeed: {resp:?}"
    );
    let r = &resp["result"];
    assert_eq!(
        r["resultText"], "all done: pomegranate",
        "final beats chunk: {r:?}"
    );
    assert_eq!(r["usage"]["inputTokens"], 1_000, "{r:?}");
    assert_eq!(r["usage"]["outputTokens"], 2_000, "{r:?}");
    assert_eq!(r["usage"]["costUsd"], 2.5, "{r:?}");
    assert_eq!(r["state"], "cancelled", "{r:?}");
    let get = client.call(7, "run/get", json!({ "runId": run_id })).await;
    assert_eq!(
        r["completedAt"], get["result"]["completedAt"],
        "completedAt must mirror the run row"
    );
}

#[tokio::test]
async fn run_result_falls_back_to_the_last_chunk_when_no_final_exists() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(SeedingRunDriver::new(
            vec![
                AdapterEventPayload::MessageChunk {
                    role: "assistant".to_string(),
                    text: Classified {
                        class: ContentClass::Visible,
                        value: "first".to_string(),
                    },
                },
                AdapterEventPayload::MessageChunk {
                    role: "assistant".to_string(),
                    text: Classified {
                        class: ContentClass::Visible,
                        value: "second".to_string(),
                    },
                },
            ],
            vec![],
        )));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let run_id = submit_run(&mut client).await;
    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "cancel must succeed: {cancel:?}"
    );
    let resp = client
        .call(6, "run/result", json!({ "runId": run_id }))
        .await;
    assert!(
        resp.get("error").is_none(),
        "run/result must succeed: {resp:?}"
    );
    let r = &resp["result"];
    assert_eq!(
        r["resultText"], "second",
        "falls back to the last chunk when no final exists: {r:?}"
    );
}

#[tokio::test]
async fn run_result_returns_null_text_and_usage_when_nothing_was_journaled() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(FakeRunDriver));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let run_id = submit_run(&mut client).await;
    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "cancel must succeed: {cancel:?}"
    );
    let resp = client
        .call(6, "run/result", json!({ "runId": run_id }))
        .await;
    assert!(
        resp.get("error").is_none(),
        "run/result must succeed: {resp:?}"
    );
    let r = &resp["result"];
    assert!(r["resultText"].is_null(), "nothing journaled: {r:?}");
    assert!(r["usage"].is_null(), "nothing journaled: {r:?}");
}

#[tokio::test]
async fn run_result_redacts_secrets_before_returning_text() {
    let harness = Harness::start(|c| {
        c.run_driver = Some(Arc::new(SeedingRunDriver::new(
            vec![AdapterEventPayload::MessageFinal {
                role: "result".to_string(),
                text: Classified {
                    class: ContentClass::Visible,
                    value: "token SECRET-123 done".to_string(),
                },
            }],
            vec!["SECRET-[0-9]+".to_string()],
        )));
    })
    .await;
    let mut client = omp_client(&harness, "omp-1").await;
    let run_id = submit_run(&mut client).await;
    let cancel = client
        .call(5, "run/cancel", json!({ "runId": run_id }))
        .await;
    assert!(
        cancel.get("error").is_none(),
        "cancel must succeed: {cancel:?}"
    );
    let resp = client
        .call(6, "run/result", json!({ "runId": run_id }))
        .await;
    assert!(
        resp.get("error").is_none(),
        "run/result must succeed: {resp:?}"
    );
    let r = &resp["result"];
    assert!(
        !r["resultText"]
            .as_str()
            .unwrap_or_default()
            .contains("SECRET-123"),
        "the raw secret must never reach the caller: {r:?}"
    );
}

/// The two usage-delta events shared by both runs in
/// `run_result_sums_claude_usage_and_takes_last_for_cumulative_adapters`.
fn shared_usage_events() -> Vec<AdapterEventPayload> {
    vec![
        AdapterEventPayload::UsageReported {
            input_tokens: 100,
            output_tokens: 200,
            cost_usd: Some(1.0),
        },
        AdapterEventPayload::UsageReported {
            input_tokens: 300,
            output_tokens: 400,
            cost_usd: Some(2.0),
        },
    ]
}

#[tokio::test]
async fn run_result_sums_claude_usage_and_takes_last_for_cumulative_adapters() {
    // Run A: adapter "claude" -- reserved names are REFUSED by legacy
    // worker/create (PROFILE_REQUIRED, -32007; orchestration.rs:466-484),
    // so go through the profile path. Claude journals per-invocation
    // deltas, so the fold sums them.
    let harness_a = Harness::start(|c| {
        c.run_driver = Some(Arc::new(SeedingRunDriver::new(
            shared_usage_events(),
            vec![],
        )));
    })
    .await;
    let mut client_a = omp_client(&harness_a, "omp-1").await;
    let task_a = client_a
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id_a = task_a["result"]["taskId"].as_str().unwrap().to_string();
    let register = client_a
        .call(
            3,
            "profile/register",
            json!({
                "adapter": "claude",
                "model": "claude-sonnet-4-5",
                "permissionEnvelope": { "fullAuto": false },
                "startupOptions": { "claude": {} },
                "environmentAllowlist": [],
                "source": "run-result-test"
            }),
        )
        .await;
    assert!(
        register.get("error").is_none(),
        "profile/register failed: {register:?}"
    );
    let profile_id = register["result"]["profileId"]
        .as_str()
        .unwrap()
        .to_string();
    let worker_a = client_a
        .call(4, "worker/create", json!({ "profileId": profile_id }))
        .await;
    assert!(
        worker_a.get("error").is_none(),
        "worker/create failed: {worker_a:?}"
    );
    let worker_id_a = worker_a["result"]["workerId"].as_str().unwrap().to_string();
    let submit_a = client_a
        .call(
            5,
            "run/submit",
            json!({ "taskId": task_id_a, "workerId": worker_id_a }),
        )
        .await;
    assert!(
        submit_a.get("error").is_none(),
        "run/submit failed: {submit_a:?}"
    );
    let run_id_a = submit_a["result"]["runId"].as_str().unwrap().to_string();
    let cancel_a = client_a
        .call(6, "run/cancel", json!({ "runId": run_id_a }))
        .await;
    assert!(
        cancel_a.get("error").is_none(),
        "cancel must succeed: {cancel_a:?}"
    );
    let result_a = client_a
        .call(7, "run/result", json!({ "runId": run_id_a }))
        .await;
    assert!(
        result_a.get("error").is_none(),
        "run/result must succeed: {result_a:?}"
    );
    let ra = &result_a["result"];
    assert_eq!(
        ra["usage"]["inputTokens"], 400,
        "claude sums deltas: {ra:?}"
    );
    assert_eq!(
        ra["usage"]["outputTokens"], 600,
        "claude sums deltas: {ra:?}"
    );
    assert_eq!(ra["usage"]["costUsd"], 3.0, "claude sums deltas: {ra:?}");

    // Run B: legacy worker/create with adapter "fake". Every non-claude
    // reporting adapter journals cumulative totals, so the fold takes the
    // last report, not a sum.
    let harness_b = Harness::start(|c| {
        c.run_driver = Some(Arc::new(SeedingRunDriver::new(
            shared_usage_events(),
            vec![],
        )));
    })
    .await;
    let mut client_b = omp_client(&harness_b, "omp-1").await;
    let run_id_b = submit_run(&mut client_b).await;
    let cancel_b = client_b
        .call(5, "run/cancel", json!({ "runId": run_id_b }))
        .await;
    assert!(
        cancel_b.get("error").is_none(),
        "cancel must succeed: {cancel_b:?}"
    );
    let result_b = client_b
        .call(6, "run/result", json!({ "runId": run_id_b }))
        .await;
    assert!(
        result_b.get("error").is_none(),
        "run/result must succeed: {result_b:?}"
    );
    let rb = &result_b["result"];
    assert_eq!(
        rb["usage"]["inputTokens"], 300,
        "fake takes the last cumulative report: {rb:?}"
    );
    assert_eq!(
        rb["usage"]["outputTokens"], 400,
        "fake takes the last cumulative report: {rb:?}"
    );
    assert_eq!(
        rb["usage"]["costUsd"], 2.0,
        "fake takes the last cumulative report: {rb:?}"
    );
}
