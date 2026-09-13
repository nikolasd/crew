//! The real [`Adapter`] implementation for `AdapterMode::Protocol`:
//! spawns `claude` with [`super::launch::build_argv`]'s fixed launch
//! argv after [`super::trust::workspace_trust_accepted`] passes, hands its
//! stdout/stdin to [`super::reader::drive_turn`] for the whole turn --
//! including the initial prompt, which `drive_turn` itself delivers
//! only once its own `initialize` control-channel handshake completes,
//! not before -- and reconciles what that turn saw against claude's own
//! durable transcript via [`super::reconcile::find_gaps`] once it ends.
//!
//! This is the first real caller for three modules this spike built
//! ahead of it: `reconcile::find_gaps` (previously dead code, reachable
//! only from its own tests and the external drop-seam test),
//! `approval_bridge`'s whole module (previously `#![allow(dead_code)]`),
//! and `reader::drive_turn` itself. It is also the first real caller of
//! [`pane::PaneSupport`]: when a `TuiSupport` bundle was ever supplied
//! to the registry, [`Self::attach_pane`] opens a crew-rendered pane for
//! the turn, best-effort -- see `pane`'s own module doc comment for the
//! format and the provenance/redaction notes worth reading before
//! citing this pane's own text as evidence of anything.
//!
//! **Not in scope here** (see this spike's own PR body for the full
//! non-goals list): `resume`/`--continue`, the other three vendors, and
//! performance work.
//!
//! `--model` is threaded through as a plain, already-resolved parameter
//! this adapter does not itself resolve -- it is handed exactly
//! `profile.model`, the run's own resolved model, the same field the
//! fix for `build_tui_adapter`'s own model-resolution gap (that
//! adapter's boot-config snapshot silently outliving a later per-run
//! choice) threads through as `run_model`. There is no boot-loaded
//! per-vendor config for protocol mode to override in the first place
//! (no `crew.json` adapters map is ever read here), so that fix's
//! three-row precedence has nothing to collide with: this adapter's own
//! rule is simply "use it, trimmed, when non-empty; omit `--model`
//! otherwise and let claude's own default apply" -- [`Self::start`]'s
//! own test proves this against a REAL launched process's real argv,
//! not a configured value, mirroring that fix's own reproduction shape.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::oneshot;

use crew_protocol::{Classified, ContentClass, TurnOutcome};

use crate::adapter::AdapterFuture;
use crate::adapter::capability::AdapterCapabilities;
use crate::adapter::error::AdapterError;
use crate::adapter::event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink};
use crate::adapter::r#trait::{
    Adapter, AdapterMessage, AdapterSnapshot, CancelScope, ProbeResult, StartSpec, VendorSessionRef,
};
use crate::approval::ApprovalService;
use crate::supervisor::{EnvironmentPolicy, EscalationTimings, TerminationOutcome};

use super::approval_bridge::ProtocolApprovalCallback;
use super::reader::{self, RunIdentity};
use super::{pane, reconcile, trust};

/// Everything [`ClaudeProtocolAdapter`] needs that only exists after the
/// daemon's own `ApprovalService` is constructed -- the server-owned
/// instance `approval/decide` dispatches through, and the callback map
/// [`build_argv`]'s caller registered as `ServerConfig::approval_callback`
/// in its place. One instance is shared by every `ClaudeProtocolAdapter`
/// this daemon ever constructs -- an approval is looked up by its own
/// id, never by which adapter instance created it, so sharing one
/// callback map across runs is correct, not merely convenient (see
/// `approval_bridge::ProtocolApprovalCallback`'s own doc comment on why
/// a per-adapter registry keyed by worker id was rejected).
#[derive(Clone)]
pub(crate) struct ProtocolBundle {
    pub(crate) approval_service: Arc<ApprovalService>,
    pub(crate) callback: Arc<ProtocolApprovalCallback>,
}

/// Drives one `claude` process over its `stream-json` control channel.
pub(crate) struct ClaudeProtocolAdapter {
    repo_root: PathBuf,
    /// `WorkerProfile::environmentAllowlist` -- mirrors
    /// `ClaudeTuiVendor`'s own field of the same name exactly.
    environment_allowlist: Vec<String>,
    /// The already-resolved model to launch with, if any -- see this
    /// module's own doc comment on why resolving it is not this
    /// adapter's job.
    model: Option<String>,
    bundle: ProtocolBundle,
    /// Where to read claude's own trust record from --
    /// `trust::default_claude_json_path()` in production; a temp path in
    /// a test. `None` only when `HOME` could not be resolved, in which
    /// case [`Self::start`] treats the workspace as untrusted (fail
    /// closed, never guessed).
    claude_json_path: Option<PathBuf>,
    /// The binary [`Self::start`] spawns -- `"claude"` (resolved via
    /// `PATH`) in production, a fake script's path in a test. A plain
    /// field, injected at construction, rather than a `PATH`-shadowing
    /// trick or a `#[cfg(test)]` branch: the same shape
    /// `AdapterConfig.bin` already gives `ClaudeTuiVendor`, so a test can
    /// prove what actually reached `Command::new` without touching
    /// global process state.
    bin: String,
    /// The live child process, held for the duration of one turn so
    /// [`Adapter::cancel`] can reach it. `None` before the first
    /// [`Adapter::start`] call and after the turn ends. `Arc`-wrapped so
    /// [`Adapter::start`]'s own run-phase task (spawned, `'static`, and
    /// so unable to borrow `&self`) can hold the same handle `cancel`/
    /// `dispose`/`snapshot` reach through `self.child` -- one physical
    /// mutex, two owners.
    child: Arc<AsyncMutex<Option<tokio::process::Child>>>,
    /// Present only when a `TuiSupport` bundle was ever supplied to the
    /// registry -- see [`pane::PaneSupport`]'s own doc comment. `None`
    /// means this adapter runs with no pane at all, never a refusal to
    /// start.
    pane_support: Option<pane::PaneSupport>,
    /// How long [`settle_after_turn`] waits at each step of its
    /// SIGINT -> SIGTERM -> SIGKILL escalation -- production's own
    /// [`EscalationTimings::default`] (5s/5s, same as every other
    /// supervised process in this daemon); a test overrides it to keep
    /// an escalation test's own runtime bounded.
    escalation: EscalationTimings,
    /// How long [`settle_after_turn`] waits, unsignaled, for the
    /// ordinary case (claude exiting on its own) before it starts
    /// escalating at all -- production's own [`SELF_EXIT_GRACE`]; a test
    /// overrides it for the same reason `escalation` is overridable.
    self_exit_grace: Duration,
    /// Test-only seam: when `true`, [`Self::start`]'s run-phase task
    /// panics deliberately right after the handshake succeeds, so a test
    /// can prove the run-phase panic supervisor (not just an ordinary
    /// `Err`) still settles the run terminal -- see
    /// `a_run_phase_panic_after_the_handshake_still_settles_the_run`.
    /// Never reachable from production construction.
    #[cfg(test)]
    panic_in_run_phase: bool,
}

impl ClaudeProtocolAdapter {
    #[must_use]
    pub(crate) fn new(
        repo_root: PathBuf,
        environment_allowlist: Vec<String>,
        model: Option<String>,
        bundle: ProtocolBundle,
        pane_support: Option<pane::PaneSupport>,
    ) -> Self {
        Self {
            repo_root,
            environment_allowlist,
            model,
            bundle,
            claude_json_path: trust::default_claude_json_path(),
            bin: "claude".to_string(),
            child: Arc::new(AsyncMutex::new(None)),
            pane_support,
            escalation: EscalationTimings::default(),
            self_exit_grace: SELF_EXIT_GRACE,
            #[cfg(test)]
            panic_in_run_phase: false,
        }
    }

    /// Overrides where [`Self::start`] reads the trust record from and
    /// which binary it spawns -- a test-only seam, never reachable from
    /// production construction (`Self::new` above is the only
    /// `pub(crate)` constructor `super::super::registry` can see).
    #[cfg(test)]
    fn with_test_overrides(mut self, claude_json_path: PathBuf, bin: String) -> Self {
        self.claude_json_path = Some(claude_json_path);
        self.bin = bin;
        self
    }

    /// Overrides the escalation timings and self-exit grace window
    /// [`settle_after_turn`] waits out -- a test-only seam, so an
    /// escalation test does not have to pay production's real windows.
    #[cfg(test)]
    fn with_escalation_timings(
        mut self,
        self_exit_grace: Duration,
        escalation: EscalationTimings,
    ) -> Self {
        self.self_exit_grace = self_exit_grace;
        self.escalation = escalation;
        self
    }

    /// Test-only seam: makes [`Self::start`]'s run-phase task panic
    /// deliberately right after the handshake succeeds -- see
    /// [`Self::panic_in_run_phase`]'s own doc comment.
    #[cfg(test)]
    fn with_run_phase_panic(mut self) -> Self {
        self.panic_in_run_phase = true;
        self
    }

    fn env(&self) -> std::collections::HashMap<String, String> {
        let current: std::collections::HashMap<String, String> = std::env::vars().collect();
        EnvironmentPolicy::baseline().build(&current, &self.environment_allowlist)
    }

    /// Attaches this run's pane, if [`Self::pane_support`] was ever
    /// supplied -- best-effort: a failure here is logged and this
    /// adapter proceeds with no pane, never fails the turn (see
    /// [`pane::PaneSupport`]'s own doc comment on why this adapter's
    /// pane is a convenience, not a control surface).
    async fn attach_pane(&self, ids: RunIdentity) -> Option<AttachedPane> {
        let support = self.pane_support.as_ref()?;
        let (target, output_tx) = pane::PaneAttachTarget::new();
        let target = Arc::new(target);
        let socket_path = support.panes_dir.join(format!("{}.sock", ids.run_id));
        let attach_server = match crate::display::AttachServer::start(
            socket_path,
            Arc::clone(&target) as Arc<dyn crate::display::AttachTarget>,
            target.on_user_input(),
        ) {
            Ok(server) => server,
            Err(err) => {
                tracing::warn!(error = %err, run_id = %ids.run_id, "failed to start this run's attach server; continuing without a pane");
                return None;
            }
        };
        let outcome = support
            .pane_coordinator
            .attach(crate::display::PaneAttachRequest {
                run_id: ids.run_id,
                worker_id: ids.worker_id,
                adapter: self.kind().to_string(),
                placement: support.placement,
                forced_backend: support.forced_backend,
                launch_program: support.launch_program,
            })
            .await;
        // The banner is the operational form of this module's own
        // provenance note -- a viewer must never be able to read even
        // one line before knowing what this pane is (see
        // `pane::attach_banner`'s own doc comment).
        let _ = output_tx.send(pane::attach_banner());
        Some(AttachedPane {
            attach_server,
            outcome,
            output_tx,
        })
    }
}

/// Releases the pane [`ClaudeProtocolAdapter::attach_pane`] set up, if
/// any. Called on every exit path out of [`Adapter::start`]'s run-phase
/// task, success or error -- a free function, not a `&self` method,
/// because that task is `'static` (spawned) and only holds a clone of
/// [`pane::PaneSupport`], never `&ClaudeProtocolAdapter` itself.
async fn detach_pane(
    pane_support: Option<&pane::PaneSupport>,
    pane: AttachedPane,
    succeeded: bool,
) {
    if let Some(support) = pane_support {
        support
            .pane_coordinator
            .detach(&pane.outcome, succeeded, support.close_on_exit)
            .await;
    }
    pane.attach_server.stop();
}

/// Reconciles what [`reader::drive_turn`] actually saw against claude's
/// own transcript, journaling the result through `sink`. Best-effort: a
/// failure here is logged, never propagated -- the turn itself already
/// completed (or failed) by the time this runs, and a
/// reconciliation-journaling failure must not turn an otherwise-successful
/// turn into a failed run. A free function, not a `&self` method, for the
/// same reason as [`detach_pane`] above: it runs from the run-phase task.
async fn reconcile_turn(
    canonical_repo_root: &Path,
    outcome: &reader::TurnOutcome,
    sink: &Arc<dyn AdapterEventSink>,
    ids: RunIdentity,
) {
    let transcript = outcome.session_id.as_deref().and_then(|session_id| {
        std::fs::read(transcript_path(canonical_repo_root, session_id)).ok()
    });
    let (examined, gaps) = match &transcript {
        Some(bytes) => reconcile::find_gaps(bytes, &outcome.entry_ids),
        None => (0, Vec::new()),
    };
    // `examined == 0` is itself a finding (`reconcile::find_gaps`'s own
    // doc comment): a turn this adapter cannot reconcile at all (no
    // session id ever arrived, the transcript file does not exist yet,
    // or every line in it failed to parse) must never be journaled as an
    // ordinary, successful reconciliation -- it is reported as a
    // distinguishable protocol-health failure instead, never silently
    // folded into `gaps_found: 0`, which would read identically to
    // "checked, and found nothing wrong."
    let payload = if examined == 0 {
        AdapterEventPayload::ProtocolHealthChanged {
            healthy: false,
            detail: Classified {
                class: ContentClass::Visible,
                value: "could not reconcile this turn against claude's own transcript \
                        (no entries were readable)"
                    .to_string(),
            },
        }
    } else {
        AdapterEventPayload::ReconciliationCompleted {
            examined,
            gaps_found: gaps.len() as u64,
            // No repair mechanism exists yet -- this adapter only
            // detects and reports a gap, it does not yet re-journal a
            // missed entry. Left at `0` rather than guessed.
            gaps_repaired: 0,
        }
    };
    if let Err(err) = sink
        .emit(AdapterEvent {
            run_id: ids.run_id,
            task_id: ids.task_id,
            worker_id: ids.worker_id,
            payload,
            cursor: None,
        })
        .await
    {
        tracing::warn!(error = %err, run_id = %ids.run_id, "failed to journal claude-protocol reconciliation");
    }
}

/// What [`ClaudeProtocolAdapter::attach_pane`] set up, kept alive for the
/// duration of one turn so [`detach_pane`] can release it afterward.
/// `output_tx` is also read from directly, by
/// [`ClaudeProtocolAdapter::start`], to hand `reader::drive_turn` a
/// place to push formatted lines into.
struct AttachedPane {
    attach_server: crate::display::AttachServer,
    outcome: crate::display::PaneAttachOutcome,
    output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
}

/// `~/.claude/projects/<slug_cwd(canonical_repo_root)>/<session_id>.jsonl`
/// -- the same on-disk layout
/// `crate::adapter::tui::claude::ClaudeTuiVendor::transcript_root`
/// resolves for the TUI path, reused verbatim rather than
/// re-derived: both adapters read the identical vendor-owned directory,
/// just at different times (this one once, after the turn; that one
/// continuously, while it runs). `HOME` unresolved falls back to
/// `/root`, matching that function's own precedent.
fn transcript_path(canonical_repo_root: &Path, session_id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(crate::adapter::tui::claude::slug_cwd(canonical_repo_root))
        .join(format!("{session_id}.jsonl"))
}

/// How long a completed turn's own claude process gets to exit on its
/// own before [`settle_after_turn`] starts signaling it. `drive_turn`
/// dropping its own `stdin`/`stdout` handles when it returns closes
/// this adapter's write half of the pipe, which SHOULD make claude see
/// EOF and exit -- a live capture's own call 9 reasoned this, but never
/// independently tested it (that call's own harness never closed its
/// side of stdin, unlike this adapter's `drive_turn`, so it could not
/// observe this specifically). This window exists so that reasoning is
/// never load-bearing: whether or not it holds, a process still running
/// after it gets escalated exactly like a wedged process anywhere else
/// in this daemon would.
const SELF_EXIT_GRACE: Duration = Duration::from_secs(5);

/// Reaps `child` (if still present -- a no-op otherwise) via the normal
/// escalation ladder and emits `ProcessExited`, best-effort. The shared
/// tail of every "this run must still end up terminal even though the
/// ordinary success path never reached its own `ProcessExited` emit"
/// case [`Adapter::start`]'s run-phase task and its panic supervisor
/// both hit: a post-handshake [`reader::drive_turn`] `Err` (nothing
/// upstream of it settles the child in that case, matching this
/// function's own pre-split behavior for a pre-handshake `Err` exactly
/// -- see `start`'s own doc comment) and a post-handshake panic (this
/// run's own supervisor learns of it via a [`tokio::task::JoinHandle`],
/// not by observing this function itself).
async fn settle_and_emit_exit_best_effort(
    child: &Arc<AsyncMutex<Option<tokio::process::Child>>>,
    sink: &Arc<dyn AdapterEventSink>,
    ids: RunIdentity,
    self_exit_grace: Duration,
    escalation: EscalationTimings,
) {
    let termination = {
        let mut guard = child.lock().await;
        match guard.as_mut() {
            Some(child) => Some(settle_after_turn(child, self_exit_grace, escalation).await),
            None => None,
        }
    };
    *child.lock().await = None;
    if let Some(termination) = termination {
        let (exit_code, signal) = termination.exit_signals();
        let _ = sink
            .emit(AdapterEvent {
                run_id: ids.run_id,
                task_id: ids.task_id,
                worker_id: ids.worker_id,
                payload: AdapterEventPayload::ProcessExited { exit_code, signal },
                cursor: None,
            })
            .await;
    }
}

/// Waits for `child` to exit on its own, escalating
/// SIGINT -> SIGTERM -> SIGKILL on `escalation`'s own timings if it does
/// not -- the same discipline
/// `crate::supervisor::process::ManagedProcess::terminate` applies to
/// every other supervised process in this daemon, reused here as free
/// functions over a bare `tokio::process::Child` rather than by
/// adopting `ManagedProcess`/`Supervisor` themselves: `drive_turn`'s own
/// generic `AsyncRead`/`AsyncWrite` interface (and the fast, in-memory
/// `tokio::io::duplex`-based tests built on it) is the reason this
/// adapter still spawns via a bare `tokio::process::Command` rather than
/// `Supervisor::spawn` -- a real gap against this module's own stated
/// invariant ("every adapter launches its supervised vendor process
/// through this module"), carried forward rather than fixed here: fixing
/// it would mean rebuilding `drive_turn` over `ManagedProcess`'s framed
/// stdout/queued-stdin API instead, which is a larger change than "give
/// `child.wait()` a deadline" asks for.
///
/// Signals are sent to `child`'s own pid directly, never a process
/// group: unlike `Supervisor::spawn`'s children, this spawn was never
/// given a process group of its own (see the gap above), and claude's
/// own `-p` invocation is a single non-interactive process, not an
/// interactive shell expected to leave orphaned, signal-ignoring
/// grandchildren behind the way a PTY-hosted shell can.
async fn settle_after_turn(
    child: &mut tokio::process::Child,
    self_exit_grace: Duration,
    escalation: EscalationTimings,
) -> TerminationOutcome {
    if let Ok(Some(status)) = child.try_wait() {
        return TerminationOutcome::Exited {
            code: status.code(),
        };
    }

    if let Some(outcome) = wait_step(child, self_exit_grace).await {
        return outcome;
    }

    let Some(pid) = child.id().map(|id| id as i32) else {
        // Already reaped by something else between the checks above and
        // here -- nothing left to signal; `wait()`'s own error, if any,
        // is reported as an exit of unknown code rather than escalated
        // further, matching `ManagedProcess::terminate`'s own precedent
        // for this situation.
        return match child.wait().await {
            Ok(status) => TerminationOutcome::Exited {
                code: status.code(),
            },
            Err(_) => TerminationOutcome::Exited { code: None },
        };
    };

    let _ = kill(Pid::from_raw(pid), Signal::SIGINT);
    if let Some(outcome) = wait_step(child, escalation.sigint_to_sigterm).await {
        return outcome;
    }

    let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    if let Some(outcome) = wait_step(child, escalation.sigterm_to_sigkill).await {
        return outcome;
    }

    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    let _ = child.wait().await;
    TerminationOutcome::Killed
}

/// Waits out `duration` for `child` to exit, without signaling it.
/// `Some` only on a confirmed exit within the window; `None` for either
/// a timeout (still running) or a `wait()` error (state unknown) --
/// neither is a confirmed exit, so both mean the caller should keep
/// escalating, matching
/// `crate::supervisor::process::ManagedProcess::wait_out_step`'s own
/// stance on the identical question.
async fn wait_step(
    child: &mut tokio::process::Child,
    duration: Duration,
) -> Option<TerminationOutcome> {
    match tokio::time::timeout(duration, child.wait()).await {
        Ok(Ok(status)) => Some(TerminationOutcome::Exited {
            code: status.code(),
        }),
        Ok(Err(_)) | Err(_) => None,
    }
}

impl Adapter for ClaudeProtocolAdapter {
    fn kind(&self) -> &str {
        "claude"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        super::declared_capabilities()
    }

    /// Spawns `claude --version` directly -- no model call, matching
    /// `TuiAdapter::probe`'s own committed shape (a synchronous
    /// `std::process::Command` inside this async fn) exactly, for the
    /// same reason: a probe is inherently a one-shot version check, not
    /// a long-lived stream this adapter needs to hold open.
    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        Box::pin(async move {
            let output = std::process::Command::new(&self.bin)
                .arg("--version")
                .output()
                .map_err(|e| AdapterError::unavailable(self.kind(), "probe", e.to_string()))?;
            if !output.status.success() {
                return Err(AdapterError::unavailable(
                    self.kind(),
                    "probe",
                    "claude --version exited non-zero",
                ));
            }
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(ProbeResult {
                version: Some(version),
                auth_ready: true,
                capabilities: self.capabilities(),
                inventory_incomplete: true,
            })
        })
    }

    /// Two phases, matching [`Adapter::start`]'s own contract exactly:
    /// **up** -- everything below through the initialize handshake --
    /// runs inline and is what this future's own `.await` resolves on;
    /// **run** -- the prompt delivery, the rest of the turn, teardown,
    /// and reconciliation -- runs in a spawned, `'static` task this
    /// method never waits on, reported through `sink` alone from then
    /// on (the "named component" the trait doc comment requires: this
    /// adapter's own run-phase task).
    ///
    /// Before this split, this function awaited the whole turn inline,
    /// which is what made `run/submit`'s own JSON-RPC response (which
    /// waits on exactly this future, through `RunDriver::start`) block
    /// for a protocol-mode run's entire duration -- which in turn holds
    /// the daemon's single per-connection dispatch loop
    /// (`ipc/connection.rs`'s own read-dispatch-respond loop) for that
    /// same duration, stalling every other request on that connection,
    /// including the extension's own event-enrichment calls.
    ///
    /// A failure during spawn, the trust precheck, or the handshake
    /// itself is still returned synchronously from THIS future, exactly
    /// as before the split -- `orchestration.rs`'s own
    /// `abandon_and_announce`/`ensure_failed_after_start_error` backstop
    /// still reaches it. A failure or panic in the run-phase task AFTER
    /// the handshake is this run's own to settle (this method has
    /// already returned `Ok` by then): the task's own tail does that for
    /// an ordinary `Err`, and a dedicated supervisor task (spawned
    /// alongside it, watching its `JoinHandle`) does it for a panic --
    /// see [`settle_and_emit_exit_best_effort`]. One deliberate behavior
    /// change from before the split, named here rather than left
    /// implicit: a run-phase failure now leaves this run's workspace
    /// lease held (released only by an explicit `workspace/release` or a
    /// later `run/retry`'s abandonment), exactly like a `TuiAdapter`
    /// run-phase failure already does -- before, ANY failure inside this
    /// function, including mid-turn, abandoned the lease via
    /// `orchestration.rs`'s own start-error path. See
    /// `a_post_handshake_failure_leaves_the_lease_held_like_tui_does` for
    /// the parity this is pinned to (a record of today's behavior, not a
    /// ruling that it is correct -- that is the pending leases ADR's own
    /// question).
    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let ids = RunIdentity {
                run_id: spec.run_id,
                task_id: spec.task_id,
                worker_id: spec.worker_id,
            };
            let canonical_repo_root =
                std::fs::canonicalize(&self.repo_root).unwrap_or_else(|_| self.repo_root.clone());

            let trusted = self
                .claude_json_path
                .as_deref()
                .is_some_and(|path| trust::workspace_trust_accepted(path, &canonical_repo_root));
            if !trusted {
                sink.emit(AdapterEvent {
                    run_id: ids.run_id,
                    task_id: ids.task_id,
                    worker_id: ids.worker_id,
                    payload: AdapterEventPayload::WorkspaceTrustPending,
                    cursor: None,
                })
                .await?;
                return Err(AdapterError::auth_required(
                    self.kind(),
                    "start",
                    "claude's one-time workspace-trust prompt for this repository has not been \
                     accepted yet; trust it once from an interactive claude session in this \
                     repository, then resubmit",
                ));
            }

            // Attached before the process spawns, so a viewer sees every
            // line from the very first one -- best-effort: unlike a
            // `TuiAdapter`, this adapter's pane is a convenience view, not
            // its control surface, so a failure to attach never fails the
            // turn, only logs and proceeds without one. Handed into the
            // run-phase task below (owned, not borrowed): this "up"
            // phase never detaches it itself, on any path -- see that
            // task's own tail.
            let pane = self.attach_pane(ids).await;

            let model = self
                .model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty());
            let argv = super::launch::build_argv(model);
            let env = self.env();

            let mut command = tokio::process::Command::new(&self.bin);
            command
                .args(&argv)
                .current_dir(&canonical_repo_root)
                .env_clear()
                .envs(&env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(e) => {
                    if let Some(pane) = pane {
                        detach_pane(self.pane_support.as_ref(), pane, false).await;
                    }
                    return Err(AdapterError::process(self.kind(), "start", e.to_string()));
                }
            };
            let pid = child.id().unwrap_or(0);
            let stdin = child
                .stdin
                .take()
                .expect("stdin was requested as piped at spawn");
            let stdout = child
                .stdout
                .take()
                .expect("stdout was requested as piped at spawn");

            *self.child.lock().await = Some(child);

            if let Err(err) = sink
                .emit(AdapterEvent {
                    run_id: ids.run_id,
                    task_id: ids.task_id,
                    worker_id: ids.worker_id,
                    payload: AdapterEventPayload::ProcessStarted { pid },
                    cursor: None,
                })
                .await
            {
                // Still pre-handshake: returned synchronously, exactly
                // like every other failure above. The spawned child is
                // left in `self.child` for `dispose`/a future `cancel`
                // to reach, same as this function's pre-split behavior
                // for this exact failure (nothing here ever reaped it).
                if let Some(pane) = pane {
                    detach_pane(self.pane_support.as_ref(), pane, false).await;
                }
                return Err(err);
            }

            // --- "run" phase from here: spawned so the "up" phase above
            // can return once the handshake completes, without waiting
            // for the rest of the turn (see this method's own doc
            // comment).
            let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AdapterError>>();
            let handshake_done = Arc::new(AtomicBool::new(false));

            let task_child = Arc::clone(&self.child);
            let task_sink = Arc::clone(&sink);
            let task_approval_service = Arc::clone(&self.bundle.approval_service);
            let task_callback = Arc::clone(&self.bundle.callback);
            let task_self_exit_grace = self.self_exit_grace;
            let task_escalation = self.escalation;
            let task_canonical_repo_root = canonical_repo_root.clone();
            let task_prompt = spec.prompt.clone();
            let task_pane_output = pane.as_ref().map(|p| p.output_tx.clone());
            let task_pane_support = self.pane_support.clone();
            let task_handshake_done = Arc::clone(&handshake_done);
            #[cfg(test)]
            let task_panic_in_run_phase = self.panic_in_run_phase;

            let join_handle = tokio::spawn(async move {
                let mut ready = Some(ready_tx);

                let result: Result<(), AdapterError> = async {
                    let outcome = reader::drive_turn(
                        stdout,
                        stdin,
                        &task_sink,
                        &task_approval_service,
                        &task_callback,
                        ids,
                        task_pane_output.as_ref(),
                        &task_prompt,
                        &mut ready,
                        &task_handshake_done,
                    )
                    .await?;

                    // Test-only seam (`with_run_phase_panic`): proves the
                    // panic supervisor below, not just the ordinary `Err`
                    // path, still settles this run terminal.
                    #[cfg(test)]
                    if task_panic_in_run_phase {
                        panic!(
                            "deliberate test panic in the run phase, after the handshake \
                             (a with_run_phase_panic test seam)"
                        );
                    }

                    // The turn boundary, parity with `tui/adapter.rs`'s
                    // own `TurnEnded` emission (`TuiEvent::TurnEnded
                    // { outcome }` at that module's line ~1687).
                    // `drive_turn` returning `Ok` here IS this adapter's
                    // turn-boundary evidence -- claude's `-p` invocation
                    // only returns once its own turn is over, there is
                    // no separate "holding at its prompt" signal to wait
                    // for the way a TUI transcript tail has to watch for
                    // one. Emitted before `settle_after_turn`/
                    // `ProcessExited` below, matching the TUI path's own
                    // ordering. Always `TurnOutcome::Normal`: this
                    // adapter does not yet distinguish an API-error-ended
                    // turn from an ordinary one (a narrower follow-up,
                    // not this fix's scope).
                    task_sink
                        .emit(AdapterEvent {
                            run_id: ids.run_id,
                            task_id: ids.task_id,
                            worker_id: ids.worker_id,
                            payload: AdapterEventPayload::TurnEnded {
                                outcome: TurnOutcome::Normal,
                            },
                            cursor: None,
                        })
                        .await?;

                    let termination = {
                        let mut guard = task_child.lock().await;
                        let child = guard.as_mut().ok_or_else(|| {
                            AdapterError::invalid_vendor_state(
                                "claude",
                                "start",
                                "the spawned child process handle was missing at reap time",
                            )
                        })?;
                        settle_after_turn(child, task_self_exit_grace, task_escalation).await
                    };
                    *task_child.lock().await = None;
                    let (exit_code, signal) = termination.exit_signals();

                    task_sink
                        .emit(AdapterEvent {
                            run_id: ids.run_id,
                            task_id: ids.task_id,
                            worker_id: ids.worker_id,
                            payload: AdapterEventPayload::ProcessExited { exit_code, signal },
                            cursor: None,
                        })
                        .await?;

                    reconcile_turn(&task_canonical_repo_root, &outcome, &task_sink, ids).await;

                    Ok(())
                }
                .await;

                // Captured before `result` is potentially moved into the
                // oneshot send below -- `detach_pane`'s own `succeeded`
                // needs it regardless of which arm runs.
                let succeeded = result.is_ok();

                match ready.take() {
                    Some(tx) => {
                        // The handshake never completed -- `start`'s
                        // caller (still waiting on `ready_rx`) gets this
                        // exact result. Always `Err` in practice (see
                        // `drive_turn`'s own doc comment: `Ok` is never
                        // returned without consuming `ready` first), and
                        // nothing above was reaped on this path, matching
                        // this function's pre-split behavior for the
                        // identical failure exactly (a start-time error,
                        // backstopped by orchestration's own
                        // `ensure_failed_after_start_error`).
                        let _ = tx.send(result);
                    }
                    None => {
                        // The handshake already completed -- `start`
                        // already returned `Ok`. An `Err` here is this
                        // run's own to settle terminal; nothing else
                        // will (see this method's own doc comment).
                        if let Err(err) = &result {
                            tracing::warn!(
                                error = %err,
                                run_id = %ids.run_id,
                                "claude-protocol run phase failed after the handshake; settling this run terminal directly"
                            );
                            settle_and_emit_exit_best_effort(
                                &task_child,
                                &task_sink,
                                ids,
                                task_self_exit_grace,
                                task_escalation,
                            )
                            .await;
                        }
                    }
                }

                if let Some(pane) = pane {
                    detach_pane(task_pane_support.as_ref(), pane, succeeded).await;
                }
            });

            // A panic in the task above -- rather than an ordinary
            // `Err` -- must still not leave this run non-terminal
            // forever if it happened after the handshake (see this
            // method's own doc comment). Watched from a separate task so
            // this "up" phase itself only ever waits on `ready_rx` below,
            // never on the run-phase task's own completion.
            let supervisor_child = Arc::clone(&self.child);
            let supervisor_sink = Arc::clone(&sink);
            let supervisor_self_exit_grace = self.self_exit_grace;
            let supervisor_escalation = self.escalation;
            tokio::spawn(async move {
                if let Err(join_err) = join_handle.await {
                    tracing::error!(
                        error = %join_err,
                        run_id = %ids.run_id,
                        "claude-protocol run-phase task panicked"
                    );
                    if handshake_done.load(Ordering::SeqCst) {
                        settle_and_emit_exit_best_effort(
                            &supervisor_child,
                            &supervisor_sink,
                            ids,
                            supervisor_self_exit_grace,
                            supervisor_escalation,
                        )
                        .await;
                    }
                    // Else: the panic happened before the handshake
                    // completed -- `ready_tx` was dropped without
                    // sending, `ready_rx.await` below already saw
                    // `Err` (a closed channel) and this "up" phase
                    // already returned its own `Err`; orchestration's
                    // existing synchronous backstop
                    // (`ensure_failed_after_start_error`) covers it,
                    // same as any other start-time failure.
                }
            });

            match ready_rx.await {
                Ok(result) => result,
                Err(_closed) => Err(AdapterError::process(
                    self.kind(),
                    "start",
                    "the run-phase task ended before it could report whether claude's initialize \
                     handshake completed",
                )),
            }
        })
    }

    fn resume(
        &self,
        _session: VendorSessionRef,
        _sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Err(AdapterError::capability_unsupported(self.kind(), "resume")) })
    }

    /// Committed scope is one worker, one turn: no steering, no
    /// follow-up, no out-of-band answer delivery yet, matching
    /// `declared_capabilities().steering`'s own `None`.
    fn send(&self, _message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Err(AdapterError::capability_unsupported(self.kind(), "send")) })
    }

    /// The real bridge exists (`super::approval_bridge`), reached from
    /// `super::reader::drive_turn` while a turn is live -- this method
    /// is the same trait-level seam every other adapter's own
    /// `respond_to_approval` is (nothing calls it in production;
    /// `ApprovalService::decide` calls `ApprovalCallback::acknowledge`
    /// directly instead, per that trait's own doc comment). Kept a typed
    /// refusal here rather than duplicating the bridge behind a second
    /// entry point.
    fn respond_to_approval(&self, _approval_id: &str, _decision: &str) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            Err(AdapterError::capability_unsupported(
                self.kind(),
                "respond_to_approval",
            ))
        })
    }

    fn cancel(&self, _scope: CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut guard = self.child.lock().await;
            if let Some(child) = guard.as_mut() {
                child
                    .kill()
                    .await
                    .map_err(|e| AdapterError::process(self.kind(), "cancel", e.to_string()))?;
            }
            Ok(())
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async move {
            let running = self.child.lock().await.is_some();
            Ok(AdapterSnapshot {
                state_summary: if running {
                    "running".to_string()
                } else {
                    "idle".to_string()
                },
                children: Vec::new(),
                usage: None,
                artifacts: Vec::new(),
            })
        })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut guard = self.child.lock().await;
            if let Some(child) = guard.as_mut() {
                let _ = child.kill().await;
            }
            *guard = None;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc as StdArc;

    use tokio::sync::broadcast;

    use crew_protocol::{RunId, TaskId, WorkerId};

    use crate::approval::NoopApprovalCallback;
    use crate::db::DatabaseHandle;

    /// A minimal in-memory sink that just records every payload it was
    /// handed -- the same small fixture `reader.rs`'s own test module
    /// defines, duplicated here rather than exported across a private
    /// module boundary for one shared use.
    struct RecordingSink {
        events: StdArc<parking_lot::Mutex<Vec<AdapterEventPayload>>>,
    }

    impl AdapterEventSink for RecordingSink {
        fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
            self.events.lock().push(event.payload);
            Box::pin(async { Ok(0) })
        }

        fn note_real_user_turn(&self, _run_id: RunId) -> AdapterFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A fake `claude` that dumps its own received argv (one entry per
    /// line) to `argv_path` before doing anything else, then emits just
    /// enough `stream-json` to let [`Adapter::start`] complete: a
    /// `system/init` line (so reconciliation has a session id to look
    /// for, even though no transcript file will exist for it) and a
    /// `result` line. Mirrors
    /// `crates/runtime/tests/tui_claude_registry.rs`'s own
    /// `write_argv_recording_claude_script` -- the same proof shape
    /// (read back what was actually executed, not a stored value that
    /// could look right while the real spawn used something else), a
    /// stream-json body instead of a PTY transcript.
    fn write_argv_recording_fake_claude(
        dir: &std::path::Path,
        argv_path: &std::path::Path,
    ) -> PathBuf {
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > "{argv_path}"
read -r _first_line
echo '{{"type":"system","subtype":"init","session_id":"sess-argv-test","claude_code_version":"2.1.268","permissionMode":"auto"}}'
echo '{{"type":"result","subtype":"success"}}'
"#,
            argv_path = argv_path.display(),
        );
        let path = dir.join("fake-claude.sh");
        std::fs::write(&path, script).expect("write fake claude script");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// Keyed on the CANONICALIZED repo root -- `Self::start` canonicalizes
    /// before checking (matching `ClaudeTuiVendor::transcript_root`'s own
    /// precedent that a real recording proved necessary: a tempdir under
    /// `/tmp`/`/var` is frequently a symlink to `/private/tmp`/
    /// `/private/var` on macOS, so the raw and canonical paths can
    /// genuinely differ even for a path this test itself just created).
    fn trusted_claude_json(dir: &std::path::Path, repo_root: &std::path::Path) -> PathBuf {
        let canonical =
            std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let path = dir.join(".claude.json");
        let contents = serde_json::json!({
            "projects": {
                canonical.to_string_lossy().to_string(): { "hasTrustDialogAccepted": true },
            }
        });
        std::fs::write(&path, contents.to_string()).unwrap();
        path
    }

    /// The same reproduction shape the `build_tui_adapter` model-resolution
    /// fix used: assert against the ARGV THE REAL SPAWN RECEIVED, read
    /// back from a file the fake binary itself wrote, never a
    /// configured/stored value that could look right while the actual
    /// launch used something else. Proves `--model` reaches the real
    /// launch for this adapter the same way that fix proved it for
    /// `TuiAdapter`.
    #[tokio::test]
    async fn the_resolved_model_reaches_the_real_launched_argv() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let argv_path = dir.path().join("argv.txt");
        let bin = write_argv_recording_fake_claude(dir.path(), &argv_path);
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());

        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            Some("claude-sonnet-5".to_string()),
            ProtocolBundle {
                approval_service,
                callback,
            },
            None,
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string());

        let events = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::clone(&events),
        });

        adapter
            .start(
                StartSpec {
                    run_id: RunId::new(),
                    task_id: TaskId::new(),
                    worker_id: WorkerId::new(),
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            )
            .await
            .expect("start must succeed against the fake binary");

        let recorded_argv = std::fs::read_to_string(&argv_path).expect("fake binary must have run");
        let argv: Vec<&str> = recorded_argv.lines().collect();
        assert!(
            argv.windows(2).any(|w| w == ["--model", "claude-sonnet-5"]),
            "expected --model claude-sonnet-5 in the real launched argv, got: {argv:?}"
        );

        db.shutdown().await.ok();
    }

    /// A pane backend that always succeeds with a non-empty pane ref --
    /// mirrors `tests/tui_claude_registry.rs`'s own `FakeBackend`
    /// fixture exactly, for the identical reason: `HiddenDisplay`'s own
    /// `pane_ref` is empty, which would make "a real pane attached"
    /// indistinguishable from "no real pane at all" in this test's own
    /// assertions.
    struct FakePaneBackend;

    impl crate::display::DisplayBackendTrait for FakePaneBackend {
        fn backend_name(&self) -> &str {
            "tmux"
        }

        fn is_available(&self) -> bool {
            true
        }

        fn activate(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn status(&self) -> crew_protocol::DisplayStatus {
            crew_protocol::DisplayStatus::new(crew_protocol::DisplayBackend::Tmux, true, false)
        }

        fn create_pane(
            &self,
            req: crate::display::PaneRequest,
        ) -> crate::display::DisplayFuture<'_, crate::display::PaneHandle> {
            let handle = crate::display::PaneHandle {
                backend: crew_protocol::DisplayBackend::Tmux,
                pane_ref: "fake-pane-1".to_string(),
                placement: req.placement,
            };
            Box::pin(async move { Ok(handle) })
        }

        fn close_pane(
            &self,
            _handle: &crate::display::PaneHandle,
        ) -> crate::display::DisplayFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// End-to-end proof that `Self::attach_pane`/`detach_pane`
    /// actually reach a real `PaneCoordinator`: drives a real turn (the
    /// same fake-script harness as the model-argv test above) with a
    /// real pane wired in, then asserts a real `DisplayPaneAttached` and
    /// `DisplayPaneDetached` were journaled with the fake backend's own
    /// non-empty pane ref -- not that the two pieces were built to
    /// agree, but that wiring them together actually reaches the
    /// journal.
    ///
    /// Waits for the detach (`wait_for`) rather than asserting the
    /// moment `adapter.start(...)` returns: the pane detach now happens
    /// inside the spawned run-phase task, which `start` no longer waits
    /// on (it returns once the initialize handshake completes).
    #[tokio::test]
    async fn a_real_turn_attaches_and_detaches_a_real_pane() {
        // `tempdir_in("/tmp")`, not the bare `TempDir::new()` default
        // (`std::env::temp_dir()`, a deeply-nested path on macOS): the
        // attach socket this test binds lives under this directory, and
        // a `run_id`-length UUID appended to that deep a path overflows
        // the platform `sun_path` limit -- the same reason
        // `tests/tui_claude_registry.rs`'s own harness uses this exact
        // override.
        let dir = tempfile::Builder::new()
            .prefix("bat-pane-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let argv_path = dir.path().join("argv.txt");
        let bin = write_argv_recording_fake_claude(dir.path(), &argv_path);
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());

        let mut display_registry = crate::display::DisplayRegistry::new();
        display_registry.register(Box::new(FakePaneBackend));
        let (events_tx, _events_rx) = broadcast::channel(64);
        let pane_coordinator = Arc::new(crate::display::PaneCoordinator::new(
            Arc::new(display_registry),
            StdArc::clone(&db),
            project_id,
            events_tx,
            std::path::PathBuf::from("/opt/crew/bin/crewd"),
            dir.path().to_path_buf(),
            repo_root.clone(),
            crate::security::redaction::Redactor::new(),
        ));
        let panes_dir = dir.path().join("panes");
        std::fs::create_dir_all(&panes_dir).unwrap();

        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            None,
            ProtocolBundle {
                approval_service,
                callback,
            },
            Some(pane::PaneSupport {
                pane_coordinator,
                panes_dir,
                placement: crew_protocol::DisplayPlacement::SplitRight,
                forced_backend: None,
                launch_program: None,
                close_on_exit: crate::config::crew::CloseOnExit::Always,
            }),
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string());

        let events = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink { events });

        // `PaneCoordinator::attach`/`detach` journal against a real
        // `runs` row (a foreign-key reference, invariant 3) -- unlike
        // `RecordingSink`, they write through the real `db` this test
        // constructed, so the row has to actually exist first, the same
        // seeding `tests/tui_claude_registry.rs`'s own
        // `seed_worker_and_run` does.
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new({
            let task_id = task_id.to_string();
            let worker_id = worker_id.to_string();
            let run_id = run_id.to_string();
            let project_id = project_id.to_string();
            move |conn| {
                conn.execute(
                    "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at) \
                     VALUES (?1, ?2, 'test-owner', 1, ?3, ?3)",
                    rusqlite::params![task_id, project_id, "2026-01-01T00:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope) \
                     VALUES (?1, 'sha256:test', 'claude', 'test-model', '{}')",
                    rusqlite::params![worker_id.clone()],
                )?;
                conn.execute(
                    "INSERT INTO workers (worker_id, project_id, profile_id, resolved_profile_json, created_at) \
                     VALUES (?1, ?2, ?1, '{}', ?3)",
                    rusqlite::params![worker_id, project_id, "2026-01-01T00:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO runs (run_id, task_id, worker_id, state, created_at) \
                     VALUES (?1, ?2, ?3, 'queued', ?4)",
                    rusqlite::params![run_id, task_id, worker_id, "2026-01-01T00:00:00Z"],
                )?;
                Ok(serde_json::Value::Null)
            }
        }))
        .await
        .expect("seed task/worker/run");

        adapter
            .start(
                StartSpec {
                    run_id,
                    task_id,
                    worker_id,
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            )
            .await
            .expect(
                "start (the up phase) must succeed against the fake binary and fake pane backend",
            );

        async fn fetch_dump(db: &DatabaseHandle, run_id: RunId) -> Vec<String> {
            let run_id_string = run_id.to_string();
            db.run_domain_op(Box::new(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
                let rows = stmt
                    .query_map(rusqlite::params![run_id_string], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(serde_json::json!(rows))
            }))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
        }

        wait_for(Duration::from_secs(5), || async {
            fetch_dump(&db, run_id)
                .await
                .iter()
                .any(|e| e.contains("displayPaneDetached"))
        })
        .await;

        let dump = fetch_dump(&db, run_id).await;

        assert!(
            dump.iter()
                .any(|e| e.contains("displayPaneAttached") && e.contains("fake-pane-1")),
            "expected a DisplayPaneAttached event naming the fake backend's own pane ref, got: {dump:#?}"
        );
        assert!(
            dump.iter().any(|e| e.contains("displayPaneDetached")),
            "expected a DisplayPaneDetached event once the turn settled, got: {dump:#?}"
        );

        db.shutdown().await.ok();
    }

    /// A `claude` that ignores SIGINT and SIGTERM outright and never
    /// exits on its own -- [`settle_after_turn`]'s own escalation ladder
    /// is the only thing that ever reaps it, and only reaches SIGKILL
    /// after climbing through both prior steps: this is what makes the
    /// call 9 stdin-drop reasoning a non-assumption, per the maintainer's
    /// own ruling on this ("do not rely on it; bound the wait and
    /// escalate instead").
    fn write_wedged_fake_claude(dir: &std::path::Path) -> PathBuf {
        let script = r#"#!/bin/sh
trap '' INT TERM
read -r _first_line
echo '{"type":"system","subtype":"init","session_id":"sess-wedged","claude_code_version":"2.1.268","permissionMode":"auto"}'
echo '{"type":"result","subtype":"success"}'
while true; do sleep 1; done
"#;
        let path = dir.join("wedged-claude.sh");
        std::fs::write(&path, script).expect("write wedged fake claude script");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// End to end proof that a claude process which never exits on its
    /// own, and ignores both SIGINT and SIGTERM, still gets reaped: the
    /// turn completes normally (the protocol's own `result` line
    /// arrived), and the run-phase task still settles rather than hanging
    /// forever on `child.wait()`, with the final `ProcessExited` event
    /// reporting the SIGKILL escalation actually needed.
    ///
    /// `adapter.start(...)` itself now returns as soon as the handshake
    /// completes (the up/run split -- see `Self::start`'s own doc
    /// comment) -- long before this wedged process is ever escalated --
    /// so this test's own bounded wait moved from `start`'s own return
    /// to `wait_for` polling the recorded sink events for the SIGKILL
    /// exit directly.
    #[tokio::test]
    async fn a_wedged_process_is_escalated_to_sigkill_rather_than_hung_on() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let bin = write_wedged_fake_claude(dir.path());
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());

        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            None,
            ProtocolBundle {
                approval_service,
                callback,
            },
            None,
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string())
        .with_escalation_timings(
            Duration::from_millis(50),
            EscalationTimings {
                sigint_to_sigterm: Duration::from_millis(50),
                sigterm_to_sigkill: Duration::from_millis(50),
            },
        );

        let events = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::clone(&events),
        });

        tokio::time::timeout(
            Duration::from_secs(5),
            adapter.start(
                StartSpec {
                    run_id: RunId::new(),
                    task_id: TaskId::new(),
                    worker_id: WorkerId::new(),
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            ),
        )
        .await
        .expect("start (the up phase) must not hang: it returns once the handshake completes")
        .expect("start must still succeed: the handshake completed normally");

        wait_for(Duration::from_secs(10), || async {
            events
                .lock()
                .iter()
                .any(|payload| matches!(payload, AdapterEventPayload::ProcessExited { .. }))
        })
        .await;

        {
            let recorded = events.lock();
            let exited = recorded.iter().find_map(|payload| match payload {
                AdapterEventPayload::ProcessExited { exit_code, signal } => {
                    Some((*exit_code, signal.clone()))
                }
                _ => None,
            });
            assert_eq!(
                exited,
                Some((None, Some("SIGKILL".to_string()))),
                "expected a SIGKILL-escalated exit, got: {recorded:#?}"
            );
        }

        db.shutdown().await.ok();
    }

    // ------------------------------------------------ turn-boundary tests

    /// A fake `claude` that completes a turn cleanly: `system/init`, one
    /// `assistant` text line (so the turn produced real content, not an
    /// empty one), and a `result` with no permission denials. No argv
    /// recording -- these tests are about the run's own lifecycle state,
    /// not the launched command line (`the_resolved_model_reaches_the_real_launched_argv`
    /// already proves that separately).
    fn write_clean_completion_fake_claude(dir: &std::path::Path) -> PathBuf {
        let script = r#"#!/bin/sh
read -r _first_line
echo '{"type":"system","subtype":"init","session_id":"sess-lifecycle-test","claude_code_version":"2.1.268","permissionMode":"auto"}'
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}'
echo '{"type":"result","subtype":"success"}'
"#;
        let path = dir.join("fake-claude-clean.sh");
        std::fs::write(&path, script).expect("write fake claude script");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// The production sink chain minus settlement, same shape
    /// `tests/run_lifecycle.rs`'s own `production_sink_chain` builds
    /// (duplicated in-crate rather than imported: that file is a
    /// separate compilation unit, and the seam this test needs --
    /// `ClaudeProtocolAdapter::new`'s `pub(crate)` constructor and
    /// `with_test_overrides`'s `#[cfg(test)]` gate -- is only reachable
    /// from inside this crate's own test compilation, not from an
    /// external integration-test binary linking the built rlib).
    fn production_sink_chain(
        db: &StdArc<DatabaseHandle>,
        project_id: crew_protocol::ProjectId,
        events_tx: tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
        run_id: RunId,
    ) -> Arc<dyn AdapterEventSink> {
        let violation = StdArc::new(crate::policy::ViolationService::new(
            StdArc::clone(db),
            project_id,
            events_tx.clone(),
            None,
            crate::config::NestedViolationAction::default(),
            crate::security::redaction::Redactor::new(),
        ));
        let domain_sink = Arc::new(
            crate::adapter::DomainAdapterEventSink::new(
                StdArc::clone(db),
                project_id,
                events_tx.clone(),
                Vec::new(),
                false,
                violation,
                false,
            )
            .expect("built-in patterns always compile"),
        );
        crate::adapter::RunLifecycleSink::wrap(
            domain_sink,
            StdArc::clone(db),
            project_id,
            events_tx,
            run_id,
            StdArc::new(crate::adapter::ActivityClock::new()),
        )
    }

    /// Seeds one task/worker/run row directly, same shape the pane test
    /// above uses -- returns the identifiers for the caller to drive.
    async fn seed_task_worker_run(
        db: &DatabaseHandle,
        project_id: crew_protocol::ProjectId,
    ) -> (TaskId, WorkerId, RunId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new({
            let task_id = task_id.to_string();
            let worker_id = worker_id.to_string();
            let run_id = run_id.to_string();
            let project_id = project_id.to_string();
            move |conn| {
                conn.execute(
                    "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at) \
                     VALUES (?1, ?2, 'test-owner', 1, ?3, ?3)",
                    rusqlite::params![task_id, project_id, "2026-01-01T00:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope) \
                     VALUES (?1, 'sha256:test', 'claude', 'test-model', '{}')",
                    rusqlite::params![worker_id.clone()],
                )?;
                conn.execute(
                    "INSERT INTO workers (worker_id, project_id, profile_id, resolved_profile_json, created_at) \
                     VALUES (?1, ?2, ?1, '{}', ?3)",
                    rusqlite::params![worker_id, project_id, "2026-01-01T00:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO runs (run_id, task_id, worker_id, state, created_at) \
                     VALUES (?1, ?2, ?3, 'queued', ?4)",
                    rusqlite::params![run_id, task_id, worker_id, "2026-01-01T00:00:00Z"],
                )?;
                Ok(serde_json::Value::Null)
            }
        }))
        .await
        .expect("seed task/worker/run");
        (task_id, worker_id, run_id)
    }

    async fn run_state(db: &DatabaseHandle, run_id: RunId) -> String {
        db.run_domain_op(Box::new(move |conn| {
            let state: String = conn.query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |r| r.get(0),
            )?;
            Ok(serde_json::json!(state))
        }))
        .await
        .expect("read run state")
        .as_str()
        .expect("state is a string")
        .to_string()
    }

    /// Every journaled event kind for `run_id`, in sequence order, as
    /// `(sequence, RuntimeEventKind)` pairs -- used to check ordering
    /// between two kinds, not just that each individually appears.
    async fn journaled_kinds(db: &DatabaseHandle, run_id: RunId) -> Vec<(i64, String)> {
        db.run_domain_op(Box::new(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT sequence, event_json FROM events WHERE run_id = ?1 ORDER BY sequence",
            )?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([run_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            Ok(serde_json::json!(rows))
        }))
        .await
        .expect("read journaled events")
        .as_array()
        .expect("rows are an array")
        .iter()
        .map(|pair| {
            let seq = pair[0].as_i64().expect("sequence is an integer");
            let raw = pair[1].as_str().expect("event_json is a string");
            let value: serde_json::Value =
                serde_json::from_str(raw).expect("parse journaled event");
            let kind = value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("<unknown>")
                .to_string();
            (seq, kind)
        })
        .collect()
    }

    /// Polls `check` (an async predicate) every 20ms until it returns
    /// `true` or `bound` elapses -- what every test below now needs
    /// wherever it used to rely on `adapter.start(...).await` itself
    /// only resolving once the whole turn (not just the "up" phase, the
    /// initialize handshake) had completed. Panics past `bound`, naming
    /// it, rather than hanging a suite run.
    async fn wait_for<Fut, F>(bound: Duration, mut check: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + bound;
        loop {
            if check().await {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "condition did not become true within {bound:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// **The reproduction this fix exists for.** Pins the PROPERTY
    /// (`terminal_state_for`'s own doc comment: a zero exit after a
    /// settled turn is `RunState::unrendered_verdict()`, i.e.
    /// `"cancelled"`, never `"failed"`), not the implementation detail
    /// (that `TurnEnded` was emitted) -- a test asserting the latter
    /// would keep passing even if the lifecycle's own matching changed
    /// underneath it, with the real defect back and the test still
    /// green. This is red before this adapter emits its own turn boundary
    /// (the run reads `"failed"` after a turn that produced real content
    /// and exited 0) and green after.
    ///
    /// Synchronizes on the run's own final state (`wait_for`), not on
    /// `adapter.start(...)` returning: the up/run split means `start`
    /// itself now resolves once the initialize handshake completes, not
    /// once the whole turn (and this run's terminal state) does.
    #[tokio::test]
    async fn a_clean_protocol_turn_settles_rather_than_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let bin = write_clean_completion_fake_claude(dir.path());
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let (task_id, worker_id, run_id) = seed_task_worker_run(&db, project_id).await;

        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(64);
        let sink = production_sink_chain(&db, project_id, events_tx, run_id);

        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            tokio::sync::broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());
        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            None,
            ProtocolBundle {
                approval_service,
                callback,
            },
            None,
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string());

        adapter
            .start(
                StartSpec {
                    run_id,
                    task_id,
                    worker_id,
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            )
            .await
            .expect("start (the up phase) must succeed: the handshake completes against the fake binary");

        wait_for(Duration::from_secs(5), || async {
            matches!(
                run_state(&db, run_id).await.as_str(),
                "cancelled" | "failed"
            )
        })
        .await;

        let final_state = run_state(&db, run_id).await;
        assert_ne!(
            final_state, "failed",
            "a clean protocol turn that produced real content must not be recorded failed"
        );
        assert_eq!(
            final_state, "cancelled",
            "expected RunState::unrendered_verdict() (\"cancelled\"): real work was done, \
             but nothing (no run/finish call) rendered a verdict on it, got {final_state:?}"
        );

        db.shutdown().await.ok();
    }

    /// The turn boundary must be journaled BEFORE the process's own exit
    /// -- the same ordering the TUI path already guarantees (a
    /// transcript-tail boundary always precedes the eventual process
    /// exit). Checked from the durable journal's own sequence numbers,
    /// not from call order in this test's own code, since that is what
    /// `RunLifecycleSink` actually acts on.
    ///
    /// Synchronizes on the run's own final state (`wait_for`), not on
    /// `adapter.start(...)` returning -- see the same note on
    /// `a_clean_protocol_turn_settles_rather_than_fails` above.
    #[tokio::test]
    async fn turn_ended_is_journaled_before_process_exited_on_the_protocol_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let bin = write_clean_completion_fake_claude(dir.path());
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let (task_id, worker_id, run_id) = seed_task_worker_run(&db, project_id).await;

        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(64);
        let sink = production_sink_chain(&db, project_id, events_tx, run_id);

        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            tokio::sync::broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());
        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            None,
            ProtocolBundle {
                approval_service,
                callback,
            },
            None,
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string());

        adapter
            .start(
                StartSpec {
                    run_id,
                    task_id,
                    worker_id,
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            )
            .await
            .expect("start (the up phase) must succeed against the fake binary");

        wait_for(Duration::from_secs(5), || async {
            matches!(
                run_state(&db, run_id).await.as_str(),
                "cancelled" | "failed"
            )
        })
        .await;

        let kinds = journaled_kinds(&db, run_id).await;
        let turn_ended_seq = kinds
            .iter()
            .find(|(_, kind)| kind == "adapterTurnEvent")
            .map(|(seq, _)| *seq);
        let process_exited_seq = kinds
            .iter()
            .find(|(_, kind)| kind == "adapterProcessEvent")
            .map(|(seq, _)| *seq);
        // Both kinds share `adapterProcessEvent`'s own type tag for
        // `ProcessStarted`/`ProcessExited` alike (see `event_sink.rs`'s
        // `RuntimeEvent` mapping), so disambiguate by finding the turn
        // boundary's sequence number and asserting it precedes the
        // LAST `adapterProcessEvent`-tagged row (the exit, since start
        // was already journaled earlier in the same sequence).
        let last_process_event_seq = kinds
            .iter()
            .filter(|(_, kind)| kind == "adapterProcessEvent")
            .map(|(seq, _)| *seq)
            .max();
        assert!(
            turn_ended_seq.is_some(),
            "expected a journaled turn-boundary event; got kinds: {kinds:?}"
        );
        assert!(
            process_exited_seq.is_some(),
            "expected at least one journaled adapterProcessEvent; got kinds: {kinds:?}"
        );
        assert!(
            turn_ended_seq.unwrap() < last_process_event_seq.unwrap(),
            "the turn boundary must be journaled before the process's own exit; got kinds: {kinds:?}"
        );

        db.shutdown().await.ok();
    }

    // -------------------------------------------------- up/run split tests

    /// A fake `claude` that completes its own initialize handshake
    /// immediately, then sleeps for `SLOW_TURN_SLEEP_SECS` before the
    /// rest of the turn -- standing in for a real turn's own duration.
    ///
    /// **Margin arithmetic** (same discipline as
    /// `tui::adapter::tests`' own load-tested margin comment): two
    /// numbers, `SLOW_TURN_SLEEP_SECS` (this fake turn's own duration)
    /// and `bound` (the test's own timeout, defined at its call site
    /// below), and three constraints on their gap:
    ///
    ///   1. `bound` must stay below `SLOW_TURN_SLEEP_SECS`, or the
    ///      turn's own sleep would already have elapsed by the time the
    ///      bound does, and this would stop discriminating fixed from
    ///      broken code at all.
    ///   2. `bound` IS the spawn budget: real time for `/bin/sh` to
    ///      start, read one line, and write one line back, on whatever
    ///      runner this executes on -- there is no real handshake work
    ///      here, so nearly all of `bound` is slack for process
    ///      spawn/scheduling latency. Too small and this flakes on
    ///      infrastructure contention having nothing to do with the fix.
    ///   3. `SLOW_TURN_SLEEP_SECS - bound` is the discrimination margin:
    ///      fixed code returns in the low tens of milliseconds (spawn +
    ///      one read + one echo), broken code cannot return before the
    ///      sleep's own wall-clock floor -- a real `sleep N` can only
    ///      take AT LEAST `N` seconds under load, never less, so there
    ///      is no contention scenario where broken code finishes before
    ///      `bound` elapses. The only real risk this margin protects
    ///      against is fixed code's own spawn overrunning `bound` on an
    ///      exceptionally loaded runner, not a false pass.
    ///
    /// `SLOW_TURN_SLEEP_SECS = 6`, `bound = 3s`: constraint 1 holds
    /// (3 < 6), constraint 2 gives a 3s spawn budget (tens of thousands
    /// of times the actual work), constraint 3 gives a 3s discrimination
    /// margin between the fixed code's near-instant return and the
    /// broken code's own 6s+ floor.
    const SLOW_TURN_SLEEP_SECS: u64 = 6;

    fn write_slow_turn_fake_claude(dir: &std::path::Path) -> PathBuf {
        let script = format!(
            r#"#!/bin/sh
read -r _first_line
echo '{{"type":"control_response","response":{{"subtype":"success","request_id":"crew-initialize","response":{{}}}}}}'
sleep {SLOW_TURN_SLEEP_SECS}
echo '{{"type":"system","subtype":"init","session_id":"sess-slow-turn","claude_code_version":"2.1.268","permissionMode":"auto"}}'
echo '{{"type":"result","subtype":"success"}}'
"#
        );
        let path = dir.join("slow-turn-claude.sh");
        std::fs::write(&path, script).expect("write fake claude script");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// **The reproduction this fix exists for.** Before the up/run split,
    /// `Adapter::start` awaited [`reader::drive_turn`]'s ENTIRE return
    /// inline, which itself does not return until the turn ends -- so
    /// this is red against that code: `start` would not return until
    /// the fake claude's own `SLOW_TURN_SLEEP_SECS` sleep elapsed (a
    /// stand-in for a real turn's duration, and for the daemon's own
    /// per-connection dispatch loop blocking on it -- see this module's
    /// own doc comment on `Self::start`). Green after the split: `start`
    /// returns once the initialize handshake completes, long before the
    /// turn (and this fake process) is done.
    #[tokio::test]
    async fn start_returns_once_the_handshake_completes_not_once_the_turn_ends() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let bin = write_slow_turn_fake_claude(dir.path());
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());
        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            None,
            ProtocolBundle {
                approval_service,
                callback,
            },
            None,
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string());

        let events = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::clone(&events),
        });

        // See `SLOW_TURN_SLEEP_SECS`'s own doc comment above for the
        // margin arithmetic behind this specific value.
        let bound = Duration::from_secs(3);
        let started_at = tokio::time::Instant::now();
        tokio::time::timeout(
            bound,
            adapter.start(
                StartSpec {
                    run_id: RunId::new(),
                    task_id: TaskId::new(),
                    worker_id: WorkerId::new(),
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            ),
        )
        .await
        .expect("start must return well before the fake turn's own sleep elapses")
        .expect("start (the up phase) must succeed: the handshake completed");
        let elapsed = started_at.elapsed();

        assert!(
            elapsed < bound,
            "expected start to return once the handshake completed, long before the turn's own \
             {SLOW_TURN_SLEEP_SECS}s sleep -- took {elapsed:?}"
        );

        db.shutdown().await.ok();
    }

    /// Condition from review: a run-phase PANIC (not just an ordinary
    /// `Err`) after the handshake must still leave this run terminal --
    /// nothing else will settle it, since `start` has already returned
    /// `Ok` by the time a post-handshake panic can occur. Proven via the
    /// `with_run_phase_panic` test seam rather than provoking a real
    /// panic from production code paths.
    #[tokio::test]
    async fn a_run_phase_panic_after_the_handshake_still_settles_the_run_terminal() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let bin = write_clean_completion_fake_claude(dir.path());
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let (task_id, worker_id, run_id) = seed_task_worker_run(&db, project_id).await;

        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(64);
        let sink = production_sink_chain(&db, project_id, events_tx, run_id);

        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            tokio::sync::broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());
        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            None,
            ProtocolBundle {
                approval_service,
                callback,
            },
            None,
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string())
        .with_run_phase_panic();

        adapter
            .start(
                StartSpec {
                    run_id,
                    task_id,
                    worker_id,
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            )
            .await
            .expect("start (the up phase) must still succeed: the handshake completed before the run phase ever panics");

        wait_for(Duration::from_secs(5), || async {
            matches!(
                run_state(&db, run_id).await.as_str(),
                "cancelled" | "failed"
            )
        })
        .await;

        let final_state = run_state(&db, run_id).await;
        assert_eq!(
            final_state, "failed",
            "a run-phase panic (no TurnEnded ever emitted) must settle as failed, \
             `terminal_state_for`'s own \"no turn ever settled\" arm -- got {final_state:?}"
        );

        db.shutdown().await.ok();
    }
}
