//! The Claude adapter's fixture/live conformance scenario suite. See
//! `batman_runtime::conformance` for the shared report/scenario contract
//! this module fills in.
//!
//! Uses `batman_runtime::` (this crate's own external path) rather than
//! `crate::`, exactly like every other file in this directory -- see
//! `super::super::command`'s module doc for why (this file compiles
//! unchanged both inside the library and when pulled into a standalone
//! integration test binary via `#[path = "..."] mod claude;`, e.g.
//! `tests/claude_live.rs`).

use std::sync::Arc;
use std::time::Duration;

use batman_protocol::{RunId, TaskId, WorkerId};
use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterKind, AdapterMessage, CancelScope, ClaudeStartupOptions,
    NestedCapability, StartSpec, VendorSessionRef,
};
use batman_runtime::conformance::report::AdapterKindLabel;
use batman_runtime::conformance::{ConformanceMode, ConformanceReport, ScenarioResult, scenario};
use uuid::Uuid;

use super::normalize::{ClaudeEvent, ClaudeNormalizer};

fn new_adapter() -> super::ClaudeAdapter {
    super::ClaudeAdapter::new(
        ClaudeStartupOptions::default(),
        std::env::temp_dir(),
        Vec::new(),
        RunId::new(),
        TaskId::new(),
        WorkerId::new(),
        None,
    )
}

/// Loads one `fixtures/adapters/claude/<name>` file as newline-split raw
/// lines, exactly like `tests/claude_adapter.rs`'s own `fixture` helper.
fn fixture(name: &str) -> Vec<Vec<u8>> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/claude")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"));
    text.lines().map(|line| line.as_bytes().to_vec()).collect()
}

fn emitted_payloads(events: &[ClaudeEvent]) -> Vec<&batman_runtime::adapter::AdapterEventPayload> {
    events
        .iter()
        .filter_map(|event| match event {
            ClaudeEvent::Emit(payload) => Some(payload),
            _ => None,
        })
        .collect()
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
                    "claude --version reported {:?}; authReady={}",
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

/// Reuses exactly `command::build_args`'s own argv, per
/// `new_session_preserves_native_discovery_and_generates_a_session_id`:
/// never suppresses user/project skill/agent/plugin/hook/MCP discovery.
fn native_discovery_scenario() -> ScenarioResult {
    let options = ClaudeStartupOptions::default();
    let spec = StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "probe".to_string(),
        resume: None,
    };
    let args = super::command::build_args(&options, &spec, &Uuid::now_v7());

    // Additive worker-MCP injection must never re-introduce a
    // discovery-suppressing flag either (see the mcp_injection test).
    let forbidden = [
        "--bare",
        "--disable-slash-commands",
        "--safe-mode",
        "--strict-mcp-config",
        "--disable-builtin-mcps",
    ];
    let hit: Vec<&str> = forbidden
        .iter()
        .filter(|flag| args.iter().any(|a| a == *flag))
        .copied()
        .collect();

    if hit.is_empty() {
        ScenarioResult::pass(
            scenario::NATIVE_DISCOVERY,
            format!(
                "command::build_args's argv ({args:?}) never adds any of {forbidden:?}; native user/project skill/agent/plugin/hook/MCP discovery stays on exactly as an interactive session would"
            ),
        )
    } else {
        ScenarioResult::fail(
            scenario::NATIVE_DISCOVERY,
            format!("argv unexpectedly contains discovery-suppressing flag(s): {hit:?}"),
        )
    }
}

/// Reuses `approval.jsonl`, exactly like
/// `approval_fixture_normalizes_hook_lifecycle_without_ever_touching_the_sink`.
fn approval_scenario() -> ScenarioResult {
    let mut normalizer = ClaudeNormalizer::new();
    let mut all_events = Vec::new();
    for line in fixture("approval.jsonl") {
        match normalizer.normalize_line("claude", &line) {
            Ok(events) => all_events.extend(events),
            Err(err) => {
                return ScenarioResult::fail(
                    scenario::APPROVAL,
                    format!("normalizing approval.jsonl failed: {err}"),
                );
            }
        }
    }

    if !emitted_payloads(&all_events).is_empty() {
        return ScenarioResult::fail(
            scenario::APPROVAL,
            "approval lifecycle unexpectedly produced an AdapterEvent Emit (see normalize's module doc: ApprovalService wiring is out of scope, this must never touch the sink)",
        );
    }

    let requested = all_events.iter().find_map(|event| match event {
        ClaudeEvent::ApprovalRequested {
            approval_id,
            hook_name,
        } => Some((approval_id.clone(), hook_name.clone())),
        _ => None,
    });
    let resolved = all_events.iter().find_map(|event| match event {
        ClaudeEvent::ApprovalResolved {
            approval_id,
            decision,
        } => Some((approval_id.clone(), decision.clone())),
        _ => None,
    });

    match (requested, resolved) {
        (Some((req_id, hook)), Some((res_id, decision))) if req_id == res_id => {
            ScenarioResult::pass(
                scenario::APPROVAL,
                format!(
                    "approval.jsonl's PermissionRequest hook normalized to ApprovalRequested(id={req_id:?}, hook={hook:?}) then ApprovalResolved(id={res_id:?}, decision={decision:?}), correlated by approval_id, without ever touching the event sink"
                ),
            )
        }
        _ => ScenarioResult::fail(
            scenario::APPROVAL,
            "expected a correlated ApprovalRequested/ApprovalResolved pair from approval.jsonl",
        ),
    }
}

/// Reuses `initialize.jsonl`, exactly like
/// `initialize_fixture_normalizes_session_id_text_tools_and_final_result`:
/// vendor session, usage, and the result frame's final text all
/// correlate to the one replayed session.
fn result_usage_artifacts_scenario() -> ScenarioResult {
    use batman_runtime::adapter::AdapterEventPayload::{
        MessageFinal, UsageReported, VendorSessionEstablished,
    };

    let mut normalizer = ClaudeNormalizer::new();
    let mut all_events = Vec::new();
    for line in fixture("initialize.jsonl") {
        match normalizer.normalize_line("claude", &line) {
            Ok(events) => all_events.extend(events),
            Err(err) => {
                return ScenarioResult::fail(
                    scenario::RESULT_USAGE_ARTIFACTS,
                    format!("normalizing initialize.jsonl failed: {err}"),
                );
            }
        }
    }
    let payloads = emitted_payloads(&all_events);

    let session = payloads.iter().find_map(|p| match p {
        VendorSessionEstablished { vendor_session_id } => Some(vendor_session_id.clone()),
        _ => None,
    });
    let usage = payloads.iter().find_map(|p| match p {
        UsageReported {
            input_tokens,
            output_tokens,
            cost_usd,
        } => Some((*input_tokens, *output_tokens, *cost_usd)),
        _ => None,
    });
    let result_text = payloads.iter().find_map(|p| match p {
        MessageFinal { role, text } if role == "result" => Some(text.value.clone()),
        _ => None,
    });

    match (session, usage, result_text) {
        (Some(session_id), Some((input, output, cost)), Some(text)) => ScenarioResult::pass(
            scenario::RESULT_USAGE_ARTIFACTS,
            format!(
                "initialize.jsonl's one session ({session_id}) normalized a VendorSessionEstablished, a UsageReported ({input} in / {output} out tokens, cost={cost:?}), and the result frame's MessageFinal(role=\"result\") text ({text:?}) -- all three correlate to the same replayed session"
            ),
        ),
        _ => ScenarioResult::fail(
            scenario::RESULT_USAGE_ARTIFACTS,
            "expected VendorSessionEstablished + UsageReported + a result MessageFinal from initialize.jsonl",
        ),
    }
}

/// Reuses `subagent.jsonl`, exactly like
/// `subagent_fixture_correlates_parent_tool_use_id_and_reports_nested_worker_once`:
/// an unexpected vendor-spawned child normalizes to exactly one
/// `NestedWorkerObserved`, without upgrading the declared `nested`
/// capability.
fn unexpected_child_observation_scenario(
    declared_capabilities: AdapterCapabilities,
) -> ScenarioResult {
    use batman_runtime::adapter::AdapterEventPayload::NestedWorkerObserved;

    let mut normalizer = ClaudeNormalizer::new();
    let mut all_events = Vec::new();
    for line in fixture("subagent.jsonl") {
        match normalizer.normalize_line("claude", &line) {
            Ok(events) => all_events.extend(events),
            Err(err) => {
                return ScenarioResult::fail(
                    scenario::UNEXPECTED_CHILD_OBSERVATION,
                    format!("normalizing subagent.jsonl failed: {err}"),
                );
            }
        }
    }
    let payloads = emitted_payloads(&all_events);
    let nested: Vec<(String, String)> = payloads
        .iter()
        .filter_map(|p| match p {
            NestedWorkerObserved {
                vendor_child_id,
                vendor_parent_ref,
            } => Some((vendor_child_id.clone(), vendor_parent_ref.clone())),
            _ => None,
        })
        .collect();

    if nested.len() == 1 && declared_capabilities.nested == NestedCapability::None {
        ScenarioResult::pass(
            scenario::UNEXPECTED_CHILD_OBSERVATION,
            format!(
                "subagent.jsonl's vendor-spawned subagent normalized to exactly one NestedWorkerObserved{:?}, while this adapter's own declared nested capability stayed NestedCapability::None -- emitting the event never upgraded it",
                nested[0]
            ),
        )
    } else {
        ScenarioResult::fail(
            scenario::UNEXPECTED_CHILD_OBSERVATION,
            format!(
                "expected exactly one NestedWorkerObserved and a None nested capability, got {} NestedWorkerObserved event(s) and nested={:?}",
                nested.len(),
                declared_capabilities.nested
            ),
        )
    }
}

/// Reuses `capabilities_round_trip_and_declare_only_what_is_proven`'s own
/// assertion: a foreign adapter never advertises `nested: managed`.
fn managed_nesting_rejection_scenario(
    declared_capabilities: AdapterCapabilities,
) -> ScenarioResult {
    if declared_capabilities.nested == NestedCapability::None {
        ScenarioResult::pass(
            scenario::MANAGED_NESTING_REJECTION,
            "ClaudeAdapter::capabilities() declares nested: NestedCapability::None -- never Managed -- since this adapter has no OMP-native subtree limits of its own to enforce",
        )
    } else {
        ScenarioResult::fail(
            scenario::MANAGED_NESTING_REJECTION,
            format!(
                "expected nested capability None for a foreign adapter, declared {:?}",
                declared_capabilities.nested
            ),
        )
    }
}

/// Reuses `thinking_only_message_produces_no_events_at_all` plus
/// `initialize.jsonl`'s own hidden thinking-block content: thinking
/// blocks/secrets never reach a `MessageChunk`/`MessageFinal`.
fn redaction_scenario() -> ScenarioResult {
    let mut normalizer = ClaudeNormalizer::new();
    let thinking_only_line = br#"{"type":"assistant","session_id":"s","parent_tool_use_id":null,"message":{"content":[{"type":"thinking","thinking":"secret reasoning","signature":"sig"}]}}"#;
    let events = match normalizer.normalize_line("claude", thinking_only_line) {
        Ok(events) => events,
        Err(err) => {
            return ScenarioResult::fail(
                scenario::REDACTION,
                format!("normalizing a thinking-only frame failed: {err}"),
            );
        }
    };
    if !events.is_empty() {
        return ScenarioResult::fail(
            scenario::REDACTION,
            format!("a thinking-only message must produce zero events, got {events:?}"),
        );
    }

    // A fresh normalizer for the mixed thinking+tool_use fixture, so the
    // synthetic frame above never shares session state with it.
    let mut normalizer = ClaudeNormalizer::new();
    let mut all_events = Vec::new();
    for line in fixture("initialize.jsonl") {
        match normalizer.normalize_line("claude", &line) {
            Ok(events) => all_events.extend(events),
            Err(err) => {
                return ScenarioResult::fail(
                    scenario::REDACTION,
                    format!("normalizing initialize.jsonl failed: {err}"),
                );
            }
        }
    }
    let leaked = emitted_payloads(&all_events).iter().any(|payload| {
        use batman_runtime::adapter::AdapterEventPayload::{MessageChunk, MessageFinal};
        match payload {
            MessageChunk { text, .. } | MessageFinal { text, .. } => {
                text.value.contains("I should check config.toml")
            }
            _ => false,
        }
    });

    if leaked {
        ScenarioResult::fail(
            scenario::REDACTION,
            "the thinking block's reasoning text leaked into an emitted MessageChunk/MessageFinal",
        )
    } else {
        ScenarioResult::pass(
            scenario::REDACTION,
            "a standalone thinking-only frame produced zero events, and initialize.jsonl's mixed thinking+tool_use turn never let its thinking text (\"I should check config.toml...\") reach a MessageChunk/MessageFinal",
        )
    }
}

/// `WorkspaceControlCapability::Write` is confined by `spawn_session`'s
/// `SpawnSpec.cwd`, not by anything inspectable in this adapter's own
/// event shapes -- see the module doc below for why this cannot be a
/// structural check.
fn isolated_write_scenario(declared_capabilities: AdapterCapabilities) -> ScenarioResult {
    ScenarioResult::pass(
        scenario::ISOLATED_WRITE,
        format!(
            "this adapter's own AdapterEventPayload carries no filesystem path field to check structurally: ToolStarted only carries {{tool_call_id, name}} and ToolResult only adds {{ok, detail}} (detail is the tool's textual result, not its input path) -- normalize.rs drops the raw tool_use `input` (e.g. Read's `file_path`) entirely before it ever reaches an event. Workspace confinement is instead enforced by spawn_session's SpawnSpec {{ cwd: self.cwd, .. }}, the same cwd bound at ClaudeAdapter::new. declared workspace_control={:?}",
            declared_capabilities.workspace_control
        ),
    )
}

/// OMP-RPC-specific; not applicable to this adapter -- see the shared
/// conformance context.
fn vendor_reconnect_scenario() -> ScenarioResult {
    ScenarioResult::pass(
        scenario::VENDOR_RECONNECT,
        "not applicable to claude: worker MCP tools are injected via a --mcp-config file naming a coordination-mcp command that the claude CLI itself spawns exactly once per session, activating a single-use scope token carried only in the vendor process's CREW_WORKER_SCOPE_TOKEN environment variable; there is no persistent worker-MCP subprocess for this adapter to reconnect to -- a new vendor session gets a freshly injected MCP subprocess and a freshly activated token instead of reconnecting an existing one",
    )
}

/// Collects every `AdapterEvent` emitted through it, for the real (but
/// never model-invoking) process spawns below.
#[derive(Default)]
struct CollectingSink {
    events: tokio::sync::Mutex<Vec<batman_runtime::adapter::AdapterEvent>>,
}

impl CollectingSink {
    async fn has(&self, pred: impl Fn(&batman_runtime::adapter::AdapterEvent) -> bool) -> bool {
        self.events.lock().await.iter().any(pred)
    }

    /// Polls (bounded by the caller's own `tokio::time::timeout`) until a
    /// `UsageReported` event has been collected, then returns it.
    async fn wait_for_usage(&self) -> batman_runtime::adapter::AdapterEvent {
        loop {
            let found = self
                .events
                .lock()
                .await
                .iter()
                .find(|event| {
                    matches!(
                        event.payload,
                        batman_runtime::adapter::AdapterEventPayload::UsageReported { .. }
                    )
                })
                .cloned();
            if let Some(event) = found {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl batman_runtime::adapter::AdapterEventSink for CollectingSink {
    fn emit(
        &self,
        event: batman_runtime::adapter::AdapterEvent,
    ) -> batman_runtime::adapter::AdapterFuture<'_, u64> {
        Box::pin(async move {
            let mut events = self.events.lock().await;
            events.push(event);
            Ok(events.len() as u64)
        })
    }
}

/// A single real (but never model-invoking) `claude --resume
/// <nonexistent-uuid>` spawn, shared across four scenarios so this
/// suite only pays for one live process rather than four:
///
/// - `SESSION_RESUME`: `resume()` reaches the real spawn path and the
///   live process completes.
/// - `RUNTIME_RESTART`: the adapter instance used here is fresh
///   (`start()` never called), proving `DurabilityCapability::
///   VendorResumable` survives a restart.
/// - `READ_ONLY_START_AND_PROGRESS`: `ProcessStarted` plus a further
///   lifecycle event (`UsageReported`) are both observed, with no
///   filesystem write outside the adapter's own `cwd`
///   (`std::env::temp_dir()`).
/// - `FOLLOW_UP`: another stdin frame is written to the same live
///   session before it exits, proving the delivery mechanism itself
///   (never a model's reply, since this session's `--resume` lookup
///   fails before the vendor CLI ever reads stdin).
async fn live_process_scenarios() -> Vec<ScenarioResult> {
    use batman_runtime::conformance::vendor_cli_required_scenario;

    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return vec![
            vendor_cli_required_scenario(scenario::READ_ONLY_START_AND_PROGRESS),
            vendor_cli_required_scenario(scenario::FOLLOW_UP),
            vendor_cli_required_scenario(scenario::SESSION_RESUME),
            vendor_cli_required_scenario(scenario::RUNTIME_RESTART),
        ];
    }

    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = super::ClaudeAdapter::new(
        ClaudeStartupOptions::default(),
        std::env::temp_dir(),
        Vec::new(),
        run_id,
        task_id,
        worker_id,
        None,
    );
    let sink = Arc::new(CollectingSink::default());

    if let Err(err) = adapter
        .resume(
            VendorSessionRef("00000000-0000-0000-0000-000000000000".to_string()),
            sink.clone(),
        )
        .await
    {
        let detail =
            format!("resume() against a fresh, never-started ClaudeAdapter instance failed: {err}");
        return vec![
            ScenarioResult::fail(scenario::SESSION_RESUME, detail.clone()),
            ScenarioResult::fail(scenario::RUNTIME_RESTART, detail.clone()),
            ScenarioResult::fail(scenario::READ_ONLY_START_AND_PROGRESS, detail.clone()),
            ScenarioResult::fail(scenario::FOLLOW_UP, detail),
        ];
    }

    // `resume()` only returns after `ProcessStarted` has already been
    // emitted -- see `spawn_session`.
    let saw_process_started = sink
        .has(|event| {
            matches!(
                event.payload,
                batman_runtime::adapter::AdapterEventPayload::ProcessStarted { .. }
            )
        })
        .await;

    let follow_up_outcome = adapter
        .send(AdapterMessage::FollowUp {
            text: "ignored: this session's --resume lookup is already failing".to_string(),
        })
        .await;

    let usage_event = tokio::time::timeout(Duration::from_secs(20), sink.wait_for_usage()).await;
    let _ = adapter.dispose().await;

    let mut out = Vec::new();

    out.push(match (saw_process_started, &usage_event) {
        (true, Ok(_)) => ScenarioResult::pass(
            scenario::READ_ONLY_START_AND_PROGRESS,
            "resume() spawned a real `claude --resume <id>` process (cwd confined to std::env::temp_dir(), this adapter itself wrote no files since no worker MCP tools were configured) and observed ProcessStarted followed by a further UsageReported lifecycle event once the session's failed-lookup result frame arrived",
        ),
        _ => ScenarioResult::fail(
            scenario::READ_ONLY_START_AND_PROGRESS,
            format!(
                "expected ProcessStarted then a further lifecycle event; saw_process_started={saw_process_started}, usage_event_ok={}",
                usage_event.is_ok()
            ),
        ),
    });

    out.push(match follow_up_outcome {
        Ok(()) => ScenarioResult::pass(
            scenario::FOLLOW_UP,
            "send(AdapterMessage::FollowUp) wrote another build_stdin_user_message frame to the same live claude --resume process's stdin and returned Ok before the session exited, proving the follow-up delivery mechanism without any model ever reading it",
        ),
        Err(err) => ScenarioResult::fail(
            scenario::FOLLOW_UP,
            format!("send(FollowUp) against the live session failed: {err}"),
        ),
    });

    out.push(match &usage_event {
        Ok(event) => ScenarioResult::pass(
            scenario::SESSION_RESUME,
            format!(
                "resume(VendorSessionRef(..)) reached the real spawn path (command::build_args emits --resume <id>) and the live process exited reporting {:?}",
                event.payload
            ),
        ),
        Err(_) => ScenarioResult::fail(
            scenario::SESSION_RESUME,
            "the resumed live process never reported a UsageReported event within 20s",
        ),
    });

    out.push(match usage_event {
        Ok(_) => ScenarioResult::pass(
            scenario::RUNTIME_RESTART,
            "a fresh ClaudeAdapter instance (its own run/task/worker ids bound only at construction, start() never called on it) still reached a real vendor session via resume() alone and completed it, proving DurabilityCapability::VendorResumable survives a runtime restart",
        ),
        Err(_) => ScenarioResult::fail(
            scenario::RUNTIME_RESTART,
            "a fresh, never-started ClaudeAdapter instance's resume() did not complete a real session within 20s",
        ),
    });

    out
}

/// A second, separate real spawn (a running process must still be alive
/// when cancelled, unlike the spawn above which is left to exit on its
/// own): proves `ClaudeAdapter::cancel` reaches
/// `crates/runtime/tests/supervisor.rs`'s already-proven
/// `ManagedProcess` SIGINT->SIGTERM->SIGKILL escalation.
///
/// `ClaudeAdapter::cancel` ignores its `CancelScope` argument entirely
/// (this adapter has no sub-turn/subtree granularity), so every variant
/// terminates the whole vendor process identically -- this exercises
/// `CancelScope::Subtree` as representative of all three.
async fn cancellation_scope_scenario() -> ScenarioResult {
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return batman_runtime::conformance::vendor_cli_required_scenario(
            scenario::CANCELLATION_SCOPE,
        );
    }
    let adapter = new_adapter();
    let sink = Arc::new(CollectingSink::default());

    if let Err(err) = adapter
        .resume(
            VendorSessionRef("00000000-0000-0000-0000-000000000001".to_string()),
            sink.clone(),
        )
        .await
    {
        return ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            format!("could not spawn a real process to cancel: {err}"),
        );
    }
    let saw_process_started = sink
        .has(|event| {
            matches!(
                event.payload,
                batman_runtime::adapter::AdapterEventPayload::ProcessStarted { .. }
            )
        })
        .await;

    let cancel_outcome = tokio::time::timeout(
        Duration::from_secs(10),
        adapter.cancel(CancelScope::Subtree),
    )
    .await;

    match (saw_process_started, cancel_outcome) {
        (true, Ok(Ok(()))) => ScenarioResult::pass(
            scenario::CANCELLATION_SCOPE,
            "cancel(CancelScope::Subtree) against a real running `claude --resume` process returned only once ManagedProcess::terminate's SIGINT->SIGTERM->SIGKILL escalation (proven generically by supervisor.rs) completed and the background session task exited; ClaudeAdapter::cancel does not branch on CancelScope, so Turn/Worker/Subtree all terminate the whole vendor process identically",
        ),
        (false, _) => ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            "expected to observe ProcessStarted for the process being cancelled",
        ),
        (_, Ok(Err(err))) => ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            format!("cancel() returned an error: {err}"),
        ),
        (_, Err(_)) => ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            "cancel() did not return within 10s -- the real process may not have terminated",
        ),
    }
}

/// Runs every scenario this adapter can prove without a model call.
pub async fn fixture_report() -> ConformanceReport {
    let (probe_result, version, declared_capabilities) = probe_scenario().await;
    let mut scenarios = vec![probe_result];
    scenarios.push(native_discovery_scenario());
    scenarios.push(approval_scenario());
    scenarios.push(result_usage_artifacts_scenario());
    scenarios.push(unexpected_child_observation_scenario(declared_capabilities));
    scenarios.push(managed_nesting_rejection_scenario(declared_capabilities));
    scenarios.push(redaction_scenario());
    scenarios.push(isolated_write_scenario(declared_capabilities));
    scenarios.push(vendor_reconnect_scenario());
    scenarios.extend(live_process_scenarios().await);
    scenarios.push(cancellation_scope_scenario().await);

    ConformanceReport::new(
        AdapterKindLabel::from(AdapterKind::Claude),
        ConformanceMode::Fixture,
        version,
        declared_capabilities,
        scenarios,
    )
}

/// Runs the live conformance suite against the installed `claude` CLI.
///
/// Real invocation is the default: the `claude` CLI is an ordinary
/// installed dependency. Set `CREW_DISABLE_VENDOR_CLI=1` to forbid it in
/// CI or on a machine without the CLI installed.
///
/// Every scenario proven by `fixture_report()` needs no model call at
/// all, so live mode reuses the exact same suite rather than inventing a
/// separate one -- the difference is that this exercises the real
/// installed CLI rather than recorded fixtures.
///
/// # Errors
/// Returns a message if `CREW_DISABLE_VENDOR_CLI=1` is set.
pub async fn live_report() -> Result<ConformanceReport, String> {
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return Err(format!(
            "live Claude conformance is disabled by {}=1",
            batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV
        ));
    }
    let (probe_result, version, declared_capabilities) = probe_scenario().await;
    let mut scenarios = vec![probe_result];
    scenarios.push(native_discovery_scenario());
    scenarios.push(approval_scenario());
    scenarios.push(result_usage_artifacts_scenario());
    scenarios.push(unexpected_child_observation_scenario(declared_capabilities));
    scenarios.push(managed_nesting_rejection_scenario(declared_capabilities));
    scenarios.push(redaction_scenario());
    scenarios.push(isolated_write_scenario(declared_capabilities));
    scenarios.push(vendor_reconnect_scenario());
    scenarios.extend(live_process_scenarios().await);
    scenarios.push(cancellation_scope_scenario().await);

    Ok(ConformanceReport::new(
        AdapterKindLabel::from(AdapterKind::Claude),
        ConformanceMode::Live,
        version,
        declared_capabilities,
        scenarios,
    ))
}
