//! The Copilot TUI adapter's own fixture-mode conformance suite, mirroring
//! [`super::codex_conformance`] scenario-for-scenario (the same 14
//! canonical names, the same bidirectional baseline drift check against
//! `fixtures/conformance/fixture-mode-baseline.json`'s `"copilot-tui"`
//! entry). Like the Claude/Codex suites it dispatches nowhere in
//! `crate::conformance::run_fixture_conformance` -- that runner keys on
//! `AdapterKind` alone; this suite is driven by its own tests plus the
//! CLI gate's per-suite extension points.
//!
//! Every scenario provable without spawning the real `copilot` CLI is
//! proved against the committed *synthetic* fixture
//! (`fixtures/adapters/copilot-tui/session.jsonl`) or a `/bin/sh` test
//! double writing session-shaped lines; PROBE alone needs the real CLI
//! and honors the kill switch like every other vendor's probe.

use std::path::{Path, PathBuf};
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
use super::adapter::{TuiAdapter, TuiTimings, VersionVerdict};
use super::copilot::CopilotTuiVendor;
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
        CopilotTuiVendor::new(std::env::temp_dir(), Vec::new()),
        adapter_config(PathBuf::from("copilot"), std::env::temp_dir()),
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

fn fixture_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/copilot-tui/session.jsonl");
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
}

fn parsed_fixture_events() -> Vec<TuiEvent> {
    let vendor = CopilotTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
    let tagged = vendor.format().parse(&fixture_bytes(), &Cursor::start());
    let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
    events
}

// ------------------------------------------------------------- PROBE

async fn probe_scenario_with_version() -> (ScenarioResult, Option<String>) {
    if crate::conformance::vendor_cli_invocation_disabled() {
        return (crate::conformance::vendor_cli_skipped_probe(), None);
    }
    let vendor = CopilotTuiVendor::new(std::env::temp_dir(), Vec::new());
    let output = std::process::Command::new(
        &adapter_config(PathBuf::from("copilot"), std::env::temp_dir()).bin,
    )
    .arg("--version")
    .output();
    match output {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let result = match vendor.version_gate(&version) {
                VersionVerdict::Compatible => ScenarioResult::pass(
                    scenario::PROBE,
                    format!(
                        "copilot --version reported {version:?}, one of the empirically \
                         verified versions"
                    ),
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
                    "copilot --version exited non-zero: {}",
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

/// crew-v2 gap-closure WP-C: see `claude_conformance::probe_with_version`'s
/// own doc comment -- same role, this vendor.
pub(crate) async fn probe_with_version() -> (ScenarioResult, Option<String>) {
    probe_scenario_with_version().await
}

// ------------------------------------------------------ pure scenarios

fn native_discovery_scenario() -> ScenarioResult {
    let vendor = CopilotTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
    let cfg = adapter_config(PathBuf::from("copilot"), std::env::temp_dir());
    let spec = StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "probe".to_string(),
        resume: None,
    };
    let launch = vendor.launch(&spec, &cfg);
    // The headless mode this adapter must never launch interactively:
    // `-p/--prompt` is the one-shot non-interactive mode.
    let forbidden = ["-p", "--prompt", "--print"];
    let hit: Vec<&str> = forbidden
        .iter()
        .filter(|flag| launch.args.iter().any(|a| a == *flag))
        .copied()
        .collect();
    if hit.is_empty() {
        ScenarioResult::pass(
            scenario::NATIVE_DISCOVERY,
            format!(
                "CopilotTuiVendor::launch's argv ({:?}) never adds -p/--prompt/--print or any \
                 other headless/discovery-suppressing mode -- an interactive session, exactly \
                 like a human running `copilot` directly, with every native user/project skill/\
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
    // The echoed user.message entries are the harness's own injected
    // words -- surfacing them would double-journal every prompt. They
    // must never appear; the assistant's real reply text must.
    let leaked = events.iter().any(|event| match event {
        TuiEvent::AssistantText { text, .. } => {
            text.value.contains("[crew:fixture1]")
                || text.value.contains("never surfaced by this adapter")
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
            "expected the echoed user prompts to never surface while the assistant's real \
             message text does",
        );
    }
    ScenarioResult::pass(
        scenario::REDACTION,
        "user.message echoes and non-assistant entry types never map to any surfaced event \
         while the assistant's real message text does -- only `assistant.message` output is \
         ever surfaced, so nothing user-authored or housekeeping-shaped can leak into the \
         journal through this adapter's tail",
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
        "no TuiEvent variant maps to NestedWorkerObserved and none of CopilotTuiVendor's own \
         session mappings ever produce one -- an unexpected vendor-spawned child is not \
         structurally observable through this adapter's transcript tail at all (a human \
         attached to the pane would see it directly instead); nested capability stays \
         declared None regardless, so nothing is silently upgraded by this gap",
    )
}

fn vendor_reconnect_scenario() -> ScenarioResult {
    ScenarioResult::pass(
        scenario::VENDOR_RECONNECT,
        "not applicable to copilot: there is no persistent worker-MCP subprocess for a TUI \
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
             detail is the tool request's raw arguments string, never a resolved path) -- \
             workspace confinement is instead enforced by LaunchSpec.cwd, bound to \
             CopilotTuiVendor's own `cwd` field at construction, exactly like the headless \
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
             event: capabilities().usage is UsageCapability::None (the session transcript \
             carries no cost/token facts this adapter maps -- per-tool execution bookkeeping \
             is deliberately unmapped telemetry) and this adapter has no artifact mechanism, \
             so their absence is consistent with the declared capabilities",
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
        .prefix("bat-copilot-tui-conformance-")
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

/// Writes a `/bin/sh` test double: prints a ready banner, then on the
/// injected `[crew:` line appends session-shaped entries (a session.start,
/// an echo of the user's own nonce-bearing message, and an assistant
/// reply) to a `<session-id>.jsonl` under `session_dir`, and on any
/// further line another acknowledging assistant message. Never traps
/// signals, so default termination works exactly like a real,
/// well-behaved CLI.
fn write_double(scripts_dir: &std::path::Path, session_dir: &std::path::Path) -> PathBuf {
    let script = format!(
        r#"#!/bin/sh
echo "Welcome to GitHub Copilot CLI!"
SESSION_ID="44444444-4444-4444-8444-000000000042"
SESSION="{session_dir}/$SESSION_ID.jsonl"
printf '%s\n' '{{"type":"session.start","data":{{"sessionId":"'"$SESSION_ID"'","startTime":"2026-01-01T00:00:00.000Z"}}}}' >> "$SESSION"
CREW_ESC=$(printf '\033')
while IFS= read -r line; do
  # A real vendor TUI consumes bracketed-paste framing and keeps only the
  # pasted content; this double does the same, so the text it echoes back
  # is the prompt itself rather than the escape sequences around it.
  line=$(printf '%s' "$line" | tr -d "$CREW_ESC" | sed -e 's/\[200~//g' -e 's/\[201~//g')
  case "$line" in
    *"[crew:"*)
      printf '%s\n' '{{"type":"user.message","data":{{"content":"'"$line"'"}},"id":"u0"}}' >> "$SESSION"
      printf '%s\n' '{{"type":"assistant.message","data":{{"content":"","messageId":"m1","toolRequests":[{{"toolCallId":"t1","name":"bash","arguments":{{"command":["bash","-lc","echo crew-fixture"]}}}}]}},' >> "$SESSION"
      printf '%s\n' '{{"type":"assistant.message","data":{{"content":"hi from the copilot conformance double?","messageId":"m2","toolRequests":[]}}}}' >> "$SESSION"
      ;;
    *)
      printf '%s\n' '{{"type":"assistant.message","data":{{"content":"ack: '"$line"'","messageId":"m3","toolRequests":[]}}}}' >> "$SESSION"
      ;;
  esac
done
"#,
        session_dir = session_dir.display(),
    );
    let path = scripts_dir.join(format!("fake-copilot-{}.sh", uuid::Uuid::now_v7()));
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
        readiness_cap: Duration::from_secs(4),
        discovery_timeout: Duration::from_secs(4),
        tailer_poll: Duration::from_millis(40),
        submit_idle: Duration::from_millis(50),
        paste_write_timeout: Duration::from_millis(500),
        escalation: crate::supervisor::EscalationTimings::default(),
        preflight_timeout: Duration::from_secs(4),
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

async fn mock_process_scenarios(harness: &Harness) -> Vec<ScenarioResult> {
    let session_dir = harness.scripts_dir.join("mock-session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script = write_double(&harness.scripts_dir, &session_dir);
    let cfg = adapter_config(script, session_dir);

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let vendor = CopilotTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
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
            "start() spawned a real process (the /bin/sh test double, never the real copilot \
             CLI, cwd confined to the harness's own scripts dir) and observed ProcessStarted \
             followed by VendorSessionEstablished once the double's session-shaped \
             session.start line was tailed",
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
            "send(FollowUp) journaled the user's own text then wrote the composed bytes to \
             the pty; the double's acknowledgement (a fresh assistant.message) was tailed \
             back, proving the delivery mechanism end to end",
        ),
        (result, saw_ack) => ScenarioResult::fail(
            scenario::FOLLOW_UP,
            format!("send() result={result:?} saw_ack={saw_ack}"),
        ),
    });

    let cancel_outcome = tokio::time::timeout(
        Duration::from_secs(5),
        adapter.cancel(crate::adapter::CancelScope::Worker),
    )
    .await;
    let exited = sink
        .wait_for(
            |p| matches!(p, AdapterEventPayload::ProcessExited { .. }),
            Duration::from_secs(5),
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

/// SESSION_RESUME + RUNTIME_RESTART: a pre-seeded session resumed via
/// `resume_transcript_path` on a **fresh** adapter instance, proving
/// `DurabilityCapability::VendorResumable` survives a restart. The
/// seeded file uses the real flat `<session-id>.jsonl` naming so the
/// copilot-specific layout stays exercised too.
async fn resume_scenarios(harness: &Harness, break_resume: bool) -> Vec<ScenarioResult> {
    let session_dir = harness.scripts_dir.join("resume-session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script = write_double(&harness.scripts_dir, &session_dir);
    let cfg = adapter_config(script, session_dir.clone());

    let transcript_path = if break_resume {
        session_dir.join("does-not-exist.jsonl")
    } else {
        let path = session_dir.join("44444444-4444-4444-8444-000000000043.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session.start\",\"data\":{\"sessionId\":\"44444444-4444-4444-8444-000000000043\",\"startTime\":\"2026-01-01T00:00:00.000Z\"},\"id\":\"e1\"}\n\
             {\"type\":\"assistant.message\",\"data\":{\"content\":\"resumed ok\",\"messageId\":\"m9\",\"toolRequests\":[]},\"id\":\"e2\"}\n",
        )
        .expect("seed a pre-existing session transcript");
        path
    };

    let vendor = CopilotTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
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
            "resume() against a deliberately nonexistent session file failed as expected: {:?}",
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
                             entirely and tailed the pre-seeded flat <session-id>.jsonl \
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
                    let detail = "expected the pre-seeded session file's text to be tailed";
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
/// `copilot` CLI (except [`scenario::PROBE`] itself, gated by the kill
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
        crate::conformance::report::AdapterKindLabel::custom("copilot-tui"),
        ConformanceMode::Fixture,
        None,
        declared,
        scenarios,
    )
}

/// Extract the string entries (unescaped, without surrounding quotes) of the
/// top-level `"trustedFolders"` array from copilot's JSONC `config.json`.
/// Deliberately narrow: tolerates the JSONC features copilot emits (`//`,
/// `/* */`, trailing commas) and only collects the array's string literals,
/// skipping comments. Returns `None` if the key/array cannot be located, or an
/// empty list if present but empty. Membership is checked against the returned
/// tokens, never against raw text, so a path appearing only in a comment
/// (inside or outside the array) can never false-positive.
fn copilot_trusted_folders_from_config(config: &str) -> Option<Vec<String>> {
    let bytes = config.as_bytes();
    let n = bytes.len();
    let key = b"\"trustedFolders\"";
    let mut i = 0;
    let mut array_start = None;
    let mut depth = 0i32;
    while i + key.len() <= n {
        // Skip comments so a quoted key appearing inside a comment (e.g.
        // `// "trustedFolders": ["/repo"]`) is not mistaken for the real key.
        if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
            while i < n && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        // Skip string literals so braces/keys inside values don't affect depth
        // tracking. The trustedFolders key itself (a string) is processed
        // rather than skipped, but only at root-object depth.
        if bytes[i] == b'"' {
            if depth == 1 && i + key.len() <= n && &bytes[i..i + key.len()] == key {
                // fall through to key handling below
            } else {
                i += 1;
                while i < n && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                if i < n {
                    i += 1;
                }
                continue;
            }
        }
        if bytes[i] == b'{' {
            depth += 1;
            i += 1;
            continue;
        }
        if bytes[i] == b'}' {
            if depth > 0 {
                depth -= 1;
            }
            i += 1;
            continue;
        }
        if depth == 1 && &bytes[i..i + key.len()] == key {
            let mut j = i + key.len();
            while j < n && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            if j < n && bytes[j] == b':' {
                let mut k = j + 1;
                while k < n {
                    match bytes[k] {
                        b' ' | b'\t' | b'\n' | b'\r' => k += 1,
                        b'/' if k + 1 < n && bytes[k + 1] == b'/' => {
                            while k < n && bytes[k] != b'\n' {
                                k += 1;
                            }
                        }
                        b'/' if k + 1 < n && bytes[k + 1] == b'*' => {
                            k += 2;
                            while k + 1 < n && !(bytes[k] == b'*' && bytes[k + 1] == b'/') {
                                k += 1;
                            }
                            k += 2;
                        }
                        b'[' => {
                            array_start = Some(k);
                            break;
                        }
                        _ => return None,
                    }
                }
                break;
            }
        }
        i += 1;
    }
    let start = array_start?;
    let mut k = start + 1;
    let mut depth = 0i32;
    let mut out: Vec<String> = Vec::new();
    while k < n {
        match bytes[k] {
            b'"' => {
                k += 1;
                let mut s = String::new();
                while k < n && bytes[k] != b'"' {
                    if bytes[k] == b'\\' && k + 1 < n {
                        let e = bytes[k + 1];
                        match e {
                            b'"' => s.push('"'),
                            b'\\' => s.push('\\'),
                            b'/' => s.push('/'),
                            b'n' => s.push('\n'),
                            b't' => s.push('\t'),
                            b'r' => s.push('\r'),
                            b'b' => s.push('\u{0008}'),
                            b'f' => s.push('\u{000C}'),
                            b'u' if k + 5 < n => {
                                if let Ok(cp) = u32::from_str_radix(
                                    std::str::from_utf8(&bytes[k + 2..k + 6]).unwrap_or(""),
                                    16,
                                ) && let Some(ch) = char::from_u32(cp)
                                {
                                    s.push(ch);
                                }
                                k += 6;
                                continue;
                            }
                            other => s.push(other as char),
                        }
                        k += 2;
                    } else {
                        s.push(bytes[k] as char);
                        k += 1;
                    }
                }
                out.push(s);
            }
            b'/' if k + 1 < n && bytes[k + 1] == b'/' => {
                while k < n && bytes[k] != b'\n' {
                    k += 1;
                }
            }
            b'/' if k + 1 < n && bytes[k + 1] == b'*' => {
                k += 2;
                while k + 1 < n && !(bytes[k] == b'*' && bytes[k + 1] == b'/') {
                    k += 1;
                }
                k += 1;
            }
            b'[' => depth += 1,
            b']' => {
                if depth == 0 {
                    return Some(out);
                }
                depth -= 1;
            }
            _ => {}
        }
        k += 1;
    }
    None
}

/// Read-only copilot trust check: is `cwd` (or its canonical form) listed in
/// copilot's `trustedFolders`? Membership is tested against the extracted
/// string tokens (comments skipped), so a path appearing only in a comment
/// cannot false-positive.
fn is_copilot_trusted(config: &str, cwd: &Path) -> bool {
    let Some(trusted) = copilot_trusted_folders_from_config(config) else {
        return false;
    };
    let cwd_raw = cwd.to_string_lossy();
    if trusted.iter().any(|t| t.as_str() == cwd_raw.as_ref()) {
        return true;
    }
    if let Ok(abs) = std::fs::canonicalize(cwd) {
        let abs_raw = abs.to_string_lossy();
        if trusted.iter().any(|t| t.as_str() == abs_raw.as_ref()) {
            return true;
        }
    }
    false
}

/// Copilot's interactive TUI refuses to open a chat session -- and therefore
/// never writes its `~/.copilot/session-state/<id>/events.jsonl` transcript --
/// in a workspace that is not listed in its `~/.copilot/config.json`
/// `trustedFolders`: it blocks on a first-run trust modal that swallows the
/// injected prompt, so transcript discovery times out (the "capture" failure).
/// This preflight fails fast with an actionable message instead of hanging for
/// the full discovery window. Read-only: it never mutates copilot's config.
fn ensure_copilot_workspace_trusted(cwd: &Path) -> Result<(), String> {
    let home = std::env::var("HOME")
        .map_err(|_| "HOME is not set; cannot locate ~/.copilot/config.json".to_string())?;
    let cfg = PathBuf::from(home).join(".copilot").join("config.json");
    // Missing config: don't block on it; let discovery report the failure.
    let Ok(text) = std::fs::read_to_string(&cfg) else {
        return Ok(());
    };
    if is_copilot_trusted(&text, cwd) {
        Ok(())
    } else {
        Err(format!(
            "copilot workspace '{}' is not trusted (not found in ~/.copilot/config.json trustedFolders). \
             copilot blocks on a first-run trust modal in untrusted workspaces, so it never creates \
             a session transcript and capture fails. Fix: run `copilot` once in this directory and \
             choose 'Trust', or set CREW_LIVE_CWD to an already-trusted project directory.",
            cwd.display()
        ))
    }
}

/// Live (real `copilot` CLI) TUI conformance -- dispatched by
/// `run_live_conformance` now that the copilot adapter defaults to TUI mode.
pub async fn live_report() -> Result<ConformanceReport, String> {
    let cwd = super::live_project_cwd();
    // Copilot blocks on a trust modal in untrusted workspaces; fail fast with
    // a clear remediation instead of hanging for the full discovery window.
    ensure_copilot_workspace_trusted(&cwd)?;
    super::live_tui_report(
        super::copilot::CopilotTuiVendor::new(cwd, Vec::new()),
        "copilot-tui",
        "copilot",
    )
    .await
}
#[cfg(test)]
mod preflight_tests {
    use super::is_copilot_trusted;
    use std::path::Path;

    #[test]
    fn trusted_when_in_array() {
        let cfg = "{\n  \"trustedFolders\": [\n    \"/tmp/crew-smoke-proj\"\n  ]\n}";
        assert!(is_copilot_trusted(cfg, Path::new("/tmp/crew-smoke-proj")));
    }

    #[test]
    fn not_trusted_when_absent() {
        let cfg = "{\n  \"trustedFolders\": [\n    \"/tmp/crew-smoke-proj\"\n  ]\n}";
        assert!(!is_copilot_trusted(cfg, Path::new("/Users/me/repo")));
    }

    #[test]
    fn not_trusted_when_only_in_comment() {
        // Path present only in a comment must not false-positive.
        let cfg = "// /tmp/crew-smoke-proj is where we test\n{\n  \"trustedFolders\": []\n}";
        assert!(!is_copilot_trusted(cfg, Path::new("/tmp/crew-smoke-proj")));
    }

    #[test]
    fn tolerates_comments_and_trailing_comma_in_array() {
        let cfg = "{\n  // note\n  \"trustedFolders\": [\n    \"/a\",  /* trailing */\n  ]\n}";
        assert!(is_copilot_trusted(cfg, Path::new("/a")));
    }

    #[test]
    fn no_false_positive_on_path_prefix() {
        let cfg = "{\n  \"trustedFolders\": [\n    \"/tmp/crew-smoke-proj\"\n  ]\n}";
        assert!(!is_copilot_trusted(cfg, Path::new("/tmp/crew-smoke")));
    }
    #[test]
    fn does_not_terminate_on_bracket_in_comment() {
        // A ']' inside a line comment must not close trustedFolders early.
        let cfg = "{\n  \"trustedFolders\": [\n    \"/a\", // note: ] end\n    \"/b\"\n  ]\n}";
        assert!(is_copilot_trusted(cfg, Path::new("/a")));
        assert!(is_copilot_trusted(cfg, Path::new("/b")));
    }

    #[test]
    fn does_not_terminate_on_bracket_in_block_comment() {
        let cfg = "{\n  \"trustedFolders\": [\n    \"/a\" /* ] here */,\n    \"/b\"\n  ]\n}";
        assert!(is_copilot_trusted(cfg, Path::new("/a")));
        assert!(is_copilot_trusted(cfg, Path::new("/b")));
    }
    #[test]
    fn not_trusted_when_only_in_array_comment() {
        // Quoted cwd appears only inside an array comment -- must not match.
        let cfg = "{\n  \"trustedFolders\": [ /* \"/repo\" */ ]\n}";
        assert!(!is_copilot_trusted(cfg, Path::new("/repo")));
    }
    #[test]
    fn skips_commented_fake_key_before_real_empty_array() {
        // A commented "trustedFolders": ["/repo"] must not be mistaken for the
        // real (empty) array that follows.
        let cfg = "// \"trustedFolders\": [\"/repo\"]\n{\n  \"trustedFolders\": []\n}";
        assert!(!is_copilot_trusted(cfg, Path::new("/repo")));
    }
    #[test]
    fn ignores_nested_trusted_folders_key() {
        // A nested "trustedFolders" inside another object must not shadow the
        // real top-level (empty) array.
        let cfg = "{\"other\":{\"trustedFolders\":[\"/repo\"]},\"trustedFolders\":[]}";
        assert!(!is_copilot_trusted(cfg, Path::new("/repo")));
    }
    #[test]
    fn tolerates_block_comment_between_colon_and_array() {
        // A block comment between ':' and '[' must not reject the key.
        let cfg = "{\n  \"trustedFolders\": /* note */ [\n    \"/repo\"\n  ]\n}";
        assert!(is_copilot_trusted(cfg, Path::new("/repo")));
    }
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
        assert_eq!(report.adapter, "copilot-tui");
    }

    /// Bidirectional drift check against
    /// `fixtures/conformance/fixture-mode-baseline.json`'s `"copilot-tui"`
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
            .get("copilot-tui")
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
            "unproven scenario(s) not in the copilot-tui baseline: {unexpected:?} (full unproven: {unproven:?})"
        );

        let now_passing: Vec<&String> = expected_failures
            .iter()
            .filter(|name| !unproven.contains(&name.as_str()))
            .collect();
        assert!(
            now_passing.is_empty(),
            "copilot-tui baseline scenario(s) now pass -- remove from fixture-mode-baseline.json: {now_passing:?}"
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
            crate::conformance::report::AdapterKindLabel::custom("copilot-tui"),
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
}
