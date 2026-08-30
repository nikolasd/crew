//! The correlated approval flow: request creation paired with a run pause,
//! ownership-enforced decisions, and adapter-callback semantics that never
//! ask again on a failed callback -- they mark the run `protocolUnhealthy`
//! instead.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crew_protocol::{ApprovalId, ApprovalRequest, EventEnvelope, ProjectId, RunId, RunState};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::db::DatabaseHandle;
use crate::domain::{DomainError, DomainRepository, RunFlag, broadcast_committed, embed_envelope};

/// A boxed future returned by [`ApprovalCallback::acknowledge`].
pub type CallbackFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

/// The adapter-callback seam invoked after a decision is recorded. The
/// (later) adapter registry plan implements this against real harnesses;
/// [`NoopApprovalCallback`] acknowledges immediately for tests and
/// fixtures without an adapter, and a test-injected failing callback
/// exercises the `protocolUnhealthy` path.
pub trait ApprovalCallback: Send + Sync {
    fn acknowledge(&self, approval_id: ApprovalId, decision: &str) -> CallbackFuture<'static>;
}

/// Acknowledges every callback immediately. The default when no adapter
/// registry is wired up.
pub struct NoopApprovalCallback;

impl ApprovalCallback for NoopApprovalCallback {
    fn acknowledge(&self, _approval_id: ApprovalId, _decision: &str) -> CallbackFuture<'static> {
        Box::pin(async { Ok(()) })
    }
}

/// Errors returned by [`ApprovalService`] operations.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    /// The requesting principal does not own the task this approval
    /// belongs to.
    #[error("principal {instance_id} does not own the task for approval {approval_id}")]
    Forbidden {
        instance_id: String,
        approval_id: ApprovalId,
    },
    /// The approval already has a decision that conflicts with the one
    /// requested.
    #[error("approval {approval_id} already has a conflicting decision")]
    Conflict { approval_id: ApprovalId },
    /// The run this approval belongs to has already settled (reached a
    /// terminal state); a decision cannot target it.
    #[error("run {run_id} has already settled; cannot decide approval {approval_id}")]
    RunSettled {
        approval_id: ApprovalId,
        run_id: RunId,
    },
    /// The approval requires a human decision, but the caller identified
    /// as a model.
    #[error("approval {approval_id} requires a human decision")]
    HumanRequired { approval_id: ApprovalId },
    /// A referenced record was not found.
    #[error("{kind} {id} not found")]
    NotFound { kind: &'static str, id: String },
    /// A domain-layer command failed.
    #[error(transparent)]
    Domain(#[from] DomainError),
}

/// The outcome of [`ApprovalService::decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecideOutcome {
    /// A new decision was recorded and the callback succeeded: the run
    /// returned to `working`.
    Decided,
    /// A new decision was recorded but the callback failed: the decision
    /// is kept and the run is marked `protocolUnhealthy` instead of being
    /// asked again.
    DecidedCallbackFailed,
    /// An identical decision was already on record; this call is a no-op.
    AlreadyDecided,
}

/// Routes approval creation and decisions through the domain repository,
/// enforcing ownership, idempotency, and the settled-run invariant that
/// only the domain repository's mechanical layer does not itself know.
pub struct ApprovalService {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    callback: Arc<dyn ApprovalCallback>,
    events_tx: broadcast::Sender<EventEnvelope>,
}

impl ApprovalService {
    #[must_use]
    pub fn new(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        callback: Arc<dyn ApprovalCallback>,
        events_tx: broadcast::Sender<EventEnvelope>,
    ) -> Self {
        Self {
            db,
            project_id,
            callback,
            events_tx,
        }
    }

    /// Called when an adapter reports it needs approval for `action`.
    /// Atomically creates the request and transitions the run
    /// `working -> waitingUser`.
    ///
    /// # Errors
    /// Returns [`ApprovalError::Domain`] if the run does not exist or is
    /// not in `working` state.
    pub async fn request(&self, approval: ApprovalRequest) -> Result<(), ApprovalError> {
        let project_id = self.project_id;
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.create_approval(&approval).map(|c| {
                    embed_envelope(serde_json::json!({ "sequence": c.sequence }), &c.envelope)
                })
            }))
            .await
            .map_err(ApprovalError::Domain)?;
        self.broadcast(&mut result);
        Ok(())
    }

    /// `approval/decide`: records `decision` for `approval_id` and invokes
    /// the configured [`ApprovalCallback`] after recording; a failed
    /// callback keeps the decision and marks the run `protocolUnhealthy`
    /// rather than asking again.
    ///
    /// `load_snapshot` still runs as a caller-side pre-check for
    /// `run_id` and `humanRequired`: reads a decision write never mutates
    /// (approvals never change which run they belong to, and nothing ever
    /// flips `human_required` after creation), so a snapshot of them can
    /// never go stale between this call and the guarded write below.
    /// Ownership is different -- `reconcile/omp` can rebind a task's
    /// `owner_client_instance_id` at any time via
    /// [`crate::domain::DomainRepository::reconcile_ownership`], including
    /// in the window between this call and the guarded write -- so it is
    /// **not** pre-checked here (R71). It is arbitrated exclusively inside
    /// [`DomainRepository::decide_approval`]'s guarded transaction, along
    /// with whether a different decision is already on record (a losing
    /// call is refused with [`ApprovalError::Conflict`]), whether this is
    /// an idempotent replay ([`DecideOutcome::AlreadyDecided`], which
    /// re-applies nothing), and whether the run has already settled
    /// ([`ApprovalError::RunSettled`]). The database actor interleaves
    /// whole `run_domain_op` round trips, so none of these can be
    /// caller-side pre-checks (R70, R71): the guarded write is the sole
    /// arbiter, exactly one `ApprovalDecided` event is journaled per
    /// approval, and only the deciding call fires side effects.
    ///
    /// # Errors
    /// Returns [`ApprovalError::Forbidden`] if `principal_instance_id`
    /// does not own the task, [`ApprovalError::HumanRequired`] if a
    /// human-required approval is decided by a model,
    /// [`ApprovalError::Conflict`] if a different decision is already on
    /// record, and [`ApprovalError::RunSettled`] if the run has already
    /// reached a terminal state.
    pub async fn decide(
        &self,
        approval_id: ApprovalId,
        principal_instance_id: &str,
        decision: &str,
        reason: &crew_protocol::Redacted,
        decided_by: crew_protocol::DecidedBy,
    ) -> Result<DecideOutcome, ApprovalError> {
        let snapshot = self.load_snapshot(approval_id).await?;

        if snapshot.human_required && decided_by != crew_protocol::DecidedBy::Human {
            return Err(ApprovalError::HumanRequired { approval_id });
        }

        let project_id = self.project_id;
        let principal_instance_id_owned = principal_instance_id.to_string();
        let decision_owned = decision.to_string();
        let reason_owned = reason.clone();
        let decided_by_owned = decided_by;
        let mut decide_result = match self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.decide_approval(
                    approval_id,
                    &principal_instance_id_owned,
                    &decision_owned,
                    &reason_owned,
                    decided_by_owned,
                )
                .map(|c| embed_envelope(serde_json::json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
        {
            // The guarded write is the arbiter: a losing racer never journals
            // an event and never reaches the callback below.
            Ok(value) => value,
            Err(DomainError::NotOwner { .. }) => {
                return Err(ApprovalError::Forbidden {
                    instance_id: principal_instance_id.to_string(),
                    approval_id,
                });
            }
            Err(DomainError::AlreadyResolved { existing, .. }) => {
                return if existing == decision {
                    Ok(DecideOutcome::AlreadyDecided)
                } else {
                    Err(ApprovalError::Conflict { approval_id })
                };
            }
            Err(DomainError::RunSettled { .. }) => {
                return Err(ApprovalError::RunSettled {
                    approval_id,
                    run_id: snapshot.run_id,
                });
            }
            Err(err) => return Err(ApprovalError::Domain(err)),
        };
        self.broadcast(&mut decide_result);

        match self.callback.acknowledge(approval_id, decision).await {
            Ok(()) => {
                let run_id = snapshot.run_id;
                let working = RunState::try_from("working").expect("working is valid");
                let mut result = self
                    .db
                    .run_domain_op(Box::new(move |conn| {
                        let mut repo = DomainRepository::new(conn, project_id);
                        repo.transition_run(run_id, &working, None).map(|c| {
                            embed_envelope(
                                serde_json::json!({ "sequence": c.sequence }),
                                &c.envelope,
                            )
                        })
                    }))
                    .await
                    .map_err(ApprovalError::Domain)?;
                self.broadcast(&mut result);
                Ok(DecideOutcome::Decided)
            }
            Err(_) => {
                let run_id = snapshot.run_id;
                let mut result = self
                    .db
                    .run_domain_op(Box::new(move |conn| {
                        let mut repo = DomainRepository::new(conn, project_id);
                        repo.set_run_flag(run_id, RunFlag::ProtocolUnhealthy, true)
                            .map(|c| {
                                embed_envelope(
                                    serde_json::json!({ "sequence": c.sequence }),
                                    &c.envelope,
                                )
                            })
                    }))
                    .await
                    .map_err(ApprovalError::Domain)?;
                self.broadcast(&mut result);
                Ok(DecideOutcome::DecidedCallbackFailed)
            }
        }
    }

    /// Broadcasts the envelope embedded by a mutation's `run_domain_op`
    /// closure to live subscribers, if present, then strips it so the
    /// caller's JSON-RPC response never carries the internal key.
    fn broadcast(&self, value: &mut Value) {
        broadcast_committed(&self.events_tx, value);
    }

    async fn load_snapshot(
        &self,
        approval_id: ApprovalId,
    ) -> Result<ApprovalSnapshot, ApprovalError> {
        let value: Value = self
            .db
            .run_domain_op(Box::new(move |conn| {
                conn.query_row(
                    "SELECT run_id, human_required FROM approvals WHERE approval_id = ?1",
                    [approval_id.to_string()],
                    |row| {
                        Ok(serde_json::json!({
                            "runId": row.get::<_, String>(0)?,
                            "humanRequired": row.get::<_, i64>(1)? != 0,
                        }))
                    },
                )
                .map_err(|_| DomainError::NotFound {
                    kind: "approval",
                    id: approval_id.to_string(),
                })
            }))
            .await
            .map_err(ApprovalError::Domain)?;

        Ok(ApprovalSnapshot {
            run_id: RunId::parse(value["runId"].as_str().unwrap_or_default()).map_err(|_| {
                ApprovalError::NotFound {
                    kind: "run",
                    id: "invalid".to_string(),
                }
            })?,
            human_required: value["humanRequired"].as_bool().unwrap_or(false),
        })
    }
}

struct ApprovalSnapshot {
    run_id: RunId,
    human_required: bool,
}
