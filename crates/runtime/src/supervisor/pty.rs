//! PTY-based supervision for TUI-mode workers: the vendor CLI runs on a
//! real pseudo-terminal so it renders its interactive UI, while the
//! supervisor keeps the same discipline as [`super::process::ManagedProcess`]
//! -- an explicit, already-allowlisted environment (never implicit
//! inheritance), a process group of its own (the PTY spawn makes the child
//! a session leader, so `pgid == pid`), and escalating termination
//! (SIGINT -> SIGTERM -> SIGKILL, group-wide).
//!
//! Raw PTY output fans out only to an output broadcast channel for viewers
//! (the attach server); it is never parsed -- observation of TUI workers
//! happens through vendor transcripts, not scraped terminal frames.
//!
//! Two concerns `portable_pty::Child` does not solve for a daemon that
//! must never block a `tokio` worker thread are handled by dedicated OS
//! threads, mirroring the output pump thread below:
//!
//! - `Child::wait`/`try_wait` are blocking syscalls, and its `unix` impl
//!   (plain `std::process::Child`) does not reap on drop -- a handle
//!   dropped without an explicit `wait()` leaves a zombie until some
//!   *other* `wait()` call on the same pid happens to run, if ever. A
//!   single reaper thread, spawned at construction and owning the
//!   `Child` exclusively, blocks on one `wait()` call and republishes the
//!   result through a [`watch`] channel -- so exactly one `waitpid` ever
//!   runs, reaping is guaranteed the instant the process dies regardless
//!   of when (or whether) anything calls `wait`/`terminate`, and the
//!   channel is freely cloneable for concurrent exit observation.
//! - PTY writes (`write_all`/`flush` on the master) are blocking and can
//!   stall under backpressure; a dedicated writer thread owns the writer
//!   and drains a bounded job queue, so `write_input` only ever awaits
//!   channel capacity and an ack, never the write itself, on the runtime.

use std::future::Future;
use std::io::{Read, Write};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::process::{EscalationTimings, SpawnSpec, SupervisorError, TerminationOutcome};

/// Initial terminal geometry for TUI workers. Viewers may later request a
/// different size through [`PtyProcess::resize`].
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// Broadcast capacity in output chunks. A lagging viewer skips ahead
/// (misses frames) rather than exerting backpressure on the worker.
const OUTPUT_CHANNEL_CAPACITY: usize = 64;

/// Bound on queued-but-unwritten input jobs. A caller awaiting
/// [`PtyProcess::write_input`] blocks on channel capacity (an async
/// await, never a blocked OS thread) once this many writes are already
/// queued ahead of it.
const INPUT_CHANNEL_CAPACITY: usize = 32;

/// Poll interval for the termination escalation's own liveness probing
/// (distinct from exit observation, which is now push-based via the
/// reaper thread's [`watch`] channel).
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// One queued write: the bytes to send to the PTY master and where to
/// deliver the result, so a write failure is surfaced to the caller
/// rather than swallowed by the writer thread.
struct WriteJob {
    bytes: Vec<u8>,
    ack: oneshot::Sender<std::io::Result<()>>,
}

/// Wrapper asserting that a MasterPty is safe to share across threads.
/// The underlying PTY file descriptor is thread-safe at the OS level.
struct SyncMasterPty(Box<dyn MasterPty + Send>);

// SAFETY: PTY file descriptors are managed by the OS kernel which
// serializes concurrent access, making them inherently thread-safe.
unsafe impl Sync for SyncMasterPty {}

impl std::ops::Deref for SyncMasterPty {
    type Target = dyn MasterPty + Send;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// A supervised child process running on a pseudo-terminal: raw output
/// fan-out for attach viewers, input injection, resize, and the same
/// group-wide escalating termination as the pipe-based supervisor.
pub struct PtyProcess {
    master: SyncMasterPty,
    input_tx: mpsc::Sender<WriteJob>,
    pid: i32,
    out_tx: broadcast::Sender<Vec<u8>>,
    escalation: EscalationTimings,
    /// Fed by the reaper thread spawned in [`Self::spawn`]; `None` until
    /// the child has exited. Cloneable, so any number of concurrent
    /// watchers (and `wait`/`terminate` themselves) can observe the same
    /// exit without racing each other on the underlying `waitpid`.
    exit_rx: watch::Receiver<Option<ExitStatus>>,
}

impl PtyProcess {
    /// Spawns `spec` on a fresh PTY. Environment semantics are identical
    /// to [`super::Supervisor::spawn`]: the child receives *exactly*
    /// `spec.env` (env-cleared first), nothing inherited implicitly.
    ///
    /// # Errors
    /// Returns [`SupervisorError`] if the PTY cannot be opened, the
    /// process cannot be spawned, or its pid cannot be observed.
    pub fn spawn(spec: &SpawnSpec, escalation: EscalationTimings) -> Result<Self, SupervisorError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|source| SupervisorError::Pty {
                message: format!("openpty failed: {source}"),
            })?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        cmd.env_clear();
        for (name, value) in &spec.env {
            cmd.env(name, value);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|source| SupervisorError::Spawn {
                program: spec.program.clone(),
                source: std::io::Error::other(source.to_string()),
            })?;
        // The slave fd stays open inside the child; the parent's copy must
        // close so reads on the master observe EOF when the child exits.
        drop(pair.slave);

        let pid = child
            .process_id()
            .ok_or(SupervisorError::MissingPid)
            .map(|pid| pid as i32)?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|source| SupervisorError::Pty {
                message: format!("cloning pty reader failed: {source}"),
            })?;
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|source| SupervisorError::Pty {
                message: format!("taking pty writer failed: {source}"),
            })?;

        let (out_tx, _) = broadcast::channel(OUTPUT_CHANNEL_CAPACITY);
        // Reads on a PTY master are blocking; a dedicated thread per
        // worker pumps raw bytes into the broadcast. The thread ends on
        // EOF/EIO (child exited and slave closed) and having zero viewers
        // is normal -- send errors are ignored.
        let pump_tx = out_tx.clone();
        std::thread::Builder::new()
            .name(format!("pty-output-{pid}"))
            .spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = pump_tx.send(buf[..n].to_vec());
                        }
                    }
                }
            })
            .map_err(|source| SupervisorError::Pty {
                message: format!("spawning pty output pump thread failed: {source}"),
            })?;

        // Writes are blocking too; a dedicated thread owns the writer
        // exclusively and drains queued jobs, so `write_input` never
        // blocks the runtime -- it only awaits queue capacity and an ack.
        // The thread (and the caller's queued sends) end once every
        // `input_tx` clone is dropped (the channel closes and
        // `blocking_recv` returns `None`).
        let (input_tx, mut input_rx) = mpsc::channel::<WriteJob>(INPUT_CHANNEL_CAPACITY);
        std::thread::Builder::new()
            .name(format!("pty-input-{pid}"))
            .spawn(move || {
                while let Some(job) = input_rx.blocking_recv() {
                    let result = writer.write_all(&job.bytes).and_then(|()| writer.flush());
                    // The caller may have stopped awaiting (e.g. it timed
                    // out); a dropped ack receiver is not this thread's
                    // problem.
                    let _ = job.ack.send(result);
                }
            })
            .map_err(|source| SupervisorError::Pty {
                message: format!("spawning pty input writer thread failed: {source}"),
            })?;

        // A single reaper thread is the *only* caller of `Child::wait`
        // for this process, for its whole lifetime: it owns `child`
        // exclusively, blocks on the one syscall that both observes and
        // reaps the exit, and republishes the result. This closes the
        // double-wait hazard (two callers racing `try_wait`/`wait` on the
        // same handle) and guarantees reaping happens even if this
        // `PtyProcess` is dropped without anyone calling `wait` or
        // `terminate` -- the thread runs independently of the struct's
        // lifetime and finishes its `wait()` regardless.
        let (exit_tx, exit_rx) = watch::channel(None::<ExitStatus>);
        let mut child = child;
        std::thread::Builder::new()
            .name(format!("pty-reaper-{pid}"))
            .spawn(move || {
                let status = child
                    .wait()
                    .unwrap_or_else(|_| ExitStatus::with_exit_code(1));
                // No receiver left (every `PtyProcess` clone already
                // observed exit and was dropped) is not an error -- the
                // reap itself already happened as a side effect of the
                // `wait()` call above.
                let _ = exit_tx.send(Some(status));
            })
            .map_err(|source| SupervisorError::Pty {
                message: format!("spawning pty reaper thread failed: {source}"),
            })?;

        Ok(Self {
            master: SyncMasterPty(pair.master),
            input_tx,
            pid,
            out_tx,
            escalation,
            exit_rx,
        })
    }

    /// This process's own pid. The PTY spawn makes the child a session
    /// leader, so this is also its process group id.
    #[must_use]
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Writes raw bytes (keystrokes) to the PTY master -- what the worker
    /// reads as terminal input. The actual blocking write happens on a
    /// dedicated thread; this only awaits queue capacity and the write's
    /// result, so it is safe to call from a `tokio` task even while the
    /// worker is not draining its input (e.g. mid-render).
    ///
    /// # Errors
    /// Returns [`SupervisorError::Pty`] if the write fails (e.g. the
    /// worker exited and the PTY closed) or the writer thread is gone.
    pub async fn write_input(&self, bytes: &[u8]) -> Result<(), SupervisorError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.input_tx
            .send(WriteJob {
                bytes: bytes.to_vec(),
                ack: ack_tx,
            })
            .await
            .map_err(|_| SupervisorError::Pty {
                message: "pty input writer thread is no longer running".to_string(),
            })?;
        ack_rx
            .await
            .map_err(|_| SupervisorError::Pty {
                message: "pty input writer thread dropped the write without a result".to_string(),
            })?
            .map_err(|source| SupervisorError::Pty {
                message: format!("pty input write failed: {source}"),
            })
    }

    /// Subscribes a new viewer to the raw PTY output stream. Lagging
    /// viewers skip missed chunks; they never slow the worker down.
    #[must_use]
    pub fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.out_tx.subscribe()
    }

    /// Resizes the PTY (e.g. to the attach server's largest-viewer
    /// geometry). A resize failure is not actionable for the caller and
    /// never fatal to the worker -- it is intentionally swallowed.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// The current exit status if the reaper thread has already observed
    /// one, without waiting. The `try_wait`-equivalent primitive that
    /// `terminate`'s escalation steps poll -- reading the channel's
    /// latest value, never touching the child itself (only the reaper
    /// thread does that).
    fn try_exit(&self) -> Option<ExitStatus> {
        self.exit_rx.borrow().clone()
    }

    /// Waits for the process to exit on its own, without signaling.
    /// Equivalent to `exit_watcher().await` but takes `&mut self` for API
    /// symmetry with the pipe-based supervisor's `wait`.
    pub async fn wait(&mut self) -> ExitStatus {
        self.exit_watcher().await
    }

    /// An owned, independently-pollable future that resolves once this
    /// process has exited. Unlike `wait`, this takes `&self` and may be
    /// called any number of times concurrently -- each call clones the
    /// reaper thread's `watch` receiver, so many observers (e.g. the
    /// attach server watching for exit alongside the orchestrator calling
    /// `terminate`) see the same exit without racing a shared handle.
    pub fn exit_watcher(&self) -> impl Future<Output = ExitStatus> + Send + 'static {
        let mut exit_rx = self.exit_rx.clone();
        async move {
            loop {
                if let Some(status) = exit_rx.borrow().as_ref() {
                    return status.clone();
                }
                if exit_rx.changed().await.is_err() {
                    // The reaper thread is gone without ever publishing a
                    // status -- it must have panicked before `wait()`
                    // returned. Report a failure status rather than hang
                    // forever; this should not happen in practice since
                    // the only fallible step inside the thread already
                    // falls back to `ExitStatus::with_exit_code(1)`.
                    return ExitStatus::with_exit_code(1);
                }
            }
        }
    }

    /// Gracefully terminates the process *tree*, escalating
    /// SIGINT -> SIGTERM -> SIGKILL group-wide, mirroring
    /// [`super::process::ManagedProcess::terminate`]'s discipline: a
    /// leader observed already exited is never signaled at all, and each
    /// later signal is guarded by a fresh group liveness probe.
    pub async fn terminate(&mut self) -> TerminationOutcome {
        if let Some(status) = self.try_exit() {
            return TerminationOutcome::Exited {
                code: Some(status.exit_code() as i32),
            };
        }

        let _ = self.signal_group(Signal::SIGINT);
        let mut leader_outcome = self.wait_out_step(self.escalation.sigint_to_sigterm).await;

        if leader_outcome.is_none() && self.group_is_live() {
            let _ = self.signal_group(Signal::SIGTERM);
            leader_outcome = self.wait_out_step(self.escalation.sigterm_to_sigkill).await;
        }

        if let Some(outcome) = leader_outcome {
            return outcome;
        }

        if self.group_is_live() {
            let _ = self.signal_group(Signal::SIGKILL);
            let status = self.wait().await;
            let _ = status;
            return TerminationOutcome::Killed;
        }
        let status = self.wait().await;
        TerminationOutcome::Exited {
            code: Some(status.exit_code() as i32),
        }
    }

    /// Sends `signal` to the whole process group (the PTY child is its
    /// session and group leader).
    fn signal_group(&self, signal: Signal) -> nix::Result<()> {
        kill(Pid::from_raw(-self.pid), signal)
    }

    /// Fresh existence probe for the process group (signal 0).
    fn group_is_live(&self) -> bool {
        kill(Pid::from_raw(-self.pid), None).is_ok()
    }

    /// Polls out up to `duration` for the leader to exit. Like the
    /// pipe-based supervisor, the *whole group* must be empty before the
    /// leader's exit is reported as the final outcome; otherwise the
    /// remaining window is honored so a live descendant gets its grace
    /// period before the caller escalates.
    async fn wait_out_step(&mut self, duration: Duration) -> Option<TerminationOutcome> {
        let deadline = tokio::time::Instant::now() + duration;
        let mut leader_outcome = None;
        while tokio::time::Instant::now() < deadline {
            if leader_outcome.is_none()
                && let Some(status) = self.try_exit()
            {
                leader_outcome = Some(TerminationOutcome::Exited {
                    code: Some(status.exit_code() as i32),
                });
                if !self.group_is_live() {
                    return leader_outcome;
                }
            }
            tokio::time::sleep(EXIT_POLL_INTERVAL.min(duration)).await;
        }

        if self.group_is_live() {
            None
        } else {
            leader_outcome
        }
    }
}

impl Drop for PtyProcess {
    /// Parity with the pipe-based supervisor's `kill_on_drop(true)`: a
    /// dropped handle must never leak a running worker tree. This only
    /// *signals* -- it never blocks waiting for the signal to take
    /// effect. Reaping is not this Drop's job and is guaranteed
    /// regardless: the reaper thread spawned in [`Self::spawn`] runs
    /// independently of this struct's lifetime and completes its
    /// `wait()` (reaping the child) the moment the process actually
    /// dies, whether or not anything ever called `terminate`/`wait`, and
    /// whether it dies before or after this `drop` runs.
    fn drop(&mut self) {
        if self.try_exit().is_none() {
            let _ = kill(Pid::from_raw(-self.pid), Signal::SIGKILL);
        }
    }
}
