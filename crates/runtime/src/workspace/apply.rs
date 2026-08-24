//! Workspace apply with artifact store integration.
//!
//! Applies workspace changes by fetching artifacts and applying them
//! using the specified strategy (ApplyPatch or CherryPick).

use crate::workspace::artifact_store::sha256_hex;
use crew_protocol::{ApplyRequest, ApplyResult, ApplyStrategy, Artifact, ArtifactId, ArtifactKind};
use std::process::Command;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("artifact not found: {0}")]
    ArtifactNotFound(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("conflict: expected revision {expected}, got {actual}")]
    StaleRevision { expected: String, actual: String },
}

/// Workspace applier that fetches artifacts from a store and applies them.
pub struct WorkspaceApplier {
    path: std::path::PathBuf,
    store: Option<Arc<crate::workspace::ArtifactStore>>,
    run_id: Option<crew_protocol::RunId>,
}

impl WorkspaceApplier {
    pub fn new(path: std::path::PathBuf) -> Self {
        WorkspaceApplier {
            path,
            store: None,
            run_id: None,
        }
    }

    pub fn from_store(
        path: std::path::PathBuf,
        store: Arc<crate::workspace::ArtifactStore>,
        run_id: crew_protocol::RunId,
    ) -> Self {
        WorkspaceApplier {
            path,
            store: Some(store),
            run_id: Some(run_id),
        }
    }

    /// Applies a workspace change using the specified strategy.
    /// Validates `expected_target_revision` before mutating.
    pub async fn apply(&self, request: &ApplyRequest) -> Result<ApplyResult, ApplyError> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| ApplyError::ArtifactNotFound("no artifact store".to_string()))?;

        // Fetch the artifact content
        let content = store
            .fetch_content(&request.artifact_id)
            .await
            .map_err(|e| ApplyError::ArtifactNotFound(e.to_string()))?;

        // Validate expected_target_revision BEFORE any mutation
        let current_head = self.get_current_head()?;
        if current_head != request.expected_target_revision {
            return Ok(ApplyResult {
                lease_id: request.lease_id.clone(),
                success: false,
                conflict_artifact_id: None,
                target_revision_after: Some(current_head),
                error_code: Some("STALE_REVISION".to_string()),
            });
        }

        match request.strategy {
            ApplyStrategy::ApplyPatch => self.apply_patch(&content, &request.lease_id).await,
            ApplyStrategy::CherryPick => self.cherry_pick(&content, &request.lease_id).await,
        }
    }

    /// Records a conflict report as a `ConflictReport` artifact and builds
    /// the corresponding failed [`ApplyResult`].
    ///
    /// A conflict is a legitimate outcome, not an error: the caller needs
    /// `success: false` plus the evidence, not a generic internal failure.
    /// When no store is configured the same result is returned without an
    /// artifact id — a missing store must not turn a conflict back into an
    /// `Err`.
    async fn conflict_result(&self, lease_id: &str, report: String) -> ApplyResult {
        let conflict_artifact_id = match self.store {
            Some(ref store) => {
                let content = report.into_bytes();
                let artifact = Artifact {
                    artifact_id: ArtifactId::new(),
                    kind: ArtifactKind::ConflictReport,
                    sha256: sha256_hex(&content),
                    byte_length: content.len() as u64,
                    media_type: "text/plain; charset=utf-8".to_string(),
                    storage_path: format!("conflicts/{lease_id}.txt"),
                    run_id: self.run_id.as_ref().map(|r| r.to_string()),
                };
                store.store(artifact, content).await.ok()
            }
            None => None,
        };

        ApplyResult {
            lease_id: lease_id.to_string(),
            success: false,
            conflict_artifact_id,
            target_revision_after: self.get_current_head().ok(),
            error_code: Some("CONFLICT".to_string()),
        }
    }

    /// Gets the current HEAD revision.
    fn get_current_head(&self) -> Result<String, ApplyError> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| ApplyError::Git(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            return Err(ApplyError::Git("Failed to get HEAD".to_string()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Applies a patch file to the workspace.
    ///
    /// `git apply --check` runs first, so a rejected patch never mutates
    /// the tree and no abort is needed on this path.
    async fn apply_patch(
        &self,
        patch_content: &[u8],
        lease_id: &str,
    ) -> Result<ApplyResult, ApplyError> {
        // Write patch to a temporary file
        let patch_path = self.path.join(format!("incoming_{}.patch", lease_id));
        std::fs::write(&patch_path, patch_content)?;

        // Dry-run first: `--check` reports why the patch is rejected
        // without touching the working tree.
        let check = Command::new("git")
            .current_dir(&self.path)
            .args(["apply", "--check", patch_path.to_str().unwrap_or("")])
            .output()
            .map_err(|e| ApplyError::Git(format!("Failed to execute git: {}", e)))?;

        if !check.status.success() {
            let _ = std::fs::remove_file(&patch_path);
            let report = format!(
                "strategy: applyPatch\nlease: {lease_id}\nrejection:\n{}",
                String::from_utf8_lossy(&check.stderr).trim_end()
            );
            return Ok(self.conflict_result(lease_id, report).await);
        }

        // Apply the patch for real
        let applied = Command::new("git")
            .current_dir(&self.path)
            .args(["apply", patch_path.to_str().unwrap_or("")])
            .output()
            .map_err(|e| ApplyError::Git(format!("Failed to execute git: {}", e)))?;

        let _ = std::fs::remove_file(&patch_path);

        if !applied.status.success() {
            let report = format!(
                "strategy: applyPatch\nlease: {lease_id}\nrejection:\n{}",
                String::from_utf8_lossy(&applied.stderr).trim_end()
            );
            return Ok(self.conflict_result(lease_id, report).await);
        }

        // Get the new HEAD
        let target_revision_after = self.get_current_head().ok();

        Ok(ApplyResult {
            lease_id: lease_id.to_string(),
            success: true,
            conflict_artifact_id: None,
            target_revision_after,
            error_code: None,
        })
    }

    /// Cherry-picks commits from the artifact.
    ///
    /// A failed pick is aborted before anything else runs: restoring the
    /// tree matters more than reporting why it broke, and leaving a
    /// half-finished cherry-pick would block every later operation on the
    /// workspace.
    async fn cherry_pick(
        &self,
        commit_content: &[u8],
        lease_id: &str,
    ) -> Result<ApplyResult, ApplyError> {
        // The artifact should contain commit IDs to cherry-pick
        let content_str = String::from_utf8_lossy(commit_content);
        let commit_ids: Vec<&str> = content_str.lines().filter(|l| !l.is_empty()).collect();

        if commit_ids.is_empty() {
            return Err(ApplyError::Git("No commit IDs in artifact".to_string()));
        }

        // Cherry-pick each commit
        for commit_id in &commit_ids {
            let status = Command::new("git")
                .current_dir(&self.path)
                .args(["cherry-pick", commit_id])
                .status()
                .map_err(|e| ApplyError::Git(format!("Failed to execute git: {}", e)))?;

            if !status.success() {
                // Collect the conflicting paths *before* aborting -- the
                // abort is what erases them.
                let conflict_files: Vec<String> = Command::new("git")
                    .current_dir(&self.path)
                    .args(["diff", "--name-only"])
                    .output()
                    .map(|output| {
                        String::from_utf8_lossy(&output.stdout)
                            .lines()
                            .filter(|l| !l.is_empty())
                            .map(|l| l.to_string())
                            .collect()
                    })
                    .unwrap_or_else(|_| vec!["unknown conflict".to_string()]);

                // Ignore an abort failure: there is nothing better to do
                // if the restore itself breaks, and the conflict result is
                // still the honest answer.
                let _ = Command::new("git")
                    .current_dir(&self.path)
                    .args(["cherry-pick", "--abort"])
                    .status();

                let report = format!(
                    "strategy: cherryPick\nlease: {lease_id}\nfailed commit: {commit_id}\nconflicting files:\n{}",
                    conflict_files.join("\n")
                );
                return Ok(self.conflict_result(lease_id, report).await);
            }
        }

        // Get the new HEAD
        let target_revision_after = self.get_current_head().ok();

        Ok(ApplyResult {
            lease_id: lease_id.to_string(),
            success: true,
            conflict_artifact_id: None,
            target_revision_after,
            error_code: None,
        })
    }
}
