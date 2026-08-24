//! Workspace inspection implementation.
//!
//! Captures real evidence from a workspace: dirty files, untracked files,
//! commit history, and generates a patch artifact stored in the ArtifactStore.

use crate::workspace::artifact_store::sha256_hex;
use crew_protocol::{Artifact, ArtifactId, ArtifactKind, InspectRequest, InspectResult};
use std::process::Command;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InspectError {
    #[error("git error: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub struct WorkspaceInspector {
    path: std::path::PathBuf,
    store: Option<Arc<crate::workspace::ArtifactStore>>,
    run_id: Option<crew_protocol::RunId>,
}

impl WorkspaceInspector {
    pub fn new(path: std::path::PathBuf) -> Self {
        WorkspaceInspector {
            path,
            store: None,
            run_id: None,
        }
    }

    pub fn with_store(
        path: std::path::PathBuf,
        store: Arc<crate::workspace::ArtifactStore>,
        run_id: crew_protocol::RunId,
    ) -> Self {
        WorkspaceInspector {
            path,
            store: Some(store),
            run_id: Some(run_id),
        }
    }

    /// Inspects the workspace and captures real evidence.
    /// Persists the patch to the ArtifactStore if one is configured.
    pub async fn inspect(&self, request: &InspectRequest) -> Result<InspectResult, InspectError> {
        // Get base revision (HEAD)
        let base_revision = self.get_rev_parse("HEAD")?;

        // Get current revision (working tree HEAD)
        let current_revision = self.get_rev_parse("HEAD").ok();

        // Get dirty file count (modified but not staged)
        let dirty_file_count = self.get_dirty_file_count()?;

        // Get untracked file count
        let untracked_file_count = self.get_untracked_file_count()?;

        // Get commit IDs (recent commits)
        let commit_ids = self.get_commit_ids()?;

        // Generate a patch by running `git diff`
        let patch_content = self.generate_patch()?;

        // Store the patch in the ArtifactStore if configured
        let patch_artifact_id = if let Some(ref store) = self.store {
            let artifact = Artifact {
                artifact_id: ArtifactId::new(),
                kind: ArtifactKind::Patch,
                sha256: sha256_hex(&patch_content),
                byte_length: patch_content.len() as u64,
                media_type: "application/x-git-diff".to_string(),
                storage_path: format!("patches/{}.patch", patch_content.len()),
                run_id: self.run_id.as_ref().map(|r| r.to_string()),
            };
            store
                .store(artifact, patch_content)
                .await
                .map_err(|e| InspectError::Git(format!("Failed to store patch: {}", e)))?
        } else {
            ArtifactId::new()
        };

        Ok(InspectResult {
            lease_id: request.lease_id.clone(),
            patch_artifact_id,
            commit_count: commit_ids.len() as u64,
            commit_ids,
            dirty_file_count,
            untracked_file_count,
            base_revision,
            current_revision,
        })
    }

    /// Gets the HEAD revision using `git rev-parse HEAD`.
    fn get_rev_parse(&self, refspec: &str) -> Result<String, InspectError> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["rev-parse", refspec])
            .output()
            .map_err(|e| InspectError::Git(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(InspectError::Git(format!(
                "git rev-parse {} failed: {}",
                refspec, stderr
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Gets the count of dirty files (modified but not staged).
    fn get_dirty_file_count(&self) -> Result<u64, InspectError> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["diff", "--name-only", "--diff-filter=M"])
            .output()
            .map_err(|e| InspectError::Git(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            return Ok(0);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.lines().filter(|l| !l.is_empty()).count() as u64)
    }

    /// Gets the count of untracked files.
    fn get_untracked_file_count(&self) -> Result<u64, InspectError> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["status", "--porcelain"])
            .output()
            .map_err(|e| InspectError::Git(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            return Ok(0);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.lines().filter(|l| l.starts_with("??")).count() as u64)
    }

    /// Gets recent commit IDs.
    fn get_commit_ids(&self) -> Result<Vec<String>, InspectError> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["log", "--oneline", "--max-count=10"])
            .output()
            .map_err(|e| InspectError::Git(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            return Ok(vec![]);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout
            .lines()
            .filter_map(|l| {
                let parts: Vec<&str> = l.splitn(2, ' ').collect();
                parts.first().map(|s| s.to_string())
            })
            .collect())
    }

    /// Generates a patch by running `git diff`.
    fn generate_patch(&self) -> Result<Vec<u8>, InspectError> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["diff"])
            .output()
            .map_err(|e| InspectError::Git(format!("Failed to execute git: {}", e)))?;

        Ok(output.stdout)
    }
}
