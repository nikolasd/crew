//! Integration tests for the vendor-agnostic TUI adapter shell
//! ([`crew_runtime::adapter::TuiAdapter`]) against a mock vendor: a real
//! `/bin/sh` script on a real PTY (no fake process below the PTY
//! boundary) paired with a fake JSONL transcript format, so the whole
//! spawn -> attach -> pane -> readiness -> inject -> discover -> tail
//! pipeline is exercised end to end without any real vendor CLI.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::broadcast;

use crew_protocol::{
    Classified, ContentClass, DisplayBackend, DisplayConfig, DisplayPlacement, EventEnvelope,
    ProjectId, RunId, RuntimeEvent, RuntimeEventKind, TaskId, WorkerId,
};
use crew_runtime::adapter::tui::{
    Cursor, LaunchSpec, ResumeContext, TranscriptFormat, TuiAdapter, TuiEvent, TuiTimings,
    TuiVendor, VersionVerdict, parse_jsonl_chunk,
};
use crew_runtime::adapter::{
    Adapter, AdapterEvent, AdapterEventPayload, AdapterEventSink, AdapterFuture, AdapterMessage,
    CancelScope, StartSpec, VendorSessionRef,
};
use crew_runtime::config::crew::{AdapterConfig, AdapterMode, CloseOnExit, PermissionMode};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::display::{
    DisplayBackendTrait, DisplayFuture, DisplayRegistry, HiddenDisplay, PaneCoordinator,
    PaneHandle, PaneRequest,
};
use crew_runtime::supervisor::EscalationTimings;

// ------------------------------------------------------------- test sink

/// Records every emitted event (not just its payload -- resume tests
/// assert on the ids it was stamped with), in order. Mirrors
/// `CodexAdapter`'s own `RecordingSink` test fixture.
struct RecordingSink(StdMutex<Vec<AdapterEvent>>);

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self(StdMutex::new(Vec::new())))
    }

    fn events(&self) -> Vec<AdapterEvent> {
        self.0
            .lock()
            .expect("recording sink mutex never poisoned")
            .clone()
    }

    fn payloads(&self) -> Vec<AdapterEventPayload> {
        self.events().into_iter().map(|e| e.payload).collect()
    }
}

impl AdapterEventSink for RecordingSink {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
        let mut guard = self.0.lock().expect("recording sink mutex never poisoned");
        guard.push(event);
        let sequence = guard.len() as u64;
        drop(guard);
        Box::pin(async move { Ok(sequence) })
    }
}

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

fn kind_of(payload: &AdapterEventPayload) -> &'static str {
    match payload {
        AdapterEventPayload::ProcessStarted { .. } => "ProcessStarted",
        AdapterEventPayload::ProcessExited { .. } => "ProcessExited",
        AdapterEventPayload::VendorSessionEstablished { .. } => "VendorSessionEstablished",
        AdapterEventPayload::MessageChunk { .. } => "MessageChunk",
        AdapterEventPayload::MessageFinal { .. } => "MessageFinal",
        AdapterEventPayload::ToolStarted { .. } => "ToolStarted",
        AdapterEventPayload::ToolProgress { .. } => "ToolProgress",
        AdapterEventPayload::ToolResult { .. } => "ToolResult",
        AdapterEventPayload::UsageReported { .. } => "UsageReported",
        AdapterEventPayload::ArtifactProduced { .. } => "ArtifactProduced",
        AdapterEventPayload::ProtocolHealthChanged { .. } => "ProtocolHealthChanged",
        AdapterEventPayload::NestedWorkerObserved { .. } => "NestedWorkerObserved",
        AdapterEventPayload::QuestionDetected { .. } => "QuestionDetected",
        AdapterEventPayload::OutOfBandInput { .. } => "OutOfBandInput",
    }
}

// -------------------------------------------------------------- fake pane

/// A pane backend that always succeeds, recording every create/close
/// call -- mirrors `PaneCoordinator`'s own private test fixture,
/// duplicated here since that one is not exported.
struct FakeBackend {
    wire_backend: DisplayBackend,
    pane_ref: String,
}

impl FakeBackend {
    fn new(pane_ref: &str) -> Self {
        Self {
            wire_backend: DisplayBackend::Tmux,
            pane_ref: pane_ref.to_string(),
        }
    }
}

impl DisplayBackendTrait for FakeBackend {
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
        crew_protocol::DisplayStatus::new(self.wire_backend, true, false)
    }

    fn create_pane(&self, _req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
        let handle = PaneHandle {
            backend: self.wire_backend,
            pane_ref: self.pane_ref.clone(),
        };
        Box::pin(async move { Ok(handle) })
    }

    fn close_pane(&self, _handle: &PaneHandle) -> DisplayFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// Everything one test needs: a real (temp-file) database, a
/// `PaneCoordinator` resolving to a `FakeBackend` every time, and the
/// pane coordinator's own event broadcast so pane attach/detach can be
/// asserted on directly.
struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<DatabaseHandle>,
    pane_coordinator: Arc<PaneCoordinator>,
    pane_events_rx: broadcast::Receiver<EventEnvelope>,
    fake_backend: Arc<FakeBackendHandle>,
    panes_dir: PathBuf,
}

/// `PaneCoordinator`'s registry takes ownership of the backend; this
/// struct lets the test still observe `close_call_count()` through a
/// second, shared `Arc` the registered backend forwards into.
struct FakeBackendHandle {
    close_calls: Arc<StdMutex<usize>>,
}

struct ForwardingBackend {
    inner: FakeBackend,
    close_calls: Arc<StdMutex<usize>>,
    /// Artificial delay before `create_pane` resolves -- used only by
    /// the readiness-gate-vs-slow-pane-attach regression test, to widen
    /// the window between the PTY spawning and the readiness gate
    /// actually starting to read from it.
    create_pane_delay: Duration,
}

impl DisplayBackendTrait for ForwardingBackend {
    fn backend_name(&self) -> &str {
        self.inner.backend_name()
    }
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }
    fn activate(&mut self) -> Result<(), String> {
        self.inner.activate()
    }
    fn status(&self) -> crew_protocol::DisplayStatus {
        self.inner.status()
    }
    fn create_pane(&self, req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
        let delay = self.create_pane_delay;
        let inner_future = self.inner.create_pane(req);
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            inner_future.await
        })
    }
    fn close_pane(&self, handle: &PaneHandle) -> DisplayFuture<'_, ()> {
        *self.close_calls.lock().expect("mutex never poisoned") += 1;
        self.inner.close_pane(handle)
    }
}

async fn harness() -> Harness {
    harness_with_pane_delay(Duration::ZERO).await
}

async fn harness_with_pane_delay(create_pane_delay: Duration) -> Harness {
    let dir = tempfile::Builder::new()
        .prefix("bat-tui-adapter-")
        .tempdir_in("/tmp")
        .expect("create temp dir");
    let db_path = dir.path().join("state.db");
    let db = Arc::new(
        DatabaseHandle::start(db_path)
            .await
            .expect("start database"),
    );

    let close_calls = Arc::new(StdMutex::new(0usize));
    let mut registry = DisplayRegistry::new();
    registry.register(Box::new(ForwardingBackend {
        inner: FakeBackend::new("fake-pane-1"),
        close_calls: Arc::clone(&close_calls),
        create_pane_delay,
    }));
    registry.register(Box::new(HiddenDisplay::new(DisplayConfig::default())));

    let (events_tx, pane_events_rx) = broadcast::channel(64);
    let panes_dir = dir.path().join("panes");
    fs::create_dir_all(&panes_dir).expect("create panes dir");

    let pane_coordinator = Arc::new(PaneCoordinator::new(
        Arc::new(registry),
        Arc::clone(&db),
        ProjectId::new(),
        events_tx,
        PathBuf::from("/opt/crew/crewd"),
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
    ));

    Harness {
        _dir: dir,
        db,
        pane_coordinator,
        pane_events_rx,
        fake_backend: Arc::new(FakeBackendHandle { close_calls }),
        panes_dir,
    }
}

impl Harness {
    async fn shutdown(self) {
        self.db.shutdown().await.expect("shutdown database");
    }
}

// ---------------------------------------------------------- mock vendor

/// A JSONL transcript line understood by [`MockFormat`]:
/// `{"type":"assistant"|"tool"|"session", ...}`; anything else degrades
/// to `Raw`, exactly like a real vendor format would for an entry it
/// does not recognize.
struct MockFormat;

impl TranscriptFormat for MockFormat {
    fn parse(&self, raw: &[u8], cursor: &Cursor) -> (Vec<TuiEvent>, Cursor) {
        parse_jsonl_chunk(raw, cursor, |value| {
            let entry_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let events = match entry_type {
                "assistant" => {
                    let text = value.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let is_question = value
                        .get("question")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    vec![TuiEvent::AssistantText {
                        text: Classified {
                            class: ContentClass::Visible,
                            value: text.to_string(),
                        },
                        is_question,
                        ts: None,
                    }]
                }
                "tool" => {
                    let tool = value.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                    let detail = value.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                    vec![TuiEvent::ToolActivity {
                        tool: tool.to_string(),
                        detail: Classified {
                            class: ContentClass::Visible,
                            value: detail.to_string(),
                        },
                        ts: None,
                    }]
                }
                "session" => {
                    let id = value.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    vec![TuiEvent::SessionMeta {
                        vendor_session_id: id.to_string(),
                    }]
                }
                other => vec![TuiEvent::Raw {
                    entry_type: other.to_string(),
                }],
            };
            (events, None)
        })
    }
}

/// Which mock CLI script variant [`MockTuiVendor::launch`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MockScript {
    /// Prints a ready line, goes quiet, then on receiving the injected
    /// line (nonce included) writes a full mock transcript.
    Reactive,
    /// Prints a ready line, then never writes anything to the
    /// transcript regardless of what it receives (drives the discovery-
    /// failure test).
    Silent,
    /// Prints several bursts of output before going quiet, to exercise
    /// the readiness gate's quiet-window timing.
    Bursty,
    /// Traps SIGINT/SIGTERM as no-ops, so `cancel(Worker)` must escalate
    /// all the way to SIGKILL -- exercises signal-preserving exit
    /// evidence.
    Stubborn,
}

struct MockTuiVendor {
    session_dir: PathBuf,
    control_log: PathBuf,
    scripts_dir: PathBuf,
    script: MockScript,
    version: String,
}

impl MockTuiVendor {
    fn new(work_dir: &Path, script: MockScript) -> Self {
        let session_dir = work_dir.join("session");
        fs::create_dir_all(&session_dir).expect("create session dir");
        let vendor = Self {
            session_dir,
            control_log: work_dir.join("control.log"),
            scripts_dir: work_dir.to_path_buf(),
            script,
            version: "mock-1.0.0".to_string(),
        };
        vendor.write_script();
        vendor
    }

    fn transcript_path(&self) -> PathBuf {
        self.session_dir.join("session.jsonl")
    }

    fn script_path(&self) -> PathBuf {
        self.scripts_dir.join(script_file_name(self.script))
    }

    fn write_script(&self) {
        let body = match self.script {
            MockScript::Reactive => REACTIVE_SCRIPT,
            MockScript::Silent => SILENT_SCRIPT,
            MockScript::Bursty => BURSTY_SCRIPT,
            MockScript::Stubborn => STUBBORN_SCRIPT,
        };
        let path = self.script_path();
        fs::write(&path, body).expect("write mock vendor script");
        let mut perms = fs::metadata(&path).expect("stat script").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("chmod script");
    }
}

fn script_file_name(script: MockScript) -> &'static str {
    match script {
        MockScript::Reactive => "reactive.sh",
        MockScript::Silent => "silent.sh",
        MockScript::Bursty => "bursty.sh",
        MockScript::Stubborn => "stubborn.sh",
    }
}

const REACTIVE_SCRIPT: &str = r#"#!/bin/sh
echo "MOCK VENDOR READY"
while IFS= read -r line; do
  printf '%s %s\n' "$(date +%s%N)" "$line" >> "$CONTROL_LOG"
  case "$line" in
    *"[crew:"*)
      (
        printf '%s\n' "{\"type\":\"user\",\"text\":\"$line\"}" >> "$TRANSCRIPT"
        sleep 0.05
        printf '%s\n' '{"type":"session","id":"sess-mock"}' >> "$TRANSCRIPT"
        sleep 0.05
        printf '%s\n' '{"type":"assistant","text":"got it","question":false}' >> "$TRANSCRIPT"
        sleep 0.05
        printf '%s\n' '{"type":"tool","tool":"bash","detail":"ran ls"}' >> "$TRANSCRIPT"
        sleep 0.05
        printf '%s\n' '{"type":"assistant","text":"continue?","question":true}' >> "$TRANSCRIPT"
      ) &
      ;;
  esac
done
"#;

const SILENT_SCRIPT: &str = r#"#!/bin/sh
echo "MOCK VENDOR READY (silent)"
while IFS= read -r line; do
  printf '%s %s\n' "$(date +%s%N)" "$line" >> "$CONTROL_LOG"
done
"#;

const BURSTY_SCRIPT: &str = r#"#!/bin/sh
i=0
while [ "$i" -lt 4 ]; do
  echo "burst-$i"
  sleep 0.08
  i=$((i + 1))
done
while IFS= read -r line; do
  printf '%s %s\n' "$(date +%s%N)" "$line" >> "$CONTROL_LOG"
done
"#;

const STUBBORN_SCRIPT: &str = r#"#!/bin/sh
trap '' INT TERM
echo "MOCK VENDOR READY (stubborn)"
while IFS= read -r line; do
  printf '%s %s\n' "$(date +%s%N)" "$line" >> "$CONTROL_LOG"
  case "$line" in
    *"[crew:"*)
      printf '%s\n' "{\"type\":\"user\",\"text\":\"$line\"}" >> "$TRANSCRIPT"
      printf '%s\n' '{"type":"session","id":"sess-mock"}' >> "$TRANSCRIPT"
      ;;
  esac
done
"#;

impl TuiVendor for MockTuiVendor {
    fn kind(&self) -> &'static str {
        "mock"
    }

    fn launch(&self, _spec: &StartSpec, _cfg: &AdapterConfig) -> LaunchSpec {
        let mut env = HashMap::new();
        env.insert(
            "TRANSCRIPT".to_string(),
            self.transcript_path().to_string_lossy().into_owned(),
        );
        env.insert(
            "CONTROL_LOG".to_string(),
            self.control_log.to_string_lossy().into_owned(),
        );
        env.insert("PATH".to_string(), "/bin:/usr/bin".to_string());
        LaunchSpec {
            program: self.script_path(),
            args: Vec::new(),
            cwd: self.scripts_dir.clone(),
            env,
        }
    }

    fn resume_launch(
        &self,
        _session: &VendorSessionRef,
        spec: &StartSpec,
        cfg: &AdapterConfig,
    ) -> LaunchSpec {
        self.launch(spec, cfg)
    }

    fn transcript_root(&self, _spec: &StartSpec, _cfg: &AdapterConfig) -> PathBuf {
        self.session_dir.clone()
    }

    /// The mock's own layout: every session's transcript *is*
    /// `<session_dir>/session.jsonl`, whatever the session id -- so this
    /// override is what lets the deterministic-derivation tests below
    /// prove the trait default's contract end-to-end without renaming
    /// files mid-test.
    fn transcript_path_for_session(
        &self,
        _session: &VendorSessionRef,
        spec: &StartSpec,
        cfg: &AdapterConfig,
    ) -> PathBuf {
        let _ = (spec, cfg);
        self.transcript_path()
    }

    fn format(&self) -> Arc<dyn TranscriptFormat> {
        Arc::new(MockFormat)
    }

    fn compose_input(&self, message: &str) -> Vec<u8> {
        format!("{}\n", message.replace('\n', " ")).into_bytes()
    }

    fn interrupt_sequence(&self) -> Vec<u8> {
        b"__INTERRUPT__\n".to_vec()
    }

    fn permission_args(&self, _mode: PermissionMode) -> Vec<String> {
        Vec::new()
    }

    fn version_gate(&self, probed: &str) -> VersionVerdict {
        if probed == self.version {
            VersionVerdict::Compatible
        } else {
            VersionVerdict::Incompatible {
                detail: format!("unexpected mock vendor version: {probed}"),
            }
        }
    }
}

fn fast_timings() -> TuiTimings {
    TuiTimings {
        readiness_quiet: Duration::from_millis(60),
        readiness_cap: Duration::from_secs(4),
        discovery_timeout: Duration::from_secs(4),
        tailer_poll: Duration::from_millis(40),
        escalation: EscalationTimings {
            sigint_to_sigterm: Duration::from_millis(150),
            sigterm_to_sigkill: Duration::from_millis(150),
        },
    }
}

fn adapter_config() -> AdapterConfig {
    AdapterConfig {
        enabled: true,
        bin: "mock".to_string(),
        mode: AdapterMode::Tui,
        permission_mode: PermissionMode::Default,
        model: None,
        profile: "default".to_string(),
        session_dir: None,
        extra_args: Vec::new(),
    }
}

fn spec(run_id: RunId, task_id: TaskId, worker_id: WorkerId, prompt: &str) -> StartSpec {
    StartSpec {
        run_id,
        task_id,
        worker_id,
        prompt: prompt.to_string(),
        resume: None,
    }
}

fn read_control_log(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Builds a `TuiAdapter<MockTuiVendor>` bound to `run_id`/`task_id`/
/// `worker_id` at construction, exactly like production `build_adapter`
/// would for a real vendor -- every test constructs its adapter this way
/// so `resume()`'s ids are never accidentally fresh/fabricated ones.
#[allow(clippy::too_many_arguments)]
fn build_adapter(
    vendor: MockTuiVendor,
    harness: &Harness,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    timings: TuiTimings,
    resume: ResumeContext,
) -> TuiAdapter<MockTuiVendor> {
    TuiAdapter::new(
        vendor,
        adapter_config(),
        run_id,
        task_id,
        worker_id,
        Arc::clone(&harness.pane_coordinator),
        harness.panes_dir.clone(),
        DisplayPlacement::SplitRight,
        None,
        CloseOnExit::Always,
        timings,
        resume,
    )
}

// -------------------------------------------------------------------- tests

#[tokio::test]
async fn full_lifecycle_event_order_question_and_tool_mapping() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();

    adapter
        .start(
            spec(run_id, task_id, worker_id, "hello worker"),
            sink.clone(),
        )
        .await
        .expect("start must succeed against the reactive mock vendor");

    let ready = wait_until(
        || {
            sink.payloads()
                .iter()
                .any(|p| matches!(p, AdapterEventPayload::QuestionDetected { .. }))
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(ready, "expected a QuestionDetected event eventually");

    let kinds: Vec<&'static str> = sink.payloads().iter().map(kind_of).collect();

    assert_eq!(
        kinds.first(),
        Some(&"ProcessStarted"),
        "ProcessStarted must be first: {kinds:?}"
    );
    let first_session = kinds
        .iter()
        .position(|k| *k == "VendorSessionEstablished")
        .expect("a VendorSessionEstablished from nonce discovery");
    assert!(
        first_session > 0,
        "VendorSessionEstablished must follow ProcessStarted: {kinds:?}"
    );

    let tool_started = kinds.iter().position(|k| *k == "ToolStarted");
    let tool_result = kinds.iter().position(|k| *k == "ToolResult");
    match (tool_started, tool_result) {
        (Some(started), Some(result)) => assert!(
            started < result,
            "ToolStarted must precede its condensed ToolResult: {kinds:?}"
        ),
        other => panic!("expected a ToolStarted/ToolResult pair, got {other:?} in {kinds:?}"),
    }

    let message_final = kinds
        .iter()
        .position(|k| *k == "MessageFinal")
        .expect("assistant text with question:false must map to MessageFinal");
    let question = kinds
        .iter()
        .position(|k| *k == "QuestionDetected")
        .expect("assistant text with question:true must map to QuestionDetected");
    assert!(
        message_final < question,
        "the non-question assistant text precedes the question in the mock transcript: {kinds:?}"
    );

    // Cancelling the worker settles the run; the exit watcher (spawned
    // once, at start) must then journal exactly one ProcessExited.
    adapter
        .cancel(CancelScope::Worker)
        .await
        .expect("cancel(Worker)");
    let settled = wait_until(
        || {
            sink.payloads()
                .iter()
                .filter(|p| matches!(p, AdapterEventPayload::ProcessExited { .. }))
                .count()
                == 1
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(settled, "expected exactly one ProcessExited after cancel");

    harness.shutdown().await;
}

#[tokio::test]
async fn readiness_gate_injects_only_after_the_quiet_window() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-bursty-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Bursty);
    let control_log = vendor.control_log.clone();

    let mut timings = fast_timings();
    timings.readiness_quiet = Duration::from_millis(200);
    timings.readiness_cap = Duration::from_secs(3);
    // Give discovery time even though this test never satisfies it --
    // discovery runs after injection and this test does not wait for it.
    timings.discovery_timeout = Duration::from_millis(300);

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        timings,
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();

    let start = tokio::time::Instant::now();
    // Discovery will time out (the bursty script never reacts to
    // input), so `start()` itself returns `Err` -- this test only cares
    // about *when* the injected line was written, which the failure
    // path still does before discovery even begins.
    let _ = adapter
        .start(spec(run_id, task_id, worker_id, "hello"), sink.clone())
        .await;

    // Generous ceiling: CI runners run this suite alongside dozens of
    // other tests, and tokio timer lag under that load can push a correct
    // quiet-window injection well past any tight bound (observed >5s on a
    // macos-latest runner). A genuine gating regression still fails fast
    // in the elapsed assertions below; this wait only bounds the poll.
    let injected = wait_until(
        || !read_control_log(&control_log).trim().is_empty(),
        Duration::from_secs(30),
    )
    .await;
    assert!(injected, "the composed prompt must eventually be written");
    let elapsed = start.elapsed();

    // Four 80ms bursts plus a 200ms quiet window is ~520ms; injecting
    // right after the first burst (an ungated readiness check) would
    // land near 0ms, and waiting out the whole cap would land near 3s.
    // A wide, CI-tolerant window around the true value distinguishes
    // both failure modes from correct gating.
    assert!(
        elapsed >= Duration::from_millis(350),
        "injection happened too early to have waited for quiet: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(2500),
        "injection happened too late -- looks like it waited out the whole cap: {elapsed:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn discovery_failure_fails_the_run_tears_down_the_pty_and_closes_the_pane() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-silent-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Silent);

    let mut timings = fast_timings();
    timings.discovery_timeout = Duration::from_millis(250);

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        timings,
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();

    let result = adapter
        .start(spec(run_id, task_id, worker_id, "hello"), sink.clone())
        .await;

    let err = result.expect_err("discovery must time out against the silent mock vendor");
    assert_eq!(
        err.code(),
        "process",
        "expected a typed process failure: {err}"
    );

    let payloads = sink.payloads();
    let pid = payloads.iter().find_map(|p| match p {
        AdapterEventPayload::ProcessStarted { pid } => Some(*pid),
        _ => None,
    });
    assert!(
        pid.is_some(),
        "ProcessStarted must have been journaled: {payloads:?}"
    );
    assert!(
        payloads
            .iter()
            .any(|p| matches!(p, AdapterEventPayload::ProcessExited { .. })),
        "a typed ProcessExited must be journaled on discovery failure: {payloads:?}"
    );

    // The pty must actually be dead: signal-0 existence probe fails.
    let pid = pid.expect("checked above");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pty child must be torn down after a discovery failure"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        harness.fake_backend.close_calls_taken(),
        1,
        "the pane opened for this run must be closed exactly once on discovery failure"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn send_writes_the_composed_bytes_to_the_pty() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-send-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let control_log = vendor.control_log.clone();
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();

    adapter
        .start(spec(run_id, task_id, worker_id, "hello"), sink.clone())
        .await
        .expect("start");

    adapter
        .send(AdapterMessage::FollowUp {
            text: "do-more-work".to_string(),
        })
        .await
        .expect("send");

    let seen = wait_until(
        || read_control_log(&control_log).contains("do-more-work"),
        Duration::from_secs(5),
    )
    .await;
    assert!(seen, "the composed follow-up text must reach the pty");

    let journaled = sink.payloads().into_iter().any(|p| {
        matches!(
            p,
            AdapterEventPayload::MessageChunk { role, text }
                if role == "user" && text.value == "do-more-work"
        )
    });
    assert!(
        journaled,
        "send must journal the user's own text through the sink"
    );

    adapter.dispose().await.expect("dispose");
    harness.shutdown().await;
}

#[tokio::test]
async fn cancel_turn_writes_the_interrupt_sequence_to_the_pty() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-interrupt-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let control_log = vendor.control_log.clone();
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();

    adapter
        .start(spec(run_id, task_id, worker_id, "hello"), sink.clone())
        .await
        .expect("start");

    adapter
        .cancel(CancelScope::Turn)
        .await
        .expect("cancel(Turn)");

    let seen = wait_until(
        || read_control_log(&control_log).contains("__INTERRUPT__"),
        Duration::from_secs(5),
    )
    .await;
    assert!(seen, "the interrupt sequence must reach the pty");

    adapter.dispose().await.expect("dispose");
    harness.shutdown().await;
}

#[tokio::test]
async fn cancel_turn_with_no_active_run_is_a_no_op_success() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-noop-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let adapter = build_adapter(
        vendor,
        &harness,
        RunId::new(),
        TaskId::new(),
        WorkerId::new(),
        fast_timings(),
        ResumeContext::default(),
    );

    adapter
        .cancel(CancelScope::Turn)
        .await
        .expect("cancel with nothing to interrupt is a no-op, not a kill failure");

    harness.shutdown().await;
}

#[tokio::test]
async fn pane_attach_is_journaled_with_the_real_pane_ref_from_the_fake_backend() {
    let mut harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-pane-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();

    adapter
        .start(spec(run_id, task_id, worker_id, "hello"), sink.clone())
        .await
        .expect("start");

    let envelope = tokio::time::timeout(Duration::from_secs(5), harness.pane_events_rx.recv())
        .await
        .expect("a pane attach must broadcast promptly")
        .expect("pane events channel must stay open");

    match envelope.event {
        RuntimeEvent::DisplayEvent {
            kind,
            backend,
            pane_ref,
            ..
        } => {
            assert_eq!(kind, RuntimeEventKind::DisplayPaneAttached);
            assert_eq!(backend, DisplayBackend::Tmux);
            assert_eq!(
                pane_ref, "fake-pane-1",
                "must be the real ref, never a placeholder"
            );
        }
        other => panic!("expected a DisplayEvent, got {other:?}"),
    }

    adapter.dispose().await.expect("dispose");
    harness.shutdown().await;
}

#[tokio::test]
async fn out_of_band_input_is_journaled_when_a_viewer_types_into_the_attached_pane() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-oob-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();

    adapter
        .start(spec(run_id, task_id, worker_id, "hello"), sink.clone())
        .await
        .expect("start");

    let socket_path = harness.panes_dir.join(format!("{run_id}.sock"));
    let mut stream = wait_for_socket(&socket_path, Duration::from_secs(5))
        .await
        .expect("attach socket must be bound");
    stream
        .write_all(b"TYPED_BY_A_HUMAN_VIEWER\n")
        .await
        .expect("write viewer keystrokes");

    let seen = wait_until(
        || {
            sink.payloads()
                .iter()
                .any(|p| matches!(p, AdapterEventPayload::OutOfBandInput { .. }))
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(seen, "a viewer keystroke must journal OutOfBandInput");

    // The payload itself structurally carries no free text (only
    // backend/pane_ref) -- pinned here so the property is not just an
    // artifact of the enum's current shape.
    let backend_and_ref = sink.payloads().into_iter().find_map(|p| match p {
        AdapterEventPayload::OutOfBandInput { backend, pane_ref } => Some((backend, pane_ref)),
        _ => None,
    });
    let (backend, pane_ref) = backend_and_ref.expect("checked above");
    assert_eq!(backend, DisplayBackend::Tmux);
    assert_eq!(pane_ref, "fake-pane-1");

    adapter.dispose().await.expect("dispose");
    harness.shutdown().await;
}

#[tokio::test]
async fn resume_journals_under_constructor_ids_with_no_injection_and_no_discovery_when_a_transcript_path_is_provided()
 {
    let mut harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-resume-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let control_log = vendor.control_log.clone();

    // A pre-existing transcript, as if from a previously established
    // session -- seeded *outside* `vendor.transcript_root()` (the mock's
    // `session_dir`, which discovery would scan) entirely, and
    // `transcript_root()` itself is left with no matching file at all.
    // So this is not merely "discovery finding it would be a
    // coincidence" -- discovery structurally *cannot* find it: were
    // `resume_transcript_path` ever ignored in favor of discovery, the
    // scan would come up empty against an untouched `session_dir` and
    // `resume()` would fail outright (a nonce-discovery timeout) rather
    // than quietly happening to tail the right file anyway. A passing
    // test is therefore proof the known path was used directly, not a
    // behavior that could pass by accident.
    let transcript_path = work_dir.path().join("resumed-elsewhere.jsonl");
    fs::write(
        &transcript_path,
        "{\"type\":\"session\",\"id\":\"sess-mock\"}\n\
         {\"type\":\"assistant\",\"text\":\"resumed ok\",\"question\":false}\n",
    )
    .expect("seed a pre-existing transcript");
    assert_eq!(
        fs::read_dir(vendor.transcript_root(
            &spec(RunId::new(), TaskId::new(), WorkerId::new(), ""),
            &adapter_config()
        ))
        .expect("session dir must exist")
        .count(),
        0,
        "transcript_root must stay empty -- discovery must have nothing to coincidentally find"
    );

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext {
            transcript_path: Some(transcript_path),
            cursor: None,
        },
    );

    let sink = RecordingSink::new();
    adapter
        .resume(VendorSessionRef("sess-mock".to_string()), sink.clone())
        .await
        .expect("resume must succeed using the provided transcript path");

    let tailed = wait_until(
        || {
            sink.payloads()
                .iter()
                .any(|p| matches!(p, AdapterEventPayload::MessageFinal { .. }))
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(tailed, "expected the pre-seeded transcript to be tailed");

    // Every event is stamped with the constructor-bound ids, never a
    // freshly fabricated run/task/worker -- `resume()` has no
    // `StartSpec` of its own to read them from.
    for event in sink.events() {
        assert_eq!(
            event.run_id, run_id,
            "event stamped with a foreign run_id: {event:?}"
        );
        assert_eq!(
            event.task_id, task_id,
            "event stamped with a foreign task_id: {event:?}"
        );
        assert_eq!(
            event.worker_id, worker_id,
            "event stamped with a foreign worker_id: {event:?}"
        );
    }

    // No prompt injection: resume never writes anything to the pty's
    // stdin, so the reactive script's own control log (which only ever
    // logs a line once it has actually been read) must stay empty.
    assert!(
        read_control_log(&control_log).is_empty(),
        "resume must never inject a prompt"
    );

    // No discovery: the transcript's own SessionMeta id ("sess-mock")
    // was still tailed correctly even though the vendor process itself
    // never touched the file -- proof the known path was used directly.
    let confirmed = sink.payloads().iter().any(|p| {
        matches!(
            p,
            AdapterEventPayload::VendorSessionEstablished { vendor_session_id }
                if vendor_session_id == "sess-mock"
        )
    });
    assert!(
        confirmed,
        "the pre-seeded transcript's own session id must have been tailed"
    );

    // Pane reopened: a real (fake-backend) pane attach happened for this
    // resume, exactly like a fresh start.
    let envelope = tokio::time::timeout(Duration::from_secs(5), harness.pane_events_rx.recv())
        .await
        .expect("a pane attach must broadcast promptly for a resume too")
        .expect("pane events channel must stay open");
    assert!(
        matches!(
            envelope.event,
            RuntimeEvent::DisplayEvent {
                kind: RuntimeEventKind::DisplayPaneAttached,
                ..
            }
        ),
        "expected a DisplayPaneAttached event, got {:?}",
        envelope.event
    );

    adapter.dispose().await.expect("dispose");
    harness.shutdown().await;
}

// ------------------------------------------------------------------ WP14

#[tokio::test]
async fn resume_without_a_known_path_derives_the_transcript_from_the_vendor_root_and_session_id() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-derive-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let control_log = vendor.control_log.clone();

    // Pre-existing transcript AT the vendor's own deterministic layout
    // (`transcript_root()/session.jsonl` via the mock's override), written
    // long enough ago that nonce discovery structurally cannot find it:
    // `find_transcript_by_nonce` only scans files touched after the
    // discovery window opened. A passing test is therefore proof the path
    // was *derived* from `transcript_root()` + the session id (the WP11
    // honest gap, closed in WP14), not that discovery happened to win a
    // race it cannot even enter.
    let transcript = vendor.transcript_path();
    fs::write(
        &transcript,
        "{\"type\":\"session\",\"id\":\"sess-mock\"}\n\
         {\"type\":\"assistant\",\"text\":\"derived path works\",\"question\":false}\n",
    )
    .expect("seed a pre-existing transcript");

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    // Empty ResumeContext: no known path, no stored cursor -- exactly what
    // a registry that knows only the session id can supply.
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();
    adapter
        .resume(VendorSessionRef("sess-mock".to_string()), sink.clone())
        .await
        .expect("resume must succeed by deriving the transcript path");

    let tailed = wait_until(
        || {
            sink.payloads().iter().any(|p| matches!(
                p,
                AdapterEventPayload::MessageFinal { text, .. } if text.value == "derived path works"
            ))
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(tailed, "the derived transcript must have been tailed");

    // No prompt injection on the derived-path resume either.
    assert!(
        read_control_log(&control_log).is_empty(),
        "resume must never inject a prompt"
    );

    adapter.dispose().await.expect("dispose");
    harness.shutdown().await;
}

#[tokio::test]
async fn resume_tails_from_the_stored_cursor_so_journaled_events_are_not_re_emitted() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-cursor-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);

    // A three-entry pre-crash transcript. Everything up to and including
    // the "first half" entry stands for events the crashed run already
    // journaled (its cursor covered them); only "second half" was never
    // durably consumed.
    let line1 = "{\"type\":\"session\",\"id\":\"sess-mock\"}\n";
    let line2 = "{\"type\":\"assistant\",\"text\":\"first half\",\"question\":false}\n";
    let line3 = "{\"type\":\"assistant\",\"text\":\"second half\",\"question\":false}\n";
    fs::write(vendor.transcript_path(), format!("{line1}{line2}{line3}")).expect("seed transcript");

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext {
            transcript_path: None,
            cursor: Some(Cursor {
                offset: (line1.len() + line2.len()) as u64,
                last_entry_id: None,
            }),
        },
    );

    let sink = RecordingSink::new();
    adapter
        .resume(VendorSessionRef("sess-mock".to_string()), sink.clone())
        .await
        .expect("resume from the stored cursor");

    let settled =
        wait_until(
            || {
                sink.payloads().iter().any(|p| matches!(
                p,
                AdapterEventPayload::MessageFinal { text, .. } if text.value == "second half"
            ))
            },
            Duration::from_secs(5),
        )
        .await;
    assert!(settled, "the post-cursor entry must be tailed");

    // The whole point of the stored cursor: everything at or before the
    // cursor was already durable when the run died and must not be
    // re-journaled.
    assert!(
        !sink.payloads().iter().any(|p| matches!(
            p,
            AdapterEventPayload::MessageFinal { text, .. } if text.value == "first half"
        )),
        "events covered by the stored cursor must never be re-emitted"
    );
    assert_eq!(
        sink.payloads()
            .iter()
            .filter(|p| matches!(p, AdapterEventPayload::MessageFinal { text, .. } if text.value == "second half"))
            .count(),
        1,
        "exactly one emission past the cursor"
    );

    adapter.dispose().await.expect("dispose");
    harness.shutdown().await;
}

#[tokio::test]
async fn start_with_a_resume_spec_continues_the_session_instead_of_injecting() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-specresume-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Reactive);
    let control_log = vendor.control_log.clone();

    // Pre-existing transcript at the vendor's deterministic layout, with an
    // old mtime so discovery cannot find it -- only the derived path works.
    fs::write(
        vendor.transcript_path(),
        "{\"type\":\"session\",\"id\":\"sess-mock\"}\n\
         {\"type\":\"assistant\",\"text\":\"spec resume works\",\"question\":false}\n",
    )
    .expect("seed a pre-existing transcript");

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext::default(),
    );

    // A StartSpec that carries a session ref: the WP14 wiring makes this a
    // continuation, not a fresh launch with a flag. The prompt must never
    // be injected into the continued session.
    let mut spec = spec(run_id, task_id, worker_id, "say hi");
    spec.resume = Some(VendorSessionRef("sess-mock".to_string()));

    let sink = RecordingSink::new();
    adapter
        .start(spec, sink.clone())
        .await
        .expect("start-with-resume must succeed");

    let tailed = wait_until(
        || {
            sink.payloads().iter().any(|p| matches!(
                p,
                AdapterEventPayload::MessageFinal { text, .. } if text.value == "spec resume works"
            ))
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(tailed, "the continued session's transcript must be tailed");

    assert!(
        read_control_log(&control_log).is_empty(),
        "start-with-resume must never inject the prompt"
    );

    adapter.dispose().await.expect("dispose");
    harness.shutdown().await;
}

#[tokio::test]
async fn readiness_gate_sees_output_produced_before_a_slow_pane_attach_resolves() {
    // A pane attach slow enough to expose the bug this guards against:
    // subscribing to the pty's output only *after* this delay would
    // miss output the mock CLI already produced the instant it started,
    // and the readiness gate would then have to wait out its entire cap
    // before giving up.
    let pane_delay = Duration::from_millis(400);
    let harness = harness_with_pane_delay(pane_delay).await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-early-output-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    // Silent: prints its ready line immediately, then never reacts to
    // anything again -- discovery is expected to time out too, which
    // this test uses as part of its own bound on total elapsed time.
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Silent);

    let mut timings = fast_timings();
    timings.readiness_quiet = Duration::from_millis(100);
    timings.readiness_cap = Duration::from_secs(2);
    timings.discovery_timeout = Duration::from_millis(300);

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        timings,
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();
    let start = tokio::time::Instant::now();
    let _ = adapter
        .start(spec(run_id, task_id, worker_id, "hello"), sink.clone())
        .await;
    let elapsed = start.elapsed();

    // Buggy (subscribing only after the slow pane attach): the gate
    // never sees the pre-delay "ready" output and waits out its whole
    // 2s cap before even reaching discovery -- total elapsed would land
    // near pane_delay + readiness_cap + discovery_timeout = 400ms +
    // 2000ms + 300ms ~= 2.7s. Fixed: it lands near pane_delay +
    // readiness_quiet + discovery_timeout = 400ms + 100ms + 300ms ~=
    // 0.8s. The 2200ms threshold is not the midpoint of those two (that
    // would be ~1.75s) -- it sits deliberately closer to the buggy value,
    // trading a slimmer 500ms margin on that side for a generous 1.4s
    // margin on the fixed side, since scheduling jitter under a fully
    // parallel test run is far more likely to *slow down* the fast path
    // than to speed up the slow one.
    assert!(
        elapsed < Duration::from_millis(2200),
        "readiness must have seen the pre-delay output rather than waiting out its cap: {elapsed:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn cancel_worker_preserves_the_real_termination_signal_when_escalated_to_sigkill() {
    let harness = harness().await;
    let work_dir = tempfile::Builder::new()
        .prefix("bat-tui-mock-stubborn-")
        .tempdir_in("/tmp")
        .expect("mock work dir");
    let vendor = MockTuiVendor::new(work_dir.path(), MockScript::Stubborn);

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = build_adapter(
        vendor,
        &harness,
        run_id,
        task_id,
        worker_id,
        fast_timings(),
        ResumeContext::default(),
    );

    let sink = RecordingSink::new();
    adapter
        .start(spec(run_id, task_id, worker_id, "hello"), sink.clone())
        .await
        .expect("start must succeed even against a signal-trapping mock vendor");

    adapter
        .cancel(CancelScope::Worker)
        .await
        .expect("cancel(Worker)");

    let settled = wait_until(
        || {
            sink.payloads()
                .iter()
                .any(|p| matches!(p, AdapterEventPayload::ProcessExited { .. }))
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        settled,
        "expected a ProcessExited after escalating to SIGKILL"
    );

    let exit = sink.payloads().into_iter().find_map(|p| match p {
        AdapterEventPayload::ProcessExited { exit_code, signal } => Some((exit_code, signal)),
        _ => None,
    });
    assert_eq!(
        exit,
        Some((None, Some("SIGKILL".to_string()))),
        "a SIGKILL-escalated exit must preserve the real signal, not fall back to exit_code:1/signal:None"
    );

    harness.shutdown().await;
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(err);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

impl FakeBackendHandle {
    fn close_calls_taken(&self) -> usize {
        *self.close_calls.lock().expect("mutex never poisoned")
    }
}
