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
use super::profile::{AdapterKind, AdapterMode, StartupOptions, WorkerProfile};
use super::run_lifecycle::RunLifecycleSink;
use super::r#trait::{Adapter, AdapterMessage, StartSpec, VendorSessionRef};
use super::tui::{
    ClaudeTuiVendor, CodexTuiVendor, CopilotTuiVendor, Cursor, OmpTuiVendor, ResumeContext,
    TuiAdapter, TuiSupport, TuiVendor,
};
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
    /// **Today's production implementation ([`crate::policy::PolicyEvaluator`])
    /// reads `effective_capabilities` ZERO times** -- the org-governance
    /// checks that once read it (model/adapter allowlists, required
    /// capabilities) were retired (crew-v2 gap-closure WP5). The parameter
    /// is retained for signature stability, not because anything gates on
    /// it today.
    ///
    /// **Binding constraint for whoever adds the first real capability
    /// check here (a post-0.5.0 follow-up, not this WP): DENY-ON-UNPROVEN.**
    /// A gated capability whose backing evidence is
    /// [`crate::conformance::ScenarioOutcome::Skipped`] (the kill switch,
    /// a missing probe, or any other skipped-gating scenario) must be
    /// refused with a typed rejection naming which of those it was --
    /// never silently stripped (that would be a fabricated disproof, R52)
    /// and never silently granted (that would let an unattempted scenario
    /// pass as proof). `effective_capabilities` already carries `Skipped`
    /// scenarios as undowngraded (R68) precisely so this function can tell
    /// "proved" apart from "merely never disproved" -- collapsing that
    /// distinction back into a bare grant/deny here would silently reopen
    /// the skip-grants-declared hazard the R68/R52 invariants exist to
    /// close.
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
    /// The live-session cap is full (ADR-0027 wave 3). Distinct from the
    /// concurrency ceiling: that one bounds runs actively taking a turn,
    /// this one bounds sessions that EXIST, including the ones parked
    /// between turns. A TUI vendor outlives its turn, so without this cap
    /// parked sessions accumulate without limit -- and since a follow-up
    /// turn is never refused, so would concurrent turns.
    ///
    /// Refused here, at new-run admission, and never on `message/send`:
    /// steering a worker that already exists must not fail because of a
    /// cap, whereas declining to create the N+1th worker is the same shape
    /// of refusal the concurrency ceiling already has.
    #[error(
        "the live-session cap of {cap} is full ({live} sessions alive, including ones parked \
         between turns); finish or cancel a run before starting another"
    )]
    LiveSessionCapReached { cap: usize, live: usize },
    /// `mode: "headless"` was requested for a reserved adapter kind
    /// (crew-v2 gap-closure WP-C, spec §4.6: crew v2 is TUI-only). This is
    /// distinct from [`Self::TuiModeUnavailable`]: that one names a
    /// specific kind's TUI vendor gap (temporary, closes as vendors land
    /// TUI support); this one names a permanently retired control plane
    /// with no adapter code left to dispatch to at all -- `AdapterMode`
    /// keeps `Headless` deserializable (old journals/profiles must still
    /// parse), but nothing constructs a headless adapter for it anymore.
    /// [`gate_profile`] checks for this before any conformance dispatch,
    /// so it fires identically for a fresh submit (`run_one`) and a
    /// recovery resume (`AdapterRegistry::resume_run`) -- the shared
    /// pre-flight both paths run through.
    #[error(
        "adapter {0} was requested with mode: \"headless\", which is retired in crew v2 (spec \
         §4.6) -- the headless control plane has no adapter implementation to dispatch to; use \
         mode: \"tui\""
    )]
    HeadlessControlPlaneRetired(String),
    /// The register-time backstop (`WorkerProfile::validate`'s
    /// `TerminalDegradedNotImplemented`) refuses a *new* `terminalDegraded`
    /// profile, but a historical row stored before that check existed can
    /// still reach dispatch -- this is that defense-in-depth boundary,
    /// mirroring `HeadlessControlPlaneRetired`'s own two-layer shape
    /// exactly: reject here too, rather than silently building a working
    /// adapter nothing else ever transitions a run into.
    #[error(
        "profile uses startupOptions.terminalDegraded, which is not implemented -- the \
         protocol-degradation fallback it names is not wired to any trigger yet"
    )]
    TerminalDegradedNotImplemented,
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
    /// The live-session cap (ADR-0027 wave 3), from the daemon's startup
    /// policy. `None` means uncapped -- a caller that never calls
    /// [`Self::set_max_live_sessions`] (chiefly tests) behaves exactly as
    /// before this cap existed.
    max_live_sessions: Mutex<Option<usize>>,
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
            max_live_sessions: Mutex::new(None),
            running: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Sets the live-session cap from the daemon's startup policy. A
    /// setter rather than a constructor argument for the same reason the
    /// others here are: the policy is merged after this registry is built.
    pub fn set_max_live_sessions(&self, cap: u32) {
        *self.max_live_sessions.lock() = Some(cap as usize);
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
            Ok((adapter, settled, slot_free)) => {
                self.running.lock().insert(run_id, adapter);
                let running_for_watcher = Arc::clone(&self.running);
                tokio::spawn(watch_slot(slot_free, Arc::clone(&self.authorization)));
                // A resumed TUI run owns its own pane lifecycle through
                // its adapter, same as a fresh one -- `watch_settlement`
                // journals no display event of its own for any run,
                // resumed or fresh (CREW-11).
                tokio::spawn(watch_settlement(settled, running_for_watcher, run_id));
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
    ) -> Result<
        (
            Arc<dyn Adapter>,
            oneshot::Receiver<()>,
            oneshot::Receiver<()>,
        ),
        String,
    > {
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
        let mode = requested_mode(&profile.startup_options).unwrap_or_default();
        let effective_capabilities =
            gate_profile(&self.authorization, &profile, None, mode).await?;
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
        let (sink, settled, slot_free) = SettlementSink::wrap(sink);
        if let Err(err) = adapter.resume(session, sink).await {
            self.authorization.release();
            return Err(err.to_string());
        }
        Ok((adapter, settled, slot_free))
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
        // Read under the registry's own lock, before the async block: a
        // per-run policy override may only TIGHTEN a shared cap, never
        // raise one, exactly as the concurrency ceiling behaves.
        let configured_cap = *self.max_live_sessions.lock();

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
                // The live-session cap, checked under the same lock as the
                // duplicate check so two concurrent starts cannot both see
                // room for the last session.
                let cap = match (
                    configured_cap,
                    ctx.policy.as_deref().map(|p| p.max_live_sessions as usize),
                ) {
                    (Some(startup), Some(per_run)) => Some(startup.min(per_run)),
                    (Some(only), None) | (None, Some(only)) => Some(only),
                    (None, None) => None,
                };
                if let Some(cap) = cap
                    && guard.len() >= cap
                {
                    return Err(RegistryError::LiveSessionCapReached {
                        cap,
                        live: guard.len(),
                    }
                    .into());
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
                // CREW-11: `watch_settlement` no longer journals a
                // placeholder `DisplayPaneDetached` for anyone -- the
                // submit-time placeholder `DisplayPaneAttached` it would
                // have paired with is gone too (`start_queued_run`), for
                // the same reason: an append-only journal must never carry
                // an event for something that didn't happen. `run_one`
                // used to also return whether this run's pane lifecycle
                // was owned by its adapter, purely to gate that now-deleted
                // placeholder pair -- with nothing left to gate, the
                // predicate had no other reader, so it and its plumbing
                // (the `run_one` tuple slot, `pane_lifecycle_owned_by_adapter`)
                // are deleted too, not merely ignored: a live-looking
                // function with no consumer is exactly the kind of
                // artifact that outlives what it was for.
                Ok((adapter, settled, slot_free)) => {
                    running.lock().insert(run_id, adapter);
                    let running_for_watcher = Arc::clone(&running);
                    // The slot is released at the first turn boundary,
                    // independently of the session teardown below.
                    tokio::spawn(watch_slot(slot_free, Arc::clone(&authorization)));
                    tokio::spawn(watch_settlement(settled, running_for_watcher, run_id));
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
                // adapters that support it. crew-v2 gap-closure WP-C: the
                // headless Codex adapter's protocol-level `turn/steer` is
                // retired along with the rest of the headless control
                // plane -- `TuiAdapter`'s interrupt-then-compose
                // (`TuiVendor::interrupt_sequence` + `compose_input`) is
                // now the only steer path. Adapters without it refuse
                // with capability_unsupported rather than silently
                // degrading.
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

/// Settles one run: waits for its `ProcessExited`, then evicts and
/// disposes its adapter, and returns the concurrency slot. The run's
/// terminal `RunState` edge is already durable by the time this watcher
/// runs: `RunLifecycleSink` commits it as part of journaling the very
/// `ProcessExited` this signal is fired from, so the slot is never
/// released -- and no other run authorized -- while this run still reads
/// non-terminal. `Err` from `settled` means the run's sink was dropped
/// without any process exit ever being observed -- an adapter task that
/// died before emitting one (the terminal adapter itself now settles via
/// `cancel`'s synthetic `ProcessExited`, R95); that path therefore leaves
/// the run non-terminal until the boot recovery sweep. Never release on
/// that path: there is no settlement to record, and a release without one
/// would hand this run's slot to another.
///
/// CREW-11: this used to also journal a placeholder `DisplayPaneDetached`
/// for a run not owned by its adapter, paired with `start_queued_run`'s
/// now-removed placeholder `DisplayPaneAttached` -- an append-only journal
/// must never carry either half of an attach/detach pair for a pane that
/// was never really there. A pane-owning adapter's own exit watcher
/// journals its real detach through `PaneCoordinator` already; nothing
/// else needs to.
async fn watch_settlement(
    settled: oneshot::Receiver<()>,
    running: Arc<Mutex<HashMap<RunId, Arc<dyn Adapter>>>>,
    run_id: RunId,
) {
    if settled.await.is_err() {
        return;
    }
    let evicted = running.lock().remove(&run_id);
    if let Some(adapter) = evicted {
        let _ = adapter.dispose().await;
    }
}

/// Releases this run's concurrency slot the moment it stops taking a turn
/// -- the first of a turn boundary or a process exit (ADR-0027 wave 3).
///
/// Split out of [`watch_settlement`] because the two are no longer the
/// same moment. A TUI vendor's session outlives its turn, so holding the
/// slot until the process exits would pin capacity for a run that is
/// merely parked; releasing it at the boundary is what makes the ceiling
/// mean "actively taking a turn". Session teardown stays on the exit
/// signal, where it belongs.
///
/// Follow-up turns deliberately re-acquire nothing: they are admitted on
/// the run's own implicit allowance, which cannot stack because a run has
/// one PTY and one turn at a time. The resulting bound is
/// `concurrency_ceiling + max_live_sessions`, documented in ADR-0027.
async fn watch_slot(
    slot_free: oneshot::Receiver<()>,
    authorization: Arc<dyn AdapterAuthorization>,
) {
    if slot_free.await.is_err() {
        // The sink was dropped without ever firing (a start that failed
        // before any event): the slot was already released on that path.
        return;
    }
    authorization.release();
}

/// A never-started, immediately-idle placeholder occupying the run-id
/// reservation slot while the real adapter is constructed. Its `start`/
/// `resume`/`send`/etc. are never called; it exists only to make
/// `running.contains_key` true for the duration of construction.
fn build_placeholder_adapter() -> Arc<dyn Adapter> {
    // crew-v2 gap-closure WP-C: this used to construct a real (headless)
    // `OmpRpcAdapter`, deleted along with the rest of the headless
    // control plane. Any `Adapter` impl works here -- its own doc
    // comment above already establishes that `start`/`resume`/`send`/etc.
    // are never called on this value, so `TerminalAdapter` (already the
    // lightest-weight impl in this crate: a bare harness-name string, no
    // process, no I/O) is a placeholder in exactly the same
    // never-actually-used sense the old one was.
    Arc::new(super::terminal::TerminalAdapter::new(
        "registry-placeholder".to_string(),
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
) -> Result<
    (
        Arc<dyn Adapter>,
        oneshot::Receiver<()>,
        oneshot::Receiver<()>,
    ),
    String,
> {
    let profile = resolve_profile(ctx).await.map_err(String::from)?;

    let mode = requested_mode(&profile.startup_options).unwrap_or_default();
    let effective_capabilities =
        gate_profile(authorization, &profile, ctx.policy.as_deref(), mode).await?;
    // Use the workspace path from the context (isolated worktree or copy)
    // when available; fall back to the repository root.
    let cwd = ctx.workspace_path.as_deref().unwrap_or(repo_root);
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
    let (sink, settled, slot_free) = SettlementSink::wrap(sink);
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
    Ok((adapter, settled, slot_free))
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

/// WP26: memoized fixture-suite effective capabilities per `(adapter kind,
/// requested control plane)` -- WP-B added the `AdapterMode` axis, since a
/// headless and a TUI run of the same kind are gated from materially
/// different declared profiles and must never share a cache entry.
/// Stamped with the vendor-CLI version the suite ran against (`None` under
/// the kill switch). A probed version different from the stamp is the
/// invalidation signal. The guard is never held across an `await`.
type ConformanceMemo = std::collections::HashMap<
    (crate::adapter::AdapterKind, AdapterMode),
    (Option<String>, AdapterCapabilities),
>;

static CONFORMANCE_CACHE: std::sync::LazyLock<parking_lot::Mutex<ConformanceMemo>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// The shared pre-flight every adapter-bearing entry point runs before
/// any process is spawned: conformance-derived effective capabilities,
/// the policy decision, and the vendor-CLI availability probe. Shared by
/// [`run_one`] (fresh start / resume-through-`StartSpec`) and
/// [`AdapterRegistry::resume_run`] so the two paths can never drift.
///
/// `mode` is the run's REQUESTED control plane
/// (`super::registry::requested_mode(&profile.startup_options)`, resolved
/// once by the caller and threaded through here rather than re-derived --
/// find the call sites in [`run_one`]/[`AdapterRegistry::resume_run`]) --
/// crew-v2 gap-closure WP-B's fix for the WP13 scope boundary this
/// function used to carry: before WP-B, the conformance dispatch below had
/// no mode axis at all, so a `mode: "tui"` run was authorized against its
/// vendor's *headless* fixture suite's effective capabilities, even though
/// the `TuiAdapter` actually constructed for it declares a materially
/// different profile (`ProtocolKind::Terminal`, not `Structured`, for
/// one). `TerminalDegraded` profiles have no adapter kind and never reach
/// the conformance dispatch below at all, so their caller may pass
/// whichever `AdapterMode` is convenient (`requested_mode` itself returns
/// `None` for that variant).
async fn gate_profile(
    authorization: &Arc<dyn AdapterAuthorization>,
    profile: &WorkerProfile,
    policy: Option<&crate::config::RuntimePolicy>,
    mode: AdapterMode,
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
        // crew-v2 gap-closure WP-C: the headless control plane is
        // retired -- `AdapterMode::Headless` stays deserializable (a
        // pre-WP-C journal entry or profile that never set `mode` at all
        // defaults to it, per `AdapterMode`'s own doc comment), but there
        // is no adapter implementation left to dispatch to. Reject here,
        // before any conformance dispatch, so this fires identically for
        // a fresh submit and a recovery resume (see this function's own
        // doc comment).
        if mode == AdapterMode::Headless {
            return Err(
                RegistryError::HeadlessControlPlaneRetired(kind.wire_name().to_string()).into(),
            );
        }
        // WP26: the full suite is memoized per `(kind, mode)`, stamped
        // with the vendor-CLI version the availability probe observed; a
        // changed version (upgrade, downgrade, install) is the
        // invalidation signal. The probe itself stays kind-only
        // (`probe_availability_with_version` -- a vendor's installed CLI
        // version does not vary by how this runtime chooses to invoke
        // it), and runs first because it stamps the key AND lets an
        // unusable CLI fail before a pointless suite run; it stays a
        // version handshake only -- never a model call -- and its own 60s
        // cache means repeated submits re-spawn no binary. Nothing has
        // been authorized yet at this point, so a denial here releases
        // nothing.
        //
        // Both halves of the kill-switch skip-through, on record (WP-B
        // ruling): under `CREW_DISABLE_VENDOR_CLI=1` this probe itself is
        // `Skipped`, not disproved, so `availability.disproved()` below is
        // `false` and this function proceeds -- a kill-switch daemon is
        // never denied here. The fixture suite run below then reports
        // every scenario it cannot attempt under the switch as `Skipped`
        // too, and a skip strips no capability (R68/R52, see
        // `conformance::vendor_cli_required_scenario`'s doc comment), so
        // `effective_capabilities` comes back equal to the adapter's full
        // *declared* set. This is by design, not a gap: `authorize()`
        // reads zero capability fields today (see
        // `AdapterAuthorization::authorize`'s doc comment), so there is
        // nothing here for a kill-switch daemon's unproven capabilities to
        // corrupt. The hazard this skip-through would reopen -- a
        // kill-switch run sailing through a REAL capability check on
        // merely-unproven evidence -- goes live only the day `authorize`
        // grows one; that doc comment's DENY-ON-UNPROVEN constraint is
        // what must hold from that day on, not this one.
        let (availability, probed_version) =
            conformance::probe_availability_with_version(kind).await;
        if availability.disproved() {
            return Err(format!(
                "adapter {} is unavailable: {}",
                kind.wire_name(),
                availability.detail
            ));
        }
        let cache_key = (kind, mode);
        let suite_hit =
            CONFORMANCE_CACHE
                .lock()
                .get(&cache_key)
                .and_then(|(stamped_version, cached)| {
                    (*stamped_version == probed_version).then_some(*cached)
                });
        match suite_hit {
            Some(cached) => cached,
            None => {
                let effective = conformance::run_fixture_conformance(kind, mode)
                    .await
                    .effective_capabilities;
                CONFORMANCE_CACHE
                    .lock()
                    .insert(cache_key, (probed_version, effective));
                effective
            }
        }
    };

    // Policy decision: exactly once, for cache hits and misses alike --
    // the memoized capabilities never skip slot booking.
    authorization
        .authorize(profile, &effective_capabilities, policy)
        .map_err(RegistryError::AuthorizationDenied)
        .map_err(String::from)?;

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
        kind: AdapterKind,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        session: &VendorSessionRef,
    ) -> Option<std::path::PathBuf> {
        // C1 fix: every reserved kind now has a real `TuiVendor` (WP13/WP27/WP28),
        // each with a different on-disk transcript layout -- Claude/Copilot flat
        // `<root>/<session-id>.jsonl`; Codex date-partitioned rollout walk; OMP
        // timestamp-partitioned. Pre-fix this hardcoded the `"claude"` vendor +
        // `ClaudeTuiVendor`, so a codex/copilot/omp run was silently terminalized
        // on restart: its real session file never matched the Claude-shaped path
        // the gate checked. Dispatch the same way `build_tui_adapter` does so this
        // gate and `TuiAdapter::resume_from` share one per-vendor source of truth --
        // no separate hardcoded copy that can drift out of step.
        let tui = self.tui.lock().clone()?;
        let (vendor_key, vendor): (&str, Box<dyn TuiVendor>) = match kind {
            AdapterKind::Claude => (
                "claude",
                Box::new(ClaudeTuiVendor::new(self.repo_root.clone(), Vec::new())),
            ),
            AdapterKind::Codex => (
                "codex",
                Box::new(CodexTuiVendor::new(self.repo_root.clone(), Vec::new())),
            ),
            AdapterKind::Copilot => (
                "copilot",
                Box::new(CopilotTuiVendor::new(self.repo_root.clone(), Vec::new())),
            ),
            AdapterKind::OmpRpc => (
                "omp",
                Box::new(OmpTuiVendor::new(self.repo_root.clone(), Vec::new())),
            ),
        };
        let cfg = tui
            .adapters
            .get(vendor_key)
            .cloned()
            .unwrap_or_else(|| match vendor_key {
                "codex" => default_codex_tui_config(),
                "copilot" => default_copilot_tui_config(),
                "omp" => default_omp_tui_config(),
                _ => default_claude_tui_config(),
            });
        let spec = StartSpec {
            run_id,
            task_id,
            worker_id,
            prompt: String::new(),
            resume: Some(session.clone()),
        };
        Some(vendor.transcript_path_for_session(session, &spec, &cfg))
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
        if let Some(tui) = tui {
            match kind {
                super::AdapterKind::Claude => {
                    return Ok(build_tui_adapter(
                        ClaudeTuiVendor::new(
                            repo_root.to_path_buf(),
                            profile.environment_allowlist.clone(),
                        ),
                        "claude",
                        &tui,
                        repo_root,
                        run_id,
                        task_id,
                        worker_id,
                        db,
                        project_id,
                        events_tx,
                        display,
                        resume_cursor,
                    ));
                }
                super::AdapterKind::Codex => {
                    return Ok(build_tui_adapter(
                        CodexTuiVendor::new(
                            repo_root.to_path_buf(),
                            profile.environment_allowlist.clone(),
                        ),
                        "codex",
                        &tui,
                        repo_root,
                        run_id,
                        task_id,
                        worker_id,
                        db,
                        project_id,
                        events_tx,
                        display,
                        resume_cursor,
                    ));
                }
                super::AdapterKind::Copilot => {
                    return Ok(build_tui_adapter(
                        CopilotTuiVendor::new(
                            repo_root.to_path_buf(),
                            profile.environment_allowlist.clone(),
                        ),
                        "copilot",
                        &tui,
                        repo_root,
                        run_id,
                        task_id,
                        worker_id,
                        db,
                        project_id,
                        events_tx,
                        display,
                        resume_cursor,
                    ));
                }
                super::AdapterKind::OmpRpc => {
                    return Ok(build_tui_adapter(
                        OmpTuiVendor::new(
                            repo_root.to_path_buf(),
                            profile.environment_allowlist.clone(),
                        ),
                        "omp",
                        &tui,
                        repo_root,
                        run_id,
                        task_id,
                        worker_id,
                        db,
                        project_id,
                        events_tx,
                        display,
                        resume_cursor,
                    ));
                } // Every reserved kind now has a real `TuiVendor` impl
                  // (WP13/WP27/WP28); the refusal below is reachable only
                  // when no `TuiSupport` was ever supplied.
            }
        }
        return Err(RegistryError::TuiModeUnavailable(
            kind.wire_name().to_string(),
        ));
    }

    // crew-v2 gap-closure WP-C: every reserved kind's Headless fallback
    // (three headless vendor adapters, plus ompRpc's) is retired along
    // with the adapter code itself -- `gate_profile` already refuses a
    // Headless-mode profile before either `run_one` or `resume_run` ever
    // calls this function, so reaching here with one is a defense-in-depth
    // boundary (a bug bypassing that earlier gate), not a normal path.
    // `TerminalDegraded` carries no adapter kind and is unaffected by any
    // of this -- it never took the `if` branch above either. CREW-11:
    // `WorkerProfile::validate` refuses a *new* `terminalDegraded` profile
    // at `profile/register` (`TerminalDegradedNotImplemented`), but a
    // historical row stored before that check existed can still reach
    // here -- refuse it too, the same defense-in-depth shape as the
    // headless case right above, rather than silently building a working
    // `TerminalAdapter` nothing else ever transitions a run into.
    let _ = (mcp, broker); // only ever consumed by the deleted headless arms
    match &profile.startup_options {
        StartupOptions::Claude(_)
        | StartupOptions::Codex(_)
        | StartupOptions::Copilot(_)
        | StartupOptions::OmpRpc(_) => {
            let kind = profile
                .startup_options
                .adapter_kind()
                .expect("these four variants always have an AdapterKind");
            Err(RegistryError::HeadlessControlPlaneRetired(
                kind.wire_name().to_string(),
            ))
        }
        StartupOptions::TerminalDegraded(_) => Err(RegistryError::TerminalDegradedNotImplemented),
    }
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
fn build_tui_adapter<V: TuiVendor>(
    vendor: V,
    vendor_key: &str,
    tui: &Arc<TuiSupport>,
    repo_root: &std::path::Path,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    db: Arc<DatabaseHandle>,
    project_id: crew_protocol::ProjectId,
    events_tx: tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
    display: Option<crew_protocol::DisplaySelection>,
    resume_cursor: Option<Cursor>,
) -> Arc<dyn Adapter> {
    let cfg = tui
        .adapters
        .get(vendor_key)
        .cloned()
        .unwrap_or_else(|| match vendor_key {
            "codex" => default_codex_tui_config(),
            "copilot" => default_copilot_tui_config(),
            "omp" => default_omp_tui_config(),
            _ => default_claude_tui_config(),
        });
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
    let launch_program = display
        .as_ref()
        .and_then(|selection| selection.launch_program);
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
        launch_program,
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
fn default_codex_tui_config() -> AdapterConfig {
    AdapterConfig {
        enabled: true,
        bin: "codex".to_string(),
        mode: CrewAdapterMode::Tui,
        permission_mode: PermissionMode::Max,
        model: None,
        profile: "complex analysis, investigation, deep debugging".to_string(),
        session_dir: None,
        extra_args: Vec::new(),
    }
}

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

fn default_copilot_tui_config() -> AdapterConfig {
    AdapterConfig {
        enabled: true,
        bin: "copilot".to_string(),
        mode: CrewAdapterMode::Tui,
        permission_mode: PermissionMode::Max,
        model: None,
        profile: "documentation, explanations".to_string(),
        session_dir: None,
        extra_args: Vec::new(),
    }
}

fn default_omp_tui_config() -> AdapterConfig {
    AdapterConfig {
        enabled: true,
        bin: "omp".to_string(),
        mode: CrewAdapterMode::Tui,
        permission_mode: PermissionMode::Max,
        model: Some("qwen".to_string()),
        profile: "implementation, coding tasks".to_string(),
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

    /// `mode: "tui"` with NO `TuiSupport` supplied must be a typed
    /// refusal, never a silent fallback to the headless adapter. (Every
    /// reserved kind has a real vendor impl now; only the missing-support
    /// window refuses.)
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

    /// `mode: "tui"` now constructs a real TuiAdapter for EVERY reserved
    /// kind (WP13/WP27/WP28); these assert the two newest vendors wire up
    /// exactly like claude/codex do.
    #[tokio::test]
    async fn copilot_tui_mode_with_tui_support_constructs_a_real_tui_adapter() {
        let options = CopilotStartupOptions {
            mode: crate::adapter::profile::AdapterMode::Tui,
            ..CopilotStartupOptions::default()
        };
        let profile = profile(StartupOptions::Copilot(options));
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

        let adapter = result.expect("Copilot TUI mode must construct with TuiSupport supplied");
        assert_eq!(adapter.kind(), "copilot");
        assert_eq!(
            adapter.capabilities().protocol,
            crate::adapter::capability::ProtocolKind::Terminal
        );
    }

    #[tokio::test]
    async fn omp_tui_mode_with_tui_support_constructs_a_real_tui_adapter() {
        use crate::adapter::profile::OmpRpcStartupOptions;
        let options = OmpRpcStartupOptions {
            mode: crate::adapter::profile::AdapterMode::Tui,
            ..OmpRpcStartupOptions::default()
        };
        let profile = profile(StartupOptions::OmpRpc(options));
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

        let adapter = result.expect("OMP TUI mode must construct with TuiSupport supplied");
        assert_eq!(adapter.kind(), "omp-rpc");
        assert_eq!(
            adapter.capabilities().protocol,
            crate::adapter::capability::ProtocolKind::Terminal
        );
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

    /// WP27: `mode: "tui"` on Codex now constructs a real
    /// `TuiAdapter<CodexTuiVendor>` the same way claude does.
    #[tokio::test]
    async fn codex_tui_mode_with_tui_support_constructs_a_real_tui_adapter() {
        let options = CodexStartupOptions {
            mode: crate::adapter::profile::AdapterMode::Tui,
            ..CodexStartupOptions::default()
        };
        let profile = profile(StartupOptions::Codex(options));
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

        let adapter = result.expect("Codex TUI mode must construct with TuiSupport supplied");
        assert_eq!(adapter.kind(), "codex");
        assert_eq!(
            adapter.capabilities().protocol,
            crate::adapter::capability::ProtocolKind::Terminal
        );
    }

    /// crew-v2 gap-closure WP-C: `mode: "headless"` (still the
    /// `AdapterMode` default, for wire-compat with pre-WP13 profiles) on
    /// every reserved adapter kind is now retired -- `build_adapter`
    /// refuses it with the typed `HeadlessControlPlaneRetired` error,
    /// never silently building a (now-deleted) headless adapter. This
    /// replaces the old
    /// `headless_mode_is_unaffected_on_every_reserved_kind`, which
    /// asserted the opposite.
    #[tokio::test]
    async fn headless_mode_is_refused_on_every_reserved_kind_not_silently_built() {
        let (db, _dir, events_tx) = db_and_events().await;
        for (options, wire_name) in [
            (
                StartupOptions::Claude(ClaudeStartupOptions::default()),
                "claude",
            ),
            (
                StartupOptions::Codex(CodexStartupOptions::default()),
                "codex",
            ),
            (
                StartupOptions::Copilot(CopilotStartupOptions::default()),
                "copilot",
            ),
            (
                StartupOptions::OmpRpc(crate::adapter::profile::OmpRpcStartupOptions::default()),
                "ompRpc",
            ),
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
            // `Arc<dyn Adapter>` (the `Ok` type) has no `Debug` impl, so
            // `expect_err`/`unwrap_err` (which format it on the panic
            // path) don't apply here -- match manually instead.
            match result {
                Err(err) => assert_eq!(
                    err,
                    RegistryError::HeadlessControlPlaneRetired(wire_name.to_string()),
                    "{wire_name}: must be the typed retirement refusal"
                ),
                Ok(_) => panic!("{wire_name}: headless must be refused, not built"),
            }
        }
    }

    /// C1 regression: `tui_transcript_path_for_session` must dispatch on the
    /// run's vendor kind, NOT hardcode Claude. Pre-fix this always looked in
    /// Claude's layout, so a codex/copilot/omp run was abandoned on restart --
    /// its real session file never matched the Claude-shaped path the gate
    /// checked. Each kind gets a distinct `session_dir` so a wrong-vendor
    /// lookup is detectable by root.
    #[test]
    fn tui_transcript_path_for_session_dispatches_per_vendor_not_claude() {
        let tmp = tempfile::Builder::new()
            .prefix("bat-c1-gate-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let root = tmp.path().to_path_buf();
        let claude_root = root.join("claude");
        let copilot_root = root.join("copilot");
        let codex_root = root.join("codex");
        let omp_root = root.join("omp");
        for r in [&claude_root, &copilot_root, &codex_root, &omp_root] {
            std::fs::create_dir_all(r).expect("mkdir vendor root");
        }
        let sid = VendorSessionRef("session-abc".to_string());

        // Each vendor's transcript lives ONLY under that vendor's root, in
        // that vendor's own on-disk layout.
        std::fs::write(claude_root.join("session-abc.jsonl"), b"[]")
            .expect("seed claude transcript");
        std::fs::write(copilot_root.join("session-abc.jsonl"), b"[]")
            .expect("seed copilot transcript");
        let codex_file = codex_root
            .join("2026")
            .join("07")
            .join("24")
            .join("rollout-1700000000-1-session-abc.jsonl");
        std::fs::create_dir_all(codex_file.parent().expect("codex parent"))
            .expect("mkdir codex layout");
        std::fs::write(&codex_file, b"[]").expect("seed codex transcript");
        let omp_file = omp_root.join("2026-06-01T00-00-00-000Z_session-abc.jsonl");
        std::fs::write(&omp_file, b"[]").expect("seed omp transcript");

        let mut claude_cfg = default_claude_tui_config();
        claude_cfg.session_dir = Some(claude_root.to_string_lossy().into_owned());
        let mut copilot_cfg = default_copilot_tui_config();
        copilot_cfg.session_dir = Some(copilot_root.to_string_lossy().into_owned());
        let mut codex_cfg = default_codex_tui_config();
        codex_cfg.session_dir = Some(codex_root.to_string_lossy().into_owned());
        let mut omp_cfg = default_omp_tui_config();
        omp_cfg.session_dir = Some(omp_root.to_string_lossy().into_owned());

        let mut adapters: BTreeMap<String, AdapterConfig> = BTreeMap::new();
        adapters.insert("claude".to_string(), claude_cfg);
        adapters.insert("copilot".to_string(), copilot_cfg);
        adapters.insert("codex".to_string(), codex_cfg);
        adapters.insert("omp".to_string(), omp_cfg);

        let mut display = crate::display::DisplayRegistry::new();
        display.register(Box::new(crate::display::HiddenDisplay::new(
            crew_protocol::DisplayConfig::default(),
        )));
        let tui = Arc::new(TuiSupport {
            display_registry: Arc::new(display),
            panes_dir: tmp.path().to_path_buf(),
            crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
            state_dir: tmp.path().to_path_buf(),
            close_on_exit: crate::config::crew::CloseOnExit::OnSuccess,
            forced_backend: None,
            adapters,
            timings: crate::adapter::tui::TuiTimings::default(),
        });
        let registry = AdapterRegistry::new(
            Arc::new(FixtureAuthorization { allow: true }),
            root.clone(),
            None,
            Vec::new(),
        );
        registry.set_tui_support(tui);

        let path_for = |kind| {
            registry.tui_transcript_path_for_session(
                kind,
                RunId::new(),
                TaskId::new(),
                WorkerId::new(),
                &sid,
            )
        };

        // Claude sanity: resolves under the claude root.
        let claude_path = path_for(AdapterKind::Claude).expect("claude has TUI support");
        assert_eq!(claude_path, claude_root.join("session-abc.jsonl"));

        // C1 guards: each non-Claude kind must resolve under ITS root, never
        // under claude_root (the pre-fix behavior).
        let copilot_path = path_for(AdapterKind::Copilot).expect("copilot has TUI support");
        assert_eq!(copilot_path, copilot_root.join("session-abc.jsonl"));
        assert!(
            !copilot_path.starts_with(&claude_root),
            "copilot transcript must not resolve to claude's root"
        );

        let codex_path = path_for(AdapterKind::Codex).expect("codex has TUI support");
        assert_eq!(codex_path, codex_file);
        assert!(
            !codex_path.starts_with(&claude_root),
            "codex transcript must not resolve to claude's root"
        );

        let omp_path = path_for(AdapterKind::OmpRpc).expect("omp has TUI support");
        assert_eq!(omp_path, omp_file);
        assert!(
            !omp_path.starts_with(&claude_root),
            "omp transcript must not resolve to claude's root"
        );
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
    async fn an_observed_exit_evicts_the_adapter() {
        let (db, _dir) = harness().await;
        let run_id = RunId::new();
        let authorization = CountingAuthorization::new();
        let running = Arc::new(Mutex::new(HashMap::new()));
        running.lock().insert(run_id, build_placeholder_adapter());

        let (tx, rx) = oneshot::channel();
        tx.send(()).expect("send settlement");

        watch_settlement(rx, Arc::clone(&running), run_id).await;

        assert!(
            running.lock().get(&run_id).is_none(),
            "adapter should have been evicted"
        );
        // Releasing the slot is no longer this watcher's job: since
        // ADR-0027 wave 3 the slot is freed at the first TURN boundary by
        // `watch_slot`, while teardown stays here on the process exit.
        assert_eq!(
            authorization.release_count(),
            0,
            "settlement tears the session down; it does not free the slot"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_dropped_sink_without_an_exit_never_releases_a_slot() {
        let (db, _dir) = harness().await;
        let run_id = RunId::new();
        let authorization = CountingAuthorization::new();
        let running = Arc::new(Mutex::new(HashMap::new()));
        running.lock().insert(run_id, build_placeholder_adapter());

        let (tx, rx) = oneshot::channel();
        drop(tx); // Simulate sink dropped without ProcessExited

        watch_settlement(rx, Arc::clone(&running), run_id).await;

        let (slot_tx, slot_rx) = oneshot::channel();
        drop(slot_tx);
        watch_slot(
            slot_rx,
            Arc::clone(&authorization) as Arc<dyn AdapterAuthorization>,
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
            max_live_sessions: 16,
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
        // This test's own `authorize()` call never reads capabilities at
        // all (see `AdapterAuthorization::authorize`'s doc comment), so
        // any value proves the concurrency-ceiling point -- deliberately
        // not read from a specific adapter's `declared_capabilities()`.
        let capabilities = AdapterCapabilities {
            protocol: crate::adapter::capability::ProtocolKind::Terminal,
            resume: crate::adapter::capability::ResumeCapability::Session,
            steering: crate::adapter::capability::SteeringCapability::ActiveTurn,
            approvals: crate::adapter::capability::ApprovalsCapability::None,
            structured_result: false,
            usage: crate::adapter::capability::UsageCapability::None,
            nested: NestedCapability::None,
            native_view: crate::adapter::capability::NativeViewCapability::IndependentTui,
            workspace_control: crate::adapter::capability::WorkspaceControlCapability::Write,
            durability: crate::adapter::capability::DurabilityCapability::VendorResumable,
        };

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
        // watch_slot -> AdapterAuthorization::release. An exit still frees
        // the slot; since ADR-0027 wave 3 a turn boundary does too,
        // whichever comes first.
        let (db, _dir) = harness().await;
        let run_id = RunId::new();
        let running = Arc::new(Mutex::new(HashMap::new()));
        running.lock().insert(run_id, build_placeholder_adapter());

        let (sink, settled, slot_free) = SettlementSink::wrap(Arc::new(StubSink));
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

        watch_settlement(settled, Arc::clone(&running), run_id).await;
        watch_slot(slot_free, Arc::clone(&authorization)).await;

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
/// WP26: the fixture-suite memo. Tests serialize on a shared guard because
/// both the cache and the suite-run counter are process-global.
#[cfg(test)]
mod conformance_cache_tests {
    use super::*;
    // `FIXTURE_SUITE_RUNS_SERIAL`, not a module-local lock: this counter is
    // one process-global shared with `conformance::tests` too, in the same
    // unit-test binary -- see that static's own doc comment.
    use crate::adapter::profile::ClaudeStartupOptions;
    use crate::conformance::{FIXTURE_SUITE_RUNS, FIXTURE_SUITE_RUNS_SERIAL as SERIAL};
    use crew_protocol::{
        ProjectId, Run, RunFlags, RunState, TaskRef, Timestamp, Worker, WorkerProfileRef,
    };
    use std::sync::atomic::Ordering;

    fn omp_rpc_profile_with_mode(mode: AdapterMode) -> WorkerProfile {
        WorkerProfile {
            id: crate::adapter::profile::ProfileId::new(),
            adapter: "ompRpc".to_string(),
            model: String::new(),
            permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
            startup_options: StartupOptions::OmpRpc(
                crate::adapter::profile::OmpRpcStartupOptions {
                    mode,
                    ..Default::default()
                },
            ),
            environment_allowlist: Vec::new(),
            source: "test".to_string(),
        }
    }

    /// crew-v2 gap-closure WP-C: `Tui` is the only mode that reaches the
    /// conformance dispatch at all now (`Headless` is rejected in
    /// `gate_profile` before it gets there) -- so every generic
    /// memoization test in this module drives `Tui` unconditionally,
    /// unlike WP-B's `omp_rpc_profile()` (which defaulted to `Headless`,
    /// the pre-WP-C `AdapterMode` default).
    fn omp_rpc_profile() -> WorkerProfile {
        omp_rpc_profile_with_mode(AdapterMode::Tui)
    }

    #[tokio::test]
    async fn two_gate_profile_calls_run_the_fixture_suite_once() {
        let _serial = SERIAL.lock().await;
        CONFORMANCE_CACHE.lock().clear();
        FIXTURE_SUITE_RUNS.store(0, Ordering::Relaxed);
        let authorization: Arc<dyn AdapterAuthorization> =
            Arc::new(FixtureAuthorization { allow: true });
        let profile = omp_rpc_profile();

        let first = gate_profile(&authorization, &profile, None, AdapterMode::Tui)
            .await
            .expect("first gate must pass");
        assert_eq!(
            FIXTURE_SUITE_RUNS.load(Ordering::Relaxed),
            1,
            "a cold cache runs the suite exactly once"
        );

        let second = gate_profile(&authorization, &profile, None, AdapterMode::Tui)
            .await
            .expect("second gate must pass");
        assert_eq!(first, second, "the memoized capabilities must match");
        assert_eq!(
            FIXTURE_SUITE_RUNS.load(Ordering::Relaxed),
            1,
            "the second submit must reuse the memoized suite"
        );
        CONFORMANCE_CACHE.lock().clear();
    }

    #[tokio::test]
    async fn a_version_change_invalidates_the_memoized_suite() {
        let _serial = SERIAL.lock().await;
        // A stamp no probe can report (the kill switch stamps `None`) --
        // exactly the "installed version changed" signal. The stubbed
        // capabilities value itself is arbitrary (any AdapterCapabilities
        // literal proves the point) -- deliberately not read from a real
        // adapter's `declared_capabilities()`, so this test has zero
        // dependency on which adapter kinds still exist.
        let stale = (
            Some("0.0.0-stale-test".to_string()),
            AdapterCapabilities {
                protocol: crate::adapter::capability::ProtocolKind::Terminal,
                resume: crate::adapter::capability::ResumeCapability::Session,
                steering: crate::adapter::capability::SteeringCapability::ActiveTurn,
                approvals: crate::adapter::capability::ApprovalsCapability::None,
                structured_result: false,
                usage: crate::adapter::capability::UsageCapability::None,
                nested: NestedCapability::None,
                native_view: crate::adapter::capability::NativeViewCapability::IndependentTui,
                workspace_control: crate::adapter::capability::WorkspaceControlCapability::Write,
                durability: crate::adapter::capability::DurabilityCapability::VendorResumable,
            },
        );
        CONFORMANCE_CACHE
            .lock()
            .insert((AdapterKind::OmpRpc, AdapterMode::Tui), stale);
        FIXTURE_SUITE_RUNS.store(0, Ordering::Relaxed);
        let authorization: Arc<dyn AdapterAuthorization> =
            Arc::new(FixtureAuthorization { allow: true });

        gate_profile(&authorization, &omp_rpc_profile(), None, AdapterMode::Tui)
            .await
            .expect("gate must pass despite the stale stamp");
        assert_eq!(
            FIXTURE_SUITE_RUNS.load(Ordering::Relaxed),
            1,
            "a changed probed version must force one fresh suite run"
        );
        // And the refreshed entry carries the current probe result, not
        // the stale one.
        let stamp = CONFORMANCE_CACHE
            .lock()
            .get(&(AdapterKind::OmpRpc, AdapterMode::Tui))
            .and_then(|(version, _)| version.clone());
        assert_ne!(
            stamp.as_deref(),
            Some("0.0.0-stale-test"),
            "the refreshed entry must be stamped with the current probe"
        );
        CONFORMANCE_CACHE.lock().clear();
    }

    /// WP-B Task 5a (still current post-WP-C: `Tui` is the only mode that
    /// reaches this dispatch at all now): a TUI-mode submit's gate
    /// consumes the TUI suite's report, pinned at the SOURCE via
    /// `protocol` (`Terminal` for every TUI adapter).
    #[tokio::test]
    async fn a_tui_mode_submit_gates_on_the_tui_suites_effective_capabilities() {
        let _serial = SERIAL.lock().await;
        CONFORMANCE_CACHE.lock().clear();
        let authorization: Arc<dyn AdapterAuthorization> =
            Arc::new(FixtureAuthorization { allow: true });

        let effective = gate_profile(
            &authorization,
            &omp_rpc_profile_with_mode(AdapterMode::Tui),
            None,
            AdapterMode::Tui,
        )
        .await
        .expect("tui gate must pass");

        assert_eq!(
            effective.protocol,
            crate::adapter::capability::ProtocolKind::Terminal,
            "a TUI-mode submit must be gated on the TUI suite's effective capabilities: \
             {effective:?}"
        );
        CONFORMANCE_CACHE.lock().clear();
    }

    /// crew-v2 gap-closure WP-C ruling 1: `mode: "headless"` is retired.
    /// Supersedes WP-B's `a_headless_mode_submit_still_gates_on_the_headless_suites_effective_capabilities`
    /// (headless submits used to succeed; now every one is refused) and
    /// `the_memo_key_is_kind_and_mode_not_kind_alone`'s Headless half (the
    /// mode-axis distinctness that test proved is git history now that
    /// there is only one live mode to distinguish from -- the surviving
    /// half, "two submits of the same kind share one memoized run", is
    /// `two_gate_profile_calls_run_the_fixture_suite_once` above). Pins
    /// that the rejection fires BEFORE any conformance dispatch: no suite
    /// runs, no cache entry is written, and the error names the retired
    /// kind via the typed `RegistryError::HeadlessControlPlaneRetired`,
    /// not a generic string a caller could mistake for something else
    /// (an unavailable CLI, a config error, ...).
    #[tokio::test]
    async fn a_headless_mode_submit_is_rejected_before_any_suite_runs_or_the_cache_is_touched() {
        let _serial = SERIAL.lock().await;
        CONFORMANCE_CACHE.lock().clear();
        FIXTURE_SUITE_RUNS.store(0, Ordering::Relaxed);
        let authorization: Arc<dyn AdapterAuthorization> =
            Arc::new(FixtureAuthorization { allow: true });

        let err = gate_profile(
            &authorization,
            &omp_rpc_profile_with_mode(AdapterMode::Headless),
            None,
            AdapterMode::Headless,
        )
        .await
        .expect_err("a Headless-mode submit must be refused, not gated");

        assert_eq!(
            err,
            RegistryError::HeadlessControlPlaneRetired("ompRpc".to_string()).to_string(),
            "must be the typed retirement refusal, not some other denial"
        );
        assert_eq!(
            FIXTURE_SUITE_RUNS.load(Ordering::Relaxed),
            0,
            "the rejection must fire before any conformance suite runs"
        );
        assert!(
            CONFORMANCE_CACHE.lock().is_empty(),
            "a refused Headless submit must never write a cache entry"
        );
    }

    /// Seeds a task/worker/run with a resolved Claude profile snapshot in
    /// the given `mode`, for driving the REAL `resume_run`/`start` call
    /// sites end to end (not `gate_profile` in isolation).
    async fn seed_claude_profile_run(
        db: &Arc<crate::db::DatabaseHandle>,
        project_id: ProjectId,
        mode: AdapterMode,
    ) -> (TaskId, WorkerId, RunId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        let profile_json = serde_json::to_string(&WorkerProfile {
            id: crate::adapter::profile::ProfileId::new(),
            adapter: "claude".to_string(),
            model: String::new(),
            permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
            startup_options: StartupOptions::Claude(ClaudeStartupOptions {
                mode,
                ..Default::default()
            }),
            environment_allowlist: Vec::new(),
            source: "test".to_string(),
        })
        .expect("profile serializes");
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
            repo.create_worker_with_snapshot(&worker, Some(profile_json))?;
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
        (task_id, worker_id, run_id)
    }

    /// Reviewer finding on WP-B's first pass: 5a/5b/5c all call
    /// `gate_profile` directly, passing `mode` as an explicit argument --
    /// they pin that `gate_profile` USES its parameter correctly, but say
    /// nothing about whether the real call sites (`resume_run`/`run_one`)
    /// COMPUTE and pass the right value. A regression at either call site
    /// (a hardcoded `Headless`, or a wrong re-derivation) would leave
    /// every one of those tests green while TUI runs silently revert to
    /// headless-derived capabilities -- precisely the WP13-F2 bug this WP
    /// exists to close. This drives the REAL `resume_run` with a genuine
    /// Claude TUI-mode worker profile (no `TuiSupport` configured on
    /// purpose -- `build_adapter`'s typed refusal for an unsupported TUI
    /// kind is a clean `Err`, reached only AFTER `gate_profile` already
    /// ran, which is exactly the point this test needs to observe; a real
    /// running `TuiAdapter` needs a fake vendor CLI script and proves
    /// nothing more about mode-threading than this does) and asserts
    /// `CONFORMANCE_CACHE` ends up keyed `(Claude, Tui)`, never
    /// `(Claude, Headless)`.
    #[tokio::test]
    async fn resume_run_threads_the_profiles_own_requested_mode_into_the_cache_key() {
        let _serial = SERIAL.lock().await;
        CONFORMANCE_CACHE.lock().clear();
        let state_dir = tempfile::Builder::new()
            .prefix("bat-registry-mode-thread-resume-")
            .tempdir_in("/tmp")
            .expect("create state dir");
        let db = Arc::new(
            crate::db::DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .expect("start database"),
        );
        let (events_tx, _rx) = tokio::sync::broadcast::channel(16);
        let project_id = ProjectId::new();
        let (_task_id, _worker_id, run_id) =
            seed_claude_profile_run(&db, project_id, AdapterMode::Tui).await;

        let violation_service = Arc::new(crate::policy::ViolationService::new(
            Arc::clone(&db),
            project_id,
            events_tx.clone(),
            None,
            crate::config::NestedViolationAction::default(),
            crate::security::redaction::Redactor::new(),
        ));
        let registry = AdapterRegistry::new(
            Arc::new(FixtureAuthorization { allow: true }),
            state_dir.path().to_path_buf(),
            None,
            Vec::new(),
        );
        registry.set_resume_support(Arc::new(ResumeSupport {
            db: Arc::clone(&db),
            project_id,
            violation_service,
            events_tx,
        }));

        let result = registry
            .resume_run(run_id, VendorSessionRef("sess-1".to_string()), None)
            .await;
        // WP-B re-review rider: tightened from a bare `is_err()` to the
        // specific typed refusal, so an environment availability-disproof
        // (a different failure entirely) would self-diagnose here instead
        // of being misread as a mode-threading regression.
        assert_eq!(
            result,
            Err(RegistryError::TuiModeUnavailable("claude".to_string()).to_string()),
            "no TuiSupport is configured, so this must fail AFTER gate_profile ran with exactly \
             build_adapter's typed refusal for an unsupported TUI kind, not before and not some \
             other error"
        );

        let (has_tui_key, has_headless_key, cache_debug) = {
            let cache = CONFORMANCE_CACHE.lock();
            (
                cache.contains_key(&(AdapterKind::Claude, AdapterMode::Tui)),
                cache.contains_key(&(AdapterKind::Claude, AdapterMode::Headless)),
                format!("{cache:?}"),
            )
        };
        assert!(
            has_tui_key,
            "resume_run must have gated this profile's real Claude-TUI request as Tui: \
             {cache_debug}"
        );
        assert!(
            !has_headless_key,
            "a Claude-TUI profile must never populate the Headless cache entry -- that would \
             mean resume_run silently re-derived (or hardcoded) the wrong mode: {cache_debug}"
        );
        CONFORMANCE_CACHE.lock().clear();
        db.shutdown().await.expect("shutdown database");
    }

    /// The `run_one`/`AdapterRegistry::start` equivalent of the test
    /// above -- the OTHER real call site `gate_profile`'s `mode` argument
    /// must be threaded through correctly at, driven the same way (no
    /// `TuiSupport`, asserting the cache key after an expected `Err`).
    #[tokio::test]
    async fn start_threads_the_profiles_own_requested_mode_into_the_cache_key() {
        let _serial = SERIAL.lock().await;
        CONFORMANCE_CACHE.lock().clear();
        let state_dir = tempfile::Builder::new()
            .prefix("bat-registry-mode-thread-start-")
            .tempdir_in("/tmp")
            .expect("create state dir");
        let db = Arc::new(
            crate::db::DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .expect("start database"),
        );
        let (events_tx, _rx) = tokio::sync::broadcast::channel(16);
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) =
            seed_claude_profile_run(&db, project_id, AdapterMode::Tui).await;

        let violation_service = Arc::new(crate::policy::ViolationService::new(
            Arc::clone(&db),
            project_id,
            events_tx.clone(),
            None,
            crate::config::NestedViolationAction::default(),
            crate::security::redaction::Redactor::new(),
        ));
        let registry = AdapterRegistry::new(
            Arc::new(FixtureAuthorization { allow: true }),
            state_dir.path().to_path_buf(),
            None,
            Vec::new(),
        );

        let ctx = crate::service::RunDriverContext {
            db: Arc::clone(&db),
            project_id,
            run_id,
            task_id,
            worker_id,
            prompt: None,
            events_tx,
            violation_service,
            workspace_path: None,
            policy: None,
            display: None,
            activity: Arc::new(crate::adapter::ActivityClock::new()),
        };
        let result = <AdapterRegistry as RunDriver>::start(&registry, ctx).await;
        // WP-B re-review rider: tightened from a bare `is_err()` (see the
        // matching comment on `resume_run_threads...` above).
        assert_eq!(
            result,
            Err(RegistryError::TuiModeUnavailable("claude".to_string()).to_string()),
            "no TuiSupport is configured, so this must fail AFTER gate_profile ran with exactly \
             build_adapter's typed refusal, not before and not some other error"
        );

        let (has_tui_key, has_headless_key, cache_debug) = {
            let cache = CONFORMANCE_CACHE.lock();
            (
                cache.contains_key(&(AdapterKind::Claude, AdapterMode::Tui)),
                cache.contains_key(&(AdapterKind::Claude, AdapterMode::Headless)),
                format!("{cache:?}"),
            )
        };
        assert!(
            has_tui_key,
            "start (run_one) must have gated this profile's real Claude-TUI request as Tui: \
             {cache_debug}"
        );
        assert!(
            !has_headless_key,
            "a Claude-TUI profile must never populate the Headless cache entry via start/run_one \
             either: {cache_debug}"
        );
        CONFORMANCE_CACHE.lock().clear();
        db.shutdown().await.expect("shutdown database");
    }
}
