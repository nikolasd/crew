//! End-to-end lifecycle tests for the `crewd` daemon: single-instance
//! locking, stale-lock recovery, graceful stop, and idle shutdown.
//!
//! These drive the *compiled* binary (`env!("CARGO_BIN_EXE_crewd")`) as
//! real processes over real Unix domain sockets, with tempdirs for state.
//! Idle timings are kept tight (1s) so the suite stays fast, and every test
//! reaps or kills the processes it spawns so no orphans survive.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crew_runtime::lifecycle::should_idle_shutdown;
use serde_json::Value;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream as AsyncUnixStream;
use tokio::net::unix::OwnedWriteHalf;

const CREWD: &str = env!("CARGO_BIN_EXE_crewd");

/// A repository + state dir rooted under `/tmp` so socket paths stay well
/// within the platform `SUN_LEN` bound.
struct Fixture {
    state: tempfile::TempDir,
    repo: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let state = tempfile::Builder::new()
            .prefix("cl")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = tempfile::Builder::new()
            .prefix("bat-lc-r-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        Self { state, repo }
    }

    fn state_dir(&self) -> &Path {
        self.state.path()
    }

    fn repo_dir(&self) -> &Path {
        self.repo.path()
    }

    fn serve(&self, idle_seconds: Option<u64>) -> Command {
        let mut cmd = Command::new(CREWD);
        cmd.arg("serve")
            .arg("--state-dir")
            .arg(self.state_dir())
            .arg("--repo")
            .arg(self.repo_dir());
        if let Some(seconds) = idle_seconds {
            cmd.arg("--idle-seconds").arg(seconds.to_string());
        }
        cmd
    }
    fn serve_with_config(&self, config: &Path, idle_seconds: Option<u64>) -> Command {
        let mut cmd = self.serve(idle_seconds);
        cmd.arg("--config").arg(config);
        cmd
    }

    fn stop(&self) -> Command {
        let mut cmd = Command::new(CREWD);
        cmd.arg("stop")
            .arg("--state-dir")
            .arg(self.state_dir())
            .arg("--repo")
            .arg(self.repo_dir());
        cmd
    }
    /// Builds `crewd status --wait-seconds <n>`: retries the full
    /// init + `runtime/status` round-trip until the daemon has finished
    /// registering its runtime (the socket accepts connections before
    /// that point, so a bare connect is not enough).
    fn status(&self, wait_seconds: u64) -> Command {
        let mut cmd = Command::new(CREWD);
        cmd.arg("status")
            .arg("--state-dir")
            .arg(self.state_dir())
            .arg("--repo")
            .arg(self.repo_dir())
            .arg("--wait-seconds")
            .arg(wait_seconds.to_string());
        cmd
    }
}

/// Scans `<state>/repos/*/runtime.sock` for the daemon's socket.
fn find_socket(state: &Path) -> Option<PathBuf> {
    let repos = state.join("repos");
    let entries = std::fs::read_dir(&repos).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("runtime.sock");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Scans `<state>/repos/*/runtime.db`.
fn find_database(state: &Path) -> Option<PathBuf> {
    let repos = state.join("repos");
    let entries = std::fs::read_dir(&repos).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("runtime.db");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn find_lock(state: &Path) -> Option<PathBuf> {
    let repos = state.join("repos");
    let entries = std::fs::read_dir(&repos).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("runtime.lock");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Scans `<state>/repos/*/runtime.log`.
fn find_log(state: &Path) -> Option<PathBuf> {
    let repos = state.join("repos");
    let entries = std::fs::read_dir(&repos).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("runtime.log");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Waits up to `deadline` for the daemon's socket to appear.
fn wait_for_socket(state: &Path, deadline: Duration) -> PathBuf {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if let Some(socket) = find_socket(state) {
            return socket;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("daemon socket did not appear within {deadline:?}");
}

/// Kills and reaps a child, ignoring errors (used for survivors/cleanup).
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Waits up to `deadline` for `child` to exit, returning its exit code.
fn wait_for_exit(child: &mut Child, deadline: Duration) -> Option<i32> {
    let start = Instant::now();
    while start.elapsed() < deadline {
        match child.try_wait().unwrap() {
            Some(status) => return status.code(),
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    None
}

// ------------------------------------------------------------------ tests

#[test]
fn concurrent_serve_race_one_wins_other_exits_73() {
    let fixture = Fixture::new();

    // Spawn two servers against the same repo back to back. The O_EXCL lock
    // guarantees exactly one wins; the loser must exit 73 with a
    // machine-readable `already_running` document on stdout.
    let mut a = fixture
        .serve(Some(30))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut b = fixture
        .serve(Some(30))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    // Whichever exits first (with a non-None code) is the loser.
    let deadline = Duration::from_secs(10);
    let start = Instant::now();
    let mut loser: Option<usize> = None;
    while start.elapsed() < deadline && loser.is_none() {
        if a.try_wait().unwrap().is_some() {
            loser = Some(0);
        } else if b.try_wait().unwrap().is_some() {
            loser = Some(1);
        } else {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    let loser = loser.expect("one of the two servers should have lost the lock race and exited");
    let (loser_child, winner_child) = if loser == 0 {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };

    let code = loser_child.wait().unwrap().code();
    assert_eq!(code, Some(73), "the losing server must exit with code 73");

    let mut stdout = String::new();
    loser_child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let doc: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("loser stdout is not JSON: {stdout:?}: {e}"));
    assert_eq!(doc["status"], "already_running");
    assert!(doc["pid"].is_number(), "already_running doc carries a pid");
    assert!(
        doc["projectId"].is_string(),
        "already_running doc carries a projectId"
    );

    // The winner should still be serving.
    assert!(
        winner_child.try_wait().unwrap().is_none(),
        "the winning server should still be running"
    );
    kill(winner_child);
}

#[test]
fn concurrent_serve_over_stale_lock_one_wins() {
    let fixture = Fixture::new();

    // Seed a stale lock *file* with dead metadata but NO flock held: exactly
    // the shape a crashed daemon leaves behind. We resolve the lock path the
    // way the runtime does by warming up a throwaway server, then killing it
    // and overwriting its lock with definitively-dead contents.
    let mut warmup = fixture.serve(Some(30)).spawn().unwrap();
    wait_for_socket(fixture.state_dir(), Duration::from_secs(10));
    let lock_path = find_lock(fixture.state_dir()).expect("warmup server created a lock");
    kill(&mut warmup);
    let stale = serde_json::json!({
        "pid": 2_147_483_646_i64,
        "instanceToken": "stale-token",
        "runtimeVersion": "0.0.0",
        "projectId": "00000000-0000-0000-0000-000000000000",
        "socketPath": "/tmp/does-not-exist-crew.sock",
    });
    std::fs::write(&lock_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    if let Some(socket) = find_socket(fixture.state_dir()) {
        let _ = std::fs::remove_file(socket);
    }

    // Two servers now race over the stale lock. Under the old
    // remove-then-recreate scheme both could read the lock as stale, remove it,
    // and recreate -- ending up with two daemons owning the same socket and
    // database. With kernel flock the outcome is deterministic: exactly one
    // wins the exclusive lock; the other must exit 73.
    let mut a = fixture
        .serve(Some(30))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut b = fixture
        .serve(Some(30))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = Duration::from_secs(10);
    let start = Instant::now();
    let mut loser: Option<usize> = None;
    while start.elapsed() < deadline && loser.is_none() {
        if a.try_wait().unwrap().is_some() {
            loser = Some(0);
        } else if b.try_wait().unwrap().is_some() {
            loser = Some(1);
        } else {
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    let loser = loser.expect("exactly one server should lose the flock race and exit");
    let (loser_child, winner_child) = if loser == 0 {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };

    let code = loser_child.wait().unwrap().code();
    assert_eq!(code, Some(73), "the losing server must exit with code 73");

    let mut stdout = String::new();
    loser_child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let doc: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("loser stdout is not JSON: {stdout:?}: {e}"));
    assert_eq!(doc["status"], "already_running");
    assert!(doc["pid"].is_number(), "already_running doc carries a pid");

    // Exactly one winner, still serving, and it owns the socket.
    assert!(
        winner_child.try_wait().unwrap().is_none(),
        "the winning server should still be running"
    );
    wait_for_socket(fixture.state_dir(), deadline);
    kill(winner_child);
}

#[test]
fn stale_lock_is_recovered() {
    let fixture = Fixture::new();

    // Materialize the repo state dir and a stale lock: a definitely-dead pid
    // and a socket path that cannot be connected to. Neither liveness check
    // can pass, so startup must remove it and acquire the lock itself.
    //
    // We resolve the repo id the same way the runtime does by starting a
    // throwaway server, capturing the created lock path, killing it, then
    // overwriting the lock with stale contents.
    let mut warmup = fixture.serve(Some(30)).spawn().unwrap();
    wait_for_socket(fixture.state_dir(), Duration::from_secs(10));
    let lock_path = find_lock(fixture.state_dir()).expect("warmup server created a lock");
    kill(&mut warmup);
    // The killed server did not clean up its lock; overwrite it as stale.
    let stale = serde_json::json!({
        "pid": 2_147_483_646_i64,
        "instanceToken": "stale-token",
        "runtimeVersion": "0.0.0",
        "projectId": "00000000-0000-0000-0000-000000000000",
        "socketPath": "/tmp/does-not-exist-crew.sock",
    });
    std::fs::write(&lock_path, serde_json::to_vec(&stale).unwrap()).unwrap();
    // Also remove any leftover socket so connectability genuinely fails.
    if let Some(socket) = find_socket(fixture.state_dir()) {
        let _ = std::fs::remove_file(socket);
    }

    // Now a fresh server must recover the stale lock and start serving, then
    // idle-exit cleanly.
    let mut server = fixture.serve(Some(1)).spawn().unwrap();
    wait_for_socket(fixture.state_dir(), Duration::from_secs(10));

    let code = wait_for_exit(&mut server, Duration::from_secs(5));
    match code {
        Some(0) => {}
        other => {
            kill(&mut server);
            panic!("recovered server should idle-exit 0, got {other:?}");
        }
    }
}

#[test]
fn graceful_stop_removes_socket_only_after_journal_shutdown() {
    let fixture = Fixture::new();

    let mut server = fixture.serve(Some(30)).spawn().unwrap();
    let socket = wait_for_socket(fixture.state_dir(), Duration::from_secs(10));

    // The socket file can exist before the daemon has finished registering
    // its runtime, so a `stop` issued too early is answered with
    // `NotRunning`. Wait for the daemon to actually be serving first via
    // `crewd status --wait-seconds`, which polls the full init +
    // `runtime/status` round-trip until the runtime is registered.
    let status = fixture.status(30).status().unwrap();
    assert!(
        status.success(),
        "crewd status should succeed once the daemon is serving"
    );

    let status = fixture.stop().status().unwrap();
    assert!(status.success(), "crewd stop should exit 0");

    // `stop` returns only after the socket is removed, which the daemon does
    // only after the journal shutdown record is durably committed.
    assert!(
        !socket.exists(),
        "the socket must be gone after a graceful stop"
    );

    let code = wait_for_exit(&mut server, Duration::from_secs(5));
    assert_eq!(
        code,
        Some(0),
        "the daemon should exit 0 after a graceful stop"
    );

    // The final durable record must be a runtimeStopping event: proof the
    // journal was shut down before the socket disappeared.
    let db_path = find_database(fixture.state_dir()).expect("daemon created a database");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let saw_stopping = rt.block_on(async move {
        let db = crew_runtime::db::DatabaseHandle::start(db_path)
            .await
            .unwrap();
        let events = db.replay_events(0).await.unwrap();
        let saw = events
            .iter()
            .any(|e| e.event_json.contains("runtimeStopping"));
        db.shutdown().await.unwrap();
        saw
    });
    assert!(
        saw_stopping,
        "a durable runtimeStopping event must be journaled before shutdown"
    );

    // Prove the *clean* shutdown path ran: `db_actor_closed` is logged only
    // after the database actor thread was actually drained and joined, and it
    // is emitted before the socket is removed -- so with the socket already
    // gone the line must be present in runtime.log.
    let log_path = find_log(fixture.state_dir()).expect("daemon created a runtime.log");
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("db_actor_closed"),
        "the clean db-actor shutdown path must have run (db_actor_closed missing from {log_path:?}):\n{log}"
    );
}

#[test]
fn idle_shutdown_is_suppressed_by_a_live_connection() {
    let fixture = Fixture::new();

    // One-second idle interval.
    let mut server = fixture.serve(Some(1)).spawn().unwrap();
    let socket = wait_for_socket(fixture.state_dir(), Duration::from_secs(10));

    // Hold a live connection open across more than one idle interval; the
    // daemon must NOT exit while a client is connected.
    let conn = UnixStream::connect(&socket).unwrap();
    std::thread::sleep(Duration::from_millis(1800));
    assert!(
        server.try_wait().unwrap().is_none(),
        "a live connection must suppress idle shutdown"
    );

    // Disconnect; the idle timer restarts and the daemon exits after ~1s.
    drop(conn);
    let code = wait_for_exit(&mut server, Duration::from_secs(5));
    match code {
        Some(0) => {}
        other => {
            kill(&mut server);
            panic!("daemon should idle-exit 0 after the client disconnects, got {other:?}");
        }
    }
}

#[test]
fn idle_decision_ands_connections_and_active_runs() {
    let limit = Duration::from_secs(1);
    // Fully idle past the interval: shut down.
    assert!(should_idle_shutdown(0, 0, Duration::from_secs(2), limit));
    // A connected client suppresses shutdown.
    assert!(!should_idle_shutdown(1, 0, Duration::from_secs(2), limit));
    // An active run suppresses shutdown.
    assert!(!should_idle_shutdown(0, 1, Duration::from_secs(2), limit));
    // Idle but the interval has not elapsed yet.
    assert!(!should_idle_shutdown(
        0,
        0,
        Duration::from_millis(500),
        limit
    ));
}

// ------------------------------------------------------------------ IPC

/// Minimal NDJSON JSON-RPC client over a Unix domain socket, mirrored from
/// `tests/orchestration_rpc.rs`'s `Client` (each `tests/*.rs` is its own
/// compilation unit, so the helper is duplicated here on purpose).
struct IpcClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl IpcClient {
    async fn connect(path: &Path) -> Self {
        let stream = AsyncUnixStream::connect(path).await.unwrap();
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

    async fn initialize(&mut self, instance_id: &str, repo: &Path) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "client": { "name": "@nikolasd/crew", "version": "0.1.0" },
                "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
                "repository": {
                    "canonicalPath": repo.to_str().unwrap(),
                    "vcsRoot": repo.to_str().unwrap()
                },
                "auth": {
                    "role": "ompExtension",
                    "instanceId": instance_id,
                    "agentDirectory": repo.to_str().unwrap()
                },
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

/// A fake `claude` binary that passes the daemon's `version_gate` (answers
/// `--version` with a tested-range version) and, on a fresh start, journals a
/// transcript turn when a `[crew:` prompt is injected -- so a real
/// `run/submit` drives the vendor-task path with no billed CLI. Mirrors
/// `tests/tui_claude_registry.rs`'s fake claude (minus the nonce branch this
/// test never exercises).
fn write_fake_claude_script(scripts_dir: &Path, session_dir: &Path) -> PathBuf {
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "2.1.241 (Claude Code)"
  exit 0
fi
echo "Welcome to Claude Code!"
SESSION_ID="11111111-1111-4111-8111-000000000099"
TRANSCRIPT="{session_dir}/$SESSION_ID.jsonl"
if [ "$1" = "--resume" ]; then
  ( sleep 0.3; printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:30Z","message":{{"content":[{{"type":"text","text":"post-resume answer"}}]}}}}' >> "$TRANSCRIPT" ) &
  while IFS= read -r line; do :; done
  exit 0
fi
while IFS= read -r line; do
  case "$line" in
    *"[crew:"*)
      printf '%s\n' '{{"type":"user","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":"'"$line"'"}}}}' >> "$TRANSCRIPT"
      printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:01Z","message":{{"content":[{{"type":"text","text":"hi from the fixture e2e"}}]}}}}' >> "$TRANSCRIPT"
      printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","uuid":"entry-tool-1","timestamp":"2026-01-01T00:00:02Z","message":{{"content":[{{"type":"tool_use","name":"Bash","id":"toolu_1","input":{{"command":"ls"}}}}]}}}}' >> "$TRANSCRIPT"
      ;;
  esac
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

/// Extracts a run id from a replayed event envelope (tolerant of shape).
fn event_run_id(e: &Value) -> Option<&str> {
    e.get("runId").and_then(Value::as_str).or_else(|| {
        e.get("event")
            .and_then(|ev| ev.get("payload"))
            .and_then(|p| p.get("runId"))
            .and_then(Value::as_str)
    })
}

/// Polls `events/replay` until an event for `run_id` carries a
/// transcript-derived payload -- the fixture assistant text the fake claude
/// writes to its transcript, which the adapter tailer only emits after it has
/// actually observed the tailed transcript turn. This proves the run was
/// driven end-to-end (a turn was tailed), not merely registered by
/// `run/submit`.
async fn wait_for_run_event(client: &mut IpcClient, run_id: &str, deadline: Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        let replay = client
            .call(9, "events/replay", json!({ "afterSequence": 0 }))
            .await;
        if let Some(events) = replay.get("result").and_then(Value::as_array)
            && events.iter().any(|e| {
                event_run_id(e) == Some(run_id)
                    && serde_json::to_string(e).is_ok_and(|s| s.contains("hi from the fixture e2e"))
            })
        {
            return true;
        }
        if start.elapsed() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Unwind-safe guard that stops + reaps a spawned `crewd` on drop -- normal
/// return, assertion failure, or panic -- so a mid-test failure never leaks a
/// real `crewd` OS process (and its tmux server) into `/tmp`.
struct DaemonGuard<'a> {
    fixture: &'a Fixture,
    child: Option<Child>,
}

impl<'a> DaemonGuard<'a> {
    /// Spawn `crewd serve --config <config>` and wait for its socket; returns
    /// the guard plus the socket path.
    fn spawn(fixture: &'a Fixture, config: &Path) -> (Self, PathBuf) {
        let child = fixture
            .serve_with_config(config, Some(30))
            .spawn()
            .expect("served crewd must start");
        let socket = wait_for_socket(fixture.state_dir(), Duration::from_secs(15));
        (
            DaemonGuard {
                fixture,
                child: Some(child),
            },
            socket,
        )
    }

    /// Gracefully stop the daemon (`crewd stop`), reap, then force-kill if
    /// still alive. Returns the stop-command result and exit code only after
    /// process ownership is cleared; `None` means it was already reaped.
    fn shutdown(&mut self) -> Option<(std::io::Result<std::process::ExitStatus>, Option<i32>)> {
        let mut child = self.child.take()?;
        let stop_status = self.fixture.stop().status();
        let exit_code = wait_for_exit(&mut child, Duration::from_secs(15));
        kill(&mut child);
        Some((stop_status, exit_code))
    }
}

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
/// The last unproven integration seam from WP29, exercised at the *worker*
/// level (not just task registration): a real `crewd` OS process serves, an
/// OMP client submits a real `run/submit` whose fake-claude worker journals a
/// transcript turn over the actual Unix-socket JSON-RPC IPC, the process is
/// gracefully stopped and a fresh `crewd` started on the same state dir (a
/// genuine daemon restart), and a new IPC client can still replay that run's
/// journaled events. No billed vendor CLI is touched -- the `claude` adapter
/// is pointed at a fake claude via `--config`.
#[tokio::test]
async fn real_daemon_survives_serve_stop_serve_with_ipc_transcript() {
    let fixture = Fixture::new();
    let repo = std::fs::canonicalize(fixture.repo_dir()).unwrap();

    // Point the real daemon's `claude` (Tui) adapter at the fake claude, with
    // a session dir it journals its transcript into.
    let scripts_dir = tempfile::Builder::new()
        .prefix("bat-os-tui-")
        .tempdir_in("/tmp")
        .expect("create scripts dir");
    let session_dir = tempfile::Builder::new()
        .prefix("bat-os-sess-")
        .tempdir_in("/tmp")
        .expect("create session dir");
    let script_path = write_fake_claude_script(scripts_dir.path(), session_dir.path());
    let crew_json = scripts_dir.path().join("crew.json");
    std::fs::write(
        &crew_json,
        serde_json::to_string(&json!({
            "adapters": {
                "claude": {
                    "enabled": true,
                    "bin": script_path.to_str().unwrap(),
                    "mode": "tui",
                    "permissionMode": "default",
                    "profile": "test",
                    "sessionDir": session_dir.path().to_str().unwrap()
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    // First serve: real crewd, configured with the fake claude. The guard
    // stops + reaps the process on any exit path (assertion failure or panic).
    let (mut server, socket) = DaemonGuard::spawn(&fixture, &crew_json);

    let mut client = IpcClient::connect(&socket).await;
    let init = client.initialize("omp-1", &repo).await;
    assert!(init.get("error").is_none(), "initialize failed: {init:?}");

    // Register a task + a claude worker, then submit a run whose prompt makes
    // the fake claude journal a turn.
    let upsert = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    assert!(
        upsert.get("error").is_none(),
        "task/upsert failed: {upsert:?}"
    );
    let task_id = upsert["result"]["taskId"].as_str().unwrap().to_string();

    let register = client
        .call(
            3,
            "profile/register",
            json!({
                "adapter": "claude",
                "model": "test",
                "permissionEnvelope": {},
                "startupOptions": { "claude": { "mode": "tui" } },
                "environmentAllowlist": [],
                "source": "lifecycle-os-test"
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

    let wkr = client
        .call(4, "worker/create", json!({ "profileId": profile_id }))
        .await;
    assert!(wkr.get("error").is_none(), "worker/create failed: {wkr:?}");
    let worker_id = wkr["result"]["workerId"].as_str().unwrap().to_string();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id, "workerId": worker_id, "prompt": "[crew:fixture1] say hi" }),
        )
        .await;
    assert!(
        submit.get("error").is_none(),
        "run/submit failed: {submit:?}"
    );
    let run_id = submit["result"]["runId"].as_str().unwrap().to_string();

    // The run must journal at least one event (the fake claude turn) before
    // the restart.
    let journaled = wait_for_run_event(&mut client, &run_id, Duration::from_secs(30)).await;
    assert!(
        journaled,
        "run {run_id} must journal at least one event before restart"
    );

    // Graceful stop of the first daemon (genuine daemon restart). `shutdown`
    // clears/reaps it before these assertions can panic.
    let (stop_status, exit_code) = server.shutdown().expect("first daemon must still be live");
    assert!(
        stop_status.expect("crewd stop must execute").success(),
        "crewd stop must exit 0"
    );
    assert_eq!(exit_code, Some(0), "first daemon must exit 0 after stop");

    let (_server2, socket2) = DaemonGuard::spawn(&fixture, &crew_json);

    let mut client2 = IpcClient::connect(&socket2).await;
    let init2 = client2.initialize("omp-1", &repo).await;
    assert!(
        init2.get("error").is_none(),
        "re-initialize failed: {init2:?}"
    );

    // The run's journaled transcript must still be replayable after restart.
    let replay = client2
        .call(5, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    assert!(
        replay.get("error").is_none(),
        "replay after restart failed: {replay:?}"
    );
    let events = replay["result"]
        .as_array()
        .expect("events array after restart");
    let survived = events.iter().any(|e| {
        event_run_id(e) == Some(run_id.as_str())
            && serde_json::to_string(e).is_ok_and(|s| s.contains("hi from the fixture e2e"))
    });
    assert!(
        survived,
        "run {run_id} events must survive restart in the journal: {events:?}"
    );
}
