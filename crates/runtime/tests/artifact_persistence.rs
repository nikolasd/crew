//! Integration tests for artifact store persistence: a store opened with
//! on-disk storage must survive a daemon restart -- a journaled
//! `patch_artifact_id` is only useful if `workspace/apply` can still fetch
//! its patch from a *new* store handle over the same directory. Also
//! covers the total-bytes ceiling and on-disk tamper detection.

use crew_protocol::{ApplyRequest, ApplyStrategy, Artifact, ArtifactId, ArtifactKind, RunId};
use crew_runtime::workspace::{ArtifactStore, ArtifactStoreError, WorkspaceApplier};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

fn sha256_hex(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hex::encode(hasher.finalize())
}

/// Creates a real git repository with one committed file, for tests that
/// need `WorkspaceApplier` to run actual `git apply`/`git rev-parse`
/// against a real working tree -- mirrors `workspace_apply.rs`'s fixture.
fn create_fixture_repo() -> PathBuf {
    let repo = tempfile::TempDir::new()
        .expect("Failed to create temp dir")
        .keep();

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

fn head_of(repo: &PathBuf) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("Failed to get HEAD");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn patch_artifact(content: &[u8]) -> Artifact {
    Artifact {
        artifact_id: ArtifactId::new(),
        kind: ArtifactKind::Patch,
        sha256: sha256_hex(content),
        byte_length: content.len() as u64,
        media_type: "application/x-git-diff".to_string(),
        storage_path: format!("patches/{}.patch", content.len()),
        run_id: None,
    }
}

const UNBOUNDED: u64 = u64::MAX;

#[tokio::test]
async fn a_published_artifact_survives_a_store_restart() {
    let dir = tempfile::tempdir().unwrap();
    let content = b"diff --git a/file b/file\n+crew-restart-proof\n".to_vec();
    let artifact = patch_artifact(&content);
    let id = artifact.artifact_id;

    {
        let store = ArtifactStore::with_storage(dir.path().to_path_buf(), UNBOUNDED).unwrap();
        store.store(artifact, content.clone()).await.unwrap();
    } // the first daemon's handle is gone

    // A fresh handle over the same directory (the restarted daemon) must
    // find both the metadata and the exact content.
    let store = ArtifactStore::with_storage(dir.path().to_path_buf(), UNBOUNDED).unwrap();
    let metadata = store.fetch(&id).await.expect("metadata survives restart");
    assert_eq!(metadata.kind, ArtifactKind::Patch);
    let fetched = store
        .fetch_content(&id)
        .await
        .expect("content survives restart");
    assert_eq!(
        fetched, content,
        "apply must get the exact patch bytes back"
    );
}

#[tokio::test]
async fn apply_style_chunked_fetch_works_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let content = vec![b'x'; 1024];
    let artifact = patch_artifact(&content);
    let id = artifact.artifact_id;

    {
        let store = ArtifactStore::with_storage(dir.path().to_path_buf(), UNBOUNDED).unwrap();
        store.store(artifact, content.clone()).await.unwrap();
    }

    let store = ArtifactStore::with_storage(dir.path().to_path_buf(), UNBOUNDED).unwrap();
    let chunk = store.fetch_chunked(&id, 0, 4096).await.unwrap();
    assert!(chunk.complete);
    assert_eq!(chunk.artifact.byte_length, 1024);
}

#[tokio::test]
async fn list_returns_every_persisted_artifact_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = ArtifactStore::with_storage(dir.path().to_path_buf(), UNBOUNDED).unwrap();
        for content in [b"one".to_vec(), b"two".to_vec()] {
            store
                .store(patch_artifact(&content), content.clone())
                .await
                .unwrap();
        }
    }
    let store = ArtifactStore::with_storage(dir.path().to_path_buf(), UNBOUNDED).unwrap();
    let listed = store.list(None).await;
    assert_eq!(listed.artifacts.len(), 2);
}

#[tokio::test]
async fn tampered_on_disk_content_is_refused_not_served() {
    let dir = tempfile::tempdir().unwrap();
    let content = b"legitimate patch content".to_vec();
    let artifact = patch_artifact(&content);
    let sha = artifact.sha256.clone();
    let id = artifact.artifact_id;

    {
        let store = ArtifactStore::with_storage(dir.path().to_path_buf(), UNBOUNDED).unwrap();
        store.store(artifact, content).await.unwrap();
    }

    // Tamper with the content-addressed object on disk.
    let object = dir.path().join("objects").join(&sha);
    assert!(
        object.is_file(),
        "content must be stored at objects/<sha256>"
    );
    std::fs::write(&object, b"tampered bytes").unwrap();

    // The restarted store must refuse the artifact rather than serve
    // bytes that no longer hash to the recorded digest.
    let store = ArtifactStore::with_storage(dir.path().to_path_buf(), UNBOUNDED).unwrap();
    let result = store.fetch_content(&id).await;
    assert!(
        result.is_err(),
        "tampered content must never be served: {result:?}"
    );
}

#[tokio::test]
async fn the_total_bytes_ceiling_refuses_a_store_that_would_exceed_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtifactStore::with_storage(dir.path().to_path_buf(), 10).unwrap();

    let first = b"12345678".to_vec(); // 8 bytes: fits
    store
        .store(patch_artifact(&first), first.clone())
        .await
        .expect("under the ceiling");

    let second = b"87654321".to_vec(); // 8 more: would exceed 10
    let refused = store.store(patch_artifact(&second), second.clone()).await;
    assert!(
        matches!(refused, Err(ArtifactStoreError::Storage(_))),
        "a store past the ceiling must be refused with a typed error: {refused:?}"
    );

    // The refused artifact left nothing behind; the first is intact.
    let listed = store.list(None).await;
    assert_eq!(listed.artifacts.len(), 1);
}

#[tokio::test]
async fn a_purely_in_memory_store_still_works_unbounded() {
    let store = ArtifactStore::new();
    let content = b"in-memory only".to_vec();
    let artifact = patch_artifact(&content);
    let id = artifact.artifact_id;
    store.store(artifact, content.clone()).await.unwrap();
    assert_eq!(store.fetch_content(&id).await.unwrap(), content);
}

/// The proof this WP is named for: a `WorkspaceApplier` running against a
/// store handle that only ever saw the artifact through an on-disk
/// restart -- never the handle that published it -- can still fetch the
/// patch and apply it to a real git working tree. Publishing and applying
/// through two separate `ArtifactStore` instances over the same directory
/// is what stands in for "the daemon restarted between inspect and run".
#[tokio::test]
async fn workspace_applier_finds_its_patch_after_a_store_restart() {
    let dir = tempfile::tempdir().unwrap();
    let repo = create_fixture_repo();

    // A second clone is the source of the patch, exactly as in
    // `workspace_apply.rs`'s `workspace_apply_with_real_patch`.
    let source = tempfile::TempDir::new().unwrap().keep();
    std::fs::copy(repo.join("file1.txt"), source.join("file1.txt")).unwrap();
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test User"],
        vec!["add", "."],
        vec!["commit", "-m", "Initial"],
    ] {
        Command::new("git")
            .current_dir(&source)
            .args(&args)
            .output()
            .ok();
    }
    std::fs::write(source.join("file1.txt"), "restart-proof content\n").unwrap();
    let patch_output = Command::new("git")
        .current_dir(&source)
        .args(["diff"])
        .output()
        .expect("Failed to generate diff");
    let patch_content = patch_output.stdout;
    assert!(!patch_content.is_empty(), "patch should be nonempty");

    let artifact_id = ArtifactId::new();
    let artifact = Artifact {
        artifact_id,
        kind: ArtifactKind::Patch,
        sha256: sha256_hex(&patch_content),
        byte_length: patch_content.len() as u64,
        media_type: "application/x-git-diff".to_string(),
        storage_path: "restart.patch".to_string(),
        run_id: None,
    };

    {
        // The daemon before the restart: publish the patch through a
        // disk-backed store, then drop this handle entirely.
        let publishing_store =
            ArtifactStore::with_storage(dir.path().to_path_buf(), u64::MAX).unwrap();
        publishing_store
            .store(artifact, patch_content)
            .await
            .unwrap();
    } // `publishing_store` is gone -- nothing below can reach it.

    let expected_head = head_of(&repo);

    // The daemon after the restart: a brand-new store handle over the
    // same directory, with no relationship to the one that published.
    let restarted_store = ArtifactStore::with_storage(dir.path().to_path_buf(), u64::MAX).unwrap();
    let applier =
        WorkspaceApplier::from_store(repo.clone(), Arc::new(restarted_store), RunId::new());

    let request = ApplyRequest {
        lease_id: "restart-lease".to_string(),
        strategy: ApplyStrategy::ApplyPatch,
        artifact_id,
        expected_target_revision: expected_head,
        approval_correlation_id: None,
    };

    let result = applier
        .apply(&request)
        .await
        .expect("apply must run, not error, against a restarted store");
    assert!(
        result.success,
        "apply should succeed against the journaled artifact id: {result:?}"
    );

    let content = std::fs::read_to_string(repo.join("file1.txt")).unwrap();
    assert_eq!(content, "restart-proof content\n");
}
