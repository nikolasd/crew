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

/// Whether a pane attach socket has a listener behind it right now.
///
/// Connecting is the probe, because it is the only portable one. The
/// connection is dropped immediately; the attach server's accept loop
/// spawns a viewer task that finds the peer gone and exits, which costs a
/// task and no durable state — deliberately preferred over trusting
/// `Path::exists`, which is what the bug was.
pub async fn is_live(socket: &Path) -> bool {
    UnixStream::connect(socket).await.is_ok()
}

/// Removes every socket in `panes_dir` that no longer has a listener,
/// returning the paths it unlinked.
///
/// Called at daemon startup: a daemon that died without cleaning up leaves
/// its sockets behind, and nothing else ever removes them. Live sockets —
/// including other repositories' — are left alone, as are sockets younger
/// than [`SWEEP_MIN_AGE`]. A missing directory is not an error: there is
/// simply nothing to sweep.
pub async fn sweep_stale(panes_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(panes_dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        // A socket too young to judge is left alone -- see SWEEP_MIN_AGE.
        if let Ok(metadata) = entry.metadata()
            && let Ok(modified) = metadata.modified()
            && now
                .duration_since(modified)
                .map(|age| age < SWEEP_MIN_AGE)
                .unwrap_or(true)
        {
            continue;
        }
        if is_live(&path).await {
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

    #[tokio::test]
    async fn a_socket_with_a_listener_is_live() {
        let dir = short_dir();
        let path = dir.path().join("live.sock");
        let _listener = UnixListener::bind(&path).expect("bind");
        assert!(is_live(&path).await);
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
        let _listener = UnixListener::bind(&live).expect("bind");
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
