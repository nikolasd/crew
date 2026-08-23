//! Workspace materialization tests.
//!
//! Tests for git worktree and copy isolation strategies with fixture repositories.

use batman_protocol::{IsolationKind, ProjectId, RunId};
use batman_runtime::workspace::{
    DEFAULT_COPY_MAX_BYTES, DEFAULT_COPY_MAX_FILES, WorkspaceMaterializer,
};
use std::path::PathBuf;

fn test_project_id(n: u32) -> ProjectId {
    ProjectId::parse(&format!("01900000-0000-0000-0000-{n:012}")).unwrap()
}

fn test_run_id(n: u32) -> RunId {
    RunId::parse(&format!("01900000-0000-0000-0000-{n:012}")).unwrap()
}

/// Fixture that holds a temp directory and a materializer, cleaning up on drop.
struct Fixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    materializer: WorkspaceMaterializer,
}

impl Fixture {
    fn new(project_id: ProjectId) -> Self {
        let temp = tempfile::TempDir::new().expect("Failed to create temp dir");
        let repo = temp.path().to_path_buf();

        // Initialize as a git repository
        let status = std::process::Command::new("git")
            .current_dir(&repo)
            .args(["init"])
            .status()
            .expect("Failed to initialize git repo");
        assert!(status.success(), "git init should succeed");

        // Configure git user for commits
        std::process::Command::new("git")
            .current_dir(&repo)
            .args(["config", "user.email", "test@test.com"])
            .status()
            .expect("Failed to configure git user");

        std::process::Command::new("git")
            .current_dir(&repo)
            .args(["config", "user.name", "Test User"])
            .status()
            .expect("Failed to configure git user");

        // Create sample files
        std::fs::write(repo.join("README.md"), "# Test Repo\n").unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();

        // Add and commit
        let status = std::process::Command::new("git")
            .current_dir(&repo)
            .args(["add", "."])
            .status()
            .expect("Failed to add files");
        assert!(status.success(), "git add should succeed");

        let status = std::process::Command::new("git")
            .current_dir(&repo)
            .args(["commit", "-m", "Initial commit"])
            .status()
            .expect("Failed to commit");
        assert!(status.success(), "git commit should succeed");

        // Compute the materializer root path and clean it up
        let root = std::env::temp_dir().join(format!("crew-workspace-{}", project_id));
        if root.exists() {
            std::fs::remove_dir_all(&root).ok();
        }

        let materializer = WorkspaceMaterializer::new(project_id, repo.clone()).unwrap();
        Fixture {
            _temp: temp,
            repo,
            materializer,
        }
    }
}

#[test]
fn workspace_materializer_create() {
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().to_path_buf();
    let result = WorkspaceMaterializer::new(test_project_id(1), repo);
    assert!(result.is_ok());
}

#[test]
fn shared_isolation_returns_repository() {
    let fixture = Fixture::new(test_project_id(1));
    let run = test_run_id(1);

    let path = fixture
        .materializer
        .materialize(run, IsolationKind::Shared)
        .unwrap();
    assert_eq!(path, fixture.repo);
}

#[test]
fn git_worktree_creates_actual_worktree() {
    let fixture = Fixture::new(test_project_id(2));
    let run1 = test_run_id(1);
    let run2 = test_run_id(2);

    let path1 = fixture
        .materializer
        .materialize(run1, IsolationKind::GitWorktree)
        .unwrap();
    let path2 = fixture
        .materializer
        .materialize(run2, IsolationKind::GitWorktree)
        .unwrap();

    // Each run gets its own directory
    assert_ne!(path1, path2);

    // Verify directories exist
    assert!(path1.exists(), "worktree directory should exist");
    assert!(path2.exists(), "worktree directory should exist");

    // Verify files were copied to worktree (git worktree creates a checkout)
    assert!(
        path1.join("README.md").exists(),
        "README.md should be in worktree"
    );
    assert!(
        path1.join("src/main.rs").exists(),
        "src/main.rs should be in worktree"
    );

    // Verify the worktree reports the correct HEAD
    let output = std::process::Command::new("git")
        .current_dir(&path1)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("Failed to execute git");

    assert!(output.status.success(), "git rev-parse should succeed");
    let head = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(!head.is_empty(), "worktree should have a HEAD commit");

    // Clean up worktrees using git worktree remove (proper cleanup)
    let status1 = std::process::Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "remove", path1.to_str().unwrap_or("")])
        .status()
        .expect("Failed to execute git");
    assert!(
        status1.success(),
        "git worktree remove for path1 should succeed"
    );

    let status2 = std::process::Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "remove", path2.to_str().unwrap_or("")])
        .status()
        .expect("Failed to execute git");
    assert!(
        status2.success(),
        "git worktree remove for path2 should succeed"
    );
}

#[test]
fn copy_isolation_copies_files_excluding_git() {
    let fixture = Fixture::new(test_project_id(3));
    let run = test_run_id(1);

    let path = fixture
        .materializer
        .materialize(run, IsolationKind::Copy)
        .unwrap();

    // Verify files were copied
    assert!(
        path.join("README.md").exists(),
        "README.md should be copied"
    );
    assert!(
        path.join("src/main.rs").exists(),
        "src/main.rs should be copied"
    );

    // Verify .git directory is NOT copied
    assert!(
        !path.join(".git").exists(),
        ".git directory should be excluded"
    );

    // Verify file contents
    let readme = std::fs::read_to_string(path.join("README.md")).unwrap();
    assert_eq!(readme, "# Test Repo\n");
}

#[test]
fn path_guard_rejects_escape() {
    let fixture = Fixture::new(test_project_id(4));

    // Absolute path outside root
    let result = fixture.materializer.validate_path("/etc/passwd");
    assert!(
        result.is_err(),
        "absolute path outside root should be rejected"
    );

    // Relative path with `..` traversal
    let result = fixture.materializer.validate_path("../etc/passwd");
    assert!(
        result.is_err(),
        "path with `..` traversal should be rejected"
    );

    // Path inside root should succeed
    let result = fixture.materializer.validate_path("subdir/file.txt");
    assert!(result.is_ok(), "path inside root should be accepted");
}

#[test]
fn path_guard_rejects_nested_escape() {
    let fixture = Fixture::new(test_project_id(5));

    // Nested `..` that escapes after normalization
    let result = fixture.materializer.validate_path("foo/../../etc/passwd");
    assert!(result.is_err(), "nested `..` escape should be rejected");
}

#[test]
fn path_guard_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().to_path_buf();

    // Create the materializer first to get the computed root
    let project_id = ProjectId::parse("01900000-0000-0000-0000-000000000009").unwrap();
    let root = std::env::temp_dir().join(format!("crew-workspace-{}", project_id));
    std::fs::create_dir_all(&root).unwrap();
    let materializer = WorkspaceMaterializer::new(project_id, repo).unwrap();

    // Create an external directory that the symlink will point to
    let external_dir = std::env::temp_dir().join("escape-target-external");
    std::fs::create_dir_all(&external_dir).unwrap();
    std::fs::write(external_dir.join("secret.txt"), "secret\n").unwrap();

    // Create a symlink under root that points outside
    symlink(&external_dir, root.join("link-to-escape")).unwrap();

    // Create a file inside the symlinked directory
    std::fs::write(external_dir.join("new-file"), "escaped file\n").unwrap();

    // Validate the symlink path - should be rejected because it escapes
    let result = materializer.validate_path("link-to-escape/new-file");
    assert!(
        result.is_err(),
        "symlink escaping root should be rejected: {:?}",
        result
    );

    // Clean up
    let _ = std::fs::remove_dir_all(&external_dir);
    let _ = std::fs::remove_file(root.join("link-to-escape"));
}

#[test]
fn copy_isolation_recreates_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().to_path_buf();

    // Create a directory symlink pointing outside the repo
    let external_dir = std::env::temp_dir().join("external-dir-target");
    std::fs::create_dir_all(external_dir.join("subdir")).unwrap();
    std::fs::write(
        external_dir.join("subdir").join("file.txt"),
        "external file
",
    )
    .unwrap();
    symlink(&external_dir, repo.join("link-to-external-dir")).unwrap();

    // Create the materializer
    let project_id = ProjectId::parse("01900000-0000-0000-0000-000000000021").unwrap();
    let root = std::env::temp_dir().join(format!("crew-workspace-{}", project_id));
    if root.exists() {
        std::fs::remove_dir_all(&root).ok();
    }
    let materializer = WorkspaceMaterializer::new(project_id, repo).unwrap();

    // Materialize with Copy isolation
    let run = test_run_id(1);
    let path = materializer.materialize(run, IsolationKind::Copy).unwrap();

    // Verify the symlink was recreated as a directory symlink (not followed)
    let link_path = path.join("link-to-external-dir");
    let link_meta = link_path.symlink_metadata().unwrap();
    assert!(link_meta.file_type().is_symlink(), "should be a symlink");

    // Delete the external directory to distinguish recreation from copying
    let _ = std::fs::remove_dir_all(&external_dir);

    // Verify the destination entry remains a symlink (not a copied directory)
    let link_meta_after = link_path.symlink_metadata().unwrap();
    assert!(
        link_meta_after.file_type().is_symlink(),
        "should still be a symlink after external_dir removed"
    );

    // Verify sub_file is now absent (proving it was a symlink, not a copy)
    let sub_file = link_path.join("subdir").join("file.txt");
    assert!(
        !sub_file.exists(),
        "external directory contents should not exist after external_dir removed"
    );

    // Clean up the symlink itself
    let _ = std::fs::remove_file(&link_path);
}

#[test]
fn copy_isolation_recreates_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().to_path_buf();

    // Create a regular file
    std::fs::write(
        repo.join("regular.txt"),
        "regular content
",
    )
    .unwrap();

    // Create a symlink pointing outside the repo
    let external_file = std::env::temp_dir().join("external-target.txt");
    std::fs::write(
        &external_file,
        "external content
",
    )
    .unwrap();
    symlink(&external_file, repo.join("link-to-external")).unwrap();

    // Create the materializer
    let project_id = ProjectId::parse("01900000-0000-0000-0000-000000000020").unwrap();
    let root = std::env::temp_dir().join(format!("crew-workspace-{}", project_id));
    if root.exists() {
        std::fs::remove_dir_all(&root).ok();
    }
    let materializer = WorkspaceMaterializer::new(project_id, repo).unwrap();

    // Materialize with Copy isolation
    let run = test_run_id(1);
    let path = materializer.materialize(run, IsolationKind::Copy).unwrap();

    // Verify regular file was copied
    assert!(
        path.join("regular.txt").exists(),
        "regular file should be copied"
    );
    let content = std::fs::read_to_string(path.join("regular.txt")).unwrap();
    assert_eq!(
        content,
        "regular content
"
    );

    // Verify symlink was recreated (not followed) - check BEFORE exists()
    let link_meta = path.join("link-to-external").symlink_metadata().unwrap();
    assert!(
        link_meta.file_type().is_symlink(),
        "should be a symlink, not a regular file"
    );
    // Now we can check exists (which follows the symlink)
    assert!(
        path.join("link-to-external").exists(),
        "symlink target should exist"
    );

    // Verify the symlink target is the external file (not the content copied)
    let target = std::fs::read_link(path.join("link-to-external")).unwrap();
    assert_eq!(target, external_file);

    // Clean up
    let _ = std::fs::remove_file(&external_file);
}

#[test]
fn path_guard_accepts_valid_relative_path() {
    let fixture = Fixture::new(test_project_id(6));

    // Valid relative path inside the repository
    let result = fixture.materializer.validate_path("README.md");
    assert!(result.is_ok(), "valid relative path should be accepted");
}

#[test]
fn copy_isolation_refuses_a_tree_over_the_byte_ceiling() {
    let fixture = Fixture::new(test_project_id(7));
    std::fs::write(fixture.repo.join("big.bin"), vec![0u8; 4096]).unwrap();
    let materializer = WorkspaceMaterializer::new(test_project_id(7), fixture.repo.clone())
        .unwrap()
        .with_copy_limits(1024, DEFAULT_COPY_MAX_FILES);
    let run = test_run_id(7);

    let err = materializer
        .materialize(run, IsolationKind::Copy)
        .expect_err("a tree larger than the byte ceiling must be refused");
    assert!(
        err.to_string().contains("byte size"),
        "the error must name the ceiling that was hit: {err}"
    );

    // A partially copied workspace is worse than none: an adapter would run
    // against a silently incomplete checkout.
    let copy_path = std::env::temp_dir()
        .join(format!("crew-workspace-{}", test_project_id(7)))
        .join(run.to_string());
    assert!(
        !copy_path.exists(),
        "the partial destination must be removed"
    );
}

#[test]
fn copy_isolation_refuses_a_tree_over_the_file_ceiling() {
    let fixture = Fixture::new(test_project_id(8));
    let materializer = WorkspaceMaterializer::new(test_project_id(8), fixture.repo.clone())
        .unwrap()
        .with_copy_limits(DEFAULT_COPY_MAX_BYTES, 1);
    let run = test_run_id(8);

    let err = materializer
        .materialize(run, IsolationKind::Copy)
        .expect_err("a tree with more files than the ceiling must be refused");
    assert!(
        err.to_string().contains("file count"),
        "the error must name the ceiling that was hit: {err}"
    );
}

#[test]
fn teardown_removes_a_git_worktree_even_when_dirty() {
    let fixture = Fixture::new(test_project_id(9));
    let run = test_run_id(9);
    let path = fixture
        .materializer
        .materialize(run, IsolationKind::GitWorktree)
        .unwrap();
    // A real worker leaves edits behind; git refuses a non-forced removal
    // of a dirty worktree, so this is the case teardown must survive.
    std::fs::write(path.join("README.md"), "# edited by the worker\n").unwrap();
    std::fs::write(path.join("scratch.txt"), "untracked\n").unwrap();

    fixture
        .materializer
        .teardown(&path, IsolationKind::GitWorktree)
        .expect("teardown must remove a dirty worktree");

    assert!(!path.exists(), "the worktree directory must be gone");
    let listed = std::process::Command::new("git")
        .current_dir(&fixture.repo)
        .args(["worktree", "list"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&listed.stdout).to_string();
    assert!(
        !listed.contains(&run.to_string()),
        "git must no longer list the worktree: {listed}"
    );
}

#[test]
fn teardown_removes_a_copy_but_never_the_shared_repository() {
    let fixture = Fixture::new(test_project_id(10));
    let run = test_run_id(10);
    let path = fixture
        .materializer
        .materialize(run, IsolationKind::Copy)
        .unwrap();
    assert!(path.exists());

    fixture
        .materializer
        .teardown(&path, IsolationKind::Copy)
        .expect("teardown must remove a copy");
    assert!(!path.exists(), "the copied directory must be gone");

    // The single most important branch: a shared workspace's path *is* the
    // user's repository, and tearing it down must never delete it.
    fixture
        .materializer
        .teardown(&fixture.repo, IsolationKind::Shared)
        .expect("shared teardown must succeed");
    assert!(
        fixture.repo.join("README.md").exists(),
        "shared teardown must not touch the repository"
    );
}
