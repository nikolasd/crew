//! The vendor-agnostic TUI adapter shell: spawns a vendor CLI on a real
//! PTY, attaches an [`AttachServer`] and a [`PaneCoordinator`]-resolved
//! pane so a human can watch (and type into) the same session, injects
//! the initial prompt with a nonce tag, discovers the vendor's own
//! transcript file by that nonce, and tails it into normalized
//! [`AdapterEvent`]s -- the counterpart to the headless (Claude/Codex/
//! Copilot/OMP-RPC) adapters for `mode: "tui"` worker profiles.
//!
//! Per-vendor behavior ([`TuiVendor`]) is deliberately small: argv/env/cwd
//! construction, the transcript format, how to compose injected text, the
//! turn-interrupt byte sequence, and a version compatibility gate. Every
//! other concern -- PTY supervision, attach, pane lifecycle, readiness
//! gating, nonce discovery, transcript tailing, and event normalization --
//! lives here exactly once, shared by every vendor that plugs in a
//! [`TuiVendor`] impl.
//!
//! No production [`TuiVendor`] implementation exists yet (those land in
//! later work packages, one per vendor); [`crate::adapter::registry`]
//! therefore never constructs a [`TuiAdapter`] today -- a profile asking
//! for `mode: "tui"` gets a typed refusal instead of a silent headless
//! fallback. This module is exercised end-to-end here against a mock
//! vendor, so the machinery is proven and ready for the first real vendor
//! to plug into.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crew_protocol::{
    Classified, ContentClass, DisplayBackend, DisplayPlacement, RunId, TaskId, WorkerId,
};

use crate::adapter::AdapterFuture;
use crate::adapter::capability::{
    AdapterCapabilities, ApprovalsCapability, DurabilityCapability, NativeViewCapability,
    NestedCapability, ProtocolKind, ResumeCapability, SteeringCapability, UsageCapability,
    WorkspaceControlCapability,
};
use crate::adapter::error::AdapterError;
use crate::adapter::event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink};
use crate::adapter::r#trait::{
    Adapter, AdapterMessage, AdapterSnapshot, CancelScope, ProbeResult, StartSpec, VendorSessionRef,
};
use crate::config::crew::{AdapterConfig, CloseOnExit};
use crate::display::{
    AttachServer, AttachTarget, PaneAttachOutcome, PaneAttachRequest, PaneCoordinator,
};
use crate::supervisor::{EscalationTimings, PtyProcess};

use super::discovery::{DiscoveryError, find_transcript_by_nonce};
use super::tailer::{TailerHandle, TranscriptTailer};
use super::{Cursor, TranscriptFormat, TuiEvent};

/// Interactive-TUI launch instructions a [`TuiVendor`] builds: argv, cwd,
/// and the exact (already-allowlisted) environment. Deliberately not
/// [`crate::supervisor::SpawnSpec`] itself -- a vendor implementation
/// should not need to know about headless-adapter concerns like
/// stdout/stderr capture bounds that mean nothing on a PTY.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}

impl LaunchSpec {
    fn into_spawn_spec(self) -> crate::supervisor::SpawnSpec {
        crate::supervisor::SpawnSpec {
            program: self.program,
            args: self.args,
            cwd: self.cwd,
            env: self.env,
            ..crate::supervisor::SpawnSpec::minimal()
        }
    }
}

/// The outcome of [`TuiVendor::version_gate`]: whether a probed vendor CLI
/// version is one this adapter's fixed argv/transcript-format assumptions
/// were built against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionVerdict {
    Compatible,
    Incompatible { detail: String },
}

/// Per-vendor behavior a [`TuiAdapter`] drives generically. Every method
/// is a pure, no-process-spawned computation except through the
/// [`LaunchSpec`]s it returns -- `TuiAdapter` itself owns the actual
/// spawn, attach, discovery, and tailing.
pub trait TuiVendor: Send + Sync + 'static {
    /// The adapter kind, e.g. `"claude"`. Used verbatim in every
    /// [`AdapterError::adapter`] this adapter instance raises.
    fn kind(&self) -> &'static str;

    /// Interactive-TUI argv/env/cwd for a fresh session.
    fn launch(&self, spec: &StartSpec, cfg: &AdapterConfig) -> LaunchSpec;

    /// Interactive-TUI argv/env/cwd to resume a previously established
    /// vendor session.
    fn resume_launch(
        &self,
        session: &VendorSessionRef,
        spec: &StartSpec,
        cfg: &AdapterConfig,
    ) -> LaunchSpec;

    /// The directory [`find_transcript_by_nonce`] scans for this vendor's
    /// session transcript (a `session_dir` override from `cfg`, or the
    /// vendor's own default).
    fn transcript_root(&self, spec: &StartSpec, cfg: &AdapterConfig) -> PathBuf;

    /// This vendor's transcript line format.
    fn format(&self) -> Arc<dyn TranscriptFormat>;

    /// Composes a message into the exact bytes to write to the PTY (text
    /// plus this vendor's own submit convention).
    fn compose_input(&self, message: &str) -> Vec<u8>;

    /// The byte sequence this vendor's CLI interprets as "stop the current
    /// turn" (a [`CancelScope::Turn`] cancellation).
    fn interrupt_sequence(&self) -> Vec<u8>;

    /// The vendor-specific argv fragment for an abstract permission
    /// posture. Not called by [`TuiAdapter`] itself -- a vendor's own
    /// [`Self::launch`] calls this to build its argv; it is part of the
    /// trait so a vendor's argv-construction logic is independently
    /// testable.
    fn permission_args(&self, mode: crate::config::crew::PermissionMode) -> Vec<String>;

    /// Whether a probed `--version`-style string is one this adapter's
    /// fixed assumptions about argv and transcript format were built
    /// against.
    fn version_gate(&self, probed: &str) -> VersionVerdict;
}

/// Timing knobs for [`TuiAdapter`]'s readiness gate, nonce-discovery
/// timeout, transcript poll interval, and termination escalation.
/// Production uses [`Self::default`]; tests inject much shorter values so
/// the suite does not spend real wall-clock minutes.
#[derive(Debug, Clone)]
pub struct TuiTimings {
    /// How long of an output quiet period after the first PTY output
    /// means "ready for input".
    pub readiness_quiet: Duration,
    /// The absolute cap on the readiness gate's total wait, regardless of
    /// whether a quiet period was ever observed.
    pub readiness_cap: Duration,
    /// How long [`find_transcript_by_nonce`] polls before giving up.
    pub discovery_timeout: Duration,
    /// The transcript tailer's poll interval.
    pub tailer_poll: Duration,
    /// SIGINT/SIGTERM/SIGKILL escalation timings for [`PtyProcess`].
    pub escalation: EscalationTimings,
}

impl Default for TuiTimings {
    fn default() -> Self {
        Self {
            readiness_quiet: Duration::from_millis(700),
            readiness_cap: Duration::from_secs(8),
            discovery_timeout: Duration::from_secs(8),
            tailer_poll: Duration::from_millis(200),
            escalation: EscalationTimings::default(),
        }
    }
}

/// Shared, mutable pane identity [`AttachServer`]'s `on_user_input`
/// callback reads: starts as [`DisplayBackend::Hidden`]/empty (the
/// callback may fire before [`PaneCoordinator::attach`] resolves, in the
/// narrow window between the socket binding and the pane request
/// completing) and is updated once the real pane is known.
type SharedPaneIdentity = Arc<StdMutex<(DisplayBackend, String)>>;

/// Mutable per-run state, held only while a run is active. `attach` and
/// `tailer` are deliberately not carried here: the exit watcher spawned
/// in `run_pipeline` owns its own clones of both and is the sole place
/// that stops them, so nothing else needs a reference (see
/// `spawn_exit_watcher`'s doc comment).
struct RunState {
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    pty: Arc<PtyProcess>,
    watcher: JoinHandle<()>,
    sink: Arc<dyn AdapterEventSink>,
    cursor: Arc<StdMutex<Cursor>>,
    pane_ref: String,
}

/// The vendor-agnostic TUI adapter: implements [`Adapter`] against any
/// [`TuiVendor`].
pub struct TuiAdapter<V: TuiVendor> {
    vendor: Arc<V>,
    cfg: AdapterConfig,
    pane_coordinator: Arc<PaneCoordinator>,
    panes_dir: PathBuf,
    placement: DisplayPlacement,
    forced_backend: Option<DisplayBackend>,
    close_on_exit: CloseOnExit,
    timings: TuiTimings,
    run: AsyncMutex<Option<RunState>>,
}

impl<V: TuiVendor> TuiAdapter<V> {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vendor: V,
        cfg: AdapterConfig,
        pane_coordinator: Arc<PaneCoordinator>,
        panes_dir: PathBuf,
        placement: DisplayPlacement,
        forced_backend: Option<DisplayBackend>,
        close_on_exit: CloseOnExit,
        timings: TuiTimings,
    ) -> Self {
        Self {
            vendor: Arc::new(vendor),
            cfg,
            pane_coordinator,
            panes_dir,
            placement,
            forced_backend,
            close_on_exit,
            timings,
            run: AsyncMutex::new(None),
        }
    }

    fn kind(&self) -> &'static str {
        self.vendor.kind()
    }

    /// `<panes_dir>/<run_id>.sock` -- mirrors
    /// [`crate::paths::RuntimePaths::pane_socket`]'s own naming exactly,
    /// duplicated here (rather than depending on a resolved
    /// `RuntimePaths`) because a `TuiAdapter` is constructed with just the
    /// panes directory, not a full state root.
    fn socket_path(&self, run_id: RunId) -> PathBuf {
        self.panes_dir.join(format!("{run_id}.sock"))
    }

    /// The shared start/resume pipeline: spawn the PTY, attach a viewer
    /// socket, resolve a pane, gate on readiness, optionally inject a
    /// prompt, discover the vendor transcript by `discovery_key`, and
    /// start tailing it. `inject` is `Some(text)` for a fresh start
    /// (`compose_input`-ed and written before discovery) and `None` for a
    /// resume (nothing new to say; the transcript is found by the
    /// resumed session id instead of a fresh nonce).
    ///
    /// `run_slot` is the caller's already-held, already-checked-empty
    /// `self.run` guard, held for this whole call rather than re-acquired
    /// at the end: `start`/`resume` must never let a second concurrent
    /// call observe `None` and race this one into starting a second
    /// process for the same adapter instance, so the lock that guards
    /// "is a run already active" has to stay held for the entire
    /// spawn-through-tail pipeline, not just the initial check.
    #[allow(clippy::too_many_arguments)]
    async fn run_pipeline(
        &self,
        run_slot: &mut Option<RunState>,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        launch: LaunchSpec,
        transcript_root: PathBuf,
        discovery_key: String,
        inject: Option<String>,
        sink: Arc<dyn AdapterEventSink>,
    ) -> Result<(), AdapterError> {
        let started_at = SystemTime::now();
        let pty = Arc::new(
            PtyProcess::spawn(&launch.into_spawn_spec(), self.timings.escalation)
                .map_err(|err| AdapterError::process(self.kind(), "start", err.to_string()))?,
        );

        emit(
            &sink,
            run_id,
            task_id,
            worker_id,
            AdapterEventPayload::ProcessStarted {
                pid: pty.pid() as u32,
            },
        )
        .await;

        let pane_identity: SharedPaneIdentity =
            Arc::new(StdMutex::new((DisplayBackend::Hidden, String::new())));
        let attach_sink = Arc::clone(&sink);
        let attach_identity = Arc::clone(&pane_identity);
        let attach = match AttachServer::start(
            self.socket_path(run_id),
            Arc::clone(&pty) as Arc<dyn AttachTarget>,
            Box::new(move |_bytes: Vec<u8>| {
                let sink = Arc::clone(&attach_sink);
                let (backend, pane_ref) = {
                    let guard = attach_identity
                        .lock()
                        .expect("pane identity mutex never poisoned");
                    (guard.0, guard.1.clone())
                };
                tokio::spawn(async move {
                    emit(
                        &sink,
                        run_id,
                        task_id,
                        worker_id,
                        AdapterEventPayload::OutOfBandInput { backend, pane_ref },
                    )
                    .await;
                });
            }),
        ) {
            Ok(server) => Arc::new(server),
            Err(err) => {
                let _ = pty.terminate().await;
                return Err(AdapterError::process(self.kind(), "start", err.to_string()));
            }
        };

        let pane_outcome = self
            .pane_coordinator
            .attach(PaneAttachRequest {
                run_id,
                worker_id,
                adapter: self.kind().to_string(),
                placement: self.placement,
                forced_backend: self.forced_backend,
            })
            .await;
        *pane_identity
            .lock()
            .expect("pane identity mutex never poisoned") =
            (pane_outcome.backend, pane_outcome.pane_ref.clone());

        if let Err(err) = wait_for_readiness(
            &pty,
            self.timings.readiness_quiet,
            self.timings.readiness_cap,
        )
        .await
        {
            return self
                .fail_start(
                    pty,
                    attach,
                    pane_outcome,
                    sink,
                    run_id,
                    task_id,
                    worker_id,
                    err,
                )
                .await;
        }

        if let Some(text) = inject {
            let bytes = self.vendor.compose_input(&text);
            if let Err(err) = pty.write_input(&bytes).await {
                return self
                    .fail_start(
                        pty,
                        attach,
                        pane_outcome,
                        sink,
                        run_id,
                        task_id,
                        worker_id,
                        AdapterError::process(self.kind(), "start", err.to_string()),
                    )
                    .await;
            }
        }

        let discovery_started_at = started_at
            .checked_sub(Duration::from_secs(2))
            .unwrap_or(started_at);
        let transcript_path = match find_transcript_by_nonce(
            &transcript_root,
            discovery_started_at,
            &discovery_key,
            self.timings.discovery_timeout,
        )
        .await
        {
            Ok(path) => path,
            Err(err) => {
                let detail = match err {
                    DiscoveryError::InvalidNonce => "empty discovery key".to_string(),
                    DiscoveryError::Timeout { .. } => err.to_string(),
                };
                return self
                    .fail_start(
                        pty,
                        attach,
                        pane_outcome,
                        sink,
                        run_id,
                        task_id,
                        worker_id,
                        AdapterError::process(self.kind(), "start", detail),
                    )
                    .await;
            }
        };

        emit(
            &sink,
            run_id,
            task_id,
            worker_id,
            AdapterEventPayload::VendorSessionEstablished {
                vendor_session_id: transcript_path.display().to_string(),
            },
        )
        .await;

        let cursor = Arc::new(StdMutex::new(Cursor::start()));
        let (batch_tx, mut batch_rx) = mpsc::unbounded_channel::<(Vec<TuiEvent>, Cursor)>();
        let tailer = TranscriptTailer::new(
            transcript_path,
            self.vendor.format(),
            Cursor::start(),
            self.timings.tailer_poll,
        );
        let tailer_handle = Arc::new(tailer.spawn(move |events, cursor| {
            let _ = batch_tx.send((events, cursor));
        }));

        let pump_sink = Arc::clone(&sink);
        let pump_cursor = Arc::clone(&cursor);
        tokio::spawn(async move {
            while let Some((events, new_cursor)) = batch_rx.recv().await {
                for event in events {
                    emit_tui_event(&pump_sink, run_id, task_id, worker_id, event).await;
                }
                *pump_cursor.lock().expect("cursor mutex never poisoned") = new_cursor;
            }
        });

        let watcher = spawn_exit_watcher(
            Arc::clone(&pty),
            Arc::clone(&attach),
            Arc::clone(&tailer_handle),
            Arc::clone(&self.pane_coordinator),
            pane_outcome.clone(),
            self.close_on_exit,
            Arc::clone(&sink),
            run_id,
            task_id,
            worker_id,
        );

        *run_slot = Some(RunState {
            run_id,
            task_id,
            worker_id,
            pty,
            watcher,
            sink,
            cursor,
            pane_ref: pane_outcome.pane_ref,
        });
        Ok(())
    }

    /// Tears down everything `run_pipeline` had opened so far (PTY, attach
    /// socket, pane) on a failure before the run reached a durable
    /// tailing state, journals the typed failure as a `ProcessExited`
    /// evidence so `RunLifecycleSink` settles the run as failed/lost
    /// rather than leaving it stuck, and returns `err` to the caller.
    #[allow(clippy::too_many_arguments)]
    async fn fail_start(
        &self,
        pty: Arc<PtyProcess>,
        attach: Arc<AttachServer>,
        pane_outcome: PaneAttachOutcome,
        sink: Arc<dyn AdapterEventSink>,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        err: AdapterError,
    ) -> Result<(), AdapterError> {
        let outcome = pty.terminate().await;
        attach.stop();
        self.pane_coordinator
            .detach(&pane_outcome, false, self.close_on_exit)
            .await;
        let (exit_code, signal) = outcome.exit_signals();
        emit(
            &sink,
            run_id,
            task_id,
            worker_id,
            AdapterEventPayload::ProcessExited { exit_code, signal },
        )
        .await;
        Err(err)
    }
}

/// Emits one event through `sink`, logging (never panicking) if the
/// journal write itself failed -- mirrored from every other adapter's
/// best-effort telemetry emission (a lost telemetry event must never be
/// fatal to the run).
async fn emit(
    sink: &Arc<dyn AdapterEventSink>,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    payload: AdapterEventPayload,
) {
    if let Err(err) = sink
        .emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload,
        })
        .await
    {
        tracing::warn!(error = %err, run_id = %run_id, "tui adapter failed to journal an event");
    }
}

/// Maps one parsed [`TuiEvent`] to the [`AdapterEventPayload`](s) it
/// produces and emits them, in order:
/// `AssistantText{is_question:false}` -> `MessageFinal`,
/// `AssistantText{is_question:true}` -> `QuestionDetected`,
/// `ToolActivity` -> `ToolStarted` then a condensed `ToolResult` (the
/// transcript only ever reports completed activity, never live
/// start/finish pairs, so both are synthesized from one entry with a
/// freshly generated correlation id), `SessionMeta` -> a (possibly
/// repeated) `VendorSessionEstablished`, `TurnEnded` -> nothing (no
/// adapter-event-sink payload exists for a bare turn boundary and nothing
/// downstream consumes one yet), `Raw` -> a debug trace only (transcript
/// format drift is expected, not itself an error worth journaling
/// durably; see this module's own doc comment).
async fn emit_tui_event(
    sink: &Arc<dyn AdapterEventSink>,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    event: TuiEvent,
) {
    match event {
        TuiEvent::AssistantText {
            text,
            is_question,
            ts: _,
        } => {
            let payload = if is_question {
                AdapterEventPayload::QuestionDetected { text }
            } else {
                AdapterEventPayload::MessageFinal {
                    role: "assistant".to_string(),
                    text,
                }
            };
            emit(sink, run_id, task_id, worker_id, payload).await;
        }
        TuiEvent::ToolActivity {
            tool,
            detail,
            ts: _,
        } => {
            let tool_call_id = Uuid::now_v7().to_string();
            emit(
                sink,
                run_id,
                task_id,
                worker_id,
                AdapterEventPayload::ToolStarted {
                    tool_call_id: tool_call_id.clone(),
                    name: tool.clone(),
                },
            )
            .await;
            emit(
                sink,
                run_id,
                task_id,
                worker_id,
                AdapterEventPayload::ToolResult {
                    tool_call_id,
                    name: tool,
                    ok: true,
                    detail,
                },
            )
            .await;
        }
        TuiEvent::SessionMeta { vendor_session_id } => {
            emit(
                sink,
                run_id,
                task_id,
                worker_id,
                AdapterEventPayload::VendorSessionEstablished { vendor_session_id },
            )
            .await;
        }
        TuiEvent::TurnEnded => {}
        TuiEvent::Raw { entry_type } => {
            tracing::debug!(entry_type, run_id = %run_id, "tui transcript: unrecognized entry");
        }
    }
}

/// Waits for the PTY's first output, then for `quiet` with no further
/// output, bounded overall by `cap`. If `quiet` is never reached before
/// `cap` elapses, returns `Ok(())` anyway (proceeding on a chatty CLI
/// rather than failing the run outright); a closed output channel before
/// any output arrived (the process died immediately) is reported as an
/// error.
async fn wait_for_readiness(
    pty: &PtyProcess,
    quiet: Duration,
    cap: Duration,
) -> Result<(), AdapterError> {
    let mut rx = pty.subscribe_output();
    let deadline = tokio::time::Instant::now() + cap;

    // Wait for the first output (a single check, not a loop: every
    // outcome below either proceeds past this point or returns).
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(AdapterError::process(
            "tui",
            "start",
            "no output observed on the pty before the readiness cap elapsed",
        ));
    }
    match tokio::time::timeout(remaining, rx.recv()).await {
        Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
        Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
            return Err(AdapterError::process(
                "tui",
                "start",
                "the worker process exited before producing any output",
            ));
        }
        Err(_) => {
            return Err(AdapterError::process(
                "tui",
                "start",
                "no output observed on the pty before the readiness cap elapsed",
            ));
        }
    }

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let wait = quiet.min(remaining);
        match tokio::time::timeout(wait, rx.recv()).await {
            Ok(_) => continue,
            Err(_) => return Ok(()),
        }
    }
}

/// Spawns the single task that owns this run's settlement: waits for the
/// PTY to exit (naturally, or because [`TuiAdapter::cancel`]/`dispose`
/// called [`PtyProcess::terminate`]), stops the tailer and attach server,
/// honors `close_on_exit` through the pane coordinator, and journals
/// `ProcessExited` -- exactly once, from exactly one place, regardless of
/// why or how the process died.
#[allow(clippy::too_many_arguments)]
fn spawn_exit_watcher(
    pty: Arc<PtyProcess>,
    attach: Arc<AttachServer>,
    tailer: Arc<TailerHandle>,
    pane_coordinator: Arc<PaneCoordinator>,
    pane_outcome: PaneAttachOutcome,
    close_on_exit: CloseOnExit,
    sink: Arc<dyn AdapterEventSink>,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let status = pty.exit_watcher().await;
        tailer.stop();
        attach.stop();
        let succeeded = status.success();
        pane_coordinator
            .detach(&pane_outcome, succeeded, close_on_exit)
            .await;
        emit(
            &sink,
            run_id,
            task_id,
            worker_id,
            AdapterEventPayload::ProcessExited {
                exit_code: Some(status.exit_code() as i32),
                signal: None,
            },
        )
        .await;
    })
}

impl<V: TuiVendor> Adapter for TuiAdapter<V> {
    fn kind(&self) -> &str {
        self.vendor.kind()
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            // Control is unstructured byte injection into a live
            // terminal, not a request/response vendor protocol -- the
            // same "degraded control" `ProtocolKind::Terminal` already
            // names for the terminal-automation fallback adapter, even
            // though observation here is a structured transcript tail
            // (as good as any headless adapter's).
            protocol: ProtocolKind::Terminal,
            resume: ResumeCapability::Session,
            steering: SteeringCapability::ActiveTurn,
            approvals: ApprovalsCapability::None,
            structured_result: false,
            usage: UsageCapability::None,
            nested: NestedCapability::None,
            native_view: NativeViewCapability::IndependentTui,
            workspace_control: WorkspaceControlCapability::Write,
            durability: DurabilityCapability::VendorResumable,
        }
    }

    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        Box::pin(async move {
            let placeholder = StartSpec {
                run_id: RunId::new(),
                task_id: TaskId::new(),
                worker_id: WorkerId::new(),
                prompt: String::new(),
                resume: None,
            };
            let launch = self.vendor.launch(&placeholder, &self.cfg);
            let output = std::process::Command::new(&launch.program)
                .arg("--version")
                .output()
                .map_err(|e| AdapterError::unavailable(self.kind(), "probe", e.to_string()))?;
            if !output.status.success() {
                return Err(AdapterError::unavailable(
                    self.kind(),
                    "probe",
                    "vendor CLI --version exited non-zero",
                ));
            }
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let VersionVerdict::Incompatible { detail } = self.vendor.version_gate(&version) {
                return Err(AdapterError::incompatible_version(
                    self.kind(),
                    "probe",
                    detail,
                ));
            }
            Ok(ProbeResult {
                version: Some(version),
                auth_ready: true,
                capabilities: self.capabilities(),
                inventory_incomplete: true,
            })
        })
    }

    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            // Held for the entire pipeline below, not just this check:
            // see `run_pipeline`'s own doc comment for why a second
            // concurrent `start`/`resume` must never be able to observe
            // `None` and race this call into starting a second process.
            let mut guard = self.run.lock().await;
            if guard.is_some() {
                return Err(AdapterError::invalid_vendor_state(
                    self.kind(),
                    "start",
                    "adapter already has an active run",
                ));
            }
            let launch = self.vendor.launch(&spec, &self.cfg);
            let transcript_root = self.vendor.transcript_root(&spec, &self.cfg);
            let nonce = Uuid::now_v7().to_string();
            let injected = format!("{}\n\n[crew:{nonce}]", spec.prompt);
            self.run_pipeline(
                &mut guard,
                spec.run_id,
                spec.task_id,
                spec.worker_id,
                launch,
                transcript_root,
                nonce,
                Some(injected),
                sink,
            )
            .await
        })
    }

    fn resume(
        &self,
        session: VendorSessionRef,
        sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            // See `start`'s own comment: held for the whole pipeline.
            let mut guard = self.run.lock().await;
            if guard.is_some() {
                return Err(AdapterError::invalid_vendor_state(
                    self.kind(),
                    "resume",
                    "adapter already has an active run",
                ));
            }
            let placeholder = StartSpec {
                run_id: RunId::new(),
                task_id: TaskId::new(),
                worker_id: WorkerId::new(),
                prompt: String::new(),
                resume: Some(session.clone()),
            };
            let launch = self.vendor.resume_launch(&session, &placeholder, &self.cfg);
            let transcript_root = self.vendor.transcript_root(&placeholder, &self.cfg);
            self.run_pipeline(
                &mut guard,
                placeholder.run_id,
                placeholder.task_id,
                placeholder.worker_id,
                launch,
                transcript_root,
                session.0,
                None,
                sink,
            )
            .await
        })
    }

    fn send(&self, message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let guard = self.run.lock().await;
            let Some(run) = guard.as_ref() else {
                return Err(AdapterError::invalid_vendor_state(
                    self.kind(),
                    "send",
                    "no active run",
                ));
            };
            let text = match message {
                AdapterMessage::Steer { text }
                | AdapterMessage::FollowUp { text }
                | AdapterMessage::Answer { text }
                | AdapterMessage::PeerMessage { text } => text,
            };
            run.sink
                .emit(AdapterEvent {
                    run_id: run.run_id,
                    task_id: run.task_id,
                    worker_id: run.worker_id,
                    payload: AdapterEventPayload::MessageChunk {
                        role: "user".to_string(),
                        text: Classified {
                            class: ContentClass::Visible,
                            value: text.clone(),
                        },
                    },
                })
                .await
                .map_err(|e| AdapterError::process(self.kind(), "send", e.to_string()))?;
            let bytes = self.vendor.compose_input(&text);
            run.pty
                .write_input(&bytes)
                .await
                .map_err(|e| AdapterError::process(self.kind(), "send", e.to_string()))
        })
    }

    fn respond_to_approval(&self, _approval_id: &str, _decision: &str) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            Err(AdapterError::capability_unsupported(
                self.kind(),
                "respondToApproval",
            ))
        })
    }

    fn cancel(&self, scope: CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            match scope {
                CancelScope::Turn => {
                    let guard = self.run.lock().await;
                    let Some(run) = guard.as_ref() else {
                        // No active run to interrupt is a clean no-op,
                        // not a kill failure (matches the terminal
                        // adapter's `cancel`-with-nothing-to-settle
                        // judgement): an `Err` here would read as a
                        // live vendor process a signal failed against.
                        return Ok(());
                    };
                    run.pty
                        .write_input(&self.vendor.interrupt_sequence())
                        .await
                        .map_err(|e| AdapterError::process(self.kind(), "cancel", e.to_string()))
                }
                CancelScope::Worker | CancelScope::Subtree => {
                    let run = {
                        let mut guard = self.run.lock().await;
                        guard.take()
                    };
                    let Some(run) = run else {
                        return Ok(());
                    };
                    // The exit watcher spawned in `run_pipeline` observes
                    // this termination and performs the one-and-only
                    // teardown + `ProcessExited` emission for this run;
                    // `cancel` itself never emits or tears down a second
                    // time (mirrors `CodexAdapter::cancel`'s own
                    // "the pump must not be aborted here" discipline).
                    let _ = run.pty.terminate().await;
                    Ok(())
                }
            }
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async move {
            let guard = self.run.lock().await;
            let Some(run) = guard.as_ref() else {
                return Ok(AdapterSnapshot::default());
            };
            let cursor = run
                .cursor
                .lock()
                .expect("cursor mutex never poisoned")
                .clone();
            Ok(AdapterSnapshot {
                state_summary: format!("tui[{}] pane={}", self.kind(), run.pane_ref),
                children: Vec::new(),
                // WP12 handoff: cursor persistence into the `runs` table
                // is a follow-up work package. Until then, the tailer's
                // durable position is carried here on the adapter side
                // and surfaced through `snapshot()` rather than lost --
                // WP12 should read it from here (or from a dedicated
                // sink-meta parameter, if one lands first) and persist it
                // transactionally alongside each batch's journaled
                // events.
                usage: Some(serde_json::json!({
                    "cursor": {
                        "offset": cursor.offset,
                        "lastEntryId": cursor.last_entry_id,
                    }
                })),
                artifacts: Vec::new(),
            })
        })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let run = {
                let mut guard = self.run.lock().await;
                guard.take()
            };
            let Some(run) = run else {
                return Ok(());
            };
            let _ = run.pty.terminate().await;
            let _ = run.watcher.await;
            Ok(())
        })
    }
}
