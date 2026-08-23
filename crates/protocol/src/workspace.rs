//! Workspace lease contracts for concurrent worker isolation.
//!
//! Types for lease acquisition, inspection, and application, plus the
//! workspace lifecycle events that drive the durable journal and display
//! subscriptions.

use crate::ids::{ArtifactId, ProjectId, RunId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The isolation mode requested for a workspace lease.
///
/// `shared` allows multiple read-only workers to share one path;
/// `write` requires exclusive isolation (git-worktree or copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum LeaseMode {
    ReadOnly,
    Write,
}

/// The isolation strategy used to materialize a workspace.
///
/// `shared` means no isolation (single path shared by read-only workers);
/// `gitWorktree` uses `git worktree add --detach`; `copy` copies the tree
/// without following symlinks or copying `.git` administrative data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum IsolationKind {
    Shared,
    GitWorktree,
    Copy,
}

/// The lifecycle state of a workspace lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum WorkspaceState {
    Allocating,
    Active,
    Dirty,
    Released,
    CleanupFailed,
}

/// A workspace lease record stored in the durable journal and projection table.
///
/// Every lease carries a canonical path, the repository root it belongs to,
/// the base revision (HEAD at acquisition time), the owning run ID, and
/// a monotonically increasing acquisition sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct WorkspaceLease {
    pub lease_id: String,
    pub project_id: ProjectId,
    pub run_id: RunId,
    pub mode: LeaseMode,
    pub isolation_kind: IsolationKind,
    pub path: String,
    pub canonical_repository_root: String,
    pub base_revision: String,
    pub state: WorkspaceState,
    pub acquired_at: String,
    pub released_at: Option<String>,
    /// Monotonically increasing sequence number assigned at acquisition.
    #[ts(type = "number")]
    pub acquisition_sequence: u64,
}

/// Parameters for acquiring a workspace lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct LeaseRequest {
    pub run_id: RunId,
    pub mode: LeaseMode,
    pub requested_isolation: Option<IsolationKind>,
}

/// Information about an active or recently-released lease, returned by `get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct WorkspaceInfo {
    pub lease_id: String,
    pub run_id: RunId,
    pub mode: LeaseMode,
    pub isolation_kind: IsolationKind,
    pub path: String,
    pub state: WorkspaceState,
    pub base_revision: String,
}

/// Parameters for inspecting a workspace's current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct InspectRequest {
    pub lease_id: String,
}

/// Evidence captured by `inspect`: a binary-safe patch, commit list, and
/// dirty/untracked state summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct InspectResult {
    pub lease_id: String,
    pub patch_artifact_id: ArtifactId,
    #[ts(type = "number")]
    pub commit_count: u64,
    pub commit_ids: Vec<String>,
    #[ts(type = "number")]
    pub dirty_file_count: u64,
    #[ts(type = "number")]
    pub untracked_file_count: u64,
    pub base_revision: String,
    pub current_revision: Option<String>,
}

/// Parameters for applying a workspace change.
///
/// This is a mechanical operation only: OMP must explicitly select the
/// artifact, the strategy, and the expected target revision. Crew
/// never auto-selects a patch or resolves conflicts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ApplyRequest {
    pub lease_id: String,
    pub strategy: ApplyStrategy,
    pub artifact_id: ArtifactId,
    pub expected_target_revision: String,
    pub approval_correlation_id: Option<String>,
}

/// The mechanical strategy for applying a workspace change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ApplyStrategy {
    ApplyPatch,
    CherryPick,
}

/// Result of applying a workspace change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ApplyResult {
    pub lease_id: String,
    pub success: bool,
    pub conflict_artifact_id: Option<ArtifactId>,
    pub target_revision_after: Option<String>,
    pub error_code: Option<String>,
}

/// Parameters for releasing a workspace lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ReleaseRequest {
    pub lease_id: String,
}

/// Parameters for listing active leases for a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[expect(dead_code)]
pub struct LeaseListRequest {
    pub project_id: ProjectId,
}

/// A workspace lease lifecycle event produced by `acquire`/`release`/`inspect`/`apply`.
///
/// Serialized as an adjacently tagged enum: `{ "type": "...", "payload": { ... } }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[ts(export)]
pub enum WorkspaceEvent {
    LeaseRequested {
        lease_id: String,
        run_id: RunId,
        mode: LeaseMode,
    },
    LeaseAcquired {
        lease_id: String,
        run_id: RunId,
        path: String,
        isolation_kind: IsolationKind,
        base_revision: String,
    },
    WorkspaceDirty {
        lease_id: String,
        #[ts(type = "number")]
        dirty_file_count: u64,
        #[ts(type = "number")]
        untracked_file_count: u64,
    },
    WorkspaceInspected {
        lease_id: String,
        patch_artifact_id: ArtifactId,
        #[ts(type = "number")]
        commit_count: u64,
        #[ts(type = "number")]
        dirty_file_count: u64,
        #[ts(type = "number")]
        untracked_file_count: u64,
    },
    ApplyStarted {
        lease_id: String,
        strategy: ApplyStrategy,
        artifact_id: ArtifactId,
        expected_target_revision: String,
    },
    ApplyCompleted {
        lease_id: String,
        success: bool,
        conflict_artifact_id: Option<ArtifactId>,
        target_revision_after: Option<String>,
    },
    LeaseReleased {
        lease_id: String,
        run_id: RunId,
    },
    CleanupFailed {
        lease_id: String,
        error: String,
    },
    /// An artifact was published for a workspace (inspect or apply produced one).
    ArtifactPublished {
        lease_id: String,
        artifact_id: ArtifactId,
        kind: String,
    },
    /// An apply attempt produced a conflict that OMP must resolve.
    ApplyConflict {
        lease_id: String,
        conflict_artifact_id: ArtifactId,
        strategy: ApplyStrategy,
        expected_target_revision: String,
    },
}
