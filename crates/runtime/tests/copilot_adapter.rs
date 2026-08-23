//! Integration tests for the version-gated Copilot ACP adapter.
//!
//! Every test here is a genuine no-model-call structured-protocol check:
//! pure fixture/negotiation/normalization assertions, or a real
//! `copilot --acp` handshake (`initialize`/`session/list`) that never
//! sends a `session/prompt`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use batman_runtime::ScopeTokenStore;
use batman_runtime::adapter::copilot::client::{
    CopilotAcpClient, CopilotClientEvent, parse_initialize_response,
};
use batman_runtime::adapter::copilot::compatibility::{
    COPILOT_MAX_ACP_PROTOCOL_VERSION, COPILOT_MIN_ACP_PROTOCOL_VERSION,
    copilot_acp_protocol_version_supported, copilot_cli_version_known,
};
use batman_runtime::adapter::copilot::normalize::copilot_normalize_session_update;
use batman_runtime::adapter::copilot::{CopilotAdapter, CopilotSpawnPlan};
use batman_runtime::adapter::mcp_config::{
    AdapterMcpConfig, McpLaunchContext, coordination_mcp_config_document,
};
use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterErrorCode, ApprovalsCapability, DurabilityCapability,
    NativeViewCapability, NestedCapability, ProtocolKind, ResumeCapability, SteeringCapability,
    UsageCapability, WorkspaceControlCapability,
};
use serde_json::Value;
use tokio::time::timeout;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/copilot")
        .join(name)
}

fn load_json_fixture(name: &str) -> Value {
    let path = fixture_path(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("parsing fixture {}: {e}", path.display()))
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

// ------------------------------------------------------- compatibility.rs

#[test]
fn known_cli_version_is_exact_match_against_every_empirically_verified_version() {
    // 1.0.73 was installed when this work started; the CLI's own
    // background auto-updater (`copilot update`) moved it to 1.0.75,
    // 1.0.78, and later 1.0.80. Each was reprobed with a real
    // `initialize` handshake and confirmed to negotiate the same ACP v1
    // shape (see `compatibility.rs`'s module doc, and
    // `real_binary_initialize_and_session_list_never_invoke_a_model`
    // below, which fails if the *installed* version is absent from the
    // table). All four are exact-match known versions; nothing else is.
    assert!(copilot_cli_version_known("1.0.73"));
    assert!(copilot_cli_version_known("1.0.75"));
    assert!(copilot_cli_version_known("1.0.78"));
    assert!(copilot_cli_version_known("1.0.80"));
    // A neighbouring patch release is never assumed compatible, and a
    // prefix is never a match.
    assert!(!copilot_cli_version_known("1.0.74"));
    assert!(!copilot_cli_version_known("1.0.77"));
    assert!(!copilot_cli_version_known("1.0.79"));
    assert!(!copilot_cli_version_known("1.0.81"));
    assert!(!copilot_cli_version_known("1.0.7"));
    assert!(!copilot_cli_version_known(""));
}

#[test]
fn only_acp_protocol_v1_is_supported() {
    assert_eq!(COPILOT_MIN_ACP_PROTOCOL_VERSION, 1);
    assert_eq!(COPILOT_MAX_ACP_PROTOCOL_VERSION, 1);
    assert!(copilot_acp_protocol_version_supported(1));
    assert!(!copilot_acp_protocol_version_supported(0));
    assert!(!copilot_acp_protocol_version_supported(2));
}

#[test]
fn raising_the_max_acp_protocol_version_requires_a_session_update_nested_worker_mapping() {
    // Category C, documented as a permanent wall while
    // `COPILOT_MAX_ACP_PROTOCOL_VERSION == 1`: ACP protocol v1 has no
    // `session/update` variant this adapter can map to a nested-worker
    // observation (see `normalize.rs`'s own module doc). Nothing to
    // guard until a newer protocol version is actually accepted.
    if COPILOT_MAX_ACP_PROTOCOL_VERSION <= 1 {
        return;
    }
    // Once a newer ACP version is accepted, `normalize.rs` must have
    // grown a real branch producing `NestedWorkerObserved` for it --
    // this inspects the actual source text (not just that the constant
    // changed) so raising the version without also adding the mapping
    // fails loudly here, rather than silently reopening the gap
    // `unexpected_child_observation_scenario` currently reports
    // honestly as a genuine, permanent limitation.
    let source = include_str!("../src/adapter/copilot/normalize.rs");
    assert!(
        source.contains("NestedWorkerObserved"),
        "COPILOT_MAX_ACP_PROTOCOL_VERSION was raised above 1, but normalize.rs still has no \
         session/update branch producing NestedWorkerObserved -- add the mapping for the new \
         ACP version's subagent-observation variant before raising this constant."
    );
}

// ---------------------------------------------------- initialize negotiation

#[test]
fn real_1_0_73_fixture_negotiates_protocol_v1_and_v1_field_names() {
    let response = load_json_fixture("initialize-v1.json");
    let result = response
        .get("result")
        .expect("fixture is a JSON-RPC response with a result");
    let negotiated =
        parse_initialize_response(result).expect("a v1-shaped initialize response parses");

    assert_eq!(negotiated.protocol_version, 1);
    assert_eq!(negotiated.agent_version.as_deref(), Some("1.0.73"));
    // v1 field names read directly off the real observed response:
    // `agentCapabilities.loadSession`, `.mcpCapabilities.{http,sse}`,
    // `.promptCapabilities.{image,embeddedContext}`,
    // `.sessionCapabilities.list` -- never v2 names (`tools`, etc.).
    assert!(negotiated.load_session);
    assert!(negotiated.session_list);
    assert!(negotiated.mcp_http);
    assert!(negotiated.mcp_sse);
    assert!(negotiated.image);
    assert!(negotiated.embedded_context);
}

#[test]
fn an_unsupported_negotiated_protocol_version_is_refused_as_incompatible() {
    let mut response = load_json_fixture("initialize-v1.json");
    response["result"]["protocolVersion"] = Value::from(2);
    let result = response.get("result").unwrap();

    let error =
        parse_initialize_response(result).expect_err("an unsupported protocol version must fail");
    assert_eq!(error.error_code(), AdapterErrorCode::IncompatibleVersion);
}

#[test]
fn a_missing_agent_version_is_unknown_not_implicitly_verified() {
    // R57: `ensure_client`'s doc comment claims the version check is
    // unconditional, but its old guard (`if let Some(version) = ... &&
    // !known`) let a vendor response omitting `agentInfo.version` proceed
    // against a completely unverified CLI. The shared decision function
    // both `ensure_client` and `probe()` now consult must treat a missing
    // version exactly like an unknown one.
    use batman_runtime::adapter::copilot::compatibility::copilot_negotiated_version_verified;

    let mut response = load_json_fixture("initialize-v1.json");
    response["result"]["agentInfo"]
        .as_object_mut()
        .expect("fixture has agentInfo")
        .remove("version");
    let negotiated = parse_initialize_response(response.get("result").unwrap())
        .expect("a response without agentInfo.version still parses");
    assert_eq!(negotiated.agent_version, None);

    assert!(!copilot_negotiated_version_verified(
        negotiated.agent_version.as_deref()
    ));
    assert!(copilot_negotiated_version_verified(Some("1.0.73")));
    assert!(!copilot_negotiated_version_verified(Some("0.0.1")));
}

#[tokio::test]
async fn ensure_client_refuses_end_to_end_when_the_agent_omits_its_version() {
    // R57 review W-1: the predicate test above proves the decision function;
    // this drives `ensure_client` itself (via `resume`) against a fake ACP
    // agent whose initialize response omits `agentInfo.version`, so
    // reverting the wiring in `ensure_client` fails here even if the
    // predicate stays correct. No vendor CLI, no model call.
    use std::os::unix::fs::PermissionsExt;

    use batman_runtime::adapter::{AdapterEvent, AdapterEventSink, AdapterFuture};

    struct NullSink;
    impl AdapterEventSink for NullSink {
        fn emit(&self, _event: AdapterEvent) -> AdapterFuture<'_, u64> {
            Box::pin(async { Ok(0) })
        }
    }

    // Build the fake agent's initialize response from the real fixture,
    // with `agentInfo.version` stripped.
    let mut fixture = load_json_fixture("initialize-v1.json");
    fixture["result"]["agentInfo"]
        .as_object_mut()
        .expect("fixture has agentInfo")
        .remove("version");
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": fixture["result"],
    });
    let response_line = serde_json::to_string(&response).unwrap();

    // A fake ACP agent: reads the initialize request, answers it with the
    // version-less response, then waits to be shut down.
    let dir = tempfile::Builder::new()
        .prefix("bat-cop-fake-")
        .tempdir_in("/tmp")
        .unwrap();
    let program = dir.path().join("fake-copilot");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\ntrap 'exit 0' INT TERM\nread -r req\ncat <<'ACPEOF'\n{response_line}\nACPEOF\nwhile :; do sleep 1; done\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

    let adapter = CopilotAdapter::new(
        program,
        std::env::temp_dir(),
        batman_runtime::adapter::CopilotStartupOptions::default(),
        Vec::new(),
        batman_protocol::RunId::new(),
        batman_protocol::TaskId::new(),
        batman_protocol::WorkerId::new(),
        None,
    );

    let error = timeout(
        // Generous: the refusal path terminates the fake agent via the
        // supervisor's SIGINT -> SIGTERM escalation, whose per-step
        // deadlines dominate this test's wall time.
        Duration::from_secs(30),
        adapter.resume(
            batman_runtime::adapter::VendorSessionRef("session-1".to_string()),
            std::sync::Arc::new(NullSink),
        ),
    )
    .await
    .expect("resume did not hang")
    .expect_err("a version-less agent must be refused before any session call");
    assert_eq!(error.error_code(), AdapterErrorCode::IncompatibleVersion);
    assert!(
        error.detail().contains("agentInfo.version"),
        "the refusal must name the missing-version case: {}",
        error.detail()
    );
}

#[test]
fn a_response_missing_protocol_version_is_a_protocol_error() {
    let error = parse_initialize_response(&serde_json::json!({}))
        .expect_err("a response missing protocolVersion must fail");
    assert_eq!(error.error_code(), AdapterErrorCode::Protocol);
}

// --------------------------------------------------------------- no TCP ever

#[test]
fn copilot_acp_client_source_never_constructs_a_port_argument() {
    // Structural proof, not just a runtime debug_assert: the only place
    // the literal `--port` appears anywhere in client.rs is the defensive
    // guard that *refuses* it (`debug_assert!` comparing argv tokens
    // against it) -- whitelisted verbatim below and stripped before the
    // scan, so any other occurrence (a real construction site) fails
    // this test.
    let source = include_str!("../src/adapter/copilot/client.rs");
    let code_only: String = source
        .lines()
        .map(str::trim_start)
        .filter(|line| !line.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let guard_start = code_only
        .find("debug_assert!(")
        .expect("client.rs must contain the --port debug_assert guard");
    let guard_end = code_only[guard_start..]
        .find(");")
        .map(|offset| guard_start + offset + 2)
        .expect("the debug_assert guard must be terminated");
    let guard_block = &code_only[guard_start..guard_end];
    assert!(
        guard_block.contains("--port"),
        "the debug_assert block right after `--acp` argv construction must be the --port guard, found: {guard_block}"
    );
    let sanitized = format!("{}{}", &code_only[..guard_start], &code_only[guard_end..]);
    assert!(
        !sanitized.contains("--port"),
        "client.rs must never construct a --port argv token outside the whitelisted debug_assert guard"
    );
}

#[tokio::test]
async fn real_binary_port_zero_opens_no_tcp_listener() {
    let Some(copilot) = real_copilot_binary() else {
        eprintln!("skipping: `copilot` is not on PATH");
        return;
    };
    // Deliberately probes the REAL binary's own `--port` flag directly
    // (bypassing `CopilotAcpClient`, which never builds this flag) to
    // prove the installed 1.0.73 CLI itself opens no TCP listener for
    // it -- the empirical fact `client.rs`'s module doc relies on.
    let mut child = tokio::process::Command::new(&copilot)
        .args(["--acp", "--port", "0"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning the real copilot binary");
    let pid = child
        .id()
        .expect("pid observable for a freshly spawned child");

    tokio::time::sleep(Duration::from_millis(750)).await;

    let lsof = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-i", "TCP", "-sTCP:LISTEN"])
        .output();
    let _ = child.kill().await;
    let _ = child.wait().await;

    match lsof {
        Ok(output) => {
            let listing = String::from_utf8_lossy(&output.stdout);
            assert!(
                listing.trim().is_empty(),
                "the real copilot --acp --port 0 binary must open no TCP listener, found: {listing}"
            );
        }
        Err(e) => eprintln!("skipping listener assertion: lsof unavailable ({e})"),
    }
}

#[tokio::test]
async fn a_supervised_process_exit_is_reported_with_its_real_status() {
    let client = CopilotAcpClient::spawn_with_raw_args(
        Path::new("/bin/sh"),
        Path::new("."),
        vec!["-c".to_string(), "exit 3".to_string()],
        HashMap::new(),
    )
    .await
    .expect("spawning /bin/sh -c 'exit 3'");

    let event = timeout(Duration::from_secs(5), client.next_event())
        .await
        .expect("the process exit event arrives promptly")
        .expect("the reader task is still alive");

    match event {
        CopilotClientEvent::ProcessExited { exit_code, signal } => {
            assert_eq!(exit_code, Some(3));
            assert_eq!(signal, None);
        }
        other => panic!("expected ProcessExited, got {other:?}"),
    }
}

// -------------------------------------------------- normalize.rs (fixtures)

#[test]
fn session_updates_fixture_normalizes_every_variant_correctly() {
    let updates: Vec<Value> = load_jsonl_fixture("session-updates.jsonl")
        .into_iter()
        .map(|frame| frame["params"]["update"].clone())
        .collect();
    assert_eq!(updates.len(), 10);

    // 1. user_message_chunk -> visible MessageChunk{role: "user"}.
    let payloads = copilot_normalize_session_update(&updates[0]);
    assert_eq!(payloads.len(), 1);
    match &payloads[0] {
        batman_runtime::adapter::AdapterEventPayload::MessageChunk { role, text } => {
            assert_eq!(role, "user");
            assert_eq!(text.class, batman_protocol::ContentClass::Visible);
            assert_eq!(text.value, "Fix the failing assertion in adapter.rs");
        }
        other => panic!("expected MessageChunk, got {other:?}"),
    }

    // 2. agent_thought_chunk -> dropped before it ever becomes an event.
    assert!(copilot_normalize_session_update(&updates[1]).is_empty());

    // 3. agent_message_chunk -> visible MessageChunk{role: "assistant"}.
    let payloads = copilot_normalize_session_update(&updates[2]);
    match &payloads[0] {
        batman_runtime::adapter::AdapterEventPayload::MessageChunk { role, .. } => {
            assert_eq!(role, "assistant");
        }
        other => panic!("expected MessageChunk, got {other:?}"),
    }

    // 4. tool_call -> ToolStarted.
    let payloads = copilot_normalize_session_update(&updates[3]);
    match &payloads[0] {
        batman_runtime::adapter::AdapterEventPayload::ToolStarted { tool_call_id, name } => {
            assert_eq!(tool_call_id, "tool-000000000001");
            assert_eq!(name, "Read adapter.rs");
        }
        other => panic!("expected ToolStarted, got {other:?}"),
    }

    // 5. tool_call_update{status: completed} -> ToolResult{ok: true}.
    let payloads = copilot_normalize_session_update(&updates[4]);
    match &payloads[0] {
        batman_runtime::adapter::AdapterEventPayload::ToolResult { ok, detail, .. } => {
            assert!(ok);
            assert_eq!(detail.value, "fn main() {}");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // 7. tool_call_update{status: in_progress} -> ToolProgress.
    let payloads = copilot_normalize_session_update(&updates[6]);
    match &payloads[0] {
        batman_runtime::adapter::AdapterEventPayload::ToolProgress { tool_call_id, .. } => {
            assert_eq!(tool_call_id, "tool-000000000002");
        }
        other => panic!("expected ToolProgress, got {other:?}"),
    }

    // 8. tool_call_update{status: completed, content: [diff]} -> ToolResult
    //    whose detail names only the path, never the old/new file text.
    let payloads = copilot_normalize_session_update(&updates[7]);
    match &payloads[0] {
        batman_runtime::adapter::AdapterEventPayload::ToolResult { ok, detail, .. } => {
            assert!(ok);
            assert_eq!(detail.value, "diff: /workspace/adapter.rs");
            assert!(!detail.value.contains("assert_eq"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // 10. tool_call_update{status: failed} -> ToolResult{ok: false}.
    let payloads = copilot_normalize_session_update(&updates[9]);
    match &payloads[0] {
        batman_runtime::adapter::AdapterEventPayload::ToolResult { ok, detail, .. } => {
            assert!(!ok);
            assert_eq!(detail.value, "permission denied");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn an_unrecognized_session_update_variant_normalizes_to_no_events() {
    let update = serde_json::json!({ "sessionUpdate": "plan", "entries": [] });
    assert!(copilot_normalize_session_update(&update).is_empty());
}

// ------------------------------------------------------------- capabilities

#[test]
fn declared_capabilities_match_exactly_what_this_adapter_tests() {
    let adapter = CopilotAdapter::new(
        PathBuf::from("copilot"),
        std::env::temp_dir(),
        batman_runtime::adapter::CopilotStartupOptions::default(),
        Vec::new(),
        batman_protocol::RunId::new(),
        batman_protocol::TaskId::new(),
        batman_protocol::WorkerId::new(),
        None,
    );
    let capabilities: AdapterCapabilities = adapter.capabilities();
    assert_eq!(capabilities.protocol, ProtocolKind::Structured);
    // Proven by `real_1_0_73_fixture_negotiates_protocol_v1_and_v1_field_names`
    // observing `agentCapabilities.loadSession: true`, and
    // `session_load`/`session/load` being exercised in `Adapter::resume`.
    assert_eq!(capabilities.resume, ResumeCapability::Session);
    // ACP v1 has no mid-turn steering distinct from a follow-up prompt
    // after a turn ends; never tested against a real turn (that would be
    // a model call), so declared `None`, not assumed.
    assert_eq!(capabilities.steering, SteeringCapability::None);
    // Proven by the `permission.jsonl`-driven
    // `respond_permission_answers_a_real_pending_request_over_the_wire`
    // test: this adapter both observes AND resolves a real
    // `session/request_permission` request.
    assert_eq!(capabilities.approvals, ApprovalsCapability::Controllable);
    assert!(!capabilities.structured_result);
    // ACP v1's `PromptResponse` carries only a `stopReason`, never a
    // token/cost usage object -- absent from the protocol, not merely
    // untested.
    assert_eq!(capabilities.usage, UsageCapability::None);
    assert_eq!(capabilities.nested, NestedCapability::None);
    assert_eq!(capabilities.native_view, NativeViewCapability::None);
    // Proven by the `session-updates.jsonl` `edit`-kind tool call
    // (`tool-000000000002`)'s `diff` content normalizing into a `ToolResult`.
    assert_eq!(
        capabilities.workspace_control,
        WorkspaceControlCapability::Write
    );
    // Proven by `agentCapabilities.loadSession: true` plus real
    // historical sessions returned from a live `session/list` probe.
    assert_eq!(
        capabilities.durability,
        DurabilityCapability::VendorResumable
    );
}

// -------------------------------------------------- real installed binary

#[tokio::test]
async fn real_binary_initialize_and_session_list_never_invoke_a_model() {
    let Some(copilot) = real_copilot_binary() else {
        eprintln!("skipping: `copilot` is not on PATH");
        return;
    };

    let client = timeout(
        Duration::from_secs(10),
        CopilotAcpClient::spawn(&copilot, Path::new("."), Vec::new(), HashMap::new()),
    )
    .await
    .expect("spawning the real copilot --acp binary did not hang")
    .expect("spawning the real copilot --acp binary");

    let negotiated = timeout(Duration::from_secs(10), client.initialize())
        .await
        .expect("initialize did not hang")
        .expect("a real handshake with the installed binary succeeds");
    assert_eq!(negotiated.protocol_version, 1);
    // The installed binary can auto-update itself between test runs
    // (observed 1.0.73 -> 1.0.75 mid-development); assert it is a
    // version this adapter has empirically verified rather than pinning
    // one exact string that will go stale.
    let observed_version = negotiated
        .agent_version
        .as_deref()
        .expect("agentInfo.version present");
    assert!(
        copilot_cli_version_known(observed_version),
        "installed copilot CLI {observed_version} is not in COPILOT_KNOWN_CLI_VERSIONS; \
         reprobe and add it after confirming it negotiates the same ACP v1 shape"
    );

    // A real, no-model-call structured probe: `session/list` only reads
    // Copilot's own persisted session metadata, never a prompt/model
    // call.
    let sessions = timeout(Duration::from_secs(10), client.session_list())
        .await
        .expect("session/list did not hang")
        .expect("session/list succeeds against an authenticated installed CLI");
    assert!(
        sessions.get("sessions").and_then(Value::as_array).is_some(),
        "session/list response must carry a `sessions` array, got: {sessions}"
    );

    client.shutdown().await;
}

// ------------------------------------------------------- permission flow

#[tokio::test]
async fn respond_permission_answers_a_real_pending_request_over_the_wire() {
    let fixture = load_jsonl_fixture("permission.jsonl");
    let request_line = serde_json::to_string(&fixture[0]).unwrap();
    let expected_response = fixture[1].clone();

    let output_dir =
        std::env::temp_dir().join(format!("copilot-adapter-test-{}", std::process::id()));
    std::fs::create_dir_all(&output_dir).unwrap();
    let output_path = output_dir.join("response.json");
    let _ = std::fs::remove_file(&output_path);

    // A fake ACP agent: emits the fixture's real `session/request_permission`
    // request immediately, then writes whatever this client answers with
    // to a file this test can inspect.
    let script = format!(
        "cat <<'ACPEOF'\n{request_line}\nACPEOF\nread -r resp\nprintf '%s' \"$resp\" > {}\n",
        output_path.display()
    );

    let client = CopilotAcpClient::spawn_with_raw_args(
        Path::new("/bin/sh"),
        Path::new("."),
        vec!["-c".to_string(), script],
        HashMap::new(),
    )
    .await
    .expect("spawning the fake ACP agent");

    let event = timeout(Duration::from_secs(5), client.next_event())
        .await
        .expect("the fake agent's permission request arrives promptly")
        .expect("the reader task is still alive");

    let (request_id, request) = match event {
        CopilotClientEvent::PermissionRequested {
            request_id,
            request,
        } => (request_id, request),
        other => panic!("expected PermissionRequested, got {other:?}"),
    };
    assert_eq!(request_id, 42);
    assert_eq!(request.session_id, "11111111-1111-4111-8111-000000000001");
    assert_eq!(request.tool_call_id, "tool-000000000001");
    assert_eq!(request.options.len(), 2);
    assert_eq!(client.pending_permission_ids(), vec![42]);

    client
        .respond_permission(request_id, "allow-once")
        .expect("answering a real pending permission request");
    assert!(client.pending_permission_ids().is_empty());

    // Answering an already-answered request is an explicit error, never
    // a silent no-op.
    let error = client
        .respond_permission(request_id, "allow-once")
        .expect_err("responding twice to the same request must fail");
    assert_eq!(error.error_code(), AdapterErrorCode::InvalidVendorState);

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
    .await
    .expect("the fake agent wrote a response before this test gave up");

    client.shutdown().await;

    let actual: Value =
        serde_json::from_str(&written).expect("the response this client sent is valid JSON");
    assert_eq!(actual, expected_response);

    let _ = std::fs::remove_dir_all(&output_dir);
}

// -------------------------------------------- worker MCP config injection

fn mcp_config_for_test() -> AdapterMcpConfig {
    AdapterMcpConfig {
        scope_tokens: std::sync::Arc::new(ScopeTokenStore::new()),
        project_id: batman_protocol::ProjectId::new(),
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: PathBuf::from("/tmp/crew-state"),
        repository: PathBuf::from("/tmp/my-repo"),
    }
}

fn adapter_with_mcp(mcp: Option<AdapterMcpConfig>) -> (CopilotAdapter, batman_protocol::RunId) {
    let run_id = batman_protocol::RunId::new();
    let adapter = CopilotAdapter::new(
        PathBuf::from("copilot"),
        std::env::temp_dir(),
        batman_runtime::adapter::CopilotStartupOptions::default(),
        Vec::new(),
        run_id,
        batman_protocol::TaskId::new(),
        batman_protocol::WorkerId::new(),
        mcp,
    );
    (adapter, run_id)
}

#[test]
fn spawn_plan_injects_additional_mcp_config_matching_the_shared_document_shape() {
    let (adapter, run_id) = adapter_with_mcp(Some(mcp_config_for_test()));
    let plan: CopilotSpawnPlan = adapter.spawn_plan();

    let flag_index = plan
        .args
        .iter()
        .position(|arg| arg == "--additional-mcp-config")
        .expect("--additional-mcp-config must be present when mcp is Some");
    let value = plan
        .args
        .get(flag_index + 1)
        .expect("--additional-mcp-config must carry a value argument");
    let document: Value = serde_json::from_str(value)
        .expect("--additional-mcp-config value must be well-formed JSON");

    let context = McpLaunchContext {
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: PathBuf::from("/tmp/crew-state"),
        repository: PathBuf::from("/tmp/my-repo"),
        run_id,
    };
    assert_eq!(document, coordination_mcp_config_document(&context));
}

#[test]
fn spawn_plan_never_puts_the_scope_token_in_argv_only_in_env() {
    let (adapter, _run_id) = adapter_with_mcp(Some(mcp_config_for_test()));
    let plan = adapter.spawn_plan();

    let token = plan
        .reserved_token
        .clone()
        .expect("a reserved token is produced when mcp is Some");
    assert!(!token.is_empty());
    assert!(
        plan.args.iter().all(|arg| !arg.contains(&token)),
        "the scope token must never appear in argv, got: {:?}",
        plan.args
    );
    assert_eq!(
        plan.env.get("CREW_WORKER_SCOPE_TOKEN"),
        Some(&token),
        "the scope token must be present in env under CREW_WORKER_SCOPE_TOKEN"
    );
}

#[test]
fn disable_builtin_mcps_is_never_added_regardless_of_mcp_config() {
    for mcp in [None, Some(mcp_config_for_test())] {
        let (adapter, _run_id) = adapter_with_mcp(mcp);
        let plan = adapter.spawn_plan();
        assert!(
            !plan.args.iter().any(|arg| arg == "--disable-builtin-mcps"),
            "native MCP discovery must never be disabled, got: {:?}",
            plan.args
        );
    }
}

#[test]
fn spawn_plan_is_unchanged_when_mcp_is_none() {
    let startup_options = batman_runtime::adapter::CopilotStartupOptions {
        allow_tool: Some(vec!["fs_read".to_string()]),
        deny_tool: Some(vec!["fs_write".to_string()]),
        log_level: Some("debug".to_string()),
    };
    let adapter = CopilotAdapter::new(
        PathBuf::from("copilot"),
        std::env::temp_dir(),
        startup_options,
        Vec::new(),
        batman_protocol::RunId::new(),
        batman_protocol::TaskId::new(),
        batman_protocol::WorkerId::new(),
        None,
    );
    let plan = adapter.spawn_plan();

    assert_eq!(
        plan.args,
        vec![
            "--allow-tool=fs_read".to_string(),
            "--deny-tool=fs_write".to_string(),
            "--log-level".to_string(),
            "debug".to_string(),
        ]
    );
    assert!(plan.reserved_token.is_none());
    assert!(!plan.env.contains_key("CREW_WORKER_SCOPE_TOKEN"));
}

// -------------------------------------------------------- conformance.rs

#[tokio::test]
async fn fixture_conformance_report_covers_every_canonical_scenario_and_provable_ones_pass() {
    use batman_runtime::adapter::copilot::conformance::fixture_report;
    use batman_runtime::conformance::scenario::ALL;

    let report = fixture_report().await;
    assert_eq!(
        report.scenarios.len(),
        14,
        "expected exactly 14 scenarios, got: {:?}",
        report.scenarios.iter().map(|s| s.name).collect::<Vec<_>>()
    );
    let mut seen = std::collections::HashSet::new();
    for name in ALL {
        assert!(
            report.scenarios.iter().any(|s| s.name == name),
            "missing canonical scenario {name}"
        );
        assert!(seen.insert(name), "duplicate scenario name: {name}");
    }
    for scenario in &report.scenarios {
        assert!(
            ALL.contains(&scenario.name),
            "reported scenario {} is not a canonical name",
            scenario.name
        );
    }

    // Every scenario genuinely provable without a model call must pass.
    // `UNEXPECTED_CHILD_OBSERVATION` is a real, honestly-reported gap:
    // ACP v1 has no session/update variant this adapter maps to
    // `NestedWorkerObserved`, so it legitimately fails here, exactly
    // like PROBE is allowed to fail against an unreachable local
    // selector. `SESSION_RESUME`/`RUNTIME_RESTART` are likewise honest
    // gaps: the installed copilot CLI does not persist a never-prompted
    // session across a process boundary, and proving full cross-process
    // resume would require an actual turn (a model call), which this
    // suite must never make.
    let honest_gaps = [
        batman_runtime::conformance::scenario::UNEXPECTED_CHILD_OBSERVATION,
        batman_runtime::conformance::scenario::SESSION_RESUME,
        batman_runtime::conformance::scenario::RUNTIME_RESTART,
    ];
    for scenario in &report.scenarios {
        if honest_gaps.contains(&scenario.name) {
            continue;
        }
        // Under the kill switch, every scenario routed through
        // `real_client` is forbidden from spawning `copilot --acp` at all
        // and reports an honest skip instead (R52). Any *other* reason for
        // failing here is still a real regression.
        if batman_runtime::conformance::vendor_cli_invocation_disabled() && scenario.was_skipped() {
            assert!(
                scenario
                    .detail
                    .contains(batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV),
                "scenario {} was skipped for a reason other than the kill switch: {}",
                scenario.name,
                scenario.detail
            );
            continue;
        }
        assert!(
            scenario.proved(),
            "expected scenario {} to pass, detail: {}",
            scenario.name,
            scenario.detail
        );
    }
}
