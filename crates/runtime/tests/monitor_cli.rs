//! Integration test for `crewd monitor`: binds a real, in-process
//! [`Server`] on an on-disk socket (exactly like `orchestration_rpc.rs`'s
//! own `Harness`), commits a task/worker/run through it over a raw
//! socket client, then spawns the actual compiled `crewd monitor`
//! subcommand as a subprocess pointed at the same repository/state
//! directory -- proving the real CLI binary, not just the library
//! function, connects, replays, and renders a committed run. A second
//! test proves a `display` principal (exactly what `crewd monitor`
//! authenticates as) cannot call an orchestration mutation method.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use batman_runtime::db::DatabaseHandle;
use batman_runtime::ipc::{PeerCredentialReader, PeerCredentials, Server, ServerConfig};
use batman_runtime::paths::RuntimePaths;
use nix::unistd::Uid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::process::Command;
use tokio::sync::oneshot;

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

/// Binds a real daemon on an on-disk socket and serves it on a spawned
/// task until the returned sender is dropped or sent to. Returns the
/// state/repo directories `crewd monitor`'s own independent
/// `RuntimePaths::resolve` call will use to find the same socket.
async fn start_daemon() -> (
    PathBuf,
    PathBuf,
    tempfile::TempDir,
    tempfile::TempDir,
    oneshot::Sender<()>,
) {
    let state = tempfile::Builder::new()
        .prefix("bat-mon-state-")
        .tempdir_in("/tmp")
        .unwrap();
    let repo = tempfile::Builder::new()
        .prefix("bat-mon-repo-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();

    let paths = RuntimePaths::resolve(state.path(), repo.path()).unwrap();
    let db = Arc::new(DatabaseHandle::start(paths.database.clone()).await.unwrap());
    let config = ServerConfig {
        credential_reader: Arc::new(FakeReader {
            uid: Some(Uid::current().as_raw()),
        }),
        ..ServerConfig::default()
    };
    let server = Server::bind(paths.socket.clone(), db, paths.project_id, config)
        .await
        .unwrap();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    tokio::spawn(async move {
        let _ = server
            .serve(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    (
        state.path().to_path_buf(),
        repo.path().to_path_buf(),
        state,
        repo,
        shutdown_tx,
    )
}

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

    async fn initialize(&mut self, role: &str, instance_id: &str, agent_dir: &str) -> Value {
        let auth = match role {
            "ompExtension" => json!({
                "role": "ompExtension",
                "instanceId": instance_id,
                "agentDirectory": agent_dir
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
                "repository": { "canonicalPath": agent_dir, "vcsRoot": agent_dir },
                "auth": auth,
                "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
                "lastSequence": null
            }
        }))
        .await;
        self.recv().await
    }
}

#[tokio::test]
async fn monitor_renders_a_committed_run_and_exits_cleanly_on_sigint() {
    let (state_dir, repo_dir, _state_guard, _repo_guard, _shutdown) = start_daemon().await;
    let paths = RuntimePaths::resolve(&state_dir, &repo_dir).unwrap();
    let repo_str = repo_dir.to_str().unwrap();

    let mut client = Client::connect(&paths.socket).await;
    let init = client
        .initialize("ompExtension", "monitor-test", repo_str)
        .await;
    assert!(init.get("error").is_none(), "initialize failed: {init:?}");

    let task = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "monitor-test", "revision": 1 }),
        )
        .await;
    let task_id = task["result"]["taskId"].as_str().unwrap().to_string();
    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:mon", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = worker["result"]["workerId"].as_str().unwrap().to_string();
    // `run/submit`'s own RPC response may report an error (no resolved
    // profile, no adapter registry configured here) -- the run is still
    // committed as `queued` regardless (ADR-0013; see
    // `orchestration_rpc.rs`'s own
    // `run_submit_without_driver_reports_adapter_unavailable_but_preserves_queued_run`),
    // which is exactly the real, replayable `RunEvent` this test needs
    // `crewd monitor` to render.
    let _submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id }),
        )
        .await;

    let list = client
        .call(5, "run/list", json!({ "taskId": task_id }))
        .await;
    let run_id = list["result"]["runs"][0]["runId"]
        .as_str()
        .unwrap()
        .to_string();

    // Spawn the real compiled `crewd monitor` subcommand, pointed at
    // the same on-disk socket via its own independent
    // `RuntimePaths::resolve(state_dir, repo)` computation -- proving the
    // actual CLI binary, not just the library function underneath it.
    let mut child = Command::new(env!("CARGO_BIN_EXE_crewd"))
        .args([
            "monitor",
            "--state-dir",
            state_dir.to_str().unwrap(),
            "--repo",
            repo_str,
        ])
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawning crewd monitor must succeed");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();

    let rendered = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("crewd monitor must render a line within 10 seconds")
        .expect("reading monitor stdout must not error")
        .expect("monitor stdout must not close before rendering anything");

    assert!(
        rendered.contains(&run_id[..8]) && rendered.contains("queued"),
        "expected the monitor's first line to render the committed run's short id and \
         `queued` state; got: {rendered:?}"
    );

    // Prove clean SIGINT handling: send the real signal (not `kill()`,
    // which is SIGKILL) and confirm the process exits with status 0
    // rather than being terminated.
    let pid = nix::unistd::Pid::from_raw(child.id().expect("child has a pid") as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT)
        .expect("sending SIGINT must succeed");
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("crewd monitor must exit within 5 seconds of SIGINT")
        .expect("waiting on the child must not error");
    assert!(
        status.success(),
        "crewd monitor must exit cleanly (status 0) on SIGINT, got: {status:?}"
    );
}

#[tokio::test]
async fn a_display_role_monitor_cannot_call_orchestration_mutation_methods() {
    let (state_dir, repo_dir, _state_guard, _repo_guard, _shutdown) = start_daemon().await;
    let paths = RuntimePaths::resolve(&state_dir, &repo_dir).unwrap();

    let mut client = Client::connect(&paths.socket).await;
    let init = client
        .initialize("display", "crewd-monitor", repo_dir.to_str().unwrap())
        .await;
    assert!(init.get("error").is_none(), "initialize failed: {init:?}");

    let attempt = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "x", "revision": 1 }),
        )
        .await;
    assert_eq!(
        attempt["error"]["code"], -32601,
        "a display principal (exactly what crewd monitor authenticates as) must get \
         METHOD_NOT_FOUND for task/upsert, proving the monitor cannot mutate: {attempt:?}"
    );
}
