//! The adapter registry: implements [`crate::service::RunDriver`] by
//! resolving a run's immutable worker profile, gating start on
//! conformance-derived effective capabilities through an injected
//! [`AdapterAuthorization`], constructing the matching [`Adapter`], and
//! owning it for the run's lifetime in a run-indexed table.
//!
//! # Scope boundary (documented, not silently omitted)
//! [`RunDriverContext::prompt`] carries the task's initial content (closed
//! as part of the M2/M3 gap-closure milestone): `run_one` passes
//! `ctx.prompt.clone().unwrap_or_default()` into [`StartSpec::prompt`], so
//! `run/submit`'s optional `RunSpec::prompt` now reaches the adapter at
//! start time. Delivering a *later* follow-up to an already-running
//! adapter instance is a separate seam (`RunDriver::send_follow_up`,
//! implemented below and invoked from `OrchestrationService::message_send`)
//! rather than a second `start()` call. Claude/Codex/Copilot adapters
//! constructed here now receive worker-coordination MCP config too
//! (closed alongside the prompt gap): `AdapterRegistry::new` accepts an
//! `Option<AdapterMcpConfig>`, built by `lifecycle::serve()` from a
//! resolved `crewd` binary path, state dir, and repository root, and
//! threaded into every Claude/Codex/Copilot adapter this registry
//! constructs. OMP-RPC's in-process host-tool bridge instead needs a
//! `CoordinationBroker`, supplied after construction via
//! [`AdapterRegistry::set_broker`] (see that method's own doc comment
//! for why it cannot be a constructor argument).
//!
//! `mode: "tui"` for `claude` now constructs a real
//! `TuiAdapter<ClaudeTuiVendor>` (WP13) rather than the typed refusal
//! every reserved kind still gets otherwise: [`AdapterRegistry::set_tui_support`]
//! supplies the [`super::tui::TuiSupport`] bundle a `TuiAdapter` needs
//! beyond its own vendor impl, for exactly the same "only available
//! after IPC bind" reason `set_broker` exists (see that struct's own doc
//! comment).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use crew_protocol::{DisplayPlacement, RunId, TaskId, WorkerId};
use tokio::sync::oneshot;

use super::activity::ActivityClock;
use super::capability::{AdapterCapabilities, NestedCapability};
use super::event_sink::{AdapterEventSink, DomainAdapterEventSink, SettlementSink};
use super::mcp_config::AdapterMcpConfig;
use super::profile::{AdapterKind, StartupOptions, WorkerProfile};
use super::run_lifecycle::RunLifecycleSink;
use super::r#trait::{Adapter, AdapterMessage, StartSpec, VendorSessionRef};
use super::tui::{ClaudeTuiVendor, Cursor, ResumeContext, TuiAdapter, TuiSupport, TuiVendor};
use crate::adapter::CancelScope;
use crate::config::crew::{AdapterConfig, AdapterMode as CrewAdapterMode, PermissionMode};
use crate::conformance;
use crate::coordination::CoordinationBroker;
use crate::db::DatabaseHandle;
use crate::display::PaneCoordinator;
use crate::domain::DomainRepository;
use crate::service::{AdapterFuture as RunDriverFuture, RunDriver, RunDriverContext};
/// adapter, given `effective_capabilities` -- always the conformance-
/// filtered set, never the adapter's raw declared claims. Production
/// construction of [`AdapterRegistry`] requires a real implementation;
/// tests inject an allow/deny fixture (see [`FixtureAuthorization`]).
pub trait AdapterAuthorization: Send + Sync {
    /// `policy` is the run's own resolved [`crate::config::RuntimePolicy`]
    /// -- the startup policy re-merged with that run's `policyOverrides`.
    /// `None` means "use the authorizer's own startup policy", which is the
    /// behavior every run without overrides and every test path relies on.
    ///
    /// # Errors
    /// Returns a human-readable denial reason. The run is never started
    /// when this returns `Err`.
    fn authorize(
        &self,
        profile: &WorkerProfile,
        effective_capabilities: &AdapterCapabilities,
        policy: Option<&crate::config::RuntimePolicy>,
    ) -> Result<(), String>;

    /// Releases a previously-booked concurrency slot when a run settles.
    /// Called exactly once from every settlement path (success, error,
    /// cancellation). Safe to call even if no slot was booked.
    fn release(&self);
}

/// A deterministic allow/deny fixture for tests. Production callers must
/// supply a real policy, per the plan's "do not ship a permissive
/// production authorization implementation."
pub struct FixtureAuthorization {
    pub allow: bool,
}

impl AdapterAuthorization for FixtureAuthorization {
    fn authorize(
        &self,
        _profile: &WorkerProfile,
        _effective_capabilities: &AdapterCapabilities,
        _policy: Option<&crate::config::RuntimePolicy>,
    ) -> Result<(), String> {
        if self.allow {
            Ok(())
        } else {
            Err("denied by fixture authorization".to_string())
        }
    }

    fn release(&self) {
        // No-op: fixture authorization has no concurrency slots.
    }
}

/// Why [`AdapterRegistry::start`] could not start (or continue driving)
/// a run. Always converted to a plain `String` at the [`RunDriver`]
/// boundary (that trait's own contract), but kept structured internally
/// so tests can assert on the exact rejection reason.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("run {0} already has a running adapter instance")]
    DuplicateStart(RunId),
    #[error("worker has no resolved profile snapshot")]
    NoResolvedProfile,
    #[error("failed to read the resolved worker profile: {0}")]
    ProfileUnreadable(String),
    #[error("authorization denied: {0}")]
    AuthorizationDenied(String),
    #[error("no adapter is currently running for run {0}")]
    NoRunningAdapter(RunId),
    /// `mode: "tui"` was requested for an adapter kind with no
    /// [`crate::adapter::tui::TuiVendor`] implementation yet -- a typed
    /// refusal, never a silent fallback to the headless adapter (the
    /// pre-flight ruling this registry follows for every adapter kind
    /// until its TUI vendor impl lands).
    #[error("adapter {0} has no TUI-mode implementation yet; mode: \"tui\" is unavailable for it")]
    TuiModeUnavailable(String),
    /// [`AdapterRegistry::resume_run`] was called before the caller ever
    /// supplied [`ResumeSupport`] via
    /// [`AdapterRegistry::set_resume_support`]. Fail closed: a resume
    /// without its own journal/sink wiring could only run unsupervised.
    #[error("resume support was never supplied (set_resume_support); cannot resume run {0}")]
    ResumeUnsupported(RunId),
}

impl From<RegistryError> for String {
    fn from(err: RegistryError) -> Self {
        err.to_string()
    }
}

/// Everything [`AdapterRegistry::resume_run`] needs that, exactly like
/// the [`CoordinationBroker`] and [`TuiSupport`] bundles, only exists
/// once the IPC server has bound: the journal handle and project id the
/// resumed run's sink stack writes through, the mid-run policy-violation
/// service that sink stack consults, and the live event broadcast every
/// journaled mutation must fan out on. Supplied once via
/// [`AdapterRegistry::set_resume_support`]; a caller that never resumes
/// (every test but the resume ones) simply never calls it.
pub struct ResumeSupport {
    pub db: Arc<DatabaseHandle>,
    pub project_id: crew_protocol::ProjectId,
    pub violation_service: Arc<crate::policy::ViolationService>,
    pub events_tx: tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
}

/// Implements [`RunDriver`] against the four real worker adapters.
///
/// Always constructed behind an `Arc` in practice (exactly like every
/// other `RunDriver`, per `OrchestrationService`'s own
/// `run_driver: Option<Arc<dyn RunDriver>>` field) -- `Self::start`'s
/// `'static` future clones every field it needs out of `&self` rather
/// than borrowing it, so this requirement is never actually load-bearing
/// for soundness, only for the instance to still exist by the time a
/// caller awaits the future.
pub struct AdapterRegistry {
    authorization: Arc<dyn AdapterAuthorization>,
    /// The working directory every supervised vendor process is launched
    /// in. One registry instance serves one repository, exactly like one
    /// `crewd` daemon does.
    repo_root: PathBuf,
    /// Worker-coordination MCP launch config, given to every Claude/Codex/
    /// Copilot adapter this registry constructs so their supervised vendor
    /// processes can reach the `crew` coordination MCP server. `None`
    /// for callers (chiefly tests) that never asked for worker MCP tools.
    mcp: Option<AdapterMcpConfig>,
    /// The [`CoordinationBroker`] OMP-RPC adapters answer their in-process
    /// `host_tool_call` bridge against. Set once, after construction, via
    /// [`Self::set_broker`] -- unlike `mcp`, the real broker instance is
    /// owned by [`crate::ipc::Server`] and only exists after `Server::bind`
    /// returns, which happens *after* this registry must already be handed
    /// to [`crate::ipc::ServerConfig::run_driver`]. `None` until set (or
    /// permanently, for callers that never call the setter): OMP-RPC
    /// adapters constructed in that window get no broker, matching their
    /// existing `broker: None` behavior exactly.
    broker: Mutex<Option<Arc<CoordinationBroker>>>,
    /// TUI-mode support (see this module's own doc comment and
    /// [`super::tui::TuiSupport`]'s). `None` until [`Self::set_tui_support`]
    /// is called (or permanently, for callers -- chiefly tests -- that
    /// never call it): every reserved kind's `mode: "tui"` gets the same
    /// typed refusal it always has in that window.
    tui: Mutex<Option<Arc<TuiSupport>>>,
    /// The [`ResumeSupport`] bundle [`Self::resume_run`] drives a
    /// continuation through. `None` until [`Self::set_resume_support`] is
    /// called (or permanently, for callers that never resume); `None`
    /// makes every `resume_run` a typed [`RegistryError::ResumeUnsupported`]
    /// refusal, never a silently unwired resume.
    resume_support: Mutex<Option<Arc<ResumeSupport>>>,
    running: Arc<Mutex<HashMap<RunId, Arc<dyn Adapter>>>>,
    /// Org security patterns for redaction.
    org_security_patterns: Vec<String>,
    /// Per-run liveness clocks (WP19): touched by every event flowing
    /// through each run's [`RunLifecycleSink::wrap`], read by lifecycle's
    /// timeout sweep. Defaults to an empty clock -- a caller that never
    /// calls [`Self::set_activity_clock`] (chiefly tests) simply never
    /// has timeouts to sweep.
    activity: Mutex<Option<Arc<ActivityClock>>>,
}

impl AdapterRegistry {
    #[must_use]
    pub fn new(
        authorization: Arc<dyn AdapterAuthorization>,
        repo_root: PathBuf,
        mcp: Option<AdapterMcpConfig>,
        org_security_patterns: Vec<String>,
    ) -> Self {
        Self {
            authorization,
            repo_root,
            mcp,
            org_security_patterns,
            broker: Mutex::new(None),
            tui: Mutex::new(None),
            resume_support: Mutex::new(None),
            activity: Mutex::new(None),
            running: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Supplies the real [`CoordinationBroker`] for this registry's
    /// OMP-RPC adapters' in-process host-tool bridge, once the caller
    /// field's own doc comment for why this cannot be a constructor
    /// argument.
    pub fn set_broker(&self, broker: Arc<CoordinationBroker>) {
        *self.broker.lock() = Some(broker);
    }

    /// Supplies the [`TuiSupport`] bundle every real `TuiVendor`
    /// dispatch in `build_adapter` needs. A setter rather than a
    /// constructor argument for the same reason `mcp` is *not* one here
    /// but `set_broker`'s target is: every field `TuiSupport` wraps
    /// (display registry, `crewd` path, state dir) is already available
    /// at `lifecycle::serve()`'s `AdapterRegistry::new` call site, but
    /// keeping this a post-construction setter -- exactly like
    /// `set_broker` -- means a caller that never wants `mode: "tui"`
    /// reachable at all (every test but the ones that opt in) simply
    /// never calls it, and every reserved kind keeps the typed refusal it
    /// always had.
    pub fn set_tui_support(&self, tui: Arc<TuiSupport>) {
        *self.tui.lock() = Some(tui);
    }

    /// Supplies the [`ResumeSupport`] bundle [`Self::resume_run`] needs.
    /// A post-construction setter -- exactly like `set_broker`/
    /// `set_tui_support`, for exactly the same reason: the server-owned
    /// violation service and event broadcast only exist after
    /// `Server::bind` returns, which happens after this registry must
    /// already be handed to [`crate::ipc::ServerConfig::run_driver`].
    pub fn set_resume_support(&self, support: Arc<ResumeSupport>) {
        *self.resume_support.lock() = Some(support);
    }

    /// Supplies the shared [`ActivityClock`] every run's lifecycle sink
    /// touches and lifecycle's timeout sweep reads. A post-construction
    /// setter -- exactly like `set_broker` -- because the clock must be
    /// the *same* instance the sweep task is handed, and that task is
    /// spawned by `lifecycle::serve` after this registry is already
    /// running. Unset (tests) means an empty clock: sinks touch it, the
    /// sweep has nothing to read.
    pub fn set_activity_clock(&self, activity: Arc<ActivityClock>) {
        *self.activity.lock() = Some(activity);
    }

    /// The clock to hand a run's lifecycle sink: the shared instance when
    /// [`Self::set_activity_clock`] was called, else a fresh empty one
    /// whose touches nothing ever reads.
    #[must_use]
    pub fn activity_clock(&self) -> Arc<ActivityClock> {
        self.activity
            .lock()
            .clone()
            .unwrap_or_else(|| Arc::new(ActivityClock::new()))
    }

    /// The adapter instance currently running for `run_id`, if any --
    /// exposed for tests and for the message-forwarding seam this
    /// module's own doc comment names as a follow-up.
    #[must_use]
    pub fn running_adapter(&self, run_id: RunId) -> Option<Arc<dyn Adapter>> {
        self.running.lock().get(&run_id).cloned()
    }

    /// How many adapters this registry is currently driving. Exposed for
    /// tests asserting an instance was actually inserted/removed.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.running.lock().len()
    }
    /// Resumes `run_id` on a previously-established vendor session: the
    /// first production caller of [`Adapter::resume`].
    ///
    /// This is deliberately an inherent method, *not* a [`RunDriver`]
    /// trait method: `RunDriver` is the run/submit seam (its only other
    /// implementation is the tests' [`FakeRunDriver`], and nothing in
    /// that seam consumes a resume), while resume is the recovery-driven
    /// continuation of one specific run -- its caller (WP15's boot sweep)
    /// already holds this registry concretely. Hanging it anywhere in the
    /// orchestration service instead would re-route it through the
    /// submit-time state machine, but a resume is not a submission: it
    /// creates no run row, takes no prompt, and continues the same run.
    ///
    /// Eligibility is the caller's judgment (WP15 checks vendor session
    /// presence and adapter availability before calling); this method
    /// re-runs the same policy/authorization/availability pre-flight a
    /// fresh start gets -- authorization is per-spawn, so a resumed run
    /// books its concurrency slot exactly like a new one -- then builds a
    /// fresh adapter instance for the run, hands it the stored cursor via
    /// its construction-time [`ResumeContext`], and calls `resume`.
    pub async fn resume_run(
        &self,
        run_id: RunId,
        session: VendorSessionRef,
        cursor: Option<Cursor>,
    ) -> Result<(), String> {
        // Reserve the run-id slot atomically with the duplicate check,
        // exactly like `Self::start`: a concurrent resume/start for the
        // same run must be rejected as a duplicate, never raced into two
        // live adapters.
        {
            let mut guard = self.running.lock();
            if guard.contains_key(&run_id) {
                return Err(RegistryError::DuplicateStart(run_id).into());
            }
            guard.insert(run_id, build_placeholder_adapter());
        }

        let result = self.resume_one(run_id, session, cursor).await;
        match result {
            Ok((adapter, settled)) => {
                self.running.lock().insert(run_id, adapter);
                let running_for_watcher = Arc::clone(&self.running);
                let authorization_for_watcher = Arc::clone(&self.authorization);
                // A resumed TUI run owns its own pane lifecycle through
                // its adapter, same as a fresh one. `resume_run` carries
                // no `DisplaySelection` -- recovery callers do not resolve
                // displays -- so there is nothing to journal a placeholder
                // detach from either way; `watch_settlement`'s `None`
                // display skips that path entirely.
                let display: Option<crew_protocol::DisplaySelection> = None;
                let Some(support) = self.resume_support.lock().clone() else {
                    unreachable!("resume_one cannot succeed without resume support");
                };
                tokio::spawn(watch_settlement(
                    settled,
                    running_for_watcher,
                    authorization_for_watcher,
                    display,
                    Arc::clone(&support.db),
                    support.project_id,
                    run_id,
                ));
                Ok(())
            }
            Err(err) => {
                // The reservation must not leak on any failure path -- a
                // rejected resume leaves the run resumable again.
                self.running.lock().remove(&run_id);
                Err(err)
            }
        }
    }

    /// The body of [`Self::resume_run`], between slot reservation and
    /// insertion: resolve the run's worker profile, gate it, build the
    /// sink stack and the adapter, and hand the session to `resume`.
    async fn resume_one(
        &self,
        run_id: RunId,
        session: VendorSessionRef,
        cursor: Option<Cursor>,
    ) -> Result<(Arc<dyn Adapter>, oneshot::Receiver<()>), String> {
        let Some(support) = self.resume_support.lock().clone() else {
            return Err(RegistryError::ResumeUnsupported(run_id).into());
        };
        let (task_id, worker_id) = {
            let run_id_string = run_id.to_string();
            let value = support
                .db
                .run_domain_op(Box::new(move |conn| {
                    let row: (String, String) = conn.query_row(
                        "SELECT task_id, worker_id FROM runs WHERE run_id = ?1",
                        [&run_id_string],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    Ok(serde_json::json!({ "task_id": row.0, "worker_id": row.1 }))
                }))
                .await
                .map_err(|err| format!("run {run_id} is unreadable: {err}"))?;
            let parse_id = |key: &str| -> Result<String, String> {
                value
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| format!("run {run_id} has no {key}"))
            };
            let task_id = parse_id("task_id")?;
            let worker_id = parse_id("worker_id")?;
            (
                TaskId::parse(&task_id).map_err(|e| format!("run {run_id}: bad task id: {e}"))?,
                WorkerId::parse(&worker_id)
                    .map_err(|e| format!("run {run_id}: bad worker id: {e}"))?,
            )
        };
        let profile = resolve_worker_profile(&support.db, support.project_id, worker_id)
            .await
            .map_err(String::from)?;
        // No run-specific policy overrides are re-merged here: the run's
        // own policy was fixed at submit time; the authorizer falls back
        // to its startup policy (its documented behavior for `None`),
        // which is the same policy the boot sweep itself runs under.
        let effective_capabilities = gate_profile(&self.authorization, &profile, None).await?;
        // Fresh starts launch at the run's isolated workspace when one was
        // materialized; no such path is stored per-run, so a resumed
        // process lands back at the repository root (disclosed WP14 gap).
        let cwd = self.repo_root.as_path();
        let adapter = match build_adapter(
            &profile,
            cwd,
            run_id,
            task_id,
            worker_id,
            self.mcp.clone(),
            self.broker.lock().clone(),
            self.tui.lock().clone(),
            Arc::clone(&support.db),
            support.project_id,
            support.events_tx.clone(),
            None,
            cursor,
        ) {
            Ok(adapter) => adapter,
            Err(err) => {
                self.authorization.release();
                return Err(err.into());
            }
        };
        // The identical fail-closed sink stack a fresh start gets:
        // redaction before durability (invariant 4), lifecycle edges from
        // journaled evidence, and settlement signalled only after the
        // terminal edge committed.
        let sink = match DomainAdapterEventSink::new(
            Arc::clone(&support.db),
            support.project_id,
            support.events_tx.clone(),
            self.org_security_patterns.clone(),
            effective_capabilities.nested != NestedCapability::Managed,
            Arc::clone(&support.violation_service),
            // Resume support carries no workspace context; a resumed run
            // is treated as shared (the conservative side for the WP20
            // write-violation detector).
            false,
        ) {
            Ok(sink) => Arc::new(sink) as Arc<dyn AdapterEventSink>,
            Err(err) => {
                self.authorization.release();
                return Err(format!("org security patterns failed to compile: {err}"));
            }
        };
        let sink = RunLifecycleSink::wrap(
            sink,
            Arc::clone(&support.db),
            support.project_id,
            support.events_tx.clone(),
            run_id,
            self.activity_clock(),
        );
        let (sink, settled) = SettlementSink::wrap(sink);
        if let Err(err) = adapter.resume(session, sink).await {
            self.authorization.release();
            return Err(err.to_string());
        }
        Ok((adapter, settled))
    }
}

impl RunDriver for AdapterRegistry {
    fn active_run_count(&self) -> usize {
        self.running_count()
    }

    fn start(&self, ctx: RunDriverContext) -> RunDriverFuture<'static, Result<(), String>> {
        let authorization = Arc::clone(&self.authorization);
        let repo_root = self.repo_root.clone();
        let mcp = self.mcp.clone();
        let broker = self.broker.lock().clone();
        let tui = self.tui.lock().clone();
        let org_security_patterns = self.org_security_patterns.clone();
        let running = Arc::clone(&self.running);

        Box::pin(async move {
            // Reserve the run-id slot atomically with the duplicate
            // check: nothing between "is it already running" and
            // "mark it running" can race a second concurrent `start`
            // for the same run past this point, since both hold the
            // same lock for the whole check-then-insert.
            {
                let mut guard = running.lock();
                if guard.contains_key(&ctx.run_id) {
                    return Err(RegistryError::DuplicateStart(ctx.run_id).into());
                }
                // A placeholder; overwritten with the real adapter once
                // constructed below. Never observable from outside this
                // function: readers only see it as "already running",
                // exactly the state a duplicate-start rejection wants.
                guard.insert(ctx.run_id, build_placeholder_adapter());
            }

            let run_id = ctx.run_id;
            match run_one(
                &ctx,
                &authorization,
                &repo_root,
                mcp,
                broker,
                tui,
                org_security_patterns,
            )
            .await
            {
                Ok((adapter, settled, pane_lifecycle_owned_by_adapter)) => {
                    running.lock().insert(run_id, adapter);
                    let running_for_watcher = Arc::clone(&running);
                    let authorization_for_watcher = Arc::clone(&authorization);
                    // A TUI-mode run's own `TuiAdapter` already attaches
                    // and detaches its pane for real, through the
                    // `PaneCoordinator` built in `build_adapter` --
                    // `watch_settlement` must not *also* journal the
                    // placeholder-pane `DisplayPaneDetached` below for
                    // it, or the run would get two (rider: collapse the
                    // double detach now that a TUI run is reachable).
                    let display = if pane_lifecycle_owned_by_adapter {
                        None
                    } else {
                        ctx.display.clone()
                    };
                    let db = Arc::clone(&ctx.db);
                    let project_id = ctx.project_id;
                    tokio::spawn(watch_settlement(
                        settled,
                        running_for_watcher,
                        authorization_for_watcher,
                        display,
                        db,
                        project_id,
                        run_id,
                    ));
                    Ok(())
                }
                Err(err) => {
                    // The reservation above must not leak on any failure
                    // path -- a rejected/failed start must be startable
                    // again.
                    running.lock().remove(&run_id);
                    Err(err)
                }
            }
        })
    }

    fn send_follow_up(
        &self,
        run_id: RunId,
        _task_id: TaskId,
        _worker_id: WorkerId,
        prompt: String,
        kind: crew_protocol::MessageKind,
    ) -> RunDriverFuture<'static, Result<(), String>> {
        let running = Arc::clone(&self.running);

        Box::pin(async move {
            let adapter = running.lock().get(&run_id).cloned().ok_or_else(|| {
                <RegistryError as Into<String>>::into(RegistryError::NoRunningAdapter(run_id))
            })?;

            use crew_protocol::MessageKind;
            let message = match kind {
                // The one kind with dedicated redirect semantics on the
                // adapters that support it (TUI interrupt-then-compose,
                // Codex turn/steer); adapters without it refuse with
                // capability_unsupported rather than silently degrading.
                MessageKind::Steer => AdapterMessage::Steer { text: prompt },
                MessageKind::Answer => AdapterMessage::Answer { text: prompt },
                MessageKind::PeerMessage => AdapterMessage::PeerMessage { text: prompt },
                // Assign/question/approval-decision/cancel/shutdown all
                // carry follow-up delivery semantics at the vendor.
                MessageKind::Assign
                | MessageKind::FollowUp
                | MessageKind::Question
                | MessageKind::ApprovalDecision
                | MessageKind::Cancel
                | MessageKind::Shutdown => AdapterMessage::FollowUp { text: prompt },
            };

            adapter.send(message).await.map_err(|err| err.to_string())
        })
    }

    fn running_adapter(&self, run_id: RunId) -> Option<Arc<dyn Adapter>> {
        let running = Arc::clone(&self.running);
        running.lock().get(&run_id).cloned()
    }

    fn cancel_run(
        &self,
        run_id: RunId,
        scope: CancelScope,
    ) -> RunDriverFuture<'static, Result<crate::service::CancelOutcome, String>> {
        let running = Arc::clone(&self.running);

        Box::pin(async move {
            // An absent adapter is not a kill failure: the run settled or
            // never started one. Report the typed clean outcome so callers
            // never mistake it for a failed kill (R13).
            let Some(adapter) = running.lock().get(&run_id).cloned() else {
                return Ok(crate::service::CancelOutcome::NoRunningAdapter);
            };

            adapter
                .cancel(scope)
                .await
                .map(|()| crate::service::CancelOutcome::Cancelled)
                .map_err(|e| e.to_string())
        })
    }
}

/// Journals `DisplayPaneDetached` for a run that has settled.
///
/// Failures are swallowed: this runs on a detached watcher task after the
/// run is already over, so there is no caller to report to and a lost
/// pane record must never keep a finished adapter alive.
async fn emit_pane_detached(
    db: &Arc<crate::db::DatabaseHandle>,
    project_id: crew_protocol::ProjectId,
    run_id: RunId,
    backend: crew_protocol::DisplayBackend,
    placement: crew_protocol::DisplayPlacement,
) {
    let _ = db
        .run_domain_op(Box::new(move |conn| {
            let mut repo = crate::domain::DomainRepository::new(conn, project_id);
            repo.record_display_event(
                crew_protocol::RuntimeEventKind::DisplayPaneDetached,
                run_id,
                backend,
                placement,
                String::new(),
            )
            .map(|_| serde_json::Value::Null)
        }))
        .await;
}

/// Settles one run: waits for its `ProcessExited`, then evicts and
/// disposes its adapter, returns the concurrency slot, and journals the
/// display detach. The run's terminal `RunState` edge is already durable by
/// the time this watcher runs: `RunLifecycleSink` commits it as part of
/// journaling the very `ProcessExited` this signal is fired from, so the
/// slot is never released -- and no other run authorized -- while this run
/// still reads non-terminal. `Err` from `settled` means the run's sink was
/// dropped without any process exit ever being observed -- an adapter
/// task that died before emitting one (the terminal adapter itself now
/// settles via `cancel`'s synthetic `ProcessExited`, R95); that
/// path therefore leaves the run non-terminal until the boot recovery sweep.
/// Never release or journal a detach on that path: there is no settlement to
/// record, and a release without one would hand this run's slot to another.
async fn watch_settlement(
    settled: oneshot::Receiver<()>,
    running: Arc<Mutex<HashMap<RunId, Arc<dyn Adapter>>>>,
    authorization: Arc<dyn AdapterAuthorization>,
    display: Option<crew_protocol::DisplaySelection>,
    db: Arc<crate::db::DatabaseHandle>,
    project_id: crew_protocol::ProjectId,
    run_id: RunId,
) {
    if settled.await.is_err() {
        return;
    }
    let evicted = running.lock().remove(&run_id);
    if let Some(adapter) = evicted {
        let _ = adapter.dispose().await;
    }
    authorization.release();
    if let Some(selection) = display
        && let Some(backend) = selection.selected
    {
        emit_pane_detached(&db, project_id, run_id, backend, selection.placement).await;
    }
}

/// A never-started, immediately-idle placeholder occupying the run-id
/// reservation slot while the real adapter is constructed. Its `start`/
/// `resume`/`send`/etc. are never called; it exists only to make
/// `running.contains_key` true for the duration of construction.
fn build_placeholder_adapter() -> Arc<dyn Adapter> {
    Arc::new(super::OmpRpcAdapter::new(
        WorkerProfile {
            id: crate::adapter::ProfileId::new(),
            adapter: "ompRpc".to_string(),
            model: String::new(),
            permission_envelope: serde_json::json!({}),
            startup_options: StartupOptions::OmpRpc(super::OmpRpcStartupOptions::default()),
            environment_allowlist: Vec::new(),
            source: "registry-placeholder".to_string(),
        },
        super::OmpRpcAdapterOptions::default(),
        None,
    ))
}

async fn run_one(
    ctx: &RunDriverContext,
    authorization: &Arc<dyn AdapterAuthorization>,
    repo_root: &std::path::Path,
    mcp: Option<AdapterMcpConfig>,
    broker: Option<Arc<CoordinationBroker>>,
    tui: Option<Arc<TuiSupport>>,
    org_security_patterns: Vec<String>,
) -> Result<(Arc<dyn Adapter>, oneshot::Receiver<()>, bool), String> {
    let profile = resolve_profile(ctx).await.map_err(String::from)?;

    let effective_capabilities =
        gate_profile(authorization, &profile, ctx.policy.as_deref()).await?;
    // Use the workspace path from the context (isolated worktree or copy)
    // when available; fall back to the repository root.
    let cwd = ctx.workspace_path.as_deref().unwrap_or(repo_root);
    let pane_lifecycle_owned_by_adapter = pane_lifecycle_owned_by_adapter(&profile.startup_options);
    // The same DB lookups that decide fresh-start vs continuation also
    // carry the stored tailer position: a resumed TUI adapter must
    // re-tail from exactly where the pre-crash incarnation stopped, or
    // it re-journals events already committed (the failure
    // `Adapter::resume`'s cursor contract exists to prevent).
    let (stored_vendor_session_id, stored_cursor) =
        match stored_resume_state(&ctx.db, ctx.run_id).await {
            Ok(state) => state,
            Err(err) => {
                authorization.release();
                return Err(err);
            }
        };
    let adapter = match build_adapter(
        &profile,
        cwd,
        ctx.run_id,
        ctx.task_id,
        ctx.worker_id,
        mcp,
        broker,
        tui,
        Arc::clone(&ctx.db),
        ctx.project_id,
        ctx.events_tx.clone(),
        ctx.display.clone(),
        stored_cursor,
    ) {
        Ok(adapter) => adapter,
        Err(err) => {
            authorization.release();
            return Err(err.into());
        }
    };
    // Fail closed: a sink whose org redaction patterns do not compile
    // must never journal anything (invariant 4, R14).
    let sink = match DomainAdapterEventSink::new(
        Arc::clone(&ctx.db),
        ctx.project_id,
        ctx.events_tx.clone(),
        org_security_patterns,
        effective_capabilities.nested != NestedCapability::Managed,
        Arc::clone(&ctx.violation_service),
        ctx.workspace_path.is_some(),
    ) {
        Ok(sink) => Arc::new(sink) as Arc<dyn AdapterEventSink>,
        Err(err) => {
            authorization.release();
            return Err(format!("org security patterns failed to compile: {err}"));
        }
    };
    let sink = RunLifecycleSink::wrap(
        sink,
        Arc::clone(&ctx.db),
        ctx.project_id,
        ctx.events_tx.clone(),
        ctx.run_id,
        Arc::clone(&ctx.activity),
    );
    let (sink, settled) = SettlementSink::wrap(sink);
    if let Err(err) = adapter
        .start(
            StartSpec {
                run_id: ctx.run_id,
                task_id: ctx.task_id,
                worker_id: ctx.worker_id,
                prompt: ctx.prompt.clone().unwrap_or_default(),
                // A run whose row already carries a vendor session id is
                // a continuation of that same vendor session, never a
                // fresh start wearing new clothes (and never a retry --
                // retries create new runs; this is the same run). The
                // adapter decides what `Some` means for its own protocol;
                // every headless adapter already implemented this and the
                // TUI adapter now skips injection and resumes its launch.
                resume: stored_vendor_session_id.map(VendorSessionRef),
            },
            sink,
        )
        .await
    {
        authorization.release();
        return Err(err.to_string());
    }
    Ok((adapter, settled, pane_lifecycle_owned_by_adapter))
}

/// The resolved profile snapshot for one worker, read through the domain
/// repository. Shared by the fresh-start path (`run_one`, which reads ids
/// off its `RunDriverContext`) and resume (`resume_one`, which re-derives
/// them from the run row).
async fn resolve_worker_profile(
    db: &Arc<DatabaseHandle>,
    project_id: crew_protocol::ProjectId,
    worker_id: WorkerId,
) -> Result<WorkerProfile, RegistryError> {
    let project_id_for_query = project_id;
    let snapshot = db
        .run_domain_op(Box::new(move |conn| {
            let repo = DomainRepository::new(conn, project_id_for_query);
            let snapshot = repo.resolved_profile_snapshot(worker_id)?;
            Ok(serde_json::json!({ "snapshot": snapshot }))
        }))
        .await
        .map_err(|err| RegistryError::ProfileUnreadable(err.to_string()))?;
    let snapshot = snapshot
        .get("snapshot")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let Some(snapshot) = snapshot else {
        return Err(RegistryError::NoResolvedProfile);
    };
    serde_json::from_str(&snapshot).map_err(|err| RegistryError::ProfileUnreadable(err.to_string()))
}

/// The shared pre-flight every adapter-bearing entry point runs before
/// any process is spawned: conformance-derived effective capabilities,
/// the policy decision, and the vendor-CLI availability probe. Shared by
/// [`run_one`] (fresh start / resume-through-`StartSpec`) and
/// [`AdapterRegistry::resume_run`] so the two paths can never drift.
async fn gate_profile(
    authorization: &Arc<dyn AdapterAuthorization>,
    profile: &WorkerProfile,
    policy: Option<&crate::config::RuntimePolicy>,
) -> Result<AdapterCapabilities, String> {
    // Handle TerminalDegraded specially (it has no adapter kind)
    let effective_capabilities = if profile.adapter_kind().is_none() {
        // TerminalDegraded uses the terminal adapter with degraded capabilities
        // We need to extract the backend from the startup options
        if let StartupOptions::TerminalDegraded(opts) = &profile.startup_options() {
            super::terminal::TerminalAdapter::new(opts.backend.clone()).capabilities()
        } else {
            return Err("TerminalDegraded profile has no startup options".to_string());
        }
    } else {
        let Some(kind) = profile.adapter_kind() else {
            return Err("no adapter kind".to_string());
        };
        // Scope boundary (WP13, documented not silently omitted):
        // `run_fixture_conformance` dispatches by `AdapterKind` only --
        // it has no `mode` axis -- so a `mode: "tui"` run is authorized
        // against its *headless* fixture suite's effective capabilities
        // (e.g. Claude headless's `ApprovalsCapability::Controllable`),
        // even though the `TuiAdapter` actually constructed below
        // declares a materially different profile (`ProtocolKind::Terminal`,
        // `ApprovalsCapability::None`, `UsageCapability::None`, ...). Giving
        // this call a `mode` parameter means widening the closed
        // `AdapterKind`-keyed dispatch `conformance::run_fixture_conformance`/
        // `run_live_conformance`/`probe_availability` and the `crewd
        // conformance`/`adapters --json` CLI surfaces all share -- out of
        // scope for the WP that first makes `mode: "tui"` reachable at
        // all; flagged for a follow-up rather than fixed here.
        conformance::run_fixture_conformance(kind)
            .await
            .effective_capabilities
    };

    // Policy first: it is the cheaper, machine-independent decision, and
    // probing a vendor CLI for a run policy already forbids would spawn a
    // process to answer a question that no longer matters.
    authorization
        .authorize(profile, &effective_capabilities, policy)
        .map_err(RegistryError::AuthorizationDenied)
        .map_err(String::from)?;

    // Then availability: deny an unusable vendor CLI here rather than
    // letting `adapter.start()` fail after a process is spawned. The probe
    // is a version handshake only -- never a model call -- and is cached
    // for 60s, so repeated submits do not re-spawn the binary.
    //
    // Only a *disproof* denies: a skipped probe (the kill switch) was never
    // attempted, so it is not evidence the CLI is unusable.
    if let Some(kind) = profile.adapter_kind() {
        let availability = conformance::probe_availability(kind).await;
        if availability.disproved() {
            authorization.release();
            return Err(format!(
                "adapter {} is unavailable: {}",
                kind.wire_name(),
                availability.detail
            ));
        }
    }
    Ok(effective_capabilities)
}

/// The resume seam a run row carries: the `vendor_session_id` a prior
/// incarnation of this run already established (if any), and the
/// transcript-tailer position that incarnation reached (`None` when the
/// column is NULL -- nothing was ever durably consumed). A session turns
/// what would have been a fresh `StartSpec` into a continuation
/// (`StartSpec::resume`) -- resuming the same run through its same
/// vendor session, never a retry that would fabricate a new one.
///
/// Fails closed: an unreadable row is an error, not a fresh launch -- a
/// transient DB read failure must never silently turn a continuation of
/// a live vendor session into a second one.
async fn stored_resume_state(
    db: &Arc<DatabaseHandle>,
    run_id: RunId,
) -> Result<(Option<String>, Option<Cursor>), String> {
    let run_id_string = run_id.to_string();
    let value = db
        .run_domain_op(Box::new(move |conn| {
            let row: (Option<String>, Option<String>) = conn.query_row(
                "SELECT vendor_session_id, transcript_cursor FROM runs WHERE run_id = ?1",
                [&run_id_string],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok(serde_json::json!({
                "vendor_session_id": row.0,
                "transcript_cursor": row.1,
            }))
        }))
        .await
        .map_err(|err| format!("run {run_id} resume state is unreadable: {err}"))?;
    let vendor_session_id = value
        .get("vendor_session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    // The cursor column holds opaque JSON of a `Cursor` (migration 10).
    // A value we cannot parse means the row's resume position is not
    // honorable -- failing closed beats re-tailing from byte zero and
    // re-journaling already-committed events.
    let transcript_cursor =
        match value
            .get("transcript_cursor")
            .and_then(serde_json::Value::as_str)
        {
            Some(json) => Some(serde_json::from_str::<Cursor>(json).map_err(|err| {
                format!("run {run_id} has an unreadable transcript cursor: {err}")
            })?),
            None => None,
        };
    Ok((vendor_session_id, transcript_cursor))
}
async fn resolve_profile(ctx: &RunDriverContext) -> Result<WorkerProfile, RegistryError> {
    resolve_worker_profile(&ctx.db, ctx.project_id, ctx.worker_id).await
}

/// The [`super::profile::AdapterMode`] a startup-options variant
/// requested, or `None` for [`StartupOptions::TerminalDegraded`] (which
/// carries no mode field -- it wraps an arbitrary underlying harness
/// rather than one of the four reserved adapter kinds).
pub(crate) fn requested_mode(
    startup_options: &StartupOptions,
) -> Option<super::profile::AdapterMode> {
    match startup_options {
        StartupOptions::Claude(options) => Some(options.mode),
        StartupOptions::Codex(options) => Some(options.mode),
        StartupOptions::Copilot(options) => Some(options.mode),
        StartupOptions::OmpRpc(options) => Some(options.mode),
        StartupOptions::TerminalDegraded(_) => None,
    }
}

/// Whether the startup options select a mode whose owning adapter
/// journals its own real pane attach/detach pair -- today only Claude's
/// `mode: "tui"`, whose [`super::tui::TuiAdapter`] attaches through its
/// `PaneCoordinator`. Such a run must never also receive the submit-time
/// placeholder pane events `start_queued_run` journals for every other
/// backend-resolving run: pairing attach/detach consumers would see two
/// attaches against one detach. Kept beside [`requested_mode`] so
/// `run_one`'s watcher decision and `start_queued_run`'s placeholder skip
/// cannot drift apart.
pub(crate) fn pane_lifecycle_owned_by_adapter(startup_options: &StartupOptions) -> bool {
    requested_mode(startup_options) == Some(super::profile::AdapterMode::Tui)
        && startup_options.adapter_kind() == Some(super::AdapterKind::Claude)
}

/// The capabilities the given reserved adapter kind declares, read off a
/// throwaway adapter instance so each kind's `capabilities()` body remains
/// the single source of truth (no second, driftable capability literal
/// lives here). Construction is pure in-memory bookkeeping -- no process is
/// spawned and no vendor CLI probe runs -- which is exactly what the boot
/// recovery sweep's *eligibility* check needs: "does this run's adapter
/// even claim session resumption", decided before any resume is attempted.
/// The full availability/policy pre-flight still runs later, inside
/// [`Self::resume_run`], through the same `gate_profile` a fresh start uses.
pub(crate) fn declared_capabilities_for_kind(kind: AdapterKind) -> Option<AdapterCapabilities> {
    let cwd = std::path::PathBuf::from(".");
    Some(match kind {
        AdapterKind::Claude => super::ClaudeAdapter::new(
            super::ClaudeStartupOptions::default(),
            cwd,
            Vec::new(),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            None,
        )
        .capabilities(),
        AdapterKind::Codex => {
            super::CodexAdapter::new(cwd, super::CodexStartupOptions::default(), Vec::new(), None)
                .capabilities()
        }
        AdapterKind::Copilot => super::CopilotAdapter::new(
            PathBuf::from("copilot"),
            cwd,
            super::CopilotStartupOptions::default(),
            Vec::new(),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            None,
        )
        .capabilities(),
        AdapterKind::OmpRpc => super::OmpRpcAdapter::declared_capabilities(),
    })
}

impl AdapterRegistry {
    /// The deterministic transcript path a resumed TUI session would tail
    /// (`TuiVendor::transcript_path_for_session`), or `None` when this
    /// daemon could not resume a TUI-mode run for this kind at all: no
    /// [`TuiSupport`] was ever supplied, or the kind has no configured
    /// adapter entry. This is the WP15 sweep's TUI-eligibility check -- the
    /// same derivation the resumed adapter itself will perform, evaluated
    /// against the filesystem *before* anything is spawned.
    #[must_use]
    pub fn tui_transcript_path_for_session(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        session: &VendorSessionRef,
    ) -> Option<std::path::PathBuf> {
        let tui = self.tui.lock().clone()?;
        let cfg = tui.adapters.get("claude")?;
        let vendor = ClaudeTuiVendor::new(self.repo_root.clone(), Vec::new());
        let spec = StartSpec {
            run_id,
            task_id,
            worker_id,
            prompt: String::new(),
            resume: Some(session.clone()),
        };
        Some(vendor.transcript_path_for_session(session, &spec, cfg))
    }
}
#[allow(clippy::too_many_arguments)]
fn build_adapter(
    profile: &WorkerProfile,
    repo_root: &std::path::Path,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    mcp: Option<AdapterMcpConfig>,
    broker: Option<Arc<CoordinationBroker>>,
    tui: Option<Arc<TuiSupport>>,
    db: Arc<DatabaseHandle>,
    project_id: crew_protocol::ProjectId,
    events_tx: tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
    display: Option<crew_protocol::DisplaySelection>,
    // The stored tailer position a resume hands the constructed adapter
    // (`None` for every fresh start). Only a `TuiAdapter` consumes it --
    // headless adapters carry their session state internally.
    resume_cursor: Option<Cursor>,
) -> Result<Arc<dyn Adapter>, RegistryError> {
    // `mode: "tui"` dispatches to a `TuiAdapter<V>` for a vendor with a
    // `TuiVendor` implementation. Claude's landed (WP13): given both a
    // `TuiSupport` bundle (via `AdapterRegistry::set_tui_support`) and
    // the Claude kind, this constructs a real `TuiAdapter<ClaudeTuiVendor>`.
    // Every other reserved kind (no vendor impl yet) -- and Claude itself
    // when no `TuiSupport` was ever supplied -- keeps the typed refusal
    // rather than silently starting the headless adapter a caller who
    // asked for a TUI session did not request. `Headless` (the default)
    // is completely unaffected: it falls through to the match below
    // exactly as before.
    if requested_mode(&profile.startup_options) == Some(super::profile::AdapterMode::Tui) {
        let kind = profile
            .startup_options
            .adapter_kind()
            .expect("Tui mode only applies to a startup_options variant with an AdapterKind");
        if kind == super::AdapterKind::Claude
            && let Some(tui) = tui
        {
            return Ok(build_claude_tui_adapter(
                &tui,
                repo_root,
                run_id,
                task_id,
                worker_id,
                db,
                project_id,
                events_tx,
                display,
                profile.environment_allowlist.clone(),
                resume_cursor,
            ));
        }
        return Err(RegistryError::TuiModeUnavailable(
            kind.wire_name().to_string(),
        ));
    }

    let adapter: Arc<dyn Adapter> = match &profile.startup_options {
        StartupOptions::Claude(options) => Arc::new(super::ClaudeAdapter::new(
            options.clone(),
            repo_root.to_path_buf(),
            profile.environment_allowlist.clone(),
            run_id,
            task_id,
            worker_id,
            mcp,
        )),
        StartupOptions::Codex(options) => Arc::new(super::CodexAdapter::new(
            repo_root.to_path_buf(),
            options.clone(),
            profile.environment_allowlist.clone(),
            mcp,
        )),
        StartupOptions::Copilot(options) => Arc::new(super::CopilotAdapter::new(
            PathBuf::from("copilot"),
            repo_root.to_path_buf(),
            options.clone(),
            profile.environment_allowlist.clone(),
            run_id,
            task_id,
            worker_id,
            mcp,
        )),
        StartupOptions::OmpRpc(_) => Arc::new(super::OmpRpcAdapter::new(
            profile.clone(),
            super::OmpRpcAdapterOptions::default(),
            broker,
        )),
        StartupOptions::TerminalDegraded(opts) => {
            Arc::new(super::terminal::TerminalAdapter::new(opts.backend.clone()))
                as Arc<dyn super::r#trait::Adapter>
        }
    };
    Ok(adapter)
}

/// Constructs a real `TuiAdapter<ClaudeTuiVendor>` bound to this run's
/// ids -- the WP11-report plumbing list, filled in: a fresh
/// [`PaneCoordinator`] built from `tui`'s static fields plus this run's
/// own `db`/`project_id`/`events_tx` (only available from
/// [`RunDriverContext`], never at registry-construction time -- see
/// [`TuiSupport`]'s own doc comment), the placement this run's own
/// display selection already resolved (or [`DisplayPlacement::SplitRight`]
/// when none was), and `tui.adapters["claude"]` (or a sane built-in
/// default if a caller never configured one) for the vendor's own
/// `AdapterConfig`.
///
/// The constructed adapter's `ResumeContext` carries this run's stored
/// tailer position (WP12's `runs.transcript_cursor`) so a subsequent
/// [`Adapter::resume`] re-tails from exactly where the journal says the
/// previous incarnation stopped; the transcript path itself is derived
/// deterministically inside the adapter from the vendor's own layout.
#[allow(clippy::too_many_arguments)]
fn build_claude_tui_adapter(
    tui: &Arc<TuiSupport>,
    repo_root: &std::path::Path,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    db: Arc<DatabaseHandle>,
    project_id: crew_protocol::ProjectId,
    events_tx: tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
    display: Option<crew_protocol::DisplaySelection>,
    environment_allowlist: Vec<String>,
    resume_cursor: Option<Cursor>,
) -> Arc<dyn Adapter> {
    let cfg = tui
        .adapters
        .get("claude")
        .cloned()
        .unwrap_or_else(default_claude_tui_config);
    let pane_coordinator = Arc::new(PaneCoordinator::new(
        Arc::clone(&tui.display_registry),
        db,
        project_id,
        events_tx,
        tui.crewd_path.clone(),
        tui.state_dir.clone(),
        repo_root.to_path_buf(),
    ));
    let placement = display
        .as_ref()
        .map(|selection| selection.placement)
        .unwrap_or(DisplayPlacement::SplitRight);
    let vendor = ClaudeTuiVendor::new(repo_root.to_path_buf(), environment_allowlist);
    Arc::new(TuiAdapter::new(
        vendor,
        cfg,
        run_id,
        task_id,
        worker_id,
        pane_coordinator,
        tui.panes_dir.clone(),
        placement,
        tui.forced_backend,
        tui.close_on_exit,
        tui.timings.clone(),
        ResumeContext {
            transcript_path: None,
            cursor: resume_cursor,
        },
    ))
}

/// The Claude TUI adapter's own built-in defaults, for a `TuiSupport`
/// whose `adapters` map (`CrewConfig.adapters`, threaded in at
/// `set_tui_support` time) never carried a `"claude"` entry -- a caller
/// that supplies TUI support at all is expected to also supply this, but
/// falling back rather than panicking keeps a misconfigured deployment
/// merely under-configured, never crashed.
fn default_claude_tui_config() -> AdapterConfig {
    AdapterConfig {
        enabled: true,
        bin: "claude".to_string(),
        mode: CrewAdapterMode::Tui,
        permission_mode: PermissionMode::Max,
        model: None,
        profile: "complex analysis, investigation, deep debugging".to_string(),
        session_dir: None,
        extra_args: Vec::new(),
    }
}

#[cfg(test)]
mod build_adapter_tests {
    //! Unit tests for the private [`build_adapter`] function, reachable
    //! only from inside this crate (an external integration test crate
    //! cannot call it). These deliberately never call `.start()` on the
    //! returned adapter -- for Claude/Codex/Copilot that would spawn a
    //! real vendor CLI and, for Claude specifically, immediately send a
    //! real (billed) model turn (see `ClaudeAdapter::start`'s own
    //! `build_stdin_user_message` call) -- so they can only prove that
    //! `build_adapter` accepts and threads an `Option<AdapterMcpConfig>`/
    //! `Option<Arc<CoordinationBroker>>` through to construction without
    //! erroring, not that the constructed adapter's own `start()` later
    //! *uses* it correctly. That mechanism is proven separately and
    //! thoroughly, with zero process spawn, by each adapter's own
    //! dedicated test suite (e.g. `tests/claude_adapter.rs`'s
    //! `mcp_injection_appends_mcp_config_after_native_discovery_args...`
    //! and `mcp_injection_env_carries_only_the_scope_token`).
    use std::collections::BTreeMap;

    use super::*;
    use crate::adapter::profile::{
        ClaudeStartupOptions, CodexStartupOptions, CopilotStartupOptions,
    };
    use crate::coordination::ScopeTokenStore;

    fn mcp_config() -> AdapterMcpConfig {
        AdapterMcpConfig {
            scope_tokens: Arc::new(ScopeTokenStore::new()),
            project_id: crew_protocol::ProjectId::new(),
            crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
            state_dir: std::env::temp_dir(),
            repository: std::env::temp_dir(),
        }
    }

    /// A real (but throwaway) `DatabaseHandle` plus a broadcast sender,
    /// for `build_adapter`'s trailing `db`/`project_id`/`events_tx`
    /// parameters -- unused by every branch these tests exercise except
    /// the Claude-TUI one, but still real values rather than a fake,
    /// exactly like `settlement_tests::harness` below.
    async fn db_and_events() -> (
        Arc<DatabaseHandle>,
        tempfile::TempDir,
        tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
    ) {
        let dir = tempfile::Builder::new()
            .prefix("bat-build-adapter-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db = Arc::new(
            DatabaseHandle::start(dir.path().join("state.db"))
                .await
                .expect("start database"),
        );
        let (events_tx, _rx) = tokio::sync::broadcast::channel(16);
        (db, dir, events_tx)
    }

    fn profile(startup_options: StartupOptions) -> WorkerProfile {
        WorkerProfile {
            id: super::super::profile::ProfileId::new(),
            adapter: "test".to_string(),
            model: "test-model".to_string(),
            permission_envelope: serde_json::json!({}),
            startup_options,
            environment_allowlist: Vec::new(),
            source: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn claude_branch_accepts_some_mcp_config() {
        let profile = profile(StartupOptions::Claude(ClaudeStartupOptions::default()));
        let (db, _dir, events_tx) = db_and_events().await;
        let result = build_adapter(
            &profile,
            std::path::Path::new("/tmp"),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            Some(mcp_config()),
            None,
            None,
            db,
            crew_protocol::ProjectId::new(),
            events_tx,
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "Claude branch must accept Some(mcp): {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn codex_branch_accepts_some_mcp_config() {
        let profile = profile(StartupOptions::Codex(CodexStartupOptions::default()));
        let (db, _dir, events_tx) = db_and_events().await;
        let result = build_adapter(
            &profile,
            std::path::Path::new("/tmp"),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            Some(mcp_config()),
            None,
            None,
            db,
            crew_protocol::ProjectId::new(),
            events_tx,
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "Codex branch must accept Some(mcp): {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn copilot_branch_accepts_some_mcp_config() {
        let profile = profile(StartupOptions::Copilot(CopilotStartupOptions::default()));
        let (db, _dir, events_tx) = db_and_events().await;
        let result = build_adapter(
            &profile,
            std::path::Path::new("/tmp"),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            Some(mcp_config()),
            None,
            None,
            db,
            crew_protocol::ProjectId::new(),
            events_tx,
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "Copilot branch must accept Some(mcp): {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    /// `mode: "tui"` on a reserved adapter kind with no `TuiVendor`
    /// implementation (Codex/Copilot) -- or on Claude when no
    /// `TuiSupport` was ever supplied -- must be a typed refusal, never a
    /// silent fallback to the headless adapter.
    #[tokio::test]
    async fn tui_mode_without_a_vendor_impl_is_a_typed_refusal_not_a_silent_headless_fallback() {
        let options = ClaudeStartupOptions {
            mode: crate::adapter::profile::AdapterMode::Tui,
            ..ClaudeStartupOptions::default()
        };
        let profile = profile(StartupOptions::Claude(options));
        let (db, _dir, events_tx) = db_and_events().await;

        let result = build_adapter(
            &profile,
            std::path::Path::new("/tmp"),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            None,
            None,
            None, // no TuiSupport supplied
            db,
            crew_protocol::ProjectId::new(),
            events_tx,
            None,
            None,
        );

        match result {
            Ok(_) => panic!("mode: tui must be refused with no TuiSupport supplied"),
            Err(err) => assert!(
                matches!(err, RegistryError::TuiModeUnavailable(ref kind) if kind == "claude"),
                "expected a TuiModeUnavailable(\"claude\") refusal, got: {err}"
            ),
        }
    }

    /// `mode: "tui"` on Codex/Copilot still refuses even *with*
    /// `TuiSupport` supplied -- only Claude has a real `TuiVendor` impl
    /// (WP13); the other two still have none.
    #[tokio::test]
    async fn tui_mode_on_codex_and_copilot_still_refuses_even_with_tui_support_supplied() {
        let (db, _dir, events_tx) = db_and_events().await;
        for options in [
            StartupOptions::Codex(CodexStartupOptions {
                mode: crate::adapter::profile::AdapterMode::Tui,
                ..CodexStartupOptions::default()
            }),
            StartupOptions::Copilot(CopilotStartupOptions {
                mode: crate::adapter::profile::AdapterMode::Tui,
                ..CopilotStartupOptions::default()
            }),
        ] {
            let expected_kind = profile(options.clone())
                .startup_options
                .adapter_kind()
                .expect("reserved kind")
                .wire_name()
                .to_string();
            let profile = profile(options);
            let result = build_adapter(
                &profile,
                std::path::Path::new("/tmp"),
                RunId::new(),
                TaskId::new(),
                WorkerId::new(),
                None,
                None,
                Some(test_tui_support()),
                Arc::clone(&db),
                crew_protocol::ProjectId::new(),
                events_tx.clone(),
                None,
                None,
            );
            match result {
                Ok(_) => panic!("{expected_kind}: mode: tui must still refuse (no TuiVendor impl)"),
                Err(err) => assert!(
                    matches!(err, RegistryError::TuiModeUnavailable(ref kind) if *kind == expected_kind),
                    "{expected_kind}: expected TuiModeUnavailable, got: {err}"
                ),
            }
        }
    }

    /// `mode: "tui"` on Claude, with `TuiSupport` supplied, constructs a
    /// real adapter rather than refusing -- the registry threading this
    /// WP adds. Asserted via `kind()`/`capabilities()` only: `.start()`
    /// is never called here (see this module's own doc comment on why),
    /// so this proves construction succeeds and reports the TUI
    /// capability profile, not full runtime behavior (covered instead by
    /// `tests/tui_claude_registry.rs`'s real end-to-end run).
    #[tokio::test]
    async fn claude_tui_mode_with_tui_support_constructs_a_real_tui_adapter() {
        let options = ClaudeStartupOptions {
            mode: crate::adapter::profile::AdapterMode::Tui,
            ..ClaudeStartupOptions::default()
        };
        let profile = profile(StartupOptions::Claude(options));
        let (db, _dir, events_tx) = db_and_events().await;

        let result = build_adapter(
            &profile,
            std::path::Path::new("/tmp"),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            None,
            None,
            Some(test_tui_support()),
            db,
            crew_protocol::ProjectId::new(),
            events_tx,
            None,
            None,
        );

        let adapter = result.expect("Claude TUI mode must construct with TuiSupport supplied");
        assert_eq!(adapter.kind(), "claude");
        assert_eq!(
            adapter.capabilities().protocol,
            crate::adapter::capability::ProtocolKind::Terminal
        );
    }

    /// `mode: "headless"` (the default) on every reserved adapter kind
    /// must be completely unaffected by the `mode: "tui"` guard above.
    #[tokio::test]
    async fn headless_mode_is_unaffected_on_every_reserved_kind() {
        let (db, _dir, events_tx) = db_and_events().await;
        for options in [
            StartupOptions::Claude(ClaudeStartupOptions::default()),
            StartupOptions::Codex(CodexStartupOptions::default()),
            StartupOptions::Copilot(CopilotStartupOptions::default()),
        ] {
            let profile = profile(options);
            let result = build_adapter(
                &profile,
                std::path::Path::new("/tmp"),
                RunId::new(),
                TaskId::new(),
                WorkerId::new(),
                None,
                None,
                None,
                Arc::clone(&db),
                crew_protocol::ProjectId::new(),
                events_tx.clone(),
                None,
                None,
            );
            assert!(
                result.is_ok(),
                "headless mode must still build normally: {}",
                result.err().map(|e| e.to_string()).unwrap_or_default()
            );
        }
    }

    /// A minimal but real `TuiSupport` for these unit tests: a registry
    /// with only `HiddenDisplay` registered (never touches a real
    /// backend since `.start()` is never called here).
    fn test_tui_support() -> Arc<TuiSupport> {
        let mut registry = crate::display::DisplayRegistry::new();
        registry.register(Box::new(crate::display::HiddenDisplay::new(
            crew_protocol::DisplayConfig::default(),
        )));
        Arc::new(TuiSupport {
            display_registry: Arc::new(registry),
            panes_dir: std::env::temp_dir(),
            crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
            state_dir: std::env::temp_dir(),
            close_on_exit: crate::config::crew::CloseOnExit::OnSuccess,
            forced_backend: None,
            adapters: BTreeMap::new(),
            timings: crate::adapter::tui::TuiTimings::default(),
        })
    }
}

#[cfg(test)]
mod settlement_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crew_protocol::ProjectId;
    use tokio::sync::oneshot;

    use super::*;

    /// Counts how many times `release()` was called.
    struct CountingAuthorization(Arc<AtomicUsize>);

    impl CountingAuthorization {
        fn new() -> Arc<Self> {
            Arc::new(Self(Arc::new(AtomicUsize::new(0))))
        }

        fn release_count(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }

    impl AdapterAuthorization for CountingAuthorization {
        fn authorize(
            &self,
            _profile: &WorkerProfile,
            _effective_capabilities: &AdapterCapabilities,
            _policy: Option<&crate::config::RuntimePolicy>,
        ) -> Result<(), String> {
            Ok(())
        }

        fn release(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn harness() -> (Arc<crate::db::DatabaseHandle>, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("bat-settlement-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            crate::db::DatabaseHandle::start(db_path)
                .await
                .expect("start database"),
        );
        (db, dir)
    }

    #[tokio::test]
    async fn an_observed_exit_evicts_the_adapter_and_releases_the_slot() {
        let (db, _dir) = harness().await;
        let project_id = ProjectId::new();
        let run_id = RunId::new();
        let authorization = CountingAuthorization::new();
        let running = Arc::new(Mutex::new(HashMap::new()));
        running.lock().insert(run_id, build_placeholder_adapter());

        let (tx, rx) = oneshot::channel();
        tx.send(()).expect("send settlement");

        watch_settlement(
            rx,
            Arc::clone(&running),
            Arc::clone(&authorization) as Arc<dyn AdapterAuthorization>,
            None,
            Arc::clone(&db),
            project_id,
            run_id,
        )
        .await;

        assert_eq!(
            authorization.release_count(),
            1,
            "expected exactly one release"
        );
        assert!(
            running.lock().get(&run_id).is_none(),
            "adapter should have been evicted"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_dropped_sink_without_an_exit_never_releases_a_slot() {
        let (db, _dir) = harness().await;
        let project_id = ProjectId::new();
        let run_id = RunId::new();
        let authorization = CountingAuthorization::new();
        let running = Arc::new(Mutex::new(HashMap::new()));
        running.lock().insert(run_id, build_placeholder_adapter());

        let (tx, rx) = oneshot::channel();
        drop(tx); // Simulate sink dropped without ProcessExited

        watch_settlement(
            rx,
            Arc::clone(&running),
            Arc::clone(&authorization) as Arc<dyn AdapterAuthorization>,
            None,
            Arc::clone(&db),
            project_id,
            run_id,
        )
        .await;

        assert_eq!(
            authorization.release_count(),
            0,
            "dropped sink must never release a slot"
        );
        assert!(
            running.lock().get(&run_id).is_some(),
            "adapter should still be in the map"
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// R67: the full settlement chain, not its halves. A synthetic
    /// `ProcessExited` emitted through a real [`SettlementSink`] must fire
    /// the receiver [`watch_settlement`] holds, and settling through a
    /// real ceiling-1 [`crate::policy::PolicyEvaluator`] -- the production
    /// `AdapterAuthorization` -- must free the booked slot so the next
    /// `authorize()` clears the ceiling. Breaking either handoff fails
    /// this test: a sink that stops firing on exit leaves the dropped
    /// sender to error the receiver (no release, final authorize fails),
    /// and a watcher that stops releasing fails the same assert.
    #[tokio::test]
    async fn a_process_exited_through_the_settlement_sink_frees_the_policy_ceiling() {
        use super::super::event_sink::{AdapterEvent, AdapterEventPayload};

        struct StubSink;
        impl AdapterEventSink for StubSink {
            fn emit(&self, _event: AdapterEvent) -> crate::adapter::AdapterFuture<'_, u64> {
                Box::pin(async { Ok(0) })
            }
        }

        let policy = crate::config::RuntimePolicy {
            fingerprint: "test".to_string(),
            display_backend: crate::config::crew::DisplayBackend::Auto,
            retention: "30d".to_string(),
            concurrency_ceiling: 1,
            org_security_patterns: vec![],
            copy_max_bytes: crate::workspace::DEFAULT_COPY_MAX_BYTES,
            copy_max_files: crate::workspace::DEFAULT_COPY_MAX_FILES,
            nested_violation_action: crate::config::NestedViolationAction::QuarantineAndCancel,
        };
        let evaluator = Arc::new(crate::policy::PolicyEvaluator::new(policy));
        let authorization: Arc<dyn AdapterAuthorization> = Arc::clone(&evaluator) as _;

        let profile = WorkerProfile {
            id: crate::adapter::ProfileId::new(),
            adapter: "ompRpc".to_string(),
            model: String::new(),
            permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
            startup_options: StartupOptions::OmpRpc(Default::default()),
            environment_allowlist: Vec::new(),
            source: "test".to_string(),
        };
        let capabilities = crate::adapter::omp_rpc::OmpRpcAdapter::declared_capabilities();

        // Book the one slot, then prove the ceiling is exhausted.
        authorization
            .authorize(&profile, &capabilities, None)
            .expect("the first slot is within the ceiling of 1");
        let denied = authorization
            .authorize(&profile, &capabilities, None)
            .expect_err("the second authorize must exhaust the ceiling of 1");
        assert!(
            denied.contains("concurrency ceiling"),
            "unexpected denial: {denied}"
        );

        // Settle via the REAL chain: ProcessExited -> SettlementSink ->
        // watch_settlement -> AdapterAuthorization::release.
        let (db, _dir) = harness().await;
        let project_id = ProjectId::new();
        let run_id = RunId::new();
        let running = Arc::new(Mutex::new(HashMap::new()));
        running.lock().insert(run_id, build_placeholder_adapter());

        let (sink, settled) = SettlementSink::wrap(Arc::new(StubSink));
        sink.emit(AdapterEvent {
            run_id,
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit exit");
        // Drop the sink so a SettlementSink that stopped firing on exit
        // yields Err (a clean test failure) instead of pending forever --
        // a oneshot that already sent still delivers after its sender
        // drops, so the positive path is untouched.
        drop(sink);

        watch_settlement(
            settled,
            Arc::clone(&running),
            Arc::clone(&authorization),
            None,
            Arc::clone(&db),
            project_id,
            run_id,
        )
        .await;

        assert!(
            running.lock().get(&run_id).is_none(),
            "the settled adapter must be evicted"
        );
        authorization
            .authorize(&profile, &capabilities, None)
            .expect("the settled slot must be free again (R67)");
        db.shutdown().await.expect("shutdown database");
    }
}
