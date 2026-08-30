//! CREW-30 regression test: the fd-inheritance-through-fork race that made
//! the old bare-`connect()` liveness probe false-positive.
//!
//! **Mechanism (proven, not speculative):** macOS has no atomic
//! `SOCK_CLOEXEC`; `socket()` then a separate `fcntl(FD_CLOEXEC)` leaves a
//! window where a `fork()` -- from *any* subprocess spawn happening
//! anywhere in the process, not just this test's own -- hands a listening
//! fd to a child without close-on-exec. The child holds the socket
//! "listening" (a bare `connect()` completes against it) for its own
//! lifetime, however brief, but has no attach-protocol code behind that fd
//! and can never respond to anything.
//!
//! **What this test proves:** under exactly the load shape that reproduces
//! the race (continuous forking, moderate CPU contention), the *real*
//! production probe (`pane_socket::is_live`, which requires
//! [`attach::LIVENESS_MARKER`] within a bounded timeout, not just a
//! completed connect) reports zero false positives. This is the "green
//! under the new [probe]" half of the regression; the "red under the old
//! [bare-connect] probe" half is not re-derived here every run -- it is
//! already proven and recorded below and in `pane_socket::is_live`'s own
//! doc comment: **452 false positives out of 40,000 iterations (1.13%)**
//! with continuous forking, against 0/20,000 with no load and 0/20,000
//! with CPU-only load in the same investigation run. If `is_live` is ever
//! weakened back to a bare connect, this test is expected to start
//! failing under the same load, at roughly that same ~1% rate.
//!
//! Bounded to a few thousand iterations (not the 40k/~30min the original
//! investigation ran) so this stays a normal part of the gate rather than
//! a manual-only diagnostic -- the mechanism is proven, so this test's job
//! is catching a regression, not re-discovering the bug.

use std::os::unix::net::UnixListener as StdUnixListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crew_runtime::display::pane_socket::is_live;

/// Bounded by count and by wall time, whichever comes first -- this must
/// never turn into a multi-minute gate test.
const ITERATIONS: usize = 4_000;
const MAX_DURATION: Duration = Duration::from_secs(60);

/// Spins roughly half the machine's cores doing pointless work, to
/// reproduce the scheduling contention a real concurrent test run puts on
/// the scheduler -- deliberately *not* every core: saturating every core
/// with non-yielding busy loops starves tokio's own worker threads (which
/// also want `available_parallelism()` of them) entirely, making the
/// harness hang rather than race, which was a bug in the original
/// harness, not evidence about production (nothing suggests `pane/reopen`
/// hangs for real users under load). Stops when `stop` flips.
fn spawn_cpu_load(stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    let n = (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        / 2)
    .max(1);
    (0..n)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut x: u64 = 0;
                while !stop.load(Ordering::Relaxed) {
                    x = x.wrapping_add(1).wrapping_mul(2654435761);
                    std::hint::black_box(x);
                }
            })
        })
        .collect()
}

/// Continuously spawns and reaps short-lived children (`true`) on a
/// dedicated thread -- the necessary ingredient the investigation proved:
/// 0/20,000 with no load or CPU-only load, 452/40,000 once this was added.
/// Stops when `stop` flips.
fn spawn_fork_load(stop: Arc<AtomicBool>) -> std::thread::JoinHandle<u64> {
    std::thread::spawn(move || {
        let mut spawned: u64 = 0;
        while !stop.load(Ordering::Relaxed) {
            if let Ok(mut child) = std::process::Command::new("true").spawn() {
                let _ = child.wait();
                spawned += 1;
            }
        }
        spawned
    })
}

#[tokio::test]
async fn is_live_has_no_false_positives_under_fork_and_cpu_load() {
    let dir = tempfile::Builder::new()
        .prefix("crew-30-race-")
        .tempdir_in("/tmp")
        .expect("temp dir");

    let stop_cpu = Arc::new(AtomicBool::new(false));
    let cpu_threads = spawn_cpu_load(Arc::clone(&stop_cpu));
    let stop_fork = Arc::new(AtomicBool::new(false));
    let fork_thread = spawn_fork_load(Arc::clone(&stop_fork));

    let deadline = Instant::now() + MAX_DURATION;
    let mut false_positives: Vec<(usize, Duration)> = Vec::new();
    let mut i = 0usize;
    while i < ITERATIONS && Instant::now() < deadline {
        let path = dir.path().join(format!("race-{i}.sock"));

        // The exact race: bind (production and the old bare probe both
        // used a std listener here -- std closes synchronously on drop,
        // unlike tokio's deferred-to-reactor close, matching "the process
        // holding the fd is gone"), then drop it immediately.
        let dropped_at = {
            let _listener = StdUnixListener::bind(&path).expect("bind");
            Instant::now()
        };

        // The real production probe -- not a reimplemented stand-in.
        if is_live(&path).await {
            false_positives.push((i, dropped_at.elapsed()));
        }

        let _ = std::fs::remove_file(&path);
        i += 1;
    }

    stop_cpu.store(true, Ordering::Relaxed);
    for t in cpu_threads {
        let _ = t.join();
    }
    stop_fork.store(true, Ordering::Relaxed);
    let children_spawned = fork_thread.join().unwrap_or(0);

    if !false_positives.is_empty() {
        for (iteration, gap) in &false_positives {
            eprintln!(
                "CREW-30 regression: is_live falsely reported iteration {iteration} live \
                 {gap:?} after its listener was dropped"
            );
        }
    }

    assert!(
        false_positives.is_empty(),
        "is_live must never report a dropped/stale socket as live, even under fork+CPU load \
         ({} false positives out of {i} iterations, {children_spawned} children spawned during \
         the run) -- this is the exact CREW-30 regression: a bare connect can complete against \
         an fd a raced fork()'d child inherited, so the probe must require the liveness marker, \
         not just a completed connect",
        false_positives.len()
    );
}
