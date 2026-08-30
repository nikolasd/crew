//! The orchestration domain repository.
//!
//! Every mutating command runs one SQLite transaction that appends a durable
//! event to the `events` journal and updates the relevant projection row(s),
//! then commits. If the projection update fails, the transaction rolls back
//! and no event is retained. The append-only journal remains authoritative;
//! projection tables are rebuildable from it.
//!
//! The runtime is the sole authority for run-state transitions: every
//! transition is validated through [`super::transitions::check_transition`]
//! before its event is appended.

use crew_protocol::{
    ApprovalRequest, DeliveryState, EventEnvelope, EventSource, PlanGetResult, PlanSpec,
    PolicyViolationId, ProjectId, Run, RunFlags, RunId, RunMessage, RunState, RuntimeEvent,
    RuntimeEventKind, SubtaskSpec, TaskId, TaskRef, Timestamp, Worker, WorkerId,
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use tokio::sync::broadcast;

use super::transitions::{TransitionError, check_transition};

/// Errors returned by [`DomainRepository`] commands.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// A database operation failed.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// A requested run-state transition was illegal.
    #[error(transparent)]
    Transition(#[from] TransitionError),
    /// A referenced record was not found.
    #[error("{kind} {id} not found")]
    NotFound { kind: &'static str, id: String },
    /// A guarded mutation refused to write because `instance_id` does not
    /// own the task the mutation targets. Checked inside the same guarded
    /// transaction as the write it protects -- not as a caller-side
    /// pre-check -- so a `reconcile/omp` ownership rebind landing between a
    /// caller's snapshot read and the write cannot let a stale owner's
    /// decision through (R71).
    #[error("task {task_id} is not owned by {instance_id}")]
    NotOwner {
        task_id: String,
        instance_id: String,
    },
    /// A guarded `task/upsert` write refused because the presented
    /// revision is lower than the revision already stored -- OMP re-sent a
    /// stale intent. Checked inside [`DomainRepository::upsert_task`]'s own
    /// guarded write, not a caller-side pre-check read from a separate
    /// `run_domain_op` round trip the database actor could interleave with
    /// another write to the same task (R74, applying R70-R72's doctrine to
    /// task writes).
    #[error("task {task_id} revision {presented} is lower than stored revision {stored}")]
    RevisionTooLow {
        task_id: String,
        presented: u64,
        stored: u64,
    },
    /// A guarded `reconcile/omp` write refused because the presented
    /// revision does not match the revision currently stored -- the
    /// caller's snapshot of the task is stale. Checked inside
    /// [`DomainRepository::reconcile_ownership`]'s own guarded write, for
    /// the same reason as [`Self::RevisionTooLow`] (R74).
    #[error("task {task_id} revision {presented} does not match stored revision {stored}")]
    RevisionMismatch {
        task_id: String,
        presented: u64,
        stored: u64,
    },
    /// A guarded mutation refused to write because the row already carries a
    /// resolution (or, for an approval, a decision) committed by an earlier
    /// decision. `existing` is the resolution on record, so a service layer
    /// can distinguish an idempotent replay from a contradictory second
    /// decision.
    #[error("{kind} {id} was already resolved as {existing}")]
    AlreadyResolved {
        kind: &'static str,
        id: String,
        existing: String,
    },
    /// A guarded mutation refused to write because the run it belongs to has
    /// already reached a terminal state.
    #[error("run {run_id} has already settled")]
    RunSettled { run_id: String },
    /// A guarded mutation refused to write because the run is quarantined
    /// by an undecided policy violation. Checked inside the same guarded
    /// transaction as the write it protects -- not as a caller-side
    /// pre-check round trip the database actor could interleave with a
    /// quarantine landing on the same run (R78, applying R70-R81's
    /// doctrine to the quarantine gates).
    #[error("run {run_id} is quarantined by an undecided policy violation")]
    PolicyQuarantined { run_id: String },
    /// A leader-originated steering message was refused because the run's
    /// turn budget is exhausted (`turns_used >= turn_limit`). Checked
    /// inside [`DomainRepository::record_message`]'s own guarded
    /// transaction (WP19) -- never a caller-side pre-check. The typed
    /// refusal travels to the RPC caller as `BUDGET_EXCEEDED`; the
    /// durable `BudgetExceeded` fact is journaled (and broadcast) by a
    /// follow-up commit, not inside this rolled-back one.
    #[error("run {run_id} exceeded its turn budget: {turns_used}/{turn_limit} turns used")]
    BudgetExceeded {
        run_id: String,
        turns_used: u32,
        turn_limit: u32,
    },
    /// A serialization step failed.
    #[error("failed to serialize event: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The database actor thread is no longer running.
    #[error("database actor is not running")]
    ActorUnavailable,
}

/// The committed result of a mutation: the durable event sequence number the
/// mutation produced, and the exact envelope callers should broadcast to
/// live subscribers.
#[derive(Debug, Clone)]
pub struct Committed {
    pub sequence: u64,
    pub envelope: EventEnvelope,
}

/// The result of [`DomainRepository::record_policy_violation`]: the
/// journal commit, plus whether the run was already actioned --
/// quarantined or already terminal -- immediately *before* this
/// violation's commit. That flag is read inside the same atomic
/// `run_domain_op` closure as the journal write itself, not a separate,
/// earlier round trip (R75): [`crate::policy::ViolationService::apply_action`]
/// uses it in place of a caller-held snapshot to decide whether to
/// (re)apply the configured [`crate::config::NestedViolationAction`], so a
/// concurrent `decide("release")` committing between an earlier snapshot
/// read and this journal write can no longer leave that decision stale.
#[derive(Debug, Clone)]
pub struct PolicyViolationRecordOutcome {
    pub committed: Committed,
    pub already_actioned: bool,
}

/// A policy violation's correlating ids -- `run_id`, `task_id`, and
/// `worker_id` -- for [`crate::policy::ViolationService`] to thread
/// through to [`DomainRepository::resolve_policy_violation`] and its
/// follow-up commits. It carries no ownership or resolution state:
/// whether a decision may commit at all -- including ownership (R72) --
/// is decided inside [`DomainRepository::resolve_policy_violation`], not
/// by any field read here.
#[derive(Debug, Clone)]
pub struct PolicyViolationSnapshot {
    pub run_id: String,
    pub task_id: String,
    pub worker_id: String,
}

/// Names one boolean field on [`RunFlags`], so
/// [`DomainRepository::set_run_flag`] can arbitrate a single flag change
/// inside its own guarded write instead of taking a whole
/// caller-computed [`RunFlags`] struct on trust (R73). It is `pub`,
/// re-exported from `domain` (a `pub mod` of this crate), and integration
/// tests construct it directly as `crew_runtime::domain::RunFlag` -- it
/// is not internal to this crate. What is true is narrower: it is not a
/// protocol type, so the wire shape of `RunFlagsChanged` is unaffected by
/// it and still carries the full [`RunFlags`] struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFlag {
    DegradedControl,
    NeedsReconciliation,
    ProtocolUnhealthy,
    PolicyQuarantined,
    WorkspaceDirty,
    ChildrenActive,
    TurnSettled,
}

impl RunFlag {
    fn apply(self, flags: &mut RunFlags, value: bool) {
        match self {
            RunFlag::DegradedControl => flags.degraded_control = value,
            RunFlag::NeedsReconciliation => flags.needs_reconciliation = value,
            RunFlag::ProtocolUnhealthy => flags.protocol_unhealthy = value,
            RunFlag::PolicyQuarantined => flags.policy_quarantined = value,
            RunFlag::WorkspaceDirty => flags.workspace_dirty = value,
            RunFlag::ChildrenActive => flags.children_active = value,
            RunFlag::TurnSettled => flags.turn_settled = value,
        }
    }
}

/// OMP's answer to a pending child-worker request
/// ([`DomainRepository::decide_child`]): acceptance binds the OMP-created
/// child identifiers; denial carries the operator-facing reason. Making
/// the two arms one type keeps their field requirements unrepresentable
/// to mix up (an acceptance without child ids, a denial with them). Not a
/// protocol type -- the journaled `ChildEvent`'s wire shape is unaffected.
#[derive(Debug, Clone)]
pub enum ChildDecision {
    Accept {
        child_task_id: crew_protocol::TaskId,
        child_worker_id: crew_protocol::WorkerId,
        child_run_id: RunId,
    },
    Deny {
        reason: String,
    },
}

/// Embeds `envelope` into `value` under a reserved key so it survives the
/// `run_domain_op` boundary -- whose closures are constrained to return a
/// plain [`Value`] -- back out to the async service layer, which broadcasts
/// it to live subscribers via [`take_envelope`] before the key is stripped.
#[must_use]
pub fn embed_envelope(mut value: Value, envelope: &EventEnvelope) -> Value {
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "__envelope".to_string(),
            serde_json::to_value(envelope)
                .expect("EventEnvelope is a plain, serializable wire type"),
        );
    }
    value
}

/// Removes and deserializes the envelope embedded by [`embed_envelope`], if
/// present. A read-only lookup that never embedded one returns `None`.
#[must_use]
pub fn take_envelope(value: &mut Value) -> Option<EventEnvelope> {
    let raw = value.as_object_mut()?.remove("__envelope")?;
    serde_json::from_value(raw).ok()
}

/// Takes the envelope [`embed_envelope`] embedded in `value` (if present),
/// sends it to every live `events/subscribe` listener on `events_tx`, and
/// returns its committed sequence number. Every call site that commits a
/// [`DomainRepository`] mutation across a `run_domain_op` boundary --
/// `OrchestrationService`, `ApprovalService`, `CoordinationBroker`, and
/// `crate::adapter::event_sink::DomainAdapterEventSink` alike -- should
/// route through this one function rather than reimplementing the
/// take-then-send pair inline, so there is exactly one place this
/// take-before-strip-then-broadcast behavior can regress (see
/// `docs/architecture.md` §18 item 3).
pub fn broadcast_committed(
    events_tx: &broadcast::Sender<EventEnvelope>,
    value: &mut Value,
) -> Option<u64> {
    let envelope = take_envelope(value)?;
    let sequence = envelope.sequence;
    let _ = events_tx.send(envelope);
    Some(sequence)
}

/// A repository over the orchestration projection tables and the durable
/// event journal. Holds no state of its own; every command borrows a
/// connection and commits before returning.
pub struct DomainRepository<'c> {
    conn: &'c mut Connection,
    project_id: ProjectId,
}

impl<'c> DomainRepository<'c> {
    /// Creates a repository bound to `conn` for `project_id`.
    #[must_use]
    pub fn new(conn: &'c mut Connection, project_id: ProjectId) -> Self {
        Self { conn, project_id }
    }

    /// Journals a [`crew_protocol::WorkspaceEvent`] durably. Workspace
    /// and artifact state lives in the lease database and the artifact
    /// store, not in a projection table, so this appends the event only --
    /// but it appends it through the same transaction and sequence
    /// allocator as every other mutation, so a monitor replaying the
    /// journal sees workspace activity interleaved with run activity in
    /// real commit order.
    ///
    /// # Errors
    /// Returns [`DomainError`] if the append fails.
    pub fn record_workspace_event(
        &mut self,
        kind: crew_protocol::WorkspaceEvent,
        run_id: crew_protocol::RunId,
        lease_id: String,
    ) -> Result<Committed, DomainError> {
        self.append_and_apply(
            &RuntimeEvent::WorkspaceEvent {
                kind,
                run_id,
                lease_id,
            },
            None,
            None,
            Some(run_id),
            |_| Ok(()),
        )
    }

    /// Like [`Self::record_workspace_event`], but refuses inside the same
    /// append transaction if the run is policy-quarantined (R78). Used for
    /// `workspace/apply`'s `ApplyStarted` append, so the journal can never
    /// record an apply start for a run quarantined at that instant -- the
    /// residue is exactly the append-to-working-tree-mutation gap, which
    /// no cross-database transaction can close.
    pub fn record_workspace_event_unless_quarantined(
        &mut self,
        kind: crew_protocol::WorkspaceEvent,
        run_id: crew_protocol::RunId,
        lease_id: String,
    ) -> Result<Committed, DomainError> {
        self.append_and_apply(
            &RuntimeEvent::WorkspaceEvent {
                kind,
                run_id,
                lease_id,
            },
            None,
            None,
            Some(run_id),
            move |tx| {
                let quarantined: i64 = tx.query_row(
                    "SELECT flags_policy_quarantined FROM runs WHERE run_id = ?1",
                    [run_id.to_string()],
                    |row| row.get(0),
                )?;
                if quarantined != 0 {
                    return Err(DomainError::PolicyQuarantined {
                        run_id: run_id.to_string(),
                    });
                }
                Ok(())
            },
        )
    }

    /// Journals a display pane attaching to or detaching from a run.
    ///
    /// Like workspace events, a pane has no projection table: the durable
    /// record is the journal entry, so a monitor replaying the journal
    /// sees pane activity in real commit order against the run it belongs
    /// to.
    ///
    /// # Errors
    /// Returns [`DomainError`] if the append fails.
    pub fn record_display_event(
        &mut self,
        kind: crew_protocol::RuntimeEventKind,
        run_id: crew_protocol::RunId,
        backend: crew_protocol::DisplayBackend,
        placement: crew_protocol::DisplayPlacement,
        pane_ref: String,
    ) -> Result<Committed, DomainError> {
        self.append_and_apply(
            &RuntimeEvent::DisplayEvent {
                kind,
                run_id,
                backend,
                placement,
                pane_ref,
            },
            None,
            None,
            Some(run_id),
            |_| Ok(()),
        )
    }

    /// Appends an event and runs `apply` against the same transaction,
    /// committing both atomically. Returns the assigned sequence number.
    fn append_and_apply<F>(
        &mut self,
        event: &RuntimeEvent,
        task_id: Option<crew_protocol::TaskId>,
        worker_id: Option<crew_protocol::WorkerId>,
        run_id: Option<crew_protocol::RunId>,
        apply: F,
    ) -> Result<Committed, DomainError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<(), DomainError>,
    {
        let project_id = self.project_id;
        let tx = self.conn.transaction()?;
        let timestamp = Timestamp::now();

        // Build the envelope with a provisional sequence of 0; the real
        // sequence is the rowid assigned on insert. The bare `RuntimeEvent`
        // is persisted in `event_json`; `sequence`, `timestamp`,
        // `project_id`, `run_id`, `task_id`, and `worker_id` are also
        // durable in their own columns, so `replay()`
        // (`ipc/connection.rs`) can reconstruct the full envelope from
        // those columns plus the bare event. `parent_worker_id` and
        // `vendor_event_ref` are not parameters here and remain NULL on
        // disk; the full envelope built below (with those two fields set
        // from context) is still returned so callers can broadcast it to
        // live subscribers.
        let envelope = {
            // Insert with a placeholder, then rewrite with the real sequence.
            tx.execute(
                "INSERT INTO events (timestamp, project_id, run_id, event_json, task_id, worker_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    timestamp.as_str(),
                    project_id.to_string(),
                    run_id.map(|r| r.to_string()),
                    "{}",
                    task_id.map(|id| id.to_string()),
                    worker_id.map(|id| id.to_string()),
                ],
            )?;
            let sequence = tx.last_insert_rowid() as u64;
            let event_json = serde_json::to_string(event)?;
            tx.execute(
                "UPDATE events SET event_json = ?1 WHERE sequence = ?2",
                rusqlite::params![event_json, sequence],
            )?;
            EventEnvelope {
                sequence,
                timestamp: timestamp.clone(),
                project_id,
                task_id,
                worker_id,
                run_id,
                parent_worker_id: None,
                source: EventSource::Runtime,
                event: event.clone(),
                vendor_event_ref: None,
            }
        };
        let sequence = envelope.sequence;

        apply(&tx)?;
        tx.commit()?;
        Ok(Committed { sequence, envelope })
    }

    /// Upserts an OMP-owned task. Idempotent for an identical revision.
    /// Both guards live inside the write itself: the `ON CONFLICT` arm
    /// applies only when the presented revision is not lower than the
    /// stored one (R74) AND the presented owner matches the task's
    /// current owner -- an existing task may only be re-upserted by its
    /// current owner; transferring ownership goes through
    /// `reconcile/omp`, never through `task/upsert` (R76), so a second
    /// OMP-extension client cannot seize a task it never reconciled by
    /// presenting the stored revision with its own instance id. A refused
    /// write is classified inside the same transaction -- nothing else
    /// can have changed the row since the `ON CONFLICT` arm declined: a
    /// stored revision higher than the one presented is
    /// [`DomainError::RevisionTooLow`]; otherwise (the revision would have
    /// been accepted) the presented owner does not match the stored one,
    /// [`DomainError::NotOwner`]. A caller-side pre-check read in a
    /// separate `run_domain_op` round trip could be interleaved with
    /// another write to the same task, so both checks must be
    /// re-evaluated from inside this transaction, not from a snapshot
    /// taken before it opened. Creating a task (no existing row) binds
    /// ownership to the presented id unconditionally -- there is no prior
    /// owner to protect. Emits a `TaskCreated`/`TaskUpdated` event.
    pub fn upsert_task(
        &mut self,
        task_id: crew_protocol::TaskId,
        task_ref: &TaskRef,
    ) -> Result<Committed, DomainError> {
        // Read for the event kind only. This runs before `append_and_apply`
        // opens its transaction because the event must be fully built first;
        // it is safe because the database actor executes whole
        // `run_domain_op` closures serially, so nothing can create or delete
        // the row between this read and the guarded write below.
        let existed: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM tasks WHERE task_id = ?1",
                [task_id.to_string()],
                |_| Ok(true),
            )
            .unwrap_or(false);

        let kind = if existed {
            RuntimeEventKind::TaskUpdated
        } else {
            RuntimeEventKind::TaskCreated
        };
        let event = RuntimeEvent::TaskEvent {
            kind,
            task_id,
            owner_client_instance_id: task_ref.owner_client_instance_id.clone(),
            revision: task_ref.revision,
        };
        let owner = task_ref.owner_client_instance_id.clone();
        let revision = task_ref.revision;
        let project = self.project_id;
        self.append_and_apply(&event, Some(task_id), None, None, move |tx| {
            let now = Timestamp::now();
            let affected = tx.execute(
                "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(task_id) DO UPDATE SET
                   owner_client_instance_id = excluded.owner_client_instance_id,
                   revision = excluded.revision,
                   updated_at = excluded.updated_at
                 WHERE excluded.revision >= tasks.revision
                   AND excluded.owner_client_instance_id = tasks.owner_client_instance_id",
                rusqlite::params![
                    task_id.to_string(),
                    project.to_string(),
                    owner,
                    revision,
                    now.as_str(),
                ],
            )?;
            if affected == 0 {
                // The conflict arm declined: classify inside the same
                // transaction, since nothing else can have changed the row
                // since. A stored revision higher than the one presented
                // wins regardless of ownership (an owner is entitled to
                // know its own upsert is stale); otherwise the presented
                // owner does not match the current one.
                let (stored_revision, stored_owner): (u64, String) = tx.query_row(
                    "SELECT revision, owner_client_instance_id FROM tasks WHERE task_id = ?1",
                    [task_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if revision < stored_revision {
                    return Err(DomainError::RevisionTooLow {
                        task_id: task_id.to_string(),
                        presented: revision,
                        stored: stored_revision,
                    });
                }
                debug_assert_ne!(
                    owner, stored_owner,
                    "the ON CONFLICT arm only declines a non-lower revision when the owner predicate failed"
                );
                return Err(DomainError::NotOwner {
                    task_id: task_id.to_string(),
                    instance_id: owner,
                });
            }
            Ok(())
        })
    }

    /// Creates a worker, persisting its immutable profile reference. Emits
    /// a `WorkerCreated` event. Fails if the worker id already exists.
    pub fn create_worker(&mut self, worker: &Worker) -> Result<Committed, DomainError> {
        self.create_worker_with_snapshot(worker, None)
    }

    /// Like [`Self::create_worker`], but also stores the full resolved
    /// [`crate::adapter::WorkerProfile`] snapshot (serialized JSON,
    /// including `startupOptions`/`environmentAllowlist`/`source` --
    /// everything `WorkerProfileRef`'s five frozen fields cannot carry)
    /// alongside the worker row, when `worker/create` resolved a
    /// `profileId`. Copied in at creation time and never re-read from the
    /// profile store afterward, so the worker's own row is immutable
    /// regardless of what later happens to the source profile: this is
    /// what makes "changing the source profile after worker creation
    /// never mutates the stored snapshot" true even if a profile store
    /// implementation someday allows updates.
    pub fn create_worker_with_snapshot(
        &mut self,
        worker: &Worker,
        resolved_profile_json: Option<String>,
    ) -> Result<Committed, DomainError> {
        let event = RuntimeEvent::WorkerEvent {
            kind: RuntimeEventKind::WorkerCreated,
            worker_id: worker.worker_id,
            profile_id: worker.profile_ref.id.to_string(),
        };
        let worker = worker.clone();
        let project = self.project_id;
        self.append_and_apply(&event, None, Some(worker.worker_id), None, move |tx| {
            let profile = &worker.profile_ref;
            tx.execute(
                "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO NOTHING",
                rusqlite::params![
                    profile.id.to_string(),
                    profile.fingerprint,
                    profile.adapter,
                    profile.model,
                    serde_json::to_string(&profile.permission_envelope)?,
                ],
            )?;
            tx.execute(
                "INSERT INTO workers (worker_id, project_id, profile_id, parent_worker_id, created_at, resolved_profile_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    worker.worker_id.to_string(),
                    project.to_string(),
                    profile.id.to_string(),
                    worker.parent_worker_id.map(|w| w.to_string()),
                    worker.created_at.as_str(),
                    resolved_profile_json,
                ],
            )?;
            Ok(())
        })
    }

    /// Reads back the full resolved [`crate::adapter::WorkerProfile`]
    /// snapshot json stored by [`Self::create_worker_with_snapshot`], or
    /// `None` for a worker created without a `profileId` (e.g. `adapter:
    /// "fake"`/`"ompNative"`). Runtime-internal only: never exposed over
    /// `worker/get`'s wire response, which stays exactly
    /// `WorkerProfileRef`'s five frozen fields. The (later) adapter
    /// registry reads this to reconstruct the exact validated launch
    /// profile for a run's worker.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if `worker_id` does not exist.
    pub fn resolved_profile_snapshot(
        &self,
        worker_id: WorkerId,
    ) -> Result<Option<String>, DomainError> {
        self.conn
            .query_row(
                "SELECT resolved_profile_json FROM workers WHERE worker_id = ?1",
                [worker_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => DomainError::NotFound {
                    kind: "worker",
                    id: worker_id.to_string(),
                },
                other => DomainError::Sqlite(other),
            })
    }

    /// Submits a run in `queued` state. Requires the task and worker to
    /// exist (enforced by foreign keys). Emits a `RunQueued` event.
    ///
    /// `policy_fingerprint` is the SHA-256 of the
    /// merged [`crate::config::RuntimePolicy`] this run was authorized
    /// under, so a later violation or audit can be resolved against a
    /// specific policy rather than against whatever is merged today.
    /// `None` for callers with no merged config (tests, embeddings), which
    /// leaves the column NULL rather than fabricating a fingerprint.
    ///
    /// `principal_instance_id` is the connected `ompExtension` instance to
    /// arbitrate ownership against, re-read from `tasks` inside this same
    /// guarded write (R77) -- not from a caller-side snapshot a
    /// `reconcile/omp` rebind could invalidate between read and write.
    /// `None` for callers with no external principal to check: every
    /// internal, adapter-, and recovery-driven submission (retries staged
    /// by the runtime itself, crash recovery, test seeding) is already
    /// trusted and has no connected caller to arbitrate against.
    ///
    /// # Errors
    /// Returns [`DomainError::NotOwner`] if `principal_instance_id` is
    /// `Some` and does not own `run.task_id`, or [`DomainError::NotFound`]
    /// if that task does not exist.
    pub fn submit_run(
        &mut self,
        run: &Run,
        policy_fingerprint: Option<&str>,
        principal_instance_id: Option<&str>,
    ) -> Result<Committed, DomainError> {
        let event = RuntimeEvent::RunEvent {
            kind: RuntimeEventKind::RunQueued,
            run_id: run.run_id,
            task_id: run.task_id,
            worker_id: run.worker_id,
            state: run.state.to_string(),
        };
        let run = run.clone();
        let policy_fingerprint = policy_fingerprint.map(str::to_string);
        let principal_instance_id = principal_instance_id.map(str::to_string);
        self.append_and_apply(
            &event,
            Some(run.task_id),
            Some(run.worker_id),
            Some(run.run_id),
            move |tx| {
                let now = Timestamp::now();
                // Ownership is arbitrated here, inside the guarded write,
                // not by a caller-side snapshot (R70-R77 doctrine): the
                // database actor interleaves whole `run_domain_op`
                // closures, so only a re-read from inside this same
                // transaction can observe a `reconcile/omp` rebind that
                // commits between a caller's snapshot read and this write.
                if let Some(principal_instance_id) = principal_instance_id {
                    let owner: Option<String> = tx
                        .query_row(
                            "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                            [run.task_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match owner {
                        Some(owner) if owner == principal_instance_id => {}
                        Some(_) => {
                            return Err(DomainError::NotOwner {
                                task_id: run.task_id.to_string(),
                                instance_id: principal_instance_id,
                            });
                        }
                        None => {
                            return Err(DomainError::NotFound {
                                kind: "task",
                                id: run.task_id.to_string(),
                            });
                        }
                    }
                }
                tx.execute(
                    "INSERT INTO runs (run_id, task_id, worker_id, state,
                       flags_degraded_control, flags_needs_reconciliation, flags_protocol_unhealthy,
                       flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
                       flags_turn_settled,
                       vendor_session_id, created_at, started_at, completed_at, policy_fingerprint)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                    rusqlite::params![
                        run.run_id.to_string(),
                        run.task_id.to_string(),
                        run.worker_id.to_string(),
                        run.state.to_string(),
                        run.flags.degraded_control as i64,
                        run.flags.needs_reconciliation as i64,
                        run.flags.protocol_unhealthy as i64,
                        run.flags.policy_quarantined as i64,
                        run.flags.workspace_dirty as i64,
                        run.flags.children_active as i64,
                        run.flags.turn_settled as i64,
                        run.vendor_session_id,
                        now.as_str(),
                        run.started_at.as_ref().map(|t| t.as_str().to_string()),
                        run.completed_at.as_ref().map(|t| t.as_str().to_string()),
                        policy_fingerprint,
                    ],
                )?;
                Ok(())
            },
        )
    }

    /// Transitions a run to a new state, authorizing the caller before
    /// validating the edge. Emits the matching `Run*` event and updates
    /// the projection. A refusal on either ground appends nothing.
    ///
    /// Authorization precedes validity: a non-owning caller sees
    /// [`DomainError::NotOwner`], never `ILLEGAL_TRANSITION`, regardless
    /// of whether the edge it asked for would otherwise be legal (R77).
    /// This is the opposite precedence from [`Self::upsert_task`]'s
    /// revision-before-ownership check (R76): there, the only way to
    /// present a stale revision is to already be the task's actual
    /// owner racing itself, so disclosing staleness first tells a
    /// legitimate caller something it is entitled to know. Here the
    /// caller is, by construction, not yet known to have any standing
    /// over the run at all, so it must clear ownership before this
    /// method will say anything about the run's current state --
    /// matching [`Self::submit_run`] and [`Self::record_message`], which
    /// have no pre-write validity check for ownership to outrank.
    ///
    /// `principal_instance_id` is checked twice for this precedence:
    /// once as a plain `self.conn` read, immediately before
    /// [`check_transition`], so a non-owner's illegal-edge attempt still
    /// classifies as `NotOwner`; and again -- the authoritative,
    /// race-safe check -- re-read from `tasks` inside this guarded
    /// write, immediately before the mutating `UPDATE` (R77). Only the
    /// second read runs inside the `run_domain_op` closure the database
    /// actor executes as one indivisible unit, so only it can observe a
    /// concurrent `reconcile/omp` rebind that commits between the first
    /// read and this write; a race between the two reads can only ever
    /// make the first one agree or disagree with the second, never let
    /// a mutation through without the second read's independent
    /// approval. `None` for every internal caller -- adapter-driven
    /// lifecycle transitions, approval/violation resolution returning a
    /// run to `working`, crash recovery, and test seeding all transition
    /// a run on the runtime's own authority, with no connected caller to
    /// arbitrate against. Only `run/cancel`'s handler, the one
    /// client-facing caller, passes `Some`.
    ///
    /// # Errors
    /// Returns [`DomainError::NotOwner`] if `principal_instance_id` is
    /// `Some` and does not own the run's task, or [`DomainError::NotFound`]
    /// if that task does not exist, in addition to the transition errors
    /// documented above.
    pub fn transition_run(
        &mut self,
        run_id: crew_protocol::RunId,
        to: &RunState,
        principal_instance_id: Option<&str>,
    ) -> Result<Committed, DomainError> {
        let (from_str, task_id_str, worker_id_str): (String, String, String) = self
            .conn
            .query_row(
                "SELECT state, task_id, worker_id FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|_| DomainError::NotFound {
                kind: "run",
                id: run_id.to_string(),
            })?;

        let task_id =
            crew_protocol::TaskId::parse(&task_id_str).map_err(|_| DomainError::NotFound {
                kind: "task",
                id: task_id_str.clone(),
            })?;

        // Authorization precedes validity (R77) -- see the doc comment
        // above. This is a plain snapshot read, not inside the
        // closure-granular boundary `run_domain_op` gives the guarded
        // write below; the race-safe re-check that actually protects
        // the mutation still happens there, immediately before the
        // `UPDATE`.
        if let Some(principal_instance_id) = principal_instance_id {
            let owner: Option<String> = self
                .conn
                .query_row(
                    "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                    [task_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            match owner {
                Some(owner) if owner == principal_instance_id => {}
                Some(_) => {
                    return Err(DomainError::NotOwner {
                        task_id: task_id.to_string(),
                        instance_id: principal_instance_id.to_string(),
                    });
                }
                None => {
                    return Err(DomainError::NotFound {
                        kind: "task",
                        id: task_id.to_string(),
                    });
                }
            }
        }

        let from = RunState::try_from(from_str.as_str()).map_err(|_| DomainError::NotFound {
            kind: "run-state",
            id: from_str.clone(),
        })?;
        check_transition(&run_id.to_string(), &from, to)?;

        let worker_id =
            crew_protocol::WorkerId::parse(&worker_id_str).map_err(|_| DomainError::NotFound {
                kind: "worker",
                id: worker_id_str.clone(),
            })?;

        let kind = kind_for_state(to);
        let event = RuntimeEvent::RunEvent {
            kind,
            run_id,
            task_id,
            worker_id,
            state: to.to_string(),
        };
        let to_owned = to.clone();
        let is_terminal = to.is_terminal();
        let entering_working = to.to_string() == "starting";
        let principal_instance_id = principal_instance_id.map(str::to_string);
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            move |tx| {
                let now = Timestamp::now();
                // Ownership is arbitrated here, inside the guarded write,
                // immediately before the mutating `UPDATE` -- see
                // `submit_run`'s doc comment for why a re-read from inside
                // this same closure, not a caller-side snapshot, is what
                // makes this safe against a concurrent `reconcile/omp`
                // rebind (R70-R77 doctrine).
                if let Some(principal_instance_id) = principal_instance_id {
                    let owner: Option<String> = tx
                        .query_row(
                            "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                            [task_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match owner {
                        Some(owner) if owner == principal_instance_id => {}
                        Some(_) => {
                            return Err(DomainError::NotOwner {
                                task_id: task_id.to_string(),
                                instance_id: principal_instance_id,
                            });
                        }
                        None => {
                            return Err(DomainError::NotFound {
                                kind: "task",
                                id: task_id.to_string(),
                            });
                        }
                    }
                }
                tx.execute(
                    "UPDATE runs SET state = ?1 WHERE run_id = ?2",
                    rusqlite::params![to_owned.to_string(), run_id.to_string()],
                )?;
                if entering_working {
                    tx.execute(
                        "UPDATE runs SET started_at = COALESCE(started_at, ?1) WHERE run_id = ?2",
                        rusqlite::params![now.as_str(), run_id.to_string()],
                    )?;
                }
                if is_terminal {
                    tx.execute(
                        "UPDATE runs SET completed_at = ?1 WHERE run_id = ?2",
                        rusqlite::params![now.as_str(), run_id.to_string()],
                    )?;
                }
                Ok(())
            },
        )
    }

    /// Reads the run's current flags row, immediately before whatever
    /// guarded write is about to build a new [`RunFlags`] value from it --
    /// on `self.conn`, *before* [`Self::append_and_apply`] opens its SQL
    /// transaction, not inside it. Shared by [`Self::set_run_flag`] and
    /// [`Self::release_quarantine`] so both flag-mutating entry points
    /// read from the identical query. See [`Self::set_run_flag`]'s doc
    /// comment for why a plain, pre-transaction read is still atomic with
    /// respect to concurrent writers (R73): [`crate::db::DatabaseHandle`]'s
    /// single-owner actor thread, not a transaction, is what closes the
    /// gap.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if `run_id` does not exist.
    pub(crate) fn read_run_flags(
        &self,
        run_id: crew_protocol::RunId,
    ) -> Result<RunFlags, DomainError> {
        self.conn
            .query_row(
                "SELECT flags_degraded_control, flags_needs_reconciliation, flags_protocol_unhealthy,
                        flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
                        flags_turn_settled
                 FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| {
                    Ok(RunFlags {
                        degraded_control: row.get::<_, i64>(0)? != 0,
                        needs_reconciliation: row.get::<_, i64>(1)? != 0,
                        protocol_unhealthy: row.get::<_, i64>(2)? != 0,
                        policy_quarantined: row.get::<_, i64>(3)? != 0,
                        workspace_dirty: row.get::<_, i64>(4)? != 0,
                        children_active: row.get::<_, i64>(5)? != 0,
                        turn_settled: row.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .map_err(|_| DomainError::NotFound {
                kind: "run",
                id: run_id.to_string(),
            })
    }

    /// Journals `prompt` as `run_id`'s durable intent (ADR-0028).
    ///
    /// Appends an event and mutates no table. The prompt is evidence of
    /// what was *asked*, not run state, so it belongs in the journal
    /// rather than in a `runs` column -- which also means the retention
    /// sweep bounds it like every other event, instead of one row pinning
    /// a prompt forever.
    ///
    /// `prompt` must already have crossed the [ADR-0006] boundary. This
    /// method cannot enforce that: like every event field after redaction
    /// it is a plain `String`, and the type system's guarantee ends where
    /// the redactor's output begins. Its sole caller (`run/submit`)
    /// classifies the prompt `Visible` and passes it through
    /// `Redactor::sanitize_fragment` first, and the test that proves the
    /// property byte-scans the database rather than trusting this comment.
    pub fn record_run_prompt(
        &mut self,
        run_id: crew_protocol::RunId,
        task_id: crew_protocol::TaskId,
        worker_id: crew_protocol::WorkerId,
        prompt: String,
    ) -> Result<Committed, DomainError> {
        let event = RuntimeEvent::RunPromptEvent {
            run_id,
            task_id,
            worker_id,
            prompt,
        };
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            |_tx| Ok(()),
        )
    }

    /// Writes `flags` verbatim to `run_id`'s row and journals the matching
    /// `RunFlagsChanged` event. Shared by [`Self::set_run_flag`] and
    /// [`Self::release_quarantine`] so both build their commit from the
    /// exact same `UPDATE`/event pair -- one guarded-write path for every
    /// mutation of `runs.flags_*`, preserving R73's sole-writer property
    /// even as R75 adds a second caller.
    fn write_run_flags(
        &mut self,
        run_id: crew_protocol::RunId,
        flags: RunFlags,
    ) -> Result<Committed, DomainError> {
        let event = RuntimeEvent::RunFlagsEvent {
            run_id,
            flags: flags.clone(),
        };
        self.append_and_apply(&event, None, None, Some(run_id), move |tx| {
            tx.execute(
                "UPDATE runs SET
                   flags_degraded_control = ?1, flags_needs_reconciliation = ?2,
                   flags_protocol_unhealthy = ?3, flags_policy_quarantined = ?4,
                   flags_workspace_dirty = ?5, flags_children_active = ?6,
                   flags_turn_settled = ?7
                 WHERE run_id = ?8",
                rusqlite::params![
                    flags.degraded_control as i64,
                    flags.needs_reconciliation as i64,
                    flags.protocol_unhealthy as i64,
                    flags.policy_quarantined as i64,
                    flags.workspace_dirty as i64,
                    flags.children_active as i64,
                    flags.turn_settled as i64,
                    run_id.to_string(),
                ],
            )?;
            Ok(())
        })
    }

    /// Reads the run's current flags, flips exactly `flag` to `value`, and
    /// writes the whole row back -- all inside this one call, with nothing
    /// else able to observe or mutate the row in between -- then emits a
    /// `RunFlagsChanged` event carrying the resulting full [`RunFlags`]
    /// struct (the event's wire shape is unchanged).
    ///
    /// This replaced a `set_run_flags(run_id, &RunFlags)` that took the
    /// whole struct from the caller. Every real caller only ever wanted to
    /// flip one flag, having read the "current" struct from an earlier,
    /// separate `run_domain_op` round trip -- a snapshot that could go
    /// stale if anything else mutated a *different* flag on the same run
    /// while the caller was, say, awaiting a vendor callback.
    /// [`crate::db::DatabaseHandle`]'s actor interleaves whole
    /// `run_domain_op` closures, never a caller's async steps, so that
    /// snapshot-then-write-back shape could silently revert a concurrent
    /// flag change: a lost update neither side detects (R73). Reading and
    /// writing inside this one call removes the gap -- R70-R72's
    /// guarded-write doctrine applied to a flag flip rather than a
    /// decision.
    ///
    /// The read ([`Self::read_run_flags`]) executes on `self.conn`
    /// *before* [`Self::append_and_apply`] opens its SQL transaction, not
    /// inside it: the event this method emits carries the post-flip
    /// [`RunFlags`] struct by value, so that struct must be fully built
    /// before the closure handed to `append_and_apply` even exists. So the
    /// thing that actually guards this read against a racing write is not
    /// a transaction -- it is [`crate::db::DatabaseHandle`]'s single-owner
    /// actor thread, which runs one `run_domain_op` closure to completion
    /// before starting the next. That makes this method's read-then-write
    /// atomic at *closure* granularity, unlike
    /// [`Self::resolve_policy_violation`], which re-reads from inside its
    /// already-open `tx` and is guarded at *transaction* granularity.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if `run_id` does not exist.
    pub fn set_run_flag(
        &mut self,
        run_id: crew_protocol::RunId,
        flag: RunFlag,
        value: bool,
    ) -> Result<Committed, DomainError> {
        let mut flags = self.read_run_flags(run_id)?;
        flag.apply(&mut flags, value);
        self.write_run_flags(run_id, flags)
    }

    /// Clears `Run.flags.policyQuarantined`, but only if no *other* policy
    /// violation against this run is still unresolved. Called by
    /// [`crate::policy::ViolationService::decide`]'s `"release"` path,
    /// after [`Self::resolve_policy_violation`] has already committed the
    /// resolution being released -- so the `policy_violations` count below
    /// never counts that violation, only a *different*, still-open one.
    ///
    /// This is the fix for the second half of R75: `decide`'s release used
    /// to call an unconditional [`Self::set_run_flag`]`(run_id,
    /// PolicyQuarantined, false)` as its own, independent commit. A fresh
    /// violation recorded on this run (by
    /// [`Self::record_policy_violation`]/[`crate::policy::ViolationService::apply_action`])
    /// in the gap between the release's `resolve_policy_violation` commit
    /// and that unconditional clear got its quarantine silently reverted
    /// by a release that targeted an entirely different, unrelated
    /// violation. Both reads here ([`Self::read_run_flags`], then the
    /// `policy_violations` count) and the write they gate run on
    /// `self.conn`/inside this one call, before [`Self::append_and_apply`]
    /// opens its transaction -- the same closure-granularity boundary
    /// [`Self::set_run_flag`] uses (R73): [`crate::db::DatabaseHandle`]'s
    /// single-owner actor thread runs one whole `run_domain_op` closure to
    /// completion before starting the next, so a fresh violation's own
    /// [`Self::record_policy_violation`] commit and this method's
    /// unresolved-count read can never interleave with each other --
    /// whichever one commits first is the one the other observes.
    ///
    /// Returns `Ok(None)` with no write and no event if the flag is
    /// already clear or another violation is still unresolved; `Ok(Some(_))`
    /// with the commit if the flag was cleared.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if `run_id` does not exist.
    pub fn release_quarantine(
        &mut self,
        run_id: crew_protocol::RunId,
    ) -> Result<Option<Committed>, DomainError> {
        let mut flags = self.read_run_flags(run_id)?;
        if !flags.policy_quarantined {
            return Ok(None);
        }
        let other_unresolved: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM policy_violations WHERE run_id = ?1 AND resolution IS NULL",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        if other_unresolved > 0 {
            return Ok(None);
        }
        flags.policy_quarantined = false;
        self.write_run_flags(run_id, flags).map(Some)
    }

    /// Records a message in `recorded` delivery state (record-before-send).
    /// Emits a `MessageRecorded` event.
    ///
    /// `principal_instance_id` arbitrates ownership against the message's
    /// *run* (re-read from `runs` inside this guarded write, immediately
    /// before the `INSERT`, then checked against `tasks` -- R77), never
    /// against `message.task_id` as presented: that field is caller
    /// content, not something this write may trust for authorization, so
    /// a caller cannot dodge the check by asserting a `taskId` it happens
    /// to own for a `runId` it does not. `None` for `coordination/send`
    /// and `coordination/publishArtifact` (`crate::coordination::broker`):
    /// a `workerMcp` principal's authority is its scope token, already
    /// verified against the run at connection time and re-checked by
    /// `run_task_id != task_id` in the broker -- it is never the
    /// task-owning `ompExtension` instance, so an ownership check here
    /// would refuse every legitimate worker message. Only `message/send`
    /// (`ompExtension`-only) passes `Some`.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if the message's run or task does
    /// not exist, or [`DomainError::NotOwner`] if `principal_instance_id`
    /// is `Some` and does not own the run's task.
    ///
    /// When `enforce_quarantine` is set, the write also refuses (inside
    /// the same guarded transaction, after the owner re-read so a
    /// non-owner cannot probe quarantine state) if the run is
    /// policy-quarantined (R78). Callers that must deliver even from a
    /// quarantined run (`coordination/send`, `reportBlocked`, `askPolicy`
    /// -- everything routed through the broker's `send_internal`; the
    /// quarantine gate is `publishArtifact`-only by design) pass `false`.
    ///
    /// When `enforce_live` is set, the write refuses (same transaction,
    /// same ordering rationale) if the run has already settled (R94):
    /// the broker's `require_live_run` is a caller-side pre-check in its
    /// own round trip, so a run settling between that check and this
    /// write would otherwise journal a message against a terminal run.
    pub fn record_message(
        &mut self,
        message: &RunMessage,
        principal_instance_id: Option<&str>,
        enforce_quarantine: bool,
        enforce_live: bool,
    ) -> Result<(Committed, bool), DomainError> {
        let event = RuntimeEvent::MessageEvent {
            kind: RuntimeEventKind::MessageRecorded,
            message_id: message.message_id,
            run_id: message.run_id,
            task_id: message.task_id,
            delivery_state: delivery_state_str(&message.delivery_state).to_string(),
        };
        let message = message.clone();
        let principal_instance_id = principal_instance_id.map(str::to_string);
        // Set inside the guarded write when this message resolved the
        // run's open question escalation (WP20) -- read after the commit.
        let answered = std::rc::Rc::new(std::cell::Cell::new(false));
        let answered_for_tx = std::rc::Rc::clone(&answered);
        let committed = self.append_and_apply(
            &event,
            Some(message.task_id),
            Some(message.sender_worker_id),
            Some(message.run_id),
            move |tx| {
                if let Some(principal_instance_id) = principal_instance_id {
                    let owning_task_id: String = tx
                        .query_row(
                            "SELECT task_id FROM runs WHERE run_id = ?1",
                            [message.run_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()?
                        .ok_or(DomainError::NotFound {
                            kind: "run",
                            id: message.run_id.to_string(),
                        })?;
                    let owner: Option<String> = tx
                        .query_row(
                            "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                            [owning_task_id.clone()],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match owner {
                        Some(owner) if owner == principal_instance_id => {}
                        Some(_) => {
                            return Err(DomainError::NotOwner {
                                task_id: owning_task_id,
                                instance_id: principal_instance_id,
                            });
                        }
                        None => {
                            return Err(DomainError::NotFound {
                                kind: "task",
                                id: owning_task_id,
                            });
                        }
                    }
                }
                if enforce_live {
                    // Re-read the run's state inside this same
                    // transaction (R94): the broker's require_live_run
                    // pre-check reads a snapshot the database actor can
                    // interleave a settling transition behind.
                    let state: String = tx.query_row(
                        "SELECT state FROM runs WHERE run_id = ?1",
                        [message.run_id.to_string()],
                        |row| row.get(0),
                    )?;
                    let parsed =
                        RunState::try_from(state.as_str()).map_err(|_| DomainError::NotFound {
                            kind: "run-state",
                            id: state.clone(),
                        })?;
                    if parsed.is_terminal() {
                        return Err(DomainError::RunSettled {
                            run_id: message.run_id.to_string(),
                        });
                    }
                }
                if enforce_quarantine {
                    // Read the flag inside this same transaction -- a
                    // caller-side pre-check reads a snapshot the database
                    // actor can interleave a quarantine behind (R78).
                    // Deliberately AFTER the owner re-read above, so a
                    // non-owner cannot distinguish a quarantined run from
                    // a healthy one by error code.
                    let quarantined: i64 = tx.query_row(
                        "SELECT flags_policy_quarantined FROM runs WHERE run_id = ?1",
                        [message.run_id.to_string()],
                        |row| row.get(0),
                    )?;
                    if quarantined != 0 {
                        return Err(DomainError::PolicyQuarantined {
                            run_id: message.run_id.to_string(),
                        });
                    }
                }
                // Turn budget guard (WP19): leader-originated steering
                // kinds consume a worker turn. Checked inside this same
                // guarded transaction -- a caller-side pre-check would
                // read a snapshot another send could land behind. A run
                // with no budgets row (submitted before WP19, or via
                // retry) has no explicit budget and is never refused.
                if is_turn_consuming(&message.kind) {
                    let row: Option<(i64, i64)> = tx
                        .query_row(
                            "SELECT turns_used, turn_limit FROM budgets WHERE run_id = ?1",
                            [message.run_id.to_string()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()?;
                    if let Some((turns_used, turn_limit)) = row {
                        let (turns_used, turn_limit) = (u32::try_from(turns_used).unwrap_or(u32::MAX), u32::try_from(turn_limit).unwrap_or(0));
                        if turns_used >= turn_limit {
                            return Err(DomainError::BudgetExceeded {
                                run_id: message.run_id.to_string(),
                                turns_used,
                                turn_limit,
                            });
                        }
                        tx.execute(
                            "UPDATE budgets SET turns_used = turns_used + 1 WHERE run_id = ?1",
                            [message.run_id.to_string()],
                        )?;
                    }
                }
                tx.execute(
                    "INSERT INTO messages (message_id, run_id, sender_worker_id, recipient_worker_id,
                       task_id, kind, payload, delivery_state, created_at, sent_at, acknowledged_at, reply_to)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    rusqlite::params![
                        message.message_id.to_string(),
                        message.run_id.to_string(),
                        message.sender_worker_id.to_string(),
                        message.recipient_worker_id.map(|w| w.to_string()),
                        message.task_id.to_string(),
                        message_kind_str(&message.kind),
                        message.payload,
                        delivery_state_str(&message.delivery_state),
                        message.created_at.as_str(),
                        message.sent_at.as_ref().map(|t| t.as_str().to_string()),
                        message.acknowledged_at.as_ref().map(|t| t.as_str().to_string()),
                        message.reply_to.map(|m| m.to_string()),
                    ],
                )?;
                if message.kind == crew_protocol::MessageKind::Answer
                    && message.reply_to.is_some()
                {
                    let now = Timestamp::now();
                    let resolved = tx.execute(
                        "UPDATE escalations SET answered_by = 'ompExtension', \
                           answer = ?1, decided_at = ?2 \
                         WHERE run_id = ?3 AND kind = 'question' AND decided_at IS NULL",
                        rusqlite::params![
                            message.payload,
                            now.as_str(),
                            message.run_id.to_string(),
                        ],
                    )? > 0;
                    if resolved {
                        answered_for_tx.set(true);
                    }
                }
                Ok(())
            },
        )?;
        Ok((committed, answered.get()))
    }
    /// Appends a `Diagnostic` event scoped to `run_id`, with no projection
    /// side effect. Used for runtime-observed conditions -- such as a
    /// follow-up message that could not be delivered to a running adapter
    /// -- that must be journaled and broadcast without failing the RPC
    /// that triggered them or mutating any record.
    pub fn record_diagnostic(
        &mut self,
        run_id: RunId,
        level: crew_protocol::DiagnosticLevel,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Result<Committed, DomainError> {
        let event = RuntimeEvent::Diagnostic {
            level,
            code: code.into(),
            message: message.into(),
        };
        self.append_and_apply(&event, None, None, Some(run_id), |_tx| Ok(()))
    }

    /// Snapshots a run's turn budget (WP19): records the plan provenance on
    /// the run row and creates its `budgets` row with the resolved limit.
    /// Projection-only bookkeeping -- the `RunQueued` event already
    /// committed the submission fact; a budget is a *limit* the leader's
    /// plan and config imply for the run, and the durable facts are the
    /// later `BudgetExceeded` and turn-consuming message events that
    /// reference it. Written once at submit; a run whose row already
    /// exists keeps it.
    ///
    /// # Errors
    /// Returns [`DomainError`] if either projection write fails.
    pub fn attach_turn_budget(
        &mut self,
        run_id: RunId,
        task_id: TaskId,
        plan_ref: Option<String>,
        turn_limit: u32,
    ) -> Result<(), DomainError> {
        if let Some(plan_ref) = &plan_ref {
            self.conn.execute(
                "UPDATE runs SET plan_ref = ?1 WHERE run_id = ?2",
                rusqlite::params![plan_ref, run_id.to_string()],
            )?;
        }
        self.conn.execute(
            "INSERT INTO budgets (run_id, task_id, turns_used, turn_limit) VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![
                run_id.to_string(),
                task_id.to_string(),
                i64::from(turn_limit)
            ],
        )?;
        Ok(())
    }

    /// Journals the durable `BudgetExceeded` fact for a run whose guard
    /// just refused a steering message (WP19). Reads the current counter
    /// and the run's correlating ids here rather than trusting
    /// caller-carried numbers, so the event always matches the row the
    /// refusal saw. The caller broadcasts the returned envelope; the RPC
    /// answer itself carries the typed `BUDGET_EXCEEDED` code.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the run or its budget row is
    /// missing, plus the usual append failures.
    /// Journals the durable `EscalationAnswered` fact for a run whose open
    /// question escalation a just-recorded Answer message resolved (WP20).
    /// Reads the run's correlating ids and the stored answer here rather
    /// than trusting caller-carried values, so the event always matches the
    /// row the resolution wrote.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the run or its escalation is
    /// missing, plus the usual append failures.
    pub fn journal_escalation_answered(
        &mut self,
        run_id: RunId,
        answered_by: crew_protocol::AnsweredBy,
    ) -> Result<Committed, DomainError> {
        let (task_id, worker_id, answer): (String, String, Option<String>) = self
            .conn
            .query_row(
                "SELECT r.task_id, r.worker_id, e.answer \
                 FROM runs r JOIN escalations e ON e.run_id = r.run_id \
                 WHERE r.run_id = ?1 AND e.kind = 'question' AND e.decided_at IS NOT NULL \
                 ORDER BY e.decided_at DESC LIMIT 1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or(DomainError::NotFound {
                kind: "escalation",
                id: run_id.to_string(),
            })?;
        let task_id = TaskId::parse(&task_id).map_err(|_| DomainError::NotFound {
            kind: "task",
            id: task_id.clone(),
        })?;
        let worker_id = WorkerId::parse(&worker_id).map_err(|_| DomainError::NotFound {
            kind: "worker",
            id: worker_id.clone(),
        })?;
        let event = RuntimeEvent::EscalationAnswered {
            run_id,
            task_id,
            worker_id,
            answered_by,
            answer,
        };
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            |_tx| Ok(()),
        )
    }

    /// Journals an `EscalationRaised` fact for `run_id` (WP20): a
    /// machine-assigned `reason` (`write_violation`, `repeated_failure`)
    /// plus optional redacted `question` text. Projection-only callers --
    /// the sink's write-violation detector and the lifecycle's
    /// repeated-failure check -- invoke this as their own committed op and
    /// broadcast the returned envelope themselves.
    ///
    /// # Errors
    /// Returns [`DomainError`] for the usual append failures; a run that
    /// vanished between detection and commit surfaces as
    /// [`DomainError::NotFound`].
    pub fn record_escalation_raised(
        &mut self,
        run_id: RunId,
        reason: impl Into<String>,
        question: Option<String>,
    ) -> Result<Committed, DomainError> {
        let reason = reason.into();
        let (task_id, worker_id): (String, String) = self
            .conn
            .query_row(
                "SELECT task_id, worker_id FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(DomainError::NotFound {
                kind: "run",
                id: run_id.to_string(),
            })?;
        let task_id = TaskId::parse(&task_id).map_err(|_| DomainError::NotFound {
            kind: "task",
            id: task_id.clone(),
        })?;
        let worker_id = WorkerId::parse(&worker_id).map_err(|_| DomainError::NotFound {
            kind: "worker",
            id: worker_id.clone(),
        })?;
        let event = RuntimeEvent::EscalationRaised {
            run_id,
            task_id,
            worker_id,
            reason: reason.clone(),
            question: question.clone(),
        };
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            move |tx| {
                // I1: persist the escalation projection row inside the same
                // transaction that appends the `EscalationRaised` event
                // (invariant 1: intent and side-effect projection commit
                // together). The sibling site `record_adapter_event` inserts
                // its row in this closure too; inserting on `conn` directly
                // would orphan the row if the event append rolled back.
                tx.execute(
                    "INSERT INTO escalations (escalation_id, run_id, kind, question, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        crew_protocol::EscalationId::new().to_string(),
                        run_id.to_string(),
                        reason,
                        question,
                        Timestamp::now().as_str(),
                    ],
                )?;
                Ok(())
            },
        )
    }

    /// Whether this run's predecessor for the same task also ended `failed`
    /// -- WP20's two-consecutive-failures trigger for a `repeated_failure`
    /// escalation. The predecessor is the most-recent prior run by
    /// `(started_at, run_id)` for this task, **regardless of terminality**:
    /// the query does not filter to terminal states, so a concurrently
    /// running (non-terminal) sibling that started after the real last
    /// terminal run can be picked instead. This is a deliberate fail-safe,
    /// not a gap -- it only ever *masks* a genuine two-in-a-row streak
    /// (reads as `false` when the true predecessor in fact failed), never
    /// fabricates one, so it can only under-escalate, never spuriously
    /// escalate.
    ///
    /// # Errors
    /// Returns [`DomainError`] on query failure only; an unknown run reads
    /// as `false`.
    #[must_use]
    pub fn previous_run_for_task_also_failed(&self, run_id: RunId) -> bool {
        let Ok(task_id): Result<String, _> = self.conn.query_row(
            "SELECT task_id FROM runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        ) else {
            return false;
        };
        let Ok(prev_state): Result<String, _> = self.conn.query_row(
            "SELECT state FROM runs \
                 WHERE task_id = ?1 AND run_id != ?2 AND started_at IS NOT NULL \
                 AND started_at < (SELECT started_at FROM runs WHERE run_id = ?2) \
                 ORDER BY started_at DESC, run_id DESC \
                 LIMIT 1",
            rusqlite::params![task_id, run_id.to_string()],
            |row| row.get(0),
        ) else {
            // No preceding run for this task -- not a repeat.
            return false;
        };
        prev_state.as_str() == "failed"
    }

    pub fn journal_budget_exceeded(&mut self, run_id: RunId) -> Result<Committed, DomainError> {
        let (task_id, worker_id): (String, String) = self
            .conn
            .query_row(
                "SELECT task_id, worker_id FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(DomainError::NotFound {
                kind: "run",
                id: run_id.to_string(),
            })?;
        let (turns_used, turn_limit): (i64, i64) = self
            .conn
            .query_row(
                "SELECT turns_used, turn_limit FROM budgets WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(DomainError::NotFound {
                kind: "budget",
                id: run_id.to_string(),
            })?;
        let task_id = TaskId::parse(&task_id).map_err(|_| DomainError::NotFound {
            kind: "task",
            id: task_id.clone(),
        })?;
        let worker_id = WorkerId::parse(&worker_id).map_err(|_| DomainError::NotFound {
            kind: "worker",
            id: worker_id.clone(),
        })?;
        let event = RuntimeEvent::BudgetExceeded {
            run_id,
            task_id,
            worker_id,
            turns_used: u32::try_from(turns_used).unwrap_or(u32::MAX),
            turn_limit: u32::try_from(turn_limit).unwrap_or(0),
        };
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            |_tx| Ok(()),
        )
    }

    /// WP20's write-violation detector: when `tool_name` is a write-shaped
    /// tool AND the run was spawned from an approved plan subtask that
    /// declared `writes: false`, opens a `write_violation` escalation and
    /// journals `EscalationRaised{reason: "write_violation"}`. `None` when
    /// the run has no plan provenance (never backfilled), the referenced
    /// subtask declared writes, or the tool is not write-shaped -- those
    /// are clean no-ops, never errors.
    ///
    /// # Errors
    /// Returns [`DomainError`] for missing rows once detection matched,
    /// and the usual append failures.
    pub fn raise_write_violation_if_declared_read_only(
        &mut self,
        run_id: RunId,
        _tool_name: &str,
    ) -> Result<Option<Committed>, DomainError> {
        let plan_ref: Option<String> = self
            .conn
            .query_row(
                "SELECT plan_ref FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(plan_ref) = plan_ref else {
            return Ok(None);
        };
        // plan_ref is the JSON `run/submit` stored: {"planId", "subtaskId"}.
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&plan_ref) else {
            return Ok(None);
        };
        let (Some(plan_id), Some(subtask_id)) = (
            parsed.get("planId").and_then(serde_json::Value::as_str),
            parsed.get("subtaskId").and_then(serde_json::Value::as_str),
        ) else {
            return Ok(None);
        };
        let subtasks_json: Option<String> = self
            .conn
            .query_row(
                "SELECT subtasks_json FROM plans WHERE run_id = ?1",
                [plan_id],
                |row| row.get(0),
            )
            .optional()?;
        // `subtasks_json` is `serde_json::to_string(&plan.subtasks)`: a BARE
        // array, not an object wrapping one (matches propose_plan's writer).
        let declared_writes = subtasks_json
            .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
            .and_then(|v| {
                v.as_array()?
                    .iter()
                    .find(|s| s["id"] == serde_json::Value::String(subtask_id.to_string()))
                    .map(|s| s["writes"] == serde_json::Value::Bool(true))
            });
        match declared_writes {
            // Declared read-only in this plan -> violation.
            Some(false) => {}
            _ => return Ok(None),
        }
        self.record_escalation_raised(run_id, "write_violation", None)
            .map(Some)
    }

    /// Journals a `WorkerTimeout` liveness report for `run_id` (WP19),
    /// unless the run has settled or is unknown -- the sweep snapshots the
    /// clock before this check, so a run that terminated between snapshot
    /// and commit must not receive a timeout fact. Returns the committed
    /// envelope for broadcast, or `None` when there was nothing to report.
    ///
    /// # Errors
    /// Returns [`DomainError`] for the usual append failures.
    pub fn record_worker_timeout_if_live(
        &mut self,
        run_id: RunId,
        kind: crew_protocol::TimeoutKind,
        since_ms: u64,
    ) -> Result<Option<Committed>, DomainError> {
        let Some((task_id, worker_id, state)): Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT task_id, worker_id, state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let parsed_state =
            RunState::try_from(state.as_str()).map_err(|_| DomainError::NotFound {
                kind: "run-state",
                id: state.clone(),
            })?;
        if parsed_state.is_terminal() {
            return Ok(None);
        }
        let task_id = TaskId::parse(&task_id).map_err(|_| DomainError::NotFound {
            kind: "task",
            id: task_id.clone(),
        })?;
        let worker_id = WorkerId::parse(&worker_id).map_err(|_| DomainError::NotFound {
            kind: "worker",
            id: worker_id.clone(),
        })?;
        let event = RuntimeEvent::WorkerTimeout {
            run_id,
            task_id,
            worker_id,
            kind,
            since_ms,
        };
        Ok(Some(self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            |_tx| Ok(()),
        )?))
    }

    /// ADR-0027 wave 3's inactivity backstop: settles a run whose vendor
    /// finished a turn that the leader then never closed.
    ///
    /// Settles to `lost`, never `succeeded`. The runtime observed only
    /// silence, and ADR-0015's precedent is to name that uncertainty rather
    /// than guess an outcome -- a success reached by nobody looking would be
    /// a fabricated fact. The run's journaled result text stays readable
    /// either way, since `run/result` reads any terminal state.
    ///
    /// Returns `None` unless the run is parked in `waitingUser` **and**
    /// flagged `turnSettled`. That pairing is the whole safety property: a
    /// `waitingUser` that means "the worker asked a question" or "an
    /// approval is pending" must never be settled by a timeout, and neither
    /// must a `waitingPeer` or a `paused` run. The read and the transition
    /// share one transaction so a leader finishing the run concurrently
    /// cannot be raced.
    pub fn settle_abandoned_turn(
        &mut self,
        run_id: RunId,
    ) -> Result<Option<Committed>, DomainError> {
        let Some((state, turn_settled)): Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT state, flags_turn_settled FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        if state != "waitingUser" || turn_settled == 0 {
            return Ok(None);
        }
        let to = RunState::try_from("lost").map_err(|_| DomainError::NotFound {
            kind: "run-state",
            id: "lost".to_string(),
        })?;
        self.transition_run(run_id, &to, None).map(Some)
    }

    /// Updates a message's delivery state. Emits the matching `Message*`
    /// event.
    pub fn update_delivery(
        &mut self,
        message_id: crew_protocol::MessageId,
        state: &DeliveryState,
    ) -> Result<Committed, DomainError> {
        let (run_id_str, task_id_str): (String, String) = self
            .conn
            .query_row(
                "SELECT run_id, task_id FROM messages WHERE message_id = ?1",
                [message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| DomainError::NotFound {
                kind: "message",
                id: message_id.to_string(),
            })?;
        let run_id =
            crew_protocol::RunId::parse(&run_id_str).map_err(|_| DomainError::NotFound {
                kind: "run",
                id: run_id_str.clone(),
            })?;
        let task_id =
            crew_protocol::TaskId::parse(&task_id_str).map_err(|_| DomainError::NotFound {
                kind: "task",
                id: task_id_str.clone(),
            })?;

        let kind = match state {
            DeliveryState::Sent => RuntimeEventKind::MessageSent,
            DeliveryState::Acknowledged => RuntimeEventKind::MessageAcknowledged,
            DeliveryState::Failed => RuntimeEventKind::MessageFailed,
            DeliveryState::Recorded | DeliveryState::Unknown => RuntimeEventKind::MessageRecorded,
        };
        let event = RuntimeEvent::MessageEvent {
            kind,
            message_id,
            run_id,
            task_id,
            delivery_state: delivery_state_str(state).to_string(),
        };
        let state = state.clone();
        self.append_and_apply(&event, Some(task_id), None, Some(run_id), move |tx| {
            let now = Timestamp::now();
            tx.execute(
                "UPDATE messages SET delivery_state = ?1 WHERE message_id = ?2",
                rusqlite::params![delivery_state_str(&state), message_id.to_string()],
            )?;
            match state {
                DeliveryState::Sent => {
                    tx.execute(
                        "UPDATE messages SET sent_at = COALESCE(sent_at, ?1) WHERE message_id = ?2",
                        rusqlite::params![now.as_str(), message_id.to_string()],
                    )?;
                }
                DeliveryState::Acknowledged => {
                    tx.execute(
                        "UPDATE messages SET acknowledged_at = COALESCE(acknowledged_at, ?1) WHERE message_id = ?2",
                        rusqlite::params![now.as_str(), message_id.to_string()],
                    )?;
                }
                _ => {}
            }
            Ok(())
        })
    }

    /// Creates an approval request and atomically transitions its run
    /// `working -> waitingUser`, in one durable event. Called when an
    /// adapter reports it needs approval for `action`.
    pub fn create_approval(
        &mut self,
        approval: &ApprovalRequest,
    ) -> Result<Committed, DomainError> {
        let (from_str,): (String,) = self
            .conn
            .query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [approval.run_id.to_string()],
                |row| Ok((row.get(0)?,)),
            )
            .map_err(|_| DomainError::NotFound {
                kind: "run",
                id: approval.run_id.to_string(),
            })?;
        let from = RunState::try_from(from_str.as_str()).map_err(|_| DomainError::NotFound {
            kind: "run-state",
            id: from_str.clone(),
        })?;
        let waiting_user = RunState::try_from("waitingUser").expect("waitingUser is valid");
        check_transition(&approval.run_id.to_string(), &from, &waiting_user)?;

        let event = RuntimeEvent::ApprovalEvent {
            kind: RuntimeEventKind::ApprovalRequested,
            approval_id: approval.approval_id,
            run_id: approval.run_id,
            task_id: approval.task_id,
            action: approval.action.clone(),
            decided_by: None,
            reason: None,
        };
        let approval = approval.clone();
        self.append_and_apply(
            &event,
            Some(approval.task_id),
            None,
            Some(approval.run_id),
            move |tx| {
                tx.execute(
                    "INSERT INTO approvals (approval_id, run_id, task_id, action, arguments,
                       human_required, policy_reason, created_at, decided_at, decision)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        approval.approval_id.to_string(),
                        approval.run_id.to_string(),
                        approval.task_id.to_string(),
                        approval.action,
                        serde_json::to_string(&approval.arguments)?,
                        approval.human_required as i64,
                        approval.policy_reason,
                        approval.created_at.as_str(),
                        approval.decided_at.as_ref().map(|t| t.as_str().to_string()),
                        approval.decision,
                    ],
                )?;
                tx.execute(
                    "UPDATE runs SET state = 'waitingUser' WHERE run_id = ?1",
                    rusqlite::params![approval.run_id.to_string()],
                )?;
                Ok(())
            },
        )
    }

    /// Records an approval decision: sets `decision`/`decided_at`/`decided_by`
    /// and appends an `ApprovalDecided` event.
    ///
    /// This is the **only** authority on whether an approval may be decided.
    /// The database actor interleaves whole `run_domain_op` closures, never
    /// a service's sequence of round trips, so any caller-side pre-check is
    /// advisory only (R70, R71): ownership, conflict, and the terminal-run
    /// state are all re-checked from inside this one guarded transaction,
    /// never from a snapshot a caller read earlier. `principal_instance_id`
    /// is checked against `tasks.owner_client_instance_id` first, before the
    /// `UPDATE` guarded by `decision IS NULL` -- a `reconcile/omp` ownership
    /// rebind that commits between a caller's snapshot read and this write
    /// must invalidate the stale caller, and it can only do that if the
    /// check happens here, not in `ApprovalService::decide`. The `UPDATE`
    /// deliberately precedes the terminal-run guard so an already-decided
    /// approval reports [`DomainError::AlreadyResolved`] even when its run
    /// has also settled; an `Err` returned here discards the appended event
    /// together with the rejected write (the transaction rolls back as a
    /// whole).
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if no such approval or task exists,
    /// [`DomainError::NotOwner`] if `principal_instance_id` does not own the
    /// approval's task, [`DomainError::AlreadyResolved`] if a decision is
    /// already on record, or [`DomainError::RunSettled`] if the run has
    /// reached a terminal state.
    pub fn decide_approval(
        &mut self,
        approval_id: crew_protocol::ApprovalId,
        principal_instance_id: &str,
        decision: &str,
        reason: &str,
        decided_by: crew_protocol::DecidedBy,
    ) -> Result<Committed, DomainError> {
        let (run_id_str, task_id_str, action): (String, String, String) = self
            .conn
            .query_row(
                "SELECT run_id, task_id, action FROM approvals WHERE approval_id = ?1",
                [approval_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|_| DomainError::NotFound {
                kind: "approval",
                id: approval_id.to_string(),
            })?;
        let run_id =
            crew_protocol::RunId::parse(&run_id_str).map_err(|_| DomainError::NotFound {
                kind: "run",
                id: run_id_str.clone(),
            })?;
        let task_id =
            crew_protocol::TaskId::parse(&task_id_str).map_err(|_| DomainError::NotFound {
                kind: "task",
                id: task_id_str.clone(),
            })?;

        let event = RuntimeEvent::ApprovalEvent {
            kind: RuntimeEventKind::ApprovalDecided,
            approval_id,
            run_id,
            task_id,
            action,
            decided_by: Some(decided_by),
            reason: Some(reason.to_string()),
        };
        let decision = decision.to_string();
        let reason = reason.to_string();
        let principal_instance_id = principal_instance_id.to_string();
        self.append_and_apply(&event, Some(task_id), None, Some(run_id), move |tx| {
            let now = Timestamp::now();
            // Ownership is arbitrated here, inside the guarded transaction,
            // not by a caller-side snapshot read: the database actor
            // interleaves whole `run_domain_op` closures, so a
            // `reconcile_ownership` rebind can commit between a caller's
            // snapshot read and this write. Only a re-read from inside this
            // same transaction can observe that rebind (R71).
            let owner: Option<String> = tx
                .query_row(
                    "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                    [task_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            match owner {
                Some(owner) if owner == principal_instance_id => {}
                Some(_) => {
                    return Err(DomainError::NotOwner {
                        task_id: task_id.to_string(),
                        instance_id: principal_instance_id,
                    });
                }
                None => {
                    return Err(DomainError::NotFound {
                        kind: "task",
                        id: task_id.to_string(),
                    });
                }
            }
            let affected = tx.execute(
                "UPDATE approvals SET decision = ?1, decided_at = ?2, decided_by = ?3, reason = ?4
                 WHERE approval_id = ?5 AND decision IS NULL",
                rusqlite::params![
                    decision,
                    now.as_str(),
                    // The bare wire token, never the JSON-quoted form (R34).
                    decided_by.as_str(),
                    reason,
                    approval_id.to_string(),
                ],
            )?;
            if affected == 0 {
                // Either a concurrent decision won the row, or the approval
                // does not exist. Classify from inside the same transaction;
                // nothing else can have changed it since.
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT decision FROM approvals
                         WHERE approval_id = ?1 AND decision IS NOT NULL",
                        [approval_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                return Err(match existing {
                    Some(existing) => DomainError::AlreadyResolved {
                        kind: "approval",
                        id: approval_id.to_string(),
                        existing,
                    },
                    None => DomainError::NotFound {
                        kind: "approval",
                        id: approval_id.to_string(),
                    },
                });
            }
            let state: String = tx.query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            let parsed = RunState::try_from(state.as_str()).map_err(|_| DomainError::NotFound {
                kind: "run-state",
                id: state.clone(),
            })?;
            if parsed.is_terminal() {
                return Err(DomainError::RunSettled {
                    run_id: run_id.to_string(),
                });
            }
            Ok(())
        })
    }

    /// Proposes a leader plan for a run: inserts a `plans` row in the
    /// `proposed` state and appends a `PlanProposed` event. The plan is
    /// keyed 1:1 by `run_id` -- at most one plan row may exist per run,
    /// and re-proposing one that already exists (proposed or decided) is
    /// refused as [`DomainError::AlreadyResolved`] so a stale second
    /// proposal cannot clobber a live decision. `task_id`/`worker_id` are
    /// read from the run row (the daemon does not own routing; it only
    /// reflects the run it supervises) and carried on the event so a
    /// monitor replaying the journal can reconstruct the subtask graph.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if no such run exists, or
    /// [`DomainError::AlreadyResolved`] if a plan already exists for it.
    pub fn propose_plan(
        &mut self,
        run_id: RunId,
        owner_client_instance_id: &str,
        task_text: &str,
        plan: &PlanSpec,
    ) -> Result<Committed, DomainError> {
        let run_id_str = run_id.to_string();
        // Reflect the run the plan is for; the daemon does not create or
        // edit OMP's task graph (invariant 6).
        let (task_id_str, worker_id_str): (String, String) = self
            .conn
            .query_row(
                "SELECT task_id, worker_id FROM runs WHERE run_id = ?1",
                [run_id_str.clone()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(DomainError::NotFound {
                kind: "run",
                id: run_id_str.clone(),
            })?;
        let task_id = TaskId::parse(&task_id_str).map_err(|_| DomainError::NotFound {
            kind: "task",
            id: task_id_str.clone(),
        })?;
        let worker_id = WorkerId::parse(&worker_id_str).map_err(|_| DomainError::NotFound {
            kind: "worker",
            id: worker_id_str.clone(),
        })?;

        // Refuse to overwrite an existing plan for this run (proposed or
        // already decided) -- see doc above.
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM plans WHERE run_id = ?1",
                [run_id_str.clone()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(status) = existing {
            return Err(DomainError::AlreadyResolved {
                kind: "plan",
                id: run_id_str,
                existing: status,
            });
        }

        let subtasks_json = serde_json::to_string(&plan.subtasks).map_err(|err| {
            DomainError::Sqlite(rusqlite::Error::ToSqlConversionFailure(Box::new(err)))
        })?;
        let now = Timestamp::now();
        let status = "proposed".to_string();
        let project_id = self.project_id.to_string();
        let event = RuntimeEvent::PlanProposed {
            run_id,
            task_id,
            worker_id,
            plan: plan.clone(),
        };
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            move |tx| {
                tx.execute(
                    "INSERT INTO plans (plan_id, project_id, run_id, task_id, worker_id, \
                     owner_client_instance_id, task_text, subtasks_json, status, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        run_id_str,
                        project_id,
                        run_id_str,
                        task_id_str,
                        worker_id_str,
                        owner_client_instance_id,
                        task_text,
                        subtasks_json,
                        status,
                        now.as_str(),
                    ],
                )?;
                Ok(())
            },
        )
    }

    /// Decides a previously proposed plan for a run: guards inside the
    /// writing transaction (re-read `status` and `owner_client_instance_id`
    /// from `plans`, mirroring [`Self::decide_approval`'s] race-safe owner
    /// check -- R71), journals a `PlanDecided` event, and writes the
    /// decision onto the row. The daemon enforces nothing about *routing*;
    /// it only persists the leader's decision.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if no plan exists for the run,
    /// [`DomainError::NotOwner`] if `principal_instance_id` does not own
    /// the plan's task, or [`DomainError::AlreadyResolved`] if the plan has
    /// already been decided.
    pub fn decide_plan(
        &mut self,
        run_id: RunId,
        principal_instance_id: &str,
        approved: bool,
        reason: Option<&str>,
    ) -> Result<Committed, DomainError> {
        let run_id_str = run_id.to_string();
        // The run row is stable for a run, so reading task_id/worker_id here
        // (outside the guarded transaction) is safe; only the `plans` row's
        // owner/status guard must live inside the writing transaction (R71).
        let (task_id_str, worker_id_str): (String, String) = self
            .conn
            .query_row(
                "SELECT task_id, worker_id FROM runs WHERE run_id = ?1",
                [run_id_str.clone()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(DomainError::NotFound {
                kind: "run",
                id: run_id_str.clone(),
            })?;
        let task_id = TaskId::parse(&task_id_str).map_err(|_| DomainError::NotFound {
            kind: "task",
            id: task_id_str.clone(),
        })?;
        let worker_id = WorkerId::parse(&worker_id_str).map_err(|_| DomainError::NotFound {
            kind: "worker",
            id: worker_id_str.clone(),
        })?;

        let now = Timestamp::now();
        let decision = if approved { "approved" } else { "rejected" };
        let reason_str = reason.map(str::to_string);
        let principal = principal_instance_id.to_string();
        let event = RuntimeEvent::PlanDecided {
            run_id,
            task_id,
            worker_id,
            approved,
            reason: reason.map(str::to_string),
        };
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            move |tx| {
                // Race-safe owner + status re-read inside the writing
                // transaction (R71): a `reconcile/omp` rebind cannot land
                // between a caller snapshot and this write because the
                // database actor serializes whole `run_domain_op` closures.
                let row: Option<(String, String)> = tx
                    .query_row(
                        "SELECT owner_client_instance_id, status FROM plans WHERE run_id = ?1",
                        [run_id_str.clone()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let (owner, status) = match row {
                    Some(row) => row,
                    None => {
                        return Err(DomainError::NotFound {
                            kind: "plan",
                            id: run_id_str.clone(),
                        });
                    }
                };
                if owner != principal {
                    return Err(DomainError::NotOwner {
                        task_id: owner,
                        instance_id: principal.clone(),
                    });
                }
                if status != "proposed" {
                    return Err(DomainError::AlreadyResolved {
                        kind: "plan",
                        id: run_id_str.clone(),
                        existing: status,
                    });
                }
                tx.execute(
                    "UPDATE plans SET status = ?1, decided_at = ?2, decided_reason = ?3 \
                     WHERE run_id = ?4 AND status = 'proposed'",
                    rusqlite::params![decision, now.as_str(), reason_str, run_id_str,],
                )?;
                Ok(())
            },
        )
    }

    /// Returns the most recently proposed plan for a run and its decision,
    /// if any (`plan/get`, ADR-0024 open read). `plan: None` means no plan
    /// has been proposed yet; `approved: None` means a plan exists but has
    /// not yet been decided.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if no such run exists.
    pub fn get_plan(&self, run_id: RunId) -> Result<PlanGetResult, DomainError> {
        let run_id_str = run_id.to_string();
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM runs WHERE run_id = ?1",
                [run_id_str.clone()],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(DomainError::NotFound {
                kind: "run",
                id: run_id_str,
            });
        }
        let row: Option<(String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT subtasks_json, status FROM plans WHERE run_id = ?1 \
                 ORDER BY created_at DESC LIMIT 1",
                [run_id_str],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok(PlanGetResult {
                run_id,
                plan: None,
                approved: None,
            }),
            Some((subtasks_json, status)) => {
                let subtasks: Vec<SubtaskSpec> =
                    serde_json::from_str(&subtasks_json).map_err(|err| {
                        DomainError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(err),
                        ))
                    })?;
                Ok(PlanGetResult {
                    run_id,
                    plan: Some(PlanSpec { subtasks }),
                    // `None` means a plan exists but has not yet been
                    // decided; only a decided row yields `Some(..)`.
                    approved: match status.as_deref() {
                        Some("approved") => Some(true),
                        Some("rejected") => Some(false),
                        _ => None,
                    },
                })
            }
        }
    }

    /// Records a mid-run policy violation: inserts the [`policy_violations`]
    /// row and appends a `PolicyViolationRecorded` event, then reports
    /// whether the run was already actioned (quarantined or terminal)
    /// *before* this commit. Does not itself touch `Run.flags` -- callers
    /// apply the quarantine flag via [`DomainRepository::set_run_flag`] as
    /// a separate commit, so existing `RunFlagsChanged` consumers see it
    /// without new code.
    ///
    /// The `already_actioned` read (`self.conn.query_row`, immediately
    /// below) executes *before* [`Self::append_and_apply`] opens its SQL
    /// transaction, not inside it -- the same closure-granularity pattern
    /// [`Self::set_run_flag`] uses (R73), for the identical reason: this
    /// method's event is built from plain parameters, not from the read
    /// result, so nothing forces the read into the transaction, but
    /// nothing needs to. What guards it is not a transaction but
    /// [`crate::db::DatabaseHandle`]'s single-owner actor thread, which
    /// runs one whole `run_domain_op` closure to completion before
    /// starting the next: no concurrent `decide("release")` call can
    /// commit its [`Self::resolve_policy_violation`] or its quarantine
    /// clear in between this read and the `INSERT` a few lines below,
    /// because both live inside this one closure. Before this fix, that
    /// read was a separate, earlier `run_domain_op` round trip
    /// (`ViolationService::load_run_state_and_flags`), one full command
    /// apart from the write that mattered -- exactly the gap a concurrent
    /// release's quarantine-clear could land in and go unnoticed (R75).
    ///
    /// `code` is the machine-readable violation code (`nested_worker_denied`
    /// or `cost_ceiling_exceeded`). `vendor_child_id`/`vendor_parent_ref` are
    /// `None` for violations with no vendor child, such as a cost ceiling --
    /// an empty string there would be a lie rather than an absence.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if `run_id` does not exist.
    #[allow(clippy::too_many_arguments)]
    pub fn record_policy_violation(
        &mut self,
        violation_id: PolicyViolationId,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        code: &str,
        observed_event_sequence: u64,
        policy_fingerprint: &str,
        vendor_child_id: Option<&str>,
        vendor_parent_ref: Option<&str>,
        action: &str,
    ) -> Result<PolicyViolationRecordOutcome, DomainError> {
        // Not `Self::read_run_flags`: that rebuilds the full six-column
        // `RunFlags` struct, and this check only ever needs
        // `policy_quarantined` plus `state` -- two columns, not six.
        let (quarantined, state): (i64, String) = self
            .conn
            .query_row(
                "SELECT flags_policy_quarantined, state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| DomainError::NotFound {
                kind: "run",
                id: run_id.to_string(),
            })?;
        let parsed_state =
            RunState::try_from(state.as_str()).map_err(|_| DomainError::NotFound {
                kind: "run-state",
                id: state.clone(),
            })?;
        let already_actioned = quarantined != 0 || parsed_state.is_terminal();

        let event = RuntimeEvent::PolicyViolationRecorded {
            kind: RuntimeEventKind::PolicyViolationRecorded {
                violation_id,
                code: code.to_string(),
                observed_event_sequence,
                policy_fingerprint: policy_fingerprint.to_string(),
                vendor_child_id: vendor_child_id.map(str::to_string),
                vendor_parent_ref: vendor_parent_ref.map(str::to_string),
                action: action.to_string(),
            },
            run_id,
            task_id,
            worker_id,
        };
        let vendor_child_id = vendor_child_id.map(str::to_string);
        let vendor_parent_ref = vendor_parent_ref.map(str::to_string);
        let action = action.to_string();
        let committed = self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            move |tx| {
                let now = Timestamp::now();
                tx.execute(
                    "INSERT INTO policy_violations (violation_id, run_id, task_id, worker_id,
                       vendor_child_id, vendor_parent_ref, action, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![
                        violation_id.to_string(),
                        run_id.to_string(),
                        task_id.to_string(),
                        worker_id.to_string(),
                        vendor_child_id,
                        vendor_parent_ref,
                        action,
                        now.as_str(),
                    ],
                )?;
                Ok(())
            },
        )?;
        Ok(PolicyViolationRecordOutcome {
            committed,
            already_actioned,
        })
    }

    /// Looks up a policy violation's `run_id`/`task_id`/`worker_id`, for
    /// [`crate::policy::ViolationService`] to thread through to
    /// [`DomainRepository::resolve_policy_violation`] and its follow-up
    /// commits. It does not carry ownership, `resolution`, or the run's
    /// state: gating on those -- including ownership (R72) -- happens
    /// inside [`DomainRepository::resolve_policy_violation`], where it
    /// cannot race the write.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if `violation_id` does not exist.
    pub fn policy_violation_snapshot(
        &mut self,
        violation_id: PolicyViolationId,
    ) -> Result<PolicyViolationSnapshot, DomainError> {
        self.conn
            .query_row(
                "SELECT run_id, task_id, worker_id FROM policy_violations WHERE violation_id = ?1",
                [violation_id.to_string()],
                |row| {
                    Ok(PolicyViolationSnapshot {
                        run_id: row.get::<_, String>(0)?,
                        task_id: row.get::<_, String>(1)?,
                        worker_id: row.get::<_, String>(2)?,
                    })
                },
            )
            .map_err(|_| DomainError::NotFound {
                kind: "policy-violation",
                id: violation_id.to_string(),
            })
    }

    /// Resolves a previously-recorded policy violation: records
    /// `resolution`/`resolved_by` and appends a `PolicyViolationDecided`
    /// event. Does not touch `Run.flags` or run state -- callers apply
    /// those via [`DomainRepository::set_run_flag`]/
    /// [`DomainRepository::release_quarantine`]/
    /// [`DomainRepository::transition_run`] as separate commits.
    ///
    /// This is the **only** authority on whether a violation may be
    /// resolved. The database actor interleaves whole `run_domain_op`
    /// closures, never a service's sequence of round trips, so any
    /// caller-side pre-check is advisory only (R54, R72): ownership,
    /// conflict, and the terminal-run state are all re-checked from inside
    /// this one guarded transaction, never from a snapshot a caller read
    /// earlier. `principal_instance_id` is checked against
    /// `tasks.owner_client_instance_id` first, before the `UPDATE` guarded
    /// by `resolution IS NULL` -- a `reconcile/omp` ownership rebind that
    /// commits between a caller's snapshot read and this write must
    /// invalidate the stale caller, and it can only do that if the check
    /// happens here, not in `ViolationService::decide` (mirrors R71's
    /// `decide_approval`). The `UPDATE` deliberately precedes the
    /// terminal-run guard so an already-decided violation reports
    /// [`DomainError::AlreadyResolved`] even when its run has also
    /// settled; an `Err` returned here discards the appended event
    /// together with the rejected write (the transaction rolls back as a
    /// whole).
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] if no such violation or task
    /// exists, [`DomainError::NotOwner`] if `principal_instance_id` does
    /// not own the violation's task, [`DomainError::AlreadyResolved`] if a
    /// resolution is already on record, or [`DomainError::RunSettled`] if
    /// `resolution` is `"release"` and the run has reached a terminal
    /// state.
    pub fn resolve_policy_violation(
        &mut self,
        violation_id: PolicyViolationId,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        principal_instance_id: &str,
        resolution: &str,
    ) -> Result<Committed, DomainError> {
        let event = RuntimeEvent::PolicyViolationDecided {
            kind: RuntimeEventKind::PolicyViolationDecided {
                violation_id,
                resolution: resolution.to_string(),
                // The resolver is by definition the authorized principal:
                // the guarded write below refuses anyone else.
                resolved_by: principal_instance_id.to_string(),
            },
            run_id,
            task_id,
            worker_id,
        };
        let resolution = resolution.to_string();
        let principal_instance_id = principal_instance_id.to_string();
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            move |tx| {
                let now = Timestamp::now();
                // Ownership is arbitrated here, inside the guarded
                // transaction, not by a caller-side snapshot read: the
                // database actor interleaves whole `run_domain_op`
                // closures, so a `reconcile_ownership` rebind can commit
                // between a caller's snapshot read and this write. Only a
                // re-read from inside this same transaction can observe
                // that rebind (R72, mirroring R71's `decide_approval`).
                let owner: Option<String> = tx
                    .query_row(
                        "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                        [task_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                match owner {
                    Some(owner) if owner == principal_instance_id => {}
                    Some(_) => {
                        return Err(DomainError::NotOwner {
                            task_id: task_id.to_string(),
                            instance_id: principal_instance_id,
                        });
                    }
                    None => {
                        return Err(DomainError::NotFound {
                            kind: "task",
                            id: task_id.to_string(),
                        });
                    }
                }
                let affected = tx.execute(
                    "UPDATE policy_violations SET resolution = ?1, resolved_by = ?2, resolved_at = ?3
                     WHERE violation_id = ?4 AND resolution IS NULL",
                    rusqlite::params![resolution, principal_instance_id, now.as_str(), violation_id.to_string()],
                )?;
                if affected == 0 {
                    // Either a concurrent decision won the row, or the
                    // violation does not exist. Classify from inside the same
                    // transaction; nothing else can have changed it since.
                    let existing: Option<String> = tx
                        .query_row(
                            "SELECT resolution FROM policy_violations
                             WHERE violation_id = ?1 AND resolution IS NOT NULL",
                            [violation_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()?;
                    return Err(match existing {
                        Some(existing) => DomainError::AlreadyResolved {
                            kind: "policy-violation",
                            id: violation_id.to_string(),
                            existing,
                        },
                        None => DomainError::NotFound {
                            kind: "policy-violation",
                            id: violation_id.to_string(),
                        },
                    });
                }
                if resolution == "release" {
                    let state: String = tx.query_row(
                        "SELECT state FROM runs WHERE run_id = ?1",
                        [run_id.to_string()],
                        |row| row.get(0),
                    )?;
                    let parsed = RunState::try_from(state.as_str()).map_err(|_| {
                        DomainError::NotFound {
                            kind: "run-state",
                            id: state.clone(),
                        }
                    })?;
                    if parsed.is_terminal() {
                        return Err(DomainError::RunSettled {
                            run_id: run_id.to_string(),
                        });
                    }
                }
                Ok(())
            },
        )
    }

    /// Rebinds a task's owning OMP client instance during reconciliation.
    /// The revision match is enforced by the write itself (`AND revision =
    /// ?`): a reconcile whose presented revision is stale at write time --
    /// e.g. an upsert advanced the task after the caller read its
    /// correlation -- is refused, classified in the same transaction as
    /// [`DomainError::RevisionMismatch`] (R74). The stored revision is NOT
    /// consumed: reclaim stays idempotent (a retried or repeated reconcile
    /// presenting the same revision succeeds, last reconciler wins), and a
    /// usurped owner is still refused at decision time by the in-tx
    /// ownership arbitration in `decide_approval`/`resolve_policy_violation`
    /// (R71/R72). Emits a `ReconcileOwnershipChanged` event carrying
    /// old/new owner ids and the (unchanged) stored revision.
    pub fn reconcile_ownership(
        &mut self,
        task_id: crew_protocol::TaskId,
        new_owner: &str,
        revision: u64,
    ) -> Result<Committed, DomainError> {
        // Read for the event payload only. This runs before
        // `append_and_apply` opens its transaction because the event must be
        // fully built first; it is safe because the database actor executes
        // whole `run_domain_op` closures serially, so nothing can rebind the
        // task between this read and the guarded write below.
        let old_owner: String = self
            .conn
            .query_row(
                "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| DomainError::NotFound {
                kind: "task",
                id: task_id.to_string(),
            })?;

        let event = RuntimeEvent::ReconcileEvent {
            task_id,
            old_owner_client_instance_id: old_owner,
            new_owner_client_instance_id: new_owner.to_string(),
            revision,
        };
        let new_owner = new_owner.to_string();
        self.append_and_apply(&event, Some(task_id), None, None, move |tx| {
            let now = Timestamp::now();
            let affected = tx.execute(
                "UPDATE tasks SET owner_client_instance_id = ?1, updated_at = ?3
                 WHERE task_id = ?4 AND revision = ?2",
                rusqlite::params![new_owner, revision, now.as_str(), task_id.to_string()],
            )?;
            if affected == 0 {
                // The guarded update declined: the stored revision moved
                // since the caller read its correlation (or the task
                // vanished -- kept as defense in depth even though the
                // old-owner read above already NotFounds a missing task
                // within this same serially-executed closure). Classify
                // inside the same transaction.
                let stored: Option<u64> = tx
                    .query_row(
                        "SELECT revision FROM tasks WHERE task_id = ?1",
                        [task_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                return Err(match stored {
                    Some(stored) => DomainError::RevisionMismatch {
                        task_id: task_id.to_string(),
                        presented: revision,
                        stored,
                    },
                    None => DomainError::NotFound {
                        kind: "task",
                        id: task_id.to_string(),
                    },
                });
            }
            Ok(())
        })
    }

    /// Records a child-worker request: appends `ChildWorkerRequested` and
    /// transitions the requesting run `working -> waitingPeer`. Never
    /// creates a task or worker itself -- OMP answers through
    /// [`DomainRepository::decide_child`].
    pub fn request_child(
        &mut self,
        parent_run_id: RunId,
        reason: &str,
    ) -> Result<Committed, DomainError> {
        let (from_str, task_id_str, worker_id_str): (String, String, String) = self
            .conn
            .query_row(
                "SELECT state, task_id, worker_id FROM runs WHERE run_id = ?1",
                [parent_run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|_| DomainError::NotFound {
                kind: "run",
                id: parent_run_id.to_string(),
            })?;
        let from = RunState::try_from(from_str.as_str()).map_err(|_| DomainError::NotFound {
            kind: "run-state",
            id: from_str.clone(),
        })?;
        let waiting_peer = RunState::try_from("waitingPeer").expect("waitingPeer is valid");
        check_transition(&parent_run_id.to_string(), &from, &waiting_peer)?;

        let task_id = TaskId::parse(&task_id_str).map_err(|_| DomainError::NotFound {
            kind: "task",
            id: task_id_str.clone(),
        })?;
        let worker_id = WorkerId::parse(&worker_id_str).map_err(|_| DomainError::NotFound {
            kind: "worker",
            id: worker_id_str.clone(),
        })?;

        let event = RuntimeEvent::ChildEvent {
            kind: RuntimeEventKind::ChildWorkerRequested,
            parent_run_id,
            child_task_id: None,
            child_worker_id: None,
            child_run_id: None,
            reason: Some(reason.to_string()),
        };
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(parent_run_id),
            move |tx| {
                // Re-read and re-check inside this guarded write (R94):
                // the plain-connection read above fixes error-code
                // precedence for the ordinary case, but only this read
                // can observe a transition the database actor interleaved
                // behind it -- without it, a racing settle would be
                // silently overwritten with `waitingPeer`.
                let state: String = tx.query_row(
                    "SELECT state FROM runs WHERE run_id = ?1",
                    [parent_run_id.to_string()],
                    |row| row.get(0),
                )?;
                let from =
                    RunState::try_from(state.as_str()).map_err(|_| DomainError::NotFound {
                        kind: "run-state",
                        id: state.clone(),
                    })?;
                let waiting_peer = RunState::try_from("waitingPeer").expect("waitingPeer is valid");
                check_transition(&parent_run_id.to_string(), &from, &waiting_peer)?;
                tx.execute(
                    "UPDATE runs SET state = 'waitingPeer' WHERE run_id = ?1",
                    rusqlite::params![parent_run_id.to_string()],
                )?;
                Ok(())
            },
        )
    }

    /// Records OMP's decision on a prior child-worker request and returns
    /// the parent run to `working`. Acceptance carries the OMP-created
    /// child ids; denial carries a reason. The runtime owns both
    /// transitions after the correlated decision commits. A refusal on
    /// either ground appends nothing.
    ///
    /// Authorization precedes validity, the same way and for the same
    /// reason as [`Self::transition_run`] (R77): a non-owning caller sees
    /// [`DomainError::NotOwner`], never `ILLEGAL_TRANSITION`, regardless
    /// of whether the parent run happens to be in a state that could
    /// legally return to `working`. `principal_instance_id` is checked
    /// twice for this precedence: once as a plain `self.conn` read,
    /// immediately before [`check_transition`]; and again -- the
    /// authoritative, race-safe check -- re-read from `tasks` inside
    /// this guarded write immediately before the mutating `UPDATE`, the
    /// same way [`Self::submit_run`] and [`Self::transition_run`] do.
    /// Only the second read runs inside the `run_domain_op` closure the
    /// database actor executes as one indivisible unit, so only it can
    /// observe a concurrent `reconcile/omp` rebind; the first read
    /// exists solely to fix the error-code precedence for the ordinary
    /// case. `None` is not used by any current caller:
    /// `coordination/child/decide` is `ompExtension`-only
    /// (`crate::ipc::ClientPrincipal::allowed_methods`), so every call
    /// site has a principal to arbitrate against.
    ///
    /// # Errors
    /// Returns [`DomainError::NotOwner`] if `principal_instance_id` is
    /// `Some` and does not own the parent run's task, or
    /// [`DomainError::NotFound`] if that task does not exist, in addition
    /// to the transition errors documented above.
    pub fn decide_child(
        &mut self,
        parent_run_id: RunId,
        decision: ChildDecision,
        principal_instance_id: Option<&str>,
    ) -> Result<Committed, DomainError> {
        let (from_str, task_id_str, worker_id_str): (String, String, String) = self
            .conn
            .query_row(
                "SELECT state, task_id, worker_id FROM runs WHERE run_id = ?1",
                [parent_run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|_| DomainError::NotFound {
                kind: "run",
                id: parent_run_id.to_string(),
            })?;

        let task_id = TaskId::parse(&task_id_str).map_err(|_| DomainError::NotFound {
            kind: "task",
            id: task_id_str.clone(),
        })?;

        // Authorization precedes validity (R77) -- see the doc comment
        // above. This is a plain snapshot read, not inside the
        // closure-granular boundary `run_domain_op` gives the guarded
        // write below; the race-safe re-check that actually protects
        // the mutation still happens there, immediately before the
        // `UPDATE`.
        if let Some(principal_instance_id) = principal_instance_id {
            let owner: Option<String> = self
                .conn
                .query_row(
                    "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                    [task_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            match owner {
                Some(owner) if owner == principal_instance_id => {}
                Some(_) => {
                    return Err(DomainError::NotOwner {
                        task_id: task_id.to_string(),
                        instance_id: principal_instance_id.to_string(),
                    });
                }
                None => {
                    return Err(DomainError::NotFound {
                        kind: "task",
                        id: task_id.to_string(),
                    });
                }
            }
        }

        let from = RunState::try_from(from_str.as_str()).map_err(|_| DomainError::NotFound {
            kind: "run-state",
            id: from_str.clone(),
        })?;
        let working = RunState::try_from("working").expect("working is valid");
        check_transition(&parent_run_id.to_string(), &from, &working)?;

        let worker_id = WorkerId::parse(&worker_id_str).map_err(|_| DomainError::NotFound {
            kind: "worker",
            id: worker_id_str.clone(),
        })?;

        let (kind, child_task_id, child_worker_id, child_run_id, reason) = match decision {
            ChildDecision::Accept {
                child_task_id,
                child_worker_id,
                child_run_id,
            } => (
                RuntimeEventKind::ChildWorkerAccepted,
                Some(child_task_id),
                Some(child_worker_id),
                Some(child_run_id),
                None,
            ),
            ChildDecision::Deny { reason } => (
                RuntimeEventKind::ChildWorkerRequestDenied,
                None,
                None,
                None,
                Some(reason),
            ),
        };
        let event = RuntimeEvent::ChildEvent {
            kind,
            parent_run_id,
            child_task_id,
            child_worker_id,
            child_run_id,
            reason,
        };
        let principal_instance_id = principal_instance_id.map(str::to_string);
        self.append_and_apply(
            &event,
            Some(task_id),
            Some(worker_id),
            Some(parent_run_id),
            move |tx| {
                if let Some(principal_instance_id) = principal_instance_id {
                    let owner: Option<String> = tx
                        .query_row(
                            "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                            [task_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match owner {
                        Some(owner) if owner == principal_instance_id => {}
                        Some(_) => {
                            return Err(DomainError::NotOwner {
                                task_id: task_id.to_string(),
                                instance_id: principal_instance_id,
                            });
                        }
                        None => {
                            return Err(DomainError::NotFound {
                                kind: "task",
                                id: task_id.to_string(),
                            });
                        }
                    }
                }
                tx.execute(
                    "UPDATE runs SET state = 'working' WHERE run_id = ?1",
                    rusqlite::params![parent_run_id.to_string()],
                )?;
                Ok(())
            },
        )
    }

    /// Appends a normalized adapter telemetry event (visible messages,
    /// tool lifecycle, usage, protocol health, nested-worker observation,
    /// ...) to the durable journal, correlated to
    /// `task_id`/`worker_id`/`run_id`. Unlike the mutations above, this
    /// never itself applies a run-state transition -- adapters call
    /// `transition_run` directly for that, through the same seam
    /// `FakeRunDriver` already uses. The one exception is
    /// `AdapterVendorSessionEvent`, which also records the run's vendor
    /// session id in the same transaction.
    ///
    /// `transcript_cursor` (WP12) is `Some(json)` when the caller is a TUI
    /// adapter's transcript tailer reporting the durable position reached
    /// by the batch this event belongs to (`crate::adapter::tui::Cursor`,
    /// serialized by the sink); it is written to `runs.transcript_cursor`
    /// in this same transaction, so a crash between the event commit and a
    /// separate cursor write can never leave the journal and the resume
    /// position disagreeing. `None` (a non-TUI adapter, or a TUI batch
    /// that produced no cursor-bearing event) leaves the column exactly as
    /// it was -- never cleared to NULL, which would otherwise force a full
    /// re-tail from the transcript's start on the next resume.
    ///
    /// This layer takes the already-serialized JSON text, not
    /// `crate::adapter::tui::Cursor` itself -- it stores the column's
    /// declared `TEXT` type verbatim and has no compile-time dependency on
    /// that adapter-local type. That tolerance is additive-only: `Cursor`
    /// deliberately stays forward-compatible with a *new field* a future
    /// version might add (see its own doc comment), but this layer cannot
    /// paper over a *renamed or removed* field or a changed type -- that
    /// would still fail whatever later reads the column back as `Cursor`,
    /// same as any other persisted JSON shape.
    pub fn record_adapter_event(
        &mut self,
        event: &RuntimeEvent,
        task_id: TaskId,
        worker_id: WorkerId,
        run_id: RunId,
        transcript_cursor: Option<String>,
    ) -> Result<Committed, DomainError> {
        let vendor_session_id = match event {
            RuntimeEvent::AdapterVendorSessionEvent {
                vendor_session_id, ..
            } => Some(vendor_session_id.clone()),
            _ => None,
        };
        // WP20: a journaled WorkerQuestion auto-opens its escalation row in
        // the SAME transaction -- the question event is the durable fact;
        // the open row is the projection a later `message/send {kind:
        // "answer"}` resolves. One open question-escalation per run: the
        // insert is skipped while one is still undecided (a worker that
        // asks twice before anyone answers has still asked once).
        let question = match event {
            RuntimeEvent::WorkerQuestion { question, .. } => question.clone(),
            _ => None,
        };
        self.append_and_apply(
            event,
            Some(task_id),
            Some(worker_id),
            Some(run_id),
            move |tx| {
                if let Some(vendor_session_id) = vendor_session_id {
                    tx.execute(
                        "UPDATE runs SET vendor_session_id = ?1 WHERE run_id = ?2",
                        rusqlite::params![vendor_session_id, run_id.to_string()],
                    )?;
                }
                if let Some(cursor) = transcript_cursor {
                    tx.execute(
                        "UPDATE runs SET transcript_cursor = ?1 WHERE run_id = ?2",
                        rusqlite::params![cursor, run_id.to_string()],
                    )?;
                }
                if let Some(question) = question {
                    let open: i64 = tx
                        .query_row(
                            "SELECT COUNT(*) FROM escalations \
                             WHERE run_id = ?1 AND kind = 'question' AND decided_at IS NULL",
                            [run_id.to_string()],
                            |row| row.get(0),
                        )?;
                    if open == 0 {
                        tx.execute(
                            "INSERT INTO escalations (escalation_id, run_id, kind, question, created_at) \
                             VALUES (?1, ?2, 'question', ?3, ?4)",
                            rusqlite::params![
                                crew_protocol::EscalationId::new().to_string(),
                                run_id.to_string(),
                                question,
                                Timestamp::now().as_str(),
                            ],
                        )?;
                    }
                }
                Ok(())
            },
        )
    }
}

/// Maps a run state to the event kind that records entering it.
fn kind_for_state(state: &RunState) -> RuntimeEventKind {
    match state.to_string().as_str() {
        "queued" => RuntimeEventKind::RunQueued,
        "starting" => RuntimeEventKind::RunStarting,
        "working" => RuntimeEventKind::RunWorking,
        "waitingUser" => RuntimeEventKind::RunWaitingUser,
        "waitingPeer" => RuntimeEventKind::RunWaitingPeer,
        "paused" => RuntimeEventKind::RunPaused,
        "succeeded" => RuntimeEventKind::RunSucceeded,
        "failed" => RuntimeEventKind::RunFailed,
        "cancelled" => RuntimeEventKind::RunCancelled,
        "lost" => RuntimeEventKind::RunLost,
        _ => RuntimeEventKind::RunWorking,
    }
}

/// The canonical wire string for a delivery state.
fn delivery_state_str(state: &DeliveryState) -> &'static str {
    match state {
        DeliveryState::Recorded => "recorded",
        DeliveryState::Sent => "sent",
        DeliveryState::Acknowledged => "acknowledged",
        DeliveryState::Failed => "failed",
        DeliveryState::Unknown => "unknown",
    }
}

/// The canonical wire string for a message kind.
fn message_kind_str(kind: &crew_protocol::MessageKind) -> &'static str {
    use crew_protocol::MessageKind;
    match kind {
        MessageKind::Assign => "assign",
        MessageKind::Steer => "steer",
        MessageKind::FollowUp => "followUp",
        MessageKind::Question => "question",
        MessageKind::Answer => "answer",
        MessageKind::PeerMessage => "peerMessage",
        MessageKind::ApprovalDecision => "approvalDecision",
        MessageKind::Cancel => "cancel",
        MessageKind::Shutdown => "shutdown",
    }
}

/// Whether a message kind originates from (or acts for) the leader and
/// therefore consumes one of the run's budgeted worker turns (WP19). The
/// directives that make a worker take another turn -- assignment, steering,
/// follow-ups, leader questions, approval decisions resuming a blocked turn
/// -- consume; worker-side kinds (`answer`, `peerMessage`) and the
/// run-terminating kinds (`cancel`, `shutdown`, which end work rather than
/// request more) do not.
fn is_turn_consuming(kind: &crew_protocol::MessageKind) -> bool {
    use crew_protocol::MessageKind;
    matches!(
        kind,
        MessageKind::Assign
            | MessageKind::Steer
            | MessageKind::FollowUp
            | MessageKind::Question
            | MessageKind::ApprovalDecision
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_protocol::{PolicyViolationId, ProjectId, RunId, TaskId, WorkerId, WorkerProfileRef};
    use rusqlite::Connection;

    /// Opens an in-memory database migrated by the *production* migration
    /// list, never a hand-copied schema -- a projection column added by a
    /// migration is therefore visible to these tests without a second
    /// place to update.
    fn open_test_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        crate::db::migrations::migrate(&mut conn).expect("schema");
        conn
    }

    fn seed_worker(conn: &mut Connection, project_id: ProjectId) -> (TaskId, WorkerId) {
        let mut repo = DomainRepository::new(conn, project_id);
        let task_id = TaskId::new();
        repo.upsert_task(
            task_id,
            &TaskRef {
                owner_client_instance_id: "omp-1".into(),
                revision: 1,
            },
        )
        .expect("upsert task");
        let worker_id = WorkerId::new();
        let worker = Worker {
            worker_id,
            profile_ref: WorkerProfileRef {
                id: worker_id,
                fingerprint: "sha256:fake".into(),
                adapter: "fake".into(),
                model: "test".into(),
                permission_envelope: serde_json::json!({}),
            },
            parent_worker_id: None,
            created_at: Timestamp::now(),
        };
        repo.create_worker(&worker).expect("create worker");
        (task_id, worker_id)
    }

    /// Exercises the actual `DomainRepository` API (not raw SQL): submits a
    /// run through `submit_run`, then transitions it through the repository,
    /// proving each command commits one event + one projection update in a
    /// single transaction.
    #[test]
    fn submit_run_and_transition_commit_event_and_projection_together() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (task_id, worker_id) = seed_worker(&mut conn, project_id);

        let run_id = RunId::new();
        let run = Run {
            run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").unwrap(),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };

        let mut repo = DomainRepository::new(&mut conn, project_id);
        let committed = repo
            .submit_run(&run, None, None)
            .expect("submit_run commits");
        assert_eq!(
            committed.sequence, 3,
            "task upsert (1), worker create (2), run submit (3)"
        );

        let working = RunState::try_from("starting").unwrap();
        let committed2 = repo
            .transition_run(run_id, &working, None)
            .expect("transition_run commits");
        assert_eq!(committed2.sequence, 4);

        // The projection reflects the transition.
        let state: String = conn
            .query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "starting");

        // The event journal has exactly 4 durable rows (task, worker, run, transition).
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(event_count, 4);
    }

    /// WP12: `record_adapter_event` given a cursor updates
    /// `runs.transcript_cursor` in the same transaction as the event
    /// insert -- the idempotency anchor a crashed daemon reads to re-tail
    /// without duplicating already-journaled events.
    #[test]
    fn record_adapter_event_with_cursor_updates_transcript_cursor_atomically() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (task_id, worker_id) = seed_worker(&mut conn, project_id);
        let run_id = RunId::new();
        let run = Run {
            run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").unwrap(),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };
        let mut repo = DomainRepository::new(&mut conn, project_id);
        repo.submit_run(&run, None, None).unwrap();

        let event = RuntimeEvent::AdapterMessageEvent {
            kind: RuntimeEventKind::AdapterMessageFinal,
            run_id,
            task_id,
            worker_id,
            role: "assistant".to_string(),
            text: Some("hello".to_string()),
        };
        let cursor_json = "{\"offset\":10,\"lastEntryId\":null}".to_string();
        let committed = repo
            .record_adapter_event(
                &event,
                task_id,
                worker_id,
                run_id,
                Some(cursor_json.clone()),
            )
            .expect("record_adapter_event with cursor commits");

        let stored_cursor: Option<String> = conn
            .query_row(
                "SELECT transcript_cursor FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored_cursor, Some(cursor_json));

        // The event itself is durable in the very same commit.
        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE sequence = ?1",
                [committed.sequence],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 1);
    }

    /// `record_adapter_event` with no cursor (a non-TUI adapter, or a TUI
    /// batch that advanced nothing recordable) leaves `transcript_cursor`
    /// untouched rather than overwriting it with NULL.
    #[test]
    fn record_adapter_event_without_cursor_leaves_transcript_cursor_untouched() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (task_id, worker_id) = seed_worker(&mut conn, project_id);
        let run_id = RunId::new();
        let run = Run {
            run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").unwrap(),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };
        let mut repo = DomainRepository::new(&mut conn, project_id);
        repo.submit_run(&run, None, None).unwrap();

        let first_cursor = "{\"offset\":10,\"lastEntryId\":null}".to_string();
        repo.record_adapter_event(
            &RuntimeEvent::AdapterMessageEvent {
                kind: RuntimeEventKind::AdapterMessageFinal,
                run_id,
                task_id,
                worker_id,
                role: "assistant".to_string(),
                text: Some("hello".to_string()),
            },
            task_id,
            worker_id,
            run_id,
            Some(first_cursor.clone()),
        )
        .expect("first record_adapter_event commits");

        repo.record_adapter_event(
            &RuntimeEvent::AdapterMessageEvent {
                kind: RuntimeEventKind::AdapterMessageFinal,
                run_id,
                task_id,
                worker_id,
                role: "assistant".to_string(),
                text: Some("again".to_string()),
            },
            task_id,
            worker_id,
            run_id,
            None,
        )
        .expect("second record_adapter_event commits without a cursor");

        let stored_cursor: Option<String> = conn
            .query_row(
                "SELECT transcript_cursor FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored_cursor,
            Some(first_cursor),
            "a cursor-less commit must never clobber the previously stored cursor"
        );
    }

    /// An illegal transition through the real repository API commits
    /// nothing: no new event, no projection change.
    #[test]
    fn transition_run_rejects_illegal_edge_and_appends_nothing() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (task_id, worker_id) = seed_worker(&mut conn, project_id);
        let run_id = RunId::new();
        let run = Run {
            run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").unwrap(),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };
        let mut repo = DomainRepository::new(&mut conn, project_id);
        repo.submit_run(&run, None, None).unwrap();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();

        // queued -> succeeded is not a legal edge.
        let mut repo = DomainRepository::new(&mut conn, project_id);
        let target = RunState::try_from("succeeded").unwrap();
        let err = repo.transition_run(run_id, &target, None).unwrap_err();
        assert!(matches!(err, DomainError::Transition(_)));

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "illegal transition must append no event");

        let state: String = conn
            .query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "queued", "projection must be unchanged");
    }

    /// A cost-ceiling violation passes `None` for both vendor refs; the
    /// repository must persist them as SQL NULL, not empty strings.
    #[test]
    fn record_policy_violation_persists_absent_vendor_refs_as_null() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (task_id, worker_id) = seed_worker(&mut conn, project_id);

        let run_id = RunId::new();
        let run = Run {
            run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").unwrap(),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };
        let mut repo = DomainRepository::new(&mut conn, project_id);
        repo.submit_run(&run, None, None)
            .expect("submit_run commits");

        let violation_id = PolicyViolationId::new();
        let committed = repo.record_policy_violation(
            violation_id,
            run_id,
            task_id,
            worker_id,
            "cost_ceiling_exceeded",
            7,
            "sha256:fp",
            None,
            None,
            "quarantine",
        );
        assert!(
            committed.is_ok(),
            "a cost-ceiling violation has no vendor child and must still persist: {:?}",
            committed.err()
        );

        // The vendor refs are real SQL NULLs, not empty strings.
        let (vc, vp): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT vendor_child_id, vendor_parent_ref
                 FROM policy_violations WHERE violation_id = ?1",
                [violation_id.to_string()],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
            )
            .expect("violation row exists");
        assert!(vc.is_none(), "vendor_child_id must be NULL, not empty");
        assert!(vp.is_none(), "vendor_parent_ref must be NULL, not empty");

        // The event journal has the policyViolationRecorded event, proving
        // the append+projection pair committed together.
        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_json LIKE '%policyViolationRecorded%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            event_count, 1,
            "exactly one policyViolationRecorded event must be journaled"
        );
    }

    // ------------------------------------------------- WP19 turn budgets

    /// Seeds a task + worker + queued run with a budgets row of the given
    /// limit, returning the ids.
    fn seed_budgeted_run(
        conn: &mut Connection,
        project_id: ProjectId,
        turn_limit: u32,
    ) -> (RunId, TaskId, WorkerId) {
        let (task_id, worker_id) = seed_worker(conn, project_id);
        let run_id = RunId::new();
        let run = Run {
            run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").unwrap(),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };
        let mut repo = DomainRepository::new(conn, project_id);
        repo.submit_run(&run, None, None)
            .expect("submit_run commits");
        repo.attach_turn_budget(run_id, task_id, None, turn_limit)
            .expect("attach_turn_budget");
        (run_id, task_id, worker_id)
    }

    fn leader_message(
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        kind: crew_protocol::MessageKind,
    ) -> RunMessage {
        RunMessage {
            message_id: crew_protocol::MessageId::new(),
            run_id,
            sender_worker_id: worker_id,
            recipient_worker_id: None,
            task_id,
            kind,
            payload: "next step".into(),
            delivery_state: crew_protocol::DeliveryState::Recorded,
            created_at: Timestamp::now(),
            sent_at: None,
            acknowledged_at: None,
            reply_to: None,
        }
    }

    #[test]
    fn budget_guard_increments_then_refuses_at_cap() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (run_id, task_id, worker_id) = seed_budgeted_run(&mut conn, project_id, 1);
        let committed: crate::domain::Committed;
        {
            let mut repo = DomainRepository::new(&mut conn, project_id);

            // First steering message consumes the single budgeted turn.
            let first = leader_message(
                run_id,
                task_id,
                worker_id,
                crew_protocol::MessageKind::FollowUp,
            );
            repo.record_message(&first, Some("omp-1"), true, false)
                .expect("first send within budget");

            // Second is refused with the typed error; no message row written.
            let second = leader_message(
                run_id,
                task_id,
                worker_id,
                crew_protocol::MessageKind::Steer,
            );
            let err = repo
                .record_message(&second, Some("omp-1"), true, false)
                .expect_err("send at cap must be refused");
            match err {
                DomainError::BudgetExceeded {
                    turns_used,
                    turn_limit,
                    ..
                } => {
                    assert_eq!((turns_used, turn_limit), (1, 1));
                }
                other => panic!("expected BudgetExceeded, got {other:?}"),
            }

            // The refusal's durable fact journals with the row's own numbers.
            committed = repo.journal_budget_exceeded(run_id).expect("journal");
        }
        let message_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE run_id = ?1",
                [run_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        // The refusal's durable fact journals with the row's own numbers.
        assert_eq!(message_rows, 1, "the refused message must not be recorded");
        match committed.envelope.event {
            RuntimeEvent::BudgetExceeded {
                turns_used,
                turn_limit,
                ..
            } => {
                assert_eq!((turns_used, turn_limit), (1, 1));
            }
            other => panic!("expected BudgetExceeded event, got {other:?}"),
        }
    }

    #[test]
    fn worker_side_kinds_never_consume_the_budget() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (run_id, task_id, worker_id) = seed_budgeted_run(&mut conn, project_id, 1);
        let mut repo = DomainRepository::new(&mut conn, project_id);

        for _ in 0..3 {
            let answer = leader_message(
                run_id,
                task_id,
                worker_id,
                crew_protocol::MessageKind::Answer,
            );
            repo.record_message(&answer, Some("omp-1"), true, false)
                .expect("worker-side kinds bypass the guard");
        }
        let turns_used: i64 = conn
            .query_row(
                "SELECT turns_used FROM budgets WHERE run_id = ?1",
                [run_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(turns_used, 0, "answers must not consume turns");
    }

    /// The backstop settles a turn nobody closed -- to `lost`, because
    /// silence is not evidence of success.
    #[test]
    fn the_backstop_settles_an_abandoned_settled_turn_as_lost() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (run_id, _task_id, _worker_id) = seed_budgeted_run(&mut conn, project_id, 10);
        let mut repo = DomainRepository::new(&mut conn, project_id);
        for state in ["starting", "working", "waitingUser"] {
            let target = RunState::try_from(state).unwrap();
            repo.transition_run(run_id, &target, None)
                .expect("drive to waitingUser");
        }
        repo.set_run_flag(run_id, RunFlag::TurnSettled, true)
            .expect("mark the wait as a finished turn");

        let committed = repo
            .settle_abandoned_turn(run_id)
            .expect("settle")
            .expect("an abandoned settled turn must be settled");
        match committed.envelope.event {
            RuntimeEvent::RunEvent { ref state, .. } => assert_eq!(state.as_str(), "lost"),
            ref other => panic!("expected a RunEvent, got {other:?}"),
        }
        // This layer only settles the state; clearing the marker is the
        // sweep's job, so it is still set here.
        assert!(repo.read_run_flags(run_id).unwrap().turn_settled);
    }

    /// The safety property: a wait that is not a finished turn -- a
    /// question, an approval -- must survive the backstop untouched.
    #[test]
    fn the_backstop_never_settles_a_wait_that_is_not_a_finished_turn() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (run_id, _task_id, _worker_id) = seed_budgeted_run(&mut conn, project_id, 10);
        let mut repo = DomainRepository::new(&mut conn, project_id);
        for state in ["starting", "working", "waitingUser"] {
            let target = RunState::try_from(state).unwrap();
            repo.transition_run(run_id, &target, None)
                .expect("drive to waitingUser");
        }
        // No turnSettled flag: this run is waiting on a human, not holding
        // a finished answer.

        assert!(
            repo.settle_abandoned_turn(run_id)
                .expect("settle")
                .is_none(),
            "a question wait must never be settled by a timeout"
        );
        assert!(
            !repo.read_run_flags(run_id).unwrap().turn_settled,
            "sanity: the flag really is unset"
        );
    }

    /// A run still working is not abandoned, however quiet it has been.
    #[test]
    fn the_backstop_never_settles_a_working_run() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (run_id, _task_id, _worker_id) = seed_budgeted_run(&mut conn, project_id, 10);
        let mut repo = DomainRepository::new(&mut conn, project_id);
        for state in ["starting", "working"] {
            let target = RunState::try_from(state).unwrap();
            repo.transition_run(run_id, &target, None)
                .expect("drive to working");
        }
        repo.set_run_flag(run_id, RunFlag::TurnSettled, true)
            .expect("set the flag even though the state does not match");

        assert!(
            repo.settle_abandoned_turn(run_id)
                .expect("settle")
                .is_none(),
            "the state and the flag must BOTH hold; the flag alone is not enough"
        );
    }

    #[test]
    fn timeout_facts_skip_terminal_runs_but_journal_for_live_ones() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (run_id, _task_id, _worker_id) = seed_budgeted_run(&mut conn, project_id, 10);
        let mut repo = DomainRepository::new(&mut conn, project_id);

        // Live (queued) run: the timeout fact journals and carries the kind.
        let committed = repo
            .record_worker_timeout_if_live(run_id, crew_protocol::TimeoutKind::Inactivity, 300_000)
            .expect("live run journals")
            .expect("Some(Committed) for a live run");
        match committed.envelope.event {
            RuntimeEvent::WorkerTimeout { kind, since_ms, .. } => {
                assert_eq!(kind, crew_protocol::TimeoutKind::Inactivity);
                assert_eq!(since_ms, 300_000);
            }
            other => panic!("expected WorkerTimeout event, got {other:?}"),
        }

        // Terminal run: nothing to report. Edges are walked, never jumped:
        // queued -> starting -> working -> succeeded.
        for state in ["starting", "working", "succeeded"] {
            let target = RunState::try_from(state).unwrap();
            repo.transition_run(run_id, &target, None)
                .expect("legal edge");
        }
        let none = repo
            .record_worker_timeout_if_live(run_id, crew_protocol::TimeoutKind::Total, 60_000)
            .expect("terminal run is not an error");
        assert!(
            none.is_none(),
            "a settled run must never receive a timeout fact"
        );
    }

    /// Seeds a run row directly (bypassing `submit_run`'s state machine) so
    /// the pure-query helpers can be tested against an ordered run history.
    fn insert_run(
        conn: &mut Connection,
        task_id: &str,
        worker_id: &str,
        run_id: &RunId,
        state: &str,
        started_at: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO runs (run_id, task_id, worker_id, state, created_at, started_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                run_id.to_string(),
                task_id,
                worker_id,
                state,
                "2026-01-01T00:00:00Z",
                started_at,
            ],
        )
        .expect("insert run");
    }

    /// I1: `record_escalation_raised` persists its `EscalationRaised` event
    /// and its `escalations` projection row in the SAME transaction (the
    /// projection was previously INSERT-ed on `conn` outside the append tx,
    /// risking an orphan row on rollback). The row carries the machine
    /// `reason` as `kind` and stays open (`decided_at` NULL) until an answer
    /// resolves it.
    #[test]
    fn record_escalation_raised_persists_projection_in_event_tx() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (task_id, worker_id) = seed_worker(&mut conn, project_id);
        let run_id = RunId::new();
        let run = Run {
            run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").unwrap(),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };
        let mut repo = DomainRepository::new(&mut conn, project_id);
        repo.submit_run(&run, None, None)
            .expect("submit_run commits");

        let committed = repo
            .record_escalation_raised(run_id, "write_violation", None)
            .expect("record_escalation_raised");
        // The event was appended after the RunQueued row (sequence 3).
        assert!(
            committed.sequence > 3,
            "EscalationRaised event must be journaled"
        );

        // The projection row is present with the reason as `kind`, still open.
        let (kind, question, decided_at, escalation_id): (
            String,
            Option<String>,
            Option<String>,
            String,
        ) = conn
            .query_row(
                "SELECT kind, question, decided_at, escalation_id \
                 FROM escalations WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("escalation projection persists in the event tx");
        assert_eq!(
            kind, "write_violation",
            "reason is written to the kind column"
        );
        assert!(question.is_none());
        assert!(decided_at.is_none(), "open escalation is undecided");
        assert!(!escalation_id.is_empty());
    }

    /// I2: `previous_run_for_task_also_failed` fires only on two CONSECUTIVE
    /// failures (the immediately preceding terminal run also failed), not on
    /// any prior failure. The old `COUNT(*) > 0` escalated fail -> success ->
    /// fail; the predecessor-by-start-time SQL must not.
    #[test]
    fn previous_run_for_task_also_failed_detects_consecutive_only() {
        let mut conn = open_test_db();
        let project_id = ProjectId::new();
        let (task_id, worker_id) = seed_worker(&mut conn, project_id);
        let t = task_id.to_string();
        let w = worker_id.to_string();

        // Same task, three runs ordered by start time: failed, succeeded,
        // failed. Only r3's direct predecessor (r2) matters for "repeat".
        let r1 = RunId::new();
        let r2 = RunId::new();
        let r3 = RunId::new();
        insert_run(
            &mut conn,
            &t,
            &w,
            &r1,
            "failed",
            Some("2026-01-01T01:00:00Z"),
        );
        insert_run(
            &mut conn,
            &t,
            &w,
            &r2,
            "succeeded",
            Some("2026-01-01T02:00:00Z"),
        );
        insert_run(
            &mut conn,
            &t,
            &w,
            &r3,
            "failed",
            Some("2026-01-01T03:00:00Z"),
        );

        // A different task's failure must not bleed across tasks.
        let (task_id_2, _worker_id_2) = seed_worker(&mut conn, project_id);
        let r4 = RunId::new();
        insert_run(
            &mut conn,
            &task_id_2.to_string(),
            &w,
            &r4,
            "failed",
            Some("2026-01-01T04:00:00Z"),
        );

        let repo = DomainRepository::new(&mut conn, project_id);
        // r3 just failed; its immediate predecessor r2 succeeded -- a gap
        // breaks the streak. The old code returned true (r1 is a prior failure).
        assert!(
            !repo.previous_run_for_task_also_failed(r3),
            "a succeeded run between two failures must not be a repeat"
        );
        // r2 just failed, immediately preceded by another failure (r1).
        assert!(
            repo.previous_run_for_task_also_failed(r2),
            "two consecutive failures must raise"
        );
        // r1 is the earliest: no predecessor.
        assert!(
            !repo.previous_run_for_task_also_failed(r1),
            "first failure has no preceding run"
        );
        // r4 is the only run on its task: no predecessor.
        assert!(
            !repo.previous_run_for_task_also_failed(r4),
            "first failure of a fresh task has no predecessor"
        );
    }
}
