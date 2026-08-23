//! Derives the on-disk paths Crew's runtime uses for a given repository,
//! namespaced under a [`crate::security::StateRoot`].
//!
//! Given a repository directory, [`RuntimePaths::resolve`] canonicalizes it,
//! walks up to find its VCS root (so any subdirectory of a checkout, or a
//! git worktree, maps to the same repository), and derives a stable
//! `repository-id` and [`ProjectId`] from a SHA-256 hash of that canonical
//! root -- deterministic across restarts, unlike [`ProjectId::new`]'s
//! random UUIDv7.

use std::fs;
use std::path::{Path, PathBuf};

use batman_protocol::ProjectId;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::security::{SecurityError, ensure_private_dir};

/// Errors resolving a repository's [`RuntimePaths`].
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// The supplied repository path does not exist or cannot be resolved
    /// (e.g. a dangling symlink, or a permissions error walking to it).
    #[error("failed to canonicalize repository path {path:?}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The canonical path is not valid UTF-8. Crew hashes and stores
    /// paths as UTF-8 text, so a non-UTF-8 repository path cannot be
    /// represented. On macOS this is effectively unreachable (APFS/HFS+
    /// require valid UTF-8 file names); it is reachable on Linux, where
    /// file names are arbitrary bytes.
    #[error("path is not valid UTF-8: {path:?}")]
    NonUtf8 { path: PathBuf },

    /// Defense in depth: the discovered VCS root was not an ancestor of (or
    /// equal to) the canonical repository path. VCS-root discovery only
    /// walks up the canonical path's own ancestors and never follows a
    /// `.git` file's contents, so this should be unreachable in practice.
    #[error("resolved VCS root {root:?} is not an ancestor of repository {repository:?}")]
    Escape { root: PathBuf, repository: PathBuf },

    /// The per-repository root or artifacts directory could not be created
    /// privately. See [`SecurityError`].
    #[error(transparent)]
    Security(#[from] SecurityError),
}

/// The filesystem paths Crew's runtime uses for a single repository,
/// rooted at `<state_root>/repos/<repository-id>/`.
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    /// Stable identifier derived from the repository's canonical VCS root.
    /// Deterministic across restarts -- not a fresh random UUID -- so the
    /// same repository always maps to the same id.
    pub project_id: ProjectId,
    /// `<state_root>/repos/<repository-id>/`, created mode `0700`.
    pub root: PathBuf,
    /// `<root>/runtime.sock`, the runtime's Unix domain socket.
    pub socket: PathBuf,
    /// `<root>/runtime.lock`, the single-instance lock file.
    pub lock: PathBuf,
    /// `<root>/runtime.db`, the SQLite state database.
    pub database: PathBuf,
    /// `<root>/runtime.log`, the runtime's log file.
    pub log: PathBuf,
    /// `<root>/artifacts/`, created mode `0700`.
    pub artifacts: PathBuf,
}

impl RuntimePaths {
    /// Resolves and creates the runtime paths for `repository` under
    /// `state_root`.
    ///
    /// Canonicalizes `repository`, discovers its VCS root by walking up for
    /// a `.git` entry (directory or file, to also recognize worktrees),
    /// and falls back to the canonical repository directory itself if none
    /// is found. The `repository-id` is the lowercase hex SHA-256 of the
    /// canonical UTF-8 root, truncated to 32 hex characters; `project_id`
    /// is a [`ProjectId`] built from the first 16 bytes of that same
    /// digest formatted as a UUID (not [`ProjectId::new`]'s random
    /// UUIDv7), so both are stable across restarts.
    ///
    /// # Errors
    /// Returns [`PathError`] if the repository path cannot be canonicalized
    /// or represented as UTF-8, if VCS-root discovery would escape the
    /// supplied repository, or if the per-repository directories cannot be
    /// created privately.
    pub fn resolve(state_root: &Path, repository: &Path) -> Result<Self, PathError> {
        let canonical = fs::canonicalize(repository).map_err(|source| PathError::Canonicalize {
            path: repository.to_path_buf(),
            source,
        })?;

        let vcs_root = discover_vcs_root(&canonical).unwrap_or_else(|| canonical.clone());
        if !canonical.starts_with(&vcs_root) {
            return Err(PathError::Escape {
                root: vcs_root,
                repository: canonical,
            });
        }

        let vcs_root_str = vcs_root.to_str().ok_or_else(|| PathError::NonUtf8 {
            path: vcs_root.clone(),
        })?;

        let digest = repository_digest(vcs_root_str);
        let repository_id = hex::encode(&digest[..16]);

        let mut project_uuid_bytes = [0u8; 16];
        project_uuid_bytes.copy_from_slice(&digest[..16]);
        let project_id = ProjectId::parse(&Uuid::from_bytes(project_uuid_bytes).to_string())
            .expect("Uuid::to_string always produces a string ProjectId::parse accepts");

        let root = state_root.join("repos").join(&repository_id);
        let artifacts = root.join("artifacts");

        ensure_private_dir(&root)?;
        ensure_private_dir(&artifacts)?;

        Ok(Self {
            project_id,
            socket: root.join("runtime.sock"),
            lock: root.join("runtime.lock"),
            database: root.join("runtime.db"),
            log: root.join("runtime.log"),
            artifacts,
            root,
        })
    }
}

/// The raw SHA-256 digest of a canonical VCS root's UTF-8 bytes. The
/// `repository-id` is the lowercase hex of its first 16 bytes (32 hex
/// characters), and the [`ProjectId`] is those same 16 bytes as a UUID. This
/// hashing is deliberately a pure function of the path *string* -- no
/// filesystem access -- so it can be cross-checked against the shared
/// `fixtures/repo-id/repo-id-cases.json` fixture in both Rust and TypeScript.
fn repository_digest(canonical_root: &str) -> [u8; 32] {
    Sha256::digest(canonical_root.as_bytes()).into()
}

/// The stable `repository-id` for an already-canonical VCS root: the lowercase
/// hex of the first 16 bytes of the SHA-256 of its UTF-8 bytes (32 hex
/// characters). Pure -- takes no filesystem access -- and is the exact hash
/// the TypeScript launcher's `repositoryIdFromRoot` must reproduce. Both sides
/// are guarded by `fixtures/repo-id/repo-id-cases.json`.
#[must_use]
pub fn repository_id_from_canonical_root(canonical_root: &str) -> String {
    hex::encode(&repository_digest(canonical_root)[..16])
}

/// Walks up from `canonical` (inclusive) looking for a `.git` entry -- a
/// directory (an ordinary checkout) or a file (a worktree's gitdir
/// pointer). The entry's contents are never read, only its presence, so a
/// malicious `.git` file cannot redirect resolution outside `canonical`'s
/// own ancestor chain. Returns the first ancestor that has one, or `None`
/// if none is found before the filesystem root.
fn discover_vcs_root(canonical: &Path) -> Option<PathBuf> {
    let mut current = canonical;
    loop {
        if current.join(".git").symlink_metadata().is_ok() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_vcs_root_finds_dot_git_directory_in_ancestor() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(discover_vcs_root(&nested), Some(repo.path().to_path_buf()));
    }

    #[test]
    fn discover_vcs_root_finds_dot_git_file_for_worktrees() {
        let repo = tempfile::tempdir().unwrap();
        fs::write(repo.path().join(".git"), "gitdir: /elsewhere\n").unwrap();

        assert_eq!(
            discover_vcs_root(repo.path()),
            Some(repo.path().to_path_buf())
        );
    }

    #[test]
    fn discover_vcs_root_returns_none_without_a_dot_git_entry() {
        let repo = tempfile::tempdir().unwrap();
        // Guard against the (unlikely) case the OS temp dir itself lives
        // under a git checkout, which would make this test flaky.
        if discover_vcs_root(repo.path()).is_some() {
            return;
        }
        assert_eq!(discover_vcs_root(repo.path()), None);
    }
}
