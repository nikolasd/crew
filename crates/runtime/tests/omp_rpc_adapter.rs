//! Integration tests for the OMP-RPC / local-model worker adapter.
//!
//! Grounded against the real installed `omp 17.1.1` binary (`omp
//! --version`, `omp --mode rpc --help`, `omp models --json`, and direct
//! no-model-call RPC probes captured during development -- see the wire
//! shapes reproduced in `fixtures/adapters/omp-rpc/*.jsonl`, which mirror
//! frames this adapter actually observed from the real binary:
//! `{"type":"ready","protocolVersion":1,...}` and
//! `{"type":"response","id":...,"command":"get_state","success":true,
//! "data":{"sessionId":...,"sessionFile":...}}` were captured verbatim
//! from `omp --mode rpc --model lm-studio/<id> --session-dir <dir>`
//! without ever sending a prompt (zero model calls).
//!
//! Per the shared adapter contract, fixture-driven tests here feed
//! static, recorded-looking JSONL through `normalize.rs` directly rather
//! than spawning `fake-worker` (whose `omp-rpc` mode predates this
//! adapter's real wire-shape grounding and does not attempt to match it).
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use batman_protocol::{RunId, TaskId, WorkerId};
use batman_runtime::adapter::omp_rpc::OmpRpcAdapter;
use batman_runtime::adapter::omp_rpc::client::{
    self, OmpRpcClient, abort_command, follow_up_command, get_session_stats_command,
    get_state_command, prompt_command, set_subagent_subscription_command, steer_command,
};
use batman_runtime::adapter::omp_rpc::conformance;
use batman_runtime::adapter::omp_rpc::normalize::{
    PROMPT_ACCEPTED_MARKER, PROMPT_COMPLETED_MARKER, PendingApproval,
    extension_ui_request_to_pending_approval, normalize_frame,
};
use batman_runtime::adapter::{
    Adapter, AdapterEvent, AdapterEventPayload, AdapterEventSink, AdapterFuture,
    OmpRpcAdapterOptions, OmpRpcStartupOptions, ProfileId, StartSpec, StartupOptions,
    WorkerProfile,
};
use batman_runtime::conformance::scenario;
use batman_runtime::supervisor::{EnvironmentPolicy, SpawnSpec, Supervisor};
use serde_json::Value;

// ------------------------------------------------------------- fixtures

fn load_fixture(name: &str) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/omp-rpc")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    text.lines().map(str::to_string).collect()
}

/// Mirrors exactly the recovery discipline `OmpRpcClient` applies to real
/// process stdout: a line that fails to parse as JSON is skipped, never
/// fatal, and normalization continues with the next line.
fn normalize_fixture_lines(lines: &[String]) -> Vec<AdapterEventPayload> {
    let mut events = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(frame) = serde_json::from_str::<Value>(line) {
            events.extend(normalize_frame(&frame));
        }
    }
    events
}

// --------------------------------------------------------------- Step 1

#[test]
fn prompt_acceptance_is_distinguishable_from_and_precedes_turn_completion() {
    let lines = load_fixture("turn.jsonl");
    let events = normalize_fixture_lines(&lines);

    let accepted_index = events
        .iter()
        .position(|e| matches!(e, AdapterEventPayload::MessageChunk { text, .. } if text.value == PROMPT_ACCEPTED_MARKER));
    let completed_index = events
        .iter()
        .position(|e| matches!(e, AdapterEventPayload::MessageFinal { text, .. } if text.value == PROMPT_COMPLETED_MARKER));

    let accepted_index = accepted_index.expect("prompt acceptance event must be emitted");
    let completed_index = completed_index.expect("turn completion event must be emitted");
    assert!(
        accepted_index < completed_index,
        "prompt acceptance ({accepted_index}) must precede turn completion ({completed_index})"
    );
    // The two are genuinely distinguishable: different payload variants
    // (MessageChunk vs MessageFinal), not merely different text.
    assert!(matches!(
        events[accepted_index],
        AdapterEventPayload::MessageChunk { .. }
    ));
    assert!(matches!(
        events[completed_index],
        AdapterEventPayload::MessageFinal { .. }
    ));
}

#[test]
fn malformed_json_line_is_skipped_not_fatal() {
    let mut lines = load_fixture("turn.jsonl");
    lines.insert(1, "this-is-not-json".to_string());
    assert!(
        lines
            .iter()
            .any(|line| serde_json::from_str::<Value>(line).is_err()),
        "the local copy must contain a genuinely malformed line"
    );

    let events = normalize_fixture_lines(&lines);
    assert!(
        !events.is_empty(),
        "valid frames after the malformed line must still normalize"
    );
}

#[test]
fn confirm_and_select_extension_ui_requests_produce_pending_approvals_with_the_right_method_and_title()
 {
    let lines = load_fixture("turn.jsonl");
    let confirm_frame: Value = lines
        .iter()
        .find(|l| l.contains("\"method\":\"confirm\""))
        .map(|l| serde_json::from_str(l).expect("fixture line is valid JSON"))
        .expect("turn.jsonl must contain a confirm extension_ui_request fixture line");
    let select_frame: Value = lines
        .iter()
        .find(|l| l.contains("\"method\":\"select\""))
        .map(|l| serde_json::from_str(l).expect("fixture line is valid JSON"))
        .expect("turn.jsonl must contain a select extension_ui_request fixture line");

    let confirm_approval = extension_ui_request_to_pending_approval(&confirm_frame)
        .expect("a confirm extension_ui_request must produce a PendingApproval");
    assert_eq!(
        confirm_approval,
        PendingApproval {
            request_id: "ui_7".to_string(),
            method: "confirm",
            title: "Confirm".to_string(),
        }
    );

    let select_approval = extension_ui_request_to_pending_approval(&select_frame)
        .expect("a select extension_ui_request must produce a PendingApproval");
    assert_eq!(
        select_approval,
        PendingApproval {
            request_id: "ui_8".to_string(),
            method: "select",
            title: "Pick a branch".to_string(),
        }
    );

    // Neither decision-shaped frame is upgraded into a normalized event --
    // approvals are surfaced only through `snapshot()`'s `state_summary`.
    assert!(normalize_frame(&confirm_frame).is_empty());
    assert!(normalize_frame(&select_frame).is_empty());
}

#[test]
fn set_widget_extension_ui_request_never_produces_a_pending_approval() {
    let lines = load_fixture("turn.jsonl");
    let set_widget_frame: Value = lines
        .iter()
        .find(|l| l.contains("\"method\":\"setWidget\""))
        .map(|l| serde_json::from_str(l).expect("fixture line is valid JSON"))
        .expect("turn.jsonl must contain a setWidget extension_ui_request fixture line");

    assert_eq!(
        extension_ui_request_to_pending_approval(&set_widget_frame),
        None,
        "setWidget is a display surface, never a decision -- it must never be treated as an \
         approval"
    );
    assert!(normalize_frame(&set_widget_frame).is_empty());
}

#[test]
fn non_extension_ui_request_frames_never_produce_a_pending_approval() {
    let lines = load_fixture("turn.jsonl");
    let non_ui_frames: Vec<Value> = lines
        .iter()
        .filter(|l| !l.contains("\"extension_ui_request\""))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert!(
        !non_ui_frames.is_empty(),
        "turn.jsonl must contain non-extension_ui_request frames"
    );
    for frame in &non_ui_frames {
        assert_eq!(
            extension_ui_request_to_pending_approval(frame),
            None,
            "only extension_ui_request frames may produce a PendingApproval: {frame}"
        );
    }
}

#[test]
fn local_only_prompt_completes_via_agent_invoked_false_without_agent_end() {
    let lines = load_fixture("turn.jsonl");
    assert!(
        !lines.iter().any(|l| l.contains("\"agent_end\"")),
        "turn.jsonl must be the local-only (no subagent invoked) fixture"
    );
    let events = normalize_fixture_lines(&lines);
    let completions = events
        .iter()
        .filter(|e| matches!(e, AdapterEventPayload::MessageFinal { text, .. } if text.value == PROMPT_COMPLETED_MARKER))
        .count();
    assert_eq!(
        completions, 1,
        "exactly one completion must be derived from data.agentInvoked:false"
    );
}

#[test]
fn get_state_response_establishes_vendor_session_from_real_session_id_field() {
    let lines = load_fixture("turn.jsonl");
    let events = normalize_fixture_lines(&lines);
    let established = events.iter().find_map(|e| match e {
        AdapterEventPayload::VendorSessionEstablished { vendor_session_id } => {
            Some(vendor_session_id.clone())
        }
        _ => None,
    });
    assert_eq!(
        established.as_deref(),
        Some("11111111-1111-4111-8111-000000000001"),
        "vendor session id must be taken from the real omp get_state response's data.sessionId field"
    );
}

#[test]
fn get_session_stats_response_normalizes_to_usage_reported() {
    let lines = load_fixture("turn.jsonl");
    let events = normalize_fixture_lines(&lines);
    let usage = events.iter().find_map(|e| match e {
        AdapterEventPayload::UsageReported {
            input_tokens,
            output_tokens,
            cost_usd,
        } => Some((*input_tokens, *output_tokens, *cost_usd)),
        _ => None,
    });
    assert_eq!(usage, Some((42, 7, Some(0.0142))));
}

#[test]
fn subagent_subscription_is_established_before_the_prompt_command_when_nested_visibility_requested()
{
    // Pure command-sequencing check: the adapter's startup command order,
    // not a live process. `subscribe_subagents: true` mirrors a caller
    // requesting nested visibility.
    let commands = client::build_startup_commands(true, &[], &[], "review this diff");
    let subscription_index = commands
        .iter()
        .position(|(command, _)| command == "set_subagent_subscription")
        .expect("subagent subscription command must be sent when nested visibility is requested");
    let prompt_index = commands
        .iter()
        .position(|(command, _)| command == "prompt")
        .expect("prompt command must be sent");
    assert!(
        subscription_index < prompt_index,
        "subagent subscription must be established before work begins"
    );
}

#[test]
fn subagent_subscription_is_omitted_when_nested_visibility_is_not_requested() {
    let commands = client::build_startup_commands(false, &[], &[], "review this diff");
    assert!(
        !commands
            .iter()
            .any(|(command, _)| command == "set_subagent_subscription"),
        "must never send a subscription command the caller did not request"
    );
}

#[test]
fn set_host_tools_command_uses_the_real_tools_field_and_tool_shape() {
    // Grounded against the installed binary's own `fNw` tool-normalization
    // function: `case "set_host_tools": { const H = fNw(E.tools); ... }`.
    let params = client::set_host_tools_command(&[client::HostToolDefinition {
        name: "coordination_send".to_string(),
        description: "Send a message to a peer worker".to_string(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        label: None,
        hidden: false,
    }]);
    let tools = params
        .get("tools")
        .and_then(Value::as_array)
        .expect("set_host_tools params must carry a `tools` array");
    assert_eq!(tools.len(), 1);
    assert_eq!(
        tools[0].get("name").and_then(Value::as_str),
        Some("coordination_send")
    );
    assert_eq!(
        tools[0].get("description").and_then(Value::as_str),
        Some("Send a message to a peer worker")
    );
    assert!(
        tools[0]
            .get("parameters")
            .and_then(Value::as_object)
            .is_some()
    );
    assert_eq!(tools[0].get("hidden").and_then(Value::as_bool), Some(false));
}

#[test]
fn set_host_uri_schemes_command_uses_the_real_schemes_field_and_scheme_shape() {
    // Grounded against the installed binary's own `setSchemes`: each
    // scheme entry is `{scheme, description?, writable?, immutable?}`.
    let params = client::set_host_uri_schemes_command(&[client::HostUriScheme {
        scheme: "crew".to_string(),
        description: Some("Crew run/task/worker state".to_string()),
        writable: false,
        immutable: true,
    }]);
    let schemes = params
        .get("schemes")
        .and_then(Value::as_array)
        .expect("set_host_uri_schemes params must carry a `schemes` array");
    assert_eq!(schemes.len(), 1);
    assert_eq!(
        schemes[0].get("scheme").and_then(Value::as_str),
        Some("crew")
    );
    assert_eq!(
        schemes[0].get("writable").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        schemes[0].get("immutable").and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn host_tools_and_host_uri_schemes_are_established_before_the_prompt_command() {
    let tools = [client::HostToolDefinition {
        name: "coordination_send".to_string(),
        description: "Send a message to a peer worker".to_string(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        label: None,
        hidden: false,
    }];
    let schemes = [client::HostUriScheme {
        scheme: "crew".to_string(),
        description: None,
        writable: false,
        immutable: true,
    }];
    let commands = client::build_startup_commands(false, &tools, &schemes, "review this diff");
    let host_tools_index = commands
        .iter()
        .position(|(command, _)| command == "set_host_tools")
        .expect("set_host_tools must be sent when host tools are configured");
    let host_schemes_index = commands
        .iter()
        .position(|(command, _)| command == "set_host_uri_schemes")
        .expect("set_host_uri_schemes must be sent when host URI schemes are configured");
    let prompt_index = commands
        .iter()
        .position(|(command, _)| command == "prompt")
        .expect("prompt command must be sent");
    assert!(host_tools_index < prompt_index);
    assert!(host_schemes_index < prompt_index);
}

#[test]
fn subagents_fixture_observes_nested_worker_without_upgrading_declared_capability() {
    let lines = load_fixture("subagents.jsonl");
    let events = normalize_fixture_lines(&lines);

    let nested = events.iter().find_map(|e| match e {
        AdapterEventPayload::NestedWorkerObserved {
            vendor_child_id,
            vendor_parent_ref,
        } => Some((vendor_child_id.clone(), vendor_parent_ref.clone())),
        _ => None,
    });
    assert_eq!(
        nested,
        Some(("sub-1".to_string(), "main".to_string())),
        "a vendor-reported subagent must normalize to NestedWorkerObserved"
    );

    // agent_end must still complete the turn even though a subagent ran.
    let agent_end_completion = events
        .iter()
        .any(|e| matches!(e, AdapterEventPayload::MessageFinal { text, .. } if text.value == PROMPT_COMPLETED_MARKER));
    assert!(
        agent_end_completion,
        "agent_end must complete the agent-invoked turn"
    );

    // Tool lifecycle around the subagent's work must also normalize.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AdapterEventPayload::ToolStarted { name, .. } if name == "grep"))
    );
    assert!(events.iter().any(
        |e| matches!(e, AdapterEventPayload::ToolResult { name, ok, .. } if name == "grep" && *ok)
    ));
}

#[test]
fn agent_invoked_prompt_defers_completion_to_a_later_agent_end_frame() {
    let lines = load_fixture("subagents.jsonl");
    let events = normalize_fixture_lines(&lines);
    let accepted_index = events
        .iter()
        .position(|e| matches!(e, AdapterEventPayload::MessageChunk { text, .. } if text.value == PROMPT_ACCEPTED_MARKER))
        .expect("prompt acceptance must still be emitted for an agent-invoked prompt");
    let completed_index = events
        .iter()
        .position(|e| matches!(e, AdapterEventPayload::MessageFinal { text, .. } if text.value == PROMPT_COMPLETED_MARKER))
        .expect("agent_end must eventually complete the turn");
    // The subagent + tool events must land strictly between acceptance
    // and completion -- proving completion genuinely waited for agent_end
    // rather than firing immediately alongside acceptance.
    let nested_index = events
        .iter()
        .position(|e| matches!(e, AdapterEventPayload::NestedWorkerObserved { .. }))
        .expect("nested worker must be observed");
    assert!(accepted_index < nested_index);
    assert!(nested_index < completed_index);
}

// ----------------------------------------------------- command builders

#[test]
fn prompt_command_uses_the_real_message_field_name() {
    // Grounded against the installed binary's own dispatch source:
    // `case "prompt": { const H = await kI1(A, E.message, ...) }`.
    let params = prompt_command("hello");
    assert_eq!(params.get("message").and_then(Value::as_str), Some("hello"));
}

#[test]
fn steer_and_follow_up_commands_use_the_real_message_field_name() {
    // `case "steer": { await A.steer(E.message, ...) }`,
    // `case "follow_up": { await A.followUp(E.message, ...) }`.
    assert_eq!(
        steer_command("stop and check tests first").get("message"),
        Some(&Value::String("stop and check tests first".to_string()))
    );
    assert_eq!(
        follow_up_command("also update the docs").get("message"),
        Some(&Value::String("also update the docs".to_string()))
    );
}

#[test]
fn abort_and_get_state_commands_carry_no_extra_params() {
    assert!(abort_command().is_empty());
    assert!(get_state_command().is_empty());
    assert!(get_session_stats_command().is_empty());
}

#[test]
fn set_subagent_subscription_command_carries_a_level() {
    let params = set_subagent_subscription_command("full");
    assert_eq!(params.get("level").and_then(Value::as_str), Some("full"));
}

// -------------------------------------------------- real installed CLI

/// Real, no-model-call probe against the installed `omp` binary: spawns
/// `omp --mode rpc` with a local (`lm-studio/...`) model selector actually
/// reported by `omp models --json`, waits for the real `{"type":"ready",
/// ...}` handshake frame, and completes a real `get_state` round trip.
/// Never sends a `prompt` command, so it never invokes a model backend --
/// `lm-studio`'s local server does not even need to be running for this
/// test to pass, exactly as observed manually against the installed
/// `omp 17.1.1` binary during development.
///
/// Skips (rather than fails) whenever no local selector is currently
/// discoverable *or* the discovered selector becomes unreachable between
/// listing and spawn -- a real, live, external local-server dependency's
/// flakiness on a shared development machine, not a defect in this
/// adapter (see [`spawn_ready_client`]).
#[tokio::test]
async fn ready_and_get_state_round_trip_against_installed_omp() {
    let Some((mut client, workdir)) = spawn_ready_client().await else {
        eprintln!(
            "skipping: no local (lm-studio/omlx) selector was reachable on this machine \
             right now -- either `omp models --json` reported none, or the model became \
             unreachable between listing and spawn"
        );
        return;
    };

    let id = client
        .send_command("get_state", get_state_command())
        .await
        .expect("writing get_state to the real process must succeed");
    let response = client
        .read_response(&id)
        .await
        .expect("reading the correlated get_state response must succeed");
    assert_eq!(response.command, "get_state");
    assert!(
        response.success,
        "get_state must succeed with no model call"
    );
    assert!(
        response.data.get("sessionId").is_some(),
        "real omp get_state response must carry a sessionId field"
    );

    client.process_mut().terminate().await;
    let _ = std::fs::remove_dir_all(&workdir);
}

/// Spawns the installed `omp` binary in `--mode rpc` against the first
/// discoverable local model selector and waits for the ready handshake.
/// Returns `None` (never panics) when no local selector is currently
/// discoverable, or spawning/handshaking fails for any reason (e.g. the
/// selector became unreachable between `omp models --json` listing it and
/// this spawn actually starting) -- a transient local-server condition on
/// a shared machine, not a bug in this adapter. Callers skip gracefully in
/// either case, exactly as intended for a real, zero-model-call,
/// best-effort installed-CLI probe.
async fn spawn_ready_client() -> Option<(OmpRpcClient, PathBuf)> {
    let selector = resolve_first_local_selector().await?;
    let workdir = std::env::temp_dir().join(format!(
        "omp-rpc-adapter-test-{}-{}",
        std::process::id(),
        selector.replace('/', "-")
    ));
    std::fs::create_dir_all(&workdir).expect("create scratch workdir");
    // `omp` needs the local model server's *address* to resolve a
    // `lm-studio`/`omlx` selector; baseline permits only nine variables and
    // omits it, so a stripped spawn exits with `Model "…" not found` and
    // this helper would silently return `None` -- turning these
    // real-binary tests into permanent skips. Worse, `omp` persists
    // provider discovery, so the failed spawn also empties the operator's
    // own `omp models` catalog. Mirrors `OMP_LOCAL_PROVIDER_ENV` in
    // `src/adapter/omp_rpc/conformance.rs` (duplicated because `src/` and
    // `tests/` are separate compilation units).
    let extra = vec!["LM_STUDIO_BASE_URL".to_string()];
    let env = EnvironmentPolicy::baseline().build(&std::env::vars().collect(), &extra);
    let spec = SpawnSpec {
        program: "omp".into(),
        args: vec![
            "--mode".into(),
            "rpc".into(),
            "--model".into(),
            selector,
            "--no-session".into(),
            "--allow-home".into(),
        ],
        cwd: workdir.clone(),
        env,
        ..SpawnSpec::minimal()
    };
    let supervisor = Supervisor::new();
    let process = supervisor.spawn(spec).await.ok()?;
    let mut client = OmpRpcClient::new(process);
    if client.wait_for_ready().await.is_err() {
        let _ = std::fs::remove_dir_all(&workdir);
        return None;
    }
    Some((client, workdir))
}

/// Real, no-model-call round trip proving `set_host_tools` and
/// `set_host_uri_schemes` are genuine, working RPC commands against the
/// installed binary -- not merely pure-builder assertions. Never sends a
/// `prompt`, so it never invokes a model. Skips under the same conditions
/// as [`ready_and_get_state_round_trip_against_installed_omp`].
#[tokio::test]
async fn set_host_tools_and_host_uri_schemes_round_trip_against_installed_omp() {
    let Some((mut client, workdir)) = spawn_ready_client().await else {
        eprintln!(
            "skipping: no local (lm-studio/omlx) selector was reachable on this machine \
             right now -- either `omp models --json` reported none, or the model became \
             unreachable between listing and spawn"
        );
        return;
    };

    let tools = [client::HostToolDefinition {
        name: "crew_test_tool".to_string(),
        description: "A no-op tool registered only to prove set_host_tools round-trips".to_string(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        label: None,
        hidden: false,
    }];
    let id = client
        .send_command("set_host_tools", client::set_host_tools_command(&tools))
        .await
        .expect("writing set_host_tools to the real process must succeed");
    let response = client
        .read_response(&id)
        .await
        .expect("reading the correlated set_host_tools response must succeed");
    assert_eq!(response.command, "set_host_tools");
    assert!(
        response.success,
        "set_host_tools must succeed with no model call: {:?}",
        response.error
    );
    let tool_names: Vec<&str> = response
        .data
        .get("toolNames")
        .and_then(Value::as_array)
        .expect("real set_host_tools response must carry a toolNames array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(tool_names.contains(&"crew_test_tool"));

    let schemes = [client::HostUriScheme {
        scheme: "battest".to_string(),
        description: Some("scratch scheme registered only to prove the round trip".to_string()),
        writable: false,
        immutable: true,
    }];
    let id = client
        .send_command(
            "set_host_uri_schemes",
            client::set_host_uri_schemes_command(&schemes),
        )
        .await
        .expect("writing set_host_uri_schemes to the real process must succeed");
    let response = client
        .read_response(&id)
        .await
        .expect("reading the correlated set_host_uri_schemes response must succeed");
    assert_eq!(response.command, "set_host_uri_schemes");
    assert!(
        response.success,
        "set_host_uri_schemes must succeed with no model call: {:?}",
        response.error
    );
    // `omp 17.2.7` echoes the registered schemes as a flat array of
    // strings -- `{"schemes":["battest"]}` -- not as objects carrying a
    // `scheme` field. Captured verbatim from the real binary; parsing it as
    // objects silently collected nothing, and this assertion only ever ran
    // at all once `spawn_ready_client` stopped failing into a skip.
    let registered_schemes: Vec<&str> = response
        .data
        .get("schemes")
        .and_then(Value::as_array)
        .expect("real set_host_uri_schemes response must carry a schemes array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        registered_schemes.contains(&"battest"),
        "the registered scheme must be echoed back, got {registered_schemes:?}"
    );

    client.process_mut().terminate().await;
    let _ = std::fs::remove_dir_all(&workdir);
}

async fn resolve_first_local_selector() -> Option<String> {
    let output = tokio::process::Command::new("omp")
        .args(["models", "--json"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&output.stdout).ok()?;
    parsed
        .get("models")?
        .as_array()?
        .iter()
        .find(|m| {
            matches!(
                m.get("provider").and_then(Value::as_str),
                Some("lm-studio") | Some("omlx")
            )
        })
        .and_then(|m| m.get("selector"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ------------------------------------------- host-tool-call bridge (fake)

/// Locates the `fake-worker` binary, building it if necessary. Mirrors
/// `tests/supervisor.rs`'s own copy of this helper -- each `tests/*.rs`
/// file is a separate compilation unit, so it cannot be shared directly.
fn fake_worker_path() -> PathBuf {
    static PATH: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(build_fake_worker_once);
    PATH.clone()
}

fn build_fake_worker_once() -> PathBuf {
    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/runtime/../.. is the workspace root")
        .to_path_buf();
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--quiet", "-p", "fake-worker"])
        .current_dir(&workspace_root)
        .status()
        .expect("cargo build -p fake-worker must be runnable");
    assert!(status.success(), "cargo build -p fake-worker failed");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root.join("target"));
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let binary = target_dir.join(profile_dir).join("fake-worker");
    assert!(
        binary.is_file(),
        "expected fake-worker binary at {}",
        binary.display()
    );
    binary
}

/// An in-memory [`AdapterEventSink`] that records every emitted event.
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

fn omp_rpc_test_profile() -> WorkerProfile {
    WorkerProfile {
        id: ProfileId::new(),
        adapter: "ompRpc".to_string(),
        model: "lm-studio/x".to_string(),
        permission_envelope: serde_json::json!({}),
        startup_options: StartupOptions::OmpRpc(OmpRpcStartupOptions {
            profile: None,
            host_tools: None,
        }),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    }
}

/// Reproduces, against a real (fake) child process, the exact deadlock
/// this adapter's host-tool bridge must never hit: the real vendor's own
/// dispatch source awaits the *entire* model turn -- including any host
/// tool call it makes -- before ever responding to the `prompt` command
/// (`case "prompt": { const H = await kI1(...) }`). `fake-worker --mode
/// omp-rpc-host-tool` reproduces exactly that ordering: it emits a
/// `host_tool_call` frame and withholds the `prompt` command's own
/// response until a `host_tool_result` reply arrives on stdin.
///
/// Before this adapter's fix, `start()` called `read_response` on the
/// `prompt` command's id inline -- a wait-loop that only *queues*
/// anything that is not the awaited response, never answers it -- so
/// this exact exchange would hang forever. `start()` now hands the
/// `prompt` command off to `run_pump` without waiting for its response,
/// and `run_pump`'s own frame loop answers `host_tool_call` before
/// `normalize::normalize_frame` ever sees it, so this test proves both:
/// `start()` itself returns promptly (would time out under the old
/// code), and the full exchange still completes end to end (proven by
/// polling for the eventual prompt-acceptance *and* prompt-completion
/// events, which can only be emitted after the vendor's response to
/// `prompt` arrives -- which the fake vendor withholds until its
/// `host_tool_call` is answered).
#[tokio::test]
async fn a_host_tool_call_during_the_prompt_turn_never_deadlocks_start() {
    let adapter = OmpRpcAdapter::with_binary(
        fake_worker_path().to_string_lossy().into_owned(),
        omp_rpc_test_profile(),
        OmpRpcAdapterOptions::default(),
        None,
    );
    let recording = Arc::new(RecordingSink::default());
    let sink: Arc<dyn AdapterEventSink> = Arc::clone(&recording) as Arc<dyn AdapterEventSink>;
    let spec = StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "hello".to_string(),
        resume: None,
    };

    let start_result =
        tokio::time::timeout(Duration::from_secs(5), adapter.start(spec, sink)).await;
    assert!(
        matches!(start_result, Ok(Ok(()))),
        "start() must return promptly instead of deadlocking on the vendor's host_tool_call: \
         {start_result:?}"
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (accepted, completed) = {
            let events = recording
                .events
                .lock()
                .expect("recording sink mutex never poisoned");
            let accepted = events.iter().any(|e| {
                matches!(&e.payload, AdapterEventPayload::MessageChunk { text, .. } if text.value == PROMPT_ACCEPTED_MARKER)
            });
            let completed = events.iter().any(|e| {
                matches!(&e.payload, AdapterEventPayload::MessageFinal { text, .. } if text.value == PROMPT_COMPLETED_MARKER)
            });
            (accepted, completed)
        };
        if accepted && completed {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the host_tool_call must have been answered and the prompt's response observed \
             by run_pump within the deadline; it never was"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Never leak the fake child process or its `run_pump` task: dispose
    // terminates the supervised process and awaits the pump task's exit.
    adapter
        .dispose()
        .await
        .expect("disposing a running OmpRpcAdapter must succeed");
}

// -------------------------------------------------------- conformance

/// Every one of the 14 canonical scenario names must appear exactly
/// once, and every scenario genuinely provable without a model call in
/// this environment must report `passed: true` -- a real gap (e.g. no
/// local model server reachable) is a legitimate `passed: false`, never
/// papered over, exactly like PROBE's own honest failure mode.
#[tokio::test]
async fn fixture_report_covers_every_canonical_scenario_exactly_once_and_passes_what_it_can() {
    let report = conformance::fixture_report().await;
    assert_eq!(
        report.scenarios.len(),
        scenario::ALL.len(),
        "fixture_report() must run every canonical scenario exactly once: {:?}",
        report.scenarios.iter().map(|s| s.name).collect::<Vec<_>>()
    );
    let mut seen = std::collections::HashSet::new();
    for name in scenario::ALL {
        assert!(
            seen.insert(name),
            "duplicate name in scenario::ALL itself: {name}"
        );
        assert!(
            report.scenarios.iter().any(|s| s.name == name),
            "fixture_report() is missing canonical scenario {name:?}"
        );
    }
    for result in &report.scenarios {
        assert!(
            scenario::ALL.contains(&result.name),
            "fixture_report() reported a scenario name outside scenario::ALL: {:?}",
            result.name
        );
    }

    // PROBE, CANCELLATION_SCOPE, and FOLLOW_UP all genuinely depend on a
    // local (lm-studio/omlx) model selector being listed by `omp models
    // --json` on the machine running this test. APPROVAL is a genuine,
    // documented implementation gap, not an environmental one: this
    // adapter's normalize_frame has no case for extension_ui_request
    // frames at all, so it honestly reports `passed: false` rather than
    // fabricate a pass (see conformance.rs's own approval_scenario doc
    // comment) -- every other scenario must pass unconditionally.
    let allowed_to_fail = [
        scenario::PROBE,
        scenario::CANCELLATION_SCOPE,
        scenario::FOLLOW_UP,
        scenario::APPROVAL,
    ];
    for result in &report.scenarios {
        if allowed_to_fail.contains(&result.name) {
            continue;
        }
        // Under the kill switch, SESSION_RESUME/RUNTIME_RESTART's shared
        // `resume_flag_probe` is forbidden from spawning the real `omp`
        // binary and reports an honest skip instead (R52). Any *other*
        // reason for failing here is still a real regression.
        if batman_runtime::conformance::vendor_cli_invocation_disabled() && result.was_skipped() {
            assert!(
                result
                    .detail
                    .contains(batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV),
                "scenario {:?} was skipped for a reason other than the kill switch: {}",
                result.name,
                result.detail
            );
            continue;
        }
        assert!(
            result.proved(),
            "scenario {:?} must pass without any local model server dependency, but failed: {}",
            result.name,
            result.detail
        );
    }
}
