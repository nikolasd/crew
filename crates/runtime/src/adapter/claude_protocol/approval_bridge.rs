//! Bridges a `can_use_tool` request off claude's control channel into
//! crew's own approval ledger (`crate::approval::ApprovalService`) --
//! the existing, durable, replayable mechanism no adapter has ever
//! actually called into before this one (`Adapter::respond_to_approval`
//! is `capability_unsupported` everywhere else today). No new ledger
//! type: `ApprovalRequest`/`ApprovalDecision` already carry request,
//! decision, who decided, and when.
//!
//! **The control-channel JSON shapes below are provisional**, not yet
//! checked against a live capture -- the same caution
//! `claude_protocol::reconcile`'s own transcript entry schema carries,
//! and for the identical reason: this spike's own stop conditions ask
//! that of the control channel specifically, and confirming it is part
//! of the spike's own job, not something to guess past.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use crew_protocol::{ApprovalId, ApprovalRequest, RunId, TaskId, Timestamp};
use tokio::sync::oneshot;

use crate::approval::{ApprovalCallback, ApprovalService, CallbackFuture};

/// One parsed `can_use_tool` request from claude's control channel.
/// Provisional field names -- see this module's own doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionRequest {
    /// The control channel's own correlation id for this request --
    /// echoed back verbatim in the reply so claude can match it to the
    /// pending tool call it is blocked on.
    pub(crate) request_id: String,
    pub(crate) tool_name: String,
    pub(crate) input: serde_json::Value,
    /// Claude's own id for the specific tool call this request is
    /// blocked on (`toolu_...`) -- distinct from `request_id`, which is
    /// this control-channel round trip's own correlation id, generated
    /// fresh per request. A live capture's final `result` message
    /// reports a denied tool call by THIS id, in `permission_denials`,
    /// never by `request_id` -- so reconciling a denial against
    /// whichever requests this reader actually bridged needs this
    /// field, not `request_id`. `None` when absent -- observed present
    /// on every real `can_use_tool` request this spike captured, but
    /// parsed as optional rather than required, matching this module's
    /// own tolerant-parse stance for every other field here.
    pub(crate) tool_use_id: Option<String>,
}

/// Parses one `can_use_tool` control-channel message. Tolerant the same
/// way `claude_protocol::reconcile`'s transcript parser is: a shape this
/// function does not recognize returns `None` rather than panicking or
/// guessing -- an unrecognized control message is a fact for the reader
/// loop to act on (most likely: fail the run, since a permission
/// request it cannot answer blocks claude indefinitely), not something
/// this parser should paper over.
pub(crate) fn parse_permission_request(value: &serde_json::Value) -> Option<PermissionRequest> {
    let request = value.get("request")?;
    if request.get("subtype")?.as_str()? != "can_use_tool" {
        return None;
    }
    Some(PermissionRequest {
        request_id: value.get("request_id")?.as_str()?.to_string(),
        tool_name: request.get("tool_name")?.as_str()?.to_string(),
        input: request
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        tool_use_id: request
            .get("tool_use_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// Builds the control-channel reply bytes that answer `request_id`,
/// unblocking claude's own pending tool call. `decision` is crew's own
/// wire vocabulary (`"approve"`/`"deny"`, matching
/// `crew_protocol::ApprovalDecision::decision`) -- translated here into
/// the shape claude's own protocol expects, exactly once, at this one
/// boundary, so nowhere else in this adapter has to know both
/// vocabularies.
pub(crate) fn build_permission_response(request_id: &str, decision: &str) -> Vec<u8> {
    let behavior = if decision == "approve" {
        "allow"
    } else {
        "deny"
    };
    let mut line = serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": { "behavior": behavior },
        },
    })
    .to_string();
    line.push('\n');
    line.into_bytes()
}

/// Bridges `ApprovalService::decide`'s callback into a specific in-flight
/// control-channel request via a one-shot channel captured at request
/// time -- deliberately NOT a `worker_id -> Arc<dyn Adapter>` registry
/// lookup. An approval belongs to a specific in-flight request, not to
/// an adapter instance: a registry lookup would happily deliver a
/// decision to a NEW instance after a restart, one with no pending tool
/// call and no idea what the decision refers to. The channel makes that
/// coupling explicit and lets it die with the request, which is what the
/// domain actually looks like.
pub struct ProtocolApprovalCallback {
    pending: StdMutex<HashMap<ApprovalId, oneshot::Sender<String>>>,
}

impl ProtocolApprovalCallback {
    /// `pub`, not `pub(crate)`: `crates/runtime/tests/tui_claude_registry.rs`
    /// constructs one directly, as an ordinary external dependency, to
    /// install the REAL daemon-shaped `ApprovalService` wiring
    /// (`ProtocolApprovalCallback` as its callback, exactly as
    /// `lifecycle.rs` wires it) ahead of a TUI run, and prove that run's
    /// own behavior is unaffected by it -- the same "widen specifically
    /// so an external test can prove a production property" precedent
    /// `claude_protocol::reconcile`'s own module already set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: StdMutex::new(HashMap::new()),
        }
    }

    /// Registers a one-shot receiver for `approval_id`, to be resolved
    /// once `ApprovalService::decide` calls this callback back. Callers
    /// must register BEFORE calling `ApprovalService::request`, so a
    /// decision racing in immediately after cannot be missed.
    pub(crate) fn register(&self, approval_id: ApprovalId) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("approval-callback mutex never poisoned")
            .insert(approval_id, tx);
        rx
    }
}

impl Default for ProtocolApprovalCallback {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalCallback for ProtocolApprovalCallback {
    fn acknowledge(&self, approval_id: ApprovalId, decision: &str) -> CallbackFuture<'static> {
        let sender = self
            .pending
            .lock()
            .expect("approval-callback mutex never poisoned")
            .remove(&approval_id);
        let decision = decision.to_string();
        Box::pin(async move {
            if let Some(sender) = sender {
                // A dropped receiver -- the run was cancelled, this
                // adapter was torn down, or claude's own process already
                // exited -- is a normal outcome here, not an error: the
                // decision is already durably recorded in the ledger by
                // `ApprovalService::decide` before this callback ever
                // runs, and the vendor call this reply would have
                // unblocked is already moot. The same shape as a
                // resolved race elsewhere in this ADR (a second answerer
                // arriving after the first already won) being treated as
                // a normal outcome, not an error, one layer up -- `send`'s
                // own `Result` is deliberately discarded, not propagated
                // as this callback's own failure.
                let _ = sender.send(decision);
            }
            Ok(())
        })
    }
}

/// Bridges one parsed `can_use_tool` request all the way to the reply
/// bytes claude's control channel needs: creates the approval (which
/// atomically pauses the run at `waitingUser`), registers this specific
/// request's one-shot receiver BEFORE submitting it (never after --
/// registering after would race a decision that resolves before the
/// receiver exists), and awaits the decision to build the reply.
///
/// # Errors
/// Propagates `ApprovalService::request`'s own error (e.g. the run is
/// not `working`), or a closed-channel error if the callback's sender is
/// dropped without ever sending -- which
/// [`ProtocolApprovalCallback::acknowledge`] does not do on its own
/// initiative (a `None` lookup there means this exact request id was
/// never registered, not that a decision resolved and chose to say
/// nothing); reaching this error means the whole `ProtocolApprovalCallback`
/// was dropped (this adapter's own teardown) while a decision was still
/// pending.
pub(crate) async fn handle_permission_request(
    approval_service: &ApprovalService,
    callback: &ProtocolApprovalCallback,
    run_id: RunId,
    task_id: TaskId,
    request: PermissionRequest,
) -> Result<Vec<u8>, String> {
    let approval_id = ApprovalId::new();
    let receiver = callback.register(approval_id);
    let approval = ApprovalRequest {
        approval_id,
        run_id,
        task_id,
        action: request.tool_name,
        arguments: request.input,
        human_required: true,
        policy_reason: "claude requested permission to use a tool over its control channel"
            .to_string(),
        created_at: Timestamp::now(),
        decided_at: None,
        decision: None,
        decided_by: None,
        reason: None,
    };
    approval_service
        .request(approval)
        .await
        .map_err(|err| err.to_string())?;
    let decision = receiver
        .await
        .map_err(|_| "the approval callback was dropped before a decision arrived".to_string())?;
    Ok(build_permission_response(&request.request_id, &decision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_protocol::{
        DecidedBy, Redacted, Run, RunFlags, RunState, TaskRef, Worker, WorkerId, WorkerProfileRef,
    };
    use std::sync::Arc;
    use tokio::sync::broadcast;

    use crate::db::DatabaseHandle;
    use crate::domain::DomainRepository;

    /// Seeds one task/worker/run and drives it to `working` -- the same
    /// shape `run_flags_lost_update.rs`'s own `seed_pending_approval`
    /// uses, minus the approval itself: this module's own
    /// `handle_permission_request` is what creates that, which is the
    /// thing under test here.
    async fn seed_working_run(
        db: &DatabaseHandle,
        project_id: crew_protocol::ProjectId,
    ) -> (RunId, TaskId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &TaskRef {
                    owner_client_instance_id: "omp-1".into(),
                    revision: 1,
                },
            )?;
            let worker = Worker {
                worker_id,
                profile_ref: WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".into(),
                    adapter: "claude".into(),
                    model: "test".into(),
                    permission_envelope: serde_json::json!({}),
                },
                parent_worker_id: None,
                created_at: Timestamp::now(),
            };
            repo.create_worker(&worker)?;
            let run = Run {
                run_id,
                task_id,
                worker_id,
                state: RunState::try_from("queued").expect("queued is a valid state"),
                flags: RunFlags::default(),
                vendor_session_id: None,
                started_at: None,
                completed_at: None,
            };
            repo.submit_run(&run, None, None)?;
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed task/worker/run");

        for state in ["starting", "working"] {
            let to = RunState::try_from(state).expect("valid state");
            db.run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.transition_run(run_id, &to, None)
                    .map(|_| serde_json::json!({}))
            }))
            .await
            .unwrap_or_else(|e| panic!("drive to {state} failed: {e}"));
        }
        (run_id, task_id)
    }

    /// The full bridge, end to end, against a REAL `ApprovalService` (not
    /// a fake): parses a `can_use_tool` request, submits it through
    /// `handle_permission_request` (which creates the approval and parks
    /// on this specific request's own receiver), decides it exactly the
    /// way `approval/decide` would for a human answering a dialog, and
    /// asserts the reply bytes this adapter would write back to claude's
    /// control channel are correct -- proving the whole round trip, not
    /// just its two halves in isolation.
    #[tokio::test]
    async fn a_real_approval_round_trip_produces_the_correct_control_channel_reply() {
        let state_dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let (run_id, task_id) = seed_working_run(&db, project_id).await;

        let callback = Arc::new(ProtocolApprovalCallback::new());
        let service = ApprovalService::new(
            Arc::clone(&db),
            project_id,
            Arc::clone(&callback) as Arc<dyn ApprovalCallback>,
            broadcast::channel(64).0,
        );

        let request = parse_permission_request(&serde_json::json!({
            "type": "control_request",
            "request_id": "req-42",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": { "path": "/tmp/x" },
            },
        }))
        .expect("must parse");

        let handled = tokio::spawn({
            let callback = Arc::clone(&callback);
            async move { handle_permission_request(&service, &callback, run_id, task_id, request).await }
        });

        // Give `handle_permission_request` a chance to create the
        // approval and register its receiver before deciding it --
        // mirroring the real timing (claude blocks on this reply; crew
        // decides only once the approval genuinely exists).
        tokio::task::yield_now().await;

        let approval_id = {
            let value = db
                .run_domain_op(Box::new(move |conn| {
                    conn.query_row(
                        "SELECT approval_id FROM approvals WHERE run_id = ?1",
                        [run_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .map(|approval_id| serde_json::json!({ "approvalId": approval_id }))
                    .map_err(Into::into)
                }))
                .await
                .expect("the approval must have been created");
            let approval_id = value["approvalId"].as_str().expect("a string column");
            ApprovalId::parse(approval_id).expect("a valid approval id")
        };

        let decide_service = ApprovalService::new(
            Arc::clone(&db),
            project_id,
            callback as Arc<dyn ApprovalCallback>,
            broadcast::channel(64).0,
        );
        decide_service
            .decide(
                approval_id,
                "omp-1",
                "approve",
                &Redacted::assert_runtime_authored("a human approved it"),
                DecidedBy::Human,
            )
            .await
            .expect("decide must succeed");

        let reply = handled
            .await
            .expect("the task must not panic")
            .expect("handle_permission_request must succeed");
        let value: serde_json::Value = serde_json::from_slice(&reply).expect("valid json line");
        assert_eq!(value["response"]["request_id"], "req-42");
        assert_eq!(value["response"]["response"]["behavior"], "allow");

        db.shutdown().await.ok();
    }

    #[test]
    fn parses_a_well_formed_can_use_tool_request() {
        let value = serde_json::json!({
            "type": "control_request",
            "request_id": "req-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": { "path": "/tmp/x", "content": "hi" },
                "tool_use_id": "toolu_01abc",
            },
        });
        let parsed = parse_permission_request(&value).expect("must parse");
        assert_eq!(parsed.request_id, "req-1");
        assert_eq!(parsed.tool_name, "Write");
        assert_eq!(
            parsed.input,
            serde_json::json!({ "path": "/tmp/x", "content": "hi" })
        );
        assert_eq!(parsed.tool_use_id.as_deref(), Some("toolu_01abc"));
    }

    /// `tool_use_id` is optional, not required -- an absent one parses
    /// to `None` rather than failing the whole request, matching this
    /// module's own tolerant-parse stance for every other field.
    #[test]
    fn a_missing_tool_use_id_parses_to_none() {
        let value = serde_json::json!({
            "type": "control_request",
            "request_id": "req-1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Write",
                "input": {},
            },
        });
        let parsed = parse_permission_request(&value).expect("must parse");
        assert_eq!(parsed.tool_use_id, None);
    }

    /// A different control-channel message shape (not a permission
    /// request at all) must not be misparsed as one -- silently treating
    /// an unrelated control message as `can_use_tool` would fabricate an
    /// approval nobody asked for.
    #[test]
    fn a_non_permission_control_message_does_not_parse() {
        let value = serde_json::json!({
            "type": "control_request",
            "request_id": "req-2",
            "request": { "subtype": "set_model", "model": "opus" },
        });
        assert_eq!(parse_permission_request(&value), None);
    }

    #[test]
    fn approve_maps_to_allow_and_names_the_request_id() {
        let bytes = build_permission_response("req-1", "approve");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json line");
        assert_eq!(value["response"]["request_id"], "req-1");
        assert_eq!(value["response"]["response"]["behavior"], "allow");
    }

    #[test]
    fn deny_maps_to_deny() {
        let bytes = build_permission_response("req-1", "deny");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json line");
        assert_eq!(value["response"]["response"]["behavior"], "deny");
    }

    /// The Q2 ruling, proven directly: acknowledging a decision for an
    /// approval id nobody registered (the receiver was already dropped,
    /// or never existed) must not error -- it is a normal outcome, the
    /// same shape as losing a race, not a fault.
    #[tokio::test]
    async fn acknowledging_an_unregistered_or_dropped_approval_never_errors() {
        let callback = ProtocolApprovalCallback::new();
        let result = callback.acknowledge(ApprovalId::new(), "approve").await;
        assert!(result.is_ok(), "must not error: {result:?}");

        // Registered, then the receiver is dropped before a decision --
        // simulating the run being cancelled or this adapter torn down
        // while an approval was still pending.
        let approval_id = ApprovalId::new();
        let receiver = callback.register(approval_id);
        drop(receiver);
        let result = callback.acknowledge(approval_id, "approve").await;
        assert!(
            result.is_ok(),
            "a dropped receiver must not error: {result:?}"
        );
    }

    /// The ordinary path: register, acknowledge, and the registered
    /// receiver actually gets the decision.
    #[tokio::test]
    async fn acknowledging_a_registered_approval_delivers_the_decision() {
        let callback = ProtocolApprovalCallback::new();
        let approval_id = ApprovalId::new();
        let receiver = callback.register(approval_id);
        callback
            .acknowledge(approval_id, "approve")
            .await
            .expect("must not error");
        let decision = receiver.await.expect("the receiver must get the decision");
        assert_eq!(decision, "approve");
    }
}
