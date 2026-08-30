//! Pane-socket liveness, and the sweep that removes the dead ones
//! (CREW-3 wave 3 / CREW-15).
//!
//! Two consumers needed the same question answered — "is there really a
//! pane behind this socket?" — and both were getting it wrong in the same
//! way. `pane/reopen` treated the socket *file existing* as proof of a
//! live pane, and nothing anywhere removed a socket left behind by a
//! daemon that died without cleaning up. So a crashed daemon's leftover
//! file made `pane/reopen` claim a pane was reopenable and then fail to
//! connect to it.
//!
//! A Unix domain socket file outlives its listener, so existence proves
//! nothing. The only portable proof of a listener is connecting to it:
//! `connect()` on a bound-but-unlistened path fails with
//! `ECONNREFUSED`, and on a path whose daemon is gone fails the same way.
//!
//! ## Why the sweep is safe across repositories
//!
//! Since CREW-1 the pane directory is per-*user*, not per-repository: one
//! `$XDG_RUNTIME_DIR/crew` (or `/tmp/crew-<uid>`) holds the attach sockets
//! of every repository this user runs Crew against. So a daemon starting
//! for one repository sees another repository's live sockets sitting beside
//! its own dead ones, and must not touch them. Keying the sweep on
//! liveness rather than on ownership is what makes that safe without the
//! sweep needing to know anything about repositories: a socket that
//! answers a connection is somebody's live pane and is left alone,
//! whoever it belongs to.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

/// How recently a socket must have been touched to be spared by the sweep
/// regardless of whether it answers.
///
/// Closes the startup race: two daemons can start at once, and a socket
/// that has been created but whose listener is not yet accepting would
/// otherwise look dead to the other daemon's sweep and be unlinked out
/// from under it. Nothing legitimate depends on a dead socket disappearing
/// within seconds, so the margin is free.
const SWEEP_MIN_AGE: Duration = Duration::from_secs(30);

/// How long [`is_live`] waits for [`super::attach::LIVENESS_MARKER`]
/// after a connect succeeds, before deciding the pane is not live after
/// all. `is_live` is called at most once per `pane/reopen` (never in a
/// hot loop) and the marker is already in the kernel's send buffer by the
/// time a real `AttachServer` accepts, so this bound is rarely spent in
/// practice -- if it ever needs raising, that is itself evidence worth
/// reporting, not a knob to casually retune.
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Whether a pane attach socket has a real `AttachServer` behind it right
/// now.
///
/// CREW-30: a completed `connect()` alone is not enough. It proves a
/// listening fd exists at the path; it does not prove the process behind
/// it is `AttachServer` rather than, say, a `fork()`'d child that
/// inherited the fd without close-on-exec (macOS has no atomic
/// `SOCK_CLOEXEC`, so `socket()` then `fcntl(FD_CLOEXEC)` leaves a
/// window) and never speaks -- proven with a 40,000-iteration reproducer
/// that found the false connect 0 times without concurrent forking and
/// 452 times with it, never once able to complete a single write/read
/// round trip. So the probe now requires the one thing only a real
/// `AttachServer` does: write [`super::attach::LIVENESS_MARKER`] the
/// instant it accepts, before anything else. `is_live` connects, then
/// requires that exact marker within [`LIVENESS_PROBE_TIMEOUT`].
///
/// **This trades a possible false negative for the false positive it
/// closes, deliberately.** A real `AttachServer` whose accept loop is
/// somehow slow enough to miss the timeout makes `pane/reopen` refuse a
/// pane that actually is live -- recoverable (retry, or `pane/reopen`
/// again) and honest. Claiming a dead pane is live is the lie CREW-15
/// existed to kill, and this fix exists because it turned out CREW-15
/// had not fully killed it. Anyone tempted to "fix" a false negative here
/// by loosening this check should read this paragraph first.
pub async fn is_live(socket: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket).await else {
        return false;
    };
    let mut buf = [0u8; super::attach::LIVENESS_MARKER.len()];
    matches!(
        tokio::time::timeout(LIVENESS_PROBE_TIMEOUT, stream.read_exact(&mut buf)).await,
        Ok(Ok(_)) if buf == *super::attach::LIVENESS_MARKER
    )
}

/// Removes every socket in `panes_dir` that no longer has a listener,
/// returning the paths it unlinked.
///
/// Called at daemon startup: a daemon that died without cleaning up leaves
/// its sockets behind, and nothing else ever removes them. Live sockets —
/// including other repositories' — are left alone, as are sockets younger
/// than [`SWEEP_MIN_AGE`]. A missing directory is not an error: there is
/// simply nothing to sweep.
///
/// CREW-30: `is_live` now waits up to [`LIVENESS_PROBE_TIMEOUT`] for a
/// real protocol response instead of returning the instant a bare
/// connect refuses, so probing candidates one at a time here would let N
/// stale sockets left behind by a crash add up to N times that timeout
/// to every daemon startup. Every candidate old enough to judge is
/// probed concurrently instead (`join_all`) -- startup pays at most one
/// timeout's worth of wall-clock time no matter how many sockets a
/// crashed daemon left behind, not one per socket.
///
/// A live probe also now costs one `snapshot_and_subscribe()` on the
/// `AttachServer` side (a ring-buffer clone plus a broadcast subscription,
/// both dropped microseconds later when the probe's connection closes) --
/// before CREW-30 a probe cost only a connect and a refused SYN. A pane
/// snapshot today is a screen buffer, not scrollback, so N concurrent
/// probes doing N concurrent snapshot clones at startup is not a real
/// concern; re-check this the moment a snapshot ever grows to include
/// scrollback.
pub async fn sweep_stale(panes_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(panes_dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let candidates: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                return None;
            }
            // A socket too young to judge is left alone -- see SWEEP_MIN_AGE.
            if let Ok(metadata) = entry.metadata()
                && let Ok(modified) = metadata.modified()
                && now
                    .duration_since(modified)
                    .map(|age| age < SWEEP_MIN_AGE)
                    .unwrap_or(true)
            {
                return None;
            }
            Some(path)
        })
        .collect();

    let liveness =
        futures_util::future::join_all(candidates.iter().map(|path| is_live(path))).await;

    let mut removed = Vec::new();
    for (path, live) in candidates.into_iter().zip(liveness) {
        if live {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;
    // std's listener, deliberately, not tokio's: dropping a tokio
    // `UnixListener` defers the fd close to its reactor, so a probe
    // immediately after the drop can still see a listening socket. std
    // closes on drop synchronously, which is also what the real case looks
    // like -- the process holding the fd is gone.
    use std::os::unix::net::UnixListener;

    /// Backdates `path` past [`SWEEP_MIN_AGE`] so a test does not have to
    /// wait 30 seconds to exercise the sweep.
    fn backdate(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(120);
        let times = std::fs::FileTimes::new().set_modified(old);
        let file = std::fs::File::options().write(true).open(path);
        // A socket file cannot be opened for write on every platform; fall
        // back to filetime via a hard link-free utimensat through `touch`.
        if let Ok(file) = file {
            let _ = file.set_times(times);
        } else {
            let _ = std::process::Command::new("touch")
                .arg("-t")
                .arg("202001010000")
                .arg(path)
                .status();
        }
    }

    fn short_dir() -> tempfile::TempDir {
        // Bound under /tmp: a socket path has to fit the platform's
        // sun_path limit, which the default temp root can overflow.
        tempfile::Builder::new()
            .prefix("crew-pane-live-")
            .tempdir_in("/tmp")
            .expect("temp dir")
    }

    /// Binds a real (tokio) `UnixListener` at `path` and spawns a task
    /// that accepts exactly one connection and writes CREW-30's liveness
    /// marker to it -- the minimal stand-in for a real `AttachServer`
    /// these tests need to exercise `is_live`'s positive leg. Kept
    /// running for the test's lifetime by the spawned task itself, not
    /// by the returned handle.
    fn spawn_marker_server(path: &Path) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::UnixListener::bind(path).expect("bind");
        tokio::spawn(async move {
            if let Ok((mut stream, _addr)) = listener.accept().await {
                let _ = stream
                    .write_all(crate::display::attach::LIVENESS_MARKER)
                    .await;
            }
        })
    }

    /// The positive leg: `is_live` must still say yes when something that
    /// actually speaks the attach protocol is behind the socket. A probe
    /// that always says no would pass every "not live" test in this file
    /// perfectly and never be caught without this.
    #[tokio::test]
    async fn a_socket_serving_the_liveness_marker_is_live() {
        let dir = short_dir();
        let path = dir.path().join("live.sock");
        let _server = spawn_marker_server(&path);
        assert!(is_live(&path).await);
    }

    /// The floor: a bare accepting listener that completes a connect but
    /// never speaks the marker protocol at all (the exact shape CREW-30's
    /// fork-inheritance false positive takes: something answers, nothing
    /// ever responds) must never read as live. Deterministic and cheap,
    /// so it always runs -- the fork-load reproducer in
    /// `pane_socket_liveness_race.rs` is the probabilistic crown on top
    /// of this floor, not a replacement for it.
    #[tokio::test]
    async fn a_bare_accepting_listener_that_never_speaks_the_protocol_is_not_live() {
        let dir = short_dir();
        let path = dir.path().join("mute.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        let _server = tokio::spawn(async move {
            // Accepts and holds the connection open, but (unlike
            // `spawn_marker_server`) never writes anything -- exactly
            // what an fd a raced fork()'d child inherited looks like from
            // the probing side: a connect that succeeds, then silence.
            let _kept_alive = listener.accept().await;
            std::future::pending::<()>().await;
        });
        assert!(!is_live(&path).await);
    }

    #[tokio::test]
    async fn a_leftover_socket_file_with_no_listener_is_not_live() {
        let dir = short_dir();
        let path = dir.path().join("dead.sock");
        {
            let _listener = UnixListener::bind(&path).expect("bind");
        }
        // The file survives its listener -- which is exactly why existence
        // was never proof of a pane.
        assert!(path.exists(), "the socket file outlives its listener");
        assert!(!is_live(&path).await);
    }

    #[tokio::test]
    async fn the_sweep_removes_a_dead_socket_and_keeps_a_live_one() {
        let dir = short_dir();
        let dead = dir.path().join("dead.sock");
        {
            let _listener = UnixListener::bind(&dead).expect("bind");
        }
        backdate(&dead);

        let live = dir.path().join("live.sock");
        let _server = spawn_marker_server(&live);
        backdate(&live);

        let removed = sweep_stale(dir.path()).await;

        assert_eq!(removed, vec![dead.clone()]);
        assert!(!dead.exists(), "a dead socket must be unlinked");
        assert!(
            live.exists(),
            "a live socket must survive -- it may belong to another repository's daemon"
        );
    }

    #[tokio::test]
    async fn the_sweep_spares_a_socket_too_young_to_judge() {
        let dir = short_dir();
        let fresh = dir.path().join("fresh.sock");
        {
            let _listener = UnixListener::bind(&fresh).expect("bind");
        }
        // Deliberately NOT backdated: a socket this new may belong to a
        // daemon still starting up, and unlinking it would break it.
        let removed = sweep_stale(dir.path()).await;
        assert!(removed.is_empty(), "a fresh socket must be spared");
        assert!(fresh.exists());
    }

    #[tokio::test]
    async fn the_sweep_ignores_files_that_are_not_sockets() {
        let dir = short_dir();
        let stray = dir.path().join("notes.txt");
        std::fs::write(&stray, b"not a socket").expect("write");
        backdate(&stray);
        let mut perms = std::fs::metadata(&stray).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&stray, perms).unwrap();

        let removed = sweep_stale(dir.path()).await;

        assert!(removed.is_empty());
        assert!(
            stray.exists(),
            "the sweep must only ever unlink .sock files"
        );
    }

    #[tokio::test]
    async fn sweeping_a_missing_directory_is_not_an_error() {
        let dir = short_dir();
        let missing = dir.path().join("gone");
        assert!(sweep_stale(&missing).await.is_empty());
    }
}
