//! The `RunDriver` seam: delegates adapter-backed run start to an injected
//! implementation. Production startup without an adapter registry has no
//! driver injected; `run/submit` then reports `adapter_unavailable` after
//! preserving the queued run. Orchestration tests inject [`FakeRunDriver`],
//! which drives `queued -> starting -> working` through the same domain
//! repository transitions a real adapter would use.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crew_protocol::{EventEnvelope, ProjectId, RunId, RunState, TaskId, WorkerId};
use tokio::sync::broadcast;

use crate::adapter::{Adapter, CancelScope};
use crate::db::DatabaseHandle;
use crate::domain::{DomainRepository, take_envelope};

/// A boxed future returned by [`RunDriver::start`].
pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything a [`RunDriver`] needs to start (and subsequently transition) a
/// run through the durable domain repository.
#[derive(Clone)]
pub struct RunDriverContext {
    pub db: Arc<DatabaseHandle>,
    pub project_id: ProjectId,
    pub run_id: RunId,
    pub task_id: TaskId,
    pub worker_id: WorkerId,
    pub prompt: Option<String>,
    pub events_tx: broadcast::Sender<EventEnvelope>,
    /// The mid-run nested-worker policy violation service (Hardening plan
    /// Task 1), for [`crate::adapter::registry::AdapterRegistry`] to wire
    /// into each run's [`crate::adapter::event_sink::DomainAdapterEventSink`].
    pub violation_service: Arc<crate::policy::ViolationService>,
    /// The resolved workspace path for this run. When `Some`, the adapter
    /// uses this as its working directory (for isolated worktrees or copies).
    /// When `None`, the adapter uses the repository root.
    pub workspace_path: Option<std::path::PathBuf>,
    /// The [`crate::config::RuntimePolicy`] this run was authorized under --
    /// the startup policy re-merged with the run's own `policyOverrides`.
    /// `None` means "use the authorizer's own startup policy", which is what
    /// every test path and every run without overrides supplies.
    pub policy: Option<Arc<crate::config::RuntimePolicy>>,
    /// The display backend resolved for this run at submit time, so the
    /// completion path can emit `DisplayPaneDetached` for the same pane it
    /// attached without probing the registry a second time. `None` when no
    /// backend was available (headless) or the run never reached selection.
    pub display: Option<crew_protocol::DisplaySelection>,
}

/// The typed success of [`RunDriver::cancel_run`]: an absent adapter is
/// not a kill failure. `Cancelled` means a live adapter acknowledged the
/// cancel; `NoRunningAdapter` means there was nothing to kill (the run
/// settled, or never started an adapter) -- a clean outcome, not an error
/// (R13: stringifying `NoRunningAdapter` into the `Err` channel made it
/// indistinguishable from a real kill failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Cancelled,
    NoRunningAdapter,
}

/// Seam for starting an adapter-backed run. The (later) adapter registry
/// plan implements this against real harnesses; orchestration tests inject
/// [`FakeRunDriver`].
pub trait RunDriver: Send + Sync {
    /// Starts the run described by `ctx`. Implementations drive subsequent
    /// lifecycle transitions themselves (through the same domain repository
    /// commands), rather than returning a single terminal result.
    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>>;

    /// Sends a follow-up message to an already-started run. Returns [`RegistryError::NoRunningAdapter`]
    /// if no adapter is currently driving the run.
    fn send_follow_up(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>>;

    /// Returns the adapter currently running for `run_id`, if any.
    fn running_adapter(&self, run_id: RunId) -> Option<Arc<dyn Adapter>>;

    /// Cancels a running adapter at the given scope. An absent adapter is
    /// the clean [`CancelOutcome::NoRunningAdapter`], never an `Err`;
    /// `Err` means a live adapter's kill actually failed (R13).
    fn cancel_run(
        &self,
        run_id: RunId,
        scope: CancelScope,
    ) -> AdapterFuture<'static, Result<CancelOutcome, String>>;

    /// The number of runs this driver is actively driving right now.
    /// Consumed by `runtime/status`'s `activeRuns` and the idle-shutdown
    /// decision (R87): a daemon with in-flight adapter work must never
    /// self-terminate as idle. Required, not defaulted: a driver that
    /// silently reported `0` would reintroduce R87 into a safety
    /// decision. Deliberately counts live adapters only -- a
    /// queued/starting run with no adapter yet does not suppress idle
    /// shutdown, which is safe because such a run's submitting client is
    /// still connected (connections suppress idle independently) and an
    /// orphaned queued row is owned by boot recovery.
    fn active_run_count(&self) -> usize;
}

/// A deterministic driver for orchestration tests and fixtures: acknowledges
/// immediately and transitions `queued -> starting -> working`.
#[derive(Default)]
pub struct FakeRunDriver;

impl RunDriver for FakeRunDriver {
    fn start(&self, ctx: RunDriverContext) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async move {
            transition(&ctx, "starting").await?;
            transition(&ctx, "working").await?;
            Ok(())
        })
    }

    fn send_follow_up(
        &self,
        run_id: RunId,
        _task_id: TaskId,
        _worker_id: WorkerId,
        _prompt: String,
    ) -> AdapterFuture<'static, Result<(), String>> {
        Box::pin(async move {
            Err(format!(
                "fake driver does not support follow-up for run {run_id}"
            ))
        })
    }

    fn running_adapter(&self, _run_id: RunId) -> Option<Arc<dyn Adapter>> {
        None
    }

    fn cancel_run(
        &self,
        _run_id: RunId,
        _scope: CancelScope,
    ) -> AdapterFuture<'static, Result<CancelOutcome, String>> {
        // The fake driver never spawns an adapter, so there is never
        // anything to kill: the clean no-adapter outcome, not an error.
        Box::pin(async move { Ok(CancelOutcome::NoRunningAdapter) })
    }

    fn active_run_count(&self) -> usize {
        0
    }
}

async fn transition(ctx: &RunDriverContext, to: &str) -> Result<(), String> {
    let to_state = RunState::try_from(to).map_err(|e| e.to_string())?;
    let project_id = ctx.project_id;
    let run_id = ctx.run_id;
    let mut result = ctx
        .db
        .run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.transition_run(run_id, &to_state, None)
                .map(|committed| {
                    crate::domain::embed_envelope(
                        serde_json::json!({ "sequence": committed.sequence }),
                        &committed.envelope,
                    )
                })
        }))
        .await
        .map_err(|e| e.to_string())?;
    if let Some(envelope) = take_envelope(&mut result) {
        let _ = ctx.events_tx.send(envelope);
    }
    Ok(())
}
