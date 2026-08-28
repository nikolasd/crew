//! The worker process supervisor: process-group scoped spawn, bounded
//! stdio, environment policy, and cancellation escalation
//! (SIGINT -> SIGTERM -> SIGKILL). Every adapter launches its supervised
//! vendor process through this module rather than calling
//! `tokio::process::Command` directly, so every worker gets the same
//! process-group, bounding, and escalation guarantees regardless of which
//! adapter owns it.

mod environment;
mod output;
mod process;
mod pty;

pub use environment::{EnvironmentPolicy, REDACTED_PLACEHOLDER, redacted_env_snapshot};
pub use output::{MAX_STDERR_CAPTURE_BYTES, MAX_STDOUT_FRAME_BYTES, RotatingCapture};
pub use process::{
    EscalationTimings, ManagedProcess, SpawnSpec, Supervisor, SupervisorError, TerminationOutcome,
};
pub use pty::PtyProcess;
