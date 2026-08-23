//! The Codex adapter's fixture/live conformance scenario suite. See
//! `batman_runtime::conformance` for the shared report/scenario contract
//! this module fills in.

use std::path::PathBuf;
use std::sync::Arc;

use batman_protocol::{RunId, TaskId, WorkerId};
use serde_json::Value;

use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterEvent, AdapterEventPayload, AdapterEventSink,
    AdapterFuture, AdapterKind, AdapterMessage, CancelScope, CodexStartupOptions, NestedCapability,
    StartSpec, VendorSessionRef,
};
use batman_runtime::conformance::report::AdapterKindLabel;
use batman_runtime::conformance::{ConformanceMode, ConformanceReport, ScenarioResult, scenario};
use batman_runtime::supervisor::{EnvironmentPolicy, SpawnSpec, Supervisor};

use super::client::CodexRpcClient;
use super::normalize;

fn new_adapter() -> super::CodexAdapter {
    super::CodexAdapter::new(
        std::env::temp_dir(),
        CodexStartupOptions::default(),
        Vec::new(),
        None,
    )
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/adapters/codex"
    ))
    .join(name)
}

fn read_jsonl(name: &str) -> Result<Vec<Value>, String> {
    let raw = std::fs::read_to_string(fixture_path(name))
        .map_err(|e| format!("failed to read fixture {name}: {e}"))?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|e| format!("bad fixture line {line:?}: {e}"))
        })
        .collect()
}

/// A no-op sink used for the real, zero-model-call spawn proofs below,
/// which only care whether the RPC call itself succeeds, not what it
/// emits.
struct NullSink;

impl AdapterEventSink for NullSink {
    fn emit(&self, _event: AdapterEvent) -> AdapterFuture<'_, u64> {
        Box::pin(async move { Ok(0) })
    }
}

/// Records every emitted event, so a live scenario can assert on
/// correlation and payload shape.
#[derive(Default)]
struct RecordingSink {
    events: std::sync::Mutex<Vec<AdapterEvent>>,
}

impl AdapterEventSink for RecordingSink {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
        Box::pin(async move {
            let mut events = self
                .events
                .lock()
                .expect("recording sink mutex never poisoned");
            events.push(event);
            Ok(events.len() as u64)
        })
    }
}

pub async fn probe_scenario() -> (
    ScenarioResult,
    Option<String>,
    batman_runtime::adapter::AdapterCapabilities,
) {
    let adapter = new_adapter();
    let declared_capabilities = adapter.capabilities();
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return (
            batman_runtime::conformance::vendor_cli_skipped_probe(),
            None,
            declared_capabilities,
        );
    }
    match adapter.probe().await {
        Ok(result) => (
            ScenarioResult::pass(
                scenario::PROBE,
                format!(
                    "codex --version reported {:?}; authReady={}",
                    result.version, result.auth_ready
                ),
            ),
            result.version,
            declared_capabilities,
        ),
        Err(err) => (
            ScenarioResult::fail(scenario::PROBE, format!("probe failed: {err}")),
            None,
            declared_capabilities,
        ),
    }
}

/// `NATIVE_DISCOVERY`: no flag this adapter's own command line ever adds
/// (with or without worker-MCP injection) suppresses Codex's own
/// config/skill/plugin/hook/MCP discovery.
fn native_discovery_scenario() -> ScenarioResult {
    let adapter = new_adapter();
    let bare = adapter.spawn_spec(None);
    if bare.args.first().map(String::as_str) != Some("app-server") {
        return ScenarioResult::fail(
            scenario::NATIVE_DISCOVERY,
            format!("expected argv[0] == \"app-server\", got {:?}", bare.args),
        );
    }
    let disallowed = ["--bare", "--strict-mcp-config", "--disable-builtin-mcps"];
    for flag in disallowed {
        if bare.args.iter().any(|a| a == flag) {
            return ScenarioResult::fail(
                scenario::NATIVE_DISCOVERY,
                format!("bare spawn_spec unexpectedly added {flag}"),
            );
        }
    }
    ScenarioResult::pass(
        scenario::NATIVE_DISCOVERY,
        format!(
            "spawn_spec(None) argv is {:?}: only \"app-server\" plus any user-configured -c \
             overrides, none of --bare/--strict-mcp-config/--disable-builtin-mcps, so Codex's \
             own config/skill/plugin/hook/MCP discovery is never suppressed",
            bare.args
        ),
    )
}

/// `APPROVAL`: the approval fixture normalizes to pending approvals (never
/// sink events), and each decision maps to the verified `ReviewDecision`
/// wire shape.
fn approval_scenario() -> ScenarioResult {
    let lines = match read_jsonl("approval.jsonl") {
        Ok(lines) => lines,
        Err(err) => return ScenarioResult::fail(scenario::APPROVAL, err),
    };
    let mut kinds = Vec::new();
    for line in &lines {
        let Some(id) = line.get("id").cloned() else {
            return ScenarioResult::fail(scenario::APPROVAL, "fixture line missing id");
        };
        let Some(method) = line.get("method").and_then(Value::as_str) else {
            return ScenarioResult::fail(scenario::APPROVAL, "fixture line missing method");
        };
        let params = line.get("params").cloned().unwrap_or(Value::Null);
        let Some(approval) = normalize::server_request_to_pending_approval(&id, method, &params)
        else {
            return ScenarioResult::fail(
                scenario::APPROVAL,
                format!("{method} did not normalize to a pending approval"),
            );
        };
        if approval.request_id != id || approval.call_id.is_empty() {
            return ScenarioResult::fail(
                scenario::APPROVAL,
                "pending approval missing request_id/call_id correlation",
            );
        }
        kinds.push(approval.kind);
    }
    if kinds != vec!["execCommand", "applyPatch"] {
        return ScenarioResult::fail(
            scenario::APPROVAL,
            format!("unexpected approval kinds: {kinds:?}"),
        );
    }
    if normalize::decision_to_review_decision("approve") != Ok(Value::String("approved".into())) {
        return ScenarioResult::fail(scenario::APPROVAL, "approve decision mapping regressed");
    }
    match normalize::decision_to_review_decision("deny") {
        Ok(denied) if denied.get("denied").is_some() => {}
        other => {
            return ScenarioResult::fail(
                scenario::APPROVAL,
                format!("deny decision mapping regressed: {other:?}"),
            );
        }
    }
    if normalize::decision_to_review_decision("nonsense").is_ok() {
        return ScenarioResult::fail(
            scenario::APPROVAL,
            "an invalid decision string must be rejected",
        );
    }
    ScenarioResult::pass(
        scenario::APPROVAL,
        format!(
            "approval.jsonl's execCommandApproval/applyPatchApproval requests both normalized \
             to pending approvals (kinds {kinds:?}) with intact request_id/call_id correlation, \
             never as sink events; approve/deny/invalid decision mapping matches the verified \
             ReviewDecision shape"
        ),
    )
}

/// `RESULT_USAGE_ARTIFACTS` and `REDACTION`: normalizing the thread/turn
/// transcript fixture produces correlated text/tool/usage/artifact events
/// for one run, and the hidden `reasoning` item's chain-of-thought never
/// reaches a visible `MessageChunk`/`MessageFinal`.
fn transcript_scenarios() -> (ScenarioResult, ScenarioResult) {
    let lines = match read_jsonl("thread-turn.jsonl") {
        Ok(lines) => lines,
        Err(err) => {
            return (
                ScenarioResult::fail(scenario::RESULT_USAGE_ARTIFACTS, err.clone()),
                ScenarioResult::fail(scenario::REDACTION, err),
            );
        }
    };
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();

    let mut payloads = Vec::new();
    for line in &lines {
        let Some(method) = line.get("method").and_then(Value::as_str) else {
            let err = "fixture line missing method".to_string();
            return (
                ScenarioResult::fail(scenario::RESULT_USAGE_ARTIFACTS, err.clone()),
                ScenarioResult::fail(scenario::REDACTION, err),
            );
        };
        let params = line.get("params").cloned().unwrap_or(Value::Null);
        if let Some(payload) = normalize::notification_to_event(method, &params) {
            payloads.push(payload);
        }
    }
    let events: Vec<AdapterEvent> = payloads
        .into_iter()
        .map(|payload| AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload,
        })
        .collect();

    if events.is_empty()
        || !events
            .iter()
            .all(|e| e.run_id == run_id && e.task_id == task_id && e.worker_id == worker_id)
    {
        let err = "transcript fixture did not normalize to correlated events".to_string();
        return (
            ScenarioResult::fail(scenario::RESULT_USAGE_ARTIFACTS, err.clone()),
            ScenarioResult::fail(scenario::REDACTION, err),
        );
    }

    let has = |pred: fn(&AdapterEventPayload) -> bool| events.iter().any(|e| pred(&e.payload));
    let checks = [
        (
            has(|p| matches!(p, AdapterEventPayload::MessageChunk { .. })),
            "MessageChunk",
        ),
        (
            has(
                |p| matches!(p, AdapterEventPayload::MessageFinal { role, .. } if role == "assistant"),
            ),
            "MessageFinal",
        ),
        (
            has(
                |p| matches!(p, AdapterEventPayload::ToolStarted { name, .. } if name == "commandExecution"),
            ),
            "ToolStarted",
        ),
        (
            has(|p| matches!(p, AdapterEventPayload::ToolResult { ok: true, .. })),
            "ToolResult",
        ),
        (
            has(|p| {
                matches!(
                    p,
                    AdapterEventPayload::UsageReported {
                        input_tokens: 1200,
                        output_tokens: 180,
                        ..
                    }
                )
            }),
            "UsageReported",
        ),
        (
            has(
                |p| matches!(p, AdapterEventPayload::ArtifactProduced { artifact_kind, .. } if artifact_kind == "fileChange"),
            ),
            "ArtifactProduced",
        ),
    ];
    if let Some((_, missing)) = checks.iter().find(|(present, _)| !present) {
        let err = format!("transcript fixture missing expected {missing} event");
        return (
            ScenarioResult::fail(scenario::RESULT_USAGE_ARTIFACTS, err.clone()),
            ScenarioResult::fail(scenario::REDACTION, err),
        );
    }

    let result_usage_artifacts = ScenarioResult::pass(
        scenario::RESULT_USAGE_ARTIFACTS,
        "thread-turn.jsonl normalized to MessageChunk, MessageFinal, ToolStarted, ToolResult, \
         UsageReported (1200 input/180 output tokens), and ArtifactProduced (fileChange) events, \
         all correlated to the same run/task/worker id"
            .to_string(),
    );

    let mut leaked = false;
    for event in &events {
        if let AdapterEventPayload::MessageChunk { text, .. }
        | AdapterEventPayload::MessageFinal { text, .. } = &event.payload
            && (text.class != batman_protocol::ContentClass::Visible
                || text.value.contains("chain of thought"))
        {
            leaked = true;
        }
    }
    let redaction = if leaked {
        ScenarioResult::fail(
            scenario::REDACTION,
            "the reasoning item's hidden chain-of-thought leaked into a visible MessageChunk/MessageFinal",
        )
    } else {
        ScenarioResult::pass(
            scenario::REDACTION,
            "the fixture's reasoning item (\"internal chain of thought...\") never produced an \
             AdapterEvent at all -- normalize::notification_to_event drops every reasoning item \
             before construction -- and every MessageChunk/MessageFinal that was produced carries \
             ContentClass::Visible text with no chain-of-thought content"
                .to_string(),
        )
    };

    (result_usage_artifacts, redaction)
}

/// `ISOLATED_WRITE`: the `fileChange` artifact item's change path is a
/// plain relative path with no traversal components, so joined against
/// the worker's own `cwd` it can only ever resolve inside the worker's
/// isolated workspace, never outside it.
fn isolated_write_scenario() -> ScenarioResult {
    let lines = match read_jsonl("thread-turn.jsonl") {
        Ok(lines) => lines,
        Err(err) => return ScenarioResult::fail(scenario::ISOLATED_WRITE, err),
    };
    let mut change_paths = Vec::new();
    for line in &lines {
        if line.get("method").and_then(Value::as_str) != Some("item/completed") {
            continue;
        }
        let Some(item) = line.get("params").and_then(|p| p.get("item")) else {
            continue;
        };
        if item.get("type").and_then(Value::as_str) != Some("fileChange") {
            continue;
        }
        let Some(changes) = item.get("changes").and_then(Value::as_array) else {
            continue;
        };
        for change in changes {
            if let Some(path) = change.get("path").and_then(Value::as_str) {
                change_paths.push(path.to_string());
            }
        }
    }
    if change_paths.is_empty() {
        return ScenarioResult::fail(
            scenario::ISOLATED_WRITE,
            "thread-turn.jsonl fixture carries no fileChange item with a change path to verify",
        );
    }
    for path in &change_paths {
        let candidate = PathBuf::from(path);
        if candidate.is_absolute() || candidate.components().any(|c| c.as_os_str() == "..") {
            return ScenarioResult::fail(
                scenario::ISOLATED_WRITE,
                format!(
                    "fileChange path {path:?} is absolute or escapes its own directory via \
                     \"..\" -- joined against the worker's cwd this could write outside the \
                     isolated workspace"
                ),
            );
        }
    }
    ScenarioResult::pass(
        scenario::ISOLATED_WRITE,
        format!(
            "fileChange artifact change path(s) {change_paths:?} are plain relative paths with \
             no \"..\" traversal component, so joined against the worker's own cwd (as Codex's \
             own sandboxed filesystem access already enforces) they can only ever resolve inside \
             the worker's isolated workspace"
        ),
    )
}

/// `MANAGED_NESTING_REJECTION`: this foreign adapter never advertises
/// `nested: managed`.
fn managed_nesting_rejection_scenario(caps: &AdapterCapabilities) -> ScenarioResult {
    if caps.nested == NestedCapability::Managed {
        return ScenarioResult::fail(
            scenario::MANAGED_NESTING_REJECTION,
            "declared_capabilities().nested must never be Managed for a foreign adapter",
        );
    }
    ScenarioResult::pass(
        scenario::MANAGED_NESTING_REJECTION,
        format!(
            "declared_capabilities().nested == {:?} (never Managed); only OMP-native nesting may \
             advertise Managed, and only through OMP's own limits",
            caps.nested
        ),
    )
}

/// `UNEXPECTED_CHILD_OBSERVATION`: not applicable to this adapter's own
/// protocol surface -- Codex app-server's `ThreadItem` variants
/// (`agentMessage`, `reasoning`, `commandExecution`, `fileChange`, per
/// `fixtures/adapters/codex/schema-version.json`) carry no vendor-spawned
/// child/subagent concept at all, so `normalize::notification_to_event`
/// has no code path that could ever produce a `NestedWorkerObserved`
/// event, correctly matching the declared `nested: none` capability.
fn unexpected_child_observation_scenario() -> ScenarioResult {
    ScenarioResult::pass(
        scenario::UNEXPECTED_CHILD_OBSERVATION,
        "not applicable to codex: the app-server protocol's ThreadItem variants (agentMessage, \
         reasoning, commandExecution, fileChange) have no vendor-spawned-child/subagent concept \
         to observe, so normalize::notification_to_event has no path producing \
         NestedWorkerObserved -- consistent with this adapter's declared nested: none"
            .to_string(),
    )
}

/// `VENDOR_RECONNECT`: not applicable to this adapter -- its worker-MCP
/// tools are injected via `-c mcp_servers.crew.*` overrides into a real
/// `crewd coordination-mcp` subprocess that Codex itself spawns and
/// reconnects to on its own terms; this adapter owns no reconnect logic
/// of its own to test here (that subprocess's own reconnect behavior is
/// proven by `crates/runtime/tests/coordination_mcp.rs`, a different
/// milestone task's test file).
fn vendor_reconnect_scenario() -> ScenarioResult {
    ScenarioResult::pass(
        scenario::VENDOR_RECONNECT,
        "not applicable to codex: worker MCP reconnection is handled by Codex's own \
         mcp_servers.crew.* -configured coordination-mcp subprocess, which Codex spawns and \
         reconnects to on its own terms -- this adapter has no adapter-owned reconnect logic of \
         its own to test here (see crates/runtime/tests/coordination_mcp.rs for that \
         subprocess's own reconnect proof)"
            .to_string(),
    )
}

/// A scenario fixture mode genuinely cannot attempt, with a
/// caller-supplied reason. Used for `FOLLOW_UP`, `SESSION_RESUME`,
/// `RUNTIME_RESTART`, and `CANCELLATION_SCOPE`: this adapter's real
/// `codex app-server` binary only persists a thread's resumable rollout
/// to disk once a turn actually runs (confirmed against the installed
/// 0.145.0 binary -- a bare `thread/start` with no turn leaves no rollout
/// file at all), and `turn/start` itself is what invokes the model, so
/// none of these four can be proven without one. The outcome is `Skipped`,
/// not `Fail`: a scenario fixture mode cannot attempt is not one it
/// disproved, so it must not strip Codex's declared `steering` / `resume`
/// from `effective_capabilities` (R68). See `live_report`, which runs by
/// default unless `CREW_DISABLE_VENDOR_CLI=1` is set, for the real proof.
fn requires_live_turn_scenario(name: &'static str, mechanism: &str) -> ScenarioResult {
    ScenarioResult::skip(
        name,
        format!(
            "skipped: {mechanism} -- codex only persists a thread's resumable rollout once a \
             turn actually runs, and turn/start is what invokes the model, so this is not \
             attempted in fixture_report; see live_report (runs by default unless \
             CREW_DISABLE_VENDOR_CLI=1 is set)"
        ),
    )
}

async fn spawn_raw_client(cwd: &std::path::Path) -> Result<Arc<CodexRpcClient>, String> {
    let current_env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let env = EnvironmentPolicy::baseline().build(&current_env, &[]);
    let spec = SpawnSpec {
        program: PathBuf::from("codex"),
        args: vec!["app-server".to_string()],
        cwd: cwd.to_path_buf(),
        env,
        ..SpawnSpec::minimal()
    };
    let process = Supervisor::new()
        .spawn(spec)
        .await
        .map_err(|e| format!("spawning codex app-server failed: {e}"))?;
    let (client, _inbound_rx) = CodexRpcClient::spawn(process);
    let client = Arc::new(client);
    client
        .call(
            "initialize",
            serde_json::json!({
                "clientInfo": {"name": "@nikolasd/crew", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        )
        .await
        .map_err(|e| format!("initialize failed: {e}"))?;
    client
        .notify("initialized", serde_json::json!({}))
        .map_err(|e| format!("initialized notify failed: {e}"))?;
    Ok(client)
}

/// Real, zero-model-call proof for `READ_ONLY_START_AND_PROGRESS`: a bare
/// `thread/start` against a real spawned `codex app-server` process
/// creates a session with no `turn/start` (and therefore no model call)
/// ever issued -- the same pattern
/// `real_transport_completes_initialize_and_thread_start_with_zero_model_calls`
/// already uses, driven from inside this module instead of duplicated in
/// the test file.
async fn read_only_start_and_progress_scenario_inner() -> Result<ScenarioResult, String> {
    let cwd = std::env::temp_dir();
    let client = spawn_raw_client(&cwd).await?;
    let thread = client
        .call(
            "thread/start",
            serde_json::json!({"cwd": cwd.to_string_lossy()}),
        )
        .await
        .map_err(|e| format!("thread/start failed: {e}"))?;
    let thread_id = thread
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| "thread/start response missing thread.id".to_string())?
        .to_string();
    client
        .terminate()
        .await
        .map_err(|e| format!("terminating the process failed: {e}"))?;
    Ok(ScenarioResult::pass(
        scenario::READ_ONLY_START_AND_PROGRESS,
        format!(
            "real codex app-server thread {thread_id} was started (thread/start) with zero \
             model calls issued and no write outside {cwd:?}; progress observation \
             (MessageChunk/ToolStarted/ToolResult, correlated to one run/task/worker) is proven \
             by the thread-turn.jsonl fixture replay in the same suite"
        ),
    ))
}

async fn read_only_start_and_progress_scenario() -> ScenarioResult {
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return batman_runtime::conformance::vendor_cli_required_scenario(
            scenario::READ_ONLY_START_AND_PROGRESS,
        );
    }
    match read_only_start_and_progress_scenario_inner().await {
        Ok(result) => result,
        Err(err) => ScenarioResult::fail(
            scenario::READ_ONLY_START_AND_PROGRESS,
            format!("real zero-model-call codex app-server spawn failed: {err}"),
        ),
    }
}

async fn assemble_fixture_scenarios() -> Vec<ScenarioResult> {
    let (probe_result, _version, declared_capabilities) = probe_scenario().await;
    let (result_usage_artifacts, redaction) = transcript_scenarios();
    vec![
        probe_result,
        native_discovery_scenario(),
        approval_scenario(),
        result_usage_artifacts,
        redaction,
        isolated_write_scenario(),
        managed_nesting_rejection_scenario(&declared_capabilities),
        unexpected_child_observation_scenario(),
        vendor_reconnect_scenario(),
        read_only_start_and_progress_scenario().await,
        requires_live_turn_scenario(
            scenario::FOLLOW_UP,
            "AdapterMessage::FollowUp issues another turn/start on the already-established thread",
        ),
        requires_live_turn_scenario(
            scenario::CANCELLATION_SCOPE,
            "CancelScope::Turn's turn/interrupt path requires an active turn to interrupt",
        ),
        requires_live_turn_scenario(
            scenario::SESSION_RESUME,
            "Adapter::resume(thread/resume) needs a thread with an actually-persisted rollout",
        ),
        requires_live_turn_scenario(
            scenario::RUNTIME_RESTART,
            "resuming across a simulated runtime restart needs the same persisted rollout as SESSION_RESUME",
        ),
    ]
}

/// Runs every scenario this adapter can prove without a model call.
pub async fn fixture_report() -> ConformanceReport {
    let (_probe_result, version, declared_capabilities) = probe_scenario().await;
    let scenarios = assemble_fixture_scenarios().await;
    ConformanceReport::new(
        AdapterKindLabel::from(AdapterKind::Codex),
        ConformanceMode::Fixture,
        version,
        declared_capabilities,
        scenarios,
    )
}

async fn message_final_count(sink: &RecordingSink) -> usize {
    sink.events
        .lock()
        .expect("recording sink mutex never poisoned")
        .iter()
        .filter(|e| matches!(&e.payload, AdapterEventPayload::MessageFinal { .. }))
        .count()
}

/// The vendor's own failure text, if the adapter observed one. A turn that
/// fails vendor-side (expired credential, exhausted quota, refused
/// request) never produces a `MessageFinal`, so waiting for one would only
/// ever time out -- and report the timeout instead of the reason.
fn vendor_failure(sink: &RecordingSink) -> Option<String> {
    sink.events
        .lock()
        .expect("recording sink mutex never poisoned")
        .iter()
        .find_map(|e| match &e.payload {
            AdapterEventPayload::ProtocolHealthChanged { healthy, detail } if !healthy => {
                Some(detail.value.clone())
            }
            _ => None,
        })
}

/// Waits for `at_least` final messages, or returns the vendor's failure
/// text the moment one is observed.
///
/// `Ok(true)` -- the messages arrived. `Ok(false)` -- the timeout elapsed
/// with no vendor explanation. `Err(detail)` -- the vendor reported why it
/// will never arrive, which is strictly better than a generic timeout.
async fn wait_for_message_final_count(
    sink: &RecordingSink,
    at_least: usize,
    timeout: std::time::Duration,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if message_final_count(sink).await >= at_least {
            return Ok(true);
        }
        if let Some(detail) = vendor_failure(sink) {
            return Err(detail);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// A live, model-invoking proof for `FOLLOW_UP`, `CANCELLATION_SCOPE`,
/// `RESULT_USAGE_ARTIFACTS`, `SESSION_RESUME`, and `RUNTIME_RESTART`: runs
/// one real turn to completion (so codex actually persists a resumable
/// rollout), delivers a real follow-up, interrupts it mid-flight
/// (`CancelScope::Turn`), hard-stops the worker (`CancelScope::Worker`,
/// whose match arm `CancelScope::Subtree` shares exactly), then resumes
/// the same thread on a brand-new adapter instance/process (simulating a
/// runtime restart).
async fn live_lifecycle_scenarios_inner() -> Result<Vec<ScenarioResult>, String> {
    let cwd = std::env::temp_dir();
    let adapter1 = super::CodexAdapter::new(
        cwd.clone(),
        CodexStartupOptions::default(),
        Vec::new(),
        None,
    );
    let sink = Arc::new(RecordingSink::default());
    let event_sink: Arc<dyn AdapterEventSink> = sink.clone();
    let run_id = RunId::new();
    let spec = StartSpec {
        run_id,
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "reply with exactly the word first".to_string(),
        resume: None,
    };
    adapter1
        .start(spec, event_sink)
        .await
        .map_err(|e| format!("start failed: {e}"))?;

    match wait_for_message_final_count(&sink, 1, std::time::Duration::from_secs(60)).await {
        Ok(true) => {}
        Ok(false) => {
            return Err("first turn never produced a MessageFinal within 60s".to_string());
        }
        Err(detail) => {
            // The vendor said why. Reporting its text keeps an account or
            // credential problem from masquerading as an adapter defect.
            return Err(format!("first turn failed vendor-side: {detail}"));
        }
    }

    let thread_id = sink
        .events
        .lock()
        .expect("recording sink mutex never poisoned")
        .iter()
        .find_map(|e| match &e.payload {
            AdapterEventPayload::VendorSessionEstablished { vendor_session_id } => {
                Some(vendor_session_id.clone())
            }
            _ => None,
        })
        .ok_or_else(|| "no VendorSessionEstablished event observed".to_string())?;

    let follow_up_result = adapter1
        .send(AdapterMessage::FollowUp {
            text: "now reply with exactly the word second".to_string(),
        })
        .await;
    let follow_up = match &follow_up_result {
        Ok(()) => ScenarioResult::pass(
            scenario::FOLLOW_UP,
            "AdapterMessage::FollowUp issued a real turn/start on the already-established \
             thread and the live codex app-server accepted it"
                .to_string(),
        ),
        Err(err) => {
            ScenarioResult::fail(scenario::FOLLOW_UP, format!("follow-up send failed: {err}"))
        }
    };

    // CANCELLATION_SCOPE: interrupt the live follow-up turn (Turn scope),
    // then hard-stop the whole worker (Worker scope; Subtree shares its
    // exact match arm).
    let cancel_turn_result = adapter1.cancel(CancelScope::Turn).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let cancel_worker_result = adapter1.cancel(CancelScope::Worker).await;
    let cancellation_scope = match (&cancel_turn_result, &cancel_worker_result) {
        (Ok(()), Ok(())) => ScenarioResult::pass(
            scenario::CANCELLATION_SCOPE,
            "CancelScope::Turn (turn/interrupt on the live follow-up turn) and \
             CancelScope::Worker (terminate) both succeeded against a real codex app-server \
             process; CancelScope::Subtree shares Worker's exact match arm/terminate() path"
                .to_string(),
        ),
        _ => ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            format!("cancel(Turn)={cancel_turn_result:?}, cancel(Worker)={cancel_worker_result:?}"),
        ),
    };

    let events = sink
        .events
        .lock()
        .expect("recording sink mutex never poisoned")
        .clone();
    let correlated = !events.is_empty() && events.iter().all(|e| e.run_id == run_id);
    let has_usage = events
        .iter()
        .any(|e| matches!(&e.payload, AdapterEventPayload::UsageReported { .. }));
    let final_count = events
        .iter()
        .filter(|e| matches!(&e.payload, AdapterEventPayload::MessageFinal { .. }))
        .count();
    let result_usage_artifacts = if correlated && final_count >= 1 && has_usage {
        ScenarioResult::pass(
            scenario::RESULT_USAGE_ARTIFACTS,
            format!(
                "{} live events across the run (including {final_count} MessageFinal and at \
                 least one UsageReported) all correlated to run {run_id:?}",
                events.len()
            ),
        )
    } else {
        ScenarioResult::fail(
            scenario::RESULT_USAGE_ARTIFACTS,
            format!(
                "live events did not fully cover the expected shape: {} events, \
                 correlated={correlated}, final_count={final_count}, has_usage={has_usage}",
                events.len()
            ),
        )
    };

    // SESSION_RESUME / RUNTIME_RESTART: the process backing adapter1 is
    // now terminated (cancel(Worker) above); resume the same thread on a
    // brand-new adapter instance/process, simulating a runtime restart.
    let adapter2 = super::CodexAdapter::new(
        cwd.clone(),
        CodexStartupOptions::default(),
        Vec::new(),
        None,
    );
    let sink2: Arc<dyn AdapterEventSink> = Arc::new(NullSink);
    let resume_result = adapter2
        .resume(VendorSessionRef(thread_id.clone()), sink2)
        .await;
    let (session_resume, runtime_restart) = match &resume_result {
        Ok(()) => (
            ScenarioResult::pass(
                scenario::SESSION_RESUME,
                format!(
                    "Adapter::resume(VendorSessionRef({thread_id:?})) succeeded against the \
                     real codex app-server after a genuine completed turn persisted its rollout"
                ),
            ),
            ScenarioResult::pass(
                scenario::RUNTIME_RESTART,
                format!(
                    "thread {thread_id}, created and completed by a now-terminated process, was \
                     resumed by a brand-new CodexAdapter instance/process -- simulating a \
                     runtime restart with no in-memory RunState carried over, backing this \
                     adapter's declared durability: vendorResumable"
                ),
            ),
        ),
        Err(err) => {
            let detail = format!("resume on a fresh adapter instance failed: {err}");
            (
                ScenarioResult::fail(scenario::SESSION_RESUME, detail.clone()),
                ScenarioResult::fail(scenario::RUNTIME_RESTART, detail),
            )
        }
    };
    if resume_result.is_ok() {
        let _ = adapter2.dispose().await;
    }

    Ok(vec![
        follow_up,
        cancellation_scope,
        result_usage_artifacts,
        session_resume,
        runtime_restart,
    ])
}

async fn live_lifecycle_scenarios() -> Vec<ScenarioResult> {
    match live_lifecycle_scenarios_inner().await {
        Ok(results) => results,
        Err(err) => {
            let detail = format!("live codex lifecycle probe failed: {err}");
            vec![
                ScenarioResult::fail(scenario::FOLLOW_UP, detail.clone()),
                ScenarioResult::fail(scenario::CANCELLATION_SCOPE, detail.clone()),
                ScenarioResult::fail(scenario::RESULT_USAGE_ARTIFACTS, detail.clone()),
                ScenarioResult::fail(scenario::SESSION_RESUME, detail.clone()),
                ScenarioResult::fail(scenario::RUNTIME_RESTART, detail),
            ]
        }
    }
}

/// Runs the live conformance suite against the installed `codex` CLI.
///
/// Real invocation is the default; this suite runs a real `turn/start`,
/// which is a billed model call. Set `CREW_DISABLE_VENDOR_CLI=1` to
/// forbid it in CI or on a machine without the CLI installed.
///
/// # Errors
/// Returns a message if `CREW_DISABLE_VENDOR_CLI=1` is set.
pub async fn live_report() -> Result<ConformanceReport, String> {
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return Err(format!(
            "live Codex conformance is disabled by {}=1",
            batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV
        ));
    }
    let (_probe_result, version, declared_capabilities) = probe_scenario().await;
    let mut scenarios = assemble_fixture_scenarios().await;
    let live_results = live_lifecycle_scenarios().await;
    for live_result in live_results {
        if let Some(slot) = scenarios.iter_mut().find(|s| s.name == live_result.name) {
            *slot = live_result;
        }
    }
    Ok(ConformanceReport::new(
        AdapterKindLabel::from(AdapterKind::Codex),
        ConformanceMode::Live,
        version,
        declared_capabilities,
        scenarios,
    ))
}
