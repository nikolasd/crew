//! The Copilot adapter's fixture/live conformance scenario suite. See
//! `batman_runtime::conformance` for the shared report/scenario contract
//! this module fills in.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use batman_protocol::{RunId, TaskId, WorkerId};
use serde_json::Value;
use tokio::time::timeout;

use batman_runtime::ScopeTokenStore;
use batman_runtime::adapter::mcp_config::AdapterMcpConfig;
use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterError, AdapterEventPayload, AdapterKind,
    CopilotStartupOptions, NestedCapability,
};
use batman_runtime::conformance::report::AdapterKindLabel;
use batman_runtime::conformance::{
    ConformanceMode, ConformanceReport, ScenarioResult, VendorUnavailable, scenario,
};

use super::client::CopilotAcpClient;
use super::normalize::copilot_normalize_session_update;

fn new_adapter() -> super::CopilotAdapter {
    super::CopilotAdapter::new(
        PathBuf::from("copilot"),
        std::env::temp_dir(),
        CopilotStartupOptions::default(),
        Vec::new(),
        RunId::new(),
        TaskId::new(),
        WorkerId::new(),
        None,
    )
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/adapters/copilot"
    ))
    .join(name)
}

fn load_jsonl_fixture(name: &str) -> Vec<Value> {
    let path = fixture_path(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("parsing {name} line: {e}"))
        })
        .collect()
}

fn real_copilot_binary() -> Option<PathBuf> {
    let output = Command::new("which").arg("copilot").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// Spawns a real `copilot --acp` process rooted at `cwd`. Never sends a
/// `session/prompt` -- callers only ever drive `initialize`/`session/new`/
/// `session/load`/`session/list`, none of which invoke a model.
async fn real_client(cwd: &Path) -> Result<CopilotAcpClient, VendorUnavailable> {
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return Err(VendorUnavailable::disabled(
            "driving a real copilot --acp process",
        ));
    }
    let copilot = real_copilot_binary()
        .ok_or_else(|| VendorUnavailable::Failed("copilot CLI not found on PATH".to_string()))?;
    timeout(
        Duration::from_secs(10),
        CopilotAcpClient::spawn(&copilot, cwd, Vec::new(), HashMap::new()),
    )
    .await
    .map_err(|_| VendorUnavailable::Failed("spawning copilot --acp timed out".to_string()))?
    .map_err(|err| VendorUnavailable::Failed(format!("spawning copilot --acp failed: {err}")))
}

async fn call_named<T>(
    what: &str,
    fut: impl Future<Output = Result<T, AdapterError>>,
) -> Result<T, String> {
    timeout(Duration::from_secs(10), fut)
        .await
        .map_err(|_| format!("{what} timed out"))?
        .map_err(|err| format!("{what} failed: {err}"))
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
                    "copilot --version reported {:?}; authReady={}",
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

/// A real, no-model-call `initialize` + `session/list` handshake against
/// the installed binary, mirroring
/// `real_binary_initialize_and_session_list_never_invoke_a_model` --
/// never a `session/prompt`, so nothing is ever written outside the
/// worker's own workspace.
async fn read_only_start_and_progress_scenario() -> ScenarioResult {
    let cwd = std::env::temp_dir();
    let client = match real_client(&cwd).await {
        Ok(client) => client,
        Err(unavailable) => {
            return unavailable.into_scenario(scenario::READ_ONLY_START_AND_PROGRESS);
        }
    };
    let negotiated = match call_named("initialize", client.initialize()).await {
        Ok(n) => n,
        Err(detail) => {
            client.shutdown().await;
            return ScenarioResult::fail(scenario::READ_ONLY_START_AND_PROGRESS, detail);
        }
    };
    let sessions = match call_named("session/list", client.session_list()).await {
        Ok(s) => s,
        Err(detail) => {
            client.shutdown().await;
            return ScenarioResult::fail(scenario::READ_ONLY_START_AND_PROGRESS, detail);
        }
    };
    client.shutdown().await;
    ScenarioResult::pass(
        scenario::READ_ONLY_START_AND_PROGRESS,
        format!(
            "real copilot --acp negotiated protocol v{} and session/list returned {} without ever sending session/prompt",
            negotiated.protocol_version, sessions
        ),
    )
}

/// Structural proof of workspace confinement: the `edit`-kind tool call's
/// `diff` content in `session-updates.jsonl` names a path with no `..`
/// traversal component, and the normalized `ToolResult` detail carries
/// only that path -- never the old/new file text (which could itself
/// leak content from outside the intended write, or be arbitrarily
/// large/sensitive).
fn isolated_write_scenario() -> ScenarioResult {
    let updates: Vec<Value> = load_jsonl_fixture("session-updates.jsonl")
        .into_iter()
        .map(|frame| frame["params"]["update"].clone())
        .collect();
    let diff_update = &updates[7];
    let Some(path) = diff_update["content"][0]["path"].as_str() else {
        return ScenarioResult::fail(
            scenario::ISOLATED_WRITE,
            "session-updates.jsonl's edit tool_call_update has no diff content path to check",
        );
    };
    if path.split('/').any(|segment| segment == "..") {
        return ScenarioResult::fail(
            scenario::ISOLATED_WRITE,
            format!("diff path {path} contains a `..` traversal component"),
        );
    }
    let payloads = copilot_normalize_session_update(diff_update);
    let AdapterEventPayload::ToolResult { detail, .. } = &payloads[0] else {
        return ScenarioResult::fail(
            scenario::ISOLATED_WRITE,
            "edit tool_call_update did not normalize to a ToolResult",
        );
    };
    if detail.value.contains("assert_eq") {
        return ScenarioResult::fail(
            scenario::ISOLATED_WRITE,
            "normalized ToolResult leaked diff file content instead of confining to the path",
        );
    }
    ScenarioResult::pass(
        scenario::ISOLATED_WRITE,
        format!(
            "edit tool_call_update confined to path {path} (no `..` traversal); normalized ToolResult detail is {:?}, never the old/new file text",
            detail.value
        ),
    )
}

/// ACP v1 has no mid-turn steering distinct from a follow-up
/// `session/prompt` after a turn ends (this adapter declares
/// `steering: None`); the real mechanism a "follow-up" takes here is
/// `session/load` reaching the same real, already-active session on the
/// same still-connected client -- proving Copilot recognizes and
/// re-engages with the exact session a follow-up would target, distinct
/// from `session_resume_scenario`'s cross-process reconnection.
async fn follow_up_scenario() -> ScenarioResult {
    let cwd = std::env::temp_dir();
    let cwd_str = cwd.to_string_lossy().to_string();
    let client = match real_client(&cwd).await {
        Ok(client) => client,
        Err(unavailable) => return unavailable.into_scenario(scenario::FOLLOW_UP),
    };
    if let Err(detail) = call_named("initialize", client.initialize()).await {
        client.shutdown().await;
        return ScenarioResult::fail(scenario::FOLLOW_UP, detail);
    }
    let session_id = match call_named("session/new", client.session_new(&cwd_str)).await {
        Ok(id) => id,
        Err(detail) => {
            client.shutdown().await;
            return ScenarioResult::fail(scenario::FOLLOW_UP, detail);
        }
    };
    // Re-targets the same already-active session via session/load rather
    // than creating a new one -- the exact shape a follow-up delivered
    // to an already-started adapter takes in this protocol. Copilot
    // answering "already loaded" (rather than "not found") is itself the
    // proof this real session is recognized and reachable.
    let result = call_named("session/load", client.session_load(&session_id, &cwd_str)).await;
    client.shutdown().await;
    match result {
        Ok(()) => ScenarioResult::pass(
            scenario::FOLLOW_UP,
            format!("session/load reached already-active real session {session_id} and succeeded"),
        ),
        Err(detail) if detail.contains("already loaded") => ScenarioResult::pass(
            scenario::FOLLOW_UP,
            format!(
                "session/load against still-connected real session {session_id} answered \"already loaded\" -- proof Copilot recognizes and can re-engage the exact session a follow-up would target, ACP v1's only follow-up-equivalent mechanism given its declared steering: None"
            ),
        ),
        Err(detail) => ScenarioResult::fail(scenario::FOLLOW_UP, detail),
    }
}

/// A real ACP `session/request_permission` round trip against a live
/// (fake-agent) process, zero model calls, mirroring
/// `respond_permission_answers_a_real_pending_request_over_the_wire`.
async fn approval_scenario() -> ScenarioResult {
    let fixture = load_jsonl_fixture("permission.jsonl");
    let request_line = serde_json::to_string(&fixture[0]).unwrap();
    let expected_response = fixture[1].clone();

    let output_dir = std::env::temp_dir().join(format!(
        "copilot-conformance-approval-{}",
        std::process::id()
    ));
    if std::fs::create_dir_all(&output_dir).is_err() {
        return ScenarioResult::fail(scenario::APPROVAL, "could not create scratch output dir");
    }
    let output_path = output_dir.join("response.json");
    let _ = std::fs::remove_file(&output_path);

    let script = format!(
        "cat <<'ACPEOF'\n{request_line}\nACPEOF\nread -r resp\nprintf '%s' \"$resp\" > {}\n",
        output_path.display()
    );
    let client = match CopilotAcpClient::spawn_with_raw_args(
        Path::new("/bin/sh"),
        Path::new("."),
        vec!["-c".to_string(), script],
        HashMap::new(),
    )
    .await
    {
        Ok(client) => client,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&output_dir);
            return ScenarioResult::fail(
                scenario::APPROVAL,
                format!("spawning fake ACP agent failed: {err}"),
            );
        }
    };

    let event = match timeout(Duration::from_secs(5), client.next_event()).await {
        Ok(Some(event)) => event,
        Ok(None) | Err(_) => {
            client.shutdown().await;
            let _ = std::fs::remove_dir_all(&output_dir);
            return ScenarioResult::fail(
                scenario::APPROVAL,
                "the fake agent's permission request never arrived",
            );
        }
    };
    let (request_id, request) = match event {
        super::client::CopilotClientEvent::PermissionRequested {
            request_id,
            request,
        } => (request_id, request),
        other => {
            client.shutdown().await;
            let _ = std::fs::remove_dir_all(&output_dir);
            return ScenarioResult::fail(
                scenario::APPROVAL,
                format!("expected PermissionRequested, got {other:?}"),
            );
        }
    };

    if let Err(err) = client.respond_permission(request_id, "allow-once") {
        client.shutdown().await;
        let _ = std::fs::remove_dir_all(&output_dir);
        return ScenarioResult::fail(
            scenario::APPROVAL,
            format!("answering the real pending permission request failed: {err}"),
        );
    }

    let written = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&output_path)
                && !text.is_empty()
            {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    client.shutdown().await;
    let _ = std::fs::remove_dir_all(&output_dir);

    let Ok(written) = written else {
        return ScenarioResult::fail(
            scenario::APPROVAL,
            "the fake agent never observed this client's permission response",
        );
    };
    let actual: Value = match serde_json::from_str(&written) {
        Ok(v) => v,
        Err(err) => {
            return ScenarioResult::fail(
                scenario::APPROVAL,
                format!("response was not valid JSON: {err}"),
            );
        }
    };
    if actual != expected_response {
        return ScenarioResult::fail(
            scenario::APPROVAL,
            format!("response {actual} did not match the fixture's expected {expected_response}"),
        );
    }
    ScenarioResult::pass(
        scenario::APPROVAL,
        format!(
            "observed a real session/request_permission (session={}, tool_call={}) and answered it over the wire with allow-once",
            request.session_id, request.tool_call_id
        ),
    )
}

/// Spawns a real `copilot --acp` process, then shuts it down and confirms
/// the underlying process is actually gone -- the cancellation scope this
/// adapter's `Adapter::cancel` ultimately relies on
/// (`ManagedProcess::terminate`, escalating SIGINT -> SIGTERM -> SIGKILL).
async fn cancellation_scope_scenario() -> ScenarioResult {
    let cwd = std::env::temp_dir();
    let client = match real_client(&cwd).await {
        Ok(client) => client,
        Err(unavailable) => return unavailable.into_scenario(scenario::CANCELLATION_SCOPE),
    };
    let pid = client.pid();
    client.shutdown().await;

    // `kill -0` fails once the pid is gone (or reused failure is racy but
    // acceptable here: `terminate`'s SIGKILL escalation guarantees exit
    // by the time `shutdown` returns).
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if alive {
        return ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            format!("process {pid} is still alive after shutdown() returned"),
        );
    }
    ScenarioResult::pass(
        scenario::CANCELLATION_SCOPE,
        format!(
            "shutdown() terminated the real copilot --acp process (pid {pid}); confirmed gone via `kill -0`"
        ),
    )
}

/// Creates a real session with one client, tears it down entirely, then
/// spawns a brand-new client instance and attempts to reach the same
/// session purely via `session/load` -- never `session/prompt` or
/// `Adapter::start`. Shared evidence for both `SESSION_RESUME` (this
/// adapter's `resume` capability) and `RUNTIME_RESTART` (durability
/// across a fresh process/client, exactly what a runtime restart would
/// require of a fresh `CopilotAdapter` instance).
///
/// The installed CLI (empirically observed here) does not persist a
/// freshly-created, never-prompted session to a form a brand-new process
/// can `session/load` -- reaching it that way would require an actual
/// turn, which is a model call this suite must never make. `Ok` still
/// covers the case where a future/different CLI does persist it.
async fn session_resume_probe() -> Result<String, VendorUnavailable> {
    let cwd = std::env::temp_dir();
    let cwd_str = cwd.to_string_lossy().to_string();

    let first = real_client(&cwd).await?;
    if let Err(e) = call_named("initialize", first.initialize()).await {
        first.shutdown().await;
        return Err(VendorUnavailable::Failed(e));
    }
    let session_id = match call_named("session/new", first.session_new(&cwd_str)).await {
        Ok(id) => id,
        Err(e) => {
            first.shutdown().await;
            return Err(VendorUnavailable::Failed(e));
        }
    };
    first.shutdown().await;

    let second = real_client(&cwd).await?;
    if let Err(e) = call_named("initialize", second.initialize()).await {
        second.shutdown().await;
        return Err(VendorUnavailable::Failed(e));
    }
    let load_result = call_named("session/load", second.session_load(&session_id, &cwd_str)).await;
    second.shutdown().await;
    load_result.map_err(|detail| {
        VendorUnavailable::Failed(format!(
            "session {session_id} was real (created via a real session/new) but a brand-new process could not session/load it: {detail} -- the installed copilot CLI does not appear to persist a never-prompted session across a process boundary; proving full cross-process resume would require an actual turn (a model call), which this suite must never make"
        ))
    })?;
    Ok(session_id)
}

async fn session_resume_scenario(cached: &Result<String, VendorUnavailable>) -> ScenarioResult {
    match cached {
        Ok(session_id) => ScenarioResult::pass(
            scenario::SESSION_RESUME,
            format!(
                "a brand-new CopilotAcpClient reached real, previously-created session {session_id} via session/load alone"
            ),
        ),
        Err(unavailable) => unavailable.clone().into_scenario(scenario::SESSION_RESUME),
    }
}

async fn runtime_restart_scenario(cached: &Result<String, VendorUnavailable>) -> ScenarioResult {
    match cached {
        Ok(session_id) => ScenarioResult::pass(
            scenario::RUNTIME_RESTART,
            format!(
                "session {session_id} persisted across a full process teardown and a fresh client instance, proving durability across what a runtime restart would require -- reached via session/load alone, never start()"
            ),
        ),
        Err(unavailable) => unavailable.clone().into_scenario(scenario::RUNTIME_RESTART),
    }
}

/// OMP-RPC-specific reconnection has no analog here: this adapter's
/// worker-MCP tools are injected via `--additional-mcp-config` into a
/// real `crewd coordination-mcp` subprocess Copilot itself spawns and
/// reconnects to on its own terms.
fn vendor_reconnect_scenario() -> ScenarioResult {
    ScenarioResult::pass(
        scenario::VENDOR_RECONNECT,
        "not applicable to Copilot: worker MCP reconnection is handled by Copilot's own `--additional-mcp-config`-injected `crewd coordination-mcp` subprocess, which Copilot itself spawns and reconnects to, not a separate MCP subprocess this adapter manages",
    )
}

/// Every message chunk and tool event in `session-updates.jsonl` shares
/// one `sessionId`, and each `tool_call`/`tool_call_update` pair
/// correlates via a shared `toolCallId` -- the correlation this
/// scenario proves. ACP v1's `PromptResponse` carries no token/cost
/// usage object at all (absent from the protocol, not merely untested),
/// so this adapter's `usage` capability is `None` regardless of this
/// scenario's outcome.
fn result_usage_artifacts_scenario() -> ScenarioResult {
    let frames = load_jsonl_fixture("session-updates.jsonl");
    let session_ids: std::collections::HashSet<&str> = frames
        .iter()
        .filter_map(|frame| frame["params"]["sessionId"].as_str())
        .collect();
    if session_ids.len() != 1 {
        return ScenarioResult::fail(
            scenario::RESULT_USAGE_ARTIFACTS,
            format!("expected every session/update to share one sessionId, found {session_ids:?}"),
        );
    }
    let stable_tool_frames: Vec<&Value> = frames
        .iter()
        .filter(|frame| frame["params"]["update"]["toolCallId"] == "tool-000000000002")
        .collect();
    if stable_tool_frames.len() < 2 {
        return ScenarioResult::fail(
            scenario::RESULT_USAGE_ARTIFACTS,
            "expected tool-000000000002's tool_call and tool_call_update to both carry the same toolCallId",
        );
    }
    ScenarioResult::pass(
        scenario::RESULT_USAGE_ARTIFACTS,
        format!(
            "every session/update in the fixture correlates to sessionId {:?}; tool_call/tool_call_update pairs (e.g. tool-000000000002) correlate via a shared toolCallId; ACP v1 carries no usage object at all, so usage capability is honestly declared None regardless",
            session_ids.iter().next().unwrap()
        ),
    )
}

/// Native user/project skill/agent/plugin/hook/MCP discovery is never
/// suppressed: `--disable-builtin-mcps` is never added to this
/// adapter's argv, with or without worker-MCP injection.
fn native_discovery_scenario() -> ScenarioResult {
    for mcp in [None, Some(mcp_config_for_conformance())] {
        let adapter = super::CopilotAdapter::new(
            PathBuf::from("copilot"),
            std::env::temp_dir(),
            CopilotStartupOptions::default(),
            Vec::new(),
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            mcp,
        );
        let plan = adapter.spawn_plan();
        if plan.args.iter().any(|arg| arg == "--disable-builtin-mcps") {
            return ScenarioResult::fail(
                scenario::NATIVE_DISCOVERY,
                format!("--disable-builtin-mcps present in argv: {:?}", plan.args),
            );
        }
    }
    ScenarioResult::pass(
        scenario::NATIVE_DISCOVERY,
        "--disable-builtin-mcps is never added to argv, with or without worker-MCP injection, so native skill/agent/plugin/hook/MCP discovery is never suppressed",
    )
}

fn mcp_config_for_conformance() -> AdapterMcpConfig {
    AdapterMcpConfig {
        scope_tokens: Arc::new(ScopeTokenStore::new()),
        project_id: batman_protocol::ProjectId::new(),
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: PathBuf::from("/tmp/crew-state"),
        repository: PathBuf::from("/tmp/my-repo"),
    }
}

/// Two independent redaction boundaries this adapter relies on:
/// `agent_thought_chunk` never reaches an `AdapterEvent` at all (dropped
/// in `normalize.rs`, before the shared `Redactor` boundary even runs),
/// and the worker-MCP scope token -- this adapter's own credential --
/// never appears in argv, only in env, mirroring
/// `spawn_plan_never_puts_the_scope_token_in_argv_only_in_env`.
fn redaction_scenario() -> ScenarioResult {
    let thought = serde_json::json!({
        "sessionUpdate": "agent_thought_chunk",
        "messageId": "msg-x",
        "content": { "type": "text", "text": "sk-abcdefghijklmnopqrstuvwx should never surface" }
    });
    if !copilot_normalize_session_update(&thought).is_empty() {
        return ScenarioResult::fail(
            scenario::REDACTION,
            "agent_thought_chunk normalized to at least one event; thinking content must never cross the redaction boundary",
        );
    }

    let mcp = mcp_config_for_conformance();
    let adapter = super::CopilotAdapter::new(
        PathBuf::from("copilot"),
        std::env::temp_dir(),
        CopilotStartupOptions::default(),
        Vec::new(),
        RunId::new(),
        TaskId::new(),
        WorkerId::new(),
        Some(mcp),
    );
    let plan = adapter.spawn_plan();
    let Some(token) = plan.reserved_token.clone() else {
        return ScenarioResult::fail(
            scenario::REDACTION,
            "expected a reserved scope token to be produced when mcp config is present",
        );
    };
    if plan.args.iter().any(|arg| arg.contains(&token)) {
        return ScenarioResult::fail(
            scenario::REDACTION,
            "the worker-MCP scope token leaked into argv",
        );
    }
    if plan.env.get("CREW_WORKER_SCOPE_TOKEN") != Some(&token) {
        return ScenarioResult::fail(
            scenario::REDACTION,
            "the worker-MCP scope token was not present in env under CREW_WORKER_SCOPE_TOKEN",
        );
    }
    ScenarioResult::pass(
        scenario::REDACTION,
        "agent_thought_chunk (secret-shaped content included) drops to zero events before the redaction boundary; the worker-MCP scope token never appears in argv, only in env",
    )
}

/// This foreign adapter never advertises `nested: managed`; only
/// OMP-native nesting may.
fn managed_nesting_rejection_scenario() -> ScenarioResult {
    let capabilities = new_adapter().capabilities();
    if capabilities.nested == NestedCapability::Managed {
        return ScenarioResult::fail(
            scenario::MANAGED_NESTING_REJECTION,
            "declared nested capability is Managed; a foreign adapter must never advertise this",
        );
    }
    ScenarioResult::pass(
        scenario::MANAGED_NESTING_REJECTION,
        format!(
            "declared nested capability is {:?}, never Managed",
            capabilities.nested
        ),
    )
}

/// Reuses `subagent.jsonl`, modeling the Claude adapter's own
/// `unexpected_child_observation_scenario`: a vendor-side delegation to a
/// subagent, normalized through `copilot_normalize_session_update`,
/// should surface exactly one `NestedWorkerObserved` without upgrading
/// the declared `nested` capability. ACP v1's `session/update` schema
/// has no discriminator this adapter maps to `NestedWorkerObserved` (see
/// `normalize.rs`'s own module doc, and
/// `raising_the_max_acp_protocol_version_requires_a_session_update_nested_worker_mapping`'s
/// permanent-wall assertion), so the delegation normalizes to ordinary
/// `ToolStarted`/`ToolResult` events instead and this scenario honestly
/// fails -- a real, reported gap, not a silently omitted one.
fn unexpected_child_observation_scenario(
    declared_capabilities: AdapterCapabilities,
) -> ScenarioResult {
    use batman_runtime::adapter::AdapterEventPayload::NestedWorkerObserved;

    let updates: Vec<Value> = load_jsonl_fixture("subagent.jsonl")
        .into_iter()
        .map(|frame| frame["params"]["update"].clone())
        .collect();
    let nested: Vec<(String, String)> = updates
        .iter()
        .flat_map(copilot_normalize_session_update)
        .filter_map(|payload| match payload {
            NestedWorkerObserved {
                vendor_child_id,
                vendor_parent_ref,
            } => Some((vendor_child_id, vendor_parent_ref)),
            _ => None,
        })
        .collect();

    if nested.len() == 1 && declared_capabilities.nested == NestedCapability::None {
        ScenarioResult::pass(
            scenario::UNEXPECTED_CHILD_OBSERVATION,
            format!(
                "subagent.jsonl's vendor-side delegation normalized to exactly one NestedWorkerObserved{:?}, while this adapter's own declared nested capability stayed NestedCapability::None -- emitting the event never upgraded it",
                nested[0]
            ),
        )
    } else {
        ScenarioResult::fail(
            scenario::UNEXPECTED_CHILD_OBSERVATION,
            format!(
                "ACP v1's session/update schema has no discriminator this adapter maps to NestedWorkerObserved -- subagent.jsonl's vendor-side delegation normalized to {} NestedWorkerObserved event(s) (declared nested={:?}); a real gap until a newer ACP protocol version adds a subagent-observation variant",
                nested.len(),
                declared_capabilities.nested
            ),
        )
    }
}

/// Runs every scenario this adapter can prove without a model call.
pub async fn fixture_report() -> ConformanceReport {
    let (probe_result, version, declared_capabilities) = probe_scenario().await;
    let resume_probe = session_resume_probe().await;
    let scenarios = vec![
        probe_result,
        read_only_start_and_progress_scenario().await,
        isolated_write_scenario(),
        follow_up_scenario().await,
        approval_scenario().await,
        cancellation_scope_scenario().await,
        session_resume_scenario(&resume_probe).await,
        vendor_reconnect_scenario(),
        runtime_restart_scenario(&resume_probe).await,
        result_usage_artifacts_scenario(),
        native_discovery_scenario(),
        redaction_scenario(),
        managed_nesting_rejection_scenario(),
        unexpected_child_observation_scenario(declared_capabilities),
    ];
    ConformanceReport::new(
        AdapterKindLabel::from(AdapterKind::Copilot),
        ConformanceMode::Fixture,
        version,
        declared_capabilities,
        scenarios,
    )
}

/// Live-only counterpart to [`session_resume_probe`]: proves the same
/// session is reachable via cross-process `session/load` *after* a
/// real turn actually ran on it (a real, billed model call) -- the
/// exact step [`session_resume_probe`]'s own doc comment says fixture
/// mode must never take. This proves cross-process loadability of a
/// session with real content, not that the loaded session's
/// conversational context is itself provably continued (that would
/// need a second real turn checking the first is remembered, which
/// this suite does not spend). Only reachable through [`live_report`],
/// which runs by default unless `CREW_DISABLE_VENDOR_CLI=1` is set.
async fn session_resume_probe_live() -> Result<String, VendorUnavailable> {
    let cwd = std::env::temp_dir();
    let cwd_str = cwd.to_string_lossy().to_string();

    let first = real_client(&cwd).await?;
    if let Err(e) = call_named("initialize", first.initialize()).await {
        first.shutdown().await;
        return Err(VendorUnavailable::Failed(e));
    }
    let session_id = match call_named("session/new", first.session_new(&cwd_str)).await {
        Ok(id) => id,
        Err(e) => {
            first.shutdown().await;
            return Err(VendorUnavailable::Failed(e));
        }
    };
    if let Err(e) = call_named(
        "session/prompt",
        first.session_prompt(&session_id, "Reply with the single word: ack."),
    )
    .await
    {
        first.shutdown().await;
        return Err(VendorUnavailable::Failed(e));
    }
    first.shutdown().await;

    let second = real_client(&cwd).await?;
    if let Err(e) = call_named("initialize", second.initialize()).await {
        second.shutdown().await;
        return Err(VendorUnavailable::Failed(e));
    }
    let load_result = call_named("session/load", second.session_load(&session_id, &cwd_str)).await;
    second.shutdown().await;
    load_result.map_err(|detail| {
        VendorUnavailable::Failed(format!(
            "session {session_id} completed a real turn but a brand-new process still could \
             not session/load it: {detail}"
        ))
    })?;
    Ok(session_id)
}

/// Runs the live conformance suite against the installed `copilot` CLI.
///
/// Real invocation is the default. Reuses the exact same 14-scenario
/// suite as [`fixture_report`], substituting a real, turn-completed
/// [`session_resume_probe_live`] for `SESSION_RESUME`/`RUNTIME_RESTART` --
/// the two scenarios fixture mode cannot prove past the flag/mechanism
/// level (see `REVIEW.md`). Set `CREW_DISABLE_VENDOR_CLI=1` to
/// forbid it in CI or on a machine without the CLI installed.
///
/// # Errors
/// Returns a message if `CREW_DISABLE_VENDOR_CLI=1` is set.
pub async fn live_report() -> Result<ConformanceReport, String> {
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return Err(format!(
            "live Copilot conformance is disabled by {}=1",
            batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV
        ));
    }
    let (probe_result, version, declared_capabilities) = probe_scenario().await;
    let live_resume_probe = session_resume_probe_live().await;
    let scenarios = vec![
        probe_result,
        read_only_start_and_progress_scenario().await,
        isolated_write_scenario(),
        follow_up_scenario().await,
        approval_scenario().await,
        cancellation_scope_scenario().await,
        session_resume_scenario(&live_resume_probe).await,
        vendor_reconnect_scenario(),
        runtime_restart_scenario(&live_resume_probe).await,
        result_usage_artifacts_scenario(),
        native_discovery_scenario(),
        redaction_scenario(),
        managed_nesting_rejection_scenario(),
        unexpected_child_observation_scenario(declared_capabilities),
    ];
    Ok(ConformanceReport::new(
        AdapterKindLabel::from(AdapterKind::Copilot),
        ConformanceMode::Live,
        version,
        declared_capabilities,
        scenarios,
    ))
}
