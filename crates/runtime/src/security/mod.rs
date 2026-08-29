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

    /// `path` already exists as a symlink. `DirBuilder::create` (recursive)
    /// treats an existing symlink-to-directory as "already there" and skips
    /// creating it, and the ownership/chmod steps that follow both dereference
    /// symlinks -- so without this check, a local attacker who pre-creates
    /// `path` as a symlink to a directory the expected uid already owns (but
    /// never intended to expose here) passes the ownership check and gets
    /// that arbitrary directory silently chmod-ed to `0700` and reused. Safe
    /// only as long as every caller's parent directory is itself
    /// non-world-writable (`$HOME`, `$XDG_RUNTIME_DIR`); a caller that resolves
    /// `path` under a world-writable directory (e.g. `/tmp`) must not skip
    /// this rejection.
    #[error("{path:?} is a symlink; refusing to create or reuse a directory through one")]
    UntrustedSymlink { path: PathBuf },
}

/// The root directory Crew stores all per-repository state under.
///
/// [`StateRoot::resolve`] is a function of `env` and `home` -- deliberately
/// no process-global env/home reads -- so callers (and tests) can drive it
/// from fixtures, and so it mirrors the TypeScript `resolveStateRoot`
/// exactly. It does consult the real filesystem for the fresh-vs-legacy
/// directory check (see [`StateRoot::resolve_with`]); [`StateRoot::resolve`]
/// is the production entry point that probes the real filesystem, while
/// [`StateRoot::resolve_with`] takes the probe as a parameter so tests stay
/// deterministic. Creating the directory and enforcing private permissions
/// is a separate, explicit step ([`StateRoot::ensure_private`]) performed on
/// the Rust side only; the TypeScript side never touches the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRoot(PathBuf);

impl StateRoot {
    /// Resolves the Crew state root from `env`/`home` using the precedence:
    /// `CREW_STATE_DIR` (or its pre-rename name, `BATMAN_STATE_DIR`) ->
    /// `$XDG_STATE_HOME/omp/crew` -> `$HOME/${PI_CONFIG_DIR:-.omp}/crew`.
    ///
    /// Delegates to [`Self::resolve_with`] using the real filesystem
    /// (`Path::exists`) as the existence probe -- see that function for the
    /// fresh-install-vs-legacy-directory rule this applies in the two
    /// fallback tiers.
    ///
    /// # Errors
    /// Returns [`SecurityError::RelativeOverride`] if `CREW_STATE_DIR`
    /// (or legacy `BATMAN_STATE_DIR`) or `XDG_STATE_HOME` is set but not an
    /// absolute path.
    pub fn resolve(env: &HashMap<String, String>, home: &Path) -> Result<Self, SecurityError> {
        Self::resolve_with(env, home, |path| path.exists())
    }

    /// Resolves the Crew state root exactly like [`Self::resolve`], but
    /// takes the directory-existence probe as a parameter instead of
    /// touching the real filesystem.
    ///
    /// This is what makes the function testable deterministically:
    /// production always passes the real check through [`Self::resolve`];
    /// tests inject a closure backed by a fixed set of paths (see
    /// `fixtures/state/state-root-cases.json`'s `existingDirs` field and the
    /// TypeScript `resolveStateRoot`'s equivalent `exists` parameter, which
    /// this must stay in semantic lockstep with).
    ///
    /// Precedence: `CREW_STATE_DIR` (or its pre-rename name,
    /// `BATMAN_STATE_DIR`) wins outright if set. Otherwise, in both the
    /// `XDG_STATE_HOME` tier and the `$HOME` fallback tier, the *new*
    /// `crew`-named directory is preferred, but if it does not exist yet
    /// and the *legacy* `batman`-named directory does, the legacy directory
    /// is used instead -- so a fresh install lands under `crew` while an
    /// existing install keeps working against its `batman` directory
    /// without this function ever moving data itself.
    ///
    /// # Errors
    /// Returns [`SecurityError::RelativeOverride`] if `CREW_STATE_DIR`
    /// (or legacy `BATMAN_STATE_DIR`) or `XDG_STATE_HOME` is set but not an
    /// absolute path.
    pub fn resolve_with(
        env: &HashMap<String, String>,
        home: &Path,
        exists: impl Fn(&Path) -> bool,
    ) -> Result<Self, SecurityError> {
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
            return Ok(Self(Self::preferring_legacy_if_only_it_exists(
                &path.join("omp"),
                &exists,
            )));
        }

        let pi_config_dir = env.get("PI_CONFIG_DIR").map_or(".omp", String::as_str);
        Ok(Self(Self::preferring_legacy_if_only_it_exists(
            &home.join(pi_config_dir),
            &exists,
        )))
    }

    /// Given a parent directory (`$XDG_STATE_HOME/omp` or
    /// `$HOME/${PI_CONFIG_DIR:-.omp}`), returns `parent/crew` unless
    /// `parent/batman` exists and `parent/crew` does not, in which case it
    /// returns `parent/batman`.
    fn preferring_legacy_if_only_it_exists(
        parent: &Path,
        exists: &impl Fn(&Path) -> bool,
    ) -> PathBuf {
        let crew_dir = parent.join("crew");
        let legacy_dir = parent.join("batman");
        if !exists(&crew_dir) && exists(&legacy_dir) {
            legacy_dir
        } else {
            crew_dir
        }
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
/// Rejects `path` if it exists as a symlink, via `lstat` (which does not
/// dereference the final path component, unlike `fs::metadata`/`create`).
/// Called twice by [`ensure_private_dir_as`]: once before anything touches
/// `path`, and once again immediately after `DirBuilder::create` returns, to
/// close the race where a symlink is planted in between (see that call
/// site's comment).
fn reject_symlink(path: &Path) -> Result<(), SecurityError> {
    if let Ok(link_metadata) = fs::symlink_metadata(path)
        && link_metadata.file_type().is_symlink()
    {
        return Err(SecurityError::UntrustedSymlink {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn ensure_private_dir_as(path: &Path, expected_uid: u32) -> Result<(), SecurityError> {
    // Reject a pre-existing symlink at `path` before anything else touches
    // it. `fs::symlink_metadata` (lstat) does not follow the final
    // component, so this sees the symlink itself rather than whatever it
    // points to; `DirBuilder::create` and the ownership/chmod checks below
    // all dereference, so this must run first. See `SecurityError::
    // UntrustedSymlink`'s doc comment for the attack this closes.
    reject_symlink(path)?;

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path).map_err(|source| SecurityError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    // Re-check immediately after `create()`: a symlink planted in the
    // (sub-millisecond) window between the check above and `create()`
    // returning is invisible to that first check, and `create()` itself
    // treats an existing symlink-to-directory as "already there" and
    // succeeds through it rather than erroring. This closes that race
    // rather than relying on the first check alone.
    reject_symlink(path)?;

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

/// Whether `path`'s parent directory is owned by `euid` and accessible only
/// by its owner (no group/other permission bits). The fallback admission
/// signal for [`admit_same_uid`] when peer credentials are unavailable on
/// this platform -- mirrors `ipc::server`'s own (private) `check_owner_only`
/// so a second socket (e.g. a per-worker attach socket) can enforce the
/// identical boundary without depending on that module.
#[must_use]
pub fn parent_dir_is_owner_only(path: &Path, euid: u32) -> bool {
    let dir = path.parent().unwrap_or_else(|| Path::new("/"));
    match fs::metadata(dir) {
        Ok(meta) => meta.uid() == euid && (meta.mode() & 0o077) == 0,
        Err(_) => false,
    }
}

/// Decides whether to admit a connection: the same-user peer-credential
/// boundary every Crew socket enforces before a single byte is read from
/// it. `peer_uid` is whatever the platform reported (`None` where peer
/// credentials aren't available); `owner_only_verified` is the fallback
/// signal (typically [`parent_dir_is_owner_only`]) used only when
/// `peer_uid` is `None` -- a platform that cannot report credentials fails
/// closed unless the socket's own directory already proves owner-only
/// access. Mirrors `ipc::server::Server::admit`'s decision exactly, so the
/// runtime socket and any per-worker attach socket enforce it identically.
#[must_use]
pub fn admit_same_uid(peer_uid: Option<u32>, euid: u32, owner_only_verified: bool) -> bool {
    match peer_uid {
        Some(uid) => uid == euid,
        None => owner_only_verified,
    }
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
    fn xdg_state_home_appends_omp_crew_when_nothing_exists() {
        let root = StateRoot::resolve_with(
            &env(&[("XDG_STATE_HOME", "/home/alice/.local/state")]),
            Path::new("/home/alice"),
            |_| false,
        )
        .unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.local/state/omp/crew"));
    }

    #[test]
    fn xdg_state_home_falls_back_to_legacy_omp_batman_when_only_it_exists() {
        let root = StateRoot::resolve_with(
            &env(&[("XDG_STATE_HOME", "/home/alice/.local/state")]),
            Path::new("/home/alice"),
            |path| path == Path::new("/home/alice/.local/state/omp/batman"),
        )
        .unwrap();
        assert_eq!(
            root.path(),
            Path::new("/home/alice/.local/state/omp/batman")
        );
    }

    #[test]
    fn xdg_state_home_prefers_new_omp_crew_when_both_exist() {
        let root = StateRoot::resolve_with(
            &env(&[("XDG_STATE_HOME", "/home/alice/.local/state")]),
            Path::new("/home/alice"),
            |_| true,
        )
        .unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.local/state/omp/crew"));
    }

    #[test]
    fn falls_back_to_home_omp_crew_when_nothing_exists() {
        let root =
            StateRoot::resolve_with(&HashMap::new(), Path::new("/home/alice"), |_| false).unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.omp/crew"));
    }

    #[test]
    fn falls_back_to_legacy_home_omp_batman_when_only_it_exists() {
        let root = StateRoot::resolve_with(&HashMap::new(), Path::new("/home/alice"), |path| {
            path == Path::new("/home/alice/.omp/batman")
        })
        .unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.omp/batman"));
    }

    #[test]
    fn pi_config_dir_overrides_default_directory_name_when_nothing_exists() {
        let root = StateRoot::resolve_with(
            &env(&[("PI_CONFIG_DIR", ".config-omp")]),
            Path::new("/home/alice"),
            |_| false,
        )
        .unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.config-omp/crew"));
    }

    #[test]
    fn pi_config_dir_override_falls_back_to_legacy_when_only_it_exists() {
        let root = StateRoot::resolve_with(
            &env(&[("PI_CONFIG_DIR", ".config-omp")]),
            Path::new("/home/alice"),
            |path| path == Path::new("/home/alice/.config-omp/batman"),
        )
        .unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.config-omp/batman"));
    }

    #[test]
    fn resolve_uses_the_real_filesystem_for_the_existence_probe() {
        // `resolve` (unlike `resolve_with`) probes the real filesystem; a
        // fabricated home directory that does not exist on this machine
        // must resolve to the fresh `crew` name in both fallback tiers.
        let root = StateRoot::resolve(&HashMap::new(), Path::new("/home/alice")).unwrap();
        assert_eq!(root.path(), Path::new("/home/alice/.omp/crew"));
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

    /// CREW-1 blocker: a local attacker who pre-creates `path` as a symlink
    /// to a directory the current uid *already owns* (but never intended to
    /// expose here) must be rejected outright -- not have that arbitrary
    /// directory silently chmod-ed to `0700` and reused. This is what makes
    /// relocating a private directory under a world-writable parent (e.g.
    /// `/tmp`) safe: `ensure_private_dir`'s existing ownership check alone
    /// passes here (the symlink's target really is owned by the expected
    /// uid), so only an explicit symlink rejection closes it.
    #[test]
    fn ensure_private_dir_rejects_a_symlink_even_when_its_target_is_owned_by_the_expected_uid() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::create_dir(&victim).unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o755)).unwrap();

        let attacker_planted = dir.path().join("state");
        std::os::unix::fs::symlink(&victim, &attacker_planted).unwrap();

        let real_uid = Uid::current().as_raw();
        let err = ensure_private_dir_as(&attacker_planted, real_uid)
            .expect_err("a symlinked path must be rejected even when its target is owned by us");
        match err {
            SecurityError::UntrustedSymlink { path } => assert_eq!(path, attacker_planted),
            other => panic!("expected UntrustedSymlink, got {other:?}"),
        }

        // The real point of the test: the victim directory's permissions
        // must be completely untouched -- proof we never dereferenced the
        // symlink to chmod its target.
        let mode = fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "the symlink target must never be touched, chmod-ed, or otherwise reused"
        );
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

    #[test]
    fn admit_same_uid_rejects_a_mismatched_peer_uid_even_with_owner_only_directory() {
        let euid = Uid::current().as_raw();
        let foreign_uid = euid.wrapping_add(1);

        assert!(
            !admit_same_uid(Some(foreign_uid), euid, true),
            "a reported peer uid that differs from euid must never be admitted, \
             regardless of the owner-only fallback signal"
        );
    }

    #[test]
    fn admit_same_uid_accepts_a_matching_peer_uid() {
        let euid = Uid::current().as_raw();
        assert!(admit_same_uid(Some(euid), euid, false));
    }

    #[test]
    fn admit_same_uid_falls_back_to_the_owner_only_signal_when_credentials_are_unavailable() {
        let euid = Uid::current().as_raw();
        assert!(
            admit_same_uid(None, euid, true),
            "no peer uid reported, but the directory is owner-only: admit"
        );
        assert!(
            !admit_same_uid(None, euid, false),
            "no peer uid reported and the directory is not owner-only: fail closed"
        );
    }

    #[test]
    fn parent_dir_is_owner_only_reflects_the_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        let euid = Uid::current().as_raw();
        let socket_path = dir.path().join("run.sock");

        // `tempdir()` does not itself guarantee `0700` (it is subject to
        // the process umask, e.g. `022` yields `0755`), so tighten it
        // explicitly for the true-positive case rather than assuming it.
        let mut permissions = fs::metadata(dir.path()).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(dir.path(), permissions).unwrap();
        assert!(parent_dir_is_owner_only(&socket_path, euid));

        // Loosen it back up: group/other-accessible must fail, even
        // though the owner is still correct.
        let mut loose = fs::metadata(dir.path()).unwrap().permissions();
        loose.set_mode(0o755);
        fs::set_permissions(dir.path(), loose).unwrap();
        assert!(
            !parent_dir_is_owner_only(&socket_path, euid),
            "a group/other-accessible directory must not verify as owner-only"
        );

        // Restore `0700` before checking the owner mismatch case, so this
        // assertion is isolated to the uid check alone.
        let mut tight = fs::metadata(dir.path()).unwrap().permissions();
        tight.set_mode(0o700);
        fs::set_permissions(dir.path(), tight).unwrap();
        let foreign_uid = euid.wrapping_add(1);
        assert!(
            !parent_dir_is_owner_only(&socket_path, foreign_uid),
            "a directory not owned by the checked euid must not verify as owner-only"
        );
    }
}
