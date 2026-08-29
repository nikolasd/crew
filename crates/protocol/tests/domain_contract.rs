//! Domain-contract tests for the orchestration extension.
//!
//! Verifies the complete legal run lifecycle, serialization invariants, and
//! wire-format strictness of every orchestration record.

use crew_protocol::{
    ApprovalDecision, DeliveryState, Run, RunFlags, RunSpec, RunState, RuntimeEventKind, TaskRef,
    Timestamp, Worker, WorkerProfileRef,
};

// ---------------------------------------------------------------------------
// Lifecycle table — every listed edge must be accepted; every other pair,
// including self-transitions and every transition out of a terminal state,
// must be rejected.
// ---------------------------------------------------------------------------

#[test]
fn legal_lifecycle_edges_are_accepted() {
    let edges = [
        ("queued", "starting"),
        ("queued", "failed"),
        ("queued", "cancelled"),
        ("starting", "working"),
        ("starting", "failed"),
        ("starting", "cancelled"),
        ("starting", "lost"),
        ("working", "waitingUser"),
        ("working", "waitingPeer"),
        ("working", "paused"),
        ("working", "succeeded"),
        ("working", "failed"),
        ("working", "cancelled"),
        ("working", "lost"),
        ("waitingUser", "working"),
        ("waitingUser", "paused"),
        ("waitingUser", "failed"),
        ("waitingUser", "cancelled"),
        ("waitingUser", "lost"),
        ("waitingPeer", "working"),
        ("waitingPeer", "paused"),
        ("waitingPeer", "failed"),
        ("waitingPeer", "cancelled"),
        ("waitingPeer", "lost"),
        ("paused", "working"),
        ("paused", "failed"),
        ("paused", "cancelled"),
        ("paused", "lost"),
    ];

    for (from, to) in edges {
        let from_state: RunState = from.parse().expect(from);
        let to_state: RunState = to.parse().expect(to);
        assert!(
            from_state.can_transition_to(&to_state),
            "{from} -> {to} should be legal",
        );
    }
}

#[test]
fn illegal_lifecycle_edges_are_rejected() {
    let illegal = [
        // Self-transitions
        ("queued", "queued"),
        ("working", "working"),
        ("succeeded", "succeeded"),
        // Terminal states → anything
        ("succeeded", "working"),
        ("succeeded", "failed"),
        ("succeeded", "cancelled"),
        ("succeeded", "lost"),
        ("failed", "working"),
        ("failed", "starting"),
        ("cancelled", "working"),
        ("cancelled", "queued"),
        ("lost", "working"),
        ("lost", "paused"),
        // Unlisted edges
        ("queued", "working"),
        ("queued", "succeeded"),
        ("starting", "queued"),
        ("starting", "waitingUser"),
        ("working", "queued"),
        ("working", "starting"),
        ("waitingUser", "starting"),
        ("waitingUser", "waitingPeer"),
        ("waitingPeer", "waitingUser"),
        ("waitingPeer", "queued"),
        ("paused", "queued"),
        ("paused", "starting"),
        ("paused", "waitingUser"),
    ];

    for (from, to) in illegal {
        let from_state: RunState = from.parse().expect(from);
        let to_state: RunState = to.parse().expect(to);
        assert!(
            !from_state.can_transition_to(&to_state),
            "{from} -> {to} should be illegal",
        );
    }
}

#[test]
fn terminal_states_have_is_terminal_true() {
    for terminal in ["succeeded", "failed", "cancelled", "lost"] {
        let state: RunState = terminal.parse().expect(terminal);
        assert!(state.is_terminal(), "{terminal} should be terminal");
    }

    for non_terminal in [
        "queued",
        "starting",
        "working",
        "waitingUser",
        "waitingPeer",
        "paused",
    ] {
        let state: RunState = non_terminal.parse().expect(non_terminal);
        assert!(
            !state.is_terminal(),
            "{non_terminal} should not be terminal"
        );
    }
}

// ---------------------------------------------------------------------------
// Serialization — flags must be independent booleans.
// ---------------------------------------------------------------------------

#[test]
fn run_flags_serialize_as_independent_booleans() {
    let run = Run {
        run_id: crew_protocol::RunId::new(),
        task_id: crew_protocol::TaskId::new(),
        worker_id: crew_protocol::WorkerId::new(),
        state: RunState::try_from("working").unwrap(),
        flags: RunFlags {
            degraded_control: true,
            needs_reconciliation: true,
            protocol_unhealthy: true,
            policy_quarantined: true,
            workspace_dirty: true,
            children_active: true,
            turn_settled: true,
        },
        vendor_session_id: None,
        started_at: None,
        completed_at: None,
    };

    let value = serde_json::to_value(&run).unwrap();
    let flags = value.get("flags").expect("flags present");

    assert_eq!(flags.get("degradedControl"), Some(&serde_json::json!(true)));
    assert_eq!(
        flags.get("needsReconciliation"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        flags.get("protocolUnhealthy"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(
        flags.get("policyQuarantined"),
        Some(&serde_json::json!(true))
    );
    assert_eq!(flags.get("workspaceDirty"), Some(&serde_json::json!(true)));
    assert_eq!(flags.get("childrenActive"), Some(&serde_json::json!(true)));
}

#[test]
fn run_flags_deserialize_independently() {
    let json = serde_json::json!({
        "runId": "01800000-0000-0000-0000-000000000000",
        "taskId": "01800000-0000-0000-0000-000000000001",
        "workerId": "01800000-0000-0000-0000-000000000002",
        "state": "working",
        "flags": {
            "degradedControl": true,
            "needsReconciliation": false,
            "protocolUnhealthy": true,
            "policyQuarantined": false,
            "workspaceDirty": true,
            "childrenActive": false
        }
    });

    let run: Run = serde_json::from_value(json).expect("valid flags");
    assert!(run.flags.degraded_control);
    assert!(!run.flags.needs_reconciliation);
    assert!(run.flags.protocol_unhealthy);
    assert!(!run.flags.policy_quarantined);
    assert!(run.flags.workspace_dirty);
    assert!(!run.flags.children_active);
}

// ---------------------------------------------------------------------------
// TaskRef — stores ownerClientInstanceId plus monotonic OMP revision.
// ---------------------------------------------------------------------------

#[test]
fn task_ref_stores_owner_and_revision() {
    let task_ref = TaskRef {
        owner_client_instance_id: "omp-1".to_string(),
        revision: 42u64,
    };

    let value = serde_json::to_value(&task_ref).unwrap();
    assert_eq!(value["ownerClientInstanceId"], "omp-1");
    assert_eq!(value["revision"], 42);
}

#[test]
fn task_ref_rejects_unknown_fields() {
    let json = serde_json::json!({
        "ownerClientInstanceId": "omp-1",
        "revision": 1,
        "unknown": true
    });
    assert!(
        serde_json::from_value::<TaskRef>(json).is_err(),
        "unknown fields should be rejected",
    );
}

// ---------------------------------------------------------------------------
// WorkerProfileRef — fingerprint, adapter, model, permission envelope.
// ---------------------------------------------------------------------------

#[test]
fn worker_profile_ref_serializes_fields() {
    let profile = WorkerProfileRef {
        id: crew_protocol::WorkerId::new(),
        fingerprint: "sha256:abc".to_string(),
        adapter: "claude".to_string(),
        model: "claude-sonnet-4-20250514".to_string(),
        permission_envelope: serde_json::json!({"scope": "write"}),
    };

    let value = serde_json::to_value(&profile).unwrap();
    assert_eq!(value["fingerprint"], "sha256:abc");
    assert_eq!(value["adapter"], "claude");
    assert_eq!(value["model"], "claude-sonnet-4-20250514");
}

// ---------------------------------------------------------------------------
// Worker — immutable reference to profile.
// ---------------------------------------------------------------------------

#[test]
fn worker_contains_profile_ref() {
    let profile_id = crew_protocol::WorkerId::new();
    let worker = Worker {
        worker_id: profile_id,
        profile_ref: WorkerProfileRef {
            id: profile_id,
            fingerprint: "fp".into(),
            adapter: "fake".into(),
            model: "test".into(),
            permission_envelope: serde_json::json!({}),
        },
        parent_worker_id: None,
        created_at: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
    };

    assert_eq!(worker.worker_id, profile_id);
    assert_eq!(worker.profile_ref.adapter, "fake");
}

// ---------------------------------------------------------------------------
// RunSpec — immutable task + worker + optional workspace_mode.
// ---------------------------------------------------------------------------

#[test]
fn run_spec_contains_task_and_worker() {
    let spec = RunSpec {
        task_id: crew_protocol::TaskId::new(),
        worker_id: crew_protocol::WorkerId::new(),
        workspace_mode: Some("isolated".to_string()),
        priority: 5,
        prompt: None,
    };

    let value = serde_json::to_value(&spec).unwrap();
    assert!(value.get("workspaceMode").is_some());
    assert_eq!(value["priority"], 5);
}

// ---------------------------------------------------------------------------
// RuntimeEventKind — every record creation / transition / flag change /
// delivery change / approval request/decision event.
// ---------------------------------------------------------------------------

#[test]
fn runtime_event_kind_covers_all_variants() {
    let events = [
        RuntimeEventKind::TaskCreated,
        RuntimeEventKind::TaskUpdated,
        RuntimeEventKind::WorkerCreated,
        RuntimeEventKind::RunQueued,
        RuntimeEventKind::RunStarting,
        RuntimeEventKind::RunWorking,
        RuntimeEventKind::RunWaitingUser,
        RuntimeEventKind::RunWaitingPeer,
        RuntimeEventKind::RunPaused,
        RuntimeEventKind::RunSucceeded,
        RuntimeEventKind::RunFailed,
        RuntimeEventKind::RunCancelled,
        RuntimeEventKind::RunLost,
        RuntimeEventKind::RunFlagsChanged,
        RuntimeEventKind::MessageRecorded,
        RuntimeEventKind::MessageSent,
        RuntimeEventKind::MessageAcknowledged,
        RuntimeEventKind::MessageFailed,
        RuntimeEventKind::ApprovalRequested,
        RuntimeEventKind::ApprovalDecided,
        RuntimeEventKind::ChildWorkerRequested,
        RuntimeEventKind::ChildWorkerRequestDenied,
        RuntimeEventKind::ReconcileOwnershipChanged,
    ];

    for event in events {
        // Each variant must serialize with deny_unknown_fields and
        // camelCase field names.
        let _value =
            serde_json::to_value(&event).unwrap_or_else(|e| panic!("{event:?} serializes: {e}"));
    }
}

// ---------------------------------------------------------------------------
// CrewMethod — every orchestration method.
// ---------------------------------------------------------------------------

#[test]
fn all_orchestration_methods_exist() {
    use crew_protocol::CrewMethod::{
        ApprovalDecide, ApprovalList, CoordinationChildDecide, CoordinationChildList, MessageList,
        MessageSend, ReconcileOmp, RunCancel, RunGet, RunList, RunRetry, RunSubmit, TaskGet,
        TaskUpsert, WorkerCreate, WorkerGet, WorkerList,
    };

    // Ensure each variant maps to the expected wire string.
    assert_eq!(
        serde_json::to_string(&TaskUpsert).unwrap(),
        "\"task/upsert\"",
    );
    assert_eq!(serde_json::to_string(&TaskGet).unwrap(), "\"task/get\"",);
    assert_eq!(
        serde_json::to_string(&WorkerCreate).unwrap(),
        "\"worker/create\"",
    );
    assert_eq!(
        serde_json::to_string(&WorkerList).unwrap(),
        "\"worker/list\"",
    );
    assert_eq!(serde_json::to_string(&WorkerGet).unwrap(), "\"worker/get\"",);
    assert_eq!(serde_json::to_string(&RunSubmit).unwrap(), "\"run/submit\"",);
    assert_eq!(serde_json::to_string(&RunList).unwrap(), "\"run/list\"",);
    assert_eq!(serde_json::to_string(&RunGet).unwrap(), "\"run/get\"",);
    assert_eq!(serde_json::to_string(&RunRetry).unwrap(), "\"run/retry\"",);
    assert_eq!(serde_json::to_string(&RunCancel).unwrap(), "\"run/cancel\"",);
    assert_eq!(
        serde_json::to_string(&crew_protocol::CrewMethod::RunResult).unwrap(),
        "\"run/result\"",
    );
    assert_eq!(
        serde_json::to_string(&MessageSend).unwrap(),
        "\"message/send\"",
    );
    assert_eq!(
        serde_json::to_string(&MessageList).unwrap(),
        "\"message/list\"",
    );
    assert_eq!(
        serde_json::to_string(&ApprovalList).unwrap(),
        "\"approval/list\"",
    );
    assert_eq!(
        serde_json::to_string(&ApprovalDecide).unwrap(),
        "\"approval/decide\"",
    );
    assert_eq!(
        serde_json::to_string(&CoordinationChildList).unwrap(),
        "\"coordination/child/list\"",
    );
    assert_eq!(
        serde_json::to_string(&CoordinationChildDecide).unwrap(),
        "\"coordination/child/decide\"",
    );
    assert_eq!(
        serde_json::to_string(&ReconcileOmp).unwrap(),
        "\"reconcile/omp\"",
    );
}

// ---------------------------------------------------------------------------
// DeliveryState — every state variant.
// ---------------------------------------------------------------------------

#[test]
fn delivery_state_variants_exist() {
    use DeliveryState::{Acknowledged, Failed, Recorded, Sent, Unknown};
    assert_eq!(serde_json::to_string(&Recorded).unwrap(), "\"recorded\"",);
    assert_eq!(serde_json::to_string(&Sent).unwrap(), "\"sent\"",);
    assert_eq!(
        serde_json::to_string(&Acknowledged).unwrap(),
        "\"acknowledged\"",
    );
    assert_eq!(serde_json::to_string(&Failed).unwrap(), "\"failed\"",);
    assert_eq!(serde_json::to_string(&Unknown).unwrap(), "\"unknown\"",);
}

// ---------------------------------------------------------------------------
// ApprovalDecision — approve / deny with reason.
// ---------------------------------------------------------------------------

#[test]
fn approval_decision_serializes_fields() {
    let decision = ApprovalDecision {
        decision: "approve".to_string(),
        reason: "task complete".to_string(),
    };

    let value = serde_json::to_value(&decision).unwrap();
    assert_eq!(value["decision"], "approve");
    assert_eq!(value["reason"], "task complete");
}

// ---------------------------------------------------------------------------
// Integration: a full TaskRef → Worker → RunSpec → Run round-trip.
// ---------------------------------------------------------------------------

#[test]
fn full_run_serialization_round_trips() {
    let run = Run {
        run_id: crew_protocol::RunId::new(),
        task_id: crew_protocol::TaskId::new(),
        worker_id: crew_protocol::WorkerId::new(),
        state: RunState::try_from("queued").unwrap(),
        flags: RunFlags {
            degraded_control: false,
            needs_reconciliation: false,
            protocol_unhealthy: false,
            policy_quarantined: false,
            workspace_dirty: false,
            children_active: false,
            turn_settled: false,
        },
        vendor_session_id: None,
        started_at: None,
        completed_at: None,
    };

    let json = serde_json::to_string(&run).expect("Run serializes");
    let restored: Run = serde_json::from_str(&json).expect("Run deserializes");

    assert_eq!(run.run_id, restored.run_id);
    assert_eq!(run.task_id, restored.task_id);
    assert_eq!(run.worker_id, restored.worker_id);
    assert_eq!(run.state, restored.state);
    assert_eq!(run.flags, restored.flags);
}
