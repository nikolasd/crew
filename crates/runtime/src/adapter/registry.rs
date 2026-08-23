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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;

use batman_protocol::{RunId, TaskId, WorkerId};
use tokio::sync::oneshot;

use super::capability::{AdapterCapabilities, NestedCapability};
use super::event_sink::{AdapterEventSink, DomainAdapterEventSink, SettlementSink};
use super::mcp_config::AdapterMcpConfig;
use super::profile::{StartupOptions, WorkerProfile};
use super::run_lifecycle::RunLifecycleSink;
use super::r#trait::{Adapter, AdapterMessage, StartSpec};
use crate::adapter::CancelScope;
use crate::conformance;
use crate::coordination::CoordinationBroker;
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
}

impl From<RegistryError> for String {
    fn from(err: RegistryError) -> Self {
        err.to_string()
    }
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
    running: Arc<Mutex<HashMap<RunId, Arc<dyn Adapter>>>>,
    /// Org security patterns for redaction.
    org_security_patterns: Vec<String>,
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
                org_security_patterns,
            )
            .await
            {
                Ok((adapter, settled)) => {
                    running.lock().insert(run_id, adapter);
                    let running_for_watcher = Arc::clone(&running);
                    let authorization_for_watcher = Arc::clone(&authorization);
                    let display = ctx.display.clone();
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
    ) -> RunDriverFuture<'static, Result<(), String>> {
        let running = Arc::clone(&self.running);

        Box::pin(async move {
            let adapter = running.lock().get(&run_id).cloned().ok_or_else(|| {
                <RegistryError as Into<String>>::into(RegistryError::NoRunningAdapter(run_id))
            })?;

            adapter
                .send(AdapterMessage::FollowUp { text: prompt })
                .await
                .map_err(|err| err.to_string())
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
    project_id: batman_protocol::ProjectId,
    run_id: RunId,
    backend: batman_protocol::DisplayBackend,
    placement: batman_protocol::DisplayPlacement,
) {
    let _ = db
        .run_domain_op(Box::new(move |conn| {
            let mut repo = crate::domain::DomainRepository::new(conn, project_id);
            repo.record_display_event(
                batman_protocol::RuntimeEventKind::DisplayPaneDetached,
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
    display: Option<batman_protocol::DisplaySelection>,
    db: Arc<crate::db::DatabaseHandle>,
    project_id: batman_protocol::ProjectId,
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
    org_security_patterns: Vec<String>,
) -> Result<(Arc<dyn Adapter>, oneshot::Receiver<()>), String> {
    let profile = resolve_profile(ctx).await.map_err(String::from)?;

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
        conformance::run_fixture_conformance(kind)
            .await
            .effective_capabilities
    };

    // Policy first: it is the cheaper, machine-independent decision, and
    // probing a vendor CLI for a run policy already forbids would spawn a
    // process to answer a question that no longer matters.
    authorization
        .authorize(&profile, &effective_capabilities, ctx.policy.as_deref())
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

    // Use the workspace path from the context (isolated worktree or copy)
    // when available; fall back to the repository root.
    let cwd = ctx.workspace_path.as_deref().unwrap_or(repo_root);
    let adapter = match build_adapter(
        &profile,
        cwd,
        ctx.run_id,
        ctx.task_id,
        ctx.worker_id,
        mcp,
        broker,
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
        ctx.policy
            .as_deref()
            .and_then(|p| p.cost_ceiling_per_run_usd),
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
    );
    let (sink, settled) = SettlementSink::wrap(sink);
    if let Err(err) = adapter
        .start(
            StartSpec {
                run_id: ctx.run_id,
                task_id: ctx.task_id,
                worker_id: ctx.worker_id,
                prompt: ctx.prompt.clone().unwrap_or_default(),
                resume: None,
            },
            sink,
        )
        .await
    {
        authorization.release();
        return Err(err.to_string());
    }
    Ok((adapter, settled))
}

async fn resolve_profile(ctx: &RunDriverContext) -> Result<WorkerProfile, RegistryError> {
    let db = Arc::clone(&ctx.db);
    let project_id = ctx.project_id;
    let worker_id = ctx.worker_id;
    let snapshot = db
        .run_domain_op(Box::new(move |conn| {
            let repo = DomainRepository::new(conn, project_id);
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

fn build_adapter(
    profile: &WorkerProfile,
    repo_root: &std::path::Path,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    mcp: Option<AdapterMcpConfig>,
    broker: Option<Arc<CoordinationBroker>>,
) -> Result<Arc<dyn Adapter>, RegistryError> {
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
    use super::*;
    use crate::adapter::profile::{
        ClaudeStartupOptions, CodexStartupOptions, CopilotStartupOptions,
    };
    use crate::coordination::ScopeTokenStore;

    fn mcp_config() -> AdapterMcpConfig {
        AdapterMcpConfig {
            scope_tokens: Arc::new(ScopeTokenStore::new()),
            project_id: batman_protocol::ProjectId::new(),
            crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
            state_dir: std::env::temp_dir(),
            repository: std::env::temp_dir(),
        }
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

    #[test]
    fn claude_branch_accepts_some_mcp_config() {
        let profile = profile(StartupOptions::Claude(ClaudeStartupOptions::default()));
        let result = build_adapter(
            &profile,
            std::path::Path::new("/tmp"),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            Some(mcp_config()),
            None,
        );
        assert!(
            result.is_ok(),
            "Claude branch must accept Some(mcp): {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[test]
    fn codex_branch_accepts_some_mcp_config() {
        let profile = profile(StartupOptions::Codex(CodexStartupOptions::default()));
        let result = build_adapter(
            &profile,
            std::path::Path::new("/tmp"),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            Some(mcp_config()),
            None,
        );
        assert!(
            result.is_ok(),
            "Codex branch must accept Some(mcp): {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }

    #[test]
    fn copilot_branch_accepts_some_mcp_config() {
        let profile = profile(StartupOptions::Copilot(CopilotStartupOptions::default()));
        let result = build_adapter(
            &profile,
            std::path::Path::new("/tmp"),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            Some(mcp_config()),
            None,
        );
        assert!(
            result.is_ok(),
            "Copilot branch must accept Some(mcp): {}",
            result.err().map(|e| e.to_string()).unwrap_or_default()
        );
    }
}

#[cfg(test)]
mod settlement_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use batman_protocol::ProjectId;
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
            merged: serde_json::json!({}),
            fingerprint: "test".to_string(),
            display_backend: "auto".to_string(),
            retention: "30d".to_string(),
            max_workers: 4,
            concurrency_ceiling: 1,
            allowed_models: vec![],
            allowed_adapters: vec![],
            cost_ceiling_per_run_usd: None,
            org_security_patterns: vec![],
            rollout_gates: crate::config::RolloutGates {
                vendor_terms_accepted: true,
                retention_configured: true,
                model_allowlist_set: true,
                concurrency_explicit: true,
                native_discovery_reviewed: true,
                ornith_identity_set: true,
                nested_violation_action: crate::config::NestedViolationAction::QuarantineAndCancel,
                allow_development_binary_override: false,
            },
            copy_max_bytes: crate::workspace::DEFAULT_COPY_MAX_BYTES,
            copy_max_files: crate::workspace::DEFAULT_COPY_MAX_FILES,
            required_capabilities: vec![],
            cost_ceiling_daily_usd: None,
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
