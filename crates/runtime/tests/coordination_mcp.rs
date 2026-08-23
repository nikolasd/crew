//! End-to-end integration tests for `crewd coordination-mcp`: spawns
//! the real compiled binary as a genuine child process (no faked peer
//! credentials -- the kernel reports its actual pid via `SO_PEERCRED`/
//! `LOCAL_PEERCRED`), drives it over real stdio as an MCP client would,
//! and verifies its tool calls land in a real [`CoordinationBroker`]
//! behind a real [`Server`].
//!
//! Exercises the exact reserve/spawn/bind sequencing a real adapter
//! integration needs: a scope token is reserved *before* the subprocess
//! spawns (so it can be placed in the child's own environment before
//! `execve`), then bound to the child's real pid only after `spawn()`
//! actually returns one.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use batman_protocol::{ProjectId, RunId, TaskId, Timestamp, WorkerId};
use batman_runtime::coordination::{
    ScopeBinding, ScopeTokenStore, ScopeTokenVerifier, VendorProcessIdentity,
};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::ipc::{Server, ServerConfig};
use batman_runtime::paths::RuntimePaths;
use batman_runtime::service::FakeRunDriver;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// --------------------------------------------------------------- harness

struct Harness {
    state_dir: PathBuf,
    repo_dir: PathBuf,
    scope_token_store: Arc<ScopeTokenStore>,
    database: PathBuf,
    project_id: ProjectId,
    _state: tempfile::TempDir,
    _repo: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Harness {
    async fn start() -> Self {
        let state = tempfile::Builder::new()
            .prefix("bat-mcp-s-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = tempfile::Builder::new()
            .prefix("bat-mcp-r-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();

        let paths = RuntimePaths::resolve(state.path(), repo.path()).unwrap();
        let db = Arc::new(DatabaseHandle::start(paths.database.clone()).await.unwrap());
        let scope_token_store = Arc::new(ScopeTokenStore::new());

        // Deliberately no `credential_reader` override: the real
        // `SystemPeerCredentialReader` (Default's own choice) reports the
        // subprocess's *actual* kernel-verified pid, exercising real
        // process-tree ancestry rather than a faked one.
        let config = ServerConfig {
            worker_verifier: Arc::new(ScopeTokenVerifier::new(scope_token_store.clone())),
            run_driver: Some(Arc::new(FakeRunDriver)),
            ..Default::default()
        };

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
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Self {
            state_dir: state.path().to_path_buf(),
            repo_dir: repo.path().to_path_buf(),
            scope_token_store,
            database: paths.database.clone(),
            project_id: paths.project_id,
            _state: state,
            _repo: repo,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        }
    }

    /// Seeds a task/worker/run through a real `CoordinationBroker`, so
    /// this test never depends on OMP's own orchestration RPC methods.
    async fn seed_run(&self) -> (RunId, TaskId, WorkerId) {
        let db = Arc::new(DatabaseHandle::start(self.database.clone()).await.unwrap());
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        let project_id = self.project_id;
        db.run_domain_op(Box::new(move |conn| {
            conn.execute(
                "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                rusqlite::params![task_id.to_string(), project_id.to_string(), "test-owner", 1, "2026-01-01T00:00:00Z"],
            )?;
            conn.execute(
                "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![worker_id.to_string(), "sha256:x", "fake", "m", "{}"],
            )?;
            conn.execute(
                "INSERT INTO workers (worker_id, project_id, profile_id, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![worker_id.to_string(), project_id.to_string(), worker_id.to_string(), "2026-01-01T00:00:00Z"],
            )?;
            conn.execute(
                "INSERT INTO runs (run_id, task_id, worker_id, state, created_at) VALUES (?1, ?2, ?3, 'working', ?4)",
                rusqlite::params![run_id.to_string(), task_id.to_string(), worker_id.to_string(), "2026-01-01T00:00:00Z"],
            )?;
            Ok::<_, batman_runtime::domain::DomainError>(json!({}))
        }))
        .await
        .unwrap();
        (run_id, task_id, worker_id)
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

// ------------------------------------------------------------- subprocess

/// Spawns the real `crewd coordination-mcp` binary, following the
/// documented reserve/spawn/bind sequence: reserve a token, put it in
/// the child's environment, spawn, then bind to the real returned pid.
struct McpSubprocess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpSubprocess {
    /// Spawns with the vendor identity bound to *its own* real pid --
    /// the common case for a subprocess that connects directly (no
    /// separate vendor process of its own in this test).
    async fn spawn(harness: &Harness, run_id: RunId, task_id: TaskId, worker_id: WorkerId) -> Self {
        let token = harness.scope_token_store.reserve_token();
        let (child, stdin, stdout) = Self::launch(harness, run_id, &token);
        let pid = child.id().expect("spawned child has a pid") as i32;
        harness
            .scope_token_store
            .bind(
                token,
                scope_binding(harness, task_id, worker_id, run_id, pid),
            )
            .expect("binding a freshly reserved token never collides");
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Spawns with the vendor identity bound to an explicit, *different*
    /// pid -- for tests proving ancestry actually matters (an unrelated
    /// or already-exited "vendor" must never authenticate this process).
    fn spawn_with_vendor_pid(
        harness: &Harness,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        vendor_pid: i32,
    ) -> (Self, String) {
        let token = harness.scope_token_store.reserve_token();
        harness
            .scope_token_store
            .bind(
                token.clone(),
                scope_binding(harness, task_id, worker_id, run_id, vendor_pid),
            )
            .expect("binding a freshly reserved token never collides");
        let (child, stdin, stdout) = Self::launch(harness, run_id, &token);
        (
            Self {
                child,
                stdin,
                stdout,
            },
            token,
        )
    }

    fn launch(
        harness: &Harness,
        run_id: RunId,
        token: &str,
    ) -> (Child, ChildStdin, BufReader<ChildStdout>) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crewd"))
            .arg("coordination-mcp")
            .arg("--state-dir")
            .arg(&harness.state_dir)
            .arg("--repo")
            .arg(&harness.repo_dir)
            .arg("--run-id")
            .arg(run_id.to_string())
            .env_clear()
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("CREW_WORKER_SCOPE_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn coordination-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        (child, stdin, stdout)
    }

    /// Spawns with `CREW_WORKER_SCOPE_TOKEN` absent entirely.
    fn spawn_without_scope_token(harness: &Harness, run_id: RunId) -> Child {
        Command::new(env!("CARGO_BIN_EXE_crewd"))
            .arg("coordination-mcp")
            .arg("--state-dir")
            .arg(&harness.state_dir)
            .arg("--repo")
            .arg(&harness.repo_dir)
            .arg("--run-id")
            .arg(run_id.to_string())
            .env_clear()
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn coordination-mcp")
    }

    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.try_call(id, method, params)
            .await
            .expect("coordination-mcp closed the connection before responding")
    }

    /// Like [`Self::call`], but returns `None` on a clean EOF (the
    /// process closed stdout before writing a response) instead of
    /// panicking -- for tests asserting the process never gets far
    /// enough to serve stdio at all.
    async fn try_call(&mut self, id: i64, method: &str, params: Value) -> Option<Value> {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let mut line = serde_json::to_string(&request).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();

        let mut response_line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(10),
            self.stdout.read_line(&mut response_line),
        )
        .await
        .expect("coordination-mcp response within timeout")
        .expect("read response line");
        if read == 0 {
            return None;
        }
        Some(serde_json::from_str(&response_line).expect("response line is valid JSON"))
    }
}

fn scope_binding(
    harness: &Harness,
    task_id: TaskId,
    worker_id: WorkerId,
    run_id: RunId,
    vendor_pid: i32,
) -> ScopeBinding {
    ScopeBinding {
        project_id: harness.project_id,
        task_id,
        worker_id,
        run_id,
        vendor_process: VendorProcessIdentity { pid: vendor_pid },
        expires_at: Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
    }
}

impl Drop for McpSubprocess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn coordination_mcp_lists_all_ten_tools() {
    let harness = Harness::start().await;
    let (run_id, task_id, worker_id) = harness.seed_run().await;
    let mut proxy = McpSubprocess::spawn(&harness, run_id, task_id, worker_id).await;

    let init = proxy.call(1, "initialize", json!({})).await;
    assert!(init.get("error").is_none(), "{init:?}");

    let list = proxy.call(2, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "crew_task",
            "crew_peers",
            "crew_peer_workspace",
            "crew_artifact_list",
            "crew_artifact_fetch",
            "crew_send",
            "crew_request_child",
            "crew_publish_artifact",
            "crew_report_blocked",
            "crew_ask_policy",
        ]
    );
    for tool in tools {
        assert!(tool["inputSchema"]["type"] == "object", "{tool:?}");
        assert!(tool["outputSchema"]["type"] == "object", "{tool:?}");
    }
}

#[tokio::test]
async fn coordination_mcp_fulfills_crew_task_and_crew_send_against_the_real_broker() {
    let harness = Harness::start().await;
    let (run_id, task_id, worker_id) = harness.seed_run().await;
    let mut proxy = McpSubprocess::spawn(&harness, run_id, task_id, worker_id).await;
    proxy.call(1, "initialize", json!({})).await;

    let task_call = proxy
        .call(
            2,
            "tools/call",
            json!({ "name": "crew_task", "arguments": {} }),
        )
        .await;
    let result = &task_call["result"];
    assert_eq!(result["isError"], false, "{task_call:?}");
    assert_eq!(result["structuredContent"]["taskId"], task_id.to_string());

    let send_call = proxy
        .call(
            3,
            "tools/call",
            json!({
                "name": "crew_send",
                "arguments": { "kind": "peerMessage", "payload": "hello from the real subprocess" },
            }),
        )
        .await;
    let result = &send_call["result"];
    assert_eq!(result["isError"], false, "{send_call:?}");
    assert_eq!(result["structuredContent"]["deliveryState"], "recorded");

    // Verify against the real database directly: the message the
    // subprocess sent is journaled with the *bound* worker id, never one
    // it could have supplied itself (it never had the chance to).
    let conn = rusqlite::Connection::open(&harness.database).unwrap();
    let sender: String = conn
        .query_row(
            "SELECT sender_worker_id FROM messages WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sender, worker_id.to_string());
}

#[tokio::test]
async fn coordination_mcp_rejects_a_smuggled_sender_worker_id_over_real_stdio() {
    let harness = Harness::start().await;
    let (run_id, task_id, worker_id) = harness.seed_run().await;
    let mut proxy = McpSubprocess::spawn(&harness, run_id, task_id, worker_id).await;
    proxy.call(1, "initialize", json!({})).await;

    let spoofed = WorkerId::new();
    let send_call = proxy
        .call(
            2,
            "tools/call",
            json!({
                "name": "crew_send",
                "arguments": {
                    "kind": "peerMessage",
                    "payload": "hi",
                    "senderWorkerId": spoofed.to_string(),
                },
            }),
        )
        .await;
    let result = &send_call["result"];
    assert_eq!(result["isError"], true, "{send_call:?}");

    let conn = rusqlite::Connection::open(&harness.database).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "the whole rejected call must never journal a message"
    );
}

#[tokio::test]
async fn coordination_mcp_rejects_a_run_id_the_token_is_not_bound_to() {
    let harness = Harness::start().await;
    let (run_id, task_id, worker_id) = harness.seed_run().await;

    // Reserve/bind for `run_id`, but launch the CLI with a *different*
    // --run-id -- the runtime's own scopedRunId will never match it.
    let wrong_run_id = RunId::new();
    let token = harness.scope_token_store.reserve_token();
    let (mut child, _stdin, _stdout) = McpSubprocess::launch(&harness, wrong_run_id, &token);
    let pid = child.id().expect("spawned child has a pid") as i32;
    harness
        .scope_token_store
        .bind(
            token,
            scope_binding(&harness, task_id, worker_id, run_id, pid),
        )
        .unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("coordination-mcp exits promptly on a scope mismatch")
        .expect("wait succeeds");
    assert!(
        !status.success(),
        "a run-id mismatch must be a hard failure, not silent success"
    );
}

#[tokio::test]
async fn coordination_mcp_fails_fast_with_no_scope_token_at_all() {
    let harness = Harness::start().await;
    let (run_id, _task_id, _worker_id) = harness.seed_run().await;

    let mut child = McpSubprocess::spawn_without_scope_token(&harness, run_id);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("coordination-mcp exits promptly with no scope token")
        .expect("wait succeeds");
    assert!(
        !status.success(),
        "a missing scope token must be a hard failure"
    );
}

#[tokio::test]
async fn coordination_mcp_rejects_an_expired_token() {
    let harness = Harness::start().await;
    let (run_id, task_id, worker_id) = harness.seed_run().await;

    let token = harness.scope_token_store.reserve_token();
    let (mut child, _stdin, _stdout) = McpSubprocess::launch(&harness, run_id, &token);
    let pid = child.id().expect("spawned child has a pid") as i32;
    harness
        .scope_token_store
        .bind(
            token,
            ScopeBinding {
                project_id: harness.project_id,
                task_id,
                worker_id,
                run_id,
                vendor_process: VendorProcessIdentity { pid },
                expires_at: Timestamp::parse("2000-01-01T00:00:00Z").unwrap(),
            },
        )
        .unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("coordination-mcp exits promptly on an expired token")
        .expect("wait succeeds");
    assert!(!status.success(), "an expired token must be a hard failure");
}

#[tokio::test]
async fn coordination_mcp_rejects_an_unrelated_or_already_exited_vendor_pid() {
    let harness = Harness::start().await;
    let (run_id, task_id, worker_id) = harness.seed_run().await;

    // A short-lived process that has already fully exited and been
    // reaped by the time coordination-mcp connects: its pid can no
    // longer be a live ancestor of anything, exactly like a genuinely
    // unrelated pid.
    let mut dead_vendor = Command::new("true")
        .spawn()
        .expect("spawn a short-lived process");
    let dead_vendor_pid = dead_vendor.id().expect("spawned child has a pid") as i32;
    dead_vendor
        .wait()
        .await
        .expect("the short-lived process exits");

    let (mut proxy, _token) =
        McpSubprocess::spawn_with_vendor_pid(&harness, run_id, task_id, worker_id, dead_vendor_pid);
    let status = tokio::time::timeout(Duration::from_secs(5), proxy.child.wait())
        .await
        .expect("coordination-mcp exits promptly on an unrelated vendor pid")
        .expect("wait succeeds");
    assert!(
        !status.success(),
        "an unrelated/dead vendor pid must be a hard failure"
    );
}

#[tokio::test]
async fn coordination_mcp_rejects_after_the_real_vendor_exits_and_is_revoked() {
    let harness = Harness::start().await;
    let (run_id, task_id, worker_id) = harness.seed_run().await;

    // A real vendor process: spawn it, bind the token to its real pid,
    // then let it actually exit -- simulating exactly what an adapter's
    // own supervision loop observes when its vendor process ends, and
    // exercising the documented mitigation: revoke_for_run as soon as
    // that exit is observed, before the token's own expiry.
    let mut vendor = Command::new("true")
        .spawn()
        .expect("spawn a real vendor process");
    let vendor_pid = vendor.id().expect("spawned child has a pid") as i32;
    let token = harness.scope_token_store.reserve_token();
    harness
        .scope_token_store
        .bind(
            token.clone(),
            scope_binding(&harness, task_id, worker_id, run_id, vendor_pid),
        )
        .unwrap();
    vendor.wait().await.expect("the vendor process exits");
    harness.scope_token_store.revoke_for_run(run_id);

    let (mut child, _stdin, _stdout) = McpSubprocess::launch(&harness, run_id, &token);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("coordination-mcp exits promptly once its vendor's token is revoked")
        .expect("wait succeeds");
    assert!(
        !status.success(),
        "a revoked-after-vendor-exit token must be a hard failure"
    );
}

#[tokio::test]
async fn coordination_mcp_descendant_may_reconnect_with_the_same_token_while_the_vendor_lives() {
    let harness = Harness::start().await;
    let (run_id, task_id, worker_id) = harness.seed_run().await;

    // Bind the token to *this test process's own* pid as the vendor: a
    // real, live process every child it spawns is a genuine descendant
    // of, for as long as the test runs.
    let vendor_pid = std::process::id() as i32;
    let token = harness.scope_token_store.reserve_token();
    harness
        .scope_token_store
        .bind(
            token.clone(),
            scope_binding(&harness, task_id, worker_id, run_id, vendor_pid),
        )
        .unwrap();

    let (child_a, stdin_a, stdout_a) = McpSubprocess::launch(&harness, run_id, &token);
    let mut first = McpSubprocess {
        child: child_a,
        stdin: stdin_a,
        stdout: stdout_a,
    };
    let first_init = first.call(1, "initialize", json!({})).await;
    assert!(first_init.get("error").is_none(), "{first_init:?}");
    drop(first);

    // A second, independent descendant reconnects with the *same* token
    // while the recorded vendor (this test process) is still alive.
    let (child_b, stdin_b, stdout_b) = McpSubprocess::launch(&harness, run_id, &token);
    let mut second = McpSubprocess {
        child: child_b,
        stdin: stdin_b,
        stdout: stdout_b,
    };
    let second_init = second.call(1, "initialize", json!({})).await;
    assert!(second_init.get("error").is_none(), "{second_init:?}");
}
