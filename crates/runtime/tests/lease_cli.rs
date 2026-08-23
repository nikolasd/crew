//! Integration tests for `crewd lease release` (R86): the operator
//! remedy for a lease whose owning session correlation was never
//! persisted. Such a lease is unreleasable over RPC -- `workspace/release`
//! is owner-gated and a new session is a different principal -- so the
//! compiled CLI binary, run directly against the lease database with no
//! daemon involved, must be able to force-release it by id, while:
//! - refusing when a runtime's socket exists (a live daemon's monitors
//!   could never see the out-of-band write),
//! - refusing an `active` lease without `--yes`,
//! - persisting the operation intent before the release and journaling
//!   `LeaseReleased`, so `events/replay` and audit export never show a
//!   `LeaseAcquired` with no terminating event.

use std::path::PathBuf;
use std::process::Command;

use batman_protocol::{IsolationKind, LeaseMode, RunId};
use batman_runtime::paths::RuntimePaths;
use batman_runtime::workspace::LeaseService;

/// Creates a state root plus a git repository, resolves the runtime
/// paths exactly as the CLI does, and seeds one active lease nobody's
/// session owns.
fn seed_orphan_lease() -> (tempfile::TempDir, PathBuf, String, RunId) {
    let state_dir = tempfile::Builder::new()
        .prefix("bat-lease-cli-")
        .tempdir_in("/tmp")
        .expect("create state dir");
    let repo = state_dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    let git = Command::new("git")
        .current_dir(&repo)
        .args(["init"])
        .output()
        .expect("git init");
    assert!(git.status.success());

    let paths = RuntimePaths::resolve(state_dir.path(), &repo).expect("resolve runtime paths");
    std::fs::create_dir_all(&paths.root).expect("create runtime root");
    let leases = LeaseService::open(paths.project_id, &paths.root.join("workspace-leases.db"))
        .expect("open lease service");
    let run_id = RunId::new();
    let created = leases
        .acquire(run_id, LeaseMode::Write, Some(IsolationKind::Shared))
        .expect("acquire lease");
    leases
        .activate(created.lease_id.clone(), repo.display().to_string())
        .expect("activate lease");

    (state_dir, repo, created.lease_id, run_id)
}

fn release_cmd(state_dir: &std::path::Path, repo: &std::path::Path, lease_id: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_crewd"));
    cmd.args([
        "lease",
        "release",
        "--state-dir",
        state_dir.to_str().unwrap(),
        "--repo",
        repo.to_str().unwrap(),
        "--lease-id",
        lease_id,
    ]);
    cmd
}

#[test]
fn lease_release_frees_an_orphaned_lease_and_journals_it() {
    let (state_dir, repo, lease_id, run_id) = seed_orphan_lease();

    // An active lease is refused without --yes: releasing it strips a
    // run's workspace claim (review E2).
    let refused = release_cmd(state_dir.path(), &repo, &lease_id)
        .output()
        .expect("run crewd lease release");
    assert!(
        !refused.status.success(),
        "an active lease must be refused without --yes"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("pass --yes"),
        "the refusal must name the confirmation flag"
    );

    let output = release_cmd(state_dir.path(), &repo, &lease_id)
        .arg("--yes")
        .output()
        .expect("run crewd lease release --yes");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "release must succeed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!("lease {lease_id} released")),
        "must confirm the release: {stdout}"
    );
    // The shared worktree is the repository itself: still on disk, never
    // torn down, and the operator is told so.
    assert!(
        stdout.contains("left in place"),
        "must report the surviving shared directory: {stdout}"
    );

    // The claim is genuinely gone: a fresh exclusive acquire succeeds.
    let paths = RuntimePaths::resolve(state_dir.path(), &repo).expect("resolve runtime paths");
    let leases = LeaseService::open(paths.project_id, &paths.root.join("workspace-leases.db"))
        .expect("reopen lease service");
    leases
        .acquire(RunId::new(), LeaseMode::Write, Some(IsolationKind::Shared))
        .expect("the repository must be free after the forced release");

    // The mutation is durable in the journal (invariant 7's commit half):
    // a LeaseReleased event for the freed lease's run...
    let conn = rusqlite::Connection::open(&paths.database).expect("open runtime db");
    let released_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 \
             AND event_json LIKE '%leaseReleased%'",
            rusqlite::params![run_id.to_string()],
            |row| row.get(0),
        )
        .expect("query events");
    assert_eq!(
        released_events, 1,
        "the forced release must journal exactly one LeaseReleased"
    );
    // ...and an acknowledged operation intent (invariant 4).
    let (kind, acked): (String, Option<String>) = conn
        .query_row("SELECT kind, acknowledged_at FROM operations", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .expect("query operations");
    assert_eq!(kind, "cli.lease.release");
    assert!(
        acked.is_some(),
        "the completed release must acknowledge its intent"
    );

    // Releasing again is refused with a distinct exit code (2), on stderr.
    let again = release_cmd(state_dir.path(), &repo, &lease_id)
        .arg("--yes")
        .output()
        .expect("run crewd lease release again");
    assert_eq!(
        again.status.code(),
        Some(2),
        "an already-released lease must exit 2"
    );
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already released"),
        "must name the idempotency refusal on stderr"
    );
}

#[test]
fn lease_release_refuses_an_unknown_lease_id() {
    let (state_dir, repo, _lease_id, _run_id) = seed_orphan_lease();

    let output = release_cmd(state_dir.path(), &repo, "no-such-lease")
        .output()
        .expect("run crewd lease release");
    assert_eq!(
        output.status.code(),
        Some(1),
        "an unknown id must exit 1, distinct from already-released's 2"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no lease no-such-lease exists"),
        "must name the missing lease"
    );
}

#[test]
fn lease_release_refuses_while_a_runtime_is_serving() {
    use nix::fcntl::{Flock, FlockArg};

    let (state_dir, repo, lease_id, _run_id) = seed_orphan_lease();
    let paths = RuntimePaths::resolve(state_dir.path(), &repo).expect("resolve runtime paths");

    // Liveness is the held advisory flock -- exactly what a serving
    // daemon owns and what `crewd stop` probes -- NOT the socket file.
    std::fs::write(
        &paths.lock,
        serde_json::json!({
            "pid": std::process::id(),
            "instanceToken": "test-token",
            "runtimeVersion": "0.0.0-test",
            "projectId": paths.project_id.to_string(),
            "socketPath": paths.socket.display().to_string(),
        })
        .to_string(),
    )
    .expect("write lock metadata");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&paths.lock)
        .expect("open lock file");
    let _held = Flock::lock(lock_file, FlockArg::LockExclusiveNonblock)
        .map_err(|(_, errno)| errno)
        .expect("hold the advisory lock like a live daemon");

    let output = release_cmd(state_dir.path(), &repo, &lease_id)
        .arg("--yes")
        .output()
        .expect("run crewd lease release");
    assert!(
        !output.status.success(),
        "a live runtime must refuse the out-of-band release"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("crewd stop"),
        "the refusal must name the remedy"
    );

    // Nothing was written: the lease still holds its claim.
    let leases = LeaseService::open(paths.project_id, &paths.root.join("workspace-leases.db"))
        .expect("reopen lease service");
    assert!(
        leases
            .acquire(RunId::new(), LeaseMode::Write, Some(IsolationKind::Shared))
            .is_err(),
        "the refused release must leave the exclusive claim in place"
    );
}

/// R86 review W1: an unclean crash (SIGKILL, machine crash) leaves
/// `runtime.sock` on disk with no live flock holder -- the exact case
/// this command exists for. A stale socket alone must NOT refuse, or the
/// no-remedy condition is reinstated for crashes and the operator is
/// sent to a `crewd stop` that reports NotRunning and removes nothing.
#[test]
fn lease_release_proceeds_past_a_stale_socket_left_by_a_crash() {
    let (state_dir, repo, lease_id, _run_id) = seed_orphan_lease();
    let paths = RuntimePaths::resolve(state_dir.path(), &repo).expect("resolve runtime paths");
    std::fs::write(&paths.socket, b"").expect("simulate a crash-orphaned socket");

    let output = release_cmd(state_dir.path(), &repo, &lease_id)
        .arg("--yes")
        .output()
        .expect("run crewd lease release");
    assert!(
        output.status.success(),
        "a stale socket with no flock holder must not block the remedy: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
