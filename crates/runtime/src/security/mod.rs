//! Security-critical filesystem primitives: resolving the Crew state root
//! and ensuring every directory or file Crew creates on disk is private
//! (mode `0700`/`0600`, owned by the current user) before anything else is
//! written into it.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::unistd::Uid;

use crate::env_flag::env_flag_from;

pub mod redaction;
pub mod rules;

/// Errors resolving or securing Crew's on-disk state.
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    /// `CREW_STATE_DIR` (or its pre-rename name, `BATMAN_STATE_DIR`) or
    /// `XDG_STATE_HOME` was set to a relative path. Both must be absolute,
    /// since they anchor where secrets and sockets live regardless of the
    /// current working directory.
    #[error("{var} must be an absolute path, got {value:?}")]
    RelativeOverride { var: &'static str, value: String },

    /// Creating, reading, or chmod-ing a path failed at the OS level.
    #[error("failed to create or inspect {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A pre-existing directory is owned by someone other than the current
    /// user. Reusing it would let that other user read or tamper with
    /// Crew's state, so it is rejected rather than merely warned about.
    #[error(
        "{path:?} is owned by uid {owner}, expected the current uid {expected}; refusing to reuse a directory Crew does not own"
    )]
    UntrustedOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },

    /// Permissions were set but a follow-up read shows they didn't take
    /// (e.g. an ACL or mount option overriding the mode bits).
    #[error("failed to make {path:?} private: mode is {mode:o} after chmod, expected {expected:o}")]
    InsecurePermissions {
        path: PathBuf,
        mode: u32,
        expected: u32,
    },
}

/// The root directory Crew stores all per-repository state under.
///
/// [`StateRoot::resolve`] is a pure function of `env` and `home` --
/// deliberately no process-global reads -- so callers (and tests) can drive
/// it from fixtures, and so it mirrors the TypeScript `resolveStateRoot`
/// exactly. Creating the directory and enforcing private permissions is a
/// separate, explicit step ([`StateRoot::ensure_private`]) performed on the
/// Rust side only; the TypeScript side never touches the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRoot(PathBuf);

impl StateRoot {
    /// Resolves the Crew state root from `env`/`home` using the precedence:
    /// `CREW_STATE_DIR` (or its pre-rename name, `BATMAN_STATE_DIR`) ->
    /// `$XDG_STATE_HOME/omp/batman` -> `$HOME/${PI_CONFIG_DIR:-.omp}/batman`.
    /// The on-disk directory name stays `batman` in both fallback tiers:
    /// moving already-provisioned user state is a separate, careful
    /// migration this rename does not attempt.
    ///
    /// # Errors
    /// Returns [`SecurityError::RelativeOverride`] if `CREW_STATE_DIR`
    /// (or legacy `BATMAN_STATE_DIR`) or `XDG_STATE_HOME` is set but not an
    /// absolute path.
    pub fn resolve(env: &HashMap<String, String>, home: &Path) -> Result<Self, SecurityError> {
        if let Some(value) = env_flag_from(env, "CREW_STATE_DIR", "BATMAN_STATE_DIR") {
            let path = PathBuf::from(&value);
            if !path.is_absolute() {
                return Err(SecurityError::RelativeOverride {
                    var: "CREW_STATE_DIR",
                    value,
                });
            }
            return Ok(Self(path));
        }

        if let Some(value) = env.get("XDG_STATE_HOME") {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(SecurityError::RelativeOverride {
                    var: "XDG_STATE_HOME",
                    value: value.clone(),
                });
            }
            return Ok(Self(path.join("omp").join("batman")));
        }

        let pi_config_dir = env.get("PI_CONFIG_DIR").map_or(".omp", String::as_str);
        Ok(Self(home.join(pi_config_dir).join("batman")))
    }

    /// The resolved absolute path. No filesystem access has happened yet.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Creates this directory (and any missing parents) and ensures it ends
    /// up mode `0700`, owned by the current user.
    ///
    /// # Errors
    /// Returns [`SecurityError`] if the directory cannot be created, is
    /// already owned by another user, or its permissions cannot be
    /// corrected to `0700`.
    pub fn ensure_private(&self) -> Result<&Path, SecurityError> {
        ensure_private_dir(&self.0)?;
        Ok(&self.0)
    }
}

impl AsRef<Path> for StateRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Creates `path` (and any missing parents) if needed, then ensures it is
/// mode `0700` and owned by the current user, rejecting it otherwise.
///
/// This is the single place Crew's Rust runtime creates security-sensitive
/// directories; both [`StateRoot::ensure_private`] and
/// [`crate::paths::RuntimePaths::resolve`] go through it.
///
/// # Errors
/// See [`SecurityError`].
pub fn ensure_private_dir(path: &Path) -> Result<(), SecurityError> {
    ensure_private_dir_as(path, Uid::current().as_raw())
}

/// The implementation of [`ensure_private_dir`], parameterized on the uid the
/// directory is expected to be owned by. Production always passes the real
/// effective uid via the public wrapper; the `expected_uid` seam exists so a
/// test can simulate a foreign-owned directory (a directory owned by someone
/// other than `expected_uid`) and assert [`SecurityError::UntrustedOwner`]
/// without needing root or a second real user.
fn ensure_private_dir_as(path: &Path, expected_uid: u32) -> Result<(), SecurityError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path).map_err(|source| SecurityError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let metadata = fs::metadata(path).map_err(|source| SecurityError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    if metadata.uid() != expected_uid {
        return Err(SecurityError::UntrustedOwner {
            path: path.to_path_buf(),
            owner: metadata.uid(),
            expected: expected_uid,
        });
    }

    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions).map_err(|source| SecurityError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mode = fs::metadata(path)
        .map_err(|source| SecurityError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o700 {
        return Err(SecurityError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
            expected: 0o700,
        });
    }

    Ok(())
}

/// Creates an empty file at `path` if missing, then ensures it is mode
/// `0600` and owned by the current user, rejecting it otherwise.
///
/// Used by components that materialize Crew's per-repository state files
/// (lock, database, log) so every file under [`crate::paths::RuntimePaths`]
/// is created private by construction.
///
/// # Errors
/// See [`SecurityError`].
pub fn ensure_private_file(path: &Path) -> Result<(), SecurityError> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|source| SecurityError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    let metadata = file.metadata().map_err(|source| SecurityError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let current_uid = Uid::current().as_raw();
    if metadata.uid() != current_uid {
        return Err(SecurityError::UntrustedOwner {
            path: path.to_path_buf(),
            owner: metadata.uid(),
            expected: current_uid,
        });
    }

    let mut permissions = metadata.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions).map_err(|source| SecurityError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mode = fs::metadata(path)
        .map_err(|source| SecurityError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(SecurityError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
            expected: 0o600,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn crew_state_dir_takes_precedence() {
        let root = StateRoot::resolve(
            &env(&[
                ("CREW_STATE_DIR", "/var/lib/crew"),
                ("XDG_STATE_HOME", "/home/alice/.local/state"),
            ]),
            Path::new("/home/alice"),
        )
        .unwrap();
        assert_eq!(root.path(), Path::new("/var/lib/crew"));
    }

    #[test]
    fn legacy_batman_state_dir_still_works_when_crew_state_dir_is_unset() {
        let root = StateRoot::resolve(
            &env(&[("BATMAN_STATE_DIR", "/var/lib/batman")]),
            Path::new("/home/alice"),
        )
        .unwrap();
        assert_eq!(root.path(), Path::new("/var/lib/batman"));
    }

    #[test]
    fn crew_state_dir_wins_over_legacy_batman_state_dir() {
        let root = StateRoot::resolve(
            &env(&[
                ("CREW_STATE_DIR", "/var/lib/crew"),
                ("BATMAN_STATE_DIR", "/var/lib/batman"),
            ]),
            Path::new("/home/alice"),
        )
        .unwrap();
        assert_eq!(root.path(), Path::new("/var/lib/crew"));
    }

    #[test]
    fn xdg_state_home_appends_omp_batman() {
        let root = StateRoot::resolve(
            &env(&[("XDG_STATE_HOME", "/home/alice/.local/state")]),
            Path::new("/home/alice"),
        )
        .unwrap();
        assert_eq!(
            root.path(),
            Path::new("/home/alice/.local/state/omp/batman")
        );
    }

    #[test]
    fn falls_back_to_home_omp_batman() {
        let root = StateRoot::resolve(&HashMap::new(), Path::new("/home/alice")).unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.omp/batman"));
    }

    #[test]
    fn pi_config_dir_overrides_default_directory_name() {
        let root = StateRoot::resolve(
            &env(&[("PI_CONFIG_DIR", ".config-omp")]),
            Path::new("/home/alice"),
        )
        .unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.config-omp/batman"));
    }

    #[test]
    fn rejects_relative_crew_state_dir() {
        let err = StateRoot::resolve(
            &env(&[("CREW_STATE_DIR", "relative/state")]),
            Path::new("/home/alice"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SecurityError::RelativeOverride {
                var: "CREW_STATE_DIR",
                ..
            }
        ));
    }

    #[test]
    fn rejects_relative_legacy_batman_state_dir() {
        let err = StateRoot::resolve(
            &env(&[("BATMAN_STATE_DIR", "relative/state")]),
            Path::new("/home/alice"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SecurityError::RelativeOverride {
                var: "CREW_STATE_DIR",
                ..
            }
        ));
    }

    #[test]
    fn rejects_relative_xdg_state_home() {
        let err = StateRoot::resolve(
            &env(&[("XDG_STATE_HOME", "relative/state")]),
            Path::new("/home/alice"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SecurityError::RelativeOverride {
                var: "XDG_STATE_HOME",
                ..
            }
        ));
    }

    #[test]
    fn ensure_private_dir_creates_and_chmods_to_0700() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("state");

        ensure_private_dir(&target).unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn ensure_private_dir_tightens_existing_loose_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_dir(&target).unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn ensure_private_dir_rejects_when_parent_forbids_creation() {
        let dir = tempfile::tempdir().unwrap();
        let readonly_parent = dir.path().join("readonly");
        fs::create_dir(&readonly_parent).unwrap();
        fs::set_permissions(&readonly_parent, fs::Permissions::from_mode(0o500)).unwrap();
        let target = readonly_parent.join("state");

        let result = ensure_private_dir(&target);

        // Always restore write permission so the tempdir can clean itself up.
        fs::set_permissions(&readonly_parent, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(matches!(result, Err(SecurityError::Io { .. })));
    }

    #[test]
    fn ensure_private_dir_rejects_a_foreign_owned_directory() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state");

        // The directory is created owned by the current uid; injecting a
        // *different* expected uid via the seam simulates Crew encountering a
        // pre-existing directory owned by someone else, which must be rejected
        // rather than reused.
        let real_uid = Uid::current().as_raw();
        let foreign_uid = real_uid.wrapping_add(1);

        let err = ensure_private_dir_as(&target, foreign_uid)
            .expect_err("a directory not owned by the expected uid must be rejected");

        match err {
            SecurityError::UntrustedOwner {
                owner, expected, ..
            } => {
                assert_eq!(owner, real_uid, "owner is the real creating uid");
                assert_eq!(expected, foreign_uid, "expected is the injected uid");
            }
            other => panic!("expected UntrustedOwner, got {other:?}"),
        }
    }

    #[test]
    fn ensure_private_file_creates_and_chmods_to_0600() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("runtime.db");

        ensure_private_file(&target).unwrap();

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(target.is_file());
    }
}
