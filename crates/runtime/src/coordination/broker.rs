//! The coordination broker: the worker-safe messaging and task-signal
//! surface a supervised vendor process uses through its scope-bound
//! connection.
//!
//! Record-before-delivery: every send commits `recorded` first (one
//! durable event + projection row), then attempts delivery and commits
//! the outcome (`sent`, `acknowledged`, `failed`, or `unknown`). A runtime
//! crash between the two commits leaves the message `sent`/`recorded` --
//! [`CoordinationBroker::sweep_unacknowledged_as_unknown`] settles any
//! message left in a non-terminal delivery state after recovery to
//! `unknown`; it never resends automatically.

use std::sync::Arc;

use batman_protocol::{
    COORDINATION_PAYLOAD_MAX_BYTES, DeliveryState, EventEnvelope, MessageId, MessageKind,
    ProjectId, RunId, RunMessage, RunState, TaskId, Timestamp, WorkerId, error_code,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::db::DatabaseHandle;
use crate::domain::{DomainRepository, embed_envelope, take_envelope};

use super::rate_limit::RateLimiter;

/// A JSON-RPC-shaped error, matching [`crate::service::ServiceError`]'s
/// shape so the connection dispatch layer can map either uniformly.
#[derive(Debug)]
pub struct CoordinationError {
    pub code: i32,
    pub message: String,
}

impl CoordinationError {
    fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: error_code::INVALID_PARAMS,
            message: msg.into(),
        }
    }

    fn run_settled(run_id: RunId) -> Self {
        Self {
            code: error_code::INVALID_PARAMS,
            message: format!("run {run_id} has already settled; cannot address it"),
        }
    }

    fn policy_quarantined(run_id: RunId) -> Self {
        Self {
            code: error_code::POLICY_QUARANTINED,
            message: format!(
                "run {run_id} is quarantined pending policy/violation/decide; artifact publication is blocked"
            ),
        }
    }
}

impl From<crate::domain::DomainError> for CoordinationError {
    fn from(err: crate::domain::DomainError) -> Self {
        match err {
            crate::domain::DomainError::PolicyQuarantined { run_id } => Self {
                code: error_code::POLICY_QUARANTINED,
                message: format!(
                    "run {run_id} is quarantined by an undecided policy violation; \
                     ask OMP to decide it via policy/violation/decide"
                ),
            },
            // The in-transaction liveness guard (R94) must present the
            // same error require_live_run's pre-check does, not an
            // internal error.
            crate::domain::DomainError::RunSettled { run_id } => Self {
                code: error_code::INVALID_PARAMS,
                message: format!("run {run_id} has already settled"),
            },
            other => Self {
                code: error_code::INTERNAL_ERROR,
                message: other.to_string(),
            },
        }
    }
}

/// Routes the worker-safe `coordination/*` operations to the domain
/// repository, enforcing message bounds, reply visibility, task
/// ownership, and the per-sender rate limit before any journaling. The
/// byte bound and the rate-limit budget are shared by every journaling
/// call -- `send`, `requestChild`, and `publishArtifact` -- not just
/// `send`.
pub struct CoordinationBroker {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    rate_limiter: RateLimiter,
    events_tx: broadcast::Sender<EventEnvelope>,
    lease_service: Arc<crate::workspace::LeaseService>,
    artifact_store: Arc<crate::workspace::ArtifactStore>,
}

impl CoordinationBroker {
    #[must_use]
    pub fn new(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
        lease_service: Arc<crate::workspace::LeaseService>,
        artifact_store: Arc<crate::workspace::ArtifactStore>,
    ) -> Self {
        Self {
            db,
            project_id,
            rate_limiter: RateLimiter::default(),
            events_tx,
            lease_service,
            artifact_store,
        }
    }

    /// Broadcasts the envelope embedded by a mutation's `run_domain_op`
    /// closure to live subscribers, if present, then strips it so the
    /// caller's JSON-RPC response never carries the internal key.
    fn broadcast(&self, value: &mut Value) {
        if let Some(envelope) = take_envelope(value) {
            let _ = self.events_tx.send(envelope);
        }
    }

    /// Rejects any worker-safe operation against a run that has already
    /// settled (reached a terminal state: `succeeded`, `failed`,
    /// `cancelled`, or `lost`). A scope token is only revoked promptly
    /// on observed vendor-process exit (see
    /// `crate::coordination::scope_token`'s module doc) or explicit
    /// adapter disposal -- neither is guaranteed to have happened yet
    /// the instant a run settles, so this check is independent of (and
    /// a stronger, immediate guarantee than) token revocation: a
    /// connection whose token is technically still live must still
    /// never be able to mutate or observe state for a run that is no
    /// longer active.
    async fn require_live_run(&self, run_id: RunId) -> Result<(), CoordinationError> {
        let state: String = self
            .db
            .run_domain_op(crate::service::query::run_state_op(run_id))
            .await?
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let run_state = RunState::try_from(state.as_str())
            .map_err(|_| CoordinationError::invalid_params("stored run has an invalid state"))?;
        if run_state.is_terminal() {
            return Err(CoordinationError::run_settled(run_id));
        }
        Ok(())
    }

    /// Rejects `coordination/publishArtifact` against a run currently
    /// quarantined by [`crate::policy::ViolationService`] (Hardening plan
    /// Task 1's mid-run nested-worker policy violation) -- only lifted via
    /// `policy/violation/decide`.
    async fn require_not_quarantined(&self, run_id: RunId) -> Result<(), CoordinationError> {
        let quarantined: bool = self
            .db
            .run_domain_op(Box::new(move |conn| {
                conn.query_row(
                    "SELECT flags_policy_quarantined FROM runs WHERE run_id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .map(|flag: i64| json!({ "quarantined": flag != 0 }))
                .map_err(|_| crate::domain::DomainError::NotFound {
                    kind: "run",
                    id: run_id.to_string(),
                })
            }))
            .await?
            .get("quarantined")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if quarantined {
            return Err(CoordinationError::policy_quarantined(run_id));
        }
        Ok(())
    }

    /// Rejects caller-supplied text whose UTF-8 byte length exceeds
    /// [`COORDINATION_PAYLOAD_MAX_BYTES`] before any of it can be
    /// journaled. `field` names the wire field so a worker that supplies
    /// several strings learns which one it oversized.
    fn reject_oversized(field: &str, value: &str) -> Result<(), CoordinationError> {
        if value.len() > COORDINATION_PAYLOAD_MAX_BYTES {
            return Err(CoordinationError::invalid_params(format!(
                "{field} of {} bytes exceeds the {}-byte maximum",
                value.len(),
                COORDINATION_PAYLOAD_MAX_BYTES
            )));
        }
        Ok(())
    }

    /// Charges one unit of `sender`'s per-minute budget. Every journaling
    /// coordination call draws on the *same* window -- `send`,
    /// `requestChild`, and `publishArtifact` alike -- so a worker cannot
    /// evade the limit by rotating between methods. Charged as soon as the
    /// sender's identity is known and always before journaling: a call
    /// refused by an earlier gate (settled run, oversized text, quarantine)
    /// costs no budget, while one that clears the charge and is refused by
    /// a later gate (`send`'s task-ownership or `replyTo` visibility
    /// checks) has already spent its unit.
    fn charge_rate_limit(&self, sender: WorkerId) -> Result<(), CoordinationError> {
        self.rate_limiter
            .check(sender, std::time::Instant::now())
            .map_err(|err| CoordinationError {
                code: error_code::RATE_LIMITED,
                message: err.to_string(),
            })
    }

    /// `coordination/send`: validates bounds, reply visibility, and task
    /// ownership, checks the rate limit, then records the message
    /// (`recorded`). This broker has no `RunDriver` to hand the message to,
    /// so it cannot attempt delivery at all -- unlike `OrchestrationService`'s
    /// `message/send`, it must never advance the message to `sent`, which
    /// would claim a delivery attempt that structurally never happened. The
    /// message settles at `recorded` until a future adapter integration
    /// (or, on a crash, [`Self::sweep_unacknowledged_as_unknown`]) advances
    /// or resolves it.
    #[allow(clippy::too_many_arguments)]
    pub async fn send(
        &self,
        run_id: RunId,
        sender_worker_id: WorkerId,
        task_id: TaskId,
        kind: MessageKind,
        payload: String,
        recipient_worker_id: Option<WorkerId>,
        reply_to: Option<MessageId>,
    ) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        Self::reject_oversized("payload", &payload)?;

        self.charge_rate_limit(sender_worker_id)?;

        // A child (a run's own messages) cannot address a task other than
        // the one its run belongs to.
        let run_task_id: String = self
            .db
            .run_domain_op(Box::new(move |conn| {
                conn.query_row(
                    "SELECT task_id FROM runs WHERE run_id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map(|task_id| json!({ "taskId": task_id }))
                .map_err(crate::domain::DomainError::Sqlite)
            }))
            .await
            .map_err(|_| CoordinationError::invalid_params(format!("run {run_id} not found")))?
            .get("taskId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if run_task_id != task_id.to_string() {
            return Err(CoordinationError::invalid_params(
                "a run cannot address a task other than its own",
            ));
        }

        // replyTo must reference a visible prior message on this run.
        if let Some(reply_to) = reply_to {
            let exists: bool = self
                .db
                .run_domain_op(Box::new(move |conn| {
                    conn.query_row(
                        "SELECT 1 FROM messages WHERE message_id = ?1 AND run_id = ?2",
                        rusqlite::params![reply_to.to_string(), run_id.to_string()],
                        |_| Ok(true),
                    )
                    .map(|found| json!({ "found": found }))
                    .or_else(|_| Ok(json!({ "found": false })))
                }))
                .await
                .map(|v| v["found"].as_bool().unwrap_or(false))
                .unwrap_or(false);
            if !exists {
                return Err(CoordinationError::invalid_params(format!(
                    "replyTo {reply_to} does not reference a visible prior message on this run"
                )));
            }
        }

        let message_id = MessageId::new();
        let message = RunMessage {
            message_id,
            run_id,
            sender_worker_id,
            recipient_worker_id,
            task_id,
            kind,
            payload,
            delivery_state: DeliveryState::Recorded,
            created_at: Timestamp::now(),
            sent_at: None,
            acknowledged_at: None,
            reply_to,
        };

        let project_id = self.project_id;
        let mut recorded_sequence = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_message(&message, None, false, true)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await?;
        self.broadcast(&mut recorded_sequence);

        // No `RunDriver` exists on this path -- unlike
        // `OrchestrationService::message_send` -- so there is no delivery
        // to attempt. The message stays `recorded`; advancing it to `sent`
        // here would claim a delivery attempt that never happened.
        Ok(json!({
            "messageId": message_id.to_string(),
            "deliveryState": "recorded",
            "recordedSequence": recorded_sequence["sequence"],
        }))
    }

    /// `coordination/task`: the worker-safe view of the task bound to
    /// `run_id`'s scope.
    pub async fn task(&self, run_id: RunId) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        self.db
            .run_domain_op(Box::new(move |conn| {
                conn.query_row(
                    "SELECT t.task_id, t.owner_client_instance_id, t.revision
                     FROM runs r JOIN tasks t ON r.task_id = t.task_id
                     WHERE r.run_id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok(json!({
                            "taskId": row.get::<_, String>(0)?,
                            "ownerClientInstanceId": row.get::<_, String>(1)?,
                            "revision": row.get::<_, i64>(2)?,
                        }))
                    },
                )
                .map_err(|_| crate::domain::DomainError::NotFound {
                    kind: "run",
                    id: run_id.to_string(),
                })
            }))
            .await
            .map_err(Into::into)
    }

    /// `coordination/peers`: sibling workers on the same task as `run_id`.
    pub async fn peers(&self, run_id: RunId) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        self.db
            .run_domain_op(Box::new(move |conn| {
                let task_id: String = conn
                    .query_row(
                        "SELECT task_id FROM runs WHERE run_id = ?1",
                        [run_id.to_string()],
                        |row| row.get(0),
                    )
                    .map_err(|_| crate::domain::DomainError::NotFound {
                        kind: "run",
                        id: run_id.to_string(),
                    })?;

                let mut stmt = conn.prepare(
                    "SELECT DISTINCT w.worker_id, p.adapter, r.run_id
                     FROM runs r JOIN workers w ON r.worker_id = w.worker_id
                     JOIN worker_profiles p ON w.profile_id = p.id
                     WHERE r.task_id = ?1 AND r.run_id != ?2",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![task_id, run_id.to_string()], |row| {
                        Ok(json!({
                            "workerId": row.get::<_, String>(0)?,
                            "adapter": row.get::<_, String>(1)?,
                            "runId": row.get::<_, String>(2)?,
                        }))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!({ "peers": rows }))
            }))
            .await
            .map_err(Into::into)
    }

    /// `coordination/peerWorkspace`: discovers the workspace path of a
    /// peer run on the same task, by the peer's run id. Rejects a peer on
    /// a different task -- a worker may only discover workspaces of runs
    /// coordinating on its own task, never an arbitrary run id.
    pub async fn peer_workspace(
        &self,
        run_id: RunId,
        peer_run_id: RunId,
    ) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        let (task_id, _) = self.run_participants(run_id).await?;
        let (peer_task_id, _) = self.run_participants(peer_run_id).await?;
        if peer_task_id != task_id {
            return Err(CoordinationError::invalid_params(
                "peerRunId does not belong to this run's task",
            ));
        }
        let info = self
            .lease_service
            .active_for_run(peer_run_id)
            .map_err(|e| CoordinationError {
                code: error_code::INTERNAL_ERROR,
                message: e.to_string(),
            })?
            .ok_or_else(|| {
                CoordinationError::invalid_params(format!(
                    "peer run {peer_run_id} has no active workspace lease"
                ))
            })?;
        Ok(json!({
            "path": info.path,
            "mode": match info.mode {
                batman_protocol::LeaseMode::ReadOnly => "readOnly",
                batman_protocol::LeaseMode::Write => "write",
            },
            "isolationKind": match info.isolation_kind {
                batman_protocol::IsolationKind::Shared => "shared",
                batman_protocol::IsolationKind::GitWorktree => "gitWorktree",
                batman_protocol::IsolationKind::Copy => "copy",
            },
            "state": match info.state {
                batman_protocol::WorkspaceState::Allocating => "allocating",
                batman_protocol::WorkspaceState::Active => "active",
                batman_protocol::WorkspaceState::Dirty => "dirty",
                batman_protocol::WorkspaceState::Released => "released",
                batman_protocol::WorkspaceState::CleanupFailed => "cleanupFailed",
            },
        }))
    }

    /// The set of run ids coordinating on the same task as `run_id`,
    /// including `run_id` itself. This is the scope every worker-facing
    /// artifact query is filtered by.
    ///
    /// Returned as strings because that is how `Artifact::run_id` carries
    /// its provenance -- reparsing on both sides would only add a failure
    /// mode.
    async fn task_run_ids(&self, run_id: RunId) -> Result<Vec<String>, CoordinationError> {
        let (task_id, _) = self.run_participants(run_id).await?;
        self.db
            .run_domain_op(Box::new(move |conn| {
                let mut stmt = conn.prepare("SELECT run_id FROM runs WHERE task_id = ?1")?;
                let rows = stmt
                    .query_map([task_id.to_string()], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!(rows))
            }))
            .await
            .map_err(CoordinationError::from)
            .map(|value| {
                value
                    .as_array()
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|v| v.as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            })
    }

    /// `coordination/artifactList`: artifacts published by any run on the
    /// caller's task. Project-wide `artifact/list` is deliberately not
    /// reused: a worker must never see another task's artifacts.
    pub async fn artifact_list(
        &self,
        run_id: RunId,
        kind: Option<batman_protocol::ArtifactKind>,
    ) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        let scope = self.task_run_ids(run_id).await?;
        let listed = self.artifact_store.list(kind).await;
        let artifacts: Vec<_> = listed
            .artifacts
            .into_iter()
            .filter(|a| {
                a.run_id
                    .as_deref()
                    .is_some_and(|id| scope.iter().any(|s| s == id))
            })
            .collect();
        Ok(json!({ "artifacts": artifacts }))
    }

    /// `coordination/artifactFetch`: one bounded chunk of an artifact
    /// published on the caller's task.
    ///
    /// An artifact outside the caller's task and an artifact that does not
    /// exist return the *same* message, so a worker cannot probe the
    /// project's artifact space for ids it is not entitled to see.
    pub async fn artifact_fetch(
        &self,
        run_id: RunId,
        artifact_id: batman_protocol::ArtifactId,
        offset: u64,
    ) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        let scope = self.task_run_ids(run_id).await?;
        let not_on_task =
            || CoordinationError::invalid_params("artifactId is not an artifact on this task");

        let metadata = self
            .artifact_store
            .fetch(&artifact_id)
            .await
            .map_err(|_| not_on_task())?;
        if !metadata
            .run_id
            .as_deref()
            .is_some_and(|id| scope.iter().any(|s| s == id))
        {
            return Err(not_on_task());
        }

        // The worker never chooses a length: it always gets one capped
        // chunk and paginates with `nextOffset`.
        let result = self
            .artifact_store
            .fetch_chunked(
                &artifact_id,
                offset,
                crate::workspace::ARTIFACT_FETCH_MAX_BYTES,
            )
            .await
            .map_err(|e| CoordinationError {
                code: error_code::INTERNAL_ERROR,
                message: e.to_string(),
            })?;
        serde_json::to_value(result).map_err(|e| CoordinationError {
            code: error_code::INTERNAL_ERROR,
            message: e.to_string(),
        })
    }

    /// `coordination/requestChild`: records intent only, transitions the
    /// requesting run to `waitingPeer`, and notifies OMP (via the durable
    /// event journal OMP already replays). Never creates a task or worker.
    /// Bounds `reason` to [`COORDINATION_PAYLOAD_MAX_BYTES`] and charges
    /// the requesting worker's shared per-sender budget before journaling,
    /// because the reason text lands verbatim in a durable
    /// `ChildWorkerRequested` event.
    pub async fn request_child(
        &self,
        run_id: RunId,
        reason: String,
    ) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        Self::reject_oversized("reason", &reason)?;
        let (_, sender_worker_id) = self.run_participants(run_id).await?;
        self.charge_rate_limit(sender_worker_id)?;
        let project_id = self.project_id;
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.request_child(run_id, &reason)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(CoordinationError::from)?;
        self.broadcast(&mut result);
        Ok(result)
    }

    /// `coordination/publishArtifact`: journals an artifact reference for
    /// the scoped run without a dedicated projection table -- the durable
    /// event is the record. `artifactRef` and `description` are each
    /// bounded to [`COORDINATION_PAYLOAD_MAX_BYTES`] (either one can become
    /// the journaled message payload), and the call charges the run's
    /// worker on the shared per-sender budget before journaling. The
    /// quarantine gate deliberately runs ahead of the charge so a
    /// quarantined worker still sees `POLICY_QUARANTINED`, not
    /// `RATE_LIMITED` -- keep that order.
    pub async fn publish_artifact(
        &self,
        run_id: RunId,
        artifact_ref: String,
        description: Option<String>,
    ) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        Self::reject_oversized("artifactRef", &artifact_ref)?;
        if let Some(description) = &description {
            Self::reject_oversized("description", description)?;
        }
        self.require_not_quarantined(run_id).await?;
        let (task_id, worker_id) = self.run_participants(run_id).await?;
        self.charge_rate_limit(worker_id)?;
        let project_id = self.project_id;
        let kind = MessageKind::PeerMessage;
        let payload = description.unwrap_or_else(|| artifact_ref.clone());
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                let message = RunMessage {
                    message_id: MessageId::new(),
                    run_id,
                    sender_worker_id: worker_id,
                    recipient_worker_id: None,
                    task_id,
                    kind,
                    payload,
                    delivery_state: DeliveryState::Recorded,
                    created_at: Timestamp::now(),
                    sent_at: None,
                    acknowledged_at: None,
                    reply_to: None,
                };
                // In-tx quarantine enforcement (R78): the pre-check above
                // is only the fast path that keeps a steady-state
                // quarantined worker from being charged rate budget; a
                // quarantine landing between that read and this write is
                // refused here, inside the guarded transaction.
                repo.record_message(&message, None, true, true).map(|c| {
                    embed_envelope(
                        json!({ "sequence": c.sequence, "artifactRef": artifact_ref }),
                        &c.envelope,
                    )
                })
            }))
            .await
            .map_err(CoordinationError::from)?;
        self.broadcast(&mut result);
        Ok(result)
    }

    /// `coordination/reportBlocked`: reports the scoped run is blocked, as
    /// a journaled message OMP can observe, without changing ownership.
    pub async fn report_blocked(
        &self,
        run_id: RunId,
        reason: String,
    ) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        let (task_id, worker_id) = self.run_participants(run_id).await?;
        self.send_internal(
            run_id,
            worker_id,
            task_id,
            MessageKind::PeerMessage,
            reason,
            None,
            None,
        )
        .await
    }

    /// `coordination/askPolicy`: asks OMP a policy question, as a
    /// journaled message OMP can observe, without deciding it locally.
    pub async fn ask_policy(
        &self,
        run_id: RunId,
        question: String,
    ) -> Result<Value, CoordinationError> {
        self.require_live_run(run_id).await?;
        let (task_id, worker_id) = self.run_participants(run_id).await?;
        self.send_internal(
            run_id,
            worker_id,
            task_id,
            MessageKind::Question,
            question,
            None,
            None,
        )
        .await
    }

    async fn run_participants(
        &self,
        run_id: RunId,
    ) -> Result<(TaskId, WorkerId), CoordinationError> {
        let value = self
            .db
            .run_domain_op(Box::new(move |conn| {
                conn.query_row(
                    "SELECT task_id, worker_id FROM runs WHERE run_id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok(json!({
                            "taskId": row.get::<_, String>(0)?,
                            "workerId": row.get::<_, String>(1)?,
                        }))
                    },
                )
                .map_err(|_| crate::domain::DomainError::NotFound {
                    kind: "run",
                    id: run_id.to_string(),
                })
            }))
            .await?;
        let task_id = TaskId::parse(value["taskId"].as_str().unwrap_or_default())
            .map_err(|_| CoordinationError::invalid_params("stored run has an invalid taskId"))?;
        let worker_id = WorkerId::parse(value["workerId"].as_str().unwrap_or_default())
            .map_err(|_| CoordinationError::invalid_params("stored run has an invalid workerId"))?;
        Ok((task_id, worker_id))
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_internal(
        &self,
        run_id: RunId,
        sender_worker_id: WorkerId,
        task_id: TaskId,
        kind: MessageKind,
        payload: String,
        recipient_worker_id: Option<WorkerId>,
        reply_to: Option<MessageId>,
    ) -> Result<Value, CoordinationError> {
        self.send(
            run_id,
            sender_worker_id,
            task_id,
            kind,
            payload,
            recipient_worker_id,
            reply_to,
        )
        .await
    }

    /// Settles any message left in a non-terminal delivery state
    /// (`recorded` or `sent`, never acknowledged or failed) to `unknown`.
    /// Call once at startup, after the durable journal has been recovered:
    /// a crash between record-intent and adapter acknowledgement leaves
    /// exactly this state, and this sweep never resends -- it only
    /// reclassifies the outcome as unknown.
    pub async fn sweep_unacknowledged_as_unknown(&self) -> Result<u64, CoordinationError> {
        let project_id = self.project_id;
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let ids: Vec<String> = {
                    let mut stmt = conn.prepare(
                        "SELECT message_id FROM messages WHERE delivery_state IN ('recorded', 'sent')",
                    )?;
                    stmt.query_map([], |row| row.get::<_, String>(0))?
                        .collect::<Result<Vec<_>, _>>()?
                };
                let mut repo = DomainRepository::new(conn, project_id);
                let mut count = 0u64;
                let mut envelopes = Vec::new();
                for id in ids {
                    let Ok(message_id) = MessageId::parse(&id) else { continue };
                    let committed = repo.update_delivery(message_id, &DeliveryState::Unknown)?;
                    envelopes.push(committed.envelope);
                    count += 1;
                }
                Ok(json!({ "swept": count, "__envelopes": envelopes }))
            }))
            .await?;
        let swept = result["swept"].as_u64().unwrap_or(0);
        if let Some(envelopes) = result.as_object_mut().and_then(|m| m.remove("__envelopes"))
            && let Ok(envelopes) = serde_json::from_value::<Vec<EventEnvelope>>(envelopes)
        {
            for envelope in envelopes {
                let _ = self.events_tx.send(envelope);
            }
        }
        Ok(swept)
    }

    /// Executes one MCP-shaped coordination tool call in-process,
    /// against `scope` -- the caller's own already-bound, immutable
    /// run/task/worker identity, never anything read out of
    /// `arguments`. For a caller with a real authenticated socket
    /// connection (an external MCP subprocess), that connection's own
    /// `dispatch_coordination` in `crate::ipc::connection` is the right
    /// path instead; this method exists for a caller that *is* the
    /// trusted runtime itself and has no such connection to make (see
    /// `OmpRpcAdapter`'s host-tool bridge, which owns its `run_id`/
    /// `task_id`/`worker_id` from construction, exactly as every other
    /// adapter does).
    ///
    /// Always returns a value shaped by
    /// [`super::mcp_protocol::tool_result_from_success`]/
    /// [`super::mcp_protocol::tool_result_from_error`] -- never a raw
    /// [`CoordinationError`] -- because a host-tool-call reply is
    /// itself a normal (if `isError: true`) result, not a transport
    /// failure.
    pub async fn execute_tool_call(
        &self,
        name: &str,
        arguments: &Value,
        scope: super::mcp_protocol::BoundScope,
    ) -> Value {
        let (method, params) =
            match super::mcp_protocol::translate_tool_call(name, arguments, scope) {
                Ok(translated) => translated,
                Err(err) => return super::mcp_protocol::tool_result_from_error(&err.to_string()),
            };

        let result = match method {
            "coordination/task" => self.task(scope.run_id).await,
            "coordination/peers" => self.peers(scope.run_id).await,
            "coordination/send" => {
                let kind: MessageKind = match serde_json::from_value(params["kind"].clone()) {
                    Ok(kind) => kind,
                    Err(err) => {
                        return super::mcp_protocol::tool_result_from_error(&format!(
                            "invalid kind: {err}"
                        ));
                    }
                };
                let payload = params["payload"].as_str().unwrap_or_default().to_string();
                let recipient_worker_id = params
                    .get("recipientWorkerId")
                    .and_then(Value::as_str)
                    .map(WorkerId::parse)
                    .transpose();
                let reply_to = params
                    .get("replyTo")
                    .and_then(Value::as_str)
                    .map(MessageId::parse)
                    .transpose();
                match (recipient_worker_id, reply_to) {
                    (Ok(recipient_worker_id), Ok(reply_to)) => {
                        self.send(
                            scope.run_id,
                            scope.worker_id,
                            scope.task_id,
                            kind,
                            payload,
                            recipient_worker_id,
                            reply_to,
                        )
                        .await
                    }
                    _ => Err(CoordinationError::invalid_params(
                        "recipientWorkerId/replyTo is not a valid id",
                    )),
                }
            }
            "coordination/requestChild" => {
                let reason = params["reason"].as_str().unwrap_or_default().to_string();
                self.request_child(scope.run_id, reason).await
            }
            "coordination/publishArtifact" => {
                let artifact_ref = params["artifactRef"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let description = params
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.publish_artifact(scope.run_id, artifact_ref, description)
                    .await
            }
            "coordination/reportBlocked" => {
                let reason = params["reason"].as_str().unwrap_or_default().to_string();
                self.report_blocked(scope.run_id, reason).await
            }
            "coordination/askPolicy" => {
                let question = params["question"].as_str().unwrap_or_default().to_string();
                self.ask_policy(scope.run_id, question).await
            }
            "coordination/peerWorkspace" => match params["peerRunId"].as_str().map(RunId::parse) {
                Some(Ok(peer_run_id)) => self.peer_workspace(scope.run_id, peer_run_id).await,
                _ => Err(CoordinationError::invalid_params(
                    "peerRunId is not a valid id",
                )),
            },
            "coordination/artifactList" => {
                let kind = match params.get("kind") {
                    Some(raw) => match serde_json::from_value(raw.clone()) {
                        Ok(kind) => Some(kind),
                        Err(_) => {
                            return super::mcp_protocol::tool_result_from_error(
                                "kind is not a valid artifact kind",
                            );
                        }
                    },
                    None => None,
                };
                self.artifact_list(scope.run_id, kind).await
            }
            "coordination/artifactFetch" => {
                let artifact_id = params
                    .get("artifactId")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok());
                match artifact_id {
                    Some(artifact_id) => {
                        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
                        self.artifact_fetch(scope.run_id, artifact_id, offset).await
                    }
                    None => Err(CoordinationError::invalid_params(
                        "artifactId is not a valid id",
                    )),
                }
            }
            other => Err(CoordinationError {
                code: error_code::METHOD_NOT_FOUND,
                message: format!("{other} is not routed through CoordinationBroker"),
            }),
        };

        match result {
            Ok(value) => super::mcp_protocol::tool_result_from_success(name, &value)
                .unwrap_or_else(|err| {
                    super::mcp_protocol::tool_result_from_error(&err.to_string())
                }),
            Err(err) => super::mcp_protocol::tool_result_from_error(&err.message),
        }
    }
}
