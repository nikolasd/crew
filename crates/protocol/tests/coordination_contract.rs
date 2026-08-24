//! Wire-contract tests for the coordination broker's worker-safe surface.

use crew_protocol::{
    COORDINATION_PAYLOAD_MAX_BYTES, COORDINATION_RATE_LIMIT_PER_MINUTE,
    CoordinationAskPolicyParams, CoordinationChildDecision, CoordinationPeersParams,
    CoordinationPublishArtifactParams, CoordinationReportBlockedParams,
    CoordinationRequestChildParams, CoordinationSendParams, CoordinationTaskParams, CrewMethod,
    MessageKind, ProjectId, RunId, TaskId, WorkerId,
};

#[test]
fn coordination_methods_serialize_to_exact_wire_strings() {
    assert_eq!(
        serde_json::to_string(&CrewMethod::CoordinationTask).unwrap(),
        "\"coordination/task\""
    );
    assert_eq!(
        serde_json::to_string(&CrewMethod::CoordinationPeers).unwrap(),
        "\"coordination/peers\""
    );
    assert_eq!(
        serde_json::to_string(&CrewMethod::CoordinationSend).unwrap(),
        "\"coordination/send\""
    );
    assert_eq!(
        serde_json::to_string(&CrewMethod::CoordinationRequestChild).unwrap(),
        "\"coordination/requestChild\""
    );
    assert_eq!(
        serde_json::to_string(&CrewMethod::CoordinationPublishArtifact).unwrap(),
        "\"coordination/publishArtifact\""
    );
    assert_eq!(
        serde_json::to_string(&CrewMethod::CoordinationReportBlocked).unwrap(),
        "\"coordination/reportBlocked\""
    );
    assert_eq!(
        serde_json::to_string(&CrewMethod::CoordinationAskPolicy).unwrap(),
        "\"coordination/askPolicy\""
    );
}

#[test]
fn coordination_task_params_reject_unknown_fields() {
    let valid = serde_json::json!({ "runId": RunId::new().to_string() });
    assert!(serde_json::from_value::<CoordinationTaskParams>(valid).is_ok());

    let with_unknown = serde_json::json!({ "runId": RunId::new().to_string(), "extra": true });
    assert!(serde_json::from_value::<CoordinationTaskParams>(with_unknown).is_err());
}

#[test]
fn coordination_peers_params_reject_wrong_id_type() {
    // A number where a string UUID is expected must be rejected.
    let wrong_type = serde_json::json!({ "runId": 12345 });
    assert!(serde_json::from_value::<CoordinationPeersParams>(wrong_type).is_err());
}

#[test]
fn coordination_send_params_require_sender_task_and_run() {
    let missing_sender = serde_json::json!({
        "runId": RunId::new().to_string(),
        "taskId": TaskId::new().to_string(),
        "kind": "question",
        "payload": "hi"
    });
    assert!(serde_json::from_value::<CoordinationSendParams>(missing_sender).is_err());

    let complete = serde_json::json!({
        "runId": RunId::new().to_string(),
        "senderWorkerId": WorkerId::new().to_string(),
        "taskId": TaskId::new().to_string(),
        "kind": "question",
        "payload": "hi"
    });
    assert!(serde_json::from_value::<CoordinationSendParams>(complete).is_ok());
}

#[test]
fn coordination_send_params_reject_unknown_fields() {
    let with_unknown = serde_json::json!({
        "runId": RunId::new().to_string(),
        "senderWorkerId": WorkerId::new().to_string(),
        "taskId": TaskId::new().to_string(),
        "kind": "question",
        "payload": "hi",
        "unexpectedField": 1
    });
    assert!(serde_json::from_value::<CoordinationSendParams>(with_unknown).is_err());
}

#[test]
fn coordination_send_params_accept_every_message_kind() {
    for kind in [
        MessageKind::Assign,
        MessageKind::Steer,
        MessageKind::FollowUp,
        MessageKind::Question,
        MessageKind::Answer,
        MessageKind::PeerMessage,
        MessageKind::ApprovalDecision,
        MessageKind::Cancel,
        MessageKind::Shutdown,
    ] {
        let params = CoordinationSendParams {
            run_id: RunId::new(),
            sender_worker_id: WorkerId::new(),
            task_id: TaskId::new(),
            kind,
            payload: "x".to_string(),
            recipient_worker_id: None,
            reply_to: None,
        };
        let json = serde_json::to_value(&params).unwrap();
        let restored: CoordinationSendParams = serde_json::from_value(json).unwrap();
        assert_eq!(restored.kind, params.kind);
    }
}

#[test]
fn coordination_request_child_params_require_run_and_reason() {
    let missing_reason = serde_json::json!({ "runId": RunId::new().to_string() });
    assert!(serde_json::from_value::<CoordinationRequestChildParams>(missing_reason).is_err());
}

#[test]
fn coordination_publish_artifact_params_round_trip() {
    let params = CoordinationPublishArtifactParams {
        run_id: RunId::new(),
        artifact_ref: "artifact://abc".to_string(),
        description: Some("a patch".to_string()),
    };
    let json = serde_json::to_value(&params).unwrap();
    let restored: CoordinationPublishArtifactParams = serde_json::from_value(json).unwrap();
    assert_eq!(restored, params);
}

#[test]
fn coordination_report_blocked_and_ask_policy_round_trip() {
    let blocked = CoordinationReportBlockedParams {
        run_id: RunId::new(),
        reason: "waiting on peer".to_string(),
    };
    let json = serde_json::to_value(&blocked).unwrap();
    assert_eq!(
        serde_json::from_value::<CoordinationReportBlockedParams>(json).unwrap(),
        blocked
    );

    let policy = CoordinationAskPolicyParams {
        run_id: RunId::new(),
        question: "may I write here?".to_string(),
    };
    let json = serde_json::to_value(&policy).unwrap();
    assert_eq!(
        serde_json::from_value::<CoordinationAskPolicyParams>(json).unwrap(),
        policy
    );
}

#[test]
fn coordination_child_decision_accept_and_deny_shapes() {
    let accept = CoordinationChildDecision::Accept {
        parent_run_id: RunId::new(),
        child_task_id: TaskId::new(),
        child_worker_id: WorkerId::new(),
        child_run_id: RunId::new(),
    };
    let value = serde_json::to_value(&accept).unwrap();
    assert_eq!(value["decision"], "accept");

    let deny = CoordinationChildDecision::Deny {
        parent_run_id: RunId::new(),
        reason: "policy denied".to_string(),
    };
    let value = serde_json::to_value(&deny).unwrap();
    assert_eq!(value["decision"], "deny");
    assert_eq!(value["reason"], "policy denied");
}

#[test]
fn coordination_bounds_constants_match_the_plan() {
    assert_eq!(COORDINATION_PAYLOAD_MAX_BYTES, 64 * 1024);
    assert_eq!(COORDINATION_RATE_LIMIT_PER_MINUTE, 30);
}

/// Sanity: `ProjectId` is unused directly in this file's assertions but
/// pinning the import ensures the protocol crate keeps exporting it
/// alongside the coordination types this file exercises.
#[test]
fn project_id_remains_exported() {
    let _ = ProjectId::new();
}
