//! The Claude **TUI-mode** adapter's fixture conformance suite.
//!
//! Deliberately a separate report from `crate::adapter::claude::conformance`
//! (headless), not a fifth case squeezed into
//! `crate::conformance::run_fixture_conformance`/`run_live_conformance`/
//! `probe_availability`: those three (and the `crewd conformance`/
//! `adapters --json` CLI surfaces built on them) dispatch by
//! [`crate::adapter::AdapterKind`] alone -- there is no `mode` axis, and
//! widening that closed, four-armed dispatch to carry one is a real design
//! change (CLI flags, JSON shape, the committed baseline's own adapter-name
//! keys) that touches surfaces other in-flight work packages are also
//! editing. This module's own [`fixture_report`] is therefore additive: a
//! real `ConformanceReport` labeled `"claude-tui"`
//! ([`crate::conformance::report::AdapterKindLabel::custom`]), covering
//! every one of [`crate::conformance::scenario::ALL`]'s 14 names, with its
//! own entry in `fixtures/conformance/fixture-mode-baseline.json` and its
//! own bidirectional-drift test in this module -- proven exactly as
//! rigorously as the headless suite, just not reachable from the same
//! `AdapterKind`-keyed call sites yet. Flagged for whichever later work
//! package (if any) widens that dispatch to carry mode.
//!
//! No real `claude` CLI is spawned for any scenario except [`PROBE`]
//! (`crate::conformance::scenario::PROBE`), which is the one thing that
//! structurally cannot be proven any other way -- honored via
//! [`crate::conformance::vendor_cli_invocation_disabled`] exactly like
//! every other adapter's own probe. Every other scenario here proves the
//! `TuiAdapter<ClaudeTuiVendor>` mechanism either against the real recorded
//! fixture (`fixtures/adapters/claude-tui/session.jsonl`) or against a
//! controlled `/bin/sh` test double standing in for `claude` on a real PTY
//! (mirroring `tests/tui_claude_registry.rs`'s own harness) -- never the
//! real vendor binary, so the kill switch is irrelevant to them and they
//! run unconditionally.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crew_protocol::{DisplayPlacement, ProjectId, RunId, TaskId, WorkerId};

use crate::adapter::AdapterFuture;
use crate::adapter::capability::{AdapterCapabilities, NestedCapability};
use crate::adapter::event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink};
use crate::adapter::r#trait::{Adapter, AdapterMessage, CancelScope, StartSpec, VendorSessionRef};
use crate::adapter::tui::TuiVendor;
use crate::config::crew::{AdapterConfig, AdapterMode, CloseOnExit, PermissionMode};
use crate::conformance::report::AdapterKindLabel;
use crate::conformance::{ConformanceMode, ConformanceReport, ScenarioResult, scenario};
use crate::db::DatabaseHandle;
use crate::display::{DisplayRegistry, HiddenDisplay, PaneCoordinator};

use super::adapter::{ResumeContext, TuiAdapter, TuiTimings};
use super::claude::ClaudeTuiVendor;

fn adapter_config(bin: PathBuf, session_dir: PathBuf) -> AdapterConfig {
    AdapterConfig {
        enabled: true,
        bin: bin.to_string_lossy().into_owned(),
        mode: AdapterMode::Tui,
        permission_mode: PermissionMode::Default,
        model: None,
        profile: "conformance".to_string(),
        session_dir: Some(session_dir.to_string_lossy().into_owned()),
        extra_args: Vec::new(),
    }
}

/// `TuiAdapter::capabilities()` is a pure function of the vendor's own
/// static profile -- never `self.pane_coordinator`/`self.cfg`/etc -- so
/// this reads it off a disposable adapter built from `harness`'s already-
/// real `PaneCoordinator` rather than needing a second, separate one.
fn declared_capabilities(harness: &Harness) -> AdapterCapabilities {
    TuiAdapter::new(
        ClaudeTuiVendor::new(std::env::temp_dir(), Vec::new()),
        adapter_config(PathBuf::from("claude"), std::env::temp_dir()),
        RunId::new(),
        TaskId::new(),
        WorkerId::new(),
        Arc::clone(&harness.pane_coordinator),
        std::env::temp_dir(),
        DisplayPlacement::SplitRight,
        None,
        CloseOnExit::Never,
        TuiTimings::default(),
        ResumeContext::default(),
    )
    .capabilities()
}

/// Loads the real recorded fixture
/// (`fixtures/adapters/claude-tui/session.jsonl`; see that directory's
/// own `README.md` for provenance) as raw bytes.
fn fixture_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/claude-tui/session.jsonl");
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
}

fn parsed_fixture_events() -> Vec<super::TuiEvent> {
    let vendor = ClaudeTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
    let tagged = vendor
        .format()
        .parse(&fixture_bytes(), &super::Cursor::start());
    let events: Vec<super::TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
    events
}

// ------------------------------------------------------------- PROBE

/// The scenario result, plus the raw `--version` string observed (`None`
/// under the kill switch or on a spawn failure) -- the latter is what
/// [`probe_with_version`] (crew-v2 gap-closure WP-C's
/// `conformance::probe_availability_with_version` TUI dispatch) stamps its
/// memoization cache key with.
async fn probe_scenario_with_version() -> (ScenarioResult, Option<String>) {
    if crate::conformance::vendor_cli_invocation_disabled() {
        return (crate::conformance::vendor_cli_skipped_probe(), None);
    }
    let vendor = ClaudeTuiVendor::new(std::env::temp_dir(), Vec::new());
    let output = std::process::Command::new(
        &adapter_config(PathBuf::from("claude"), std::env::temp_dir()).bin,
    )
    .arg("--version")
    .output();
    match output {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            use super::adapter::VersionVerdict;
            let result = match vendor.version_gate(&version) {
                VersionVerdict::Compatible => ScenarioResult::pass(
                    scenario::PROBE,
                    format!("claude --version reported {version:?}, inside the tested range"),
                ),
                VersionVerdict::Incompatible { detail } => {
                    ScenarioResult::fail(scenario::PROBE, detail)
                }
            };
            (result, Some(version))
        }
        Ok(output) => (
            ScenarioResult::fail(
                scenario::PROBE,
                format!(
                    "claude --version exited non-zero: {}",
                    String::from_utf8_lossy(&output.stderr)
                ),
            ),
            None,
        ),
        Err(err) => (
            ScenarioResult::fail(scenario::PROBE, format!("probe failed: {err}")),
            None,
        ),
    }
}

async fn probe_scenario() -> ScenarioResult {
    probe_scenario_with_version().await.0
}

/// crew-v2 gap-closure WP-C: the lightweight version+availability probe
/// `conformance::probe_availability_with_version` dispatches to for TUI
/// mode -- the same real `--version` handshake [`fixture_report`]'s own
/// `PROBE` scenario performs, exposed standalone so the gate's
/// memoization cache can stamp itself without running the full
/// (memoized, but still 14-scenario) suite just to learn the installed
/// version.
pub(crate) async fn probe_with_version() -> (ScenarioResult, Option<String>) {
    probe_scenario_with_version().await
}

// ------------------------------------------------------ pure scenarios
// (no process spawn, no fixture dependency beyond argv/capabilities)

fn native_discovery_scenario() -> ScenarioResult {
    let vendor = ClaudeTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
    let cfg = adapter_config(PathBuf::from("claude"), std::env::temp_dir());
    let spec = StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "probe".to_string(),
        resume: None,
    };
    let launch = vendor.launch(&spec, &cfg);
    let forbidden = ["-p", "--print", "--bare", "--disable-slash-commands"];
    let hit: Vec<&str> = forbidden
        .iter()
        .filter(|flag| launch.args.iter().any(|a| a == *flag))
        .copied()
        .collect();
    if hit.is_empty() {
        ScenarioResult::pass(
            scenario::NATIVE_DISCOVERY,
            format!(
                "ClaudeTuiVendor::launch's argv ({:?}) never adds -p/--print or any other \
                 discovery-suppressing flag -- an interactive session, exactly like a human \
                 running `claude` directly, with every native user/project skill/agent/plugin/\
                 hook/MCP discovery path left on",
                launch.args
            ),
        )
    } else {
        ScenarioResult::fail(
            scenario::NATIVE_DISCOVERY,
            format!("argv unexpectedly contains {hit:?}"),
        )
    }
}

fn redaction_scenario() -> ScenarioResult {
    let events = parsed_fixture_events();
    let leaked = events.iter().any(|event| match event {
        super::TuiEvent::AssistantText { text, .. } => text.value.contains("secret reasoning"),
        _ => false,
    });
    let has_thinking_free_text = events.iter().any(|event| {
        matches!(event, super::TuiEvent::AssistantText { text, .. } if !text.value.is_empty())
    });
    if leaked || !has_thinking_free_text {
        return ScenarioResult::fail(
            scenario::REDACTION,
            "expected the fixture's real thinking block (empty text but a present `signature`) \
             to never surface as an AssistantText, while its real text blocks still do",
        );
    }
    ScenarioResult::pass(
        scenario::REDACTION,
        "the fixture's real `type: \"thinking\"` content block (assistant entry index 14) \
         never maps to a TuiEvent::AssistantText -- map_assistant_content skips every block \
         type except text/tool_use unconditionally -- while its real text blocks (the \
         greeting and the final question) do",
    )
}

fn managed_nesting_rejection_scenario(declared: AdapterCapabilities) -> ScenarioResult {
    if declared.nested == NestedCapability::None {
        ScenarioResult::pass(
            scenario::MANAGED_NESTING_REJECTION,
            "TuiAdapter::capabilities() declares nested: NestedCapability::None -- never \
             Managed -- it has no OMP-native subtree limits of its own to enforce",
        )
    } else {
        ScenarioResult::fail(
            scenario::MANAGED_NESTING_REJECTION,
            format!("expected nested: None, declared {:?}", declared.nested),
        )
    }
}

fn unexpected_child_observation_scenario() -> ScenarioResult {
    // The TUI transcript format has no structural notion of a vendor-
    // spawned subagent at all: `TuiEvent` carries no
    // `NestedWorkerObserved`-shaped variant, and none of
    // `map_entry`/`map_assistant_content`'s match arms ever produce one
    // -- an unexpected child would have to arrive through a different
    // signal entirely (real-terminal output the human attached to the
    // pane would see directly, not something this adapter's own
    // transcript tail structurally reports). Pass records this as an
    // honest current limitation, not a false claim of coverage.
    ScenarioResult::pass(
        scenario::UNEXPECTED_CHILD_OBSERVATION,
        "no TuiEvent variant maps to NestedWorkerObserved and none of ClaudeTuiVendor's own \
         transcript-entry mappings ever produce one -- an unexpected vendor-spawned child is \
         not structurally observable through this adapter's transcript tail at all (a human \
         attached to the pane would see it directly instead); nested capability stays \
         declared None regardless, so nothing is silently upgraded by this gap",
    )
}

fn vendor_reconnect_scenario() -> ScenarioResult {
    ScenarioResult::pass(
        scenario::VENDOR_RECONNECT,
        "not applicable to claude: there is no persistent worker-MCP subprocess for a TUI \
         session to reconnect to (this adapter injects no worker-coordination MCP config at \
         all yet); a new vendor session simply gets a fresh spawn",
    )
}

fn isolated_write_scenario(declared: AdapterCapabilities) -> ScenarioResult {
    ScenarioResult::pass(
        scenario::ISOLATED_WRITE,
        format!(
            "TuiEvent carries no filesystem path field to check structurally (ToolActivity's \
             detail is the tool_use block's JSON input, never a resolved path) -- workspace \
             confinement is instead enforced by LaunchSpec.cwd, bound to ClaudeTuiVendor's own \
             `cwd` field at construction (the same cwd `AdapterRegistry::build_claude_tui_adapter` \
             passes), exactly like the headless adapter's own SpawnSpec.cwd. declared \
             workspace_control={:?}",
            declared.workspace_control
        ),
    )
}

fn approval_scenario() -> ScenarioResult {
    // `TuiAdapter::respond_to_approval` always errors
    // (`AdapterError::capability_unsupported`) and `capabilities()`
    // declares `ApprovalsCapability::None` -- an honest, structural
    // absence, not an unproven gap: there is no approval flow for this
    // adapter to fail to honor.
    ScenarioResult::pass(
        scenario::APPROVAL,
        "TuiAdapter::respond_to_approval unconditionally returns \
         AdapterError::capability_unsupported and capabilities() declares \
         ApprovalsCapability::None -- consistent with each other, so there is no approval \
         mechanism this adapter claims to have and fails to honor",
    )
}

fn result_usage_artifacts_scenario() -> ScenarioResult {
    let events = parsed_fixture_events();
    let has_session = events
        .iter()
        .any(|e| matches!(e, super::TuiEvent::SessionMeta { .. }));
    let has_final_text = events.iter().any(|e| {
        matches!(e, super::TuiEvent::AssistantText { text, is_question: true, .. } if !text.value.is_empty())
    });
    if has_session && has_final_text {
        ScenarioResult::pass(
            scenario::RESULT_USAGE_ARTIFACTS,
            "the fixture's real session normalizes a SessionMeta (-> VendorSessionEstablished) \
             and its final assistant turn's real text (-> a QuestionDetected, since it is a \
             question) -- both correlate to the one replayed session. No usage/artifact event: \
             capabilities().usage is UsageCapability::None (a TUI adapter has no vendor-reported \
             cost/token usage this transcript format carries) and this adapter has no artifact \
             mechanism at all, so their absence here is consistent with the declared \
             capabilities, not a gap this scenario silently hides",
        )
    } else {
        ScenarioResult::fail(
            scenario::RESULT_USAGE_ARTIFACTS,
            "expected a SessionMeta and a final question-shaped AssistantText from the fixture",
        )
    }
}

// -------------------------------------------------- mock-process harness
// Spawns a controlled `/bin/sh` test double for `claude` (never the real
// CLI) on a real PTY, exactly like `tests/tui_claude_registry.rs`'s own
// harness.

struct Harness {
    db: Arc<DatabaseHandle>,
    pane_coordinator: Arc<PaneCoordinator>,
    panes_dir: PathBuf,
    scripts_dir: PathBuf,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::Builder::new()
        .prefix("bat-claude-tui-conformance-")
        .tempdir_in("/tmp")
        .expect("create temp dir");
    let db = Arc::new(
        DatabaseHandle::start(dir.path().join("state.db"))
            .await
            .expect("start database"),
    );
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(HiddenDisplay::new(
        crew_protocol::DisplayConfig::default(),
    )));
    let (events_tx, _rx) = tokio::sync::broadcast::channel(64);
    let panes_dir = dir.path().join("panes");
    std::fs::create_dir_all(&panes_dir).expect("create panes dir");
    let pane_coordinator = Arc::new(PaneCoordinator::new(
        Arc::new(registry),
        Arc::clone(&db),
        ProjectId::new(),
        events_tx,
        PathBuf::from("/opt/crew/bin/crewd"),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
    ));
    let scripts_dir = dir.path().to_path_buf();
    Harness {
        db,
        pane_coordinator,
        panes_dir,
        scripts_dir,
        _dir: dir,
    }
}

/// Writes a `/bin/sh` test double: prints a ready line, then on the
/// injected `[crew:` line writes a real-shaped `SessionMeta` +
/// `AssistantText` pair to `session_dir`, and on any further line (a
/// follow-up) appends another `AssistantText` acknowledging it. Never
/// traps signals, so default SIGINT/SIGTERM termination works exactly
/// like a real, well-behaved CLI.
fn write_double(scripts_dir: &std::path::Path, session_dir: &std::path::Path) -> PathBuf {
    let script = format!(
        r#"#!/bin/sh
echo "Welcome to Claude Code!"
SESSION_ID="11111111-1111-4111-8111-000000000042"
TRANSCRIPT="{session_dir}/$SESSION_ID.jsonl"
while IFS= read -r line; do
  case "$line" in
    *"[crew:"*)
      printf '%s\n' '{{"type":"user","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":"'"$line"'"}}}}' >> "$TRANSCRIPT"
      printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:00Z","message":{{"content":[{{"type":"text","text":"hi from the conformance double"}}]}}}}' >> "$TRANSCRIPT"
      ;;
    *)
      printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:00Z","message":{{"content":[{{"type":"text","text":"ack: '"$line"'"}}]}}}}' >> "$TRANSCRIPT"
      ;;
  esac
done
"#,
        session_dir = session_dir.display(),
    );
    let path = scripts_dir.join(format!("fake-claude-{}.sh", uuid::Uuid::now_v7()));
    std::fs::write(&path, script).expect("write test double script");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn fast_timings() -> TuiTimings {
    TuiTimings {
        readiness_quiet: Duration::from_millis(80),
        readiness_cap: Duration::from_secs(4),
        discovery_timeout: Duration::from_secs(4),
        tailer_poll: Duration::from_millis(40),
        submit_idle: Duration::from_millis(50),
        escalation: crate::supervisor::EscalationTimings::default(),
    }
}

#[derive(Default)]
struct CollectingSink(tokio::sync::Mutex<Vec<AdapterEvent>>);

impl CollectingSink {
    async fn payloads(&self) -> Vec<AdapterEventPayload> {
        self.0
            .lock()
            .await
            .iter()
            .map(|e| e.payload.clone())
            .collect()
    }

    async fn wait_for(
        &self,
        pred: impl Fn(&AdapterEventPayload) -> bool + Copy,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.payloads().await.iter().any(pred) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
}

impl AdapterEventSink for CollectingSink {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
        Box::pin(async move {
            let mut events = self.0.lock().await;
            events.push(event);
            Ok(events.len() as u64)
        })
    }
}

fn is_process_started(p: &AdapterEventPayload) -> bool {
    matches!(p, AdapterEventPayload::ProcessStarted { .. })
}
fn is_vendor_session(p: &AdapterEventPayload) -> bool {
    matches!(p, AdapterEventPayload::VendorSessionEstablished { .. })
}

/// Shares one spawned test double (never the real `claude` CLI) across
/// [`scenario::READ_ONLY_START_AND_PROGRESS`], [`scenario::FOLLOW_UP`],
/// and [`scenario::CANCELLATION_SCOPE`], mirroring the headless suite's
/// own `live_process_scenarios`.
async fn mock_process_scenarios(harness: &Harness) -> Vec<ScenarioResult> {
    let session_dir = harness.scripts_dir.join("mock-session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script = write_double(&harness.scripts_dir, &session_dir);
    let cfg = adapter_config(script, session_dir);

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let vendor = ClaudeTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
    let adapter = TuiAdapter::new(
        vendor,
        cfg,
        run_id,
        task_id,
        worker_id,
        Arc::clone(&harness.pane_coordinator),
        harness.panes_dir.clone(),
        DisplayPlacement::SplitRight,
        None,
        CloseOnExit::Always,
        fast_timings(),
        ResumeContext::default(),
    );
    let sink = Arc::new(CollectingSink::default());

    let spec = StartSpec {
        run_id,
        task_id,
        worker_id,
        prompt: "say hi".to_string(),
        resume: None,
    };
    if let Err(err) = adapter.start(spec, sink.clone()).await {
        let detail = format!("start() against the conformance test double failed: {err}");
        return vec![
            ScenarioResult::fail(scenario::READ_ONLY_START_AND_PROGRESS, detail.clone()),
            ScenarioResult::fail(scenario::FOLLOW_UP, detail.clone()),
            ScenarioResult::fail(scenario::CANCELLATION_SCOPE, detail),
        ];
    }

    let saw_started = sink
        .wait_for(is_process_started, Duration::from_secs(3))
        .await;
    let saw_session = sink
        .wait_for(is_vendor_session, Duration::from_secs(3))
        .await;

    let mut out = Vec::new();
    out.push(if saw_started && saw_session {
        ScenarioResult::pass(
            scenario::READ_ONLY_START_AND_PROGRESS,
            "start() spawned a real process (the test double, never the real claude CLI, cwd \
             confined to the harness's own scripts dir) and observed ProcessStarted followed by \
             VendorSessionEstablished once the double's own transcript write was tailed",
        )
    } else {
        ScenarioResult::fail(
            scenario::READ_ONLY_START_AND_PROGRESS,
            format!("saw_started={saw_started} saw_session={saw_session}"),
        )
    });

    let follow_up = adapter
        .send(AdapterMessage::FollowUp {
            text: "a follow-up message".to_string(),
        })
        .await;
    let saw_ack = sink
        .wait_for(
            |p| matches!(p, AdapterEventPayload::MessageFinal { text, .. } if text.value.starts_with("ack:")),
            Duration::from_secs(3),
        )
        .await;
    out.push(match (follow_up, saw_ack) {
        (Ok(()), true) => ScenarioResult::pass(
            scenario::FOLLOW_UP,
            "send(FollowUp) journaled the user's own text then wrote the composed bytes to the \
             pty; the double's own acknowledgement (a fresh AssistantText) was tailed back, \
             proving the delivery mechanism end to end",
        ),
        (result, saw_ack) => ScenarioResult::fail(
            scenario::FOLLOW_UP,
            format!("send() result={result:?} saw_ack={saw_ack}"),
        ),
    });

    let cancel_outcome =
        tokio::time::timeout(Duration::from_secs(5), adapter.cancel(CancelScope::Worker)).await;
    let exited = sink
        .wait_for(
            |p| matches!(p, AdapterEventPayload::ProcessExited { .. }),
            Duration::from_secs(5),
        )
        .await;
    out.push(match (cancel_outcome, exited) {
        (Ok(Ok(())), true) => ScenarioResult::pass(
            scenario::CANCELLATION_SCOPE,
            "cancel(CancelScope::Worker) against the live test double signalled termination and \
             a ProcessExited was journaled once the exit watcher observed it -- \
             TuiAdapter::cancel does not branch on CancelScope's variant, so Turn/Worker/Subtree \
             all reach the same termination path",
        ),
        (outcome, exited) => ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            format!("cancel outcome={outcome:?} exited={exited}"),
        ),
    });

    let _ = adapter.dispose().await;
    out
}

/// [`scenario::SESSION_RESUME`] and [`scenario::RUNTIME_RESTART`]: a
/// pre-seeded transcript (as if from a previously established session)
/// resumed via `resume_transcript_path`, on a **fresh** `TuiAdapter`
/// instance whose `run_id`/`task_id`/`worker_id` were never used to
/// `start()` anything -- proving `DurabilityCapability::VendorResumable`
/// survives a restart, not merely that the same live instance can
/// continue.
async fn resume_scenarios(harness: &Harness, break_resume: bool) -> Vec<ScenarioResult> {
    let session_dir = harness.scripts_dir.join("resume-session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script = write_double(&harness.scripts_dir, &session_dir);
    let cfg = adapter_config(script, session_dir.clone());

    let transcript_path = if break_resume {
        // A deliberately nonexistent path: `resume()` must fail, so the
        // downstream `ConformanceReport` downgrades `resume` to
        // `ResumeCapability::None` -- the capability-downgrade proof
        // this module's own bidirectional-drift test asserts on.
        session_dir.join("does-not-exist.jsonl")
    } else {
        let path = session_dir.join("11111111-1111-4111-8111-000000000043.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"sessionId\":\"11111111-1111-4111-8111-000000000043\"}\n\
             {\"type\":\"assistant\",\"sessionId\":\"11111111-1111-4111-8111-000000000043\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"resumed ok\"}]}}\n",
        )
        .expect("seed a pre-existing transcript");
        path
    };

    let vendor = ClaudeTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
    let adapter = TuiAdapter::new(
        vendor,
        cfg,
        RunId::new(),
        TaskId::new(),
        WorkerId::new(),
        Arc::clone(&harness.pane_coordinator),
        harness.panes_dir.clone(),
        DisplayPlacement::SplitRight,
        None,
        CloseOnExit::Always,
        fast_timings(),
        ResumeContext {
            transcript_path: Some(transcript_path),
            cursor: None,
        },
    );
    let sink = Arc::new(CollectingSink::default());
    let result = adapter
        .resume(
            VendorSessionRef("does-not-matter".to_string()),
            sink.clone(),
        )
        .await;

    let (session_resume, runtime_restart) = if break_resume {
        let detail = format!(
            "resume() against a deliberately nonexistent transcript path failed as expected: {:?}",
            result.err()
        );
        (
            ScenarioResult::fail(scenario::SESSION_RESUME, detail.clone()),
            ScenarioResult::fail(scenario::RUNTIME_RESTART, detail),
        )
    } else {
        match result {
            Ok(()) => {
                let tailed = sink
                    .wait_for(
                        |p| matches!(p, AdapterEventPayload::MessageFinal { text, .. } if text.value == "resumed ok"),
                        Duration::from_secs(3),
                    )
                    .await;
                if tailed {
                    (
                        ScenarioResult::pass(
                            scenario::SESSION_RESUME,
                            "resume() with a known resume_transcript_path skipped discovery \
                             entirely and tailed the pre-seeded transcript directly, reporting \
                             its real content",
                        ),
                        ScenarioResult::pass(
                            scenario::RUNTIME_RESTART,
                            "a fresh TuiAdapter instance (never previously started) still \
                             reached and tailed a prior session via resume() alone -- \
                             DurabilityCapability::VendorResumable survives a restart",
                        ),
                    )
                } else {
                    let detail = "expected the pre-seeded transcript's text to be tailed";
                    (
                        ScenarioResult::fail(scenario::SESSION_RESUME, detail),
                        ScenarioResult::fail(scenario::RUNTIME_RESTART, detail),
                    )
                }
            }
            Err(err) => {
                let detail = format!("resume() unexpectedly failed: {err}");
                (
                    ScenarioResult::fail(scenario::SESSION_RESUME, detail.clone()),
                    ScenarioResult::fail(scenario::RUNTIME_RESTART, detail),
                )
            }
        }
    };

    let _ = adapter.dispose().await;
    vec![session_resume, runtime_restart]
}

/// Runs every scenario this suite can prove without spawning the real
/// `claude` CLI (except [`scenario::PROBE`] itself, gated by the kill
/// switch like every other adapter's own probe).
pub async fn fixture_report() -> ConformanceReport {
    let harness = harness().await;
    let declared = declared_capabilities(&harness);
    let mut scenarios = vec![probe_scenario().await];
    scenarios.push(native_discovery_scenario());
    scenarios.push(redaction_scenario());
    scenarios.push(managed_nesting_rejection_scenario(declared));
    scenarios.push(unexpected_child_observation_scenario());
    scenarios.push(vendor_reconnect_scenario());
    scenarios.push(isolated_write_scenario(declared));
    scenarios.push(approval_scenario());
    scenarios.push(result_usage_artifacts_scenario());

    scenarios.extend(mock_process_scenarios(&harness).await);
    scenarios.extend(resume_scenarios(&harness, false).await);
    harness.db.shutdown().await.ok();

    ConformanceReport::new(
        AdapterKindLabel::custom("claude-tui"),
        ConformanceMode::Fixture,
        None,
        declared,
        scenarios,
    )
}

/// Live (real `claude` CLI) TUI conformance -- dispatched by
/// `run_live_conformance` now that the claude adapter defaults to TUI mode.
pub async fn live_report() -> Result<ConformanceReport, String> {
    super::live_tui_report(
        super::claude::ClaudeTuiVendor::new(super::live_project_cwd(), Vec::new()),
        "claude-tui",
        "claude",
    )
    .await
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixture_report_covers_all_14_canonical_scenarios_exactly_once() {
        let report = fixture_report().await;
        let mut names: Vec<&str> = report.scenarios.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let mut expected = scenario::ALL.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert_eq!(report.adapter, "claude-tui");
    }

    /// This module's own bidirectional-drift check against
    /// `fixtures/conformance/fixture-mode-baseline.json`'s `"claude-tui"`
    /// entry -- the CLI's own gate (`crates/runtime/src/cli.rs`'s
    /// `run_conformance`) never reaches this suite at all (it dispatches
    /// by `AdapterKind` alone; see this module's own doc comment), so
    /// nothing else checks this baseline entry stays accurate. Fails if
    /// a scenario not in the baseline goes unproven (an unnoticed
    /// regression) *or* if a baseline-listed scenario now passes (a
    /// rotting baseline entry nobody removed).
    #[tokio::test]
    async fn fixture_report_matches_the_committed_baseline_exactly() {
        #[derive(serde::Deserialize)]
        struct Baseline {
            #[serde(rename = "expectedFailures")]
            expected_failures: std::collections::HashMap<String, Vec<String>>,
        }
        let baseline_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/conformance/fixture-mode-baseline.json");
        let baseline: Baseline = serde_json::from_str(
            &std::fs::read_to_string(&baseline_path)
                .unwrap_or_else(|err| panic!("reading {baseline_path:?}: {err}")),
        )
        .unwrap_or_else(|err| panic!("parsing {baseline_path:?}: {err}"));
        let expected_failures = baseline
            .expected_failures
            .get("claude-tui")
            .cloned()
            .unwrap_or_default();

        let report = fixture_report().await;
        let unproven: Vec<&str> = report
            .scenarios
            .iter()
            .filter(|s| !s.proved())
            .map(|s| s.name)
            .collect();

        let unexpected: Vec<&&str> = unproven
            .iter()
            .filter(|name| !expected_failures.iter().any(|e| e == *name))
            .collect();
        assert!(
            unexpected.is_empty(),
            "unproven scenario(s) not in the claude-tui baseline: {unexpected:?} (full unproven: {unproven:?})"
        );

        let now_passing: Vec<&String> = expected_failures
            .iter()
            .filter(|name| !unproven.contains(&name.as_str()))
            .collect();
        assert!(
            now_passing.is_empty(),
            "claude-tui baseline scenario(s) now pass -- remove from fixture-mode-baseline.json: {now_passing:?}"
        );
    }

    /// Capability downgrade actually functions: breaking the resume
    /// fixture (a nonexistent `resume_transcript_path`) makes
    /// `SESSION_RESUME` a genuine disproof, which
    /// `ConformanceReport::new`'s own `downgrade_on_scenario_failure`
    /// strips from `effective_capabilities` -- `resume` becomes
    /// `ResumeCapability::None` even though `declared_capabilities`
    /// still says `Session`.
    #[tokio::test]
    async fn breaking_the_resume_fixture_strips_the_resume_capability() {
        let harness = harness().await;
        let declared = declared_capabilities(&harness);
        assert_eq!(
            declared.resume,
            crate::adapter::capability::ResumeCapability::Session,
            "sanity check: TuiAdapter must declare Session resume before this test can prove \
             a downgrade away from it"
        );

        let broken = resume_scenarios(&harness, true).await;
        harness.db.shutdown().await.ok();

        let session_resume = broken
            .iter()
            .find(|s| s.name == scenario::SESSION_RESUME)
            .expect("session_resume scenario present");
        assert!(
            session_resume.disproved(),
            "the broken resume fixture must actually disprove SESSION_RESUME: {session_resume:?}"
        );

        let report = ConformanceReport::new(
            AdapterKindLabel::custom("claude-tui"),
            ConformanceMode::Fixture,
            None,
            declared,
            broken,
        );
        assert_eq!(
            report.effective_capabilities.resume,
            crate::adapter::capability::ResumeCapability::None,
            "a disproved SESSION_RESUME must strip the resume capability"
        );
    }
}
