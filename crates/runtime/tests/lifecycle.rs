//! End-to-end lifecycle tests for the `crewd` daemon: single-instance
//! locking, stale-lock recovery, graceful stop, and idle shutdown.
//!
//! These drive the *compiled* binary (`env!("CARGO_BIN_EXE_crewd")`) as
//! real processes over real Unix domain sockets, with tempdirs for state.
//! Idle timings are kept tight (1s) so the suite stays fast, and every test
//! reaps or kills the processes it spawns so no orphans survive.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use batman_runtime::lifecycle::should_idle_shutdown;
use serde_json::Value;

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
            .prefix("bat-lc-s-")
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

    fn stop(&self) -> Command {
        let mut cmd = Command::new(CREWD);
        cmd.arg("stop")
            .arg("--state-dir")
            .arg(self.state_dir())
            .arg("--repo")
            .arg(self.repo_dir());
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
        let db = batman_runtime::db::DatabaseHandle::start(db_path)
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
