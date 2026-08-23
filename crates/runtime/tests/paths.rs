//! Integration tests for `RuntimePaths::resolve` and `StateRoot::resolve`:
//! secure per-repository state directories, stable project ids, and
//! cross-language precedence parity with `packages/extension/src/state.ts`.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use batman_runtime::PathError;
use batman_runtime::{RuntimePaths, SecurityError, StateRoot, repository_id_from_canonical_root};

const STATE_ROOT_CASES_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/state/state-root-cases.json"
));

const REPO_ID_CASES_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/repo-id/repo-id-cases.json"
));

#[derive(serde::Deserialize)]
struct RepoIdCase {
    name: String,
    #[serde(rename = "canonicalRoot")]
    canonical_root: String,
    #[serde(rename = "repositoryId")]
    repository_id: String,
}

#[test]
fn repository_id_matches_shared_cross_language_fixture() {
    let cases: Vec<RepoIdCase> =
        serde_json::from_str(REPO_ID_CASES_FIXTURE).expect("fixture is valid JSON");
    assert!(!cases.is_empty(), "fixture must contain at least one case");

    for case in cases {
        assert_eq!(
            repository_id_from_canonical_root(&case.canonical_root),
            case.repository_id,
            "case {:?} produced an unexpected repository id",
            case.name
        );
    }
}

#[derive(serde::Deserialize)]
struct StateRootCase {
    name: String,
    env: HashMap<String, String>,
    home: String,
    #[serde(default)]
    expected: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[test]
fn state_root_precedence_matches_shared_fixture() {
    let cases: Vec<StateRootCase> =
        serde_json::from_str(STATE_ROOT_CASES_FIXTURE).expect("fixture is valid JSON");
    assert!(!cases.is_empty(), "fixture must contain at least one case");

    for case in cases {
        let home = PathBuf::from(&case.home);
        let resolved = StateRoot::resolve(&case.env, &home);

        match (&case.expected, &case.error) {
            (Some(expected), None) => {
                let root = resolved
                    .unwrap_or_else(|err| panic!("case {:?} expected Ok, got {err:?}", case.name));
                assert_eq!(
                    root.path(),
                    PathBuf::from(expected),
                    "case {:?} resolved unexpected root",
                    case.name
                );
            }
            (None, Some(_reason)) => {
                assert!(
                    resolved.is_err(),
                    "case {:?} expected an error, got {resolved:?}",
                    case.name
                );
            }
            _ => panic!(
                "case {:?} must set exactly one of `expected`/`error`",
                case.name
            ),
        }
    }
}

#[test]
fn rejects_relative_crew_state_dir_override() {
    let mut env = HashMap::new();
    env.insert("CREW_STATE_DIR".to_string(), "relative/state".to_string());
    let home = PathBuf::from("/home/alice");

    let err = StateRoot::resolve(&env, &home).expect_err("relative override must be rejected");
    assert!(matches!(
        err,
        SecurityError::RelativeOverride {
            var: "CREW_STATE_DIR",
            ..
        }
    ));
}

#[test]
fn rejects_relative_legacy_batman_state_dir_override() {
    let mut env = HashMap::new();
    env.insert("BATMAN_STATE_DIR".to_string(), "relative/state".to_string());
    let home = PathBuf::from("/home/alice");

    let err = StateRoot::resolve(&env, &home).expect_err("relative override must be rejected");
    assert!(matches!(
        err,
        SecurityError::RelativeOverride {
            var: "CREW_STATE_DIR",
            ..
        }
    ));
}

#[test]
fn rejects_relative_xdg_state_home_override() {
    let mut env = HashMap::new();
    env.insert("XDG_STATE_HOME".to_string(), "relative/state".to_string());
    let home = PathBuf::from("/home/alice");

    let err = StateRoot::resolve(&env, &home).expect_err("relative override must be rejected");
    assert!(matches!(
        err,
        SecurityError::RelativeOverride {
            var: "XDG_STATE_HOME",
            ..
        }
    ));
}

#[test]
fn resolves_paths_under_state_root_with_private_permissions() {
    let state_root = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let other_repo = tempfile::tempdir().unwrap();

    let paths = RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();

    assert!(paths.root.starts_with(state_root.path().join("repos")));
    assert_eq!(paths.socket.file_name().unwrap(), "runtime.sock");
    assert_eq!(
        std::fs::metadata(&paths.root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&paths.artifacts)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_ne!(
        paths.project_id.to_string(),
        RuntimePaths::resolve(state_root.path(), other_repo.path())
            .unwrap()
            .project_id
            .to_string()
    );
}

#[test]
fn exposes_lock_database_log_and_artifacts_under_root() {
    let state_root = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let paths = RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();

    assert!(paths.lock.starts_with(&paths.root));
    assert!(paths.database.starts_with(&paths.root));
    assert!(paths.log.starts_with(&paths.root));
    assert!(paths.artifacts.starts_with(&paths.root));
    assert_eq!(paths.artifacts.file_name().unwrap(), "artifacts");
}

#[test]
fn symlinked_repository_resolves_to_same_project_id_as_canonical_target() {
    let state_root = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();

    let link_dir = tempfile::tempdir().unwrap();
    let link = link_dir.path().join("repo-link");
    std::os::unix::fs::symlink(target.path(), &link).unwrap();

    let via_target = RuntimePaths::resolve(state_root.path(), target.path()).unwrap();
    let via_symlink = RuntimePaths::resolve(state_root.path(), &link).unwrap();

    assert_eq!(via_target.project_id, via_symlink.project_id);
    assert_eq!(via_target.root, via_symlink.root);
}

#[test]
fn differing_repositories_get_differing_project_ids() {
    let state_root = tempfile::tempdir().unwrap();
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();

    let paths_a = RuntimePaths::resolve(state_root.path(), repo_a.path()).unwrap();
    let paths_b = RuntimePaths::resolve(state_root.path(), repo_b.path()).unwrap();

    assert_ne!(paths_a.project_id, paths_b.project_id);
    assert_ne!(paths_a.root, paths_b.root);
}

#[test]
fn repeated_resolution_is_stable_across_restarts() {
    let state_root = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let first = RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();
    let second = RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();

    assert_eq!(first.project_id, second.project_id);
    assert_eq!(first.root, second.root);
}

#[test]
fn discovers_vcs_root_by_walking_up_for_dot_git_directory() {
    let state_root = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    let nested = repo.path().join("crates").join("nested");
    std::fs::create_dir_all(&nested).unwrap();

    let via_root = RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();
    let via_nested = RuntimePaths::resolve(state_root.path(), &nested).unwrap();

    assert_eq!(
        via_root.project_id, via_nested.project_id,
        "a subdirectory of a checkout must resolve to the same repository"
    );
}

#[test]
fn recognizes_worktree_dot_git_file() {
    let state_root = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    std::fs::write(
        worktree.path().join(".git"),
        "gitdir: /elsewhere/.git/worktrees/example\n",
    )
    .unwrap();
    let nested = worktree.path().join("src");
    std::fs::create_dir(&nested).unwrap();

    let via_root = RuntimePaths::resolve(state_root.path(), worktree.path()).unwrap();
    let via_nested = RuntimePaths::resolve(state_root.path(), &nested).unwrap();

    assert_eq!(via_root.project_id, via_nested.project_id);
}

#[test]
fn falls_back_to_supplied_directory_when_no_vcs_root_exists() {
    let state_root = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let nested = repo.path().join("nested");
    std::fs::create_dir(&nested).unwrap();

    let via_repo = RuntimePaths::resolve(state_root.path(), repo.path()).unwrap();
    let via_nested = RuntimePaths::resolve(state_root.path(), &nested).unwrap();

    assert_ne!(
        via_repo.project_id, via_nested.project_id,
        "without a VCS root each directory is its own repository"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn non_utf8_repository_path_is_rejected() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let state_root = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();

    let invalid_name = OsStr::from_bytes(&[0x66, 0x6f, 0xff, 0x6f]); // "fo\xFFo"
    let invalid_path = parent.path().join(invalid_name);
    std::fs::create_dir(&invalid_path).unwrap();

    let err = RuntimePaths::resolve(state_root.path(), &invalid_path)
        .expect_err("non-UTF-8 repository path must be rejected");
    assert!(matches!(err, PathError::NonUtf8 { .. }));
}
