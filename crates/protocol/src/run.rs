//! Run lifecycle states, flags, and run specification.
//!
//! `RunState` defines the complete legal lifecycle relation. `RunFlags`
//! stores independent boolean flags. `RunSpec` is the immutable request
use crate::event::RunFlags;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::Timestamp;
use crate::ids::{RunId, TaskId, WorkerId};

// ---------------------------------------------------------------------------
// RunState
// ---------------------------------------------------------------------------

/// The lifecycle state of a run.
///
/// Only the runtime applies a transition after process/protocol evidence.
/// Terminal states (`succeeded`, `failed`, `cancelled`, `lost`) have no
/// outgoing edges.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub struct RunState(String);

impl RunState {
    /// Returns `true` if this state is terminal (no outgoing edges).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.0.as_str(),
            "succeeded" | "failed" | "cancelled" | "lost"
        )
    }

    /// Returns whether a transition from `self` to `target` is legal
    /// according to the authoritative lifecycle table.
    #[must_use]
    pub fn can_transition_to(&self, target: &RunState) -> bool {
        let from = self.0.as_str();
        let to = target.0.as_str();

        match from {
            "queued" => matches!(to, "starting" | "failed" | "cancelled"),
            "starting" => matches!(to, "working" | "failed" | "cancelled" | "lost"),
            "working" => matches!(
                to,
                "waitingUser"
                    | "waitingPeer"
                    | "paused"
                    | "succeeded"
                    | "failed"
                    | "cancelled"
                    | "lost"
            ),
            "waitingUser" => {
                matches!(to, "working" | "paused" | "failed" | "cancelled" | "lost")
            }
            "waitingPeer" => {
                matches!(to, "working" | "paused" | "failed" | "cancelled" | "lost")
            }
            "paused" => matches!(to, "working" | "failed" | "cancelled" | "lost"),
            // Terminal states: no outgoing edges.
            "succeeded" | "failed" | "cancelled" | "lost" => false,
            _ => false,
        }
    }
}

impl TryFrom<&str> for RunState {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "queued" | "starting" | "working" | "waitingUser" | "waitingPeer" | "paused"
            | "succeeded" | "failed" | "cancelled" | "lost" => Ok(RunState(value.to_string())),
            _ => Err(format!("unknown run state: {value}")),
        }
    }
}

impl std::str::FromStr for RunState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value)
    }
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// RunSpec
// ---------------------------------------------------------------------------

/// Immutable request to execute a task with a worker.
///
/// Later adapter configuration resolves the complete profile snapshot
/// without changing these identity fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RunSpec {
    /// The task to execute.
    pub task_id: TaskId,
    /// The worker to execute with.
    pub worker_id: WorkerId,
    /// Optional workspace mode (`shared` or `isolated`).
    #[serde(rename = "workspaceMode", skip_serializing_if = "Option::is_none")]
    pub workspace_mode: Option<String>,
    /// Priority (higher = scheduled first).
    pub priority: i32,
    /// The task content OMP supplies for this run. Crew never authors it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// A single attempt to execute one task with one worker.
///
/// A retry creates a new `RunId`, not a new task. Only runtime
/// process/protocol evidence changes run lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct Run {
    /// The run identifier (UUIDv7).
    #[serde(rename = "runId")]
    pub run_id: RunId,
    /// The task this run executes.
    #[serde(rename = "taskId")]
    pub task_id: TaskId,
    /// The worker this run uses.
    #[serde(rename = "workerId")]
    pub worker_id: WorkerId,
    /// Current lifecycle state.
    pub state: RunState,
    /// Independent boolean flags.
    pub flags: RunFlags,
    /// Vendor session ID, if a vendor session has been established.
    #[serde(rename = "vendorSessionId", skip_serializing_if = "Option::is_none")]
    pub vendor_session_id: Option<String>,
    /// When the run entered `starting` state (UTC RFC 3339).
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    /// When the run reached a terminal state (UTC RFC 3339).
    #[serde(rename = "completedAt", skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<Timestamp>,
}

// ---------------------------------------------------------------------------
// RunResultResult
// ---------------------------------------------------------------------------

/// Token usage folded from a run's journaled `adapterUsageEvent`s.
///
/// The runtime applies the adapter-correct fold before this leaves the
/// daemon: Claude journals per-invocation deltas (summed); every other
/// reporting adapter journals cumulative totals (last one wins). Codex
/// never reports cost, so `cost_usd` is `null` there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RunUsage {
    #[serde(rename = "inputTokens")]
    #[ts(type = "number")]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens")]
    #[ts(type = "number")]
    pub output_tokens: u64,
    #[serde(rename = "costUsd")]
    pub cost_usd: Option<f64>,
}

/// Result of `run/result`: a terminal run's final journaled output.
///
/// `resultText` is `null` when the run journaled no visible final message
/// (or it was fully redacted) -- distinct from an error. `usage` is
/// `null` when the adapter never reported usage (e.g. Copilot under
/// ACP v1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RunResultResult {
    #[serde(rename = "runId")]
    pub run_id: RunId,
    pub state: RunState,
    #[serde(rename = "resultText")]
    pub result_text: Option<String>,
    pub usage: Option<RunUsage>,
    #[serde(rename = "completedAt")]
    pub completed_at: Option<String>,
}
