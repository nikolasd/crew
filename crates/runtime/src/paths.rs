//! Derives the on-disk paths Crew's runtime uses for a given repository,
//! namespaced under a [`crate::security::StateRoot`].
//!
//! Given a repository directory, [`RuntimePaths::resolve`] canonicalizes it,
//! walks up to find its VCS root (so any subdirectory of a checkout, or a
//! git worktree, maps to the same repository), and derives a stable
//! `repository-id` and [`ProjectId`] from a SHA-256 hash of that canonical
//! root -- deterministic across restarts, unlike [`ProjectId::new`]'s
//! random UUIDv7.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crew_protocol::ProjectId;
use nix::unistd::Uid;
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
    /// A short, private, per-user directory (created mode `0700`) holding
    /// one per-worker attach socket per active run across *every*
    /// repository this user runs Crew against (see [`Self::pane_socket`]).
    /// Deliberately **not** nested under `root` (CREW-1): a run id is
    /// already a globally unique UUIDv7, so the socket never needed
    /// per-repository namespacing for correctness -- only the durable
    /// state below does -- and `<root>/panes/<run-id>.sock`'s fixed
    /// ~97-byte suffix after `$HOME` overflows macOS's 104-byte
    /// `sun_path` for any real home directory. See [`pane_socket_root`]
    /// for the actual resolved location. The attach server binds a fresh
    /// socket here and removes it on stop, but the directory itself
    /// persists across runs.
    pub panes: PathBuf,
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
        let panes = pane_socket_root();

        ensure_private_dir(&root)?;
        ensure_private_dir(&artifacts)?;
        ensure_private_dir(&panes)?;

        Ok(Self {
            project_id,
            socket: root.join("runtime.sock"),
            lock: root.join("runtime.lock"),
            database: root.join("runtime.db"),
            log: root.join("runtime.log"),
            artifacts,
            panes,
            root,
        })
    }

    /// The per-worker attach socket path for `run_id`:
    /// `<panes>/<run_id>.sock`. Pure path arithmetic -- does not bind or
    /// otherwise touch the filesystem; [`crate::display::attach::AttachServer`]
    /// is what actually creates the socket node here.
    #[must_use]
    pub fn pane_socket(&self, run_id: &crew_protocol::RunId) -> PathBuf {
        self.panes.join(format!("{run_id}.sock"))
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

/// The short, per-user directory ephemeral pane-attach sockets bind under
/// (CREW-1). Reads the real environment and the real effective uid; see
/// [`pane_socket_root_with`] for the pure, testable resolution logic.
fn pane_socket_root() -> PathBuf {
    let env: HashMap<String, String> = std::env::vars().collect();
    pane_socket_root_with(&env, Uid::current().as_raw())
}

/// Resolves the pane-socket directory for a given environment and uid,
/// without touching the real process environment -- what makes this
/// testable deterministically (the same pattern
/// [`crate::security::StateRoot::resolve_with`] uses).
///
/// Prefers `$XDG_RUNTIME_DIR/crew` (short, private, and -- critically --
/// not world-writable: typically `/run/user/<uid>` on systemd Linux, so it
/// carries no new symlink-attack surface) when it is set and a socket
/// bound under it would still fit the platform `sun_path` bound. Falls
/// back to `/tmp/crew-<uid>` otherwise, whether because `$XDG_RUNTIME_DIR`
/// is unset (the common case on macOS) or because some unusual value of
/// it would itself overflow -- `/tmp/crew-<uid>` is always short enough.
/// Both candidates are created via [`ensure_private_dir`], whose symlink
/// rejection is what makes the `/tmp` fallback safe despite sitting
/// directly under a world-writable parent (see
/// [`crate::security::SecurityError::UntrustedSymlink`]).
fn pane_socket_root_with(env: &HashMap<String, String>, uid: u32) -> PathBuf {
    let fallback = PathBuf::from(format!("/tmp/crew-{uid}"));
    let xdg_runtime_dir = env
        .get("XDG_RUNTIME_DIR")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let Some(xdg_runtime_dir) = xdg_runtime_dir else {
        return fallback;
    };
    let candidate = PathBuf::from(xdg_runtime_dir).join("crew");
    if pane_socket_dir_fits(&candidate) {
        candidate
    } else {
        fallback
    }
}

/// Whether a candidate pane-socket directory leaves room for the longest
/// name [`RuntimePaths::pane_socket`] ever joins onto it: a 36-character
/// UUID (a [`crew_protocol::RunId`]'s canonical form) plus `.sock`.
fn pane_socket_dir_fits(dir: &Path) -> bool {
    let longest_run_id = "0".repeat(36);
    crate::ipc::socket_path_within_limit(&dir.join(format!("{longest_run_id}.sock")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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

    #[test]
    fn resolve_creates_a_private_panes_directory() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        let state_root = tempfile::tempdir().unwrap();

        let paths = RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();

        assert!(paths.panes.is_dir());
        // CREW-1: panes is deliberately NOT nested under root any more -- a
        // run id is already a globally unique UUIDv7, so the socket never
        // needed per-repository namespacing, and nesting it under root's
        // long `<state_root>/repos/<id>/` prefix is exactly what overflowed
        // the platform sun_path bound.
        assert!(
            !paths.panes.starts_with(&paths.root),
            "panes must not be nested under the per-repository root: {:?} vs {:?}",
            paths.panes,
            paths.root
        );
        let mode = fs::metadata(&paths.panes).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "panes directory must be created owner-only");
    }

    #[test]
    fn pane_socket_is_named_after_the_run_id_under_panes() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        let state_root = tempfile::tempdir().unwrap();

        let paths = RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();
        let run_id = crew_protocol::RunId::new();

        assert_eq!(
            paths.pane_socket(&run_id),
            paths.panes.join(format!("{run_id}.sock"))
        );
    }

    /// CREW-1 regression: even under an unrealistically long state root
    /// (simulating a long `$HOME`), the pane socket path must still fit the
    /// platform `sun_path` bound -- the entire point of no longer nesting
    /// `panes` under `root`.
    #[test]
    fn resolve_keeps_the_pane_socket_within_sun_path_under_a_very_long_state_root() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir(repo.path().join(".git")).unwrap();
        let base = tempfile::tempdir().unwrap();
        // A deliberately long path segment, simulating a long real-world
        // $HOME (e.g. a corporate-managed macOS home directory nested
        // several levels under /Users/<long.name>/Library/...).
        let long_state_root = base
            .path()
            .join("a-very-long-simulated-home-directory-segment-used-only-for-this-regression-test")
            .join("Library")
            .join("Application Support")
            .join("omp")
            .join("crew");
        fs::create_dir_all(&long_state_root).unwrap();

        let paths = RuntimePaths::resolve(&long_state_root, repo.path()).unwrap();
        let run_id = crew_protocol::RunId::new();

        assert!(
            crate::ipc::socket_path_within_limit(&paths.pane_socket(&run_id)),
            "pane socket path overflows sun_path even under a long state root: {:?}",
            paths.pane_socket(&run_id)
        );
    }

    #[test]
    fn pane_socket_root_with_falls_back_to_tmp_when_xdg_runtime_dir_is_unset() {
        let env = HashMap::new();
        assert_eq!(
            pane_socket_root_with(&env, 501),
            PathBuf::from("/tmp/crew-501")
        );
    }

    #[test]
    fn pane_socket_root_with_falls_back_to_tmp_when_xdg_runtime_dir_is_empty() {
        let mut env = HashMap::new();
        env.insert("XDG_RUNTIME_DIR".to_string(), String::new());
        assert_eq!(
            pane_socket_root_with(&env, 501),
            PathBuf::from("/tmp/crew-501")
        );
    }

    #[test]
    fn pane_socket_root_with_prefers_xdg_runtime_dir_when_it_fits() {
        let mut env = HashMap::new();
        env.insert("XDG_RUNTIME_DIR".to_string(), "/run/user/501".to_string());
        assert_eq!(
            pane_socket_root_with(&env, 501),
            PathBuf::from("/run/user/501/crew")
        );
    }

    /// CREW-1 rider: `$XDG_RUNTIME_DIR` is env-supplied and unbounded --
    /// an unusually long value must not be trusted blindly. A deliberately
    /// long one must fall back to the always-short `/tmp/crew-<uid>` form
    /// rather than produce a socket path that itself overflows.
    #[test]
    fn pane_socket_root_with_falls_back_to_tmp_when_xdg_runtime_dir_would_overflow() {
        let mut env = HashMap::new();
        let long_xdg_runtime_dir = format!("/run/user/{}", "0".repeat(200));
        env.insert("XDG_RUNTIME_DIR".to_string(), long_xdg_runtime_dir);
        assert_eq!(
            pane_socket_root_with(&env, 501),
            PathBuf::from("/tmp/crew-501"),
            "an overflowing XDG_RUNTIME_DIR must fall back to the tmp form"
        );
    }
}
