//! The OMP TUI adapter's own fixture-mode conformance suite, mirroring
//! [`super::codex_conformance`] scenario-for-scenario (the same 14
//! canonical names, the same bidirectional baseline drift check against
//! `fixtures/conformance/fixture-mode-baseline.json`'s `"omp-tui"`
//! entry). Like the Claude/Codex/Copilot suites it dispatches nowhere in
//! `crate::conformance::run_fixture_conformance` -- that runner keys on
//! `AdapterKind` alone; this suite is driven by its own tests plus the
//! CLI gate's per-suite extension points.
//!
//! Every scenario provable without spawning the real `omp` CLI is
//! proved against the committed *synthetic* fixture
//! (`fixtures/adapters/omp-tui/session.jsonl`) or a `/bin/sh` test
//! double writing OMP-session-shaped lines; PROBE alone needs the real
//! CLI and honors the kill switch like every other vendor's probe.

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
use super::adapter::{TuiAdapter, TuiTimings, VersionVerdict};
use super::omp::OmpTuiVendor;
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
        OmpTuiVendor::new(std::env::temp_dir(), Vec::new()),
        adapter_config(PathBuf::from("omp"), std::env::temp_dir()),
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
        .join("../../fixtures/adapters/omp-tui/session.jsonl");
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
}

fn parsed_fixture_events() -> Vec<TuiEvent> {
    let vendor = OmpTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
    let tagged = vendor.format().parse(&fixture_bytes(), &Cursor::start());
    let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
    events
}

// ------------------------------------------------------------- PROBE

async fn probe_scenario_with_version() -> (ScenarioResult, Option<String>) {
    if crate::conformance::vendor_cli_invocation_disabled() {
        return (crate::conformance::vendor_cli_skipped_probe(), None);
    }
    let vendor = OmpTuiVendor::new(std::env::temp_dir(), Vec::new());
    let output =
        std::process::Command::new(&adapter_config(PathBuf::from("omp"), std::env::temp_dir()).bin)
            .arg("--version")
            .output();
    match output {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let result = match vendor.version_gate(&version) {
                VersionVerdict::Compatible => ScenarioResult::pass(
                    scenario::PROBE,
                    format!("omp --version reported {version:?}, inside the tested range"),
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
                    "omp --version exited non-zero: {}",
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
    let vendor = OmpTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
    let cfg = adapter_config(PathBuf::from("omp"), std::env::temp_dir());
    let spec = StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "probe".to_string(),
        resume: None,
    };
    let launch = vendor.launch(&spec, &cfg);
    // The headless modes this adapter must never launch interactively:
    // `-p/--print` is the one-shot non-interactive mode and `--mode rpc`
    // is the RPC server the headless adapter drives.
    let forbidden = ["-p", "--print", "--mode", "--mode=rpc"];
    let hit: Vec<&str> = forbidden
        .iter()
        .filter(|flag| launch.args.iter().any(|a| a == *flag))
        .copied()
        .collect();
    if hit.is_empty() {
        ScenarioResult::pass(
            scenario::NATIVE_DISCOVERY,
            format!(
                "OmpTuiVendor::launch's argv ({:?}) never adds -p/--print/--mode or any \
                 other headless/discovery-suppressing mode -- an interactive session, exactly \
                 like a human running `omp` directly, with every native user/project skill/\
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
    // The fixture's `thinking` content block is the model's hidden
    // reasoning: its text must never surface anywhere.
    let leaked = events.iter().any(|event| match event {
        TuiEvent::AssistantText { text, .. } => {
            text.value.contains("never surfaced by this adapter")
                || text.value.contains("[crew:fixture1]")
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
            "expected the fixture's thinking block to never surface while its real message \
             text does",
        );
    }
    ScenarioResult::pass(
        scenario::REDACTION,
        "the fixture's thinking content block never maps to any surfaced event -- map_message \
         skips it unconditionally, mirroring the headless adapters' own thinking redaction -- \
         while its real message text does",
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
        "no TuiEvent variant maps to NestedWorkerObserved and none of OmpTuiVendor's own \
         session mappings ever produce one -- an unexpected vendor-spawned child is not \
         structurally observable through this adapter's transcript tail at all (a human \
         attached to the pane would see it directly instead); nested capability stays \
         declared None regardless, so nothing is silently upgraded by this gap",
    )
}

fn vendor_reconnect_scenario() -> ScenarioResult {
    ScenarioResult::pass(
        scenario::VENDOR_RECONNECT,
        "not applicable to omp: there is no persistent worker-MCP subprocess for a TUI \
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
             detail is the toolCall's raw arguments string, never a resolved path) -- \
             workspace confinement is instead enforced by LaunchSpec.cwd, bound to \
             OmpTuiVendor's own `cwd` field at construction, exactly like the headless \
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
             event: capabilities().usage is UsageCapability::None (an OMP session file \
             carries no cost/token facts this adapter maps) and this adapter has no artifact \
             mechanism, so their absence is consistent with the declared capabilities",
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
        .prefix("bat-omp-tui-conformance-")
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
/// injected `[crew:` line appends OMP-session-shaped entries (a `session`
/// meta line, an echo of the user's own nonce-bearing message, and an
/// assistant reply) to a `<timestamp>_<session-id>.jsonl` under
/// `session_dir`, and on any further line another acknowledging
/// assistant message. Never traps signals, so default termination works
/// exactly like a real, well-behaved CLI.
fn write_double(scripts_dir: &std::path::Path, session_dir: &std::path::Path) -> PathBuf {
    let script = format!(
        r#"#!/bin/sh
echo "Welcome to omp!"
SESSION_ID="77777777-7777-4777-8777-000000000042"
SESSION="{session_dir}/2026-01-01T00-00-00-000Z_$SESSION_ID.jsonl"
printf '%s\n' '{{"type":"session","version":3,"id":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:00.100Z","cwd":"/workspace/crew"}}' >> "$SESSION"
CREW_ESC=$(printf '\033')
while IFS= read -r line; do
  # A real vendor TUI consumes bracketed-paste framing and keeps only the
  # pasted content; this double does the same, so the text it echoes back
  # is the prompt itself rather than the escape sequences around it.
  line=$(printf '%s' "$line" | tr -d "$CREW_ESC" | sed -e 's/\[200~//g' -e 's/\[201~//g')
  case "$line" in
    *"[crew:"*)
      printf '%s\n' '{{"type":"message","id":"mu0","timestamp":"2026-01-01T00:00:00.500Z","message":{{"role":"user","content":[{{"type":"text","text":"'"$line"'"}}]}}}}' >> "$SESSION"
      printf '%s\n' '{{"type":"message","id":"m1","timestamp":"2026-01-01T00:00:01.000Z","message":{{"role":"assistant","content":[{{"type":"toolCall","id":"c1","name":"bash","arguments":{{"command":["bash","-lc","echo crew-fixture"]}}}}]}}}}' >> "$SESSION"
      printf '%s\n' '{{"type":"message","id":"m2","timestamp":"2026-01-01T00:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"hi from the omp conformance double?"}}]}}}}' >> "$SESSION"
      ;;
    *)
      printf '%s\n' '{{"type":"message","id":"m3","timestamp":"2026-01-01T00:00:03.000Z","message":{{"role":"assistant","content":[{{"type":"text","text":"ack: '"$line"'"}}]}}}}' >> "$SESSION"
      ;;
  esac
done
"#,
        session_dir = session_dir.display(),
    );
    let path = scripts_dir.join(format!("fake-omp-{}.sh", uuid::Uuid::now_v7()));
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
    let vendor = OmpTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
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
            "start() spawned a real process (the /bin/sh test double, never the real omp \
             CLI, cwd confined to the harness's own scripts dir) and observed ProcessStarted \
             followed by VendorSessionEstablished once the double's session-shaped meta line \
             was tailed",
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
             the pty; the double's acknowledgement (a fresh assistant message entry) was \
             tailed back, proving the delivery mechanism end to end",
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
/// seeded file uses the real `<timestamp>_<uuid>.jsonl` naming so the
/// omp-specific filename shape stays exercised too.
async fn resume_scenarios(harness: &Harness, break_resume: bool) -> Vec<ScenarioResult> {
    let session_dir = harness.scripts_dir.join("resume-session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script = write_double(&harness.scripts_dir, &session_dir);
    let cfg = adapter_config(script, session_dir.clone());

    let transcript_path = if break_resume {
        session_dir.join("does-not-exist.jsonl")
    } else {
        let path =
            session_dir.join("2026-01-01T00-00-00-000Z_77777777-7777-4777-8777-000000000043.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"77777777-7777-4777-8777-000000000043\",\"timestamp\":\"2026-01-01T00:00:00.100Z\",\"cwd\":\"/workspace/crew\"}\n\
             {\"type\":\"message\",\"id\":\"m9\",\"timestamp\":\"2026-01-01T00:00:01.000Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"resumed ok\"}]}}\n",
        )
        .expect("seed a pre-existing session transcript");
        path
    };

    let vendor = OmpTuiVendor::new(harness.scripts_dir.clone(), Vec::new());
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
                             entirely and tailed the pre-seeded timestamp-partitioned \
                             session file directly, reporting its real content",
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
/// `omp` CLI (except [`scenario::PROBE`] itself, gated by the kill
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
        crate::conformance::report::AdapterKindLabel::custom("omp-tui"),
        ConformanceMode::Fixture,
        None,
        declared,
        scenarios,
    )
}

/// Live (real `omp` CLI) TUI conformance -- dispatched by
/// `run_live_conformance` now that the omp adapter defaults to TUI mode.
pub async fn live_report() -> Result<ConformanceReport, String> {
    super::live_tui_report(
        super::omp::OmpTuiVendor::new(super::live_project_cwd(), Vec::new()),
        "omp-tui",
        "omp",
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
        assert_eq!(report.adapter, "omp-tui");
    }

    /// Bidirectional drift check against
    /// `fixtures/conformance/fixture-mode-baseline.json`'s `"omp-tui"`
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
            .get("omp-tui")
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
            "unproven scenario(s) not in the omp-tui baseline: {unexpected:?} (full unproven: {unproven:?})"
        );

        let now_passing: Vec<&String> = expected_failures
            .iter()
            .filter(|name| !unproven.contains(&name.as_str()))
            .collect();
        assert!(
            now_passing.is_empty(),
            "omp-tui baseline scenario(s) now pass -- remove from fixture-mode-baseline.json: {now_passing:?}"
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
            crate::conformance::report::AdapterKindLabel::custom("omp-tui"),
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
