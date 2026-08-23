//! Workspace materialization coordination.
//!
//! Coordinates the creation of working directories using different
//! isolation strategies (shared, git worktree, copy).

use crate::workspace::copy::CopyIsolation;
use crate::workspace::git::GitWorktree;
use batman_protocol::{IsolationKind, ProjectId, RunId};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MaterializerError {
    #[error("git error: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("path validation failed: {0}")]
    PathValidation(String),
}

impl From<crate::workspace::copy::CopyError> for MaterializerError {
    fn from(err: crate::workspace::copy::CopyError) -> Self {
        MaterializerError::Git(err.to_string())
    }
}

impl From<crate::workspace::git::GitError> for MaterializerError {
    fn from(err: crate::workspace::git::GitError) -> Self {
        MaterializerError::Git(err.to_string())
    }
}

/// Coordinates workspace materialization for different isolation strategies.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorkspaceMaterializer {
    project_id: ProjectId,
    repository: PathBuf,
    root: PathBuf,
    copy_max_bytes: u64,
    copy_max_files: u64,
}

impl WorkspaceMaterializer {
    pub fn new(project_id: ProjectId, repository: PathBuf) -> Result<Self, MaterializerError> {
        let root = std::env::temp_dir().join(format!("crew-workspace-{}", project_id));
        std::fs::create_dir_all(&root)?;
        Ok(WorkspaceMaterializer {
            project_id,
            repository,
            root,
            copy_max_bytes: crate::workspace::DEFAULT_COPY_MAX_BYTES,
            copy_max_files: crate::workspace::DEFAULT_COPY_MAX_FILES,
        })
    }

    /// Overrides the copy-isolation ceilings with the operator's configured
    /// values. Separate from `new` so every embedding gets a bounded copy by
    /// default -- a materializer built without a policy is still safe.
    #[must_use]
    pub fn with_copy_limits(mut self, max_bytes: u64, max_files: u64) -> Self {
        self.copy_max_bytes = max_bytes;
        self.copy_max_files = max_files;
        self
    }

    /// Validates that a path is within the lease root.
    /// Rejects:
    /// - Absolute paths (which cannot be within a relative root)
    /// - Paths containing `..` components (which escape the root)
    /// - Paths that escape through symlinks
    pub fn validate_path(&self, path: &str) -> Result<(), MaterializerError> {
        let path_obj = Path::new(path);

        // Reject absolute paths
        if path_obj.is_absolute() {
            return Err(MaterializerError::PathValidation(
                "Absolute paths are not allowed".to_string(),
            ));
        }

        // Lexical check: reject any path containing `..` components
        if path_obj
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(MaterializerError::PathValidation(
                "Path contains `..` component".to_string(),
            ));
        }

        // Join with root
        let candidate = self.root.join(path);
        let canonical_root = self.root.canonicalize().unwrap_or(self.root.clone());

        // Resolve the nearest existing ancestor and check it's under the root
        if let Some(canonical) = Self::resolve_nearest_existing(&candidate)
            && !canonical.starts_with(&canonical_root)
        {
            return Err(MaterializerError::PathValidation(
                "Path escapes lease root via symlink".to_string(),
            ));
        }
        // For non-existent paths, lexical validation is sufficient

        Ok(())
    }

    /// Resolves the nearest existing ancestor of a path.
    /// Returns the canonical path of the nearest existing ancestor, or None if no ancestor exists.
    fn resolve_nearest_existing(path: &Path) -> Option<PathBuf> {
        let mut current = path.to_path_buf();

        // Walk up the path until we find an existing ancestor
        loop {
            if current.exists() {
                return current.canonicalize().ok();
            }

            if !current.pop() {
                return None;
            }
        }
    }

    /// Materializes a workspace with the given isolation kind.
    /// - Shared: returns the repository path
    /// - GitWorktree: creates a git worktree at `root/<run_id>`
    /// - Copy: creates a copy at `root/<run_id>` excluding `.git`
    pub fn materialize(
        &self,
        run_id: RunId,
        isolation: IsolationKind,
    ) -> Result<PathBuf, MaterializerError> {
        match isolation {
            IsolationKind::Shared => Ok(self.repository.clone()),
            IsolationKind::GitWorktree => {
                let worktree_path = self.root.join(run_id.to_string());

                // Get base commit from the repository
                let base_commit = self.get_base_commit()?;

                // Create the git worktree using the GitWorktree type
                let git_worktree = GitWorktree {
                    repository: self.repository.clone(),
                    path: worktree_path.clone(),
                    base_commit,
                };

                git_worktree.create(&worktree_path)?;
                Ok(worktree_path)
            }
            IsolationKind::Copy => {
                let copy_path = self.root.join(run_id.to_string());
                let copier = CopyIsolation {
                    source: self.repository.clone(),
                    destination: copy_path.clone(),
                    max_bytes: self.copy_max_bytes,
                    max_files: self.copy_max_files,
                };
                copier.copy()?;
                Ok(copy_path)
            }
        }
    }

    /// Removes a materialized workspace. The inverse of [`Self::materialize`],
    /// called when its lease is released; without it every isolated run
    /// leaks a worktree or a full tree copy permanently.
    ///
    /// `IsolationKind::Shared` is deliberately a no-op: its "workspace" is
    /// the user's repository root, and removing it would delete their work.
    ///
    /// # Errors
    /// Returns [`MaterializerError`] if git refuses to remove the worktree
    /// or the directory cannot be deleted.
    pub fn teardown(&self, path: &Path, isolation: IsolationKind) -> Result<(), MaterializerError> {
        // An `allocating` lease released before phase 2 ran has no path and
        // nothing on disk; that is a clean teardown, not a failure.
        if path.as_os_str().is_empty() {
            return Ok(());
        }
        match isolation {
            IsolationKind::Shared => Ok(()),
            IsolationKind::GitWorktree => {
                let git_worktree = GitWorktree {
                    repository: self.repository.clone(),
                    path: path.to_path_buf(),
                    base_commit: String::new(),
                };
                git_worktree.remove()?;
                Ok(())
            }
            IsolationKind::Copy => {
                if path.exists() {
                    std::fs::remove_dir_all(path)?;
                }
                Ok(())
            }
        }
    }

    /// Gets the base commit (HEAD) from the repository.
    fn get_base_commit(&self) -> Result<String, MaterializerError> {
        let output = Command::new("git")
            .current_dir(&self.repository)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| MaterializerError::Git(format!("Failed to execute git: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(MaterializerError::Git(format!(
                "Failed to get base commit: {}",
                stderr
            )));
        }

        let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(commit)
    }
}
