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
use crate::supervisor::{EscalationTimings, PtyProcess, SupervisorError};

use super::classify::{GateKind, Surface};
use super::discovery::{DiscoveryError, find_transcript_by_nonce};
use super::grid::TerminalGrid;
use super::input::{PASTE_CHUNK_BYTES, paste_chunks};
use super::tailer::{TailerHandle, TranscriptTailer};
use super::verify::{PromptVerdict, verify_recorded_prompt};
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
/// spawn, attach, discovery, and tailing. [`Self::preflight`] is the one
/// documented exception: a vendor whose CLI can only answer a
/// compatibility question (e.g. "is this model selector real?") via a
/// real fetch, not a fixed rule, may perform that one process spawn there.
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
    /// This is what makes resume reliable: unlike a fresh start,
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
    ///
    /// **Contract:** the return value must be `message`'s bytes verbatim
    /// followed by exactly one trailing submit byte. The adapter shell
    /// splits the two and uses only that trailing byte: the text half is
    /// delivered by `write_paste`, which frames it as a bracketed paste
    /// and chunks it, so a vendor that transformed the text here
    /// would have its transformation silently discarded. A vendor needing
    /// a different submit convention changes the trailing byte; one
    /// needing to rewrite the text has no supported way to do it here.
    /// Debug builds assert this at both call sites.
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

    /// An optional real-process preflight run before a *fresh* start's
    /// spawn (never on resume, which continues an already-established
    /// session rather than choosing a new one) -- e.g. a selector-
    /// compatibility check the vendor's own CLI can only answer via a
    /// real fetch. Default: no preflight, every start proceeds
    /// unconditionally. Returning `Err` refuses the start before any PTY
    /// is spawned, with the same typed-rejection shape [`Self::version_gate`]
    /// produces from [`TuiAdapter::probe`] -- see the trait doc's "pure"
    /// exception this method is. `timings.preflight_timeout` bounds
    /// whatever process an override spawns -- a vendor implementation
    /// must honor it, never wait on its own command unbounded.
    fn preflight(
        &self,
        _spec: &StartSpec,
        _cfg: &AdapterConfig,
        _timings: &TuiTimings,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    /// Classifies what this vendor's TUI is currently showing, from a
    /// [`TerminalGrid`] built from the bytes it has written to the PTY
    /// since spawn. Pure, like every other method here: no process spawn,
    /// no I/O -- the grid is already built, this only reads it.
    ///
    /// `None` means this vendor has no real predicate yet: [`wait_for_readiness`]
    /// falls back to its legacy behavior for it (any output means
    /// ready, no gate can be recognized), rather than fail-closing on a
    /// vendor this slice was never asked to cover. This is a DIFFERENT
    /// claim from [`Surface::Undecided`], which a real classifier (claude,
    /// codex) returns when the screen genuinely matches no known gate or
    /// prompt yet -- conflating the two would fail-close every copilot/omp
    /// start the day this default stops being overridden for them too.
    fn classify_surface(&self, _grid: &TerminalGrid) -> Option<Surface> {
        None
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
    /// How long a single paste chunk may sit unaccepted by the PTY before
    /// the prompt is declared undeliverable -- see
    /// [`PASTE_CHUNK_WRITE_TIMEOUT`], which is this field's production
    /// value.
    ///
    /// **A failure bound, not a pacing delay.** Every other `Duration` in
    /// this struct is time a caller actually spends waiting, so shrinking
    /// it in a test makes the test faster. A timeout costs wall-clock time
    /// only when it FIRES, so shrinking this one makes nothing faster and
    /// its only other effect is to manufacture false failures on a loaded
    /// machine: an accelerated 500ms here failed
    /// `a_multi_line_prompt_reaches_the_pty_framed_as_one_intact_paste`
    /// 100% of the time under CPU load and ~8% of the time idle, because
    /// [`PtyProcess::write_input`] awaits an ack from a separate writer
    /// thread -- so this budget covers our own channel queueing and thread
    /// scheduling, not just the vendor's read. Shorten it ONLY in a test
    /// that deliberately trips it, and shorten it there and not globally.
    pub paste_write_timeout: Duration,
    /// SIGINT/SIGTERM/SIGKILL escalation timings for [`PtyProcess`].
    pub escalation: EscalationTimings,
    /// The bound on [`TuiVendor::preflight`]'s own external command, if it
    /// runs one -- a vendor whose preflight spawns a real process (e.g.
    /// [`super::omp::OmpTuiVendor`]'s model-catalog check) must never let
    /// a wedged command stall `start()` indefinitely on the daemon's
    /// fresh-start path while holding the adapter's run lock. Same
    /// convention as every other externally-bounded wait here
    /// ([`Self::readiness_cap`], [`Self::discovery_timeout`]).
    pub preflight_timeout: Duration,
}

/// How long a conformance scenario waits to *observe* an expected event
/// before declaring the scenario unproven.
///
/// **The invariant, and it is the reason this is one constant rather than
/// a literal per call site: an observation deadline must strictly dominate
/// the production bound it waits behind.** A scenario that waits less time
/// than the adapter is permitted to take fails before the thing it is
/// testing does — so the test reports a defect that is really its own
/// impatience, and it does so at whatever rate the machine happens to be
/// slow.
///
/// That inversion was present before this constant existed and was not
/// load-dependent: the cancel scenarios waited 5s for `ProcessExited`
/// while passing production's [`EscalationTimings`], whose default budget
/// is 5s to SIGTERM plus a further 5s to SIGKILL. A vendor that needed
/// full escalation failed the scenario on an idle machine.
///
/// 20 seconds is chosen to dominate every bound a scenario can sit behind
/// — the 10s escalation total being the largest a scenario waits behind —
/// and the assertions in
/// `the_observation_deadline_dominates_every_bound_a_scenario_waits_behind`
/// are what keep that true if a production bound is ever raised. That test
/// also records why `paste_write_timeout` (90s) is not among them. Like any failure
/// bound it costs wall-clock time only when it fires, so its size buys
/// nothing and its generosity costs nothing.
pub(crate) const SCENARIO_OBSERVATION_DEADLINE: Duration = Duration::from_secs(20);

/// Asserts that a test harness's [`TuiTimings`] accelerates only *pacing*
/// fields and leaves every *failure bound* at production's value.
///
/// **The distinction, once, for the group.** A pacing field is time a
/// caller actually spends waiting, so shrinking it is what makes a suite
/// fast. A failure bound costs wall-clock time only when it FIRES — so
/// shrinking one makes nothing faster, and its only other effect is
/// manufacturing false failures on a loaded machine. That was learned once
/// already on `paste_write_timeout`, after an accelerated 500ms bound
/// failed a bracketed-paste test 100% of the time under CPU load; the same
/// defect then turned up in three sibling fields of the same struct, which
/// had been left accelerated in all four conformance harnesses because the
/// rule was applied to the field that had failed rather than to the kind
/// it named.
///
/// **This is a compile-time guard, not only a runtime one.** The
/// destructuring below is exhaustive on purpose: a field added to
/// `TuiTimings` — or to `EscalationTimings` — fails to compile here until
/// whoever added it has decided which kind it is. A runtime list of
/// failure bounds would silently not cover a new one, which is how the
/// three fields this closes were missed.
#[cfg(test)]
pub(crate) fn assert_only_pacing_is_accelerated(harness: TuiTimings, harness_name: &str) {
    let production = TuiTimings::default();

    let TuiTimings {
        // Pacing — the exception list, and the only fields a harness may
        // accelerate. Bound and ignored deliberately rather than omitted,
        // so this stays exhaustive.
        readiness_quiet: _,
        tailer_poll: _,
        submit_idle: _,
        // Failure bounds — every one must equal production's.
        readiness_cap,
        discovery_timeout,
        preflight_timeout,
        paste_write_timeout,
        escalation:
            crate::supervisor::EscalationTimings {
                sigint_to_sigterm,
                sigterm_to_sigkill,
            },
    } = harness;

    for (field, actual, expected) in [
        ("readiness_cap", readiness_cap, production.readiness_cap),
        (
            "discovery_timeout",
            discovery_timeout,
            production.discovery_timeout,
        ),
        (
            "preflight_timeout",
            preflight_timeout,
            production.preflight_timeout,
        ),
        (
            "paste_write_timeout",
            paste_write_timeout,
            production.paste_write_timeout,
        ),
        (
            "escalation.sigint_to_sigterm",
            sigint_to_sigterm,
            production.escalation.sigint_to_sigterm,
        ),
        (
            "escalation.sigterm_to_sigkill",
            sigterm_to_sigkill,
            production.escalation.sigterm_to_sigkill,
        ),
    ] {
        assert_eq!(
            actual, expected,
            "{harness_name}'s fast timings accelerate the failure bound `{field}` \
             ({actual:?} against production's {expected:?}). A timeout costs wall-clock time \
             only when it fires, so accelerating it makes no test faster and only manufactures \
             false failures under load -- read `TuiTimings::default().{field}` instead. If this \
             field is genuinely pacing rather than a bound, move it to the ignored group above \
             and say why."
        );
    }
}

impl Default for TuiTimings {
    fn default() -> Self {
        Self {
            readiness_quiet: Duration::from_millis(700),
            readiness_cap: Duration::from_secs(8),
            discovery_timeout: Duration::from_secs(8),
            tailer_poll: Duration::from_millis(200),
            submit_idle: ENTER_IDLE_MIN,
            paste_write_timeout: PASTE_CHUNK_WRITE_TIMEOUT,
            escalation: EscalationTimings::default(),
            preflight_timeout: Duration::from_secs(8),
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

/// Pause between paste chunks, giving the vendor's reader and render pass
/// room to consume the previous one. Deliberately short: it is pacing, not
/// synchronization -- correctness comes from the framing and the timeout.
const PASTE_CHUNK_PAUSE: Duration = Duration::from_millis(15);

/// How long the PTY may accept NOT ONE BYTE before a paste is declared
/// stalled.
///
/// This is the primary signal; [`PASTE_CHUNK_WRITE_TIMEOUT`] is only the
/// backstop behind it. Two seconds because a vendor that has accepted
/// nothing at all for two full seconds is meaningfully stuck, while a
/// vendor -- or a loaded host -- that is merely slow keeps the write
/// alive by accepting anything at all.
///
/// Deliberately NOT a [`TuiTimings`] field. It is a failure bound, and
/// making a failure bound configurable and then accelerating it for a
/// whole test suite is exactly how false failures get manufactured: an
/// accelerated 500ms paste bound failed the bracketed-paste test 100% of
/// the time under CPU load. The one test that wants to trip this pays the
/// two seconds.
const PASTE_STALL_WINDOW: Duration = Duration::from_secs(2);

/// The absolute backstop behind [`PASTE_STALL_WINDOW`]: how long one
/// chunk may take even while the vendor keeps accepting bytes. Only a
/// vendor that dribbles -- accepting something in every stall window but
/// never finishing -- ever reaches it.
///
/// **A backstop must be much larger than the primary signal, or it IS the
/// primary signal.** This bound nearly shipped left at the 10s it had
/// when it *was* the only bound, which would have made the new failure
/// set a strict superset of the old one: every write the flat bound
/// failed, plus every write that paused for two seconds. A progress
/// bound underneath an unchanged ceiling is not a progress bound.
///
/// 90 seconds, matching [`ENTER_IDLE_CAP`] rather than being picked as a
/// round number: both are the same decision -- the point at which crew
/// stops waiting on a vendor regardless of what it appears to be doing --
/// and having one figure for it in this file is worth more than tuning
/// two. It is well above any excursion observed (the old 10s bound was
/// observed to be exceeded under 2x CPU oversubscription; nothing has
/// been seen near 90s) and well below the point where a start reads as
/// hung rather than slow.
const PASTE_CHUNK_WRITE_TIMEOUT: Duration = Duration::from_secs(90);

/// The prompt-injection half of the readiness gate: the text to type once
/// the vendor's stdin is wired, and the bound on each chunk's write.
struct PromptInjection<'a> {
    text: &'a str,
    write_timeout: Duration,
}

/// Why one chunk's write did not complete.
#[derive(Debug)]
enum ChunkWriteError {
    /// The write itself failed (the vendor closed its side, an io error).
    Failed(SupervisorError),
    /// Not one byte was accepted for `waited`. That is the window
    /// actually waited, not [`PASTE_STALL_WINDOW`]: the ceiling clamps
    /// the last window, so reporting the constant would overstate the
    /// wait whenever a caller's ceiling is shorter than the window.
    Stalled { accepted: u64, waited: Duration },
    /// Bytes kept being accepted, but the chunk was still unfinished at
    /// [`TuiTimings::paste_write_timeout`]. Distinct from `Stalled`
    /// because the two say opposite things about the vendor.
    Ceiling { accepted: u64 },
    // `accepted` in both variants is bytes accepted DURING THIS PASTE.
    // `PtyProcess::bytes_accepted` is cumulative for the process's
    // lifetime, so a follow-up `send` would otherwise report every byte
    // of the original prompt as progress on this one.
}

/// Writes one chunk, bounding the time the PTY may accept NOTHING rather
/// than the time the whole write may take.
///
/// The decision logic lives in [`bound_on_progress`]; this supplies the
/// PTY's own write future and byte counter.
///
/// A write that is advancing is never failed for being slow: each expiry
/// of [`PASTE_STALL_WINDOW`] re-reads [`PtyProcess::bytes_accepted`] and
/// keeps waiting if it moved. `paste_write_timeout` remains as an
/// absolute per-chunk backstop, so a vendor accepting one byte per window
/// cannot hold delivery open forever.
///
/// **This cannot tell a starved writer thread from a vendor that has
/// stopped reading, and no wording of the error should claim otherwise.**
/// The counter is incremented by the writer thread, so when that thread
/// is not being scheduled the observable is identical to a vendor that
/// accepts nothing: the counter simply stops moving. Distinguishing them
/// would need a second signal -- a heartbeat the thread bumps each
/// iteration, separating "running but bytes static" from "not running at
/// all" -- and nothing currently acts differently on the answer, so it is
/// deliberately not built. `Stalled`'s message names both causes for that
/// reason; it is not hedging, it is the honest limit of what was
/// observed.
///
/// What the bound DOES buy is that the distinction stops mattering for
/// the failure the bound exists for: a starved thread needs to accept
/// one byte per window to keep the write alive, where the previous flat
/// bound required it to finish a whole 1KB chunk inside 10 seconds. That
/// flat bound was measured failing 3 of 14 runs under 2x CPU
/// oversubscription.
async fn write_chunk(
    pty: &Arc<PtyProcess>,
    chunk: &[u8],
    ceiling: Duration,
    baseline: u64,
) -> Result<(), ChunkWriteError> {
    let write = pty.write_input(chunk);
    let progress = || pty.bytes_accepted();
    bound_on_progress(write, progress, PASTE_STALL_WINDOW, ceiling, baseline).await
}

/// [`write_chunk`]'s decision logic, with the PTY factored out so it can
/// be driven directly.
///
/// `write` is the pending write; `progress` reports cumulative bytes the
/// far side has accepted. Extracted as its own function for the same
/// reason `resolve_property_reference` was: the interesting cases here
/// are timing ones, and a test that has to arrange a real vendor to
/// dribble bytes at a chosen rate is testing the tty buffer as much as
/// the bound. Driven directly, "advancing slowly" and "stopped" are two
/// closures.
async fn bound_on_progress<F, P>(
    write: F,
    progress: P,
    stall_window: Duration,
    ceiling: Duration,
    baseline: u64,
) -> Result<(), ChunkWriteError>
where
    F: Future<Output = Result<(), SupervisorError>>,
    P: Fn() -> u64,
{
    let deadline = tokio::time::Instant::now() + ceiling;
    tokio::pin!(write);
    let mut last_accepted = progress();

    loop {
        // Never wait past the ceiling, so the backstop is exactly as
        // tight as it claims to be.
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(ChunkWriteError::Ceiling {
                accepted: progress().saturating_sub(baseline),
            });
        }
        let window = stall_window.min(deadline - now);

        match tokio::time::timeout(window, &mut write).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(err)) => return Err(ChunkWriteError::Failed(err)),
            Err(_) => {
                let accepted = progress();
                if accepted > last_accepted {
                    // Advancing. Keep waiting -- this is the whole point:
                    // a write that is getting somewhere is never failed
                    // for being slow, only for stopping.
                    last_accepted = accepted;
                    continue;
                }
                return Err(ChunkWriteError::Stalled {
                    accepted: accepted.saturating_sub(baseline),
                    waited: window,
                });
            }
        }
    }
}

/// Writes `text` into the PTY as one bracketed paste, in paced chunks.
///
/// The framing makes every byte of `text` content rather than keystrokes,
/// so a multi-line prompt is no longer submitted line-by-line.
///
/// **The "never a silent fragment" invariant, restated for the chunked
/// write.** It has never meant the write is atomic -- a chunked write can
/// fail after earlier chunks landed, so bytes may already have reached
/// the vendor. It means no fragment is ever *silent*, and that is now
/// satisfied at two distinct points:
///
/// - **A truncating vendor** accepts every byte and then drops some in
///   its own composer, which is invisible at the PTY boundary. Caught by
///   comparing the recorded prompt (see
///   `a_vendor_that_records_only_part_of_the_prompt_fails_the_start`) --
///   untouched by this change, since that write succeeds.
/// - **A write that does not complete** now reports how many bytes of
///   this paste the vendor accepted before it stopped, so a partial
///   delivery is stated rather than merely failed. That count is what
///   the explicit write loop in `PtyProcess` exists to make knowable.
///
/// The submit byte is never part of this: callers deliver the vendor's own
/// submit convention separately, once the TUI is idle.
async fn write_paste(
    pty: &Arc<PtyProcess>,
    kind: &str,
    op: &'static str,
    text: &str,
    write_timeout: Duration,
) -> Result<(), AdapterError> {
    let chunks = paste_chunks(text, PASTE_CHUNK_BYTES);
    let total = chunks.len();
    // Progress is reported relative to this paste, not to the process.
    let baseline = pty.bytes_accepted();
    for (index, chunk) in chunks.into_iter().enumerate() {
        match write_chunk(pty, &chunk, write_timeout, baseline).await {
            Ok(()) => {}
            Err(ChunkWriteError::Failed(err)) => {
                return Err(AdapterError::process(kind, op, err.to_string()));
            }
            Err(ChunkWriteError::Stalled { accepted, waited }) => {
                // Report what was OBSERVED, not a diagnosis of it. What
                // elapsed is our own write's acknowledgement: `write_input`
                // queues the chunk to a separate writer thread and awaits a
                // oneshot ack, so this budget spans channel queueing, that
                // thread being scheduled, the blocking write, and the ack's
                // return -- and a vendor that has stopped reading is only
                // ONE of the things that produces it. The previous wording
                // ("the vendor stopped consuming input") asserted that one
                // cause, which sent a reader to the vendor when the machine
                // being saturated produces the same timeout.
                return Err(AdapterError::process(
                    kind,
                    op,
                    format!(
                        "the vendor accepted no input for {waited:?} while chunk {} \
                         of {total} of a {} byte prompt was being written -- {accepted} bytes of \
                         the prompt had been accepted, so it was not delivered. Either the \
                         vendor has stopped reading its stdin, or this host is too loaded to run \
                         the writer thread (this bound covers our own writer thread and channel, \
                         not the vendor's read alone)",
                        index + 1,
                        text.len(),
                    ),
                ));
            }
            Err(ChunkWriteError::Ceiling { accepted }) => {
                return Err(AdapterError::process(
                    kind,
                    op,
                    format!(
                        "chunk {} of {total} of a {} byte prompt was still being written after \
                         {write_timeout:?} -- {accepted} bytes of the prompt had been accepted, \
                         advancing the whole time but never finishing, so it was not delivered",
                        index + 1,
                        text.len(),
                    ),
                ));
            }
        }
        if index + 1 < total {
            tokio::time::sleep(PASTE_CHUNK_PAUSE).await;
        }
    }
    Ok(())
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
/// empty) keeps resume's original shape: the transcript path is derived
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
    /// (`runs.transcript_cursor`), resumed from verbatim. `None`
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
    /// The submitting caller's own `$TERM_PROGRAM` hint, from the run's
    /// resolved `DisplaySelection`, letting the OS-window display backend
    /// target the caller's actual terminal application instead of always
    /// opening Terminal.app. Threaded straight through to
    /// `PaneAttachRequest`; only `OsWindowDisplay` ever reads it.
    launch_program: Option<crew_protocol::HostProgramHint>,
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
        launch_program: Option<crew_protocol::HostProgramHint>,
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
            launch_program,
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
    /// identical path through `start()` (`StartSpec.resume`
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
    #[allow(clippy::too_many_arguments)]
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
        // Deterministic derivation first: the vendor's own layout +
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

        // Fed independently of `readiness_rx`, mirroring `last_output`'s
        // own subscription above: this is the grid `wait_for_readiness`
        // polls, and it must keep accumulating through phase 2's Enter
        // wait too (the re-check there needs the CURRENT surface, not a
        // stale one from readiness time -- codex's gate can paint after
        // its composer, strictly after readiness already concluded
        // `PromptReady`), so its lifetime spans this whole function, not
        // just the call to `wait_for_readiness`.
        let grid = Arc::new(StdMutex::new(TerminalGrid::new()));
        {
            let mut grid_rx = pty.subscribe_output();
            let grid = Arc::clone(&grid);
            tokio::spawn(async move {
                loop {
                    match grid_rx.recv().await {
                        Ok(bytes) => {
                            grid.lock()
                                .expect("terminal-grid mutex never poisoned")
                                .push(&bytes);
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
                launch_program: self.launch_program,
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
        // `compose_input`'s contract is message plus exactly one trailing
        // submit byte; keep only that byte here. The text half is written
        // by `write_paste` from the original string -- framed as a
        // bracketed paste and chunked, so a multi-line prompt is content
        // rather than a sequence of Enters.
        let injected_bytes: Option<Vec<u8>> = inject.as_ref().map(|text| {
            let bytes = self.vendor.compose_input(text);
            debug_assert_eq!(
                &bytes[..bytes.len() - 1],
                text.as_bytes(),
                "{}: compose_input must pass the message through verbatim plus one submit \
                 byte -- the adapter delivers the text itself (see the trait doc)",
                self.kind()
            );
            bytes
        });
        let enter_byte: Option<&[u8]> = injected_bytes
            .as_deref()
            .map(|bytes| &bytes[bytes.len() - 1..]);
        let type_text: Option<PromptInjection<'_>> =
            inject.as_deref().map(|text| PromptInjection {
                text,
                write_timeout: self.timings.paste_write_timeout,
            });
        if let Err(err) = wait_for_readiness(
            &mut readiness_rx,
            self.kind(),
            self.vendor.as_ref(),
            &grid,
            self.timings.readiness_quiet,
            self.timings.readiness_cap,
            &pty,
            type_text,
            spawn_instant + INJECT_MIN_DELAY,
            run_id,
            task_id,
            worker_id,
            &sink,
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
            // The Enter precondition: re-classify in this SAME idle
            // window (`wait_for_output_idle` above waits up to
            // `submit_idle`/`ENTER_IDLE_CAP`, ample time for a late gate
            // to appear), never reuse the readiness-time classification --
            // see `enter_precondition`'s own doc comment for why.
            let precondition = {
                let g = grid.lock().expect("terminal-grid mutex never poisoned");
                enter_precondition(self.vendor.as_ref(), &g)
            };
            match precondition {
                EnterPrecondition::Proceed => {
                    // A write failure here means the worker already exited;
                    // the exit watcher owns reporting that -- nothing
                    // useful to add.
                    let _ = pty.write_input(enter).await;
                }
                EnterPrecondition::Blocked(surface) => {
                    // Withholding the Enter must fail the run, not just
                    // skip a step: the prompt is already pasted, so the
                    // run cannot progress either way, and a run that
                    // silently withholds and then waits out
                    // `discovery_timeout` reads as an unrelated hang, not
                    // as this decision. Same shape as the readiness-path
                    // failure above, naming the actual variant seen.
                    let detail = match surface {
                        Surface::Gate(gate) => format!(
                            "a first-run gate appeared after the prompt was pasted ({gate:?}) -- \
                             withholding the submit byte rather than confirming into it; answer \
                             it by hand once in this workspace outside crew, then retry"
                        ),
                        Surface::Undecided => "the surface became unreadable after the prompt \
                                                was pasted -- withholding the submit byte rather \
                                                than confirming into an unrecognized screen"
                            .to_string(),
                        Surface::PromptReady => {
                            unreachable!("EnterPrecondition::Proceed handles PromptReady")
                        }
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

        // Confirm the vendor recorded the WHOLE prompt, not just
        // the tail. Discovery only proves the nonce arrived, and the nonce
        // is appended -- so a vendor that accepted every byte and then
        // truncated in its own composer passes discovery and looks like a
        // success. Only compared for a fresh injection: a resume has no
        // prompt of its own to verify, and its transcript's prior turns
        // belong to earlier runs.
        if let Some(injected) = inject.as_deref()
            && let Ok(raw) = tokio::fs::read(&transcript_path).await
        {
            match verify_recorded_prompt(
                &raw,
                self.vendor.format().as_ref(),
                injected,
                &discovery_key,
            ) {
                PromptVerdict::Intact | PromptVerdict::Unverifiable => {}
                PromptVerdict::Corrupted {
                    expected_len,
                    recorded_len,
                    detail,
                } => {
                    return self
                        .fail_start(
                            pty,
                            attach,
                            pane_outcome,
                            sink,
                            run_id,
                            task_id,
                            worker_id,
                            AdapterError::process(
                                self.kind(),
                                "start",
                                format!(
                                    "the vendor recorded {recorded_len} characters of a \
                                     {expected_len}-character prompt: {detail}"
                                ),
                            ),
                        )
                        .await;
                }
            }
        }

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
    ///
    /// The `ProcessExited` this emits is never bare. `err` is
    /// the actual reason the start failed (discovery timeout, a
    /// truncation failure, ...) and, before terminate()'s own exit
    /// status is journaled, this records it as its own durable
    /// `ProtocolHealthChanged{healthy: false}` diagnostic -- the same
    /// event `resume_from` already uses for a healthy diagnostic. Before
    /// this, the only place `err` was visible was the RPC response to
    /// the leader, which is never journaled: a replay of the run showed
    /// only a clean exit, indistinguishable from one that did real work.
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
        emit(
            &sink,
            run_id,
            task_id,
            worker_id,
            AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: Classified {
                    class: ContentClass::Visible,
                    value: format!("start failed: {err}"),
                },
            },
            None,
        )
        .await;
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
        TuiEvent::TurnEnded { outcome } => {
            emit(
                sink,
                run_id,
                task_id,
                worker_id,
                AdapterEventPayload::TurnEnded { outcome },
                cursor,
            )
            .await;
        }
        TuiEvent::Raw { entry_type } => {
            tracing::debug!(entry_type, run_id = %run_id, "tui transcript: unrecognized entry");
        }
        // The sink's own side channel, never `emit` -- see
        // `AdapterEventSink::note_real_user_turn`'s doc comment for why
        // this must not become journaled content.
        TuiEvent::UserTurnStarted => {
            if let Err(err) = sink.note_real_user_turn(run_id).await {
                tracing::warn!(error = %err, run_id = %run_id, "tui adapter failed to signal a real user turn");
            }
        }
    }
}

/// The outcome of re-checking, at Enter time, whether the submit byte
/// should still be delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnterPrecondition {
    /// Deliver the Enter: `PromptReady`, or a vendor with no real
    /// predicate (`classify_surface` returned `None`) keeping its
    /// unconditional legacy behavior.
    Proceed,
    /// Withhold it. Carries the classification that caused this, so the
    /// caller can name the actual reason rather than guessing at it --
    /// `Gate` and `Undecided` are different facts about the world and
    /// read differently to whoever debugs the failure.
    Blocked(Surface),
}

/// Classifies `grid`'s CURRENT state through `vendor` to decide whether
/// the Enter byte should still be delivered -- never the classification
/// `wait_for_readiness` reached earlier. That re-check is required, not
/// belt-and-braces: a vendor whose first-run gate paints strictly after
/// its own composer (observed live on codex) can classify as
/// `PromptReady` at readiness time and `Gate` by the time the submit
/// byte is due, since the caller waits for output to go quiet in
/// between -- exactly the window a late gate has to appear in.
///
/// A withheld Enter must fail the run, not merely skip a step: the
/// prompt has already been pasted, so the run cannot progress either
/// way, and a typed failure naming what was seen is diagnosable in one
/// line where a silent skip looks like an unrelated hang until whoever
/// is debugging it reads the wrong subsystem first.
fn enter_precondition(vendor: &dyn TuiVendor, grid: &TerminalGrid) -> EnterPrecondition {
    match vendor.classify_surface(grid) {
        None | Some(Surface::PromptReady) => EnterPrecondition::Proceed,
        Some(other) => EnterPrecondition::Blocked(other),
    }
}

/// Waits for the PTY's first output, then polls `vendor.classify_surface`
/// on `grid` (which a caller-owned background task keeps fed from the
/// same broadcast stream `rx` subscribes to) until the surface is
/// decided, bounded overall by `cap`. `rx` must already be subscribed
/// *before* this is called -- see the caller's own comment on why it is
/// captured immediately after spawn rather than here.
///
/// A vendor with no real predicate yet (`classify_surface` returns
/// `None`) keeps the exact legacy behavior: first output means
/// ready, paste immediately, no gate can be recognized for it. A vendor
/// with a real predicate (claude, codex) instead:
///
/// - `PromptReady` -- proceed to paste, same as the legacy path.
/// - `Gate(kind)` -- **never paste, never Enter.** A typed start failure
///   naming the gate, honest about what crew saw rather than silently
///   treating a blocked dialog as ready. (The park-and-escalate behavior
///   this eventually becomes is later work; this slice's job is only to
///   stop writing into a gate, not to resume past one.)
/// - `Undecided` -- keep polling until `cap`, then a typed start failure
///   naming that nothing was ever positively identified. Fail closed:
///   a surface this module cannot read is exactly the failure mode this
///   whole design exists to prevent, not a case to guess through.
#[allow(clippy::too_many_arguments)]
async fn wait_for_readiness(
    rx: &mut broadcast::Receiver<Vec<u8>>,
    kind: &str,
    vendor: &dyn TuiVendor,
    grid: &StdMutex<TerminalGrid>,
    quiet: Duration,
    cap: Duration,
    pty: &Arc<PtyProcess>,
    inject: Option<PromptInjection<'_>>,
    not_before: tokio::time::Instant,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    sink: &Arc<dyn AdapterEventSink>,
) -> Result<(), AdapterError> {
    let deadline = tokio::time::Instant::now() + cap;
    // Set once this poll journals `FirstRunGateDetected` +
    // `EscalationRaised` for the gate currently blocking the run --
    // exactly once per distinct gate, even though the loop below keeps
    // re-classifying the same `Gate(kind)` on every tick until it
    // resolves. A vendor that swaps from one gate to another (observed
    // nowhere yet, but not ruled out) re-escalates for the new one.
    let mut escalated_gate: Option<GateKind> = None;

    // Wait for the first output (a single check, not a loop: every
    // outcome below either proceeds past this point or returns). This
    // only proves the vendor's stdin is wired at all -- classify_surface
    // has nothing to read before this, since the grid is still empty.
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
    // Hold until the spawn-anchored floor before reading the surface or
    // typing anything: a vendor mid-launch has not painted its real
    // screen yet, and INJECT_MIN_DELAY exists precisely so text is not
    // typed into that window either.
    if tokio::time::Instant::now() < not_before {
        tokio::time::sleep_until(not_before).await;
    }

    loop {
        let classified = {
            let g = grid.lock().expect("terminal-grid mutex never poisoned");
            vendor.classify_surface(&g)
        };
        match classified {
            None | Some(Surface::PromptReady) => break,
            Some(Surface::Gate(gate)) => {
                // A recognized first-run gate does not fail the run: it
                // parks it and hands the human the decision, journaling
                // exactly once per distinct gate even though this arm
                // re-fires on every tick the grid still reads as the same
                // `Gate(gate)`.
                if escalated_gate != Some(gate) {
                    emit(
                        sink,
                        run_id,
                        task_id,
                        worker_id,
                        AdapterEventPayload::FirstRunGateDetected {
                            kind: protocol_gate_kind(gate),
                        },
                        None,
                    )
                    .await;
                    escalated_gate = Some(gate);
                }
                // Parked on no deadline of its own -- neither `cap` nor
                // any other timeout -- until the surface changes (the next
                // loop iteration's classify above), the process exits, or
                // the run is cancelled. A human answering a first-run gate
                // by hand has no bound this adapter may impose; see the
                // approval service for the same "no Duration at all"
                // judgement made for the same reason.
                tokio::select! {
                    biased;
                    _ = pty.exit_watcher() => {
                        let classified = {
                            let g = grid.lock().expect("terminal-grid mutex never poisoned");
                            vendor.classify_surface(&g)
                        };
                        return Err(match classified {
                            Some(Surface::Gate(gate)) => AdapterError::process(
                                kind,
                                "start",
                                format!(
                                    "the worker process exited while a first-run gate was \
                                     blocking the run ({gate:?})"
                                ),
                            ),
                            _ => AdapterError::process(
                                kind,
                                "start",
                                "the worker process exited before a recognizable surface \
                                 appeared",
                            ),
                        });
                    }
                    _ = tokio::time::timeout(quiet, rx.recv()) => {}
                }
                continue;
            }
            Some(Surface::Undecided) => {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(AdapterError::process(
                        kind,
                        "start",
                        "no recognizable prompt or first-run gate appeared before the readiness \
                         cap elapsed",
                    ));
                }
                // Raced against `pty.exit_watcher()`, not detected via
                // the output channel closing: a broadcast `Receiver`
                // only reports `Closed` once every `Sender` is dropped,
                // which is tied to the PTY's own reader task noticing
                // EOF/EIO -- verified directly (a double that exits
                // clean after printing output left its channel open well
                // past several seconds, with no explicit `terminate()`
                // in between) that this does not happen promptly, or
                // perhaps ever, from a bare process exit on its own.
                // `exit_watcher()` is this codebase's own authoritative
                // signal (backed by the reaper thread's real `wait()`),
                // already used everywhere else a caller needs to know a
                // vendor process died -- resolving immediately if the
                // process had already exited before this call, and
                // whenever the reaper next observes it otherwise.
                tokio::select! {
                    biased;
                    _ = pty.exit_watcher() => {
                        let classified = {
                            let g = grid.lock().expect("terminal-grid mutex never poisoned");
                            vendor.classify_surface(&g)
                        };
                        return Err(match classified {
                            Some(Surface::Gate(gate)) => AdapterError::process(
                                kind,
                                "start",
                                format!(
                                    "the worker process exited while a first-run gate was \
                                     blocking the run ({gate:?})"
                                ),
                            ),
                            _ => AdapterError::process(
                                kind,
                                "start",
                                "the worker process exited before a recognizable surface \
                                 appeared",
                            ),
                        });
                    }
                    // A per-tick receive timeout, or any `rx` outcome
                    // other than a still-hypothetical channel close, is a
                    // pacing wake-up, not itself a failure: it means no
                    // NEW bytes arrived in this window, not that the
                    // grid is empty or will stay Undecided forever (a
                    // vendor's whole first paint can arrive as one
                    // burst, entirely consumed by the earlier "wait for
                    // first output" check above, with nothing further
                    // ever coming until the process eventually exits or
                    // writes again). Either way, loop back and re-check
                    // classify_surface on the grid a background task
                    // keeps feeding independently of this `rx` --
                    // `remaining.is_zero()` above is the only real
                    // deadline enforcement here, alongside
                    // `exit_watcher()` above as the real exit signal.
                    _ = tokio::time::timeout(quiet.min(remaining), rx.recv()) => {}
                }
                continue;
            }
        }
    }

    // Deliberately WITHOUT the submit byte: a CR sent here can be
    // swallowed by the render loop mid-layout. The Enter itself is
    // delivered by the caller once the PTY has gone quiet (see the
    // phase-2 block in `run_pipeline`) -- an idle TUI processes it
    // exactly like a human's keystroke, with no timing assumption about
    // startup speed at all.
    if let Some(injection) = inject
        && let Err(err) =
            write_paste(pty, kind, "start", injection.text, injection.write_timeout).await
    {
        return Err(AdapterError::process(
            kind,
            "start",
            format!("initial prompt injection failed: {err}"),
        ));
    }

    // Let the paste settle. This loop's shape predates classify_surface
    // and is kept as a courtesy pause after the write -- the Enter-time
    // re-check in `run_pipeline`'s phase 2 is what actually gates the
    // submit byte now, not this.
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

/// Maps this module's internal [`GateKind`] to the durable protocol enum
/// journaled on [`crew_protocol::RuntimeEvent::FirstRunGateDetected`].
/// Kept as an explicit `match` (not a `From` impl) so a new `GateKind`
/// variant fails this file to compile rather than silently falling
/// through -- the protocol enum is mirrored 1:1 by hand, on purpose.
fn protocol_gate_kind(gate: GateKind) -> crew_protocol::FirstRunGateKind {
    match gate {
        GateKind::ClaudeWorkspaceTrust => crew_protocol::FirstRunGateKind::ClaudeWorkspaceTrust,
        GateKind::ClaudeThemePicker => crew_protocol::FirstRunGateKind::ClaudeThemePicker,
        GateKind::ClaudeSignIn => crew_protocol::FirstRunGateKind::ClaudeSignIn,
        GateKind::CodexDirectoryTrust => crew_protocol::FirstRunGateKind::CodexDirectoryTrust,
        GateKind::CodexSignIn => crew_protocol::FirstRunGateKind::CodexSignIn,
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

/// Builds the prompt actually delivered to the vendor for a fresh start:
/// the caller's prompt plus a tracking tag carrying `nonce`, appended
/// exactly as before.
///
/// The tag used to be a bare `[crew:<nonce>]` suffix with
/// nothing anywhere saying what it was, and a live E2E worker refused an
/// otherwise ordinary task citing it as an injection attempt
/// (`release/live-conformance/2026-09-08-live-e2e-attempt-3.md`, F14) --
/// a correct reaction to an unexplained bracketed token at the end of a
/// prompt. That review also names F13/F14 as being in tension: relocating
/// the tag away from the prompt satisfies discovery, but
/// [`super::verify::verify_recorded_prompt`] diffs the *whole*
/// injected string against what the vendor recorded to catch silent
/// truncation, so moving the tag off the tail it currently occupies
/// would blind that check to exactly the truncation shape it exists to
/// catch. Kept here is the cheaper fix the review recommended instead:
/// the tag stays in place and in shape, but now describes itself, so a
/// safety-trained model reads it as an inert marker rather than a
/// directive to interpret.
///
/// **Wording, reviewed by staff (2026-09-09).** Purely descriptive, no
/// imperative: an early draft ended "...not an instruction; disregard
/// it", and "disregard it" is itself an instruction -- telling a
/// safety-trained model to disregard an opaque marker appended after the
/// user's task is the exact shape this self-describing wording exists to
/// stop triggering. A plainly labelled bookkeeping id needs no
/// instruction at all: there is nothing to obey, so there is nothing to
/// refuse.
///
/// **ASCII-only, deliberately.** Two reasons, not the one first
/// considered (the chunker cannot split a multi-byte scalar across
/// a paste chunk -- see `a_multibyte_scalar_is_never_split_across_chunks`
/// -- so that specific risk does not exist and is not why this matters):
/// (1) some terminals/vendor transcripts normalize or re-encode
/// typographic characters; discovery only greps the raw nonce and
/// survives that, but `verify_recorded_prompt`'s recorded-vs-expected
/// comparison is exact, so a normalized character here would report
/// "does not match" on a prompt that in fact arrived intact -- a false
/// corruption report on the very string proving prompt integrity; (2)
/// this string crosses PTY write, terminal, vendor input handling,
/// transcript serialization, and our own comparison -- only our chunker
/// is proven scalar-safe, the rest are not ours to guarantee, and ASCII
/// removes the whole class for free.
///
/// **The separator is a single space, not the blank line first tried
/// here.** A blank-line separator makes even a single-line prompt span
/// three physical lines once delivered; the four TUI conformance doubles
/// read PTY input with a shell `while IFS= read -r line` loop and only
/// recognize the injected prompt by matching `[crew:` on that one line,
/// so splitting the tag onto its own line made the double see the
/// prompt's own line as an ordinary (non-injection) line instead --
/// corrupting the recorded event sequence for every canonical scenario.
/// Staff's review explicitly allowed this fallback ("a single space is
/// acceptable, your call"). The tag still ends up at the tail of
/// whatever the prompt's own last line is, so
/// `verify_recorded_prompt`'s head-truncation check is unaffected either
/// way.
///
/// [`find_transcript_by_nonce`] and `verify_recorded_prompt` are both
/// still keyed on `nonce` itself (searched/compared as a raw substring,
/// never on the tag's exact wording), so the extra words here change
/// nothing for either consumer -- see the test below asserting that
/// parity directly. `nonce` is a freshly generated UUIDv7 (122 bits of
/// randomness beyond its millisecond timestamp component, see
/// `Uuid::now_v7` at the call site), so a first-match lookup over it is
/// not the "first-match over attacker-influenced content" bug family it
/// might otherwise look like: nothing (a concurrent run, a prior
/// transcript entry, the user's own prompt text) can contain this exact
/// value unless it was generated for, and by, this call -- there is
/// nothing to guess in advance, and nothing to collide with after the
/// fact.
///
/// The literal substring `[crew:` is preserved deliberately: the TUI
/// conformance doubles (`claude_conformance.rs`, `omp_conformance.rs`,
/// `codex_conformance.rs`, `copilot_conformance.rs`) detect the injected
/// line with a `case *"[crew:"*` shell glob, and changing that substring
/// would silently stop matching there.
fn compose_injected_prompt(prompt: &str, nonce: &str) -> String {
    format!(
        "{prompt} [crew:{nonce} run-correlation id; orchestrator bookkeeping, not task content]"
    )
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
            // start wearing a flag -- it *is* a resume: the
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
            self.vendor
                .preflight(&spec, &self.cfg, &self.timings)
                .await?;
            let launch = self.vendor.launch(&spec, &self.cfg);
            let transcript_root = self.vendor.transcript_root(&spec, &self.cfg);
            let nonce = Uuid::now_v7().to_string();
            let injected = compose_injected_prompt(&spec.prompt, &nonce);
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
            // leader is REDIRECTING work, not queueing more of it.
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
            debug_assert_eq!(
                &bytes[..bytes.len() - 1],
                text.as_bytes(),
                "{}: compose_input must pass the message through verbatim plus one submit \
                 byte -- the adapter delivers the text itself (see the trait doc)",
                self.kind()
            );
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
            // Only the vendor's submit byte comes from `compose_input`; the
            // text is framed and chunked by `write_paste`, so a multi-line
            // follow-up cannot be submitted line-by-line.
            let split_at = bytes.len() - 1;
            write_paste(
                &run.pty,
                self.kind(),
                "send",
                text,
                self.timings.paste_write_timeout,
            )
            .await?;
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
                // smuggled out here: every adapter
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

    use super::super::classify::GateKind;
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    /// Polls `condition` until it is true or `timeout` elapses, returning
    /// which. Mirrors `tests/tui_adapter.rs`'s own `wait_until` helper
    /// (duplicated here -- that file is a separate integration-test
    /// binary, not reachable from this unit-test module).
    async fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    /// A minimal [`AdapterEventSink`] that just records every payload it
    /// is given, for the `wait_for_readiness` unit tests below that need
    /// to see what the gate-escalation path journaled without a real
    /// database (that end-to-end proof, through `DomainAdapterEventSink`
    /// into a real `EscalationRaised` row with its `question`, is
    /// `event_sink.rs`'s own `first_run_gate_tests` module).
    struct GateRecordingSink(StdMutex<Vec<AdapterEventPayload>>);

    impl GateRecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self(StdMutex::new(Vec::new())))
        }

        fn payloads(&self) -> Vec<AdapterEventPayload> {
            self.0
                .lock()
                .expect("recording sink mutex never poisoned")
                .clone()
        }
    }

    impl AdapterEventSink for GateRecordingSink {
        fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
            self.0
                .lock()
                .expect("recording sink mutex never poisoned")
                .push(event.payload);
            Box::pin(async { Ok(0) })
        }

        fn note_real_user_turn(&self, _run_id: RunId) -> AdapterFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A test-only vendor whose `classify_surface` deterministically
    /// reports a fixed [`GateKind`] for its first `clear_after` calls,
    /// then `Surface::PromptReady` forever after -- lets the gate-park
    /// loop be tested on TICK COUNTS rather than real elapsed time or
    /// fixture-replay timing. A whole captured screen (e.g.
    /// `claude-trust-to-composer.raw`) typically arrives on the PTY as a
    /// single read, so replaying a fixture that transitions gate ->
    /// composer cannot reliably exercise more than one tick of this poll;
    /// this double can.
    ///
    /// `wait_for_readiness` calls no `TuiVendor` method but
    /// `classify_surface`, so every other trait method here is
    /// unreachable by construction.
    struct CountingGateVendor {
        gate: GateKind,
        clear_after: u32,
        calls: AtomicU32,
    }

    impl TuiVendor for CountingGateVendor {
        fn kind(&self) -> &'static str {
            "counting-gate"
        }
        fn launch(&self, _spec: &StartSpec, _cfg: &AdapterConfig) -> LaunchSpec {
            unreachable!("wait_for_readiness never calls TuiVendor::launch")
        }
        fn resume_launch(
            &self,
            _session: &VendorSessionRef,
            _spec: &StartSpec,
            _cfg: &AdapterConfig,
        ) -> LaunchSpec {
            unreachable!("wait_for_readiness never calls TuiVendor::resume_launch")
        }
        fn transcript_root(&self, _spec: &StartSpec, _cfg: &AdapterConfig) -> PathBuf {
            unreachable!("wait_for_readiness never calls TuiVendor::transcript_root")
        }
        fn format(&self) -> Arc<dyn TranscriptFormat> {
            unreachable!("wait_for_readiness never calls TuiVendor::format")
        }
        fn compose_input(&self, _message: &str) -> Vec<u8> {
            unreachable!("wait_for_readiness never calls TuiVendor::compose_input")
        }
        fn interrupt_sequence(&self) -> Vec<u8> {
            unreachable!("wait_for_readiness never calls TuiVendor::interrupt_sequence")
        }
        fn permission_args(&self, _mode: crate::config::crew::PermissionMode) -> Vec<String> {
            unreachable!("wait_for_readiness never calls TuiVendor::permission_args")
        }
        fn version_gate(&self, _probed: &str) -> VersionVerdict {
            unreachable!("wait_for_readiness never calls TuiVendor::version_gate")
        }
        fn classify_surface(&self, _grid: &TerminalGrid) -> Option<Surface> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.clear_after {
                Some(Surface::Gate(self.gate))
            } else {
                Some(Surface::PromptReady)
            }
        }
    }

    /// The constant is derived from the bounds it must dominate,
    /// not chosen. If a production bound is ever raised past it, this fails
    /// rather than a scenario becoming quietly flaky.
    ///
    /// **`paste_write_timeout` is deliberately excluded, and the exclusion
    /// is conditional on a property of the call sites rather than of the
    /// bound.** At 90s it is the largest failure bound in the struct, so if
    /// a scenario ever waited behind it this deadline would be inverted.
    /// None does: every harness awaits `adapter.start(spec, sink)`
    /// **unwrapped**, so a stalled paste is consumed inside `start()` and
    /// surfaces as an `Err` carrying the paste bound's byte-count message
    /// — never as an expired observation deadline. The only call any harness wraps
    /// in a deadline is `adapter.cancel(..)`, which sits behind the
    /// escalation total, and that total *is* asserted below.
    ///
    /// **What would falsify this:** wrapping `start()` or `send()` in
    /// `SCENARIO_OBSERVATION_DEADLINE` — a natural-looking change, to stop
    /// a hung start from hanging the suite. Do that and the inversion is
    /// immediate (20s against 90s), and the right fix is then to raise this
    /// deadline above the paste bound and add it to the list below, never
    /// to shorten the paste bound.
    #[test]
    fn the_observation_deadline_dominates_every_bound_a_scenario_waits_behind() {
        let production = TuiTimings::default();
        let escalation_total =
            production.escalation.sigint_to_sigterm + production.escalation.sigterm_to_sigkill;

        for (name, bound) in [
            ("readiness_cap", production.readiness_cap),
            ("discovery_timeout", production.discovery_timeout),
            ("preflight_timeout", production.preflight_timeout),
            (
                "the escalation total (sigint->sigterm->sigkill)",
                escalation_total,
            ),
        ] {
            assert!(
                SCENARIO_OBSERVATION_DEADLINE > bound,
                "a scenario waits {SCENARIO_OBSERVATION_DEADLINE:?} to observe an event, but \
                 {name} permits the adapter {bound:?} -- so the scenario would fail before the \
                 behaviour it is testing does. Raise SCENARIO_OBSERVATION_DEADLINE above it."
            );
        }
    }

    // ----------------------------------------- the paste progress bound

    /// The invariant this progress bound nearly shipped without, and the
    /// reason this test exists rather than a comment: the ceiling is a
    /// BACKSTOP, and a backstop must be much larger than the primary
    /// signal or it IS the primary signal.
    ///
    /// The first version of this change left the ceiling at the 10s it had
    /// when it was the only bound. That made the new failure set a strict
    /// superset of the old one -- every write the flat bound failed, plus
    /// every write that paused for two seconds -- so it would have made
    /// the accelerated-timeout paste failure it was written to fix
    /// strictly more likely (see `paste_write_timeout`'s own doc comment
    /// for that failure). The arithmetic was caught in review; this keeps
    /// it caught.
    #[test]
    fn the_ceiling_is_a_backstop_not_the_primary_bound() {
        assert!(
            PASTE_CHUNK_WRITE_TIMEOUT >= PASTE_STALL_WINDOW * 10,
            "the ceiling ({PASTE_CHUNK_WRITE_TIMEOUT:?}) must be far above the stall window \
             ({PASTE_STALL_WINDOW:?}), or a write that is advancing still fails on the clock \
             and the progress bound buys nothing"
        );
    }

    /// A write that never finishes on its own, so each test below is
    /// decided by the bound rather than by the write completing.
    async fn never_completes() -> Result<(), SupervisorError> {
        std::future::pending().await
    }

    /// Scaled-down stand-ins for the real constants. The bound is
    /// parameterised precisely so its LOGIC can be tested in
    /// milliseconds; the real values' relationship is asserted above.
    const TEST_WINDOW: Duration = Duration::from_millis(100);
    const TEST_CEILING: Duration = Duration::from_millis(900);

    /// THE BEHAVIOUR CHANGE. A write that is advancing must not be failed
    /// for being slow -- only for stopping. It ticks well inside every
    /// window and never completes, so if the bound were on total duration
    /// (as it was) this would fail early; on progress it survives to the
    /// backstop.
    #[tokio::test]
    async fn a_write_that_advances_slowly_is_never_failed_for_being_slow() {
        let accepted = Arc::new(AtomicU64::new(0));
        let ticker = Arc::clone(&accepted);
        let pump = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(30)).await;
                ticker.fetch_add(1, Ordering::Relaxed);
            }
        });

        let reader = Arc::clone(&accepted);
        let outcome = bound_on_progress(
            never_completes(),
            move || reader.load(Ordering::Relaxed),
            TEST_WINDOW,
            TEST_CEILING,
            0,
        )
        .await;
        pump.abort();

        match outcome {
            Err(ChunkWriteError::Ceiling { accepted }) => {
                assert!(
                    accepted > 0,
                    "the ceiling report must show the progress that did happen"
                );
            }
            other => panic!(
                "a steadily-advancing write must survive to the ceiling, never stall: {other:?}"
            ),
        }
    }

    /// The other half of the trade: a write accepting nothing fails at the
    /// stall window, not at the backstop -- sooner than the bound it
    /// replaced, not later.
    #[tokio::test]
    async fn a_write_that_accepts_nothing_stalls_at_the_window_not_the_ceiling() {
        let started = std::time::Instant::now();
        let outcome =
            bound_on_progress(never_completes(), || 0, TEST_WINDOW, TEST_CEILING, 0).await;
        let waited = started.elapsed();

        match outcome {
            Err(ChunkWriteError::Stalled { accepted, waited }) => {
                assert_eq!(accepted, 0);
                assert_eq!(
                    waited, TEST_WINDOW,
                    "the reported wait must be the real one"
                );
            }
            other => panic!("a write accepting nothing must stall: {other:?}"),
        }
        assert!(
            waited < TEST_CEILING,
            "must fail at the window, not the backstop: waited {waited:?}"
        );
    }

    /// A caller whose ceiling is shorter than the stall window still gets
    /// an honest report of how long was actually waited. An earlier
    /// version interpolated the constant here, so it claimed "2s" after
    /// waiting 500ms -- a false claim in an error message, in the change
    /// about false claims in error messages.
    #[tokio::test]
    async fn a_ceiling_shorter_than_the_stall_window_reports_the_clamped_wait() {
        let clamped = Duration::from_millis(40);
        let outcome = bound_on_progress(never_completes(), || 0, TEST_WINDOW, clamped, 0).await;

        match outcome {
            // Not an exact equality: the clamp is computed as
            // `deadline - now`, so it lands a few microseconds under the
            // nominal figure. What matters is that it reports the clamp
            // rather than the constant, which is a 2.5x difference here.
            Err(ChunkWriteError::Stalled { waited, .. }) => {
                assert!(
                    waited <= clamped && waited * 2 > clamped,
                    "the reported wait must be the clamped window ({clamped:?}), got {waited:?}"
                );
                assert!(
                    waited < TEST_WINDOW,
                    "reporting the unclamped constant ({TEST_WINDOW:?}) would overstate the \
                     wait, which is the defect this pins: got {waited:?}"
                );
            }
            other => panic!("expected a stall inside the clamped window: {other:?}"),
        }
    }

    /// Progress is relative to THIS paste. `bytes_accepted` is cumulative
    /// for the process, so without a baseline a follow-up `send` would
    /// report every byte of the original prompt as progress on the new
    /// one.
    #[tokio::test]
    async fn reported_progress_is_relative_to_this_paste_not_the_process() {
        let outcome = bound_on_progress(
            never_completes(),
            || 500_000,
            TEST_WINDOW,
            TEST_CEILING,
            500_000,
        )
        .await;

        match outcome {
            Err(ChunkWriteError::Stalled { accepted, .. }) => assert_eq!(
                accepted, 0,
                "half a megabyte written by an EARLIER paste is not progress on this one"
            ),
            other => panic!("expected a stall: {other:?}"),
        }
    }

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

    /// A turn boundary is journaled evidence (ADR-0027), so it emits --
    /// and being the line's last emitting event, it is the one that
    /// carries the cursor. That is what stops a resume from re-delivering
    /// the boundary and settling the same turn twice. (The original
    /// regression this replaced -- a trailing NON-emitting event stealing
    /// the cursor -- is still guarded by the `Raw` test below.)
    #[test]
    fn a_trailing_turn_ended_carries_the_cursor_itself() {
        let cursor = Cursor {
            offset: 10,
            last_entry_id: Some("e1".to_string()),
        };
        let placements = cursor_placements(tagged(
            vec![
                text("first"),
                TuiEvent::TurnEnded {
                    outcome: crew_protocol::TurnOutcome::Normal,
                },
            ],
            cursor.clone(),
        ));

        assert_eq!(placements.len(), 2);
        assert_eq!(
            placements[0].1, None,
            "the AssistantText shares its line with a later emitting event"
        );
        assert_eq!(
            placements[1].1,
            Some(cursor),
            "the turn boundary is the line's last emitting event and carries the cursor"
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
                TuiEvent::Raw {
                    entry_type: "unknown".to_string(),
                },
                TuiEvent::Raw {
                    entry_type: "also-unknown".to_string(),
                },
            ],
            Cursor::start(),
        ));

        assert!(placements.iter().all(|(_, cursor)| cursor.is_none()));
    }

    // ------------------------------------------ the self-describing tag

    /// The wording change must not detach the tag from what discovery and
    /// verification actually key on: both search for `nonce` as a raw
    /// substring of the recorded/delivered text, never for the tag's
    /// surrounding words. A future edit to the phrasing that accidentally
    /// dropped the nonce itself (a typo in the format string, say) would
    /// pass every other test in this file yet silently break discovery in
    /// production -- this pins the parity directly.
    #[test]
    fn the_composed_prompt_carries_the_exact_nonce_discovery_and_verification_key_on() {
        let nonce = "0198a1b2-fake-nonce-not-a-real-uuid";
        let injected = compose_injected_prompt("do the thing", nonce);
        assert!(
            injected.contains(nonce),
            "the composed prompt must carry the literal nonce discovery/verify search for: \
             {injected}"
        );
    }

    /// The conformance doubles (`claude_conformance.rs` and friends) find
    /// the injected line with a shell `case *"[crew:"*` glob. Self-describing
    /// the tag must not lose that exact substring, or those doubles stop
    /// recognizing the injected prompt and every conformance test using
    /// them goes silently inert rather than failing loudly.
    #[test]
    fn the_composed_prompt_still_carries_the_bracket_prefix_the_conformance_doubles_match_on() {
        let injected = compose_injected_prompt("do the thing", "n1");
        assert!(
            injected.contains("[crew:"),
            "conformance doubles match `*\"[crew:\"*`; got: {injected}"
        );
    }

    /// End-to-end proof that the wording change is still discoverable by
    /// the real scanner, not just a string-equality assertion against the
    /// composing function in isolation: writes a transcript containing the
    /// composed (self-describing) prompt to a temp `.jsonl` file and drives
    /// it through the actual `find_transcript_by_nonce` used in
    /// production.
    #[tokio::test]
    async fn find_transcript_by_nonce_still_locates_a_transcript_carrying_the_self_describing_tag()
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let nonce = "0198a1b2-fake-nonce-not-a-real-uuid";
        let injected = compose_injected_prompt("do the thing", nonce);
        let transcript = dir.path().join("session.jsonl");
        // `injected` now embeds a literal newline (the blank-line
        // separator); serialize through serde_json rather than
        // hand-splicing it into a string literal, or that newline would
        // land unescaped in the file and not be valid JSON.
        let entry = serde_json::json!({"type": "user", "message": {"content": injected}});
        std::fs::write(&transcript, serde_json::to_vec(&entry).expect("serialize"))
            .expect("write fixture transcript");

        let found = find_transcript_by_nonce(
            dir.path(),
            SystemTime::UNIX_EPOCH,
            nonce,
            Duration::from_secs(5),
        )
        .await
        .expect("the self-describing tag must still be discoverable by its nonce");
        assert_eq!(found, transcript);
    }

    /// Mirrors `verify_recorded_prompt`'s own truncation check with the
    /// new wording: a vendor that recorded the composed prompt
    /// byte-for-byte is intact, proving the extra explanatory words did
    /// not change how the intact case is judged.
    #[test]
    fn an_intact_self_describing_prompt_still_verifies_as_intact() {
        use crate::adapter::tui::{ClaudeTuiVendor, TuiVendor};

        let nonce = "n1";
        let injected = compose_injected_prompt("do the thing", nonce);
        let format = ClaudeTuiVendor::new(PathBuf::from("/w"), vec![]).format();
        let entry = serde_json::json!({
            "type": "user",
            "sessionId": "sess-1",
            "message": {"role": "user", "content": injected},
        });
        let mut transcript = serde_json::to_vec(&entry).expect("serialize");
        transcript.push(b'\n');

        assert_eq!(
            verify_recorded_prompt(&transcript, format.as_ref(), &injected, nonce),
            PromptVerdict::Intact
        );
    }

    // -------------------------------------------- the Enter precondition

    fn fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/adapters/tui-screens")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
    }

    fn grid_from(name: &str) -> TerminalGrid {
        let mut grid = TerminalGrid::new();
        grid.push(&fixture(name));
        grid
    }

    /// The whole point of re-checking at Enter time rather than reusing
    /// the readiness-time classification: a grid whose CURRENT state is a
    /// gate must never let the Enter through, regardless of what led up
    /// to it -- a late-appearing gate classifies identically to one that
    /// was there from the start, which is exactly what makes this a
    /// pure function of current state rather than a state machine that
    /// could latch a stale "was ready" flag.
    #[test]
    fn enter_precondition_blocks_once_a_gate_is_on_the_current_screen() {
        use crate::adapter::tui::{ClaudeTuiVendor, CodexTuiVendor};

        let claude = ClaudeTuiVendor::new(PathBuf::from("/w"), vec![]);
        assert_eq!(
            enter_precondition(&claude, &grid_from("claude-workspace-trust.raw")),
            EnterPrecondition::Blocked(Surface::Gate(GateKind::ClaudeWorkspaceTrust)),
            "a claude gate on screen must withhold the Enter, naming that gate"
        );

        // codex-composer-then-trust.raw, not codex-directory-trust.raw:
        // the latter's own FINAL state has moved on past its gate to
        // codex's ordinary startup output (established in slice 2's
        // `codex_directory_trust_is_shown_before_it_moves_on_to_startup`),
        // so pushing it whole does not leave the gate on screen. This
        // fixture's final state genuinely is the gate -- it is the one
        // whose composer paints FIRST and the gate arrives after, which
        // is exactly the live race shape being tested here.
        let codex = CodexTuiVendor::new(PathBuf::from("/w"), vec![]);
        assert_eq!(
            enter_precondition(&codex, &grid_from("codex-composer-then-trust.raw")),
            EnterPrecondition::Blocked(Surface::Gate(GateKind::CodexDirectoryTrust)),
            "codex's gate on screen must withhold the Enter -- this is the exact shape of the \
             live race that motivated the re-check: the gate paints strictly after the \
             composer, so a readiness-time classification of PromptReady says nothing about \
             what is on screen by the time Enter is due"
        );
    }

    /// The other half: a genuinely ready composer, with no gate on
    /// screen, still gets its Enter -- the re-check is not a one-way
    /// switch that only ever withholds.
    #[test]
    fn enter_precondition_proceeds_when_the_composer_is_ready_with_no_gate_showing() {
        use crate::adapter::tui::ClaudeTuiVendor;

        let claude = ClaudeTuiVendor::new(PathBuf::from("/w"), vec![]);
        assert_eq!(
            enter_precondition(&claude, &grid_from("claude-trust-to-composer.raw")),
            EnterPrecondition::Proceed,
            "a gate that has been answered and cleared, composer now up, must let the Enter \
             through -- this is the fixture that exists specifically to prove a grid (unlike \
             the accumulator) can tell the difference"
        );
    }

    /// A vendor with no real predicate yet (`classify_surface` returns
    /// `None`) must keep the pre-classification behavior: this slice
    /// cannot recognize a gate for copilot/omp, so it must not withhold
    /// their Enter on the strength of a classification it never made.
    #[test]
    fn enter_precondition_proceeds_unconditionally_for_a_vendor_with_no_predicate() {
        use crate::adapter::tui::CopilotTuiVendor;

        let copilot = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        // Content is irrelevant here -- even a screen showing a claude
        // gate must not affect a vendor that has no predicate to read it
        // with.
        assert_eq!(
            enter_precondition(&copilot, &grid_from("claude-workspace-trust.raw")),
            EnterPrecondition::Proceed
        );
    }

    /// End-to-end proof that `wait_for_readiness` itself, not just the
    /// Enter-precondition helper, recognizes a gate from a real replayed
    /// capture (the same bytes a live vendor produced, fed into the grid
    /// the exact way `run_pipeline` feeds it) and parks on it rather than
    /// pasting into it: it journals `FirstRunGateDetected` naming the
    /// gate exactly once, then keeps polling -- on no deadline of its
    /// own -- until the process exits (forced here via `terminate()`
    /// rather than waiting out the double's own sleep), at which point
    /// the failure names the gate that was blocking it. This is also
    /// this slice's fixture-driven proof of requirement (c) ("a fake
    /// vendor that exits under the gate -> failure names the gate");
    /// requirement (a)'s remaining half -- that the paired
    /// `EscalationRaised` a real sink raises carries the right `kind`
    /// and a populated `question` -- is `event_sink.rs`'s own
    /// `first_run_gate_tests` module, which proves the DB/journal side
    /// this unit test's stub sink cannot.
    #[tokio::test]
    async fn wait_for_readiness_escalates_and_then_fails_when_the_process_exits_under_a_replayed_gate_capture()
     {
        use crate::adapter::tui::ClaudeTuiVendor;

        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/adapters/tui-screens/claude-workspace-trust.raw");
        let pty = Arc::new(
            PtyProcess::spawn(
                &crate::supervisor::SpawnSpec {
                    program: PathBuf::from("/bin/sh"),
                    args: vec![
                        "-c".to_string(),
                        format!("cat '{}' && sleep 30", fixture_path.display()),
                    ],
                    ..crate::supervisor::SpawnSpec::minimal()
                },
                EscalationTimings::default(),
            )
            .expect("spawn the replay double"),
        );

        let mut readiness_rx = pty.subscribe_output();
        let grid = Arc::new(StdMutex::new(TerminalGrid::new()));
        {
            let mut grid_rx = pty.subscribe_output();
            let grid = Arc::clone(&grid);
            tokio::spawn(async move {
                loop {
                    match grid_rx.recv().await {
                        Ok(bytes) => grid
                            .lock()
                            .expect("terminal-grid mutex never poisoned")
                            .push(&bytes),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

        let vendor = ClaudeTuiVendor::new(PathBuf::from("/w"), vec![]);
        let sink = GateRecordingSink::new();
        let dyn_sink: Arc<dyn AdapterEventSink> = sink.clone();
        let run_id = RunId::new();
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();

        let pty_for_wait = Arc::clone(&pty);
        let wait = tokio::spawn(async move {
            wait_for_readiness(
                &mut readiness_rx,
                "claude",
                &vendor,
                &grid,
                Duration::from_millis(50),
                Duration::from_secs(30),
                &pty_for_wait,
                Some(PromptInjection {
                    text: "this must never be written",
                    write_timeout: Duration::from_secs(1),
                }),
                tokio::time::Instant::now(),
                run_id,
                task_id,
                worker_id,
                &dyn_sink,
            )
            .await
        });

        // Park, don't fail: wait for the escalation to actually land
        // rather than asserting it failed immediately.
        assert!(
            wait_until(|| !sink.payloads().is_empty(), Duration::from_secs(5)).await,
            "a replayed gate capture must escalate, not silently fail closed or hang"
        );
        match &sink.payloads()[..] {
            [AdapterEventPayload::FirstRunGateDetected { kind }] => {
                assert_eq!(*kind, crew_protocol::FirstRunGateKind::ClaudeWorkspaceTrust);
            }
            other => panic!("expected exactly one FirstRunGateDetected, got {other:?}"),
        }

        // The gate never clears on its own (the fixture is a static
        // replay): force the exit path instead of waiting out the
        // double's own sleep, and confirm the failure names the gate.
        let _ = pty.terminate().await;
        let result = tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("must return promptly once the process exits")
            .expect("wait_for_readiness task must not panic");

        match result {
            Err(err) => {
                let message = err.to_string();
                assert!(
                    message.contains("ClaudeWorkspaceTrust"),
                    "the error must name the gate it saw: {message}"
                );
            }
            Ok(()) => {
                panic!("a replayed gate capture must never be classified as ready to paste into")
            }
        }
    }

    /// A truly unrecognized surface must fail closed once `cap` elapses --
    /// not loop forever. This is the regression case for a real bug the
    /// test above forced out during development: the `Undecided` poll
    /// branch returned early on the first quiet-tick receive timeout
    /// instead of re-checking `classify_surface` up to the actual
    /// deadline, so a burst of first output consumed entirely by the
    /// "wait for first output" check left nothing further for a later
    /// `rx.recv()` to ever receive, and the very first per-tick timeout
    /// was misread as "never becoming ready" -- failing a gate this
    /// module could positively identify, for the wrong reason. `cap` is
    /// kept short here specifically so this test still runs fast.
    #[tokio::test]
    async fn wait_for_readiness_fails_closed_after_cap_on_an_unrecognized_surface() {
        use crate::adapter::tui::ClaudeTuiVendor;

        let pty = Arc::new(
            PtyProcess::spawn(
                &crate::supervisor::SpawnSpec {
                    program: PathBuf::from("/bin/sh"),
                    args: vec!["-c".to_string(), "echo hello && sleep 5".to_string()],
                    ..crate::supervisor::SpawnSpec::minimal()
                },
                EscalationTimings::default(),
            )
            .expect("spawn the unrecognized-surface double"),
        );

        let mut readiness_rx = pty.subscribe_output();
        let grid = Arc::new(StdMutex::new(TerminalGrid::new()));
        {
            let mut grid_rx = pty.subscribe_output();
            let grid = Arc::clone(&grid);
            tokio::spawn(async move {
                loop {
                    match grid_rx.recv().await {
                        Ok(bytes) => grid
                            .lock()
                            .expect("terminal-grid mutex never poisoned")
                            .push(&bytes),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

        let vendor = ClaudeTuiVendor::new(PathBuf::from("/w"), vec![]);
        let sink: Arc<dyn AdapterEventSink> = GateRecordingSink::new();
        let started = tokio::time::Instant::now();
        let result = wait_for_readiness(
            &mut readiness_rx,
            "claude",
            &vendor,
            &grid,
            Duration::from_millis(30),
            Duration::from_millis(300),
            &pty,
            Some(PromptInjection {
                text: "this must never be written",
                write_timeout: Duration::from_secs(1),
            }),
            tokio::time::Instant::now(),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            &sink,
        )
        .await;
        let elapsed = started.elapsed();

        let _ = pty.terminate().await;

        let message = result
            .expect_err("an unrecognized surface must never be treated as ready")
            .to_string();
        assert!(
            message.contains("no recognizable prompt or first-run gate"),
            "unexpected failure reason: {message}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "must fail at the readiness cap (300ms), not hang: took {elapsed:?}"
        );
    }

    /// A vendor that exits before any surface is positively identified
    /// must fail promptly, not spin `classify_surface` at full speed for
    /// the rest of a (deliberately long) `cap`: a closed broadcast
    /// channel's `recv()` returns immediately and keeps doing so, so
    /// treating it the same as a per-tick pacing timeout would busy-loop
    /// on the mutex and the classifier instead of waiting on anything.
    /// `cap` here is set far longer than this test's own timeout to make
    /// that failure mode visible if it regresses.
    #[tokio::test]
    async fn wait_for_readiness_fails_promptly_when_the_process_exits_unrecognized() {
        use crate::adapter::tui::ClaudeTuiVendor;

        let pty = Arc::new(
            PtyProcess::spawn(
                &crate::supervisor::SpawnSpec {
                    program: PathBuf::from("/bin/sh"),
                    args: vec!["-c".to_string(), "echo hello".to_string()],
                    ..crate::supervisor::SpawnSpec::minimal()
                },
                EscalationTimings::default(),
            )
            .expect("spawn the exiting double"),
        );

        let mut readiness_rx = pty.subscribe_output();
        let grid = Arc::new(StdMutex::new(TerminalGrid::new()));
        {
            let mut grid_rx = pty.subscribe_output();
            let grid = Arc::clone(&grid);
            tokio::spawn(async move {
                loop {
                    match grid_rx.recv().await {
                        Ok(bytes) => grid
                            .lock()
                            .expect("terminal-grid mutex never poisoned")
                            .push(&bytes),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

        let vendor = ClaudeTuiVendor::new(PathBuf::from("/w"), vec![]);
        let sink: Arc<dyn AdapterEventSink> = GateRecordingSink::new();
        let started = tokio::time::Instant::now();
        // A long cap: if the closed-channel case were mishandled as a
        // pacing tick, this test would either hang here or take
        // (approximately) this whole duration instead of failing
        // promptly once the process exits.
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_readiness(
                &mut readiness_rx,
                "claude",
                &vendor,
                &grid,
                Duration::from_millis(30),
                Duration::from_secs(30),
                &pty,
                Some(PromptInjection {
                    text: "this must never be written",
                    write_timeout: Duration::from_secs(1),
                }),
                tokio::time::Instant::now(),
                RunId::new(),
                TaskId::new(),
                WorkerId::new(),
                &sink,
            ),
        )
        .await
        .expect("must return well within the 5s outer timeout, long before the 30s readiness cap");
        let elapsed = started.elapsed();

        let message = result
            .expect_err("a process that exited unrecognized must never be treated as ready")
            .to_string();
        assert!(
            message.contains("the worker process exited"),
            "unexpected failure reason: {message}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "must fail promptly on the closed channel, not spin toward the 30s cap: took {elapsed:?}"
        );
    }

    /// Requirement (b): a vendor that clears its gate after a few ticks
    /// resumes normally. `wait_for_readiness` must escalate exactly once
    /// for the gate it first saw, then -- once the identical surface
    /// classifies as `PromptReady` on a later tick, nothing else about
    /// the call having changed -- return `Ok(())` exactly as an ungated
    /// readiness would, so the caller's ordinary phase-2 Enter delivery
    /// proceeds unmodified.
    #[tokio::test]
    async fn wait_for_readiness_resumes_normally_once_a_parked_gate_clears() {
        let pty = Arc::new(
            PtyProcess::spawn(
                &crate::supervisor::SpawnSpec {
                    program: PathBuf::from("/bin/sh"),
                    args: vec!["-c".to_string(), "printf hi && sleep 30".to_string()],
                    ..crate::supervisor::SpawnSpec::minimal()
                },
                EscalationTimings::default(),
            )
            .expect("spawn the ticking double"),
        );

        let mut readiness_rx = pty.subscribe_output();
        let grid = Arc::new(StdMutex::new(TerminalGrid::new()));
        {
            let mut grid_rx = pty.subscribe_output();
            let grid = Arc::clone(&grid);
            tokio::spawn(async move {
                loop {
                    match grid_rx.recv().await {
                        Ok(bytes) => grid
                            .lock()
                            .expect("terminal-grid mutex never poisoned")
                            .push(&bytes),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }

        let vendor = CountingGateVendor {
            gate: GateKind::ClaudeWorkspaceTrust,
            clear_after: 3,
            calls: AtomicU32::new(0),
        };
        let sink = GateRecordingSink::new();
        let dyn_sink: Arc<dyn AdapterEventSink> = sink.clone();

        let result = wait_for_readiness(
            &mut readiness_rx,
            "counting-gate",
            &vendor,
            &grid,
            Duration::from_millis(20),
            Duration::from_secs(5),
            &pty,
            None,
            tokio::time::Instant::now(),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            &dyn_sink,
        )
        .await;

        let _ = pty.terminate().await;

        result.expect("a gate that clears must resolve readiness normally, not fail it");
        match &sink.payloads()[..] {
            [AdapterEventPayload::FirstRunGateDetected { kind }] => {
                assert_eq!(*kind, crew_protocol::FirstRunGateKind::ClaudeWorkspaceTrust);
            }
            other => panic!(
                "the gate must be escalated exactly once even though classify_surface reports \
                 it on every tick until it clears: {other:?}"
            ),
        }
    }

    // ----------------------------------------------- the bracketed-paste invariant

    /// Prompt text is always delivered to a vendor TUI as a bracketed
    /// paste, never as raw keystrokes: `write_paste` is the only path
    /// permitted to write prompt bytes to a PTY. Shipped originally as a
    /// prompt-integrity fix (a multi-line prompt submitted line-by-line),
    /// live measurement later established it is also a security control
    /// -- the identical bytes written unframed are parsed by the vendor
    /// as keystrokes, and an escape sequence embedded in ordinary prompt
    /// text can move a first-run security dialog's selection off its
    /// safe default before crew's own Enter confirms it, turning an
    /// intended no-op into a silent trust grant.
    ///
    /// This file (`adapter.rs`) has every `PtyProcess::write_input` call
    /// site that exists for prompt delivery in this crate -- the other
    /// two hits for the name in the crate are `write_input`'s own
    /// definition (`supervisor/pty.rs`) and `display/attach.rs`'s relay
    /// of a human's own keystrokes typed directly into an attached pane,
    /// which is a different, already-understood mechanism (a person
    /// answering their own pane, not crew composing and submitting a
    /// prompt) and out of scope for this invariant.
    ///
    /// The check itself: every `write_input(` call site in this file's
    /// own source text must carry one of the four known-safe arguments
    /// below -- a paste chunk `write_paste` itself already framed, the
    /// single submit byte (twice, once per delivery path: fresh-start and
    /// queued `send`), or a fixed control sequence
    /// (`interrupt_sequence()`, twice: turn-cancel and steer). A fifth
    /// call site with a different argument -- most importantly, prompt
    /// TEXT written directly -- fails this test by not matching any
    /// allowed pattern, which is the whole point: a future path that
    /// writes prompt text outside `write_paste` reintroduces the bug this
    /// invariant exists to prevent even if it never touches `write_paste`
    /// itself.
    #[test]
    fn every_write_input_call_site_in_this_file_is_a_known_safe_shape() {
        let whole_file = include_str!("adapter.rs");
        // Scanning only the production portion, before `mod tests`: this
        // very test's own doc comment and regex literal below both
        // contain the substring "write_input(" many times over, which
        // would otherwise inflate the count against itself -- the same
        // self-exemption the marker guard gives its own test file, for
        // the same reason (a scanner's rule text is not what it scans).
        let source = whole_file
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map_or(whole_file, |(production, _)| production);
        let call_sites = source.matches("write_input(").count();
        assert_eq!(
            call_sites, 5,
            "the known-safe count below assumes exactly 5 call sites (write_paste's own chunk \
             write, two submit-byte writes, two interrupt-sequence writes); update the pattern \
             list AND this count together if a new one is added, don't just bump this number"
        );

        let allowed = regex::Regex::new(
            r"write_input\((chunk\)|enter\)|&bytes\[split_at\.\.\]\)|&self\.vendor\.interrupt_sequence\(\)\))",
        )
        .expect("valid regex");
        let matched = allowed.find_iter(source).count();
        assert_eq!(
            matched, call_sites,
            "a write_input( call site in this file does not match any of the known-safe \
             argument shapes -- if this is a new prompt-text write path, it must go through \
             write_paste instead; if it is a new safe shape (a single control byte or a \
             pre-framed chunk), add its exact argument text to the allowlist above"
        );
    }
}
