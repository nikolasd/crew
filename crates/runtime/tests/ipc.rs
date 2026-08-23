//! Integration tests for the initialized JSON-RPC runtime socket protocol.
//!
//! These drive the real [`batman_runtime::ipc::Server`] over a Unix domain
//! socket, exercising every foundation requirement: peer-credential
//! enforcement before parsing, bounded framing and negotiation, version
//! negotiation, role-scoped method tables, the injectable worker-credential
//! verifier, agent-directory validation, and event replay.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use batman_protocol::Timestamp;
use batman_protocol::{ProjectId, RunId, TaskId, WorkerId, error_code};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::ipc::{
    PeerCredentialReader, PeerCredentials, ScopedRun, Server, ServerConfig, VerifyError,
    WorkerCredentialVerifier,
};
use batman_runtime::paths::RuntimePaths;
use batman_runtime::security::redaction::{RawEventKind, RawRuntimeEvent, Redactor};
use nix::unistd::Uid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// ------------------------------------------------------------------ fakes

/// A [`PeerCredentialReader`] that returns fixed credentials, so tests can
/// simulate a matching UID, a mismatched UID, or a platform that cannot
/// report peer credentials at all.
struct FakeReader {
    uid: Option<u32>,
    pid: Option<i32>,
}

impl PeerCredentialReader for FakeReader {
    fn read(&self, _stream: &UnixStream) -> PeerCredentials {
        PeerCredentials {
            uid: self.uid,
            pid: self.pid,
        }
    }
}

/// A [`WorkerCredentialVerifier`] that accepts one token and consults the
/// peer PID against an allowed ancestry set.
struct FakeVerifier {
    token: String,
    allowed_pids: Vec<i32>,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
}

impl WorkerCredentialVerifier for FakeVerifier {
    fn verify(&self, scope_token: &str, peer_pid: Option<i32>) -> Result<ScopedRun, VerifyError> {
        if scope_token != self.token {
            return Err(VerifyError::InvalidToken);
        }
        match peer_pid {
            Some(pid) if self.allowed_pids.contains(&pid) => Ok(ScopedRun {
                run_id: self.run_id,
                task_id: self.task_id,
                worker_id: self.worker_id,
            }),
            _ => Err(VerifyError::OutsideAncestry),
        }
    }
}

/// A [`batman_runtime::service::RunDriver`] stub reporting a fixed live-run
/// count, for R87/R82 tests: `runtime/status` must report the driver's
/// count and `runtime/shutdown` must refuse while it is nonzero.
struct FixedCountDriver {
    count: usize,
}

impl batman_runtime::service::RunDriver for FixedCountDriver {
    fn start(
        &self,
        _ctx: batman_runtime::service::RunDriverContext,
    ) -> batman_runtime::service::AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn send_follow_up(
        &self,
        _run_id: RunId,
        _task_id: TaskId,
        _worker_id: WorkerId,
        _prompt: String,
    ) -> batman_runtime::service::AdapterFuture<'static, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn batman_runtime::adapter::Adapter>> {
        None
    }

    fn cancel_run(
        &self,
        _run_id: RunId,
        _scope: batman_runtime::adapter::CancelScope,
    ) -> batman_runtime::service::AdapterFuture<
        'static,
        Result<batman_runtime::service::CancelOutcome, String>,
    > {
        Box::pin(async { Ok(batman_runtime::service::CancelOutcome::NoRunningAdapter) })
    }

    fn active_run_count(&self) -> usize {
        self.count
    }
}

// --------------------------------------------------------------- harness

struct Harness {
    socket: PathBuf,
    project_id: ProjectId,
    owned_dir: PathBuf,
    _state: tempfile::TempDir,
    _repo: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Harness {
    async fn start(config_fn: impl FnOnce(&mut ServerConfig)) -> Self {
        // Unix domain socket paths are bounded (macOS `SUN_LEN` ~104 bytes),
        // so root the state under a short base directory rather than the
        // deeply nested default temp dir.
        let state = tempfile::Builder::new()
            .prefix("bat-s-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = tempfile::Builder::new()
            .prefix("bat-r-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();

        let paths = RuntimePaths::resolve(state.path(), repo.path()).unwrap();
        let db = Arc::new(DatabaseHandle::start(paths.database.clone()).await.unwrap());

        // Seed a durable RuntimeStarted event so replay has something to
        // return, exactly as the real foreground server does at startup.
        let redactor = Redactor::new();
        let started = redactor.sanitize(RawRuntimeEvent {
            timestamp: Timestamp::now(),
            project_id: paths.project_id,
            run_id: None,
            kind: RawEventKind::RuntimeStarted,
        });
        db.append_event(started).await.unwrap();

        let mut config = ServerConfig::default();
        config_fn(&mut config);

        let server = Server::bind(paths.socket.clone(), db, paths.project_id, config)
            .await
            .unwrap();
        let socket = server.socket_path().to_path_buf();

        let (shutdown, shutdown_rx) = oneshot::channel();
        let join = tokio::spawn(async move {
            let _ = server
                .serve(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        // The canonical repo path is owned by the current user; used as a
        // valid ompExtension agent directory.
        let owned_dir = std::fs::canonicalize(repo.path()).unwrap();

        Self {
            socket,
            project_id: paths.project_id,
            owned_dir,
            _state: state,
            _repo: repo,
            shutdown: Some(shutdown),
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

    async fn send_line(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send(&mut self, value: &Value) {
        self.send_line(&serde_json::to_string(value).unwrap()).await;
    }

    /// Writes a frame, ignoring write errors. Used when the peer is expected
    /// to close the connection in response (an oversized frame), which can
    /// surface as a broken pipe mid-write.
    async fn send_best_effort(&mut self, line: &str) {
        let _ = self.writer.write_all(line.as_bytes()).await;
        let _ = self.writer.write_all(b"\n").await;
        let _ = self.writer.flush().await;
    }

    /// Reads one NDJSON frame, or `None` at end of stream (connection closed).
    async fn recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await.unwrap();
        if n == 0 {
            return None;
        }
        Some(serde_json::from_str(line.trim_end()).unwrap())
    }
}

fn current_uid() -> u32 {
    Uid::current().as_raw()
}

fn matching_reader() -> Arc<dyn PeerCredentialReader> {
    Arc::new(FakeReader {
        uid: Some(current_uid()),
        pid: Some(4242),
    })
}

/// An `initialize` request as an ompExtension with the given agent directory
/// and client frame offer.
fn omp_init(agent_dir: &str, client_max: u32, min: (u16, u16), max: (u16, u16)) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "client": { "name": "@nikolasd/crew", "version": "0.1.0" },
            "supported": {
                "min": { "major": min.0, "minor": min.1 },
                "max": { "major": max.0, "minor": max.1 }
            },
            "repository": { "canonicalPath": agent_dir, "vcsRoot": agent_dir },
            "auth": {
                "role": "ompExtension",
                "instanceId": "omp-1",
                "agentDirectory": agent_dir
            },
            "capabilities": { "eventReplay": true, "maxFrameBytes": client_max },
            "lastSequence": null
        }
    })
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

// ------------------------------------------------------ peer credentials

#[tokio::test]
async fn peer_uid_mismatch_closes_before_json_parsing() {
    let harness = Harness::start(|c| {
        c.credential_reader = Arc::new(FakeReader {
            uid: Some(current_uid() + 1),
            pid: Some(1),
        });
    })
    .await;

    let mut client = Client::connect(&harness.socket).await;
    // Deliberately malformed JSON: if the server parsed anything it would
    // answer with a PARSE_ERROR. Instead it must have dropped us on the UID
    // mismatch before reading a byte.
    client.send_line("this is not valid json").await;
    assert!(
        client.recv().await.is_none(),
        "mismatched-UID connection must be closed with no response frame"
    );
}

#[tokio::test]
async fn peer_uid_match_is_accepted() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    let response = client.recv().await.unwrap();
    assert_eq!(
        response["result"]["capabilities"]["peerCredentialsVerified"],
        true
    );
}

#[tokio::test]
async fn unavailable_peer_credentials_fail_closed_without_owner_only() {
    let harness = Harness::start(|c| {
        c.credential_reader = Arc::new(FakeReader {
            uid: None,
            pid: None,
        });
        c.owner_only_override = Some(false);
    })
    .await;
    let mut client = Client::connect(&harness.socket).await;
    client.send_line("not json").await;
    assert!(
        client.recv().await.is_none(),
        "must fail closed when peer creds are unavailable and owner-only check fails"
    );
}

#[tokio::test]
async fn unavailable_peer_credentials_allowed_when_owner_only_verified() {
    let harness = Harness::start(|c| {
        c.credential_reader = Arc::new(FakeReader {
            uid: None,
            pid: None,
        });
        c.owner_only_override = Some(true);
    })
    .await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    let response = client.recv().await.unwrap();
    assert_eq!(
        response["result"]["capabilities"]["peerCredentialsVerified"], false,
        "the limitation must be reported in runtime capabilities"
    );
}

// -------------------------------------------------------------- req 1

#[tokio::test]
async fn oversized_bootstrap_frame_closes_connection() {
    let harness = Harness::start(|c| {
        c.credential_reader = matching_reader();
        c.runtime_max_frame_bytes = 128 * 1024;
    })
    .await;
    let mut client = Client::connect(&harness.socket).await;
    let huge = "x".repeat(200 * 1024);
    client.send_best_effort(&huge).await;
    assert!(
        client.recv().await.is_none(),
        "a bootstrap frame above the configured maximum must close the connection"
    );
}

// -------------------------------------------------------------- req 2

#[tokio::test]
async fn status_before_initialize_returns_not_initialized() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&request(1, "runtime/status", json!(null)))
        .await;
    let response = client.recv().await.unwrap();
    assert_eq!(response["error"]["code"], error_code::NOT_INITIALIZED);
}

// -------------------------------------------------------------- req 3

#[tokio::test]
async fn non_overlapping_version_range_is_incompatible() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (2, 0),
            (2, 0),
        ))
        .await;
    let response = client.recv().await.unwrap();
    assert_eq!(response["error"]["code"], error_code::INCOMPATIBLE_VERSION);
}

// -------------------------------------------------------------- req 4

#[tokio::test]
async fn successful_initialize_negotiates_protocol_project_and_frame() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    let response = client.recv().await.unwrap();
    let result = &response["result"];
    assert_eq!(result["negotiated"], json!({ "major": 1, "minor": 0 }));
    assert_eq!(result["projectId"], json!(harness.project_id.to_string()));
    // Runtime default max is 4 MiB; the client offered 1 MiB -> min is 1 MiB.
    assert_eq!(result["capabilities"]["maxFrameBytes"], 1024 * 1024);
}

// -------------------------------------------------------------- req 5

#[tokio::test]
async fn frame_offer_below_protocol_minimum_is_invalid() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024, // below the 64 KiB protocol minimum
            (1, 0),
            (1, 0),
        ))
        .await;
    let response = client.recv().await.unwrap();
    assert_eq!(response["error"]["code"], error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn frame_above_negotiated_maximum_closes_connection() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    // Negotiate exactly the 64 KiB minimum.
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            64 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    let init = client.recv().await.unwrap();
    assert_eq!(init["result"]["capabilities"]["maxFrameBytes"], 64 * 1024);

    // A post-initialize frame above the negotiated maximum must be rejected.
    let huge = "x".repeat(80 * 1024);
    let frame = serde_json::to_string(&request(2, "runtime/status", json!(huge))).unwrap();
    client.send_best_effort(&frame).await;
    assert!(
        client.recv().await.is_none(),
        "a frame above the negotiated maximum must close the connection"
    );
}

// -------------------------------------------------------------- req 6

#[tokio::test]
async fn events_replay_returns_committed_events_after_sequence() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    let _ = client.recv().await.unwrap();

    client
        .send(&request(2, "events/replay", json!({ "afterSequence": 0 })))
        .await;
    let response = client.recv().await.unwrap();
    let events = response["result"]
        .as_array()
        .expect("replay result is an array");
    assert!(
        !events.is_empty(),
        "startup RuntimeStarted event must be replayed"
    );
    assert_eq!(events[0]["sequence"], 1);
    assert_eq!(events[0]["event"]["type"], "runtimeStarted");
}

// -------------------------------------------------------------- req 7

#[tokio::test]
async fn omp_extension_receives_all_mutation_methods() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    let response = client.recv().await.unwrap();
    let methods = response["result"]["allowedMethods"].as_array().unwrap();
    let names: Vec<&str> = methods.iter().map(|m| m.as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "runtime/status",
            "events/subscribe",
            "events/replay",
            "runtime/shutdown",
            "task/upsert",
            "task/get",
            "worker/create",
            "worker/list",
            "worker/get",
            "run/submit",
            "run/list",
            "run/get",
            "run/result",
            "run/retry",
            "run/cancel",
            "message/send",
            "message/list",
            "approval/list",
            "approval/decide",
            "coordination/child/list",
            "coordination/child/decide",
            "reconcile/omp",
            "profile/register",
            "policy/violation/decide",
            "policy/violation/list",
            "workspace/acquire",
            "workspace/get",
            "workspace/release",
            "workspace/inspect",
            "workspace/apply",
            "artifact/list",
            "artifact/fetch"
        ]
    );
}

#[tokio::test]
async fn display_receives_only_status_and_event_methods() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "client": { "name": "display", "version": "0.1.0" },
                "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
                "repository": { "canonicalPath": "/tmp", "vcsRoot": "/tmp" },
                "auth": { "role": "display", "instanceId": "display-1" },
                "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
                "lastSequence": null
            }
        }))
        .await;
    let response = client.recv().await.unwrap();
    let methods = response["result"]["allowedMethods"].as_array().unwrap();
    let names: Vec<&str> = methods.iter().map(|m| m.as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "runtime/status",
            "events/subscribe",
            "events/replay",
            "task/get",
            "worker/list",
            "worker/get",
            "run/list",
            "run/get",
            "run/result",
            "message/list",
            "approval/list",
            "coordination/child/list",
            "policy/violation/list"
        ]
    );

    // A method outside the display role's table is hidden: METHOD_NOT_FOUND.
    client
        .send(&request(2, "runtime/shutdown", json!(null)))
        .await;
    let hidden = client.recv().await.unwrap();
    assert_eq!(hidden["error"]["code"], error_code::METHOD_NOT_FOUND);
}

// -------------------------------------------------------------- R87/R82

/// R87: `runtime/status` must report the run driver's live count, never a
/// hardcoded placeholder.
#[tokio::test]
async fn runtime_status_reports_the_drivers_active_run_count() {
    let harness = Harness::start(|c| {
        c.credential_reader = matching_reader();
        c.run_driver = Some(Arc::new(FixedCountDriver { count: 3 }));
    })
    .await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    client.recv().await.unwrap();

    client
        .send(&request(2, "runtime/status", json!(null)))
        .await;
    let status = client.recv().await.unwrap();
    assert_eq!(
        status["result"]["activeRuns"], 3,
        "activeRuns must come from the driver, not a placeholder: {status:?}"
    );
}

/// R82: `runtime/shutdown` must refuse while runs are live (the daemon
/// serves every connected instance), and `force: true` overrides.
#[tokio::test]
async fn runtime_shutdown_is_refused_while_runs_are_live_unless_forced() {
    let harness = Harness::start(|c| {
        c.credential_reader = matching_reader();
        c.run_driver = Some(Arc::new(FixedCountDriver { count: 1 }));
    })
    .await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    client.recv().await.unwrap();

    client
        .send(&request(2, "runtime/shutdown", json!(null)))
        .await;
    let refused = client.recv().await.unwrap();
    assert_eq!(
        refused["error"]["code"],
        error_code::INVALID_PARAMS,
        "shutdown with a live run must be refused: {refused:?}"
    );

    // The daemon is still serving after the refusal.
    client
        .send(&request(3, "runtime/status", json!(null)))
        .await;
    let status = client.recv().await.unwrap();
    assert_eq!(status["result"]["running"], true);

    // The deliberate operator escape hatch stops it.
    client
        .send(&request(4, "runtime/shutdown", json!({ "force": true })))
        .await;
    let stopped = client.recv().await.unwrap();
    assert_eq!(stopped["result"]["stopping"], true, "{stopped:?}");
}

/// R82's second gate: a second live connection also refuses an unforced
/// shutdown.
#[tokio::test]
async fn runtime_shutdown_is_refused_while_another_connection_is_live() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut first = Client::connect(&harness.socket).await;
    first
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    first.recv().await.unwrap();
    let mut second = Client::connect(&harness.socket).await;
    second
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    second.recv().await.unwrap();

    first
        .send(&request(2, "runtime/shutdown", json!(null)))
        .await;
    let refused = first.recv().await.unwrap();
    assert_eq!(
        refused["error"]["code"],
        error_code::INVALID_PARAMS,
        "shutdown with another live connection must be refused: {refused:?}"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("0 active run(s) and 1 other live connection(s)"),
        "the refusal must name the real counts: {refused:?}"
    );
}

/// R82's accept leg: a single connection with zero live runs must be
/// allowed to stop the daemon WITHOUT force -- an inverted gate ("always
/// refuse unless forced") must fail here.
#[tokio::test]
async fn runtime_shutdown_is_accepted_unforced_when_nothing_else_is_live() {
    let harness = Harness::start(|c| {
        c.credential_reader = matching_reader();
        c.run_driver = Some(Arc::new(FixedCountDriver { count: 0 }));
    })
    .await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    client.recv().await.unwrap();

    client
        .send(&request(2, "runtime/shutdown", json!(null)))
        .await;
    let stopped = client.recv().await.unwrap();
    assert_eq!(
        stopped["result"]["stopping"], true,
        "the sole idle connection must be allowed to stop the daemon unforced: {stopped:?}"
    );
}

// -------------------------------------------------------------- req 8

fn worker_init(scope_token: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "client": { "name": "worker", "version": "0.1.0" },
            "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
            "repository": { "canonicalPath": "/tmp", "vcsRoot": "/tmp" },
            "auth": { "role": "workerMcp", "instanceId": "worker-1", "scopeToken": scope_token },
            "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
            "lastSequence": null
        }
    })
}

#[tokio::test]
async fn worker_mcp_rejected_by_default_verifier() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client.send(&worker_init("anything")).await;
    let response = client.recv().await.unwrap();
    assert_eq!(response["error"]["code"], error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn worker_mcp_accepted_with_valid_credential_and_ancestry() {
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let harness = Harness::start(move |c| {
        c.credential_reader = Arc::new(FakeReader {
            uid: Some(current_uid()),
            pid: Some(9001),
        });
        c.worker_verifier = Arc::new(FakeVerifier {
            token: "good-token".to_string(),
            allowed_pids: vec![9001],
            run_id,
            task_id,
            worker_id,
        });
    })
    .await;

    // First connection: the supervised vendor process initializes.
    let mut client = Client::connect(&harness.socket).await;
    client.send(&worker_init("good-token")).await;
    let response = client.recv().await.unwrap();
    assert_eq!(response["result"]["principal"]["role"], "workerMcp");
    assert_eq!(
        response["result"]["principal"]["scopedRunId"],
        run_id.to_string(),
        "the runtime echoes back the token-bound scope, never a client-supplied one"
    );
    assert_eq!(
        response["result"]["principal"]["scopedTaskId"],
        task_id.to_string()
    );
    assert_eq!(
        response["result"]["principal"]["scopedWorkerId"],
        worker_id.to_string()
    );
    let methods = response["result"]["allowedMethods"].as_array().unwrap();
    let names: Vec<&str> = methods.iter().map(|m| m.as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "runtime/status",
            "coordination/task",
            "coordination/peers",
            "coordination/send",
            "coordination/requestChild",
            "coordination/publishArtifact",
            "coordination/reportBlocked",
            "coordination/askPolicy",
            "coordination/child/list",
            "coordination/peerWorkspace",
            "coordination/artifactList",
            "coordination/artifactFetch"
        ]
    );

    // A restarted MCP descendant reinitializes while the run is live.
    let mut restarted = Client::connect(&harness.socket).await;
    restarted.send(&worker_init("good-token")).await;
    let reinit = restarted.recv().await.unwrap();
    assert_eq!(reinit["result"]["principal"]["role"], "workerMcp");
}

#[tokio::test]
async fn worker_mcp_outside_ancestry_is_rejected() {
    let run_id = RunId::new();
    let harness = Harness::start(move |c| {
        c.credential_reader = Arc::new(FakeReader {
            uid: Some(current_uid()),
            pid: Some(1), // not in the allowed ancestry set
        });
        c.worker_verifier = Arc::new(FakeVerifier {
            token: "good-token".to_string(),
            allowed_pids: vec![9001],
            run_id,
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
        });
    })
    .await;
    let mut client = Client::connect(&harness.socket).await;
    client.send(&worker_init("good-token")).await;
    let response = client.recv().await.unwrap();
    assert_eq!(response["error"]["code"], error_code::INVALID_PARAMS);
}

// -------------------------------------------------------------- req 9

#[tokio::test]
async fn omp_agent_directory_must_be_absolute_existing_and_owned() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;

    // Valid: absolute, exists, owned by the current user.
    let mut ok = Client::connect(&harness.socket).await;
    ok.send(&omp_init(
        harness.owned_dir.to_str().unwrap(),
        1024 * 1024,
        (1, 0),
        (1, 0),
    ))
    .await;
    assert!(ok.recv().await.unwrap()["result"].is_object());

    // Relative path.
    let mut relative = Client::connect(&harness.socket).await;
    relative
        .send(&omp_init("relative/agent", 1024 * 1024, (1, 0), (1, 0)))
        .await;
    assert_eq!(
        relative.recv().await.unwrap()["error"]["code"],
        error_code::INVALID_PARAMS
    );

    // Missing path.
    let missing = harness.owned_dir.join("does-not-exist");
    let mut missing_client = Client::connect(&harness.socket).await;
    missing_client
        .send(&omp_init(
            missing.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    assert_eq!(
        missing_client.recv().await.unwrap()["error"]["code"],
        error_code::INVALID_PARAMS
    );

    // Exists and is absolute, but owned by root (uid 0), not the current user.
    let mut not_owned = Client::connect(&harness.socket).await;
    not_owned
        .send(&omp_init("/", 1024 * 1024, (1, 0), (1, 0)))
        .await;
    assert_eq!(
        not_owned.recv().await.unwrap()["error"]["code"],
        error_code::INVALID_PARAMS
    );
}

#[tokio::test]
async fn strict_role_variant_rejects_cross_role_fields() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    // ompExtension carrying a workerMcp-only `scopeToken` field.
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "client": { "name": "x", "version": "1" },
                "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
                "repository": { "canonicalPath": "/tmp", "vcsRoot": "/tmp" },
                "auth": {
                    "role": "ompExtension",
                    "instanceId": "omp-1",
                    "agentDirectory": "/tmp",
                    "scopeToken": "cross-role"
                },
                "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
                "lastSequence": null
            }
        }))
        .await;
    let response = client.recv().await.unwrap();
    assert_eq!(response["error"]["code"], error_code::INVALID_PARAMS);
}

// ---------------------------------------------------- runtime/status shape

#[tokio::test]
async fn runtime_status_reports_healthy_running_runtime() {
    let harness = Harness::start(|c| c.credential_reader = matching_reader()).await;
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&omp_init(
            harness.owned_dir.to_str().unwrap(),
            1024 * 1024,
            (1, 0),
            (1, 0),
        ))
        .await;
    let _ = client.recv().await.unwrap();

    client
        .send(&request(2, "runtime/status", json!(null)))
        .await;
    let response = client.recv().await.unwrap();
    let status = &response["result"];
    assert_eq!(status["running"], true);
    assert_eq!(status["protocol"], json!({ "major": 1, "minor": 0 }));
    assert_eq!(status["activeRuns"], 0);
    assert_eq!(status["protocolHealthy"], true);
    assert_eq!(status["projectId"], json!(harness.project_id.to_string()));
}
