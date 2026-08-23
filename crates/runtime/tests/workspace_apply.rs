//! Workspace apply integration tests.

use crew_protocol::RunId;
use crew_protocol::{ApplyRequest, ApplyStrategy, Artifact, ArtifactId, ArtifactKind, ProjectId};
use crew_runtime::workspace::{
    ARTIFACT_FETCH_MAX_BYTES, ArtifactStore, ArtifactStoreError, WorkspaceApplier,
    WorkspaceInspector,
};
use std::path::PathBuf;
use std::process::Command;

#[allow(dead_code)]
fn project_id() -> ProjectId {
    ProjectId::parse("01900000-0000-0000-0000-000000000001").unwrap()
}

/// Creates a fixture repository with sample files for testing.
fn create_fixture_repo() -> PathBuf {
    let repo = tempfile::TempDir::new()
        .expect("Failed to create temp dir")
        .keep();

    // Initialize as a git repository
    Command::new("git")
        .current_dir(&repo)
        .args(["init"])
        .output()
        .expect("Failed to initialize git repo");

    Command::new("git")
        .current_dir(&repo)
        .args(["config", "user.email", "test@test.com"])
        .output()
        .expect("Failed to configure git user");

    Command::new("git")
        .current_dir(&repo)
        .args(["config", "user.name", "Test User"])
        .output()
        .expect("Failed to configure git user");

    // Create initial file and commit
    std::fs::write(repo.join("file1.txt"), "initial content\n").unwrap();
    Command::new("git")
        .current_dir(&repo)
        .args(["add", "."])
        .output()
        .expect("Failed to add files");

    Command::new("git")
        .current_dir(&repo)
        .args(["commit", "-m", "Initial commit"])
        .output()
        .expect("Failed to commit");
    repo
}

/// Runs `git` in `repo`, returning its trimmed stdout. Panics on failure:
/// a broken fixture must not read as a legitimate test outcome.
fn git(repo: &PathBuf, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} failed to start: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn head_of(repo: &PathBuf) -> String {
    git(repo, &["rev-parse", "HEAD"])
}

/// A hex SHA-256 in the form `Artifact::sha256` carries.
fn digest(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    hex::encode(hasher.finalize())
}

fn patch_artifact(content: &[u8], sha256: String) -> Artifact {
    Artifact {
        artifact_id: ArtifactId::new(),
        kind: ArtifactKind::Patch,
        sha256,
        byte_length: content.len() as u64,
        media_type: "text/plain".to_string(),
        storage_path: "test.patch".to_string(),
        run_id: None,
    }
}

#[tokio::test]
async fn artifact_store_list_and_fetch() {
    let store = ArtifactStore::new();

    let content = b"test content".to_vec();
    let artifact = patch_artifact(&content, digest(&content));
    let id = store.store(artifact, content.clone()).await.unwrap();

    let list = store.list(None).await;
    assert_eq!(list.artifacts.len(), 1);
    assert_eq!(list.artifacts[0].artifact_id, id);

    let fetched = store.fetch(&id).await.unwrap();
    assert_eq!(fetched.artifact_id, id);
}

#[tokio::test]
async fn artifact_store_missing_artifact() {
    let store = ArtifactStore::new();
    let missing = ArtifactId::new();
    let result = store.fetch(&missing).await;
    assert!(result.is_err());
}

/// A publisher whose declared digest disagrees with its bytes is refused
/// at `store`, so the corrupt artifact never enters the store at all.
#[tokio::test]
async fn artifact_store_rejects_a_declared_digest_that_does_not_match_the_bytes() {
    let store = ArtifactStore::new();
    let content = b"test content".to_vec();
    let artifact = patch_artifact(&content, digest(b"different content"));

    let err = store
        .store(artifact, content)
        .await
        .expect_err("a mismatched digest must be refused at publish");
    assert!(
        matches!(err, ArtifactStoreError::DigestMismatch { .. }),
        "expected a digest mismatch, got {err:?}"
    );
    assert_eq!(
        store.list(None).await.artifacts.len(),
        0,
        "a refused artifact must not be stored"
    );
}

/// One `fetch_chunked` call never returns more than the ceiling, no matter
/// what length the caller asks for; the remainder is reachable by
/// following `next_offset`.
#[tokio::test]
async fn artifact_fetch_is_capped_regardless_of_requested_length() {
    let store = ArtifactStore::new();
    let content = vec![b'x'; (ARTIFACT_FETCH_MAX_BYTES as usize) + 4096];
    let artifact = patch_artifact(&content, digest(&content));
    let id = store.store(artifact, content.clone()).await.unwrap();

    let first = store.fetch_chunked(&id, 0, u64::MAX).await.unwrap();
    assert_eq!(
        first.next_offset,
        Some(ARTIFACT_FETCH_MAX_BYTES),
        "an over-large request must be clamped to the ceiling, not served whole"
    );
    assert!(!first.complete, "a clamped read is not a complete read");

    let second = store
        .fetch_chunked(&id, ARTIFACT_FETCH_MAX_BYTES, u64::MAX)
        .await
        .unwrap();
    assert_eq!(
        second.next_offset, None,
        "following next_offset must reach the end"
    );
    assert!(second.complete);
}

#[tokio::test]
async fn workspace_apply_with_real_patch() {
    let repo = create_fixture_repo();
    let store = ArtifactStore::new();

    // Create a second clone to modify (source of the patch)
    let source = tempfile::TempDir::new().unwrap().keep();
    std::fs::copy(repo.join("file1.txt"), source.join("file1.txt")).unwrap();
    Command::new("git")
        .current_dir(&source)
        .args(["init"])
        .output()
        .ok();
    Command::new("git")
        .current_dir(&source)
        .args(["config", "user.email", "test@test.com"])
        .output()
        .ok();
    Command::new("git")
        .current_dir(&source)
        .args(["config", "user.name", "Test User"])
        .output()
        .ok();
    Command::new("git")
        .current_dir(&source)
        .args(["add", "."])
        .output()
        .ok();
    Command::new("git")
        .current_dir(&source)
        .args(["commit", "-m", "Initial"])
        .output()
        .ok();

    // Modify the source and generate a patch
    std::fs::write(source.join("file1.txt"), "modified content\n").unwrap();
    let patch_output = Command::new("git")
        .current_dir(&source)
        .args(["diff"])
        .output()
        .expect("Failed to generate diff");

    let patch_content = patch_output.stdout;
    assert!(!patch_content.is_empty(), "patch should be nonempty");

    // Store the patch
    let artifact = Artifact {
        artifact_id: ArtifactId::new(),
        kind: ArtifactKind::Patch,
        sha256: digest(&patch_content),
        byte_length: patch_content.len() as u64,
        media_type: "application/x-git-diff".to_string(),
        storage_path: "test.patch".to_string(),
        run_id: None,
    };
    let artifact_id = store.store(artifact, patch_content).await.unwrap();

    // Get the current HEAD before applying
    let head_output = Command::new("git")
        .current_dir(&repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("Failed to get HEAD");
    let expected_head = String::from_utf8_lossy(&head_output.stdout)
        .trim()
        .to_string();

    let applier = WorkspaceApplier::from_store(
        repo.clone(),
        std::sync::Arc::new(store.clone()),
        RunId::new(),
    );

    let request = ApplyRequest {
        lease_id: "test-lease".to_string(),
        strategy: ApplyStrategy::ApplyPatch,
        artifact_id,
        expected_target_revision: expected_head,
        approval_correlation_id: None,
    };

    let result = applier.apply(&request).await.unwrap();
    assert!(result.success, "apply should succeed");

    // Verify the file was modified
    let content = std::fs::read_to_string(repo.join("file1.txt")).unwrap();
    assert_eq!(content, "modified content\n");
}

/// A patch that does not apply is a conflict, not an internal error: the
/// caller gets `success: false`, a `CONFLICT` code, and a stored report.
#[tokio::test]
async fn workspace_apply_patch_conflict_records_an_artifact_and_never_errors() {
    let repo = create_fixture_repo();
    let store = ArtifactStore::new();

    // A patch whose pre-image ("something else\n") does not match the
    // repo's actual file1.txt, so `git apply --check` rejects it.
    let patch_content = b"diff --git a/file1.txt b/file1.txt\n\
index 0000000..1111111 100644\n\
--- a/file1.txt\n\
+++ b/file1.txt\n\
@@ -1 +1 @@\n\
-something else\n\
+replacement\n"
        .to_vec();
    let artifact = Artifact {
        artifact_id: ArtifactId::new(),
        kind: ArtifactKind::Patch,
        sha256: digest(&patch_content),
        byte_length: patch_content.len() as u64,
        media_type: "application/x-git-diff".to_string(),
        storage_path: "conflict.patch".to_string(),
        run_id: None,
    };
    let artifact_id = store.store(artifact, patch_content).await.unwrap();

    let head = head_of(&repo);
    let applier = WorkspaceApplier::from_store(
        repo.clone(),
        std::sync::Arc::new(store.clone()),
        RunId::new(),
    );
    let result = applier
        .apply(&ApplyRequest {
            lease_id: "conflict-lease".to_string(),
            strategy: ApplyStrategy::ApplyPatch,
            artifact_id,
            expected_target_revision: head,
            approval_correlation_id: None,
        })
        .await
        .expect("a conflict must be a result, not an Err");

    assert!(!result.success);
    assert_eq!(result.error_code.as_deref(), Some("CONFLICT"));
    let conflict_id = result
        .conflict_artifact_id
        .expect("a conflict must record a report artifact");
    let report = store.fetch(&conflict_id).await.unwrap();
    assert_eq!(report.kind, ArtifactKind::ConflictReport);

    // `--check` runs before any mutation, so the tree is untouched.
    assert_eq!(
        std::fs::read_to_string(repo.join("file1.txt")).unwrap(),
        "initial content\n"
    );
}

/// A conflicting cherry-pick aborts, so the workspace is left usable, and
/// still reports the conflict with a stored report.
#[tokio::test]
async fn workspace_cherry_pick_conflict_aborts_and_records_an_artifact() {
    let repo = create_fixture_repo();
    let store = ArtifactStore::new();

    // Branch off and commit a conflicting change, then diverge on the
    // original branch so the pick cannot apply cleanly.
    let branch = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    git(&repo, &["checkout", "-b", "side"]);
    std::fs::write(repo.join("file1.txt"), "side content\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "side change"]);
    let side_commit = git(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", &branch]);
    std::fs::write(repo.join("file1.txt"), "main content\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "main change"]);

    let content = format!("{side_commit}\n").into_bytes();
    let artifact = Artifact {
        artifact_id: ArtifactId::new(),
        kind: ArtifactKind::Patch,
        sha256: digest(&content),
        byte_length: content.len() as u64,
        media_type: "text/plain".to_string(),
        storage_path: "commits.txt".to_string(),
        run_id: None,
    };
    let artifact_id = store.store(artifact, content).await.unwrap();

    let head = head_of(&repo);
    let applier = WorkspaceApplier::from_store(
        repo.clone(),
        std::sync::Arc::new(store.clone()),
        RunId::new(),
    );
    let result = applier
        .apply(&ApplyRequest {
            lease_id: "pick-lease".to_string(),
            strategy: ApplyStrategy::CherryPick,
            artifact_id,
            expected_target_revision: head,
            approval_correlation_id: None,
        })
        .await
        .expect("a conflict must be a result, not an Err");

    assert!(!result.success);
    assert_eq!(result.error_code.as_deref(), Some("CONFLICT"));
    let conflict_id = result
        .conflict_artifact_id
        .expect("a conflict must record a report artifact");
    assert_eq!(
        store.fetch(&conflict_id).await.unwrap().kind,
        ArtifactKind::ConflictReport
    );

    // The abort is what makes the workspace reusable: git no longer
    // reports an in-progress cherry-pick, and no conflict markers remain.
    assert!(
        !repo.join(".git/CHERRY_PICK_HEAD").exists(),
        "the failed cherry-pick must be aborted"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("file1.txt")).unwrap(),
        "main content\n",
        "abort must restore the pre-pick tree"
    );
}

#[tokio::test]
async fn workspace_apply_stale_revision_returns_conflict() {
    let repo = create_fixture_repo();
    let store = ArtifactStore::new();

    // Create a second clone to generate the patch from (source)
    let source = tempfile::TempDir::new().unwrap().keep();
    std::fs::copy(repo.join("file1.txt"), source.join("file1.txt")).unwrap();
    Command::new("git")
        .current_dir(&source)
        .args(["init"])
        .output()
        .ok();
    Command::new("git")
        .current_dir(&source)
        .args(["config", "user.email", "test@test.com"])
        .output()
        .ok();
    Command::new("git")
        .current_dir(&source)
        .args(["config", "user.name", "Test User"])
        .output()
        .ok();
    Command::new("git")
        .current_dir(&source)
        .args(["add", "."])
        .output()
        .ok();
    Command::new("git")
        .current_dir(&source)
        .args(["commit", "-m", "Initial"])
        .output()
        .ok();

    // Modify the source and generate a patch
    std::fs::write(source.join("file1.txt"), "modified content\n").unwrap();
    let patch_output = Command::new("git")
        .current_dir(&source)
        .args(["diff"])
        .output()
        .expect("Failed to generate diff");

    let artifact = Artifact {
        artifact_id: ArtifactId::new(),
        kind: ArtifactKind::Patch,
        sha256: digest(&patch_output.stdout),
        byte_length: patch_output.stdout.len() as u64,
        media_type: "application/x-git-diff".to_string(),
        storage_path: "test.patch".to_string(),
        run_id: None,
    };
    let artifact_id = store.store(artifact, patch_output.stdout).await.unwrap();

    // Use a STALE revision (not the current HEAD)
    let stale_revision = "0000000000000000000000000000000000000000";

    let applier = WorkspaceApplier::from_store(
        repo.clone(),
        std::sync::Arc::new(store.clone()),
        RunId::new(),
    );

    let request = ApplyRequest {
        lease_id: "test-lease".to_string(),
        strategy: ApplyStrategy::ApplyPatch,
        artifact_id,
        expected_target_revision: stale_revision.to_string(),
        approval_correlation_id: None,
    };

    let result = applier.apply(&request).await.unwrap();
    assert!(!result.success, "apply should fail with stale revision");
    assert_eq!(result.error_code.as_deref(), Some("STALE_REVISION"));

    // Verify the workspace was NOT mutated
    let content = std::fs::read_to_string(repo.join("file1.txt")).unwrap();
    assert_eq!(content, "initial content\n");
}

#[tokio::test]
async fn workspace_inspect_captures_real_evidence() {
    let repo = create_fixture_repo();
    let store = ArtifactStore::new();

    // Modify a file to create dirty state
    std::fs::write(repo.join("file1.txt"), "dirty content\n").unwrap();

    // Create an untracked file
    std::fs::write(repo.join("untracked.txt"), "untracked\n").unwrap();

    let inspector = WorkspaceInspector::with_store(
        repo.clone(),
        std::sync::Arc::new(store.clone()),
        RunId::new(),
    );

    let request = crew_protocol::InspectRequest {
        lease_id: "test-lease".to_string(),
    };

    let result = inspector.inspect(&request).await.unwrap();

    // Verify real evidence was captured
    assert_eq!(result.lease_id, "test-lease");
    assert!(result.dirty_file_count > 0, "should have dirty files");
    assert!(
        result.untracked_file_count > 0,
        "should have untracked files"
    );
    assert!(!result.commit_ids.is_empty(), "should have commits");
    assert!(
        !result.base_revision.is_empty(),
        "should have base revision"
    );
    assert!(
        !result.patch_artifact_id.to_string().is_empty(),
        "should have patch artifact ID"
    );

    // Verify the patch was stored
    let list = store.list(None).await;
    assert!(!list.artifacts.is_empty(), "should have stored artifacts");
}

/// R36: the isolation tests above hand-seed `run_id` on their input
/// fixtures; nothing proved the real producers stamp it. Reverting the
/// production stamping (`inspect.rs`/`apply.rs`) to `run_id: None` left
/// the whole suite green -- proven by doing exactly that scratch revert
/// while writing these tests and watching them fail. These drive the
/// real `WorkspaceInspector` and `WorkspaceApplier` end-to-end and read
/// the persisted artifact rows back.
#[tokio::test]
async fn inspector_stamps_the_producing_run_id_on_its_patch_artifact() {
    let repo = create_fixture_repo();
    let store = ArtifactStore::new();
    let run_id = RunId::new();

    std::fs::write(repo.join("file1.txt"), "dirty content\n").unwrap();
    let inspector =
        WorkspaceInspector::with_store(repo.clone(), std::sync::Arc::new(store.clone()), run_id);
    let result = inspector
        .inspect(&crew_protocol::InspectRequest {
            lease_id: "lease-r36".to_string(),
        })
        .await
        .unwrap();

    let stored = store.fetch(&result.patch_artifact_id).await.unwrap();
    assert_eq!(
        stored.run_id.as_deref(),
        Some(run_id.to_string().as_str()),
        "the persisted patch artifact must carry the producing run's id"
    );
}

#[tokio::test]
async fn applier_stamps_the_producing_run_id_on_its_conflict_artifact() {
    let repo = create_fixture_repo();
    let store = ArtifactStore::new();
    let run_id = RunId::new();
    let head = head_of(&repo);

    // A patch that cannot apply: it edits content the tree does not have.
    let conflicting_patch = b"diff --git a/file1.txt b/file1.txt\n\
index 0000000..1111111 100644\n\
--- a/file1.txt\n\
+++ b/file1.txt\n\
@@ -1 +1 @@\n\
-content that was never there\n\
+replacement\n"
        .to_vec();
    let artifact_id = ArtifactId::new();
    let artifact = Artifact {
        artifact_id,
        kind: ArtifactKind::Patch,
        sha256: digest(&conflicting_patch),
        byte_length: conflicting_patch.len() as u64,
        media_type: "application/x-git-diff".to_string(),
        storage_path: "patches/conflict.patch".to_string(),
        run_id: None,
    };
    store.store(artifact, conflicting_patch).await.unwrap();

    let applier =
        WorkspaceApplier::from_store(repo.clone(), std::sync::Arc::new(store.clone()), run_id);
    let result = applier
        .apply(&ApplyRequest {
            lease_id: "lease-r36".to_string(),
            strategy: ApplyStrategy::ApplyPatch,
            artifact_id,
            expected_target_revision: head,
            approval_correlation_id: None,
        })
        .await
        .unwrap();

    assert!(!result.success);
    let conflict_id = result
        .conflict_artifact_id
        .expect("a conflict must record a report artifact");
    let stored = store.fetch(&conflict_id).await.unwrap();
    assert_eq!(stored.kind, ArtifactKind::ConflictReport);
    assert_eq!(
        stored.run_id.as_deref(),
        Some(run_id.to_string().as_str()),
        "the persisted conflict artifact must carry the producing run's id"
    );
}
