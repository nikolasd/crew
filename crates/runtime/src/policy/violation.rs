//! The mid-run nested-worker policy violation flow (Hardening plan Task 1).
//!
//! Distinct from [`super::evaluate::PolicyEvaluator`], which only enforces
//! nested-worker policy at pre-authorization time (before a worker starts).
//! [`ViolationService`] handles the complementary case: a worker that is
//! already running and then unexpectedly reports a child mid-run, via
//! `AdapterEventPayload::NestedWorkerObserved` while the run's effective
//! `nested` capability is `NestedCapability::None`.
//!
//! On [`ViolationService::record_nested_worker`]: atomically persists a
//! [`batman_protocol::RuntimeEvent::PolicyViolationRecorded`], then applies
//! the configured [`NestedViolationAction`] -- `Quarantine` sets
//! `Run.flags.policyQuarantined` (blocking `message/send`,
//! `workspace/apply`, and `coordination/publishArtifact` -- see
//! `crate::service::orchestration`/`crate::coordination::broker`);
//! `Cancel` creates an audited cancellation intent and cancels the run
//! directly; `QuarantineAndCancel` (the default) does both. Idempotent
//! for the quarantine flag: a run already quarantined or already terminal
//! still gets a durable `PolicyViolationRecorded` event (so OMP sees every
//! subsequent unexpected child), but the flag is applied at most once
//! (R75) -- see [`ViolationService::record_nested_worker`] for why the
//! cancellation side effects of `Cancel`/`QuarantineAndCancel` do not
//! share that guarantee.
//!
//! Crew-v2 gap-closure WP5: a sibling `record_cost_ceiling` used to journal
//! a `cost_ceiling_exceeded` violation through the same `apply_action`
//! path, for the org-governance cost ceiling retired in that WP (deleted
//! outright -- config-sourced, and that config layer was never reachable
//! in production). Nested-worker handling below is unaffected.
//!
//! [`ViolationService::decide`] resolves a violation via
//! `policy/violation/decide`, restricted to the violation's task's
//! `owner_client_instance_id` (the owning `ompExtension` client). That
//! ownership check is arbitrated exclusively inside the guarded
//! transaction in
//! [`crate::domain::repository::DomainRepository::resolve_policy_violation`]
//! (R72), the same pattern
//! [`crate::approval::ApprovalService::decide`] uses in
//! [`crate::domain::repository::DomainRepository::decide_approval`] (R71):
//! a `reconcile/omp` ownership rebind landing in the window between a
//! caller-side snapshot read and the write could otherwise slip past a
//! pre-check and leave a stale owner's decision unrefused, so neither
//! service pre-checks ownership caller-side any longer. Whether a
//! decision may commit at all -- ownership, conflict, idempotent replay,
//! settled run -- is enforced inside that same guarded write, so two
//! concurrent `decide` calls cannot both journal a decision or both fire
//! side effects (R54, R72). Releasing quarantine on an
//! already-terminal/cancelled run is refused in that same transaction; it
//! must never be revived.
//!
//! `apply_action`'s idempotency check and `decide`'s release-time
//! un-quarantine are both arbitrated inside guarded writes rather than
//! caller-held snapshots (R75), the same doctrine applied to ownership
//! above: [`crate::domain::repository::DomainRepository::record_policy_violation`]
//! re-reads `Run.flags.policyQuarantined` and run state immediately
//! before the same call's journal commit -- not a separate, earlier
//! `run_domain_op` round trip a concurrent release could land in --  and
//! reports whether the run was already actioned; and
//! [`crate::domain::repository::DomainRepository::release_quarantine`]
//! refuses to clear the flag if a *different* violation on the run is
//! still unresolved, so a release targeting one violation can never
//! silently un-quarantine a run for another, still-open one.

use std::sync::Arc;

use batman_protocol::{
    EventEnvelope, PolicyViolationId, ProjectId, RunId, RunState, TaskId, WorkerId,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::config::NestedViolationAction;
use crate::db::{DatabaseHandle, DbError};
use crate::domain::{DomainError, DomainRepository, RunFlag, embed_envelope, take_envelope};
use crate::security::redaction::Redactor;
use crate::service::RunDriver;

/// Journaled `code` for a mid-run child observed against
/// `NestedCapability::None`.
pub const VIOLATION_CODE_NESTED_WORKER_DENIED: &str = "nested_worker_denied";

/// Errors returned by [`ViolationService`] operations.
#[derive(Debug, thiserror::Error)]
pub enum ViolationError {
    /// A database operation failed.
    #[error(transparent)]
    Domain(DomainError),
    /// `principal_instance_id` does not own the violation's task.
    #[error("client {instance_id} does not own the task for policy violation {violation_id}")]
    Forbidden {
        instance_id: String,
        violation_id: PolicyViolationId,
    },
    /// The violation was already decided with a different resolution.
    #[error("policy violation {violation_id} was already decided with a different resolution")]
    Conflict { violation_id: PolicyViolationId },
    /// Releasing quarantine on an already-terminal run would revive it.
    #[error("run {run_id} has already settled; cannot release its quarantine")]
    RunSettled {
        violation_id: PolicyViolationId,
        run_id: RunId,
    },
    /// `resolution` was not `"release"` or `"cancel"`.
    #[error("invalid resolution {resolution:?}; expected \"release\" or \"cancel\"")]
    InvalidResolution { resolution: String },
    /// No violation exists with the given id.
    #[error("policy violation {violation_id} not found")]
    NotFound { violation_id: PolicyViolationId },
    /// Recording the audited cancellation intent failed.
    #[error(transparent)]
    Db(#[from] DbError),
}

/// The outcome of [`ViolationService::decide`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecideOutcome {
    /// The violation was newly resolved.
    Decided,
    /// The violation was already resolved with this same resolution.
    AlreadyDecided,
}

/// Records and resolves mid-run nested-worker policy violations.
pub struct ViolationService {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    events_tx: broadcast::Sender<EventEnvelope>,
    run_driver: Option<Arc<dyn RunDriver>>,
    /// The configured `nestedViolationAction`, applied uniformly to every
    /// violation this daemon instance records (a single runtime-policy
    /// value, not per-run).
    action: NestedViolationAction,
}

impl ViolationService {
    #[must_use]
    pub fn new(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
        run_driver: Option<Arc<dyn RunDriver>>,
        action: NestedViolationAction,
    ) -> Self {
        Self {
            db,
            project_id,
            events_tx,
            run_driver,
            action,
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

    /// Loads a run's `policy_fingerprint` -- the merged policy it was
    /// authorized under -- to make the journaled violation auditable
    /// against a specific policy.
    ///
    /// No longer also reads `state`/`flags`: the idempotency check that
    /// used to live here (`already_actioned = flags.policy_quarantined ||
    /// state.is_terminal()`, read one whole `run_domain_op` round trip
    /// before the violation was journaled) is now arbitrated inside
    /// [`DomainRepository::record_policy_violation`]'s own call, re-read
    /// immediately before that same commit rather than a round trip
    /// earlier -- a stale value here could no longer be trusted once a
    /// concurrent `decide("release")` could commit in the gap (R75).
    ///
    /// The fingerprint is `None` for runs created before migration 6; it is
    /// journaled as an empty string rather than a fabricated value.
    async fn load_policy_fingerprint(&self, run_id: RunId) -> Result<String, ViolationError> {
        let value = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let fingerprint: Option<String> = conn
                    .query_row(
                        "SELECT policy_fingerprint FROM runs WHERE run_id = ?1",
                        [run_id.to_string()],
                        |row| row.get(0),
                    )
                    .map_err(|_| DomainError::NotFound {
                        kind: "run",
                        id: run_id.to_string(),
                    })?;
                Ok(json!(fingerprint))
            }))
            .await
            .map_err(ViolationError::Domain)?;
        Ok(value.as_str().unwrap_or_default().to_string())
    }

    /// Called by [`crate::adapter::event_sink::DomainAdapterEventSink`]
    /// when `NestedWorkerObserved` fires while the run's effective
    /// `nested` capability is `NestedCapability::None`.
    ///
    /// Idempotent for the quarantine flag: if the run is already
    /// quarantined or already terminal (a prior call already applied
    /// `action`), this still journals `PolicyViolationRecorded` -- so OMP
    /// sees every subsequent unexpected child -- but does not re-apply
    /// the flag. That `already_actioned` judgment comes back from
    /// [`DomainRepository::record_policy_violation`] itself, read inside
    /// the same call as the journal commit (R75), not from a caller-side
    /// snapshot taken a round trip earlier -- see
    /// [`DomainRepository::release_quarantine`] for why that is enough to
    /// keep the flag exactly-once even against a racing release.
    ///
    /// Not idempotent for the cancellation side effects: `already_actioned`
    /// is still a single value [`Self::apply_action`] consumes at least
    /// one more `run_domain_op` round trip later, so two concurrent
    /// violations with `action = Cancel`/`QuarantineAndCancel` can both
    /// observe `already_actioned = false` and each create an audited
    /// cancellation intent and attempt [`Self::cancel_and_transition`];
    /// the losing transition simply fails and is only logged. That
    /// residue is pre-existing, outside this fix's mechanism, and tracked
    /// as its own `REVIEW.md` finding rather than fixed here.
    ///
    /// # Errors
    /// Returns [`ViolationError::Domain`] if `run_id` does not exist.
    pub async fn record_nested_worker(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        vendor_child_id: &str,
        vendor_parent_ref: &str,
        observed_event_sequence: u64,
    ) -> Result<(), ViolationError> {
        let policy_fingerprint = self.load_policy_fingerprint(run_id).await?;

        let violation_id = PolicyViolationId::new();
        let project_id = self.project_id;
        let vendor_child_id_owned = vendor_child_id.to_string();
        let vendor_parent_ref_owned = vendor_parent_ref.to_string();
        let action_str = self.action.to_string();
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_policy_violation(
                    violation_id,
                    run_id,
                    task_id,
                    worker_id,
                    VIOLATION_CODE_NESTED_WORKER_DENIED,
                    observed_event_sequence,
                    &policy_fingerprint,
                    Some(&vendor_child_id_owned),
                    Some(&vendor_parent_ref_owned),
                    &action_str,
                )
                .map(|outcome| {
                    embed_envelope(
                        json!({
                            "sequence": outcome.committed.sequence,
                            "alreadyActioned": outcome.already_actioned,
                        }),
                        &outcome.committed.envelope,
                    )
                })
            }))
            .await
            .map_err(ViolationError::Domain)?;
        let already_actioned = result["alreadyActioned"].as_bool().unwrap_or(false);
        self.broadcast(&mut result);

        self.apply_action(
            run_id,
            worker_id,
            already_actioned,
            Some(vendor_child_id),
            Some(vendor_parent_ref),
        )
        .await
    }

    /// Applies `self.action` after a violation has been journaled.
    ///
    /// A no-op when `already_actioned` -- the violation is still journaled by
    /// the caller, but the quarantine flag is not re-applied and no second
    /// cancellation intent is created.
    async fn apply_action(
        &self,
        run_id: RunId,
        worker_id: WorkerId,
        already_actioned: bool,
        vendor_child_id: Option<&str>,
        vendor_parent_ref: Option<&str>,
    ) -> Result<(), ViolationError> {
        if already_actioned {
            return Ok(());
        }

        match self.action {
            NestedViolationAction::Quarantine => {
                self.quarantine(run_id).await?;
            }
            NestedViolationAction::Cancel => {
                let operation_id = self
                    .create_cancellation_intent(
                        run_id,
                        worker_id,
                        vendor_child_id,
                        vendor_parent_ref,
                    )
                    .await?;
                self.cancel_and_transition(run_id, Some(operation_id))
                    .await?;
            }
            NestedViolationAction::QuarantineAndCancel => {
                self.quarantine(run_id).await?;
                let operation_id = self
                    .create_cancellation_intent(
                        run_id,
                        worker_id,
                        vendor_child_id,
                        vendor_parent_ref,
                    )
                    .await?;
                self.cancel_and_transition(run_id, Some(operation_id))
                    .await?;
            }
        }

        Ok(())
    }

    /// Sets `flags.policy_quarantined = true` via
    /// [`DomainRepository::set_run_flag`], which reads the run's current
    /// flags, flips this one, and writes it back all inside its own guarded
    /// call -- no caller-held snapshot is read-modified-written across an
    /// `await`, so a concurrent mutation of a *different* flag on the same
    /// run (e.g. `ApprovalService::decide`'s callback-failure path setting
    /// `protocolUnhealthy`) cannot be silently reverted by this call (R73).
    ///
    /// Always sets the flag `true`: the one call that used to clear it,
    /// `decide`'s release path, now calls
    /// [`ViolationService::release_quarantine`] instead, which can refuse
    /// to clear the flag (R75) -- a decision this method has no reason to
    /// make.
    async fn quarantine(&self, run_id: RunId) -> Result<(), ViolationError> {
        let project_id = self.project_id;
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.set_run_flag(run_id, RunFlag::PolicyQuarantined, true)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ViolationError::Domain)?;
        self.broadcast(&mut result);
        Ok(())
    }

    /// Creates an audited cancellation intent in the `operations` table
    /// (the same mechanism `crate::db::actor` uses elsewhere) before
    /// actually cancelling, per the Hardening plan's requirement that the
    /// nested-worker cancellation path be audited.
    ///
    /// The vendor ids are `None` for a violation with no vendor child, such
    /// as a cost ceiling; the intent then records JSON `null` for them rather
    /// than an empty string that would read as a real, blank id.
    async fn create_cancellation_intent(
        &self,
        run_id: RunId,
        worker_id: WorkerId,
        vendor_child_id: Option<&str>,
        vendor_parent_ref: Option<&str>,
    ) -> Result<batman_protocol::OperationId, ViolationError> {
        use batman_protocol::{OperationId, Timestamp};

        let reason = if vendor_child_id.is_some() {
            "nested-worker policy violation"
        } else {
            "cost-ceiling policy violation"
        };
        let intent = json!({
            "runId": run_id.to_string(),
            "workerId": worker_id.to_string(),
            "vendorChildId": vendor_child_id,
            "vendorParentRef": vendor_parent_ref,
            "reason": reason,
        });
        let sanitized = Redactor::new().sanitize_json(&intent);
        let operation_id = OperationId::new();
        self.db
            .record_operation_intent(
                operation_id,
                "policyViolationCancel",
                sanitized,
                Timestamp::now(),
            )
            .await
            .map_err(ViolationError::Db)?;
        Ok(operation_id)
    }

    /// Transitions the run to `cancelled` and then calls the live adapter's
    /// `cancel(CancelScope::Worker)` if one is running (mirrors
    /// `OrchestrationService::run_cancel`'s subprocess termination).
    ///
    /// Idempotent against a racing sibling observation (R79): two
    /// concurrent violations with a cancelling action both persist an
    /// audited intent (invariant 4 keeps the intent BEFORE the transition)
    /// and both reach this method; the loser's transition fails with
    /// [`DomainError::Transition`] because the winner already terminalized
    /// the run. That is a success, not an error -- the loser acknowledges
    /// its intent as `superseded`, still attempts the adapter kill, and
    /// returns `Ok(())`, so the doc-comment promise on
    /// [`Self::record_nested_worker`] ("idempotent") is finally true and
    /// the `operations` table carries an honest audit trail: one intent
    /// acknowledged `cancelled`, one `superseded`, never two
    /// indistinguishable rows. Only [`DomainError::Transition`] is
    /// classified as superseded; every other error still fails.
    /// `operation_id` is `Some` on the observation paths, which persist an
    /// audited intent first; `policy/violation/decide`'s operator cancel
    /// passes `None` (no intent row exists there) and keeps its
    /// pre-existing strict semantics: a failed transition is an error.
    async fn cancel_and_transition(
        &self,
        run_id: RunId,
        operation_id: Option<batman_protocol::OperationId>,
    ) -> Result<(), ViolationError> {
        // Transition first, mirroring `OrchestrationService::run_cancel`:
        // the adapter's own exit event now terminalizes runs
        // (`adapter/run_lifecycle.rs`), so killing first would race a
        // `failed` edge against this `cancelled` one. The kill is still
        // attempted on every path out of the transition -- a failed
        // transition must never leave a live vendor process behind.
        let project_id = self.project_id;
        let to = RunState::try_from("cancelled").expect("cancelled is valid");
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.transition_run(run_id, &to, None)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ViolationError::Domain);
        if let Ok(committed) = result.as_mut() {
            self.broadcast(committed);
        }
        // Acknowledge the audited intent with its real outcome. A failed
        // acknowledgement is logged, never allowed to mask the cancel's
        // own result.
        let (outcome_result, ack) = match result {
            Ok(_) => (Ok(()), Some(json!({ "outcome": "cancelled" }))),
            // A sibling observation already terminalized the run: the
            // idempotent success path (R79). Only intent-backed callers
            // get this classification; the operator path stays strict.
            Err(ViolationError::Domain(crate::domain::DomainError::Transition(_)))
                if operation_id.is_some() =>
            {
                (Ok(()), Some(json!({ "outcome": "superseded" })))
            }
            Err(err) => (Err(err), None),
        };
        if let (Some(operation_id), Some(ack)) = (operation_id, ack) {
            let sanitized = Redactor::new().sanitize_json(&ack);
            if let Err(ack_err) = self.db.acknowledge_operation(operation_id, sanitized).await {
                tracing::warn!(
                    error = %ack_err,
                    run_id = %run_id,
                    operation_id = %operation_id,
                    "failed to acknowledge a policy cancellation intent"
                );
            }
        }
        if let Some(driver) = &self.run_driver
            && let Err(err) = driver
                .cancel_run(run_id, crate::adapter::CancelScope::Worker)
                .await
        {
            // A real kill failure (an absent adapter is the clean
            // `CancelOutcome::NoRunningAdapter`, not an `Err`): the run is
            // journaled `cancelled` but a vendor process may still be
            // live. Make that visible to `run/get` and the monitor via
            // `degradedControl` instead of only the log (R13). The flag
            // write uses the same guarded single-call `set_run_flag`
            // `quarantine` uses, so it journals and broadcasts.
            tracing::warn!(
                error = %err,
                run_id = %run_id,
                "failed to cancel adapter subprocess for policy violation"
            );
            let project_id = self.project_id;
            match self
                .db
                .run_domain_op(Box::new(move |conn| {
                    let mut repo = DomainRepository::new(conn, project_id);
                    repo.set_run_flag(run_id, RunFlag::DegradedControl, true)
                        .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
                }))
                .await
            {
                Ok(mut committed) => self.broadcast(&mut committed),
                Err(flag_err) => tracing::warn!(
                    error = %flag_err,
                    run_id = %run_id,
                    "failed to record degradedControl after a kill failure"
                ),
            }
        }
        outcome_result
    }

    /// `policy/violation/decide`: resolves `violation_id` as `resolution`
    /// (`"release"` or `"cancel"`), discarding whether a winning
    /// `"release"` actually cleared `flags.policyQuarantined` -- see
    /// [`Self::decide_and_release_status`] for a version that reports it.
    /// This wrapper exists only because `DecideOutcome` and this return
    /// type predate that reporting need and every other caller (including
    /// this crate's own concurrency-race test suites) already matches on
    /// a bare [`DecideOutcome`].
    ///
    /// # Errors
    /// See [`Self::decide_and_release_status`].
    pub async fn decide(
        &self,
        violation_id: PolicyViolationId,
        principal_instance_id: &str,
        resolution: &str,
    ) -> Result<DecideOutcome, ViolationError> {
        self.decide_and_release_status(violation_id, principal_instance_id, resolution)
            .await
            .map(|(outcome, _)| outcome)
    }

    /// Same arbitration as [`Self::decide`], but for a winning `"release"`
    /// also reports whether it actually cleared `flags.policyQuarantined`
    /// -- `None` for a `"cancel"` resolution or an idempotent
    /// [`DecideOutcome::AlreadyDecided`] replay, since neither computes a
    /// clearing decision. Used by `policy/violation/decide`'s RPC handler
    /// (`crate::service::orchestration::OrchestrationService::policy_violation_decide`)
    /// so an operator who just released a violation can tell, in the same
    /// response, whether the run actually left quarantine or a different,
    /// still-open violation kept it held (R75 follow-up, `agent://ReviewR75`
    /// W4) -- without a second `run/get` and without the response's
    /// `"decided"` outcome silently meaning either.
    ///
    /// Ownership is not pre-checked here: `reconcile/omp` can rebind a
    /// task's `owner_client_instance_id` at any time via
    /// [`DomainRepository::reconcile_ownership`], including in the window
    /// between this call and the guarded write, so it is arbitrated
    /// exclusively inside [`DomainRepository::resolve_policy_violation`]'s
    /// guarded transaction (R72, mirroring
    /// [`crate::approval::ApprovalService::decide`]'s R71 fix). The rest is
    /// decided by that same transaction: whether a different resolution is
    /// already on record (a losing call is refused with
    /// [`ViolationError::Conflict`]), whether this is an idempotent replay
    /// ([`DecideOutcome::AlreadyDecided`], which re-applies nothing), and
    /// -- for `"release"` -- whether the run has already settled
    /// ([`ViolationError::RunSettled`]). The database actor interleaves
    /// whole `run_domain_op` round trips, so none of these can be
    /// caller-side pre-checks (R54, R72): the guarded write is the sole
    /// arbiter, exactly one `PolicyViolationDecided` event is journaled per
    /// violation, and only the deciding call fires side effects.
    ///
    /// A `"release"` that wins this arbitration still may not clear
    /// `flags.policyQuarantined`: [`ViolationService::release_quarantine`]
    /// refuses to if a *different* policy violation on the run is still
    /// unresolved (R75), so `DecideOutcome::Decided` here means the
    /// resolution was recorded, not that the run necessarily left
    /// quarantine -- that is exactly what the returned `Option<bool>`
    /// disambiguates.
    ///
    /// # Errors
    /// Returns [`ViolationError::Forbidden`] if `principal_instance_id`
    /// does not own the task, [`ViolationError::Conflict`] if a different
    /// resolution is already on record, [`ViolationError::RunSettled`] if
    /// `resolution` is `"release"` and the run has already reached a
    /// terminal state, and [`ViolationError::InvalidResolution`] if
    /// `resolution` is neither `"release"` nor `"cancel"`.
    pub async fn decide_and_release_status(
        &self,
        violation_id: PolicyViolationId,
        principal_instance_id: &str,
        resolution: &str,
    ) -> Result<(DecideOutcome, Option<bool>), ViolationError> {
        if resolution != "release" && resolution != "cancel" {
            return Err(ViolationError::InvalidResolution {
                resolution: resolution.to_string(),
            });
        }

        let project_id = self.project_id;
        let snapshot = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                let s = repo.policy_violation_snapshot(violation_id)?;
                Ok(json!({
                    "runId": s.run_id,
                    "taskId": s.task_id,
                    "workerId": s.worker_id,
                }))
            }))
            .await
            .map_err(|_| ViolationError::NotFound { violation_id })?;
        let run_id = RunId::parse(snapshot["runId"].as_str().unwrap_or_default())
            .map_err(|_| ViolationError::NotFound { violation_id })?;
        let task_id = TaskId::parse(snapshot["taskId"].as_str().unwrap_or_default())
            .map_err(|_| ViolationError::NotFound { violation_id })?;
        let worker_id = WorkerId::parse(snapshot["workerId"].as_str().unwrap_or_default())
            .map_err(|_| ViolationError::NotFound { violation_id })?;

        let principal_instance_id_owned = principal_instance_id.to_string();
        let resolution_owned = resolution.to_string();
        let mut result = match self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.resolve_policy_violation(
                    violation_id,
                    run_id,
                    task_id,
                    worker_id,
                    &principal_instance_id_owned,
                    &resolution_owned,
                )
                .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
        {
            Ok(value) => value,
            // The guarded write is the arbiter: a losing racer never journals
            // an event and never reaches the side effects below.
            Err(DomainError::NotOwner { .. }) => {
                return Err(ViolationError::Forbidden {
                    instance_id: principal_instance_id.to_string(),
                    violation_id,
                });
            }
            Err(DomainError::AlreadyResolved { existing, .. }) => {
                return if existing == resolution {
                    Ok((DecideOutcome::AlreadyDecided, None))
                } else {
                    Err(ViolationError::Conflict { violation_id })
                };
            }
            Err(DomainError::RunSettled { .. }) => {
                return Err(ViolationError::RunSettled {
                    violation_id,
                    run_id,
                });
            }
            Err(err) => return Err(ViolationError::Domain(err)),
        };
        self.broadcast(&mut result);

        let quarantine_cleared = if resolution == "release" {
            Some(self.release_quarantine(run_id).await?)
        } else {
            self.cancel_and_transition(run_id, None).await?;
            None
        };

        Ok((DecideOutcome::Decided, quarantine_cleared))
    }

    /// Clears `flags.policy_quarantined` after a `"release"` decision, via
    /// [`DomainRepository::release_quarantine`] -- which refuses to clear
    /// the flag if a *different* policy violation on this run is still
    /// unresolved, so a release targeting one violation can never silently
    /// un-quarantine a run for another, still-open one (R75). Called only
    /// after [`Self::decide_and_release_status`]'s
    /// [`DomainRepository::resolve_policy_violation`] commit has already
    /// resolved the violation being released, so that row is never the
    /// one this method's own unresolved-count sees.
    ///
    /// Returns whether the flag was actually cleared. No write and no
    /// broadcast if it was already clear or another violation remains
    /// open -- unlike the pre-R75 `set_quarantined(run_id, false)` this
    /// replaced, a release is no longer guaranteed to change the flag it
    /// targets, and [`Self::decide_and_release_status`] reports that back
    /// to its caller instead of discarding it (R75 follow-up).
    async fn release_quarantine(&self, run_id: RunId) -> Result<bool, ViolationError> {
        let project_id = self.project_id;
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.release_quarantine(run_id)
                    .map(|maybe_committed| match maybe_committed {
                        Some(c) => embed_envelope(json!({ "cleared": true }), &c.envelope),
                        None => json!({ "cleared": false }),
                    })
            }))
            .await
            .map_err(ViolationError::Domain)?;
        let cleared = result["cleared"].as_bool().unwrap_or(false);
        self.broadcast(&mut result);
        Ok(cleared)
    }
}
