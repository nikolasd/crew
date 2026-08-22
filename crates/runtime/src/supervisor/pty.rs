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

use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

use super::process::{EscalationTimings, SpawnSpec, SupervisorError, TerminationOutcome};

/// Initial terminal geometry for TUI workers. Viewers may later request a
/// different size through [`PtyProcess::resize`].
const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 32;

/// Broadcast capacity in output chunks. A lagging viewer skips ahead
/// (misses frames) rather than exerting backpressure on the worker.
const OUTPUT_CHANNEL_CAPACITY: usize = 64;

/// Poll interval for exit observation: `portable_pty`'s `wait()` is
/// blocking, so exit is observed by polling `try_wait` on the runtime
/// instead of parking a blocking thread per worker.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A supervised child process running on a pseudo-terminal: raw output
/// fan-out for attach viewers, input injection, resize, and the same
/// group-wide escalating termination as the pipe-based supervisor.
pub struct PtyProcess {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    pid: i32,
    out_tx: broadcast::Sender<Vec<u8>>,
    escalation: EscalationTimings,
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
        let writer = pair
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

        Ok(Self {
            master: pair.master,
            writer: Mutex::new(writer),
            child,
            pid,
            out_tx,
            escalation,
        })
    }

    /// This process's own pid. The PTY spawn makes the child a session
    /// leader, so this is also its process group id.
    #[must_use]
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Writes raw bytes (keystrokes) to the PTY master -- what the worker
    /// reads as terminal input.
    ///
    /// # Errors
    /// Returns [`SupervisorError::Pty`] if the write fails (e.g. the
    /// worker exited and the PTY closed).
    pub fn write_input(&self, bytes: &[u8]) -> Result<(), SupervisorError> {
        let mut writer = self
            .writer
            .lock()
            .expect("pty writer mutex is never poisoned");
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
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

    /// Waits for the process to exit on its own, without signaling.
    /// Exit is observed by polling (`portable_pty`'s blocking `wait()`
    /// would otherwise park a thread per worker).
    pub async fn wait(&mut self) -> ExitStatus {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) => tokio::time::sleep(EXIT_POLL_INTERVAL).await,
                // `try_wait` failing means the child state is unknowable
                // through this handle (already reaped); report a failure
                // status rather than spin forever.
                Err(_) => return ExitStatus::with_exit_code(1),
            }
        }
    }

    /// Gracefully terminates the process *tree*, escalating
    /// SIGINT -> SIGTERM -> SIGKILL group-wide, mirroring
    /// [`super::process::ManagedProcess::terminate`]'s discipline: a
    /// leader observed already exited is never signaled at all, and each
    /// later signal is guarded by a fresh group liveness probe.
    pub async fn terminate(&mut self) -> TerminationOutcome {
        if let Ok(Some(status)) = self.child.try_wait() {
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
            if leader_outcome.is_none() {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        leader_outcome = Some(TerminationOutcome::Exited {
                            code: Some(status.exit_code() as i32),
                        });
                        if !self.group_is_live() {
                            return leader_outcome;
                        }
                    }
                    Ok(None) => {}
                    // Unknown leader state: keep escalating (never treat
                    // as a confirmed graceful exit).
                    Err(_) => {}
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
    /// dropped handle must never leak a running worker tree.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = kill(Pid::from_raw(-self.pid), Signal::SIGKILL);
        }
    }
}
