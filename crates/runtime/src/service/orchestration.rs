//! The orchestration RPC service: routes every Task 1 method to typed
//! [`DomainRepository`] commands (mutations) or read-only query closures
//! (lookups), translating results and errors to JSON-RPC shapes.
//!
//! OMP remains authoritative for the task graph, scheduling, and policy;
//! this service only persists OMP-supplied intent and enforces run-lifecycle
//! and ownership invariants that only the runtime can see (process/protocol
//! evidence, monotonic revision, connected-instance identity).

use std::sync::Arc;

use batman_protocol::{
    ApprovalId, ApprovalRequest, BatmanMethod, DeliveryState, EventEnvelope, IsolationKind,
    LeaseMode, MessageId, MessageKind, ProjectId, Run, RunFlags, RunId, RunMessage, RunSpec,
    RunState, TaskId, TaskRef, Timestamp, Worker, WorkerId, WorkerProfileRef, error_code,
};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::db::DatabaseHandle;
use crate::domain::{
    DomainError, DomainRepository, TransitionError, embed_envelope, take_envelope,
};
use crate::ipc::ClientPrincipal;

use super::query;
use super::run_driver::{RunDriver, RunDriverContext};
use crate::adapter::CancelScope;

/// A JSON-RPC-shaped error: `(code, message)`, mapped directly onto the
/// wire error object by the connection dispatch layer.
#[derive(Debug)]
pub struct ServiceError {
    pub code: i32,
    pub message: String,
}

impl ServiceError {
    fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: error_code::INVALID_PARAMS,
            message: msg.into(),
        }
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: error_code::INTERNAL_ERROR,
            message: msg.into(),
        }
    }
}

impl From<DomainError> for ServiceError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::Transition(TransitionError::Illegal { run_id, from, to }) => Self {
                code: error_code::ILLEGAL_TRANSITION,
                message: format!("illegal run transition for {run_id}: {from} -> {to}"),
            },
            DomainError::NotFound { kind, id } => Self {
                code: error_code::INVALID_PARAMS,
                message: format!("{kind} {id} not found"),
            },
            DomainError::AlreadyResolved { kind, id, existing } => Self {
                code: error_code::INVALID_PARAMS,
                message: format!("{kind} {id} was already resolved as {existing}"),
            },
            DomainError::RunSettled { run_id } => Self {
                code: error_code::INVALID_PARAMS,
                message: format!("run {run_id} has already settled"),
            },
            DomainError::NotOwner {
                task_id,
                instance_id,
            } => Self {
                code: error_code::INVALID_PARAMS,
                message: format!("task {task_id} is not owned by {instance_id}"),
            },
            DomainError::RevisionTooLow {
                presented, stored, ..
            } => Self {
                code: error_code::INVALID_PARAMS,
                message: format!("revision {presented} is lower than stored revision {stored}"),
            },
            DomainError::RevisionMismatch {
                presented, stored, ..
            } => Self {
                code: error_code::INVALID_PARAMS,
                message: format!("revision {presented} does not match stored revision {stored}"),
            },
            DomainError::PolicyQuarantined { run_id } => Self {
                code: error_code::POLICY_QUARANTINED,
                message: format!(
                    "run {run_id} is quarantined by an undecided policy violation; decide it via policy/violation/decide"
                ),
            },
            other => Self::internal(other.to_string()),
        }
    }
}

impl From<crate::approval::ApprovalError> for ServiceError {
    fn from(err: crate::approval::ApprovalError) -> Self {
        use crate::approval::ApprovalError;
        match err {
            ApprovalError::Forbidden { .. } => Self {
                code: error_code::INVALID_PARAMS,
                message: err.to_string(),
            },
            ApprovalError::Conflict { .. }
            | ApprovalError::RunSettled { .. }
            | ApprovalError::HumanRequired { .. } => Self {
                code: error_code::INVALID_PARAMS,
                message: err.to_string(),
            },
            ApprovalError::NotFound { kind, id } => Self {
                code: error_code::INVALID_PARAMS,
                message: format!("{kind} {id} not found"),
            },
            ApprovalError::Domain(domain_err) => Self::from(domain_err),
        }
    }
}

impl From<crate::policy::ViolationError> for ServiceError {
    fn from(err: crate::policy::ViolationError) -> Self {
        use crate::policy::ViolationError;
        match err {
            ViolationError::Forbidden { .. } => Self {
                code: error_code::INVALID_PARAMS,
                message: err.to_string(),
            },
            ViolationError::Conflict { .. }
            | ViolationError::RunSettled { .. }
            | ViolationError::InvalidResolution { .. } => Self {
                code: error_code::INVALID_PARAMS,
                message: err.to_string(),
            },
            ViolationError::NotFound { .. } => Self {
                code: error_code::INVALID_PARAMS,
                message: err.to_string(),
            },
            ViolationError::Domain(domain_err) => Self::from(domain_err),
            ViolationError::Db(db_err) => Self::internal(db_err.to_string()),
        }
    }
}

/// Maps a lease failure onto the RPC error surface.
///
/// [`LeaseError::IsolationRequired`] is the one variant a caller can fix by
/// changing its request -- asking for `gitWorktree` or `copy` isolation
/// yields an independent workspace -- so it is `invalid_params`. Every other
/// variant is an internal fault the caller cannot act on.
fn lease_error_to_service_error(err: crate::workspace::LeaseError) -> ServiceError {
    match err {
        crate::workspace::LeaseError::IsolationRequired => {
            ServiceError::invalid_params(err.to_string())
        }
        other => ServiceError::internal(other.to_string()),
    }
}

/// Routes every orchestration method to the domain repository. Holds no
/// mutable state itself; every command borrows the shared
/// [`DatabaseHandle`] and commits on the actor thread.
pub struct OrchestrationService {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    run_driver: Option<Arc<dyn RunDriver>>,
    approval: Arc<crate::approval::ApprovalService>,
    violation: Arc<crate::policy::ViolationService>,
    events_tx: broadcast::Sender<EventEnvelope>,
    lease_service: Arc<crate::workspace::LeaseService>,
    artifact_store: Arc<crate::workspace::ArtifactStore>,
    repository: std::path::PathBuf,
    /// The startup policy every run is authorized under unless that run
    /// supplies `policyOverrides`. `None` in tests and in any embedding
    /// that starts the service without a merged config, in which case the
    /// authorizer falls back to its own startup policy.
    policy: Option<Arc<crate::config::RuntimePolicy>>,
    /// The startup config layers, retained so a run carrying
    /// `policyOverrides` can be re-merged against them at submit time
    /// rather than being authorized under a policy it did not request.
    layered_config: Option<Arc<crate::config::LayeredConfig>>,
    /// The display backends this machine can attach a run's pane to.
    /// Resolved once per `run/submit` against the caller's
    /// `displayPreference`, so an adapter never re-probes.
    display: Arc<crate::display::DisplayRegistry>,
}

/// The outcome of [`OrchestrationService::abandon_lease`]: what
/// [`OrchestrationService::abandon_and_announce`] must tell live monitors
/// once cleanup finishes, chosen so it can never announce `LeaseReleased`
/// for a row that never actually left `allocating`/`active`.
enum AbandonOutcome {
    /// The lease row is `released` and, if isolated, its workspace was
    /// torn down cleanly.
    Released,
    /// The lease row is `released`, but its materialized workspace could
    /// not be torn down and needs the doctor's attention.
    ReleasedWithCleanupFailure { message: String },
    /// `release()` itself failed: the row is still `allocating`/`active`.
    ReleaseFailed { message: String },
}

impl OrchestrationService {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        run_driver: Option<Arc<dyn RunDriver>>,
        approval_callback: Arc<dyn crate::approval::ApprovalCallback>,
        violation: Arc<crate::policy::ViolationService>,
        events_tx: broadcast::Sender<EventEnvelope>,
        lease_service: Arc<crate::workspace::LeaseService>,
        artifact_store: Arc<crate::workspace::ArtifactStore>,
        repository: std::path::PathBuf,
    ) -> Self {
        let approval = Arc::new(crate::approval::ApprovalService::new(
            db.clone(),
            project_id,
            approval_callback,
            events_tx.clone(),
        ));
        Self {
            db,
            project_id,
            run_driver,
            approval,
            violation,
            events_tx,
            lease_service,
            artifact_store,
            repository,
            policy: None,
            layered_config: None,
            display: Arc::new(crate::display::DisplayRegistry::with_default_backends(
                batman_protocol::DisplayConfig::default(),
            )),
        }
    }

    /// Attaches the merged startup policy and the layers it came from. The
    /// daemon calls this once, before serving; every other embedding leaves
    /// it unset, gets `None` for a run's policy, and so falls back to the
    /// authorizer's own startup policy.
    ///
    /// The layers are what make `policyOverrides` meaningful: without them
    /// a per-run override has nothing to merge onto and is ignored.
    #[must_use]
    pub fn with_policy(
        mut self,
        layered_config: Arc<crate::config::LayeredConfig>,
        policy: Arc<crate::config::RuntimePolicy>,
    ) -> Self {
        self.display = Arc::new(crate::display::DisplayRegistry::with_default_backends(
            batman_protocol::DisplayConfig {
                backend: policy
                    .display_backend
                    .parse()
                    .unwrap_or(batman_protocol::DisplayBackend::Terminal),
                width: None,
                height: None,
            },
        ));
        self.layered_config = Some(layered_config);
        self.policy = Some(policy);
        self
    }

    /// Broadcasts the envelope embedded by a mutation's `run_domain_op`
    /// closure to live subscribers, if present, then strips it so the
    /// caller's JSON-RPC response never carries the internal key.
    fn broadcast(&self, value: &mut Value) {
        if let Some(envelope) = take_envelope(value) {
            let _ = self.events_tx.send(envelope);
        }
    }

    /// Dispatches one already role-authorized orchestration method.
    /// `principal` is consulted for ownership checks: `reconcile/omp`,
    /// `task/upsert`, `approval/decide`, every run-lifecycle mutation
    /// that guards a runs-DB write against another instance's task
    /// (`run/submit`, `run/retry`, `run/cancel`, `message/send`,
    /// `coordination/child/decide`, `workspace/acquire` -- R77), and every
    /// lease-scoped method that resolves a lease before acting on it
    /// (`workspace/get`, `workspace/release`, `workspace/inspect`,
    /// `workspace/apply` -- R81, via [`Self::require_lease_owner`]). Role
    /// admission itself already happened in the connection layer's method
    /// table; every one of those methods is `ompExtension`-only
    /// (`crate::ipc::ClientPrincipal::allowed_methods`), so `principal` is
    /// always the connected extension instance to arbitrate against, never
    /// a scoped `workerMcp` caller.
    pub async fn dispatch(
        &self,
        method: BatmanMethod,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        match method {
            BatmanMethod::TaskUpsert => self.task_upsert(principal, params).await,
            BatmanMethod::TaskGet => self.task_get(params).await,
            BatmanMethod::WorkerCreate => self.worker_create(params).await,
            BatmanMethod::WorkerList => self.worker_list().await,
            BatmanMethod::WorkerGet => self.worker_get(params).await,
            BatmanMethod::RunSubmit => self.run_submit(principal, params).await,
            BatmanMethod::RunList => self.run_list(params).await,
            BatmanMethod::RunGet => self.run_get(params).await,
            BatmanMethod::RunResult => self.run_result(params).await,
            BatmanMethod::RunRetry => self.run_retry(principal, params).await,
            BatmanMethod::RunCancel => self.run_cancel(principal, params).await,
            BatmanMethod::MessageSend => self.message_send(principal, params).await,
            BatmanMethod::MessageList => self.message_list(params).await,
            BatmanMethod::ApprovalList => self.approval_list(params).await,
            BatmanMethod::ApprovalDecide => self.approval_decide(principal, params).await,
            BatmanMethod::ReconcileOmp => self.reconcile_omp(principal, params).await,
            BatmanMethod::CoordinationChildList => {
                self.coordination_child_list(principal, params).await
            }
            BatmanMethod::CoordinationChildDecide => {
                self.coordination_child_decide(principal, params).await
            }
            BatmanMethod::ProfileRegister => self.profile_register(params).await,
            BatmanMethod::PolicyViolationDecide => {
                self.policy_violation_decide(principal, params).await
            }
            BatmanMethod::PolicyViolationList => self.policy_violation_list(params).await,
            BatmanMethod::WorkspaceAcquire => self.workspace_acquire(principal, params).await,
            BatmanMethod::WorkspaceGet => self.workspace_get(principal, params).await,
            BatmanMethod::WorkspaceRelease => self.workspace_release(principal, params).await,
            BatmanMethod::WorkspaceInspect => self.workspace_inspect(principal, params).await,
            BatmanMethod::WorkspaceApply => self.workspace_apply(principal, params).await,
            BatmanMethod::ArtifactList => self.artifact_list(principal, params).await,
            BatmanMethod::ArtifactFetch => self.artifact_fetch(principal, params).await,
            _ => Err(ServiceError::internal(
                "method is not routed through OrchestrationService",
            )),
        }
    }

    // ------------------------------------------------------------- task

    /// `ownerClientInstanceId` must equal the connected `principal`'s own
    /// instance id -- this is param validation against the identity the
    /// connection layer already authenticated, not the R76 ownership
    /// guard: the legitimate extension always presents its own session
    /// id, so no caller behavior changes. Revision monotonicity and,
    /// for an existing task, ownership of the row itself are both
    /// arbitrated inside `upsert_task`'s own guarded write (R74/R76): a
    /// caller-side pre-check read in a separate round trip could be
    /// interleaved with another write to this task.
    async fn task_upsert(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let task_id = parse_or_new_task_id(params.get("taskId"))?;
        let owner = str_field(params, "ownerClientInstanceId")?;
        if owner != principal.instance_id {
            return Err(ServiceError::invalid_params(format!(
                "ownerClientInstanceId {owner} must match the connected instance {} -- \
                 task/upsert cannot bind a task to another instance; reconcile/omp rebinds \
                 ownership",
                principal.instance_id
            )));
        }
        let revision = u64_field(params, "revision")?;

        let task_ref = TaskRef {
            owner_client_instance_id: owner,
            revision,
        };
        let project_id = self.project_id;
        let mut sequence = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.upsert_task(task_id, &task_ref)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut sequence);

        Ok(json!({
            "taskId": task_id.to_string(),
            "sequence": sequence["sequence"],
        }))
    }

    async fn task_get(&self, params: &Value) -> Result<Value, ServiceError> {
        let task_id = parse_task_id(params.get("taskId"))?;
        self.db
            .run_domain_op(query::task_get_op(task_id))
            .await
            .map_err(ServiceError::from)
    }

    // ----------------------------------------------------------- worker

    async fn worker_create(&self, params: &Value) -> Result<Value, ServiceError> {
        let parent_worker_id = params
            .get("parentWorkerId")
            .and_then(Value::as_str)
            .map(WorkerId::parse)
            .transpose()
            .map_err(|_| ServiceError::invalid_params("parentWorkerId is not a valid id"))?;

        let legacy_fields_present = ["fingerprint", "adapter", "model", "permissionEnvelope"]
            .iter()
            .any(|field| params.get(*field).is_some());

        let (fingerprint, adapter, model, permission_envelope, resolved_profile_json) =
            if let Some(profile_id_value) = params.get("profileId") {
                if legacy_fields_present {
                    return Err(ServiceError::invalid_params(
                        "profileId and fingerprint/adapter/model/permissionEnvelope are mutually exclusive",
                    ));
                }
                let profile_id_str = profile_id_value
                    .as_str()
                    .ok_or_else(|| ServiceError::invalid_params("profileId must be a string"))?;
                let profile_id = crate::adapter::ProfileId::parse(profile_id_str)
                    .map_err(|_| ServiceError::invalid_params("profileId is not a valid id"))?;
                let resolved = self
                    .db
                    .run_domain_op(Box::new(move |conn| {
                        crate::adapter::ProfileStore::get(&*conn, profile_id)
                            .map(|(profile, fingerprint)| {
                                json!({
                                    "fingerprint": fingerprint,
                                    "adapter": profile.adapter,
                                    "model": profile.model,
                                    "permissionEnvelope": profile.permission_envelope,
                                    // The full resolved profile snapshot,
                                    // copied verbatim into the worker row
                                    // -- see `create_worker_with_snapshot`.
                                    "fullProfile": profile,
                                })
                            })
                            .map_err(|err| DomainError::NotFound {
                                kind: "profile",
                                id: err.to_string(),
                            })
                    }))
                    .await
                    .map_err(|_| {
                        ServiceError::invalid_params(format!(
                            "profileId {profile_id} was not found"
                        ))
                    })?;
                (
                    resolved["fingerprint"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    resolved["adapter"].as_str().unwrap_or_default().to_string(),
                    resolved["model"].as_str().unwrap_or_default().to_string(),
                    resolved["permissionEnvelope"].clone(),
                    Some(
                        serde_json::to_string(&resolved["fullProfile"])
                            .expect("a resolved WorkerProfile always serializes"),
                    ),
                )
            } else {
                let fingerprint = str_field(params, "fingerprint")?;
                let adapter = str_field(params, "adapter")?;
                let model = str_field(params, "model")?;
                let permission_envelope = params
                    .get("permissionEnvelope")
                    .cloned()
                    .unwrap_or(json!({}));
                if crate::adapter::AdapterKind::from_wire_name(&adapter).is_some() {
                    return Err(ServiceError {
                        code: error_code::PROFILE_REQUIRED,
                        message: format!(
                            "adapter {adapter:?} requires a resolved profileId; register one via profile/register"
                        ),
                    });
                }
                (fingerprint, adapter, model, permission_envelope, None)
            };

        let worker_id = WorkerId::new();
        let profile = WorkerProfileRef {
            id: worker_id,
            fingerprint,
            adapter,
            model,
            permission_envelope,
        };
        let worker = Worker {
            worker_id,
            profile_ref: profile,
            parent_worker_id,
            created_at: Timestamp::now(),
        };

        let project_id = self.project_id;
        let mut sequence = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.create_worker_with_snapshot(&worker, resolved_profile_json)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut sequence);

        Ok(json!({
            "workerId": worker_id.to_string(),
            "sequence": sequence["sequence"],
        }))
    }

    // ------------------------------------------------- adapter profiles

    /// Validates and registers a [`crate::adapter::WorkerProfile`],
    /// returning its freshly minted `profileId` and content fingerprint.
    /// Deliberately outside the append-only `events` journal -- profile
    /// registration is configuration, not an orchestration fact (see
    /// `crate::adapter::profile_store`) -- so there is nothing to
    /// broadcast here.
    async fn profile_register(&self, params: &Value) -> Result<Value, ServiceError> {
        let mut profile: crate::adapter::WorkerProfile = serde_json::from_value(params.clone())
            .map_err(|err| {
                ServiceError::invalid_params(format!("invalid worker profile: {err}"))
            })?;
        profile.id = crate::adapter::ProfileId::new();
        let fingerprint = profile.fingerprint();
        let policy = crate::adapter::EffectivePolicy::baseline();

        self.db
            .run_domain_op(Box::new({
                let profile = profile.clone();
                let fingerprint = fingerprint.clone();
                move |conn| {
                    crate::adapter::ProfileStore::register(&*conn, &profile, &policy, &fingerprint)
                        .map(|()| Value::Null)
                        .map_err(|err| DomainError::NotFound {
                            kind: "profile registration rejected",
                            id: err.to_string(),
                        })
                }
            }))
            .await
            .map_err(|err| ServiceError::invalid_params(err.to_string()))?;

        Ok(json!({
            "profileId": profile.id.to_string(),
            "fingerprint": fingerprint,
        }))
    }

    async fn worker_list(&self) -> Result<Value, ServiceError> {
        self.db
            .run_domain_op(query::worker_list_op(self.project_id))
            .await
            .map_err(ServiceError::from)
    }

    async fn worker_get(&self, params: &Value) -> Result<Value, ServiceError> {
        let worker_id = parse_worker_id(params.get("workerId"))?;
        self.db
            .run_domain_op(query::worker_get_op(worker_id))
            .await
            .map_err(ServiceError::from)
    }

    // -------------------------------------------------------------- run

    /// Everything `run/submit` and `run/retry` share once a `queued` run row
    /// exists: the display-pane event, the driver check, workspace
    /// materialization, and `RunDriver::start`. Returns the resolved
    /// workspace path so the caller can report it.
    ///
    /// # Errors
    /// Returns `ADAPTER_UNAVAILABLE` when no driver is injected,
    /// `invalid_params` for an unrecognized `workspace_mode`, and an internal
    /// error when the lease, materializer, or driver start fails.
    #[allow(clippy::too_many_arguments)]
    async fn start_queued_run(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        prompt: Option<String>,
        workspace_mode: Option<&str>,
        policy: Option<Arc<crate::config::RuntimePolicy>>,
        display: Option<batman_protocol::DisplaySelection>,
        display_placement: batman_protocol::DisplayPlacement,
    ) -> Result<Option<(std::path::PathBuf, IsolationKind)>, ServiceError> {
        // A pane is journaled only once the run row exists, so a replayer
        // never sees a pane attach to a run it has not seen created. No
        // available backend means no event at all -- headless is a normal
        // outcome, not a failure.
        if let Some(backend) = display.as_ref().and_then(|s| s.selected) {
            let placement = display_placement;
            let project_id = self.project_id;
            let mut attached = self
                .db
                .run_domain_op(Box::new(move |conn| {
                    let mut repo = DomainRepository::new(conn, project_id);
                    repo.record_display_event(
                        batman_protocol::RuntimeEventKind::DisplayPaneAttached,
                        run_id,
                        backend,
                        placement,
                        // The registry resolves availability without
                        // activating a backend, so no vendor pane id
                        // exists yet. Never a filesystem path.
                        String::new(),
                    )
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
                }))
                .await
                .map_err(ServiceError::from)?;
            self.broadcast(&mut attached);
        }

        let Some(driver) = self.run_driver.clone() else {
            // The queued run is preserved; the caller learns the adapter
            // registry is unavailable without a fabricated "started" state.
            return Err(ServiceError {
                code: error_code::ADAPTER_UNAVAILABLE,
                message: "adapter_unavailable".to_string(),
            });
        };

        // Resolve the workspace. `shared` (and an absent mode) runs in the
        // repository itself; `isolated` and `copy` each materialize a real
        // per-run workspace. An unrecognized mode is the caller's error
        // rather than a silent downgrade: falling back to the shared
        // repository would let a typo run a write-capable agent directly
        // against the user's working tree.
        let isolation = match workspace_mode {
            None | Some("shared") => None,
            Some("isolated") => Some(IsolationKind::GitWorktree),
            Some("copy") => Some(IsolationKind::Copy),
            Some(other) => {
                return Err(ServiceError::invalid_params(format!(
                    "workspaceMode must be one of shared, isolated, copy; got {other:?}"
                )));
            }
        };
        let (lease, workspace_path) = match isolation {
            Some(isolation) => {
                // The lease row (not the event below) is the operative
                // claim: every future acquire and conflict check queries
                // `workspace_leases`, so it commits first, matching the
                // commit-then-broadcast order used everywhere else. A crash
                // between this write and the event below leaves an
                // `allocating` row with no matching event -- exactly the
                // gap `LeaseService::stale()`'s grace-period check exists to
                // surface. Reversing the order would instead leave a
                // journaled event for a lease row that was never created,
                // with no doctor check able to find it.
                let lease = self
                    .lease_service
                    .acquire(run_id, LeaseMode::Write, Some(isolation))
                    .map_err(|e| ServiceError::internal(e.to_string()))?;
                // No monitor can have observed this lease until this event
                // commits, so a failure here releases silently: there is
                // no `LeaseRequested` for a compensating `LeaseReleased` to
                // answer.
                if let Err(err) = self
                    .emit_workspace_event(
                        batman_protocol::WorkspaceEvent::LeaseRequested {
                            lease_id: lease.lease_id.clone(),
                            run_id,
                            mode: LeaseMode::Write,
                        },
                        run_id,
                        lease.lease_id.clone(),
                    )
                    .await
                {
                    self.abandon_lease(&lease, None);
                    return Err(err);
                }
                let materializer = match self.materializer() {
                    Ok(materializer) => materializer,
                    Err(err) => {
                        self.abandon_and_announce(&lease, run_id, None).await?;
                        return Err(err);
                    }
                };
                let real_path = match materializer.materialize(run_id, lease.isolation_kind) {
                    Ok(real_path) => real_path,
                    Err(err) => {
                        self.abandon_and_announce(&lease, run_id, None).await?;
                        return Err(ServiceError::internal(err.to_string()));
                    }
                };
                if let Err(err) = self.lease_service.activate(
                    lease.lease_id.clone(),
                    real_path.to_string_lossy().to_string(),
                ) {
                    self.abandon_and_announce(&lease, run_id, Some(&real_path))
                        .await?;
                    return Err(ServiceError::internal(err.to_string()));
                }
                if let Err(err) = self
                    .emit_workspace_event(
                        batman_protocol::WorkspaceEvent::LeaseAcquired {
                            lease_id: lease.lease_id.clone(),
                            run_id,
                            path: real_path.to_string_lossy().to_string(),
                            isolation_kind: lease.isolation_kind,
                            base_revision: lease.base_revision.clone(),
                        },
                        run_id,
                        lease.lease_id.clone(),
                    )
                    .await
                {
                    self.abandon_and_announce(&lease, run_id, Some(&real_path))
                        .await?;
                    return Err(err);
                }
                (Some(lease), Some((real_path, isolation)))
            }
            None => (None, None),
        };

        let project_id = self.project_id;
        let ctx = RunDriverContext {
            db: self.db.clone(),
            project_id,
            run_id,
            task_id,
            worker_id,
            prompt,
            events_tx: self.events_tx.clone(),
            violation_service: Arc::clone(&self.violation),
            workspace_path: workspace_path.as_ref().map(|(path, _)| path.clone()),
            policy,
            display,
        };
        // Orchestration-test-scope: awaited synchronously so the caller
        // observes the final committed state deterministically.
        if let Err(err) = driver.start(ctx).await.map_err(ServiceError::internal) {
            // The run never started, so nothing else will ever release this
            // lease or remove its worktree -- `run/retry` would simply
            // allocate a second one on top. `LeaseRequested`/`LeaseAcquired`
            // were already journaled above, so this must answer with the
            // same `LeaseReleased`/`CleanupFailed` pair every other
            // abandonment past that point emits.
            if let Some(lease) = &lease {
                self.abandon_and_announce(
                    lease,
                    run_id,
                    workspace_path.as_ref().map(|(path, _)| path.as_path()),
                )
                .await?;
            }
            return Err(err);
        }

        Ok(workspace_path)
    }

    /// The `workspaceMode` string a run response echoes for a resolved
    /// isolation kind -- derived from the resolved kind, never the raw
    /// request string, so a future resolution fallback cannot make the
    /// echo lie again (R89). One authority for `run/submit`, `run/retry`,
    /// and `run/get`.
    fn workspace_mode_echo(kind: IsolationKind) -> &'static str {
        match kind {
            IsolationKind::Shared => "shared",
            IsolationKind::GitWorktree => "isolated",
            IsolationKind::Copy => "copy",
        }
    }

    /// `principal` arbitrates ownership of `taskId` against
    /// `submit_run`'s own guarded write (R77) -- never as a caller-side
    /// pre-check: the database actor interleaves whole `run_domain_op`
    /// closures, so only a re-read from inside that same transaction can
    /// observe a `reconcile/omp` rebind landing between this call and the
    /// write.
    async fn run_submit(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let task_id = parse_task_id(params.get("taskId"))?;
        let worker_id = parse_worker_id(params.get("workerId"))?;
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        let workspace_mode = params
            .get("workspaceMode")
            .and_then(Value::as_str)
            .map(str::to_string);

        // Re-merge this run's own `policyOverrides` on top of the startup
        // layers, so the run is authorized against -- and fingerprinted
        // with -- exactly the policy it asked for. Without overrides (or
        // without a merged startup config at all) this is the startup
        // policy unchanged, and `None` lets the authorizer fall back to its
        // own. A malformed or lock-violating override is the caller's
        // fault, so it is `invalid_params`, never an internal error.
        let policy = match (params.get("policyOverrides"), self.layered_config.as_ref()) {
            (Some(overrides), Some(layers)) => Some(Arc::new(
                layers
                    .merge(Some(overrides))
                    .map_err(|e| ServiceError::invalid_params(e.to_string()))?,
            )),
            _ => self.policy.clone(),
        };

        // Resolve the caller's display preference once, here, against the
        // live registry -- the adapter consumes the outcome rather than
        // re-probing. An absent preference means "any available backend,
        // embedded"; an unresolvable one yields `selected: None`, which is
        // headless, not an error.
        let display_preference: batman_protocol::DisplayPreference = params
            .get("displayPreference")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|e| {
                ServiceError::invalid_params(format!("displayPreference is malformed: {e}"))
            })?
            .unwrap_or(batman_protocol::DisplayPreference {
                ordered: Vec::new(),
                placement: batman_protocol::DisplayPlacement::Embedded,
            });
        let display = Some(self.display.resolve(&display_preference));

        // The task must exist. `task_get_op` selects by task id alone,
        // with no project-id predicate -- and neither does any of the
        // owner-arbitration queries this fix adds below (S2). That is
        // correct, not merely convenient: each runtime process owns
        // exactly one project's database, so "task id" and "task id in
        // this project" already coincide; there is no cross-project
        // rejection to perform here.
        self.db
            .run_domain_op(query::task_get_op(task_id))
            .await
            .map_err(|_| {
                ServiceError::invalid_params(format!("task {task_id} not found in this project"))
            })?;

        let run_id = RunId::new();
        let run = Run {
            run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").expect("queued is a valid RunState"),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };

        let project_id = self.project_id;
        let fingerprint = policy.as_ref().map(|p| p.fingerprint.clone());
        let principal_instance_id = principal.instance_id.clone();
        let mut submit_result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.submit_run(&run, fingerprint.as_deref(), Some(&principal_instance_id))
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut submit_result);

        let workspace_path = self
            .start_queued_run(
                run_id,
                task_id,
                worker_id,
                prompt,
                workspace_mode.as_deref(),
                policy,
                display.clone(),
                display_preference.placement,
            )
            .await?;

        let mut result = json!({
            "runId": run_id.to_string(),
            "taskId": task_id.to_string(),
            "sequence": submit_result["sequence"],
        });
        if let Some((path, kind)) = &workspace_path {
            result["workspacePath"] = json!(path.to_string_lossy().to_string());
            result["workspaceMode"] = json!(Self::workspace_mode_echo(*kind));
        }
        if let Some(selection) = &display {
            result["display"] = serde_json::to_value(selection)
                .expect("DisplaySelection always serializes to JSON");
        }
        Ok(result)
    }

    async fn run_list(&self, params: &Value) -> Result<Value, ServiceError> {
        let task_id = params
            .get("taskId")
            .and_then(Value::as_str)
            .map(TaskId::parse)
            .transpose()
            .map_err(|_| ServiceError::invalid_params("taskId is not a valid id"))?;
        self.db
            .run_domain_op(query::run_list_op(task_id, self.project_id))
            .await
            .map_err(ServiceError::from)
    }

    async fn run_get(&self, params: &Value) -> Result<Value, ServiceError> {
        let run_id = parse_run_id(params.get("runId"))?;
        let mut result = self
            .db
            .run_domain_op(query::run_get_op(run_id))
            .await
            .map_err(ServiceError::from)?;

        // Append workspace info if an active lease exists for this run.
        // A lease-DB failure propagates (R62 review W2): collapsing it to
        // "no workspace" would silently hide a real workspace from callers.
        if let Some(info) = self
            .lease_service
            .active_for_run(run_id)
            .map_err(|e| ServiceError::internal(e.to_string()))?
        {
            result["workspacePath"] = json!(info.path);
            result["workspaceMode"] = json!(Self::workspace_mode_echo(info.isolation_kind));
        }
        Ok(result)
    }

    /// `run/result`: a terminal run's final journaled output. Refuses a
    /// run that is still in flight -- a partial answer is never returned.
    async fn run_result(&self, params: &Value) -> Result<Value, ServiceError> {
        let run_id = parse_run_id(params.get("runId"))?;
        let run = self
            .db
            .run_domain_op(query::run_get_op(run_id))
            .await
            .map_err(ServiceError::from)?;

        let state = run["state"].as_str().unwrap_or_default().to_string();
        let is_terminal = RunState::try_from(state.as_str())
            .map(|s| s.is_terminal())
            .unwrap_or(false);
        if !is_terminal {
            return Err(ServiceError::invalid_params(format!(
                "run {run_id} is not finished (state: {state})"
            )));
        }

        let residue = self
            .db
            .run_domain_op(query::run_result_events_op(run_id))
            .await
            .map_err(ServiceError::from)?;

        Ok(json!({
            "runId": run_id.to_string(),
            "state": state,
            "resultText": residue["resultText"],
            "usage": residue["usage"],
            "completedAt": run["completedAt"],
        }))
    }

    /// `run/retry` takes a prior `RunId` (must be terminal) and a `WorkerId`,
    /// and optionally a `prompt`, `workspaceMode`, and `displayPreference`. It
    /// creates a distinct `RunId` inheriting the prior run's `TaskId`, then
    /// routes through the same adapter start path as `run/submit`. When no
    /// driver is available it returns `adapter_unavailable` while preserving
    /// the queued run row, identical to submit's behavior.
    ///
    /// `principal` arbitrates ownership the same way `run/submit` does
    /// (R77), against the prior run's own task -- derived from `prior`'s
    /// row here, never a client-supplied field, so a caller cannot claim a
    /// task it owns to retry a run under a different, unowned task.
    async fn run_retry(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let prior_run_id = parse_run_id(params.get("priorRunId"))?;
        let worker_id = parse_worker_id(params.get("workerId"))?;

        // Parse optional prompt, workspaceMode, and displayPreference the same
        // way run_submit does, so retry can drive the adapter with the same
        // knobs.
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::to_string);
        let workspace_mode = params
            .get("workspaceMode")
            .and_then(Value::as_str)
            .map(str::to_string);
        let display_preference: batman_protocol::DisplayPreference = params
            .get("displayPreference")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|e| {
                ServiceError::invalid_params(format!("displayPreference is malformed: {e}"))
            })?
            .unwrap_or(batman_protocol::DisplayPreference {
                ordered: Vec::new(),
                placement: batman_protocol::DisplayPlacement::Embedded,
            });
        let display = Some(self.display.resolve(&display_preference));

        let prior = self
            .db
            .run_domain_op(query::run_get_op(prior_run_id))
            .await
            .map_err(ServiceError::from)?;
        let task_id = TaskId::parse(prior["taskId"].as_str().unwrap_or_default())
            .map_err(|_| ServiceError::internal("stored run has an invalid taskId"))?;
        let prior_state = prior["state"].as_str().unwrap_or_default();
        let prior_is_terminal = RunState::try_from(prior_state)
            .map(|s| s.is_terminal())
            .unwrap_or(false);
        if !prior_is_terminal {
            return Err(ServiceError::invalid_params(format!(
                "run {prior_run_id} is not in a terminal state ({prior_state})"
            )));
        }

        let new_run_id = RunId::new();
        let run = Run {
            run_id: new_run_id,
            task_id,
            worker_id,
            state: RunState::try_from("queued").expect("queued is valid"),
            flags: RunFlags::default(),
            vendor_session_id: None,
            started_at: None,
            completed_at: None,
        };
        let project_id = self.project_id;
        // A retry is a fresh authorization, so it is fingerprinted with the
        // policy in force now -- never with whatever the prior run carried.
        let fingerprint = self.policy.as_ref().map(|p| p.fingerprint.clone());
        let principal_instance_id = principal.instance_id.clone();
        let mut sequence = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.submit_run(&run, fingerprint.as_deref(), Some(&principal_instance_id))
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut sequence);

        // Route retry through the same start path as submit, so the adapter
        // actually runs. A failure here preserves the queued run row (same
        // as submit).
        let workspace_path = self
            .start_queued_run(
                new_run_id,
                task_id,
                worker_id,
                prompt,
                workspace_mode.as_deref(),
                self.policy.clone(),
                display.clone(),
                display_preference.placement,
            )
            .await?;

        let mut result = json!({
            "runId": new_run_id.to_string(),
            "taskId": task_id.to_string(),
            "priorRunId": prior_run_id.to_string(),
            "sequence": sequence["sequence"],
        });
        if let Some((path, kind)) = &workspace_path {
            result["workspacePath"] = json!(path.to_string_lossy().to_string());
            result["workspaceMode"] = json!(Self::workspace_mode_echo(*kind));
        }
        if let Some(selection) = &display {
            result["display"] = serde_json::to_value(selection)
                .expect("DisplaySelection always serializes to JSON");
        }
        Ok(result)
    }

    /// OMP may request cancellation; the transition is applied only after
    /// this synchronous domain check succeeds (representing the runtime's
    /// own bookkeeping — a real adapter's acknowledgement is a Worker
    /// Adapters plan concern).
    ///
    /// `principal` arbitrates ownership the same way `run/submit` does
    /// (R77), against `transition_run`'s own guarded write.
    async fn run_cancel(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let run_id = parse_run_id(params.get("runId"))?;
        let project_id = self.project_id;
        let to = RunState::try_from("cancelled").expect("cancelled is valid");
        let principal_instance_id = principal.instance_id.clone();
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.transition_run(run_id, &to, Some(&principal_instance_id))
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut result);

        // Also terminate the actual vendor subprocess if one is running.
        if let Some(driver) = &self.run_driver
            && let Err(err) = driver.cancel_run(run_id, CancelScope::Worker).await
        {
            // A real kill failure (an absent adapter is the clean
            // `CancelOutcome::NoRunningAdapter`, not an `Err`): the run is
            // journaled `cancelled` but a vendor process may still be
            // live. Make that visible to `run/get` and the monitor via
            // `degradedControl` (R93), mirroring the policy-violation
            // path's R13 treatment -- guarded write, journaled, broadcast.
            tracing::warn!(error = %err, run_id = %run_id, "failed to cancel running adapter subprocess");
            match self
                .db
                .run_domain_op(Box::new(move |conn| {
                    let mut repo = DomainRepository::new(conn, project_id);
                    repo.set_run_flag(run_id, crate::domain::RunFlag::DegradedControl, true)
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

        Ok(result)
    }
    // ------------------------------------------------------- workspace

    /// Builds a materializer carrying this daemon's configured copy
    /// ceilings. Every workspace materialization goes through here so no
    /// call site can accidentally construct an unbounded copier.
    fn materializer(&self) -> Result<crate::workspace::WorkspaceMaterializer, ServiceError> {
        let materializer =
            crate::workspace::WorkspaceMaterializer::new(self.project_id, self.repository.clone())
                .map_err(|e| ServiceError::internal(e.to_string()))?;
        Ok(match self.policy.as_ref() {
            Some(policy) => {
                materializer.with_copy_limits(policy.copy_max_bytes, policy.copy_max_files)
            }
            None => materializer,
        })
    }

    /// Undoes a lease whose run never started. Releases the lease so the
    /// next acquisition is not blocked by a row nothing will ever activate,
    /// then removes whatever materialization managed to put on disk.
    ///
    /// Best-effort by construction: the caller is already returning the
    /// original fault, so a failure here is logged rather than replacing
    /// the error the caller is reporting. `materialized` is `None` when
    /// materialization itself failed -- `teardown` no-ops on an empty
    /// path, but `git worktree remove` fails outright on a worktree that
    /// was never created, so a never-materialized lease must not attempt
    /// one.
    ///
    /// The three [`AbandonOutcome`] variants distinguish releases that
    /// genuinely reached `released` -- with or without a disk-cleanup
    /// problem worth flagging -- from a `release()` call that itself
    /// failed, whose row is still `allocating`/`active`. A caller must
    /// never announce `LeaseReleased` for the latter.
    fn abandon_lease(
        &self,
        lease: &crate::workspace::CreatedLease,
        materialized: Option<&std::path::Path>,
    ) -> AbandonOutcome {
        let released = match self.lease_service.release(lease.lease_id.clone()) {
            Ok(()) => true,
            // Already `released` -- most likely a racing `workspace/release`
            // call -- so the row's state already matches what
            // `LeaseReleased` announces; nothing failed here.
            Err(crate::workspace::LeaseError::AlreadyReleased { .. }) => true,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    lease_id = %lease.lease_id,
                    run_id = %lease.run_id,
                    "failed to release the lease of a run that never started"
                );
                false
            }
        };

        let teardown_error = materialized.and_then(|path| {
            match self
                .materializer()
                .map_err(|e| e.message.clone())
                .and_then(|m| {
                    m.teardown(path, lease.isolation_kind)
                        .map_err(|e| e.to_string())
                }) {
                Ok(()) => None,
                Err(message) => {
                    tracing::warn!(
                        error = %message,
                        lease_id = %lease.lease_id,
                        path = %path.display(),
                        "failed to tear down the workspace of a run that never started"
                    );
                    Some(message)
                }
            }
        });

        match (released, teardown_error) {
            (true, None) => AbandonOutcome::Released,
            (true, Some(message)) => {
                // The release is genuine -- `released_at` is set -- but the
                // leaked directory still needs the doctor's attention, so
                // `state` moves to `cleanupFailed` even though the lease
                // itself is gone.
                let _ = self
                    .lease_service
                    .mark_cleanup_failed(lease.lease_id.clone());
                AbandonOutcome::ReleasedWithCleanupFailure { message }
            }
            (false, teardown_error) => {
                let _ = self
                    .lease_service
                    .mark_cleanup_failed(lease.lease_id.clone());
                let message = match teardown_error {
                    Some(msg) => {
                        format!("release failed and workspace teardown failed: {msg}")
                    }
                    None => "failed to release lease".to_string(),
                };
                AbandonOutcome::ReleaseFailed { message }
            }
        }
    }

    /// Runs [`Self::abandon_lease`] and announces the outcome to live
    /// monitors. Both `start_queued_run` and `workspace_acquire` journal
    /// `LeaseRequested` before any fallible step past `acquire` runs, so an
    /// abandonment past that point must resolve every connected monitor's
    /// view of the lease, exactly as `workspace_release` already
    /// guarantees for a caller-initiated release:
    /// - [`AbandonOutcome::Released`]: announce `LeaseReleased`.
    /// - [`AbandonOutcome::ReleasedWithCleanupFailure`]: the lease really
    ///   is released, so announce `CleanupFailed` (naming the leaked
    ///   directory) *and* `LeaseReleased`.
    /// - [`AbandonOutcome::ReleaseFailed`]: the row never left
    ///   `allocating`/`active`, so announce only `CleanupFailed` -- a
    ///   `LeaseReleased` here would be false.
    ///
    /// # Errors
    /// Returns a [`ServiceError`] if journaling an event fails. That
    /// failure -- not the fault that triggered the abandonment -- is what
    /// the caller should report: a dead journal is the more serious fault,
    /// and the lease row has already been updated either way.
    async fn abandon_and_announce(
        &self,
        lease: &crate::workspace::CreatedLease,
        run_id: RunId,
        materialized: Option<&std::path::Path>,
    ) -> Result<(), ServiceError> {
        match self.abandon_lease(lease, materialized) {
            AbandonOutcome::Released => {
                self.emit_workspace_event(
                    batman_protocol::WorkspaceEvent::LeaseReleased {
                        lease_id: lease.lease_id.clone(),
                        run_id,
                    },
                    run_id,
                    lease.lease_id.clone(),
                )
                .await
            }
            AbandonOutcome::ReleasedWithCleanupFailure { message } => {
                self.emit_workspace_event(
                    batman_protocol::WorkspaceEvent::CleanupFailed {
                        lease_id: lease.lease_id.clone(),
                        error: message,
                    },
                    run_id,
                    lease.lease_id.clone(),
                )
                .await?;
                self.emit_workspace_event(
                    batman_protocol::WorkspaceEvent::LeaseReleased {
                        lease_id: lease.lease_id.clone(),
                        run_id,
                    },
                    run_id,
                    lease.lease_id.clone(),
                )
                .await
            }
            AbandonOutcome::ReleaseFailed { message } => {
                self.emit_workspace_event(
                    batman_protocol::WorkspaceEvent::CleanupFailed {
                        lease_id: lease.lease_id.clone(),
                        error: message,
                    },
                    run_id,
                    lease.lease_id.clone(),
                )
                .await
            }
        }
    }

    /// Journals a [`batman_protocol::WorkspaceEvent`] and broadcasts it to
    /// live subscribers, exactly as every domain mutation does. Workspace
    /// state itself lives in the lease database, so this is the only way a
    /// monitor learns a lease was taken, inspected, applied, or released.
    async fn emit_workspace_event(
        &self,
        kind: batman_protocol::WorkspaceEvent,
        run_id: RunId,
        lease_id: String,
    ) -> Result<(), ServiceError> {
        let project_id = self.project_id;
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_workspace_event(kind, run_id, lease_id)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut result);
        Ok(())
    }

    /// Like [`Self::emit_workspace_event`], but the append refuses inside
    /// its own transaction if the run is policy-quarantined (R78): used
    /// for `workspace/apply`'s `ApplyStarted`, so the journal can never
    /// record an apply start for a run quarantined at that instant.
    async fn emit_workspace_event_unless_quarantined(
        &self,
        kind: batman_protocol::WorkspaceEvent,
        run_id: RunId,
        lease_id: String,
    ) -> Result<(), ServiceError> {
        let project_id = self.project_id;
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_workspace_event_unless_quarantined(kind, run_id, lease_id)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut result);
        Ok(())
    }

    /// Ownership arbitration (R77) happens here, in the handler, as a
    /// dedicated domain round trip immediately before
    /// [`crate::workspace::LeaseService::acquire`] -- not inside a
    /// runs-DB write, because there isn't one to embed it in until
    /// `record_workspace_event` below: the lease itself lives in
    /// [`crate::workspace::LeaseService`]'s own database file, a separate
    /// connection this method's `db.run_domain_op` calls cannot share a
    /// transaction with. That leaves a residual, bounded window: a
    /// `reconcile/omp` rebind that commits between this check and
    /// `lease_service.acquire` below is not observed, so a caller that
    /// loses ownership in that exact gap can still allocate a lease this
    /// call returns as `active`. No fabricated cross-database atomicity
    /// closes that gap -- see [`super::query::run_owner_op`]'s doc comment.
    /// A caller refused here allocates nothing: `run_owner_op` runs before
    /// `lease_service.acquire`, so no lease and no workspace event exist
    /// to unwind.
    async fn workspace_acquire(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        use batman_protocol::LeaseRequest;
        let request: LeaseRequest = serde_json::from_value(params.clone())
            .map_err(|e| ServiceError::invalid_params(e.to_string()))?;
        let run_id = request.run_id;
        let mode = request.mode;
        let isolation = request.requested_isolation;

        self.db
            .run_domain_op(query::run_owner_op(run_id, principal.instance_id.clone()))
            .await
            .map_err(ServiceError::from)?;

        // Phase 1: allocate the lease (state: allocating, path: empty).
        // `IsolationRequired` is caller-correctable -- the request can succeed
        // by asking for gitWorktree/copy isolation -- so it maps to
        // invalid_params rather than an internal fault.
        let lease = self
            .lease_service
            .acquire(run_id, mode, isolation)
            .map_err(lease_error_to_service_error)?;
        // No monitor can have observed this lease until this event
        // commits, so a failure here releases silently: there is no
        // `LeaseRequested` for a compensating `LeaseReleased` to answer.
        if let Err(err) = self
            .emit_workspace_event(
                batman_protocol::WorkspaceEvent::LeaseRequested {
                    lease_id: lease.lease_id.clone(),
                    run_id,
                    mode,
                },
                run_id,
                lease.lease_id.clone(),
            )
            .await
        {
            self.abandon_lease(&lease, None);
            return Err(err);
        }

        // Phase 2: materialize the workspace. Every step from here on can
        // fail with the lease already committed as `allocating` (or, past
        // `activate`, `active`); each arm releases it and tears down
        // whatever materialization left on disk rather than leaking both.
        let materializer = match self.materializer() {
            Ok(materializer) => materializer,
            Err(err) => {
                self.abandon_and_announce(&lease, run_id, None).await?;
                return Err(err);
            }
        };
        let real_path = match materializer.materialize(run_id, lease.isolation_kind) {
            Ok(real_path) => real_path,
            Err(err) => {
                self.abandon_and_announce(&lease, run_id, None).await?;
                return Err(ServiceError::internal(err.to_string()));
            }
        };

        // Phase 3: activate the lease with the real path
        if let Err(err) = self.lease_service.activate(
            lease.lease_id.clone(),
            real_path.to_string_lossy().to_string(),
        ) {
            self.abandon_and_announce(&lease, run_id, Some(&real_path))
                .await?;
            return Err(ServiceError::internal(err.to_string()));
        }
        if let Err(err) = self
            .emit_workspace_event(
                batman_protocol::WorkspaceEvent::LeaseAcquired {
                    lease_id: lease.lease_id.clone(),
                    run_id,
                    path: real_path.to_string_lossy().to_string(),
                    isolation_kind: lease.isolation_kind,
                    base_revision: lease.base_revision.clone(),
                },
                run_id,
                lease.lease_id.clone(),
            )
            .await
        {
            self.abandon_and_announce(&lease, run_id, Some(&real_path))
                .await?;
            return Err(err);
        }

        Ok(json!({
            "leaseId": lease.lease_id,
            "runId": lease.run_id.to_string(),
            "mode": match lease.mode { batman_protocol::LeaseMode::ReadOnly => "readOnly", batman_protocol::LeaseMode::Write => "write" },
            "isolationKind": match lease.isolation_kind { batman_protocol::IsolationKind::Shared => "shared", batman_protocol::IsolationKind::GitWorktree => "gitWorktree", batman_protocol::IsolationKind::Copy => "copy" },
            "path": real_path.to_string_lossy().to_string(),
            "state": "active",
            "baseRevision": lease.base_revision,
            "acquisitionSequence": lease.acquisition_sequence,
        }))
    }

    /// Resolves `lease_id` via [`crate::workspace::LeaseService::get`] and
    /// confirms `principal` currently owns the run it belongs to -- the
    /// ownership gate shared, byte-for-byte, by all four lease-scoped
    /// methods (`workspace/get`, `workspace/release`, `workspace/inspect`,
    /// `workspace/apply`; R81). As on `workspace_acquire` (R77), this is a
    /// dedicated domain round trip against the runs database, separate
    /// from `LeaseService`'s own database file, so the two cannot commit
    /// atomically: a `reconcile/omp` rebind that commits inside the gap
    /// between this check and whatever the caller does next is not
    /// observed -- see [`super::query::run_owner_op`]'s doc comment. Each
    /// caller documents its own residual window past this point.
    async fn require_lease_owner(
        &self,
        principal: &ClientPrincipal,
        lease_id: String,
        enforce_quarantine: bool,
    ) -> Result<batman_protocol::WorkspaceInfo, ServiceError> {
        // One refusal string for every caller-distinguishable failure of
        // this gate: unknown leaseId, unowned lease, and a lease whose run
        // row is missing all answer identically, so neither the error code
        // (R84) nor the message text is an existence oracle -- and the
        // ownership arm cannot leak the owning task/instance ids to a
        // caller that was just told it does not own the lease.
        const LEASE_REFUSAL: &str = "leaseId is not a lease on a run you own";
        let lease = self.lease_service.get(lease_id).map_err(|e| match e {
            crate::workspace::LeaseError::NotFound { .. } => {
                ServiceError::invalid_params(LEASE_REFUSAL)
            }
            other => ServiceError::internal(other.to_string()),
        })?;
        // When asked (inspect/apply), the quarantine flag is read in the
        // SAME domain op as the owner check, so lease, owner, and flags
        // come from one consistent snapshot -- there is no window between
        // the ownership gate and the quarantine gate (R78).
        let op = if enforce_quarantine {
            query::run_owner_not_quarantined_op(lease.run_id, principal.instance_id.clone())
        } else {
            query::run_owner_op(lease.run_id, principal.instance_id.clone())
        };
        self.db.run_domain_op(op).await.map_err(|e| match e {
            crate::domain::DomainError::NotOwner { .. }
            | crate::domain::DomainError::NotFound { .. } => {
                ServiceError::invalid_params(LEASE_REFUSAL)
            }
            other => ServiceError::from(other),
        })?;
        Ok(lease)
    }

    /// [`Self::require_lease_owner`] runs before this method returns any
    /// of its fields. `workspace_get` is read-only -- no teardown, no
    /// materialization, no apply -- and every field it discloses is
    /// already reproducible by any caller from the pre-existing,
    /// unrelated `events/replay` stream (`LeaseRequested` carries `mode`;
    /// `LeaseAcquired` carries `path`, `isolationKind`, `baseRevision`;
    /// `state` is derivable from the presence or absence of a later
    /// `LeaseReleased`/`CleanupFailed` event). Gating this method closes
    /// no disclosure that stream doesn't already leave open. It is gated
    /// anyway: a uniform ownership surface across all four lease-scoped
    /// methods is worth more than a documented exception that a future
    /// response field could silently invalidate. The residual window
    /// documented on [`Self::require_lease_owner`] applies here too: a
    /// `reconcile/omp` rebind that commits between the gate and the
    /// response below is not observed.
    async fn workspace_get(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let lease_id = str_field(params, "leaseId")?;
        let info = self.require_lease_owner(principal, lease_id, false).await?;

        // Canonical `WorkspaceInfo` serialization; byte-identical to the
        // previous hand-rolled shape (R55 review E1).
        serde_json::to_value(&info)
            .map_err(|e| ServiceError::internal(format!("serializing WorkspaceInfo: {e}")))
    }

    /// [`Self::require_lease_owner`] runs before
    /// [`crate::workspace::LeaseService::release`] tears down anything --
    /// a caller refused there releases nothing, so the lease is left
    /// `active` and no `LeaseReleased`/`CleanupFailed` event is
    /// journaled. Its residual window is the single gap between the gate
    /// and `lease_service.release` below: a `reconcile/omp` rebind that
    /// commits inside it is not observed.
    async fn workspace_release(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let request: batman_protocol::ReleaseRequest = serde_json::from_value(params.clone())
            .map_err(|e| ServiceError::invalid_params(e.to_string()))?;
        // Read the lease before releasing it: after release the row is
        // gone, and the event must carry the run it belonged to.
        let lease = self
            .require_lease_owner(principal, request.lease_id.clone(), false)
            .await?;
        self.lease_service
            .release(request.lease_id.clone())
            .map_err(|e| match e {
                // Releasing an already-released lease is the caller's
                // error (a racing double-release), not an internal fault --
                // abandon_lease treats the same condition as benign
                // (R84 review W4).
                crate::workspace::LeaseError::AlreadyReleased { .. } => {
                    ServiceError::invalid_params(e.to_string())
                }
                other => ServiceError::internal(other.to_string()),
            })?;

        // Tear the materialized directory down. The lease is already
        // released, so a teardown failure is an operator problem (a leaked
        // worktree), not a caller error: record it as `cleanupFailed` for
        // the doctor and still report the release as successful.
        let cleanup_error = self
            .materializer()
            .and_then(|m| {
                m.teardown(std::path::Path::new(&lease.path), lease.isolation_kind)
                    .map_err(|e| ServiceError::internal(e.to_string()))
            })
            .err();

        if let Some(err) = &cleanup_error {
            let _ = self
                .lease_service
                .mark_cleanup_failed(request.lease_id.clone());
            self.emit_workspace_event(
                batman_protocol::WorkspaceEvent::CleanupFailed {
                    lease_id: request.lease_id.clone(),
                    error: err.message.clone(),
                },
                lease.run_id,
                request.lease_id.clone(),
            )
            .await?;
        }

        self.emit_workspace_event(
            batman_protocol::WorkspaceEvent::LeaseReleased {
                lease_id: request.lease_id.clone(),
                run_id: lease.run_id,
            },
            lease.run_id,
            request.lease_id.clone(),
        )
        .await?;
        Ok(json!({
            "released": true,
            "cleanupFailed": cleanup_error.is_some(),
        }))
    }

    /// [`Self::require_lease_owner`] runs before any materialization: a
    /// caller refused there never reaches a real git workspace it never
    /// acquired, and journals nothing (`WorkspaceInspected`,
    /// `ArtifactPublished`). The quarantine flag is read in the same
    /// domain op as the owner check (R78), so there is no gate-to-gate
    /// window -- but unlike `workspace_apply`, inspect has NO in-tx
    /// re-check on its appends: its quarantine span runs from that gate
    /// through the git read and both journal appends. Deliberate: by
    /// append time the disclosure (the git read) has already happened,
    /// so refusing the journal entry would hide real activity rather
    /// than prevent it. The ownership residual window is wider than
    /// `workspace_release`'s single gap, though: the git inspection
    /// itself and both journal entries below sit inside it, so a
    /// `reconcile/omp` rebind that commits after the gate can still let a
    /// caller that just lost ownership inspect the workspace and have its
    /// patch published and journaled under the former owner's principal.
    async fn workspace_inspect(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        use batman_protocol::InspectRequest;
        let request: InspectRequest = serde_json::from_value(params.clone())
            .map_err(|e| ServiceError::invalid_params(e.to_string()))?;
        let lease = self
            .require_lease_owner(principal, request.lease_id.clone(), true)
            .await?;

        let inspector = crate::workspace::WorkspaceInspector::with_store(
            std::path::PathBuf::from(&lease.path),
            self.artifact_store.clone(),
            lease.run_id,
        );
        let result = inspector
            .inspect(&request)
            .await
            .map_err(|e| ServiceError::internal(e.to_string()))?;
        self.emit_workspace_event(
            batman_protocol::WorkspaceEvent::WorkspaceInspected {
                lease_id: result.lease_id.clone(),
                patch_artifact_id: result.patch_artifact_id,
                commit_count: result.commit_count,
                dirty_file_count: result.dirty_file_count,
                untracked_file_count: result.untracked_file_count,
            },
            lease.run_id,
            result.lease_id.clone(),
        )
        .await?;
        // The patch an inspect produced is fetchable, so publishing it is
        // its own event -- a monitor listing artifacts must not have to
        // infer their existence from an inspect result it may have missed.
        self.emit_workspace_event(
            batman_protocol::WorkspaceEvent::ArtifactPublished {
                lease_id: result.lease_id.clone(),
                artifact_id: result.patch_artifact_id,
                kind: "patch".to_string(),
            },
            lease.run_id,
            result.lease_id.clone(),
        )
        .await?;

        // Serialize the canonical protocol type rather than a hand-rolled
        // shape: `InspectResult`'s serde output is byte-identical to the
        // previous `json!` block (camelCase, ids as UUID strings), and the
        // extension validates against its schema $def (R55).
        serde_json::to_value(&result)
            .map_err(|e| ServiceError::internal(format!("serializing InspectResult: {e}")))
    }

    /// [`Self::require_lease_owner`] runs before artifact resolution or
    /// the `ApplyStarted` journal entry: a caller refused there is
    /// refused for ownership (`-32602`), not for a not-yet-resolved
    /// artifact (`-32603`), and journals nothing. The quarantine flag is
    /// read in the same domain op as the owner check AND re-checked
    /// inside the `ApplyStarted` append's own transaction (R78), so the
    /// journal can never record an apply start for a quarantined run;
    /// the quarantine residue is exactly the append-to-working-tree gap
    /// below, which no cross-database transaction closes. The ownership
    /// residual window remains wider than `workspace_release`'s single
    /// gap: the `ApplyStarted` append sits inside it, before
    /// `applier.apply` actually mutates the working tree -- a
    /// `reconcile/omp` rebind that commits after the gate can leave an
    /// `ApplyStarted` event and a real working-tree mutation attributed
    /// to a caller that is no longer the owner.
    async fn workspace_apply(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        use batman_protocol::ApplyRequest;
        let request: ApplyRequest = serde_json::from_value(params.clone())
            .map_err(|e| ServiceError::invalid_params(e.to_string()))?;
        let lease = self
            .require_lease_owner(principal, request.lease_id.clone(), true)
            .await?;

        let applier = crate::workspace::WorkspaceApplier::from_store(
            std::path::PathBuf::from(&lease.path),
            self.artifact_store.clone(),
            lease.run_id,
        );
        // Re-checked inside the append's own transaction (R78): a
        // quarantine landing between the gate above and this append can
        // never leave an ApplyStarted record for a quarantined run. The
        // remaining residue is exactly the gap between this append and
        // applier.apply's working-tree mutation below, which no
        // cross-database transaction can close.
        self.emit_workspace_event_unless_quarantined(
            batman_protocol::WorkspaceEvent::ApplyStarted {
                lease_id: request.lease_id.clone(),
                strategy: request.strategy,
                artifact_id: request.artifact_id,
                expected_target_revision: request.expected_target_revision.clone(),
            },
            lease.run_id,
            request.lease_id.clone(),
        )
        .await?;
        let result = applier
            .apply(&request)
            .await
            .map_err(|e| ServiceError::internal(e.to_string()))?;
        self.emit_workspace_event(
            batman_protocol::WorkspaceEvent::ApplyCompleted {
                lease_id: result.lease_id.clone(),
                success: result.success,
                conflict_artifact_id: result.conflict_artifact_id,
                target_revision_after: result.target_revision_after.clone(),
            },
            lease.run_id,
            result.lease_id.clone(),
        )
        .await?;
        // A conflict is the one apply outcome OMP must act on, so it gets
        // its own event rather than being buried in `ApplyCompleted`'s
        // `success: false`.
        if let Some(conflict_artifact_id) = result.conflict_artifact_id {
            self.emit_workspace_event(
                batman_protocol::WorkspaceEvent::ApplyConflict {
                    lease_id: result.lease_id.clone(),
                    conflict_artifact_id,
                    strategy: request.strategy,
                    expected_target_revision: request.expected_target_revision.clone(),
                },
                lease.run_id,
                result.lease_id.clone(),
            )
            .await?;
            self.emit_workspace_event(
                batman_protocol::WorkspaceEvent::ArtifactPublished {
                    lease_id: result.lease_id.clone(),
                    artifact_id: conflict_artifact_id,
                    kind: "conflictReport".to_string(),
                },
                lease.run_id,
                result.lease_id.clone(),
            )
            .await?;
        }

        // Canonical `ApplyResult` serialization; byte-identical to the
        // previous hand-rolled shape (R55).
        serde_json::to_value(&result)
            .map_err(|e| ServiceError::internal(format!("serializing ApplyResult: {e}")))
    }

    async fn artifact_list(
        &self,
        principal: &crate::ipc::ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let kind_str = str_field(params, "kind").ok();
        let kind = kind_str.as_ref().and_then(|k| match k.as_str() {
            "patch" => Some(batman_protocol::ArtifactKind::Patch),
            "commitList" => Some(batman_protocol::ArtifactKind::CommitList),
            "conflictReport" => Some(batman_protocol::ArtifactKind::ConflictReport),
            "workspaceManifest" => Some(batman_protocol::ArtifactKind::WorkspaceManifest),
            _ => None,
        });
        let task_id = params
            .get("taskId")
            .and_then(Value::as_str)
            .map(TaskId::parse)
            .transpose()
            .map_err(|_| ServiceError::invalid_params("taskId is not a valid id"))?;
        // Resolve the scope: run ids belonging to tasks this session owns.
        let project_id = self.project_id;
        let scope: Vec<String> = self
            .db
            .run_domain_op(crate::service::query::owned_run_ids_op(
                principal.instance_id.clone(),
                task_id,
                project_id,
            ))
            .await
            .map_err(ServiceError::from)?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // Fetch all artifacts and keep only those in scope; the store
        // already returns the canonical `ArtifactListResult`.
        let mut result = self.artifact_store.list(kind).await;
        result.artifacts.retain(|a| {
            a.run_id
                .as_deref()
                .is_some_and(|id| scope.iter().any(|s| s == id))
        });
        // Canonical `ArtifactListResult` serialization; byte-identical to
        // the previous hand-rolled shape (R55).
        serde_json::to_value(&result)
            .map_err(|e| ServiceError::internal(format!("serializing ArtifactListResult: {e}")))
    }
    async fn artifact_fetch(
        &self,
        principal: &crate::ipc::ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        use batman_protocol::ArtifactId;
        let artifact_id: ArtifactId = serde_json::from_value(
            params
                .get("artifactId")
                .cloned()
                .ok_or_else(|| ServiceError::invalid_params("artifactId is required"))?,
        )
        .map_err(|e| ServiceError::invalid_params(e.to_string()))?;
        let offset: u64 = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let length: u64 = params
            .get("length")
            .and_then(Value::as_u64)
            .unwrap_or(crate::workspace::ARTIFACT_FETCH_MAX_BYTES);

        // Resolve the scope and check the artifact belongs to it.
        let project_id = self.project_id;
        let scope: Vec<String> = self
            .db
            .run_domain_op(crate::service::query::owned_run_ids_op(
                principal.instance_id.clone(),
                None,
                project_id,
            ))
            .await
            .map_err(ServiceError::from)?
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        // Authorize on metadata only, BEFORE reading and hashing content:
        // fetching first left a latency side-channel between "exists but
        // not yours" and "does not exist" (R35). One refusal message for
        // unknown, out-of-scope, and unstamped alike -- no oracle.
        let refusal =
            || ServiceError::invalid_params("artifactId is not an artifact on a task you own");
        let metadata = self
            .artifact_store
            .fetch(&artifact_id)
            .await
            .map_err(|_| refusal())?;
        let in_scope = metadata
            .run_id
            .as_deref()
            .is_some_and(|id| scope.iter().any(|s| s == id));
        if !in_scope {
            return Err(refusal());
        }

        // Post-authorization, the caller is a proven owner and the id is
        // known, so a failure here (digest mismatch on read -- on-disk
        // tampering) is an internal fault worth naming, never the
        // ownership refusal (R35 review W6).
        let result = self
            .artifact_store
            .fetch_chunked(&artifact_id, offset, length)
            .await
            .map_err(|e| ServiceError::internal(format!("artifact content read failed: {e}")))?;

        // Canonical `ArtifactFetchResult` serialization; byte-identical to
        // the previous hand-rolled shape (R55).
        serde_json::to_value(&result)
            .map_err(|e| ServiceError::internal(format!("serializing ArtifactFetchResult: {e}")))
    }

    // ---------------------------------------------------------- message

    /// `principal` arbitrates ownership the same way `run/submit` does
    /// (R77), against `record_message`'s own guarded write -- resolved
    /// there from `runId`'s actual owning run, never from `taskId` as
    /// presented in `params`.
    async fn message_send(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let run_id = parse_run_id(params.get("runId"))?;
        let sender_worker_id = parse_worker_id(params.get("senderWorkerId"))?;
        let task_id = parse_task_id(params.get("taskId"))?;
        let kind = parse_message_kind(params.get("kind"))?;
        let payload = str_field(params, "payload")?;
        let recipient_worker_id = params
            .get("recipientWorkerId")
            .and_then(Value::as_str)
            .map(WorkerId::parse)
            .transpose()
            .map_err(|_| ServiceError::invalid_params("recipientWorkerId is not a valid id"))?;
        let reply_to = params
            .get("replyTo")
            .and_then(Value::as_str)
            .map(MessageId::parse)
            .transpose()
            .map_err(|_| ServiceError::invalid_params("replyTo is not a valid id"))?;

        let follow_up_payload = payload.clone();
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
        let principal_instance_id = principal.instance_id.clone();
        let mut sequence = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                // Quarantine enforced inside record_message's own guarded
                // transaction, after the owner re-read (R78) -- the old
                // caller-side ensure_not_quarantined pre-check read a
                // snapshot a quarantine could land behind.
                // `message/send` deliberately passes `enforce_live: false`:
                // OMP may journal a message against a run in any state --
                // the delivery diagnostic path already handles a run with
                // no live adapter (R94 gates only the worker-MCP broker
                // writes whose doc promises liveness).
                repo.record_message(&message, Some(&principal_instance_id), true, false)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut sequence);

        // Best-effort live delivery to an already-running adapter. A
        // missing driver, a `queued`/not-yet-started run (the normal case
        // -- `NoRunningAdapter`), or any other delivery failure must never
        // fail this RPC or unwind the message already durably recorded
        // above: the message stays `recorded` and the run's state is
        // untouched, matching `RunDriver::send_follow_up`'s own contract.
        // A genuine success, symmetrically, must advance the message past
        // `recorded` -- leaving it there forever regardless of outcome is
        // exactly the deliveryState inversion this branch exists to avoid.
        if let Some(driver) = self.run_driver.clone() {
            match driver
                .send_follow_up(run_id, task_id, sender_worker_id, follow_up_payload)
                .await
            {
                Ok(()) => {
                    let mut sent = self
                        .db
                        .run_domain_op(Box::new(move |conn| {
                            let mut repo = DomainRepository::new(conn, project_id);
                            repo.update_delivery(message_id, &DeliveryState::Sent)
                                .map(|c| {
                                    embed_envelope(json!({ "sequence": c.sequence }), &c.envelope)
                                })
                        }))
                        .await
                        .map_err(ServiceError::from)?;
                    self.broadcast(&mut sent);
                }
                Err(err) => {
                    let mut diagnostic = self
                        .db
                        .run_domain_op(Box::new(move |conn| {
                            let mut repo = DomainRepository::new(conn, project_id);
                            repo.record_diagnostic(
                                run_id,
                                batman_protocol::DiagnosticLevel::Warning,
                                "follow_up_delivery_failed",
                                format!("run {run_id}: {err}"),
                            )
                            .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
                        }))
                        .await
                        .map_err(ServiceError::from)?;
                    self.broadcast(&mut diagnostic);
                }
            }
        }

        Ok(json!({
            "messageId": message_id.to_string(),
            "sequence": sequence["sequence"],
        }))
    }

    async fn message_list(&self, params: &Value) -> Result<Value, ServiceError> {
        let run_id = parse_run_id(params.get("runId"))?;
        self.db
            .run_domain_op(query::message_list_op(run_id))
            .await
            .map_err(ServiceError::from)
    }

    // --------------------------------------------------------- approval

    async fn approval_list(&self, params: &Value) -> Result<Value, ServiceError> {
        let run_id = params
            .get("runId")
            .and_then(Value::as_str)
            .map(RunId::parse)
            .transpose()
            .map_err(|_| ServiceError::invalid_params("runId is not a valid id"))?;
        self.db
            .run_domain_op(query::approval_list_op(run_id))
            .await
            .map_err(ServiceError::from)
    }

    async fn approval_decide(
        &self,
        principal: &crate::ipc::ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let approval_id = params
            .get("approvalId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("approvalId is required"))
            .and_then(|s| {
                ApprovalId::parse(s)
                    .map_err(|_| ServiceError::invalid_params("approvalId is not a valid id"))
            })?;
        let decision = str_field(params, "decision")?;
        if decision != "approve" && decision != "deny" {
            return Err(ServiceError::invalid_params(
                "decision must be \"approve\" or \"deny\"",
            ));
        }
        let reason = str_field(params, "reason")?;
        // The rationale is an audit fact (R59): an empty one is refused at
        // the boundary rather than silently persisted as a record that a
        // rationale exists when none does.
        if reason.trim().is_empty() {
            return Err(ServiceError::invalid_params("reason must not be empty"));
        }

        // Deserialize through the canonical serde tokens rather than a
        // hand-rolled string match, so `DecidedBy`'s rename attributes stay
        // the single authority (R34 review S-3). Absent defaults to Model.
        let decided_by = match params.get("decidedBy") {
            None | Some(Value::Null) => batman_protocol::DecidedBy::Model,
            Some(value) => serde_json::from_value::<batman_protocol::DecidedBy>(value.clone())
                .map_err(|_| {
                    ServiceError::invalid_params(format!(
                        "decidedBy must be \"human\" or \"model\"; got {value}"
                    ))
                })?,
        };

        let outcome = self
            .approval
            .decide(
                approval_id,
                &principal.instance_id,
                &decision,
                &reason,
                decided_by,
            )
            .await
            .map_err(ServiceError::from)?;

        Ok(json!({
            "approvalId": approval_id.to_string(),
            "outcome": match outcome {
                crate::approval::DecideOutcome::Decided => "decided",
                crate::approval::DecideOutcome::DecidedCallbackFailed => "decidedCallbackFailed",
                crate::approval::DecideOutcome::AlreadyDecided => "alreadyDecided",
            },
        }))
    }

    /// `policy/violation/decide`: resolves a mid-run nested-worker policy
    /// violation as `"release"` or `"cancel"`, restricted to the owning
    /// `ompExtension` client (the violation's task's
    /// `owner_client_instance_id`) by
    /// [`crate::policy::ViolationService::decide_and_release_status`].
    ///
    /// The result carries `"quarantineCleared": bool` only for a newly
    /// decided (`outcome: "decided"`) `"release"`: `true` if this call
    /// actually cleared `Run.flags.policyQuarantined`, `false` if a
    /// *different*, still-unresolved violation on the run kept it held
    /// (R75) -- the run then still refuses `message/send`/`workspace/apply`
    /// until that other violation is also decided. The field is absent for
    /// a `"cancel"` resolution (the run is ending; quarantine state is
    /// moot) and for an idempotent `"alreadyDecided"` replay (no clearing
    /// decision was made this call) -- both cases where computing a value
    /// would imply information this call does not have.
    async fn policy_violation_decide(
        &self,
        principal: &crate::ipc::ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let violation_id = params
            .get("violationId")
            .and_then(Value::as_str)
            .ok_or_else(|| ServiceError::invalid_params("violationId is required"))
            .and_then(|s| {
                batman_protocol::PolicyViolationId::parse(s)
                    .map_err(|_| ServiceError::invalid_params("violationId is not a valid id"))
            })?;
        let resolution = str_field(params, "resolution")?;

        let (outcome, quarantine_cleared) = self
            .violation
            .decide_and_release_status(violation_id, &principal.instance_id, &resolution)
            .await
            .map_err(ServiceError::from)?;

        let mut response = json!({
            "violationId": violation_id.to_string(),
            "outcome": match outcome {
                crate::policy::DecideOutcome::Decided => "decided",
                crate::policy::DecideOutcome::AlreadyDecided => "alreadyDecided",
            },
        });
        if let Some(cleared) = quarantine_cleared {
            response["quarantineCleared"] = json!(cleared);
        }
        Ok(response)
    }

    /// `policy/violation/list`: the discovery surface for which violation
    /// still holds a quarantine (R80). Project-wide like the other read
    /// ops (`run/list`, `approval/list`) -- the documented read-side
    /// policy; optionally narrowed to one run. An undecided row
    /// (`resolution` null) on a quarantined run is the holder.
    async fn policy_violation_list(&self, params: &Value) -> Result<Value, ServiceError> {
        let run_id = params
            .get("runId")
            .and_then(Value::as_str)
            .map(RunId::parse)
            .transpose()
            .map_err(|_| ServiceError::invalid_params("runId is not a valid id"))?;
        self.db
            .run_domain_op(query::policy_violation_list_op(run_id, self.project_id))
            .await
            .map_err(ServiceError::from)
    }

    /// Rebinds a task from a disconnected OMP client instance to the
    /// connected `principal`, only when task ID and monotonic OMP revision
    /// match -- enforced inside the guarded write itself (R74); journals
    /// the old/new owner IDs. The stored revision is not changed, so
    /// reclaim stays idempotent across retries and restarts.
    async fn reconcile_omp(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let task_id = parse_task_id(params.get("taskId"))?;
        let revision = u64_field(params, "revision")?;

        // The revision match is arbitrated inside `reconcile_ownership`'s
        // guarded write (R74): a caller-side pre-check read in a separate
        // round trip could be interleaved with a write to the same task.

        let new_owner = principal.instance_id.clone();
        let project_id = self.project_id;
        let mut sequence = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.reconcile_ownership(task_id, &new_owner, revision)
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await
            .map_err(ServiceError::from)?;
        self.broadcast(&mut sequence);

        Ok(json!({
            "taskId": task_id.to_string(),
            "newOwnerClientInstanceId": principal.instance_id,
            "sequence": sequence["sequence"],
        }))
    }

    // ---------------------------------------------------- coordination

    /// `coordination/child/list`: pending child-worker requests. A
    /// `workerMcp` principal sees only its own scoped run's request;
    /// `ompExtension`/`display` see every pending request in the project.
    async fn coordination_child_list(
        &self,
        principal: &crate::ipc::ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let scoped_run_id = principal.scoped_run_id;
        let requested_run_id = params
            .get("runId")
            .and_then(Value::as_str)
            .map(RunId::parse)
            .transpose()
            .map_err(|_| ServiceError::invalid_params("runId is not a valid id"))?;
        let run_filter = match scoped_run_id {
            Some(run_id) => Some(run_id),
            None => requested_run_id,
        };

        self.db
            .run_domain_op(Box::new(move |conn| {
                let (sql, params): (&str, Vec<String>) = match run_filter {
                    Some(run_id) => (
                        "SELECT sequence, event_json FROM events
                         WHERE run_id = ?1 AND event_json LIKE '%\"childEvent\"%'
                         ORDER BY sequence",
                        vec![run_id.to_string()],
                    ),
                    None => (
                        "SELECT sequence, event_json FROM events
                         WHERE event_json LIKE '%\"childEvent\"%' ORDER BY sequence",
                        vec![],
                    ),
                };
                let mut stmt = conn.prepare(sql)?;
                let rows: Vec<Value> = stmt
                    .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                        row.get::<_, String>(1)
                    })?
                    .filter_map(|r| r.ok())
                    .filter_map(|json_text| serde_json::from_str::<Value>(&json_text).ok())
                    .collect();
                Ok(json!({ "requests": rows }))
            }))
            .await
            .map_err(ServiceError::from)
    }

    /// `coordination/child/decide`: OMP's answer to a prior
    /// `coordination/requestChild`. Acceptance supplies the OMP-created
    /// child ids and returns the parent run to `working`; denial records
    /// a reason and also returns the parent to `working`.
    ///
    /// `principal` arbitrates ownership the same way `run/submit` does
    /// (R77), against `decide_child`'s own guarded write. Every caller of
    /// this method is `ompExtension` (`coordination/child/decide` is not
    /// in `workerMcp`'s `allowed_methods` -- `coordination/requestChild`
    /// is the distinct, worker-scoped method that raises the request this
    /// answers), so `principal.instance_id` is always the connected
    /// extension instance being arbitrated, never a scoped worker.
    async fn coordination_child_decide(
        &self,
        principal: &ClientPrincipal,
        params: &Value,
    ) -> Result<Value, ServiceError> {
        let parent_run_id = parse_run_id(params.get("parentRunId"))?;
        let decision = str_field(params, "decision")?;
        let project_id = self.project_id;
        let principal_instance_id = principal.instance_id.clone();

        match decision.as_str() {
            "accept" => {
                let child_task_id = parse_task_id(params.get("childTaskId"))?;
                let child_worker_id = parse_worker_id(params.get("childWorkerId"))?;
                let child_run_id = parse_run_id(params.get("childRunId"))?;
                let mut result = self
                    .db
                    .run_domain_op(Box::new(move |conn| {
                        let mut repo = DomainRepository::new(conn, project_id);
                        repo.decide_child(
                            parent_run_id,
                            crate::domain::ChildDecision::Accept {
                                child_task_id,
                                child_worker_id,
                                child_run_id,
                            },
                            Some(&principal_instance_id),
                        )
                        .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
                    }))
                    .await
                    .map_err(ServiceError::from)?;
                self.broadcast(&mut result);
                Ok(result)
            }
            "deny" => {
                let reason = str_field(params, "reason")?;
                let mut result = self
                    .db
                    .run_domain_op(Box::new(move |conn| {
                        let mut repo = DomainRepository::new(conn, project_id);
                        repo.decide_child(
                            parent_run_id,
                            crate::domain::ChildDecision::Deny { reason },
                            Some(&principal_instance_id),
                        )
                        .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
                    }))
                    .await
                    .map_err(ServiceError::from)?;
                self.broadcast(&mut result);
                Ok(result)
            }
            other => Err(ServiceError::invalid_params(format!(
                "decision must be \"accept\" or \"deny\", got {other:?}"
            ))),
        }
    }
}

// ----------------------------------------------------------------- parsing

fn str_field(params: &Value, field: &'static str) -> Result<String, ServiceError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ServiceError::invalid_params(format!("{field} is required")))
}

fn u64_field(params: &Value, field: &'static str) -> Result<u64, ServiceError> {
    params.get(field).and_then(Value::as_u64).ok_or_else(|| {
        ServiceError::invalid_params(format!(
            "{field} is required and must be a non-negative integer"
        ))
    })
}

fn parse_task_id(value: Option<&Value>) -> Result<TaskId, ServiceError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("taskId is required"))
        .and_then(|s| {
            TaskId::parse(s).map_err(|_| ServiceError::invalid_params("taskId is not a valid id"))
        })
}

fn parse_or_new_task_id(value: Option<&Value>) -> Result<TaskId, ServiceError> {
    match value.and_then(Value::as_str) {
        Some(s) => {
            TaskId::parse(s).map_err(|_| ServiceError::invalid_params("taskId is not a valid id"))
        }
        None => Ok(TaskId::new()),
    }
}

fn parse_worker_id(value: Option<&Value>) -> Result<WorkerId, ServiceError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("workerId is required"))
        .and_then(|s| {
            WorkerId::parse(s)
                .map_err(|_| ServiceError::invalid_params("workerId is not a valid id"))
        })
}

fn parse_run_id(value: Option<&Value>) -> Result<RunId, ServiceError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("runId is required"))
        .and_then(|s| {
            RunId::parse(s).map_err(|_| ServiceError::invalid_params("runId is not a valid id"))
        })
}

fn parse_message_kind(value: Option<&Value>) -> Result<MessageKind, ServiceError> {
    let raw = value
        .and_then(Value::as_str)
        .ok_or_else(|| ServiceError::invalid_params("kind is required"))?;
    match raw {
        "assign" => Ok(MessageKind::Assign),
        "steer" => Ok(MessageKind::Steer),
        "followUp" => Ok(MessageKind::FollowUp),
        "question" => Ok(MessageKind::Question),
        "answer" => Ok(MessageKind::Answer),
        "peerMessage" => Ok(MessageKind::PeerMessage),
        "approvalDecision" => Ok(MessageKind::ApprovalDecision),
        "cancel" => Ok(MessageKind::Cancel),
        "shutdown" => Ok(MessageKind::Shutdown),
        other => Err(ServiceError::invalid_params(format!(
            "unknown message kind {other:?}"
        ))),
    }
}

/// Suppresses an unused-import warning for a type referenced only through
/// generic bounds in this module's signatures.
#[allow(unused_imports)]
use ApprovalRequest as _ApprovalRequest;
#[allow(unused_imports)]
use RunSpec as _RunSpec;
