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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, oneshot};
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
    /// The deterministic transcript path for a resumed session:
    /// `<transcript_root>/<session-id>.jsonl` by default -- the layout at
    /// least one real vendor (Claude) uses, whose transcript filename stem
    /// *is* the session id (a UUID). A vendor whose resumed-session naming
    /// differs overrides this.
    ///
    /// This is what makes resume reliable (WP14): unlike a fresh start,
    /// a resume has no freshly injected nonce to discover the transcript
    /// by, and the vendor may never re-touch an existing transcript
    /// within any discovery window -- but the runtime already knows the
    /// session id (`runs.vendor_session_id`) and the vendor's own root
    /// layout, so the path follows without touching the filesystem.
    fn transcript_path_for_session(
        &self,
        session: &VendorSessionRef,
        spec: &StartSpec,
        cfg: &AdapterConfig,
    ) -> PathBuf {
        self.transcript_root(spec, cfg)
            .join(format!("{}.jsonl", session.0))
    }

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

    /// A best-effort vendor session id derived from a transcript's own
    /// path, used as the *initial* `VendorSessionEstablished` value the
    /// instant a transcript is found -- before any `SessionMeta` entry
    /// (which later corrects/confirms it) has actually been tailed.
    /// Never a full path: the default derives the file stem (which is
    /// the session id itself for at least one real vendor's on-disk
    /// layout); a vendor whose layout differs overrides this.
    fn session_id_from_transcript_path(&self, path: &Path) -> Option<String> {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_string)
    }
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
    /// Signals the exit watcher to call [`PtyProcess::terminate`] itself
    /// and report the real `TerminationOutcome` (see `spawn_exit_watcher`'s
    /// doc comment) -- `cancel`/`dispose` never call `terminate` directly,
    /// so there is exactly one caller and no race over which signal/exit
    /// code gets journaled.
    terminate_tx: oneshot::Sender<()>,
    sink: Arc<dyn AdapterEventSink>,
    pane_ref: String,
}

/// Everything a caller that already knows a prior session's durable
/// state can hand a [`TuiAdapter`] so its `resume()` needs no discovery
/// and re-tails from the exact stored position. The registry supplies
/// this (from `runs.vendor_session_id`/`runs.transcript_cursor`) when it
/// constructs an adapter it is about to resume; `Default` (both fields
/// empty) keeps the pre-WP14 shape: the transcript path is derived
/// deterministically from the vendor's own layout
/// ([`TuiVendor::transcript_path_for_session`]) and tailing starts from
/// the beginning of the file.
#[derive(Debug, Clone, Default)]
pub struct ResumeContext {
    /// An already-known transcript path for `resume()` to tail directly,
    /// skipping even the deterministic derivation. `Some` wins over the
    /// derived path; a caller that has only the session id leaves this
    /// `None`.
    pub transcript_path: Option<PathBuf>,
    /// The durable tailer position reached before the crash
    /// (`runs.transcript_cursor`, WP12), resumed from verbatim. `None`
    /// means nothing was ever durably consumed -- tailing starts at
    /// [`Cursor::start`], which cannot duplicate anything because every
    /// event batch persists its cursor transactionally with the events
    /// themselves.
    pub cursor: Option<Cursor>,
}

/// The vendor-agnostic TUI adapter: implements [`Adapter`] against any
/// [`TuiVendor`].
pub struct TuiAdapter<V: TuiVendor> {
    vendor: Arc<V>,
    cfg: AdapterConfig,
    /// Bound to this adapter instance at construction (not read from
    /// `StartSpec`), so `resume()` -- which carries no `StartSpec` at
    /// all -- has a correlation to stamp on its `AdapterEvent`s even
    /// from a *fresh* instance (e.g. after a genuine runtime restart),
    /// not only when resuming on the same instance that previously
    /// called `start()`. Mirrors `ClaudeAdapter`'s own `run_id`/`task_id`/
    /// `worker_id` fields exactly.
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    pane_coordinator: Arc<PaneCoordinator>,
    panes_dir: PathBuf,
    placement: DisplayPlacement,
    forced_backend: Option<DisplayBackend>,
    close_on_exit: CloseOnExit,
    timings: TuiTimings,
    /// Resume-time knowledge supplied at construction: see
    /// [`ResumeContext`]. Read only by `resume()` (and by `start()` when
    /// its `StartSpec.resume` is set -- the same continuation, reached
    /// through a different seam).
    resume: ResumeContext,
    run: AsyncMutex<Option<RunState>>,
}

impl<V: TuiVendor> TuiAdapter<V> {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vendor: V,
        cfg: AdapterConfig,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        pane_coordinator: Arc<PaneCoordinator>,
        panes_dir: PathBuf,
        placement: DisplayPlacement,
        forced_backend: Option<DisplayBackend>,
        close_on_exit: CloseOnExit,
        timings: TuiTimings,
        resume: ResumeContext,
    ) -> Self {
        Self {
            vendor: Arc::new(vendor),
            cfg,
            run_id,
            task_id,
            worker_id,
            pane_coordinator,
            panes_dir,
            placement,
            forced_backend,
            close_on_exit,
            timings,
            resume,
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

    /// The shared resume continuation, reached from two seams: a caller
    /// that already holds the session ref calls [`Adapter::resume`]
    /// directly, and a caller whose `StartSpec.resume` is set reaches the
    /// identical path through `start()` (WP14 wiring -- `StartSpec.resume`
    /// is never treated as a fresh launch with a flag bolted on).
    ///
    /// Respawns the vendor via [`TuiVendor::resume_launch`] (no prompt
    /// injection), reopens the attach socket and pane exactly like a
    /// fresh start, tails the transcript at the deterministic
    /// session-derived path (or an explicitly supplied one) starting from
    /// the stored cursor, and journals one resume-flavored
    /// `ProtocolHealthChanged` diagnostic. `run_slot` is the caller's
    /// already-held, already-checked-empty `self.run` guard -- see
    /// `run_pipeline`'s own doc comment for why it stays held throughout.
    async fn resume_from(
        &self,
        run_slot: &mut Option<RunState>,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        session: &VendorSessionRef,
        sink: Arc<dyn AdapterEventSink>,
    ) -> Result<(), AdapterError> {
        let placeholder = StartSpec {
            run_id,
            task_id,
            worker_id,
            prompt: String::new(),
            resume: Some(session.clone()),
        };
        let launch = self.vendor.resume_launch(session, &placeholder, &self.cfg);
        let transcript_root = self.vendor.transcript_root(&placeholder, &self.cfg);
        // Deterministic derivation first (WP14): the vendor's own layout +
        // the already-known session id. An explicitly supplied
        // `ResumeContext::transcript_path` still wins over the derivation.
        let transcript_path = self.resume.transcript_path.clone().unwrap_or_else(|| {
            self.vendor
                .transcript_path_for_session(session, &placeholder, &self.cfg)
        });
        let tail_from = self.resume.cursor.clone().unwrap_or_else(Cursor::start);

        // Resume-flavored diagnostics: journaled evidence that this run
        // continued an existing vendor session rather than starting fresh
        // (the respawn itself is the ordinary `ProcessStarted` below; the
        // re-established session id is re-journaled by the tailer's
        // initial `VendorSessionEstablished`). The offset is the position
        // tailing actually resumed from.
        emit(
            &sink,
            run_id,
            task_id,
            worker_id,
            AdapterEventPayload::ProtocolHealthChanged {
                healthy: true,
                detail: Classified {
                    class: ContentClass::Visible,
                    value: format!(
                        "resumed vendor session {}; tailing {} from byte offset {}",
                        session.0,
                        transcript_path.display(),
                        tail_from.offset
                    ),
                },
            },
            None,
        )
        .await;

        self.run_pipeline(
            run_slot,
            run_id,
            task_id,
            worker_id,
            launch,
            transcript_root,
            session.0.clone(),
            None,
            Some(transcript_path),
            tail_from,
            sink,
        )
        .await
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
        known_transcript_path: Option<PathBuf>,
        // The transcript position this pipeline's tailer starts from:
        // `Cursor::start` for a fresh start, the stored pre-crash
        // position (`ResumeContext::cursor`) for a resume -- never
        // hard-coded here, or a resumed session would replay (and
        // re-journal) events an earlier run already committed.
        tail_from: Cursor,
        sink: Arc<dyn AdapterEventSink>,
    ) -> Result<(), AdapterError> {
        let started_at = SystemTime::now();
        let pty = Arc::new(
            PtyProcess::spawn(&launch.into_spawn_spec(), self.timings.escalation)
                .map_err(|err| AdapterError::process(self.kind(), "start", err.to_string()))?,
        );
        // Captured immediately, before any other `.await` (the
        // `ProcessStarted` emit, `AttachServer::start`, and
        // `pane_coordinator.attach()` below all yield): a broadcast
        // receiver only sees values sent *after* it subscribes, and the
        // pump thread can start producing output the instant the process
        // is spawned, so subscribing any later risks missing the very
        // first output the readiness gate is waiting for -- which would
        // otherwise only resolve by waiting out the whole cap.
        let mut readiness_rx = pty.subscribe_output();

        emit(
            &sink,
            run_id,
            task_id,
            worker_id,
            AdapterEventPayload::ProcessStarted {
                pid: pty.pid() as u32,
            },
            None,
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
                        None,
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
            &mut readiness_rx,
            self.kind(),
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

        // A resume with an already-known transcript path (e.g. from a
        // stored cursor -- see `resume_transcript_path`'s doc comment)
        // skips discovery entirely: nonce-grepping a resumed session's
        // transcript is unreliable (the vendor may never re-touch it
        // within the discovery window) and, unlike a fresh start, there
        // is nothing this adapter itself just injected to search for.
        let transcript_path = match known_transcript_path {
            Some(path) => path,
            None => {
                let discovery_started_at = started_at
                    .checked_sub(Duration::from_secs(2))
                    .unwrap_or(started_at);
                match find_transcript_by_nonce(
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
                }
            }
        };

        // A best-effort initial guess (never a full path -- see
        // `TuiVendor::session_id_from_transcript_path`'s doc comment);
        // the first real `SessionMeta` entry the tailer encounters
        // corrects/confirms it via the identical `VendorSessionEstablished`
        // mapping in `emit_tui_event`. When the vendor cannot derive one
        // at all, this deliberately emits nothing rather than a
        // fabricated placeholder id (a prior "unknown" fallback here was
        // itself indistinguishable from a real vendor session id once
        // journaled) -- `runs.vendor_session_id` simply stays unset until
        // a real `SessionMeta` entry establishes it.
        if let Some(initial_session_id) = self
            .vendor
            .session_id_from_transcript_path(&transcript_path)
        {
            emit(
                &sink,
                run_id,
                task_id,
                worker_id,
                AdapterEventPayload::VendorSessionEstablished {
                    vendor_session_id: initial_session_id,
                },
                None,
            )
            .await;
        }

        let (batch_tx, mut batch_rx) = mpsc::unbounded_channel::<(Vec<TuiEvent>, Cursor)>();
        let tailer = TranscriptTailer::new(
            transcript_path,
            self.vendor.format(),
            tail_from,
            self.timings.tailer_poll,
        );
        let tailer_handle = Arc::new(tailer.spawn(move |events, cursor| {
            let _ = batch_tx.send((events, cursor));
        }));

        let pump_sink = Arc::clone(&sink);
        tokio::spawn(async move {
            while let Some((events, new_cursor)) = batch_rx.recv().await {
                for (event, batch_cursor) in cursor_placements(events, new_cursor) {
                    emit_tui_event(&pump_sink, run_id, task_id, worker_id, event, batch_cursor)
                        .await;
                }
            }
        });

        let (terminate_tx, terminate_rx) = oneshot::channel();
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
            terminate_rx,
        );

        *run_slot = Some(RunState {
            run_id,
            task_id,
            worker_id,
            pty,
            watcher,
            terminate_tx,
            sink,
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
            None,
        )
        .await;
        Err(err)
    }
}

/// Emits one event through `sink`, logging (never panicking) if the
/// journal write itself failed -- mirrored from every other adapter's
/// best-effort telemetry emission (a lost telemetry event must never be
/// fatal to the run). `cursor` is `Some` only for the one emitted event
/// (if any) that concludes a tailed transcript batch -- see
/// `emit_tui_event`'s own doc comment.
async fn emit(
    sink: &Arc<dyn AdapterEventSink>,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    payload: AdapterEventPayload,
    cursor: Option<Cursor>,
) {
    if let Err(err) = sink
        .emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload,
            cursor,
        })
        .await
    {
        tracing::warn!(error = %err, run_id = %run_id, "tui adapter failed to journal an event");
    }
}

/// Pairs each event of one tailed batch with the cursor it should carry
/// when emitted (`emit_tui_event`'s own `cursor` parameter): `Some(new_cursor)`
/// on the batch's last *emitting* event (`super::last_emitting_index`),
/// `None` on every other one -- never unconditionally the batch's last
/// event, which may be a trailing `TurnEnded`/`Raw` that emits nothing at
/// all. Extracted from the pump loop as its own pure function so the
/// placement rule is unit-testable without a real tailer/vendor/PTY, and
/// so a regression in the pump loop's wiring (not just in
/// `last_emitting_index` itself) has exactly one function standing
/// between it and this module's own tests.
fn cursor_placements(events: Vec<TuiEvent>, new_cursor: Cursor) -> Vec<(TuiEvent, Option<Cursor>)> {
    let last_emitting = super::last_emitting_index(&events);
    events
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            let cursor = if Some(index) == last_emitting {
                Some(new_cursor.clone())
            } else {
                None
            };
            (event, cursor)
        })
        .collect()
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
///
/// `cursor` is the tailer's advanced position for the *entire batch* this
/// `event` came from, passed by the caller (the pump loop, via
/// `cursor_placements`) only on the batch's last *emitting* `TuiEvent` --
/// never unconditionally the batch's last `TuiEvent`, which may be a
/// trailing `TurnEnded`/`Raw` that emits nothing at all; attaching it
/// there would leave the stored cursor pointing before an event this call
/// already journaled -- `None` for every other `TuiEvent` in the batch. It is
/// attached here to whichever of this event's own emitted payloads is
/// emitted last (`ToolResult` rather than `ToolStarted` for
/// `ToolActivity`), so the durable cursor and the event(s) that observed
/// everything up to it commit together. A batch whose *every* `TuiEvent`
/// emits nothing (`last_emitting_index` returns `None`) never has its
/// advance persisted at all, which is safe -- nothing was journaled for
/// any of it the first time either, so re-parsing the whole batch after a
/// crash produces no duplicate.
async fn emit_tui_event(
    sink: &Arc<dyn AdapterEventSink>,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    event: TuiEvent,
    cursor: Option<Cursor>,
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
            emit(sink, run_id, task_id, worker_id, payload, cursor).await;
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
                None,
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
                cursor,
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
                cursor,
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
/// error. `rx` must already be subscribed *before* this is called --
/// see the caller's own comment on why it is captured immediately after
/// spawn rather than here.
async fn wait_for_readiness(
    rx: &mut broadcast::Receiver<Vec<u8>>,
    kind: &str,
    quiet: Duration,
    cap: Duration,
) -> Result<(), AdapterError> {
    let deadline = tokio::time::Instant::now() + cap;

    // Wait for the first output (a single check, not a loop: every
    // outcome below either proceeds past this point or returns).
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(AdapterError::process(
            kind,
            "start",
            "no output observed on the pty before the readiness cap elapsed",
        ));
    }
    match tokio::time::timeout(remaining, rx.recv()).await {
        Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
        Ok(Err(broadcast::error::RecvError::Closed)) => {
            return Err(AdapterError::process(
                kind,
                "start",
                "the worker process exited before producing any output",
            ));
        }
        Err(_) => {
            return Err(AdapterError::process(
                kind,
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

/// Spawns the single task that owns this run's settlement: races the PTY
/// exiting naturally against a termination request from
/// [`TuiAdapter::cancel`]/`dispose` (`terminate_rx`) -- this task is the
/// *only* caller of [`PtyProcess::terminate`] once a run is tailing;
/// `fail_start` owns the pre-tail phase (a run that never reached this
/// watcher at all, because spawn/attach/readiness/injection/discovery
/// itself failed, terminates the pty itself on that separate,
/// self-contained path -- see `fail_start`'s own doc comment). So there
/// is exactly one place that ever decides the real `exit_code`/`signal`
/// for an induced exit *once a run is being tailed* (no race between
/// this task reading one outcome and `cancel` reading a different one
/// for the same termination). Either way, once
/// the process is down, stops the tailer and attach server, honors
/// `close_on_exit` through the pane coordinator, and journals
/// `ProcessExited` -- exactly once, from exactly one place.
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
    terminate_rx: oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (exit_code, signal) = tokio::select! {
            status = pty.exit_watcher() => (Some(status.exit_code() as i32), None),
            _ = terminate_rx => {
                pty.terminate().await.exit_signals()
            }
        };
        tailer.stop();
        attach.stop();
        let succeeded = exit_code == Some(0) && signal.is_none();
        pane_coordinator
            .detach(&pane_outcome, succeeded, close_on_exit)
            .await;
        emit(
            &sink,
            run_id,
            task_id,
            worker_id,
            AdapterEventPayload::ProcessExited { exit_code, signal },
            None,
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
            // A `StartSpec` that carries a session ref is not a fresh
            // start wearing a flag -- it *is* a resume (WP14 wiring): the
            // vendor continues its existing session, nothing is injected,
            // and tailing picks up from the stored position. Fresh ids on
            // the spec are the correlation (the registry binds the same
            // run/task/worker into this adapter at construction).
            if let Some(session) = spec.resume {
                return self
                    .resume_from(
                        &mut guard,
                        spec.run_id,
                        spec.task_id,
                        spec.worker_id,
                        &session,
                        sink,
                    )
                    .await;
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
                None,
                Cursor::start(),
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
            // No `StartSpec` carries this adapter's real ids across
            // `Adapter::resume`'s signature; `self.run_id`/`task_id`/
            // `worker_id` (bound at construction, see `TuiAdapter`'s own
            // doc comment) are what every emitted event is stamped with
            // -- never fabricated fresh ids for a run/task/worker this
            // adapter has no correlation to.
            self.resume_from(
                &mut guard,
                self.run_id,
                self.task_id,
                self.worker_id,
                &session,
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
                    cursor: None,
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
                    // Signal the exit watcher (spawned once in
                    // `run_pipeline`) to call `PtyProcess::terminate`
                    // itself and perform the one-and-only teardown +
                    // `ProcessExited` emission for this run; `cancel`
                    // itself never terminates, emits, or tears down
                    // directly (mirrors `CodexAdapter::cancel`'s own
                    // "the pump must not be aborted here" discipline, and
                    // avoids a race over which caller's `terminate()`
                    // result gets journaled). A dropped receiver (the
                    // watcher already finished -- the process had
                    // already exited naturally) makes this a no-op.
                    let _ = run.terminate_tx.send(());
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
            Ok(AdapterSnapshot {
                state_summary: format!("tui[{}] pane={}", self.kind(), run.pane_ref),
                children: Vec::new(),
                // The tailer's durable position no longer needs to be
                // smuggled out here (WP12 handoff, closed): every adapter
                // event batch now carries its own `Cursor` through the
                // sink into `runs.transcript_cursor`, transactionally with
                // the batch's journaled event(s). `usage: None` matches
                // `capabilities().usage == UsageCapability::None` -- a TUI
                // adapter has no vendor-reported cost/token usage at all.
                usage: None,
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
            // See `cancel`'s own comment: the exit watcher is the only
            // caller of `terminate()`. Awaiting it (unlike `cancel`)
            // ensures teardown has actually finished before `dispose`
            // returns.
            let _ = run.terminate_tx.send(());
            let _ = run.watcher.await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    //! Unit-tests `cursor_placements` directly -- the exact function the
    //! pump loop in `run_pipeline` calls, not a reimplementation of it.
    //! A future edit that reverts the pump loop to placing the cursor by
    //! bare last-index (the WP12 review's Finding 1 bug) breaks these
    //! tests immediately, because there would be no other caller for
    //! `cursor_placements` to keep it alive/correct.

    use crew_protocol::{Classified, ContentClass};

    use super::*;

    fn text(value: &str) -> TuiEvent {
        TuiEvent::AssistantText {
            text: Classified {
                class: ContentClass::Visible,
                value: value.to_string(),
            },
            is_question: false,
            ts: None,
        }
    }

    #[test]
    fn a_trailing_turn_ended_does_not_take_the_cursor_from_the_message_before_it() {
        let cursor = Cursor {
            offset: 10,
            last_entry_id: Some("e1".to_string()),
        };
        let placements =
            cursor_placements(vec![text("first"), TuiEvent::TurnEnded], cursor.clone());

        assert_eq!(placements.len(), 2);
        assert_eq!(
            placements[0].1,
            Some(cursor),
            "the AssistantText, the batch's only emitting event, must carry the cursor"
        );
        assert_eq!(
            placements[1].1, None,
            "TurnEnded emits nothing and must never carry a cursor"
        );
    }

    #[test]
    fn a_trailing_raw_does_not_take_the_cursor_from_the_message_before_it() {
        let cursor = Cursor::start();
        let placements = cursor_placements(
            vec![
                text("first"),
                TuiEvent::Raw {
                    entry_type: "unknown".to_string(),
                },
            ],
            cursor.clone(),
        );

        assert_eq!(placements[0].1, Some(cursor));
        assert_eq!(placements[1].1, None);
    }

    #[test]
    fn the_cursor_rides_the_last_of_several_emitting_events() {
        let cursor = Cursor::start();
        let placements = cursor_placements(
            vec![text("first"), text("second"), TuiEvent::TurnEnded],
            cursor.clone(),
        );

        assert_eq!(placements[0].1, None);
        assert_eq!(placements[1].1, Some(cursor));
        assert_eq!(placements[2].1, None);
    }

    #[test]
    fn a_batch_where_nothing_emits_persists_no_cursor_at_all() {
        let placements = cursor_placements(
            vec![
                TuiEvent::TurnEnded,
                TuiEvent::Raw {
                    entry_type: "unknown".to_string(),
                },
            ],
            Cursor::start(),
        );

        assert!(placements.iter().all(|(_, cursor)| cursor.is_none()));
    }
}
