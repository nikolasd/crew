//! Process-group scoped supervision: every spawned worker becomes the
//! leader of its own process group, so cancellation reaches every
//! grandchild it forks, and graceful termination escalates
//! SIGINT -> SIGTERM -> SIGKILL rather than assuming any single signal is
//! honored.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use super::output::{self, RotatingCapture};

/// Errors spawning a supervised process.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("failed to spawn {program:?}: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("spawned process has no observable pid")]
    MissingPid,
    #[error("pty error: {message}")]
    Pty { message: String },
}

/// A fully specified request to spawn one supervised process.
///
/// `env` is the exact, already-allowlisted environment the child
/// receives -- the supervisor never inherits the runtime's own process
/// environment implicitly; use `crate::supervisor::EnvironmentPolicy` to
/// build `env` from a validated allowlist before constructing this.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub max_stdout_frame_bytes: usize,
    pub max_stderr_capture_bytes: usize,
}

impl SpawnSpec {
    /// A spec with every bound at its supervisor default and an empty
    /// program/args/env, for tests to override with `..SpawnSpec::minimal()`.
    #[must_use]
    pub fn minimal() -> Self {
        Self {
            program: PathBuf::new(),
            args: Vec::new(),
            cwd: std::env::temp_dir(),
            env: HashMap::new(),
            max_stdout_frame_bytes: output::MAX_STDOUT_FRAME_BYTES,
            max_stderr_capture_bytes: output::MAX_STDERR_CAPTURE_BYTES,
        }
    }
}

/// How long to wait after each escalation step before sending the next
/// signal. Production defaults to 5 seconds each (SIGINT -> wait -> SIGTERM
/// -> wait -> SIGKILL); tests inject much shorter waits.
#[derive(Debug, Clone, Copy)]
pub struct EscalationTimings {
    pub sigint_to_sigterm: Duration,
    pub sigterm_to_sigkill: Duration,
}

impl Default for EscalationTimings {
    fn default() -> Self {
        Self {
            sigint_to_sigterm: Duration::from_secs(5),
            sigterm_to_sigkill: Duration::from_secs(5),
        }
    }
}

/// How long a process whose protocol stream has already ended gets to
/// exit on its own before [`ManagedProcess::settle`] escalates. A
/// completed vendor run is already exiting when its stdout closes, so
/// this window is almost always uncontended; it exists so a wedged
/// process cannot hold a concurrency slot open indefinitely.
const SETTLE_GRACE: Duration = Duration::from_secs(1);

/// The result of [`ManagedProcess::terminate`].
#[derive(Debug, Clone, Copy)]
pub enum TerminationOutcome {
    /// The process exited on its own (cooperatively, after SIGINT or
    /// SIGTERM), with the given exit code if available.
    Exited { code: Option<i32> },
    /// The process had to be escalated all the way to SIGKILL.
    Killed,
}

impl TerminationOutcome {
    /// The `(exit_code, signal)` pair
    /// `AdapterEventPayload::ProcessExited` carries for this outcome.
    #[must_use]
    pub fn exit_signals(self) -> (Option<i32>, Option<String>) {
        match self {
            Self::Exited { code } => (code, None),
            Self::Killed => (None, Some("SIGKILL".to_string())),
        }
    }
}

/// Spawns supervised processes, each in its own process group.
pub struct Supervisor {
    escalation: EscalationTimings,
}

impl Supervisor {
    #[must_use]
    pub fn new() -> Self {
        Self {
            escalation: EscalationTimings::default(),
        }
    }

    #[must_use]
    pub fn with_escalation(escalation: EscalationTimings) -> Self {
        Self { escalation }
    }

    /// Spawns `spec` as the leader of a fresh process group, with piped
    /// stdin/stdout/stderr and a bounded reader/capture already attached
    /// to stdout/stderr.
    ///
    /// # Errors
    /// Returns [`SupervisorError`] if the process cannot be spawned or its
    /// pid cannot be observed.
    pub async fn spawn(&self, spec: SpawnSpec) -> Result<ManagedProcess, SupervisorError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A fresh process group led by this child (pgid == child pid), so
        // every grandchild it forks inherits the same group and a single
        // `kill(-pgid, ..)` reaches all of them.
        cmd.as_std_mut().process_group(0);

        let mut child = cmd.spawn().map_err(|source| SupervisorError::Spawn {
            program: spec.program.clone(),
            source,
        })?;
        let pid = child.id().ok_or(SupervisorError::MissingPid)? as i32;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is always piped");
        let stderr = child.stderr.take().expect("stderr is always piped");

        // `SpawnSpec`'s bounds may tighten but never loosen the
        // supervisor's own absolute ceilings.
        let max_stdout_frame_bytes = spec
            .max_stdout_frame_bytes
            .min(output::MAX_STDOUT_FRAME_BYTES);
        let max_stderr_capture_bytes = spec
            .max_stderr_capture_bytes
            .min(output::MAX_STDERR_CAPTURE_BYTES);
        let stdout_rx = output::spawn_stdout_reader(stdout, max_stdout_frame_bytes);
        let stderr_capture = output::spawn_stderr_capture(stderr, max_stderr_capture_bytes);

        Ok(ManagedProcess {
            child,
            pid,
            stdin,
            stdout_rx,
            stderr_capture,
            escalation: self.escalation,
        })
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// A supervised child process: its process group, bounded stdout frames,
/// a rotating stderr capture, and cancellation escalation.
pub struct ManagedProcess {
    child: Child,
    pid: i32,
    stdin: Option<ChildStdin>,
    stdout_rx: mpsc::Receiver<Vec<u8>>,
    stderr_capture: Arc<Mutex<RotatingCapture>>,
    escalation: EscalationTimings,
}

impl ManagedProcess {
    /// This process's own pid (the process group leader).
    #[must_use]
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Writes `bytes` to the process's stdin, if it is still open.
    ///
    /// # Errors
    /// Returns an I/O error if the write fails, or a "stdin closed" error
    /// if stdin was already taken or the pipe closed.
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("stdin is closed"));
        };
        stdin.write_all(bytes).await?;
        stdin.flush().await
    }

    /// Closes the process's stdin (e.g. to signal EOF to a cooperative
    /// reader), if not already closed.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Receives the next bounded stdout frame (line), or `None` once the
    /// process's stdout has closed or a bound was exceeded.
    pub async fn next_stdout_frame(&mut self) -> Option<Vec<u8>> {
        self.stdout_rx.recv().await
    }

    /// A snapshot of the current rotating stderr capture. Never exceeds
    /// the configured cap.
    #[must_use]
    pub fn stderr_snapshot(&self) -> Vec<u8> {
        self.stderr_capture
            .lock()
            .expect("stderr capture mutex is never poisoned")
            .snapshot()
    }

    /// Waits for the process to exit on its own, without sending any
    /// signal.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Settles a process whose protocol stream has ended: waits out
    /// [`SETTLE_GRACE`] for a self-exit -- the ordinary end of a completed
    /// run -- and escalates through [`Self::terminate`] only if it is still
    /// alive, so a normal completion never pays the escalation window while a
    /// wedged process (or a live process-group member the leader left behind)
    /// is still reached. Always returns an outcome; never blocks
    /// indefinitely.
    pub async fn settle(&mut self) -> TerminationOutcome {
        match tokio::time::timeout(SETTLE_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => TerminationOutcome::Exited {
                code: status.code(),
            },
            // Timed out (still running) or `wait()` itself failed
            // (outcome unknown) -- both must escalate rather than be
            // reported as a confirmed graceful exit.
            Ok(Err(_)) | Err(_) => self.terminate().await,
        }
    }

    /// Sends `signal` to the whole process group (not just the direct
    /// child), so a supervised worker's own children are reached too.
    pub fn signal_group(&self, signal: Signal) -> nix::Result<()> {
        kill(Pid::from_raw(-self.pid), signal)
    }

    /// Gracefully terminates the process *tree*, escalating
    /// SIGINT -> SIGTERM -> SIGKILL. Always returns only after the
    /// directly-owned leader process has actually exited (and been
    /// reaped). [`TerminationOutcome::Killed`] means escalation reached
    /// SIGKILL; [`TerminationOutcome::Exited`] means the leader died
    /// promptly from SIGINT or SIGTERM.
    ///
    /// Before each signal after the first, this re-confirms the process
    /// group still has a live member with a fresh existence probe
    /// (`kill(-pgid, 0)`, no actual signal sent) rather than trusting an
    /// earlier observation -- narrowing, though never fully closing, the
    /// window in which the kernel could have recycled this pgid for an
    /// unrelated process after this handle's own `wait()` reaped its
    /// leader. A leader observed *already exited* before this call ever
    /// signals anything is never signaled at all: that gap could have
    /// been arbitrarily long, so this handle can no longer be confident
    /// the pgid is still its own.
    ///
    /// This does still chase down a live process-group member even
    /// after the leader itself has exited, because POSIX requires a
    /// non-interactive shell to set SIGINT (and SIGQUIT) ignored in a
    /// backgrounded (`cmd &`) child before it execs -- a worker that is
    /// (or forks) such a shell can leave a live, SIGINT-immune
    /// grandchild behind the instant its own leader dies from step one;
    /// the guaranteed-unblockable SIGKILL step is what reaches it.
    pub async fn terminate(&mut self) -> TerminationOutcome {
        if let Ok(Some(status)) = self.child.try_wait() {
            return TerminationOutcome::Exited {
                code: status.code(),
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

        // The SIGTERM step above may have been skipped entirely if the
        // group was already found empty by its guard (`leader_outcome`
        // stays `None` in that case, since `wait_out_step` was never
        // called a second time) -- re-check freshly rather than assume
        // "still None" means "still alive": that guard is exactly what
        // must decide whether SIGKILL is safe to send at all.
        if self.group_is_live() {
            let _ = self.signal_group(Signal::SIGKILL);
            let _ = self.child.wait().await;
            return TerminationOutcome::Killed;
        }
        match self.child.wait().await {
            Ok(status) => TerminationOutcome::Exited {
                code: status.code(),
            },
            // The group was already confirmed empty, so no further
            // signal is warranted, but this handle's own leader `wait()`
            // itself failed -- report `Exited` with an unknown code
            // rather than fabricate a specific one; this is a distinct,
            // rarer situation from the confirmed-exit path above.
            Err(_) => TerminationOutcome::Exited { code: None },
        }
    }

    /// A fresh existence probe for the process group: sends signal 0 (no
    /// actual signal is delivered), which succeeds if at least one
    /// process still has this pgid. Used to avoid signaling a pgid this
    /// handle has not just confirmed still refers to a live member of
    /// its own worker's tree.
    fn group_is_live(&self) -> bool {
        kill(Pid::from_raw(-self.pid), None).is_ok()
    }

    /// Waits out the *entire* `duration` window for the whole process
    /// group, not merely until this handle's own leader exits: if the
    /// leader exits with time remaining but a fresh liveness probe still
    /// finds a live descendant (the exact shape of an orphaned,
    /// SIGINT/SIGTERM-ignoring shell background job -- POSIX requires
    /// those be created with such signals ignored), the rest of this
    /// step's configured grace period is honored before the caller may
    /// escalate, rather than moving to the next signal the instant the
    /// leader alone is gone.
    ///
    /// Returns `Some(outcome)` only once the *whole group* is confirmed
    /// empty and the leader's own `wait()` genuinely succeeded (a
    /// `wait()` error is never treated as a confirmed graceful exit --
    /// it means "unknown", so the caller must keep escalating); `None`
    /// means either the deadline was reached with something still
    /// alive, or the leader's own wait outcome is unknown.
    async fn wait_out_step(&mut self, duration: Duration) -> Option<TerminationOutcome> {
        let started = tokio::time::Instant::now();
        let leader_outcome = match tokio::time::timeout(duration, self.child.wait()).await {
            Ok(Ok(status)) => Some(TerminationOutcome::Exited {
                code: status.code(),
            }),
            // A timeout means the leader is still running; a wait()
            // error means its state is unknown -- neither is a
            // confirmed exit, so both fall through to escalation.
            Ok(Err(_)) | Err(_) => None,
        };

        if leader_outcome.is_some() {
            if !self.group_is_live() {
                return leader_outcome;
            }
            if let Some(remaining) = duration.checked_sub(started.elapsed()) {
                tokio::time::sleep(remaining).await;
            }
        }

        if self.group_is_live() {
            None
        } else {
            leader_outcome
        }
    }
}
