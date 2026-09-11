//! The worker process supervisor: process-group scoped spawn, bounded
//! stdio, environment policy, and cancellation escalation
//! (SIGINT -> SIGTERM -> SIGKILL). Every adapter launches its supervised
//! vendor process through this module rather than calling
//! `tokio::process::Command` directly, so every worker gets the same
//! process-group, bounding, and escalation guarantees regardless of which
//! adapter owns it.
//!
//! **One documented exception**: `crate::adapter::claude_protocol` spawns
//! via a bare `tokio::process::Command`, not `Supervisor::spawn` -- its
//! own `settle_after_turn` reuses this module's [`EscalationTimings`]/
//! [`TerminationOutcome`] to get the same escalation discipline, but
//! without a process group of its own (a real gap, not a design choice:
//! `claude_protocol::reader::drive_turn` reads/writes over generic
//! `AsyncRead`/`AsyncWrite`, which [`ManagedProcess`]'s framed-stdout/
//! queued-stdin API does not satisfy without a larger rewrite than that
//! adapter's own spike scope covers). Carried forward, not silently
//! divergent: see that adapter's own `settle_after_turn` doc comment.

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
pub(crate) use pty::{DEFAULT_COLS, DEFAULT_ROWS};
