//! The Codex TUI adapter's own fixture-mode conformance suite, mirroring
//! [`super::claude_conformance`] scenario-for-scenario (the same 14
//! canonical names, the same bidirectional baseline drift check against
//! `fixtures/conformance/fixture-mode-baseline.json`'s `"codex-tui"`
//! entry). Like the Claude suite it dispatches nowhere in
//! `crate::conformance::run_fixture_conformance` -- that runner keys on
//! `AdapterKind` alone; this suite is driven by its own tests plus the
//! CLI gate's per-suite extension points.
//!
//! Every scenario provable without spawning the real `codex` CLI is
//! proved against the committed *synthetic* fixture
//! (`fixtures/adapters/codex-tui/session.jsonl`) or a `/bin/sh` test
//! double writing rollout-shaped lines; PROBE alone needs the real CLI
//! and honors the kill switch like every other vendor's probe.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crew_protocol::{ProjectId, RunId, TaskId, WorkerId};

use crate::config::crew::{
    AdapterConfig, AdapterMode as AdapterModeConfig, CloseOnExit, PermissionMode,
};
use crate::conformance::scenario;
use crate::conformance::{ConformanceMode, ConformanceReport, ScenarioResult};
use crate::db::DatabaseHandle;
use crate::display::{DisplayRegistry, HiddenDisplay, PaneCoordinator};
use crew_protocol::DisplayPlacement;

use super::adapter::TuiVendor as _;
use super::adapter::{SCENARIO_OBSERVATION_DEADLINE, TuiAdapter, TuiTimings, VersionVerdict};
use super::codex::CodexTuiVendor;
use super::{Cursor, ResumeContext, TuiEvent};

use crate::adapter::AdapterFuture;
use crate::adapter::event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink};
use crate::adapter::r#trait::{Adapter, AdapterMessage, StartSpec, VendorSessionRef};

fn adapter_config(bin: PathBuf, session_dir: PathBuf) -> AdapterConfig {
    AdapterConfig {
        enabled: true,
        bin: bin.to_string_lossy().into_owned(),
        mode: AdapterModeConfig::Tui,
        permission_mode: PermissionMode::Default,
        model: None,
        profile: "conformance".to_string(),
        session_dir: Some(session_dir.to_string_lossy().into_owned()),
        extra_args: Vec::new(),
    }
}

/// `TuiAdapter::capabilities()` is a pure function of the vendor's static
/// profile -- read off a disposable adapter rather than duplicating the
/// declaration.
fn declared_capabilities(harness: &Harness) -> crate::adapter::capability::AdapterCapabilities {
    TuiAdapter::new(
        CodexTuiVendor::new(std::env::temp_dir(), Vec::new()),
        adapter_config(PathBuf::from("codex"), std::env::temp_dir()),
        RunId::new(),
        TaskId::new(),
        WorkerId::new(),
        Arc::clone(&harness.pane_coordinator),
        std::env::temp_dir(),
        DisplayPlacement::SplitRight,
        None,
        None,
        CloseOnExit::Never,
        TuiTimings::default(),
        ResumeContext::default(),
    )
    .capabilities()
}

fn fixture_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/codex-tui/session.jsonl");
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
}

fn parsed_fixture_events() -> Vec<TuiEvent> {
    let vendor = CodexTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
    let tagged = vendor.format().parse(&fixture_bytes(), &Cursor::start());
    let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
    events
}

// ------------------------------------------------------------- PROBE

async fn probe_scenario_with_version() -> (ScenarioResult, Option<String>) {
    if crate::conformance::vendor_cli_invocation_disabled() {
        return (crate::conformance::vendor_cli_skipped_probe(), None);
    }
    let vendor = CodexTuiVendor::new(std::env::temp_dir(), Vec::new());
    let output = std::process::Command::new(
        &adapter_config(PathBuf::from("codex"), std::env::temp_dir()).bin,
    )
    .arg("--version")
    .output();
    match output {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let result = match vendor.version_gate(&version) {
                VersionVerdict::Compatible => ScenarioResult::pass(
                    scenario::PROBE,
                    format!("codex --version reported {version:?}, inside the tested range"),
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
                    "codex --version exited non-zero: {}",
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

/// See `claude_conformance::probe_with_version`'s
/// own doc comment -- same role, this vendor.
pub(crate) async fn probe_with_version() -> (ScenarioResult, Option<String>) {
    probe_scenario_with_version().await
}

// ------------------------------------------------------ pure scenarios

fn native_discovery_scenario() -> ScenarioResult {
    let vendor = CodexTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
    let cfg = adapter_config(PathBuf::from("codex"), std::env::temp_dir());
    let spec = StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "probe".to_string(),
        resume: None,
    };
    let launch = vendor.launch(&spec, &cfg);
    // The headless modes this adapter must never launch interactively:
    // `app-server` is the RPC server the headless adapter drives, `exec`
    // is the one-shot non-interactive subcommand.
    let forbidden = ["app-server", "exec", "-p"];
    let hit: Vec<&str> = forbidden
        .iter()
        .filter(|flag| launch.args.iter().any(|a| a == *flag))
        .copied()
        .collect();
    if hit.is_empty() {
        ScenarioResult::pass(
            scenario::NATIVE_DISCOVERY,
            format!(
                "CodexTuiVendor::launch's argv ({:?}) never adds app-server/exec/-p or any \
                 other headless/discovery-suppressing mode -- an interactive session, exactly \
                 like a human running `codex` directly, with every native user/project skill/\
                 agent/hook/MCP discovery path left on",
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
    // The rollout's reasoning response_item is the model's hidden
    // thinking: its summary text must never surface anywhere.
    let leaked = events.iter().any(|event| match event {
        TuiEvent::AssistantText { text, .. } => {
            text.value.contains("never surfaced by this adapter")
        }
        TuiEvent::ToolActivity { detail, .. } => {
            detail.value.contains("never surfaced by this adapter")
        }
        _ => false,
    });
    let has_real_text = events
        .iter()
        .any(|e| matches!(e, TuiEvent::AssistantText { text, .. } if text.value.contains("Hi!")));
    if leaked || !has_real_text {
        return ScenarioResult::fail(
            scenario::REDACTION,
            "expected the fixture's reasoning summary to never surface while its real \
             message text does",
        );
    }
    ScenarioResult::pass(
        scenario::REDACTION,
        "the fixture's reasoning response_item never maps to any surfaced event -- \
         map_response_item skips reasoning unconditionally, mirroring the headless adapter's \
         own thinking redaction -- while its real message text does",
    )
}

fn managed_nesting_rejection_scenario(
    declared: crate::adapter::capability::AdapterCapabilities,
) -> ScenarioResult {
    if declared.nested == crate::adapter::capability::NestedCapability::None {
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
    ScenarioResult::pass(
        scenario::UNEXPECTED_CHILD_OBSERVATION,
        "no TuiEvent variant maps to NestedWorkerObserved and none of CodexTuiVendor's own \
         rollout mappings ever produce one -- an unexpected vendor-spawned child is not \
         structurally observable through this adapter's transcript tail at all (a human \
         attached to the pane would see it directly instead); nested capability stays \
         declared None regardless, so nothing is silently upgraded by this gap",
    )
}

fn vendor_reconnect_scenario() -> ScenarioResult {
    ScenarioResult::pass(
        scenario::VENDOR_RECONNECT,
        "not applicable to codex: there is no persistent worker-MCP subprocess for a TUI \
         session to reconnect to (this adapter injects no worker-coordination MCP config at \
         all yet); a new vendor session simply gets a fresh spawn",
    )
}

fn isolated_write_scenario(
    declared: crate::adapter::capability::AdapterCapabilities,
) -> ScenarioResult {
    ScenarioResult::pass(
        scenario::ISOLATED_WRITE,
        format!(
            "TuiEvent carries no filesystem path field to check structurally (ToolActivity's \
             detail is the function_call's raw arguments string, never a resolved path) -- \
             workspace confinement is instead enforced by LaunchSpec.cwd, bound to \
             CodexTuiVendor's own `cwd` field at construction, exactly like the headless \
             adapter's own SpawnSpec.cwd. declared workspace_control={:?}",
            declared.workspace_control
        ),
    )
}

fn approval_scenario() -> ScenarioResult {
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
        .any(|e| matches!(e, TuiEvent::SessionMeta { .. }));
    let has_final_text = events.iter().any(|e| {
        matches!(e, TuiEvent::AssistantText { text, is_question: true, .. } if !text.value.is_empty())
    });
    if has_session && has_final_text {
        ScenarioResult::pass(
            scenario::RESULT_USAGE_ARTIFACTS,
            "the fixture normalizes a SessionMeta (-> VendorSessionEstablished) and its \
             assistant turn's question-shaped text (-> a QuestionDetected). No usage/artifact \
             event: capabilities().usage is UsageCapability::None (a rollout transcript \
             carries no cost/token facts this adapter maps -- token_count event_msgs are \
             deliberately unmapped telemetry) and this adapter has no artifact mechanism, so \
             their absence is consistent with the declared capabilities",
        )
    } else {
        ScenarioResult::fail(
            scenario::RESULT_USAGE_ARTIFACTS,
            "expected a SessionMeta and a final question-shaped AssistantText from the fixture",
        )
    }
}

// -------------------------------------------------- mock-process harness

struct Harness {
    db: Arc<DatabaseHandle>,
    pane_coordinator: Arc<PaneCoordinator>,
    panes_dir: PathBuf,
    scripts_dir: PathBuf,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let dir = tempfile::Builder::new()
        .prefix("bat-codex-tui-conformance-")
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
        crate::security::redaction::Redactor::new(),
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

/// Writes a `/bin/sh` test double: prints a ready banner, then on the
/// injected `[crew:` line appends rollout-shaped entries (a session_meta,
/// the user's own nonce-bearing message, and an assistant response_item)
/// to a rollout file under `session_dir`, and on any further line another
/// acknowledging assistant message. Never traps signals, so default
/// termination works exactly like a real, well-behaved CLI.
///
/// The second echoed line matters now, not just as flavor text:
/// `wait_for_readiness` polls `classify_surface` before pasting, and
/// codex's own predicate looks for its composer's real banner text (see
/// `classify.rs`'s `CODEX_PROMPT_READY`) -- without this line the double
/// never reaches `PromptReady` and every scenario using it times out at
/// `readiness_cap`. (Claude's sibling double happens to say "Welcome to
/// Claude Code!", which already contains that vendor's banner text by
/// coincidence; this one needs its own line because "Welcome to Codex!"
/// does not.)
fn write_double(scripts_dir: &std::path::Path, session_dir: &std::path::Path) -> PathBuf {
    let script = format!(
        r#"#!/bin/sh
echo "Welcome to Codex!"
echo "Ask Codex to do anything"
SESSION_ID="33333333-3333-4333-8333-000000000042"
ROLLOUT="{session_dir}/rollout-2026-01-01T00-00-00-$SESSION_ID.jsonl"
CREW_ESC=$(printf '\033')
while IFS= read -r line; do
  # A real vendor TUI consumes bracketed-paste framing and keeps only the
  # pasted content; this double does the same, so the text it echoes back
  # is the prompt itself rather than the escape sequences around it.
  line=$(printf '%s' "$line" | tr -d "$CREW_ESC" | sed -e 's/\[200~//g' -e 's/\[201~//g')
  case "$line" in
    *"[crew:"*)
      printf '%s\n' '{{"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{{"session_id":"'"$SESSION_ID"'","id":"'"$SESSION_ID"'","cwd":"/workspace/crew"}}}}' >> "$ROLLOUT"
      printf '%s\n' '{{"type":"response_item","timestamp":"2026-01-01T00:00:00Z","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"'"$line"'"}}]}}}}' >> "$ROLLOUT"
      printf '%s\n' '{{"type":"response_item","timestamp":"2026-01-01T00:00:00Z","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"hi from the codex conformance double"}}]}}}}' >> "$ROLLOUT"
      ;;
    *)
      printf '%s\n' '{{"type":"response_item","timestamp":"2026-01-01T00:00:00Z","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"ack: '"$line"'"}}]}}}}' >> "$ROLLOUT"
      ;;
  esac
done
"#,
        session_dir = session_dir.display(),
    );
    let path = scripts_dir.join(format!("fake-codex-{}.sh", uuid::Uuid::now_v7()));
    std::fs::write(&path, script).expect("write test double script");
    use std::os::unix::fs::PermissionsExt as _;
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn fast_timings() -> TuiTimings {
    TuiTimings {
        readiness_quiet: Duration::from_millis(80),
        // Kept at production's own values. `readiness_cap`,
        // `discovery_timeout` and `preflight_timeout` are failure bounds,
        // not pacing delays -- see `assert_only_pacing_is_accelerated`,
        // which is what keeps this true for any field added later.
        readiness_cap: TuiTimings::default().readiness_cap,
        discovery_timeout: TuiTimings::default().discovery_timeout,
        tailer_poll: Duration::from_millis(40),
        submit_idle: Duration::from_millis(50),
        paste_write_timeout: TuiTimings::default().paste_write_timeout,
        escalation: crate::supervisor::EscalationTimings::default(),
        preflight_timeout: TuiTimings::default().preflight_timeout,
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

    fn note_real_user_turn(&self, _run_id: RunId) -> AdapterFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

fn is_process_started(p: &AdapterEventPayload) -> bool {
    matches!(p, AdapterEventPayload::ProcessStarted { .. })
}
fn is_vendor_session(p: &AdapterEventPayload) -> bool {
    matches!(p, AdapterEventPayload::VendorSessionEstablished { .. })
}

async fn mock_process_scenarios(harness: &Harness) -> Vec<ScenarioResult> {
    let session_dir = harness.scripts_dir.join("mock-session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script = write_double(&harness.scripts_dir, &session_dir);
    let cfg = adapter_config(script, session_dir);

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let vendor = CodexTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
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
        .wait_for(is_process_started, SCENARIO_OBSERVATION_DEADLINE)
        .await;
    let saw_session = sink
        .wait_for(is_vendor_session, SCENARIO_OBSERVATION_DEADLINE)
        .await;

    let mut out = Vec::new();
    out.push(if saw_started && saw_session {
        ScenarioResult::pass(
            scenario::READ_ONLY_START_AND_PROGRESS,
            "start() spawned a real process (the /bin/sh test double, never the real codex \
             CLI, cwd confined to the harness's own scripts dir) and observed ProcessStarted \
             followed by VendorSessionEstablished once the double's rollout-shaped \
             session_meta was tailed",
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
            SCENARIO_OBSERVATION_DEADLINE,
        )
        .await;
    out.push(match (follow_up, saw_ack) {
        (Ok(()), true) => ScenarioResult::pass(
            scenario::FOLLOW_UP,
            "send(FollowUp) journaled the user's own text then wrote the composed bytes to \
             the pty; the double's acknowledgement (a fresh assistant response_item) was \
             tailed back, proving the delivery mechanism end to end",
        ),
        (result, saw_ack) => ScenarioResult::fail(
            scenario::FOLLOW_UP,
            format!("send() result={result:?} saw_ack={saw_ack}"),
        ),
    });

    let cancel_outcome = tokio::time::timeout(
        SCENARIO_OBSERVATION_DEADLINE,
        adapter.cancel(crate::adapter::CancelScope::Worker),
    )
    .await;
    let exited = sink
        .wait_for(
            |p| matches!(p, AdapterEventPayload::ProcessExited { .. }),
            SCENARIO_OBSERVATION_DEADLINE,
        )
        .await;
    out.push(match (cancel_outcome, exited) {
        (Ok(Ok(())), true) => ScenarioResult::pass(
            scenario::CANCELLATION_SCOPE,
            "cancel(CancelScope::Worker) against the live test double signalled termination \
             and a ProcessExited was journaled once the exit watcher observed it",
        ),
        (outcome, exited) => ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            format!("cancel outcome={outcome:?} exited={exited}"),
        ),
    });

    let _ = adapter.dispose().await;
    out
}

/// SESSION_RESUME + RUNTIME_RESTART: a pre-seeded rollout resumed via
/// `resume_transcript_path` on a **fresh** adapter instance, proving
/// `DurabilityCapability::VendorResumable` survives a restart. The
/// seeded file uses the real date-partitioned rollout *naming* so the
/// codex-specific filename shape is exercised too.
async fn resume_scenarios(harness: &Harness, break_resume: bool) -> Vec<ScenarioResult> {
    let session_dir = harness.scripts_dir.join("resume-session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script = write_double(&harness.scripts_dir, &session_dir);
    let cfg = adapter_config(script, session_dir.clone());

    let transcript_path = if break_resume {
        session_dir.join("does-not-exist.jsonl")
    } else {
        let path = session_dir
            .join("rollout-2026-01-01T00-00-00-33333333-3333-4333-8333-000000000043.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session_meta\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"payload\":{\"session_id\":\"33333333-3333-4333-8333-000000000043\",\"cwd\":\"/workspace/crew\"}}\n\
             {\"type\":\"response_item\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"resumed ok\"}]}}\n",
        )
        .expect("seed a pre-existing rollout");
        path
    };

    let vendor = CodexTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
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
            "resume() against a deliberately nonexistent rollout path failed as expected: {:?}",
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
                        SCENARIO_OBSERVATION_DEADLINE,
                    )
                    .await;
                if tailed {
                    (
                        ScenarioResult::pass(
                            scenario::SESSION_RESUME,
                            "resume() with a known resume_transcript_path skipped discovery \
                             entirely and tailed the pre-seeded date-partitioned rollout \
                             directly, reporting its real content",
                        ),
                        ScenarioResult::pass(
                            scenario::RUNTIME_RESTART,
                            "a fresh TuiAdapter instance (never previously started) still \
                             reached and tailed a prior session via resume() alone -- \
                             DurabilityCapability::VendorResumable survives a restart",
                        ),
                    )
                } else {
                    let detail = "expected the pre-seeded rollout's text to be tailed";
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
/// `codex` CLI (except [`scenario::PROBE`] itself, gated by the kill
/// switch like every other adapter's probe).
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
        crate::conformance::report::AdapterKindLabel::custom("codex-tui"),
        ConformanceMode::Fixture,
        None,
        declared,
        scenarios,
    )
}

/// Live (real `codex` CLI) TUI conformance -- dispatched by
/// `run_live_conformance` now that the codex adapter defaults to TUI mode.
pub async fn live_report() -> Result<ConformanceReport, String> {
    super::live_tui_report(
        super::codex::CodexTuiVendor::new(super::live_project_cwd(), Vec::new()),
        "codex-tui",
        "codex",
    )
    .await
}
#[cfg(test)]
mod tests {
    use super::*;

    /// This harness may accelerate pacing fields and must leave
    /// every failure bound at production's value. The guard's own
    /// destructuring is what covers a field added later; this call is what
    /// covers THIS harness.
    #[test]
    fn fast_timings_accelerate_only_pacing_fields() {
        super::super::assert_only_pacing_is_accelerated(fast_timings(), "codex-tui");
    }

    #[tokio::test]
    async fn fixture_report_covers_all_14_canonical_scenarios_exactly_once() {
        let report = fixture_report().await;
        let mut names: Vec<&str> = report.scenarios.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let mut expected = scenario::ALL.to_vec();
        expected.sort_unstable();
        assert_eq!(names, expected);
        assert_eq!(report.adapter, "codex-tui");
    }

    /// Bidirectional drift check against
    /// `fixtures/conformance/fixture-mode-baseline.json`'s `"codex-tui"`
    /// entry -- nothing else checks this entry stays accurate (the CLI
    /// gate dispatches by `AdapterKind` alone).
    #[tokio::test]
    async fn fixture_report_matches_the_committed_baseline_exactly() {
        #[derive(serde::Deserialize)]
        struct Baseline {
            #[serde(rename = "expectedFailures")]
            expected_failures: std::collections::BTreeMap<String, Vec<String>>,
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
            .get("codex-tui")
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
            "unproven scenario(s) not in the codex-tui baseline: {unexpected:?} (full unproven: {unproven:?})"
        );

        let now_passing: Vec<&String> = expected_failures
            .iter()
            .filter(|name| !unproven.contains(&name.as_str()))
            .collect();
        assert!(
            now_passing.is_empty(),
            "codex-tui baseline scenario(s) now pass -- remove from fixture-mode-baseline.json: {now_passing:?}"
        );
    }

    /// The capability-downgrade proof: breaking the resume fixture makes
    /// SESSION_RESUME a genuine disproof, which
    /// `ConformanceReport::new`'s own `downgrade_on_scenario_failure`
    /// strips from `effective_capabilities`.
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

        let mut scenarios = mock_process_scenarios(&harness).await;
        scenarios.extend(resume_scenarios(&harness, true).await);
        let report = ConformanceReport::new(
            crate::conformance::report::AdapterKindLabel::custom("codex-tui"),
            ConformanceMode::Fixture,
            None,
            declared,
            scenarios,
        );
        assert_eq!(
            report.effective_capabilities.resume,
            crate::adapter::capability::ResumeCapability::None,
            "a failed SESSION_RESUME must downgrade resume to None"
        );
        harness.db.shutdown().await.ok();
    }

    /// The live P3b regression, reproduced end to end through the real
    /// `TuiAdapter`: codex's composer paints first (readiness classifies
    /// `PromptReady`, the prompt is pasted), then a splash screen
    /// paints, then the real directory-trust gate paints -- strictly
    /// AFTER the paste, which is exactly the window
    /// `wait_for_enter_precondition` exists to catch. Before that
    /// mechanism existed this sequence made `start()` fail outright,
    /// having already pasted into what turned out to be a gate; the fix
    /// is to park and escalate instead, identically to a gate seen
    /// before any paste at all -- never a start failure.
    ///
    /// The live regression's splash also carried an escape sequence
    /// this module does not implement, which is deliberately NOT
    /// reproduced here: `TerminalGrid::mark_unsupported`'s test-build
    /// body panics by design (see its own doc comment), and a panic
    /// inside the background task that feeds this grid from the PTY
    /// would kill that task outright, silently starving every later
    /// push -- including the gate text itself -- rather than exercising
    /// anything. That half of the regression (a gate phrase surviving
    /// an unsupported sequence elsewhere on screen) is proven at the
    /// classifier level instead, directly against the latch, by
    /// `classify.rs`'s own
    /// `a_gate_phrase_survives_an_unsupported_sequence_and_still_returns_gate`.
    /// This test's job is the other half: the parking mechanism itself,
    /// against an ordinary (fully implemented) splash.
    #[tokio::test]
    async fn a_gate_appearing_after_the_prompt_was_pasted_parks_and_escalates_rather_than_failing_start()
     {
        use crate::adapter::r#trait::CancelScope;

        let harness = harness().await;
        let session_dir = harness.scripts_dir.join("late-gate-session");
        std::fs::create_dir_all(&session_dir).expect("create session dir");

        // Deliberately its own minimal double, not `write_double`: this
        // scenario is purely about ordering and screen content, not about
        // rollout tailing or follow-ups. An earlier version of this
        // double painted the splash and gate on a fixed timer (0.7s
        // after spawn) -- that passed on a clean-tree run and failed on
        // a loaded macOS CI runner, because a timer is not an ordering
        // guarantee: any single scheduling gap wide enough for
        // `wait_for_output_idle` to see `submit_idle`'s worth of quiet
        // lets `wait_for_enter_precondition` tick against the stale
        // ready phrase before the gate has actually painted, and Enter
        // goes out early.
        //
        // The fix makes the paint a CONSEQUENCE of the paste rather than
        // of the clock, in two parts:
        //
        // 1. Ordering: `read -r` (a whole line) cannot be the trigger --
        //    the submit byte (`\r`) is deliberately withheld until this
        //    precondition passes, so nothing the double ever reads
        //    terminates a line, and blocking on one would hang forever.
        //    Blocking on a single BYTE does work: the pty starts in
        //    canonical mode, so `stty -icanon` first drops the double's
        //    own stdin out of line buffering (a real vendor CLI does its
        //    own raw-mode setup; this double must do it too, by hand);
        //    with that done, the very first byte to arrive is the
        //    bracketed-paste opener's `ESC`, which is causally after the
        //    paste has begun. If `stty` is unavailable, the canonical-mode
        //    read never unblocks and this test fails on its own deadline
        //    -- a clear failure, not a hang mistaken for something else.
        // 2. Echo: `-echo` in that same `stty` call is not a hardening
        //    afterthought, it is load-bearing. A real vendor CLI disables
        //    terminal echo the moment it takes over (it renders its own
        //    UI); this double, left in the default echoing mode, means
        //    the pty's line discipline reflects the pasted bytes back as
        //    OUTPUT -- and because ECHOCTL renders each control byte as
        //    a two-character caret sequence (`^[` for one `ESC`), the
        //    echoed bracketed-paste framing is roughly twice as long as
        //    the real bytes crewd wrote. That was enough to overflow this
        //    grid's 120-column width mid-paste, silently dropping
        //    whatever landed past the edge -- including this double's own
        //    gate text, printed immediately afterward. Disabling echo
        //    removes the paste from the screen entirely, exactly like a
        //    real vendor: nothing here needs it visible, only the gate
        //    text this double paints on its own.
        let script = r#"#!/bin/sh
echo "Ask Codex to do anything"
stty -icanon -echo min 1 time 0 2>/dev/null
dd bs=1 count=1 >/dev/null 2>&1
printf '\033[K'
printf 'Do you trust the contents of this directory?\n'
sleep 30
"#
        .to_string();
        let script_path = harness
            .scripts_dir
            .join(format!("fake-codex-late-gate-{}.sh", uuid::Uuid::now_v7()));
        std::fs::write(&script_path, script).expect("write test double script");
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms).unwrap();
        }
        let cfg = adapter_config(script_path, session_dir);

        let run_id = RunId::new();
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let vendor = CodexTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
        let adapter = Arc::new(TuiAdapter::new(
            vendor,
            cfg,
            run_id,
            task_id,
            worker_id,
            Arc::clone(&harness.pane_coordinator),
            harness.panes_dir.clone(),
            DisplayPlacement::SplitRight,
            None,
            None,
            CloseOnExit::Always,
            fast_timings(),
            ResumeContext::default(),
        ));
        let sink = Arc::new(CollectingSink::default());

        let spec = StartSpec {
            run_id,
            task_id,
            worker_id,
            prompt: "say hi".to_string(),
            resume: None,
        };
        let start_adapter = Arc::clone(&adapter);
        let start_sink: Arc<dyn AdapterEventSink> = sink.clone();
        let mut start = tokio::spawn(async move { start_adapter.start(spec, start_sink).await });

        let saw_gate = sink
            .wait_for(
                |p| {
                    matches!(
                        p,
                        AdapterEventPayload::FirstRunGateDetected { kind }
                            if *kind == crew_protocol::FirstRunGateKind::CodexDirectoryTrust
                    )
                },
                SCENARIO_OBSERVATION_DEADLINE,
            )
            .await;
        assert!(
            saw_gate,
            "the late gate must be escalated even though the prompt was already pasted (saw: \
             {:?})",
            sink.payloads().await
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut start)
                .await
                .is_err(),
            "start() must still be parked on the gate -- it must not have already failed or \
             succeeded"
        );

        // `Worker`, not `Turn`: matches this crate's own cancel-scope
        // widening at a gate park (see `TuiAdapter::cancel`'s own
        // comment) -- either scope fires the same token here, `Worker`
        // is simply the scope real cancellation of a still-starting run
        // would use.
        adapter
            .cancel(CancelScope::Worker)
            .await
            .expect("cancelling a parked start must itself succeed");

        let result = tokio::time::timeout(SCENARIO_OBSERVATION_DEADLINE, start)
            .await
            .expect("start() must resolve promptly once cancelled, not hang")
            .expect("the start() task must not panic");
        let message = result
            .expect_err("a cancelled gate-park must fail start(), not silently succeed as ready")
            .to_string();
        assert!(
            message.contains("cancelled"),
            "unexpected failure reason: {message}"
        );

        harness.db.shutdown().await.ok();
    }
}
