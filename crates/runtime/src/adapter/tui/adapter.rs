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
    /// How long the PTY must stay output-silent before the prompt's Enter
    /// (and queue-style `send`s) are delivered -- see [`ENTER_IDLE_MIN`].
    pub submit_idle: Duration,
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
            submit_idle: ENTER_IDLE_MIN,
            escalation: EscalationTimings::default(),
        }
    }
}

/// Earliest moment -- measured from `PtyProcess::spawn` -- at which the
/// prompt text may be typed into the vendor TUI: bytes written before the
/// vendor has wired its stdin are silently dropped. Text-only, never the
/// submit byte -- see [`ENTER_IDLE_MIN`].
const INJECT_MIN_DELAY: Duration = Duration::from_millis(500);

/// How long the PTY must have been output-silent before the prompt's Enter
/// is delivered. Silence this deep cannot be a working turn (a running turn
/// animates its spinner continuously) and no turn can exist yet anyway --
/// the submit byte is the very first one ever sent -- so silence here means
/// exactly "idle TUI holding our text", where Enter behaves like a human's.
const ENTER_IDLE_MIN: Duration = Duration::from_secs(10);

/// Guard for [`ENTER_IDLE_MIN`]: if the PTY still has not gone quiet by
/// then (e.g. a vendor that emits keep-alive frames forever), deliver the
/// Enter regardless rather than fail the run -- that degrades to today's
/// single-shot behavior instead of adding a new failure mode.
const ENTER_IDLE_CAP: Duration = Duration::from_secs(90);

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
    /// Freshest PTY-output instant, kept current by the watcher spawned in
    /// `run_pipeline`; `send` waits on it so queue-style messages are typed
    /// into an idle REPL (codex drops mid-turn keystrokes) instead of into
    /// an active turn.
    last_output: Arc<StdMutex<tokio::time::Instant>>,
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
        // Injection floor is anchored to the spawn instant, not to when the
        // pipeline reaches the inject call: the emit/attach/pane steps above
        // are unbounded async work that would otherwise shift the prompt
        // past the vendor's auto-submit window.
        let spawn_instant = tokio::time::Instant::now();
        // Captured immediately, before any other `.await` (the
        // `ProcessStarted` emit, `AttachServer::start`, and
        // `pane_coordinator.attach()` below all yield): a broadcast
        // receiver only sees values sent *after* it subscribes, and the
        // pump thread can start producing output the instant the process
        // is spawned, so subscribing any later risks missing the very
        // first output the readiness gate is waiting for -- which would
        // otherwise only resolve by waiting out the whole cap.
        let mut readiness_rx = pty.subscribe_output();

        // Tracks the freshest PTY output instant for phase 2: the prompt's
        // Enter is only delivered once output has been quiet for
        // [`ENTER_IDLE_MIN`], proving the TUI is idle rather than mid-render
        // or mid-turn.
        let last_output = Arc::new(StdMutex::new(tokio::time::Instant::now()));
        {
            let mut idle_rx = pty.subscribe_output();
            let last = Arc::clone(&last_output);
            tokio::spawn(async move {
                loop {
                    match idle_rx.recv().await {
                        Ok(_) => {
                            *last.lock().expect("last-output mutex never poisoned") =
                                tokio::time::Instant::now();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

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

        // Two-phase prompt delivery, mirroring how a human drives the TUI.
        // Phase 1 (inside wait_for_readiness): type the prompt TEXT once the
        // vendor's stdin is wired -- never earlier ([`INJECT_MIN_DELAY`]),
        // never with the submit byte, which a mid-render TUI swallows.
        // Phase 2 (below): deliver the single Enter only after the PTY has
        // gone output-silent for [`ENTER_IDLE_MIN`] -- since no submit byte
        // was ever sent before, that silence can only mean "idle TUI holding
        // our text", so the Enter lands exactly like a human's keystroke
        // regardless of how fast or slow the vendor's startup render was.
        let injected_bytes: Option<Vec<u8>> =
            inject.as_ref().map(|text| self.vendor.compose_input(text));
        // `compose_input`'s contract is message plus exactly one trailing
        // CR; split it so phase 1 types the text and phase 2 owns the Enter.
        let (type_bytes, enter_byte): (Option<&[u8]>, Option<&[u8]>) =
            match injected_bytes.as_deref() {
                Some(bytes) => (
                    Some(&bytes[..bytes.len() - 1]),
                    Some(&bytes[bytes.len() - 1..]),
                ),
                None => (None, None),
            };
        if let Err(err) = wait_for_readiness(
            &mut readiness_rx,
            self.kind(),
            self.timings.readiness_quiet,
            self.timings.readiness_cap,
            &pty,
            type_bytes,
            spawn_instant + INJECT_MIN_DELAY,
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

        if let Some(enter) = enter_byte {
            if let Err(err) =
                wait_for_output_idle(&last_output, self.timings.submit_idle, ENTER_IDLE_CAP).await
            {
                tracing::debug!(kind = self.kind(), "{err}");
            }
            // A write failure here means the worker already exited; the exit
            // watcher owns reporting that -- nothing useful to add.
            let _ = pty.write_input(enter).await;
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

        let (batch_tx, mut batch_rx) =
            mpsc::unbounded_channel::<(Vec<(TuiEvent, Cursor)>, Cursor)>();
        let tailer = TranscriptTailer::new(
            transcript_path,
            self.vendor.format(),
            tail_from,
            self.timings.tailer_poll,
        );
        let tailer_handle = Arc::new(tailer.spawn(move |tagged, cursor| {
            let _ = batch_tx.send((tagged, cursor));
        }));

        let pump_sink = Arc::clone(&sink);
        tokio::spawn(async move {
            while let Some((tagged, _new_cursor)) = batch_rx.recv().await {
                for (event, cursor) in cursor_placements(tagged) {
                    emit_tui_event(&pump_sink, run_id, task_id, worker_id, event, cursor).await;
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
            last_output,
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
/// fatal to the run). `cursor` is `Some` for every emitted event of a
/// tailed transcript batch, because `parse` pairs each event with its own
/// post-line `Cursor` (per-event idempotency); see
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
/// when emitted (`emit_tui_event`'s own `cursor` parameter).
///
/// `tagged` is straight from `parse`: every event is paired with its
/// *line's* own post-line `Cursor`, so two or more consecutive events
/// that share an identical `Cursor` value always originate from the same
/// transcript line (`parse_jsonl_chunk` never assigns one line's cursor
/// to another line's events). A single line commonly maps to more than
/// one `TuiEvent` -- e.g. a Claude entry that is both the session's first
/// `SessionMeta` and an `AssistantText` -- and each event still commits
/// through its own, separate journal transaction (`emit` -> one
/// `AdapterEventSink::emit` call each). Letting more than one of them
/// carry that identical `Cursor` would mean a crash between two such
/// commits durably advances `runs.transcript_cursor` past the whole line
/// on the *first* commit, so the still-uncommitted sibling is never
/// re-tailed on resume -- silently lost forever rather than safely
/// re-emitted.
///
/// So within each run of events sharing one `Cursor`, only the run's last
/// *emitting* event (`TuiEvent::emits_a_payload`, via `last_emitting_index`)
/// may carry it forward (`Some`); every other event in the run -- earlier
/// emitting siblings and any non-emitting `TurnEnded`/`Raw` tail alike --
/// gets `None`. A crash before that one carrier event's own commit simply
/// re-tails and re-emits the whole line on resume: duplication, never
/// loss. A run with no emitting event at all (`last_emitting_index`
/// returns `None`) persists no cursor for it either, which is safe: none
/// of its events were journaled the first time, so re-parsing produces no
/// duplicate.
///
/// Extracted from the pump loop as its own pure function so the placement
/// rule is unit-testable without a real tailer/vendor/PTY, and so a
/// regression in the pump loop's wiring (not just in `last_emitting_index`
/// itself) has exactly one function standing between it and this module's
/// own tests.
pub(crate) fn cursor_placements(
    tagged: Vec<(TuiEvent, Cursor)>,
) -> Vec<(TuiEvent, Option<Cursor>)> {
    let mut carries_cursor = vec![false; tagged.len()];
    let mut start = 0;
    while start < tagged.len() {
        let mut end = start;
        while end + 1 < tagged.len() && tagged[end + 1].1 == tagged[start].1 {
            end += 1;
        }
        // `end` is the run's last index (inclusive) -- every event in
        // `tagged[start..=end]` shares one line's `Cursor`.
        if let Some(offset) = tagged[start..=end]
            .iter()
            .rposition(|(event, _)| event.emits_a_payload())
        {
            carries_cursor[start + offset] = true;
        }
        start = end + 1;
    }
    tagged
        .into_iter()
        .zip(carries_cursor)
        .map(|((event, cursor), carries)| (event, carries.then_some(cursor)))
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
/// `cursor` is this event's placement from `cursor_placements` (`Some`
/// only for the last emitting event of the transcript line it came from,
/// `None` otherwise) -- see that function's own doc comment for why. It
/// is attached here to whichever of this event's own emitted payloads is
/// emitted last (`ToolResult` rather than `ToolStarted` for
/// `ToolActivity`), so the durable cursor and the event that observed
/// everything up to it commit together.
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
    pty: &Arc<PtyProcess>,
    inject: Option<&[u8]>,
    not_before: tokio::time::Instant,
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
        Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
            // First output appeared: the vendor's stdin is wired. Hold
            // until the spawn-anchored floor, then deliver the prompt text.
            // Deliberately WITHOUT the submit byte: a CR sent here can be
            // swallowed by the render loop mid-layout. The Enter itself is
            // delivered by the caller once the PTY has gone quiet (see the
            // phase-2 block in `run_pipeline`) -- an idle TUI processes it
            // exactly like a human's keystroke, with no timing assumption
            // about startup speed at all.
            if tokio::time::Instant::now() < not_before {
                tokio::time::sleep_until(not_before).await;
            }
            if let Some(bytes) = inject
                && let Err(err) = pty.write_input(bytes).await
            {
                return Err(AdapterError::process(
                    kind,
                    "start",
                    format!("initial prompt injection failed: {err}"),
                ));
            }
        }
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

/// Waits until the PTY output has been silent for `required` (phase 2 of
/// prompt delivery -- see the caller's comment). Gives up waiting at `cap`
/// and reports it via the returned error so the caller can decide to
/// proceed anyway; a vendor that genuinely never goes quiet gets today's
/// single-shot behavior rather than a new failure mode.
async fn wait_for_output_idle(
    last_output: &StdMutex<tokio::time::Instant>,
    required: Duration,
    cap: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + cap;
    loop {
        let idle_for = tokio::time::Instant::now().saturating_duration_since(
            *last_output
                .lock()
                .expect("last-output mutex never poisoned"),
        );
        if idle_for >= required {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "pty output never stayed quiet for {required:?}; delivering the \
                 submit keystroke on best-effort terms"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
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
            let injected = format!("{} [crew:{}]", spec.prompt, nonce);
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
            // A Steer interrupts the in-flight turn before composing: the
            // leader is REDIRECTING work, not queueing more of it (WP20).
            // Every other kind queues after the current turn.
            if matches!(message, AdapterMessage::Steer { .. }) {
                run.pty
                    .write_input(&self.vendor.interrupt_sequence())
                    .await
                    .map_err(|e| AdapterError::process(self.kind(), "send", e.to_string()))?;
            }
            let text: &String = match &message {
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
            let bytes = self.vendor.compose_input(text);
            // Queue-style messages must land in an IDLE REPL: codex drops
            // keystrokes typed mid-turn outright. Wait for output silence
            // first; if a vendor never goes quiet the cap expires and the
            // write proceeds immediately. Steer is exempt -- it already
            // interrupted the turn above.
            if !matches!(message, AdapterMessage::Steer { .. })
                && let Err(err) =
                    wait_for_output_idle(&run.last_output, self.timings.submit_idle, ENTER_IDLE_CAP)
                        .await
            {
                tracing::debug!(kind = self.kind(), "{err}");
            }
            // Mirror run_pipeline's split delivery: TEXT and the submit CR
            // travel as separate writes. An atomic `text\r` can be swallowed
            // whole by a vendor TUI running in bracketed-paste mode (the CR
            // becomes paste content instead of a submit), where a lone CR
            // after the text has landed behaves like a human's Enter.
            let split_at = bytes.len() - 1;
            if let Err(err) = run.pty.write_input(&bytes[..split_at]).await {
                return Err(AdapterError::process(self.kind(), "send", err.to_string()));
            }
            // The gap is load-bearing: a CR arriving microseconds after the
            // text is glued into the same input chunk and swallowed (observed
            // against live codex), while a discrete keypress ~150ms later
            // submits reliably.
            tokio::time::sleep(Duration::from_millis(150)).await;
            run.pty
                .write_input(&bytes[split_at..])
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
    //! pump loop in `run_pipeline` calls, not a reimplementation of it --
    //! pinning the placement RULE itself (which index in a same-cursor run
    //! carries `Some`) against every shape: no run, a single emitting
    //! event, a trailing non-emitting tail, multiple emitting events in
    //! one run, and no emitting event at all.
    //!
    //! These tests alone do not prove the pump loop's own WIRING to this
    //! function (that `run_pipeline` actually calls it and threads its
    //! `Option<Cursor>` into `emit_tui_event` unchanged) -- that end-to-end
    //! proof is `event_sink.rs`'s `crash_resume_tests` module, whose
    //! `emit_batch` test helper calls this exact function too (not a
    //! reimplementation) and exercises it at the journal level, including
    //! a same-line multi-event batch and a simulated crash landing between
    //! two same-line commits. What THIS module's tests do still guarantee:
    //! a future edit that reverts the pump loop to placing every event's
    //! own line cursor unconditionally (the duplication-vs-loss bug this
    //! function exists to prevent) leaves `cursor_placements` with no
    //! caller outside `#[cfg(test)]` code, so a normal (non-test) build
    //! surfaces it as dead code rather than silently compiling clean.

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

    fn tagged(events: Vec<TuiEvent>, cursor: Cursor) -> Vec<(TuiEvent, Cursor)> {
        events.into_iter().map(|e| (e, cursor.clone())).collect()
    }

    #[test]
    fn a_trailing_turn_ended_does_not_take_the_cursor_from_the_message_before_it() {
        let cursor = Cursor {
            offset: 10,
            last_entry_id: Some("e1".to_string()),
        };
        let placements = cursor_placements(tagged(
            vec![text("first"), TuiEvent::TurnEnded],
            cursor.clone(),
        ));

        assert_eq!(placements.len(), 2);
        assert_eq!(
            placements[0].1,
            Some(cursor),
            "the AssistantText, this line's only emitting event, must carry the cursor"
        );
        assert_eq!(
            placements[1].1, None,
            "TurnEnded emits nothing and must never carry a cursor"
        );
    }

    #[test]
    fn a_trailing_raw_does_not_take_the_cursor_from_the_message_before_it() {
        let cursor = Cursor::start();
        let placements = cursor_placements(tagged(
            vec![
                text("first"),
                TuiEvent::Raw {
                    entry_type: "unknown".to_string(),
                },
            ],
            cursor.clone(),
        ));

        assert_eq!(placements[0].1, Some(cursor));
        assert_eq!(placements[1].1, None);
    }

    /// The C1-followup bug this function exists to close: one transcript
    /// line mapping to two emitting events (e.g. Claude's `SessionMeta` +
    /// `AssistantText` from a single entry) means `parse` hands both the
    /// identical line `Cursor`. Only the *last* of them may carry it --
    /// otherwise a crash between their two separate journal commits would
    /// durably advance the cursor on the first commit and silently lose
    /// the second, uncommitted one on resume.
    #[test]
    fn only_the_last_of_several_same_line_emitting_events_carries_that_lines_cursor() {
        let cursor = Cursor::start();
        let placements = cursor_placements(tagged(
            vec![
                TuiEvent::SessionMeta {
                    vendor_session_id: "sess-1".to_string(),
                },
                text("first"),
            ],
            cursor.clone(),
        ));

        assert_eq!(
            placements[0].1, None,
            "SessionMeta shares its line's cursor with a later event and must not carry it"
        );
        assert_eq!(
            placements[1].1,
            Some(cursor),
            "AssistantText is the last emitting event on this line and must carry the cursor"
        );
    }

    /// A batch spans multiple lines, each with its own distinct cursor;
    /// each line's carrier decision must be made independently of the
    /// others.
    #[test]
    fn each_line_in_a_batch_gets_its_own_independent_carrier() {
        let first_line = Cursor {
            offset: 5,
            last_entry_id: Some("e1".to_string()),
        };
        let second_line = Cursor {
            offset: 11,
            last_entry_id: Some("e2".to_string()),
        };
        let mut batch = tagged(vec![text("first")], first_line.clone());
        batch.extend(tagged(
            vec![
                TuiEvent::SessionMeta {
                    vendor_session_id: "sess-1".to_string(),
                },
                text("second"),
            ],
            second_line.clone(),
        ));

        let placements = cursor_placements(batch);

        assert_eq!(placements[0].1, Some(first_line));
        assert_eq!(placements[1].1, None);
        assert_eq!(placements[2].1, Some(second_line));
    }

    #[test]
    fn a_line_where_nothing_emits_persists_no_cursor_at_all() {
        let placements = cursor_placements(tagged(
            vec![
                TuiEvent::TurnEnded,
                TuiEvent::Raw {
                    entry_type: "unknown".to_string(),
                },
            ],
            Cursor::start(),
        ));

        assert!(placements.iter().all(|(_, cursor)| cursor.is_none()));
    }
}
