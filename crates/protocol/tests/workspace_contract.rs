//! Strict contract tests for workspace and artifact wire types.
//!
//! Proves:
//! - Lease modes `readOnly` and `write`
//! - Isolation kinds `shared`, `gitWorktree`, `copy`
//! - States `allocating`, `active`, `dirty`, `released`, `cleanupFailed`
//! - Every lease carries path, canonical repository root, base revision,
//!   owner run ID, and acquisition sequence
//! - Artifacts carry SHA-256, byte length, media type, and immutable
//!   relative storage path

use crew_protocol::{
    ApplyRequest, ApplyResult, ApplyStrategy, Artifact, ArtifactFetchRequest, ArtifactFetchResult,
    ArtifactId, ArtifactKind, ArtifactListRequest, ArtifactListResult, InspectRequest,
    InspectResult, IsolationKind, LeaseMode, LeaseRequest, ProjectId, ReleaseRequest, RunId,
    WorkspaceEvent, WorkspaceInfo, WorkspaceLease, WorkspaceState,
};
use serde_json::json;

// ------------------------------------------------------------------ helpers

fn serialise<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).expect("serializable")
}

fn deserialise<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).expect("deserializable")
}

// ------------------------------------------------------------------ fixtures

fn sample_lease() -> WorkspaceLease {
    WorkspaceLease {
        lease_id: "ws-001".to_string(),
        project_id: ProjectId::parse("01900000-0000-0000-0000-000000000000").unwrap(),
        run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
        mode: LeaseMode::Write,
        isolation_kind: IsolationKind::GitWorktree,
        path: "/tmp/crew-workspace-abc123".to_string(),
        canonical_repository_root: "/tmp/crew-smoke".to_string(),
        base_revision: "abc123def456".to_string(),
        state: WorkspaceState::Active,
        acquired_at: "2026-07-22T00:00:00Z".to_string(),
        released_at: None,
        acquisition_sequence: 1,
    }
}

fn sample_artifact() -> Artifact {
    Artifact {
        artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000010").unwrap(),
        kind: ArtifactKind::Patch,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca4959e13ae37a1ff".to_string(),
        byte_length: 0,
        media_type: "text/x-patch".to_string(),
        storage_path:
            "artifacts/sha256/e3/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca4959e13ae37a1ff"
                .to_string(),
        run_id: Some("01900000-0000-0000-0000-000000000001".to_string()),
    }
}

// ------------------------------------------------------------------ tests

#[test]
fn lease_mode_serializes_as_camel_case() {
    let val = serialise(&LeaseMode::ReadOnly);
    assert_eq!(val.as_str().unwrap(), "readOnly");

    let val = serialise(&LeaseMode::Write);
    assert_eq!(val.as_str().unwrap(), "write");
}

#[test]
fn isolation_kind_serializes_as_camel_case() {
    let val = serialise(&IsolationKind::Shared);
    assert_eq!(val.as_str().unwrap(), "shared");

    let val = serialise(&IsolationKind::GitWorktree);
    assert_eq!(val.as_str().unwrap(), "gitWorktree");

    let val = serialise(&IsolationKind::Copy);
    assert_eq!(val.as_str().unwrap(), "copy");
}

#[test]
fn workspace_state_serializes_as_camel_case() {
    for (variant, expected) in [
        (WorkspaceState::Allocating, "allocating"),
        (WorkspaceState::Active, "active"),
        (WorkspaceState::Dirty, "dirty"),
        (WorkspaceState::Released, "released"),
        (WorkspaceState::CleanupFailed, "cleanupFailed"),
    ] {
        let val = serialise(&variant);
        assert_eq!(val.as_str().unwrap(), expected);
    }
}

#[test]
fn decided_by_as_str_matches_its_serde_tokens() {
    // R34's fix hand-mirrors the serde rename in `DecidedBy::as_str`; a
    // future `#[serde(rename = ...)]` silently re-creates R34 unless the
    // two are pinned together.
    use crew_protocol::DecidedBy;
    for variant in [DecidedBy::Human, DecidedBy::Model] {
        assert_eq!(serialise(&variant).as_str().unwrap(), variant.as_str());
    }
}

#[test]
fn lease_contract_has_required_fields() {
    let lease = sample_lease();
    let val = serialise(&lease);
    let obj = val.as_object().expect("lease is an object");

    assert!(obj.contains_key("leaseId"));
    assert!(obj.contains_key("projectId"));
    assert!(obj.contains_key("runId"));
    assert!(obj.contains_key("mode"));
    assert!(obj.contains_key("isolationKind"));
    assert!(obj.contains_key("path"));
    assert!(obj.contains_key("canonicalRepositoryRoot"));
    assert!(obj.contains_key("baseRevision"));
    assert!(obj.contains_key("state"));
    assert!(obj.contains_key("acquiredAt"));
    assert!(obj.contains_key("releasedAt"));
    assert!(obj.contains_key("acquisitionSequence"));
}

#[test]
fn lease_deserializes_unknown_fields_fails() {
    let raw = json!({
        "leaseId": "ws-001",
        "projectId": "01900000-0000-0000-0000-000000000000",
        "runId": "01900000-0000-0000-0000-000000000001",
        "mode": "write",
        "isolationKind": "gitWorktree",
        "path": "/tmp/ws",
        "canonicalRepositoryRoot": "/tmp/repo",
        "baseRevision": "abc123",
        "state": "active",
        "acquiredAt": "2026-07-22T00:00:00Z",
        "releasedAt": null,
        "acquisitionSequence": 1,
        "unknownField": "should fail"
    });
    let result: Result<WorkspaceLease, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "deny_unknown_fields should reject unknown fields"
    );
}

#[test]
fn lease_request_contract() {
    let req = LeaseRequest {
        run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
        mode: LeaseMode::Write,
        requested_isolation: Some(IsolationKind::GitWorktree),
    };
    let val = serialise(&req);
    assert!(val.as_object().unwrap().contains_key("runId"));
    assert!(val.as_object().unwrap().contains_key("mode"));
    assert!(val.as_object().unwrap().contains_key("requestedIsolation"));
}

#[test]
fn workspace_info_contract() {
    let info = WorkspaceInfo {
        lease_id: "ws-001".to_string(),
        run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
        mode: LeaseMode::ReadOnly,
        isolation_kind: IsolationKind::Shared,
        path: "/tmp/crew-workspace-xyz".to_string(),
        state: WorkspaceState::Active,
        base_revision: "def456".to_string(),
    };
    let val = serialise(&info);
    let obj = val.as_object().unwrap();
    assert!(obj.contains_key("leaseId"));
    assert!(obj.contains_key("path"));
    assert!(obj.contains_key("state"));
    assert_eq!(
        obj.get("mode").and_then(|v| v.as_str()).unwrap(),
        "readOnly"
    );
}

#[test]
fn inspect_request_and_result_contracts() {
    let req = InspectRequest {
        lease_id: "ws-001".to_string(),
    };
    let val = serialise(&req);
    assert!(val.as_object().unwrap().contains_key("leaseId"));

    let result = InspectResult {
        lease_id: "ws-001".to_string(),
        patch_artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000020").unwrap(),
        commit_count: 3,
        commit_ids: vec!["abc".to_string(), "def".to_string(), "ghi".to_string()],
        dirty_file_count: 2,
        untracked_file_count: 1,
        base_revision: "abc123".to_string(),
        current_revision: Some("def456".to_string()),
    };
    let val = serialise(&result);
    let obj = val.as_object().unwrap();
    assert!(obj.contains_key("patchArtifactId"));
    assert!(obj.contains_key("commitCount"));
    assert!(obj.contains_key("commitIds"));
    assert!(obj.contains_key("dirtyFileCount"));
    assert!(obj.contains_key("untrackedFileCount"));
}

#[test]
fn apply_request_and_result_contracts() {
    let req = ApplyRequest {
        lease_id: "ws-001".to_string(),
        strategy: ApplyStrategy::ApplyPatch,
        artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000030").unwrap(),
        expected_target_revision: "abc123".to_string(),
        approval_correlation_id: Some("corr-001".to_string()),
    };
    let val = serialise(&req);
    let obj = val.as_object().unwrap();
    assert!(obj.contains_key("strategy"));
    assert_eq!(
        obj.get("strategy").and_then(|v| v.as_str()).unwrap(),
        "applyPatch"
    );
    assert!(obj.contains_key("artifactId"));
    assert!(obj.contains_key("expectedTargetRevision"));
    assert!(obj.contains_key("approvalCorrelationId"));

    let result = ApplyResult {
        lease_id: "ws-001".to_string(),
        success: false,
        conflict_artifact_id: Some(
            ArtifactId::parse("01900000-0000-0000-0000-000000000040").unwrap(),
        ),
        target_revision_after: None,
        error_code: Some("staleTarget".to_string()),
    };
    let val = serialise(&result);
    let obj = val.as_object().unwrap();
    assert!(obj.contains_key("success"));
    assert!(obj.contains_key("conflictArtifactId"));
    assert!(obj.contains_key("errorCode"));
}

#[test]
fn apply_strategy_serializes_as_camel_case() {
    assert_eq!(
        serialise(&ApplyStrategy::ApplyPatch).as_str().unwrap(),
        "applyPatch"
    );
    assert_eq!(
        serialise(&ApplyStrategy::CherryPick).as_str().unwrap(),
        "cherryPick"
    );
}

#[test]
fn release_request_contract() {
    let req = ReleaseRequest {
        lease_id: "ws-001".to_string(),
    };
    let val = serialise(&req);
    assert!(val.as_object().unwrap().contains_key("leaseId"));
}

// ------------------------------------------------------------------ workspace events

#[test]
fn workspace_event_lease_requested() {
    let event = WorkspaceEvent::LeaseRequested {
        lease_id: "ws-001".to_string(),
        run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
        mode: LeaseMode::Write,
    };
    let val = serialise(&event);
    let obj = val.as_object().expect("workspace event is an object");
    // Adjacently tagged: { "type": "...", "payload": { ... } }
    assert!(obj.contains_key("type"));
    assert_eq!(
        obj.get("type").and_then(|v| v.as_str()).unwrap(),
        "leaseRequested"
    );
    let payload = obj.get("payload").expect("has payload");
    assert!(payload.as_object().unwrap().contains_key("leaseId"));
    assert!(payload.as_object().unwrap().contains_key("runId"));
    assert!(payload.as_object().unwrap().contains_key("mode"));
}

#[test]
fn workspace_event_lease_acquired() {
    let event = WorkspaceEvent::LeaseAcquired {
        lease_id: "ws-001".to_string(),
        run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
        path: "/tmp/crew-workspace-xyz".to_string(),
        isolation_kind: IsolationKind::GitWorktree,
        base_revision: "abc123".to_string(),
    };
    let val = serialise(&event);
    let obj = val.as_object().unwrap();
    assert_eq!(
        obj.get("type").and_then(|v| v.as_str()).unwrap(),
        "leaseAcquired"
    );
    let payload = obj.get("payload").unwrap();
    assert!(payload.as_object().unwrap().contains_key("path"));
    assert!(payload.as_object().unwrap().contains_key("isolationKind"));
}

#[test]
fn workspace_event_workspace_dirty() {
    let event = WorkspaceEvent::WorkspaceDirty {
        lease_id: "ws-001".to_string(),
        dirty_file_count: 5,
        untracked_file_count: 2,
    };
    let val = serialise(&event);
    assert_eq!(
        val.as_object()
            .unwrap()
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap(),
        "workspaceDirty"
    );
}

#[test]
fn workspace_event_workspace_inspected() {
    let event = WorkspaceEvent::WorkspaceInspected {
        lease_id: "ws-001".to_string(),
        patch_artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000020").unwrap(),
        commit_count: 3,
        dirty_file_count: 1,
        untracked_file_count: 0,
    };
    let val = serialise(&event);
    assert_eq!(
        val.as_object()
            .unwrap()
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap(),
        "workspaceInspected"
    );
}

#[test]
fn workspace_event_apply_started() {
    let event = WorkspaceEvent::ApplyStarted {
        lease_id: "ws-001".to_string(),
        strategy: ApplyStrategy::CherryPick,
        artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000030").unwrap(),
        expected_target_revision: "abc123".to_string(),
    };
    let val = serialise(&event);
    assert_eq!(
        val.as_object()
            .unwrap()
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap(),
        "applyStarted"
    );
}

#[test]
fn workspace_event_apply_completed() {
    let event = WorkspaceEvent::ApplyCompleted {
        lease_id: "ws-001".to_string(),
        success: true,
        conflict_artifact_id: None,
        target_revision_after: Some("def456".to_string()),
    };
    let val = serialise(&event);
    assert_eq!(
        val.as_object()
            .unwrap()
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap(),
        "applyCompleted"
    );
}

#[test]
fn workspace_event_lease_released() {
    let event = WorkspaceEvent::LeaseReleased {
        lease_id: "ws-001".to_string(),
        run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
    };
    let val = serialise(&event);
    assert_eq!(
        val.as_object()
            .unwrap()
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap(),
        "leaseReleased"
    );
}

#[test]
fn workspace_event_cleanup_failed() {
    let event = WorkspaceEvent::CleanupFailed {
        lease_id: "ws-001".to_string(),
        error: "path not owned by lease".to_string(),
    };
    let val = serialise(&event);
    assert_eq!(
        val.as_object()
            .unwrap()
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap(),
        "cleanupFailed"
    );
    assert!(
        val.as_object()
            .unwrap()
            .get("payload")
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("error")
    );
}

// ------------------------------------------------------------------ artifact contracts

#[test]
fn artifact_kind_serializes_as_camel_case() {
    for (variant, expected) in [
        (ArtifactKind::Patch, "patch"),
        (ArtifactKind::CommitList, "commitList"),
        (ArtifactKind::ConflictReport, "conflictReport"),
        (ArtifactKind::WorkspaceManifest, "workspaceManifest"),
    ] {
        let val = serialise(&variant);
        assert_eq!(val.as_str().unwrap(), expected);
    }
}

#[test]
fn artifact_contract_has_required_fields() {
    let art = sample_artifact();
    let val = serialise(&art);
    let obj = val.as_object().expect("artifact is an object");

    assert!(obj.contains_key("artifactId"));
    assert!(obj.contains_key("kind"));
    assert!(obj.contains_key("sha256"));
    assert!(obj.contains_key("byteLength"));
    assert!(obj.contains_key("mediaType"));
    assert!(obj.contains_key("storagePath"));
    assert!(obj.contains_key("runId"));
}

#[test]
fn artifact_deserializes_unknown_fields_fails() {
    let raw = json!({
        "artifactId": "01900000-0000-0000-0000-000000000010",
        "kind": "patch",
        "sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca4959e13ae37a1ff",
        "byteLength": 0,
        "mediaType": "text/x-patch",
        "storagePath": "artifacts/sha256/e3/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca4959e13ae37a1ff",
        "runId": null,
        "unknownField": "should fail"
    });
    let result: Result<Artifact, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "deny_unknown_fields should reject unknown fields"
    );
}

#[test]
fn artifact_list_request_contract() {
    let req = ArtifactListRequest {
        project_id: "my-project".to_string(),
        kind: Some(ArtifactKind::Patch),
    };
    let val = serialise(&req);
    let obj = val.as_object().unwrap();
    assert!(obj.contains_key("projectId"));
    assert!(obj.contains_key("kind"));
}

#[test]
fn artifact_list_result_contract() {
    let result = ArtifactListResult {
        artifacts: vec![sample_artifact()],
    };
    let val = serialise(&result);
    assert!(val.as_object().unwrap().contains_key("artifacts"));
}

#[test]
fn artifact_fetch_request_contract() {
    let req = ArtifactFetchRequest {
        artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000010").unwrap(),
        offset: 0,
        length: 1024,
    };
    let val = serialise(&req);
    let obj = val.as_object().unwrap();
    assert!(obj.contains_key("artifactId"));
    assert!(obj.contains_key("offset"));
    assert!(obj.contains_key("length"));
}

#[test]
fn artifact_fetch_result_contract() {
    let result = ArtifactFetchResult {
        artifact: sample_artifact(),
        content_base64: "ZGlmZiAtLSBnaXQgYS9maWxlIGIvZmlsZQpuLi4u".to_string(),
        next_offset: Some(1024),
        complete: false,
    };
    let val = serialise(&result);
    let obj = val.as_object().unwrap();
    assert!(obj.contains_key("artifact"));
    assert!(obj.contains_key("contentBase64"));
    assert!(obj.contains_key("nextOffset"));
    assert!(obj.contains_key("complete"));
}

// ------------------------------------------------------------------ round-trip

#[test]
fn lease_round_trips() {
    let original = sample_lease();
    let json = serde_json::to_string(&original).expect("serializable");
    let parsed: WorkspaceLease = serde_json::from_str(&json).expect("deserializable");
    assert_eq!(original, parsed);
}

#[test]
fn artifact_round_trips() {
    let original = sample_artifact();
    let json = serde_json::to_string(&original).expect("serializable");
    let parsed: Artifact = serde_json::from_str(&json).expect("deserializable");
    assert_eq!(original, parsed);
}

#[test]
fn workspace_event_round_trips() {
    let events: Vec<WorkspaceEvent> = vec![
        WorkspaceEvent::LeaseRequested {
            lease_id: "ws-001".to_string(),
            run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
            mode: LeaseMode::Write,
        },
        WorkspaceEvent::LeaseAcquired {
            lease_id: "ws-001".to_string(),
            run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
            path: "/tmp/ws".to_string(),
            isolation_kind: IsolationKind::Copy,
            base_revision: "abc".to_string(),
        },
        WorkspaceEvent::WorkspaceDirty {
            lease_id: "ws-001".to_string(),
            dirty_file_count: 3,
            untracked_file_count: 1,
        },
        WorkspaceEvent::WorkspaceInspected {
            lease_id: "ws-001".to_string(),
            patch_artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000020").unwrap(),
            commit_count: 5,
            dirty_file_count: 2,
            untracked_file_count: 0,
        },
        WorkspaceEvent::ApplyStarted {
            lease_id: "ws-001".to_string(),
            strategy: ApplyStrategy::ApplyPatch,
            artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000030").unwrap(),
            expected_target_revision: "abc123".to_string(),
        },
        WorkspaceEvent::ApplyCompleted {
            lease_id: "ws-001".to_string(),
            success: true,
            conflict_artifact_id: None,
            target_revision_after: Some("def456".to_string()),
        },
        WorkspaceEvent::LeaseReleased {
            lease_id: "ws-001".to_string(),
            run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
        },
        WorkspaceEvent::CleanupFailed {
            lease_id: "ws-001".to_string(),
            error: "cleanup error".to_string(),
        },
    ];
    for event in events {
        let json = serde_json::to_string(&event).expect("serializable");
        let parsed: WorkspaceEvent = serde_json::from_str(&json).expect("deserializable");
        assert_eq!(
            event,
            parsed,
            "event type mismatch for {}",
            event_type_name(&event)
        );
    }
}

fn event_type_name(event: &WorkspaceEvent) -> &'static str {
    match event {
        WorkspaceEvent::LeaseRequested { .. } => "leaseRequested",
        WorkspaceEvent::LeaseAcquired { .. } => "leaseAcquired",
        WorkspaceEvent::WorkspaceDirty { .. } => "workspaceDirty",
        WorkspaceEvent::WorkspaceInspected { .. } => "workspaceInspected",
        WorkspaceEvent::ApplyStarted { .. } => "applyStarted",
        WorkspaceEvent::ApplyCompleted { .. } => "applyCompleted",
        WorkspaceEvent::LeaseReleased { .. } => "leaseReleased",
        WorkspaceEvent::CleanupFailed { .. } => "cleanupFailed",
        WorkspaceEvent::ArtifactPublished { .. } => "artifactPublished",
        WorkspaceEvent::ApplyConflict { .. } => "applyConflict",
    }
}

// ------------------------------------------------------------------ CrewMethod workspace/* and artifact/* strings

#[test]
fn workspace_method_strings_serialize_correctly() {
    use crew_protocol::CrewMethod;
    let methods: Vec<(CrewMethod, &str)> = vec![
        (CrewMethod::WorkspaceAcquire, "workspace/acquire"),
        (CrewMethod::WorkspaceGet, "workspace/get"),
        (CrewMethod::WorkspaceRelease, "workspace/release"),
        (CrewMethod::WorkspaceInspect, "workspace/inspect"),
        (CrewMethod::WorkspaceApply, "workspace/apply"),
        (CrewMethod::ArtifactList, "artifact/list"),
        (CrewMethod::ArtifactFetch, "artifact/fetch"),
    ];
    for (method, expected) in methods {
        let json = serde_json::to_value(method).expect("serializable");
        assert_eq!(
            json.as_str().unwrap(),
            expected,
            "method {:?} should serialize as '{}'",
            method,
            expected
        );
    }
}

// ------------------------------------------------------------------ RuntimeEvent::WorkspaceEvent serialization

#[test]
fn runtime_event_workspace_event_serializes() {
    use crew_protocol::RuntimeEvent;
    let ws_event = RuntimeEvent::WorkspaceEvent {
        kind: crew_protocol::WorkspaceEvent::LeaseRequested {
            lease_id: "ws-001".to_string(),
            run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
            mode: LeaseMode::Write,
        },
        run_id: RunId::parse("01900000-0000-0000-0000-000000000001").unwrap(),
        lease_id: "ws-001".to_string(),
    };
    let json = serde_json::to_string(&ws_event).expect("serializable");
    // Verify it round-trips through RuntimeEvent
    let parsed: RuntimeEvent = serde_json::from_str(&json).expect("deserializable");
    match parsed {
        RuntimeEvent::WorkspaceEvent { .. } => {}
        other => panic!("expected WorkspaceEvent, got {:?}", other),
    }
}

#[test]
fn runtime_event_workspace_event_lease_acquired_serializes() {
    use crew_protocol::RuntimeEvent;
    let ws_event = RuntimeEvent::WorkspaceEvent {
        kind: crew_protocol::WorkspaceEvent::LeaseAcquired {
            lease_id: "ws-002".to_string(),
            run_id: RunId::parse("01900000-0000-0000-0000-000000000002").unwrap(),
            path: "/tmp/ws-002".to_string(),
            isolation_kind: IsolationKind::GitWorktree,
            base_revision: "abc123".to_string(),
        },
        run_id: RunId::parse("01900000-0000-0000-0000-000000000002").unwrap(),
        lease_id: "ws-002".to_string(),
    };
    let json = serde_json::to_string(&ws_event).expect("serializable");
    let parsed: RuntimeEvent = serde_json::from_str(&json).expect("deserializable");
    match parsed {
        RuntimeEvent::WorkspaceEvent { .. } => {}
        other => panic!("expected WorkspaceEvent, got {:?}", other),
    }
}

#[test]
fn runtime_event_workspace_event_apply_completed_serializes() {
    use crew_protocol::RuntimeEvent;
    let ws_event = RuntimeEvent::WorkspaceEvent {
        kind: crew_protocol::WorkspaceEvent::ApplyCompleted {
            lease_id: "ws-003".to_string(),
            success: true,
            conflict_artifact_id: None,
            target_revision_after: Some("def456".to_string()),
        },
        run_id: RunId::parse("01900000-0000-0000-0000-000000000003").unwrap(),
        lease_id: "ws-003".to_string(),
    };
    let json = serde_json::to_string(&ws_event).expect("serializable");
    let parsed: RuntimeEvent = serde_json::from_str(&json).expect("deserializable");
    match parsed {
        RuntimeEvent::WorkspaceEvent { .. } => {}
        other => panic!("expected WorkspaceEvent, got {:?}", other),
    }
}

#[test]
fn runtime_event_workspace_event_artifact_published_serializes() {
    use crew_protocol::RuntimeEvent;
    let ws_event = RuntimeEvent::WorkspaceEvent {
        kind: crew_protocol::WorkspaceEvent::ArtifactPublished {
            lease_id: "ws-004".to_string(),
            artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000004").unwrap(),
            kind: "patch".to_string(),
        },
        run_id: RunId::parse("01900000-0000-0000-0000-000000000004").unwrap(),
        lease_id: "ws-004".to_string(),
    };
    let json = serde_json::to_string(&ws_event).expect("serializable");
    let parsed: RuntimeEvent = serde_json::from_str(&json).expect("deserializable");
    match parsed {
        RuntimeEvent::WorkspaceEvent { .. } => {}
        other => panic!("expected WorkspaceEvent, got {:?}", other),
    }
}

#[test]
fn runtime_event_workspace_event_apply_conflict_serializes() {
    use crew_protocol::RuntimeEvent;
    let ws_event = RuntimeEvent::WorkspaceEvent {
        kind: crew_protocol::WorkspaceEvent::ApplyConflict {
            lease_id: "ws-005".to_string(),
            conflict_artifact_id: ArtifactId::parse("01900000-0000-0000-0000-000000000005")
                .unwrap(),
            strategy: ApplyStrategy::ApplyPatch,
            expected_target_revision: "abc123".to_string(),
        },
        run_id: RunId::parse("01900000-0000-0000-0000-000000000005").unwrap(),
        lease_id: "ws-005".to_string(),
    };
    let json = serde_json::to_string(&ws_event).expect("serializable");
    let parsed: RuntimeEvent = serde_json::from_str(&json).expect("deserializable");
    match parsed {
        RuntimeEvent::WorkspaceEvent { .. } => {}
        other => panic!("expected WorkspaceEvent, got {:?}", other),
    }
}

// ------------------------------------------------------------------ binary-safety test

#[test]
fn artifact_fetch_result_handles_invalid_utf8_via_base64() {
    // /4A= is the base64 encoding of bytes [0xff, 0x80] (invalid UTF-8).
    // This proves the wire contract carries binary data through base64,
    // not a Rust String that would reject non-UTF-8 bytes.
    let result = ArtifactFetchResult {
        artifact: sample_artifact(),
        content_base64: "/4A=".to_string(), // [0xff, 0x80] — invalid UTF-8
        next_offset: None,
        complete: true,
    };
    let json = serialise(&result);
    let obj = json.as_object().expect("is object");
    assert_eq!(
        obj.get("contentBase64").and_then(|v| v.as_str()),
        Some("/4A=")
    );
    // Round-trip: deserialize back and verify the base64 string is preserved.
    let parsed: ArtifactFetchResult = deserialise(json);
    assert_eq!(parsed.content_base64, "/4A=");
}
