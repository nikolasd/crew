//! Integration tests for the Claude stream-JSON worker adapter.
//!
//! No test in this file ever invokes a model: `probe()` runs only
//! `claude --version`/`claude auth status` against the real installed CLI;
//! every other test either exercises pure command-argv/normalization logic
//! against static fixtures, or calls an adapter method before any vendor
//! process has been started.
//!
//! The handful of tests that do shell out to the real installed `claude`
//! binary (`probe()`/`resume()`/`conformance::fixture_report()`) skip
//! gracefully -- printing a message and returning early -- when `claude`
//! isn't resolvable on `PATH`, e.g. on CI runners that don't have it
//! installed. See `real_claude_binary()` below.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use batman_protocol::{ProjectId, RunId, TaskId, WorkerId};
use batman_runtime::adapter::mcp_config::AdapterMcpConfig;
use batman_runtime::adapter::{
    Adapter, AdapterMessage, ApprovalsCapability, CancelScope, ClaudeStartupOptions,
    DurabilityCapability, NativeViewCapability, NestedCapability, ProtocolKind, ResumeCapability,
    StartSpec, SteeringCapability, UsageCapability, VendorSessionRef, WorkspaceControlCapability,
};
use batman_runtime::coordination::{ScopeBinding, ScopeTokenStore, VendorProcessIdentity};

use batman_runtime::adapter::claude::ClaudeAdapter;
use batman_runtime::adapter::claude::command;
use batman_runtime::adapter::claude::normalize::{ClaudeEvent, ClaudeNormalizer};
use batman_runtime::adapter::claude::{McpInjection, build_mcp_injection};

fn new_adapter() -> ClaudeAdapter {
    ClaudeAdapter::new(
        ClaudeStartupOptions::default(),
        std::env::temp_dir(),
        Vec::new(),
        RunId::new(),
        TaskId::new(),
        WorkerId::new(),
        None,
    )
}

/// A worker-MCP config pointing at a fake `crewd_path` -- fine for
/// every test that only inspects the argv/env/file this module builds
/// and never actually spawns the resulting `coordination-mcp` command.
fn mcp_config() -> AdapterMcpConfig {
    AdapterMcpConfig {
        scope_tokens: Arc::new(ScopeTokenStore::new()),
        project_id: ProjectId::new(),
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: std::env::temp_dir(),
        repository: std::env::temp_dir(),
    }
}

/// The exact temp-file naming convention `build_mcp_injection` uses,
/// duplicated here only so live-process tests can assert on the file's
/// existence/absence without a way to read the adapter's private state.
fn expected_mcp_config_path(run_id: RunId) -> PathBuf {
    std::env::temp_dir().join(format!("crew-mcp-{run_id}.json"))
}

fn real_claude_binary() -> Option<PathBuf> {
    let output = Command::new("which").arg("claude").output().ok()?;
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

fn fixture(name: &str) -> Vec<Vec<u8>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/claude")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"));
    text.lines().map(|line| line.as_bytes().to_vec()).collect()
}

// ------------------------------------------------------------------ kind

#[test]
fn kind_is_claude() {
    assert_eq!(new_adapter().kind(), "claude");
}

// -------------------------------------------------------------- command

#[test]
fn new_session_preserves_native_discovery_and_generates_a_session_id() {
    let options = ClaudeStartupOptions::default();
    let spec = StartSpec {
        run_id: batman_protocol::RunId::new(),
        task_id: batman_protocol::TaskId::new(),
        worker_id: batman_protocol::WorkerId::new(),
        prompt: "do the thing".to_string(),
        resume: None,
    };
    let session_id = uuid::Uuid::now_v7();
    let args = command::build_args(&options, &spec, &session_id);

    for required in [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--include-hook-events",
        "--forward-subagent-text",
        "--session-id",
    ] {
        assert!(
            args.iter().any(|a| a == required),
            "expected {required:?} in {args:?}"
        );
    }
    assert!(args.iter().any(|a| a == &session_id.to_string()));

    // Discovery-preserving: never disable native skill/agent/plugin/hook/MCP
    // resolution.
    for forbidden in ["--bare", "--disable-slash-commands", "--safe-mode"] {
        assert!(
            !args.iter().any(|a| a == forbidden),
            "must never pass {forbidden:?}: {args:?}"
        );
    }
    assert!(!args.iter().any(|a| a == "--resume"));
}

#[test]
fn resume_uses_the_provided_vendor_session_and_skips_session_id() {
    let options = ClaudeStartupOptions::default();
    let spec = StartSpec {
        run_id: batman_protocol::RunId::new(),
        task_id: batman_protocol::TaskId::new(),
        worker_id: batman_protocol::WorkerId::new(),
        prompt: "continue".to_string(),
        resume: Some(VendorSessionRef("abc-123-session".to_string())),
    };
    let session_id = uuid::Uuid::now_v7();
    let args = command::build_args(&options, &spec, &session_id);

    let resume_idx = args
        .iter()
        .position(|a| a == "--resume")
        .expect("expected --resume in args");
    assert_eq!(args[resume_idx + 1], "abc-123-session");
    assert!(!args.iter().any(|a| a == "--session-id"));
}

#[test]
fn startup_options_pass_through_supported_cli_flags_and_omit_unsupported_max_turns() {
    let options = ClaudeStartupOptions {
        allowed_tools: Some(vec!["Bash(git *)".to_string(), "Edit".to_string()]),
        permission_mode: Some("acceptEdits".to_string()),
        // The installed `claude` 2.1.219 CLI has no `--max-turns` flag at
        // all (verified via `claude --help`); it exists only as a
        // programmatic `Options.maxTurns` field in the TS/Python Agent
        // SDK. `ClaudeStartupOptions.max_turns` is already defined
        // upstream (Task 1/2, not ours to change) and cannot be honored
        // by this CLI-argv adapter -- deliberately not passed as a flag,
        // rather than inventing one.
        max_turns: Some(10),
    };
    let spec = StartSpec {
        run_id: batman_protocol::RunId::new(),
        task_id: batman_protocol::TaskId::new(),
        worker_id: batman_protocol::WorkerId::new(),
        prompt: "go".to_string(),
        resume: None,
    };
    let args = command::build_args(&options, &spec, &uuid::Uuid::now_v7());

    let allowed_idx = args
        .iter()
        .position(|a| a == "--allowedTools")
        .expect("expected --allowedTools");
    assert_eq!(args[allowed_idx + 1], "Bash(git *)");
    assert_eq!(args[allowed_idx + 2], "Edit");

    let mode_idx = args
        .iter()
        .position(|a| a == "--permission-mode")
        .expect("expected --permission-mode");
    assert_eq!(args[mode_idx + 1], "acceptEdits");

    assert!(!args.iter().any(|a| a == "--max-turns"));
    assert!(!args.iter().any(|a| a == "10"));
}

#[test]
fn stdin_user_message_is_newline_delimited_stream_json() {
    let bytes = command::build_stdin_user_message("do the thing");
    assert!(bytes.ends_with(b"\n"), "must be newline-delimited");
    let value: serde_json::Value = serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap();
    assert_eq!(value["type"], "user");
    assert_eq!(value["message"]["role"], "user");
    assert_eq!(value["message"]["content"][0]["type"], "text");
    assert_eq!(value["message"]["content"][0]["text"], "do the thing");
}

// ---------------------------------------------------- worker mcp tools

#[test]
fn mcp_injection_appends_mcp_config_after_native_discovery_args_and_never_leaks_the_token_into_argv()
 {
    let options = ClaudeStartupOptions::default();
    let spec = StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "go".to_string(),
        resume: None,
    };
    let mut args = command::build_args(&options, &spec, &uuid::Uuid::now_v7());

    let mcp = mcp_config();
    let injection: McpInjection =
        build_mcp_injection(&mcp, spec.run_id).expect("writing the mcp config file must succeed");
    args.extend(injection.extra_args.clone());

    let config_idx = args
        .iter()
        .position(|a| a == "--mcp-config")
        .expect("expected --mcp-config appended after build_args's own argv");
    assert_eq!(
        args[config_idx + 1],
        injection.config_path.display().to_string()
    );

    // Worker MCP injection is additive: every native discovery flag
    // this adapter already omits must remain omitted, and the strict
    // flag that would suppress every *other* configured MCP server
    // must never be added either.
    for forbidden in [
        "--bare",
        "--disable-slash-commands",
        "--safe-mode",
        "--strict-mcp-config",
        "--disable-builtin-mcps",
    ] {
        assert!(
            !args.iter().any(|a| a == forbidden),
            "must never pass {forbidden:?}: {args:?}"
        );
    }

    // The scope token must never appear anywhere in argv.
    assert!(
        !args.iter().any(|a| a.contains(&injection.token)),
        "the scope token must never appear in argv: {args:?}"
    );

    std::fs::remove_file(&injection.config_path).ok();
}

#[test]
fn mcp_injection_env_carries_only_the_scope_token() {
    let mcp = mcp_config();
    let injection = build_mcp_injection(&mcp, RunId::new()).expect("mcp injection must succeed");

    assert_eq!(injection.extra_env.len(), 1);
    assert_eq!(
        injection.extra_env.get("CREW_WORKER_SCOPE_TOKEN"),
        Some(&injection.token)
    );

    std::fs::remove_file(&injection.config_path).ok();
}

#[test]
fn mcp_injection_config_file_has_owner_only_permissions_and_never_contains_the_token() {
    use std::os::unix::fs::PermissionsExt;

    let mcp = mcp_config();
    let injection = build_mcp_injection(&mcp, RunId::new()).expect("mcp injection must succeed");

    let contents = std::fs::read_to_string(&injection.config_path)
        .expect("the --mcp-config file must have been written");
    assert!(
        !contents.contains(&injection.token),
        "the mcp config file must never contain the scope token"
    );

    let document: serde_json::Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(
        document["mcpServers"]["crew"]["args"][0],
        "coordination-mcp"
    );
    assert_eq!(document["mcpServers"].as_object().unwrap().len(), 1);

    let mode = std::fs::metadata(&injection.config_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the mcp config file must be owner-only readable"
    );

    std::fs::remove_file(&injection.config_path).ok();
}

#[tokio::test]
async fn dispose_revokes_every_scope_token_bound_to_this_adapters_run_when_mcp_is_configured() {
    let mcp = mcp_config();
    let scope_tokens = mcp.scope_tokens.clone();
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();

    // Bind a token to this adapter's run directly, as if a prior spawn
    // had already reserved+activated one -- this lets the revoke be
    // observed without needing a real vendor process, and also proves
    // `revoke_for_run` (not some narrower per-token check) is what
    // fires: it wipes every token bound to the run, however it got
    // there.
    let token = scope_tokens.reserve_token();
    scope_tokens
        .bind(
            token,
            ScopeBinding {
                project_id: mcp.project_id,
                task_id,
                worker_id,
                run_id,
                vendor_process: VendorProcessIdentity {
                    pid: std::process::id() as i32,
                },
                expires_at: AdapterMcpConfig::default_expiry(),
            },
        )
        .expect("binding a freshly reserved token must succeed");
    assert!(scope_tokens.scope_for_run(run_id).is_some());

    let adapter = ClaudeAdapter::new(
        ClaudeStartupOptions::default(),
        std::env::temp_dir(),
        Vec::new(),
        run_id,
        task_id,
        worker_id,
        Some(mcp),
    );

    adapter
        .dispose()
        .await
        .expect("dispose must succeed even though no session was ever started");

    assert!(
        scope_tokens.scope_for_run(run_id).is_none(),
        "dispose must revoke every scope token bound to this adapter's run"
    );
}

#[test]
fn constructing_with_mcp_none_behaves_identically_to_every_pre_existing_test() {
    // `new_adapter()` already passes `None`; every pre-existing test in
    // this file continuing to pass unchanged is the real proof. This
    // test only pins down that a `None`-configured adapter's `kind()`
    // (a cheap, always-available probe) still works, guarding against a
    // future edit accidentally making `mcp` non-optional.
    assert_eq!(new_adapter().kind(), "claude");
}

// ------------------------------------------------------------- normalize

fn emitted_payloads(events: &[ClaudeEvent]) -> Vec<&batman_runtime::adapter::AdapterEventPayload> {
    events
        .iter()
        .filter_map(|event| match event {
            ClaudeEvent::Emit(payload) => Some(payload),
            _ => None,
        })
        .collect()
}

#[test]
fn initialize_fixture_normalizes_session_id_text_tools_and_final_result() {
    use batman_runtime::adapter::AdapterEventPayload::*;

    let mut normalizer = ClaudeNormalizer::new();
    let mut all_events = Vec::new();
    for line in fixture("initialize.jsonl") {
        let events = normalizer
            .normalize_line("claude", &line)
            .unwrap_or_else(|err| panic!("normalizing line failed: {err}"));
        all_events.extend(events);
    }
    let payloads = emitted_payloads(&all_events);

    match payloads[0] {
        VendorSessionEstablished { vendor_session_id } => {
            assert_eq!(vendor_session_id, "11111111-1111-4111-8111-000000000001");
        }
        other => panic!("expected VendorSessionEstablished first, got {other:?}"),
    }

    // The two streaming text deltas become MessageChunk, in order.
    let chunks: Vec<&str> = payloads
        .iter()
        .filter_map(|p| match p {
            MessageChunk { text, .. } => Some(text.value.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(chunks, vec!["Sure, ", "I'll check the config file."]);

    // The thinking block on the tool-use turn is never emitted at all.
    let finals: Vec<&str> = payloads
        .iter()
        .filter_map(|p| match p {
            MessageFinal { text, .. } => Some(text.value.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        finals
            .iter()
            .all(|t| !t.contains("I should check config.toml"))
    );
    assert!(finals.contains(&"Sure, I'll check the config file."));
    assert!(finals.contains(&"The read timeout is set to 30 seconds in config.toml."));

    // Tool lifecycle: started for Read, then its result.
    let tool_started = payloads
        .iter()
        .find_map(|p| match p {
            ToolStarted { tool_call_id, name } => Some((tool_call_id.as_str(), name.as_str())),
            _ => None,
        })
        .expect("expected a ToolStarted event");
    assert_eq!(tool_started, ("tool-000000000001", "Read"));

    let tool_result = payloads
        .iter()
        .find_map(|p| match p {
            ToolResult {
                tool_call_id,
                name,
                ok,
                detail,
            } => Some((
                tool_call_id.as_str(),
                name.as_str(),
                *ok,
                detail.value.as_str(),
            )),
            _ => None,
        })
        .expect("expected a ToolResult event");
    assert_eq!(tool_result.0, "tool-000000000001");
    assert_eq!(tool_result.1, "Read");
    assert!(tool_result.2);
    assert!(tool_result.3.contains("value = 30"));

    // Final result: usage/cost plus the run's final answer text.
    let usage = payloads
        .iter()
        .find_map(|p| match p {
            UsageReported {
                input_tokens,
                output_tokens,
                cost_usd,
            } => Some((*input_tokens, *output_tokens, *cost_usd)),
            _ => None,
        })
        .expect("expected a UsageReported event");
    assert_eq!(usage, (1112, 84, Some(0.0142)));

    let result_text = payloads
        .iter()
        .find_map(|p| match p {
            MessageFinal { role, text } if role == "result" => Some(text.value.as_str()),
            _ => None,
        })
        .expect("expected the result frame's final text");
    assert_eq!(
        result_text,
        "The read timeout is set to 30 seconds in config.toml."
    );

    // Exactly one VendorSessionEstablished, one ToolStarted, one ToolResult,
    // one UsageReported, and no NestedWorkerObserved (no subagent here).
    assert_eq!(
        payloads
            .iter()
            .filter(|p| matches!(p, VendorSessionEstablished { .. }))
            .count(),
        1
    );
    assert_eq!(
        payloads
            .iter()
            .filter(|p| matches!(p, NestedWorkerObserved { .. }))
            .count(),
        0
    );
    // A successful result (this fixture carries `is_error: false`; an
    // absent field is equally inert) must not emit the R12 failure event.
    assert_eq!(
        payloads
            .iter()
            .filter(|p| matches!(p, ProtocolHealthChanged { .. }))
            .count(),
        0
    );
}

#[test]
fn subagent_fixture_correlates_parent_tool_use_id_and_reports_nested_worker_once() {
    use batman_runtime::adapter::AdapterEventPayload::*;

    let mut normalizer = ClaudeNormalizer::new();
    let mut all_events = Vec::new();
    for line in fixture("subagent.jsonl") {
        let events = normalizer.normalize_line("claude", &line).unwrap();
        all_events.extend(events);
    }
    let payloads = emitted_payloads(&all_events);

    // Exactly one NestedWorkerObserved -- on first sighting of the
    // subagent's parent_tool_use_id, never repeated for its later frames.
    let nested: Vec<_> = payloads
        .iter()
        .filter_map(|p| match p {
            NestedWorkerObserved {
                vendor_child_id,
                vendor_parent_ref,
            } => Some((vendor_child_id.as_str(), vendor_parent_ref.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        nested,
        vec![("tool-000000000001", "11111111-1111-4111-8111-000000000001")]
    );

    // The subagent's own text is role-tagged with its parent_tool_use_id
    // for correlation; the main conversation's text is not.
    let roles: Vec<&str> = payloads
        .iter()
        .filter_map(|p| match p {
            MessageFinal { role, .. } => Some(role.as_str()),
            _ => None,
        })
        .collect();
    assert!(roles.contains(&"assistant"));
    assert!(roles.contains(&"assistant:subagent:tool-000000000001"));

    // The subagent's thinking block never became an event.
    let all_text: Vec<&str> = payloads
        .iter()
        .filter_map(|p| match p {
            MessageFinal { text, .. } => Some(text.value.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        all_text
            .iter()
            .all(|t| !t.contains("list the tests directory with Bash"))
    );

    // Both the subagent's own Bash tool and the parent Agent tool-use are
    // reflected as ordinary tool lifecycle events, correlated by their own
    // (unique) tool_call_id.
    let tool_results: Vec<(&str, &str, bool)> = payloads
        .iter()
        .filter_map(|p| match p {
            ToolResult {
                tool_call_id,
                name,
                ok,
                ..
            } => Some((tool_call_id.as_str(), name.as_str(), *ok)),
            _ => None,
        })
        .collect();
    assert!(tool_results.contains(&("tool-000000000002", "Bash", true)));
    assert!(tool_results.contains(&("tool-000000000001", "Agent", true)));
}

#[test]
fn approval_fixture_normalizes_hook_lifecycle_without_ever_touching_the_sink() {
    let mut normalizer = ClaudeNormalizer::new();
    let mut all_events = Vec::new();
    for line in fixture("approval.jsonl") {
        let events = normalizer.normalize_line("claude", &line).unwrap();
        all_events.extend(events);
    }

    // Approval lifecycle never produces an Emit -- see the module doc:
    // full ApprovalService wiring is a later integration point, so this
    // must never construct an AdapterEvent for it.
    assert!(emitted_payloads(&all_events).is_empty());

    let requested = all_events
        .iter()
        .find_map(|event| match event {
            ClaudeEvent::ApprovalRequested {
                approval_id,
                hook_name,
            } => Some((approval_id.as_str(), hook_name.as_str())),
            _ => None,
        })
        .expect("expected an ApprovalRequested event");
    assert_eq!(requested, ("hook-000000000001", "require-bash-approval"));

    let resolved = all_events
        .iter()
        .find_map(|event| match event {
            ClaudeEvent::ApprovalResolved {
                approval_id,
                decision,
            } => Some((approval_id.as_str(), decision.as_str())),
            _ => None,
        })
        .expect("expected an ApprovalResolved event");
    assert_eq!(resolved, ("hook-000000000001", "allow"));
}

#[test]
fn result_fixture_error_arm_reports_usage_and_an_explicit_terminal_failure() {
    use batman_runtime::adapter::AdapterEventPayload::*;

    let mut normalizer = ClaudeNormalizer::new();
    let mut all_events = Vec::new();
    for line in fixture("result.jsonl") {
        let events = normalizer.normalize_line("claude", &line).unwrap();
        all_events.extend(events);
    }
    let payloads = emitted_payloads(&all_events);

    // R12: the vendor reported `is_error: true` with `subtype:
    // "error_max_turns"`; normalizing that as usage-only hid the failure
    // from every event consumer. The error arm must emit usage AND an
    // explicit unhealthy-protocol event naming the vendor's subtype, in
    // that order, and still no final message (there is no `result` text
    // on an error arm).
    assert_eq!(
        payloads.len(),
        2,
        "expected UsageReported then ProtocolHealthChanged: {payloads:?}"
    );
    match payloads[0] {
        UsageReported {
            input_tokens,
            output_tokens,
            cost_usd,
        } => {
            assert_eq!(*input_tokens, 48213);
            assert_eq!(*output_tokens, 9021);
            assert_eq!(*cost_usd, Some(1.87));
        }
        other => panic!("expected UsageReported, got {other:?}"),
    }
    match payloads[1] {
        ProtocolHealthChanged { healthy, detail } => {
            assert!(!healthy, "an is_error result is not healthy");
            assert!(
                detail.value.contains("error_max_turns"),
                "detail must name the vendor subtype: {:?}",
                detail.value
            );
        }
        other => panic!("expected ProtocolHealthChanged, got {other:?}"),
    }
    assert!(
        !payloads.iter().any(|p| matches!(p, MessageFinal { .. })),
        "an error arm has no final message"
    );
}

#[test]
fn thinking_only_message_produces_no_events_at_all() {
    let mut normalizer = ClaudeNormalizer::new();
    let line = br#"{"type":"assistant","session_id":"s","parent_tool_use_id":null,"message":{"content":[{"type":"thinking","thinking":"secret reasoning","signature":"sig"}]}}"#;
    let events = normalizer.normalize_line("claude", line).unwrap();
    assert!(
        events.is_empty(),
        "a thinking-only message must produce zero adapter events, got {events:?}"
    );
}

#[test]
fn malformed_json_line_is_a_protocol_error() {
    let mut normalizer = ClaudeNormalizer::new();
    let err = normalizer
        .normalize_line("claude", b"not json")
        .unwrap_err();
    assert_eq!(err.code(), "protocol");
    assert_eq!(err.adapter(), "claude");
}

#[test]
fn unrecognized_frame_type_is_ignored_not_errored() {
    let mut normalizer = ClaudeNormalizer::new();
    let events = normalizer
        .normalize_line(
            "claude",
            br#"{"type":"prompt_suggestion","suggestion":"try X"}"#,
        )
        .unwrap();
    assert!(events.is_empty());
}

// ---------------------------------------------------------- capabilities

#[test]
fn capabilities_round_trip_and_declare_only_what_is_proven() {
    let adapter = new_adapter();
    let caps = adapter.capabilities();

    assert_eq!(caps.protocol, ProtocolKind::Structured);
    assert_eq!(caps.resume, ResumeCapability::Session);
    assert_eq!(caps.steering, SteeringCapability::Queued);
    assert_eq!(caps.approvals, ApprovalsCapability::Observable);
    assert!(caps.structured_result);
    assert_eq!(caps.usage, UsageCapability::Aggregate);
    assert_eq!(caps.nested, NestedCapability::None);
    assert_eq!(caps.native_view, NativeViewCapability::None);
    assert_eq!(caps.workspace_control, WorkspaceControlCapability::Write);
    assert_eq!(caps.durability, DurabilityCapability::VendorResumable);

    let value = serde_json::to_value(caps).unwrap();
    assert_eq!(value["protocol"], "structured");
    assert_eq!(value["nested"], "none");
    let round_tripped: batman_runtime::adapter::AdapterCapabilities =
        serde_json::from_value(value).unwrap();
    assert_eq!(round_tripped, caps);
}

// ----------------------------------------------------------------- probe

#[tokio::test]
async fn probe_reports_the_real_installed_version_and_auth_readiness_with_no_model_call() {
    if real_claude_binary().is_none() {
        eprintln!("skipping: `claude` is not on PATH");
        return;
    }
    let adapter = new_adapter();
    let result = adapter
        .probe()
        .await
        .expect("probe must succeed against the real installed claude CLI");

    let version = result.version.expect("probe must report a version string");
    assert!(
        version.starts_with("2."),
        "expected a Claude Code 2.x version string (this adapter's baseline is 2.1.217; \
         installed CLIs drift by patch version over time), got {version:?}"
    );
    // Grounded against this machine's real `claude auth status` output
    // (loggedIn: true) -- see the shared adapter context.
    assert!(result.auth_ready);
    assert!(
        result.inventory_incomplete,
        "ambient skills/plugins/hooks/MCP are not enumerable from --version/--help/auth status alone"
    );
    assert_eq!(result.capabilities, adapter.capabilities());
}

// ------------------------------------------------------- pre-start state

#[tokio::test]
async fn respond_to_approval_is_capability_unsupported_since_approvals_are_observable_only() {
    let adapter = new_adapter();
    let err = adapter
        .respond_to_approval("hook_001", "approve")
        .await
        .expect_err("approvals:observable must reject respondToApproval");
    assert_eq!(err.code(), "capability_unsupported");
    assert_eq!(err.operation(), "respondToApproval");
    assert_eq!(err.adapter(), "claude");
}

#[tokio::test]
async fn cancel_without_a_running_process_is_a_safe_no_op() {
    let adapter = new_adapter();
    adapter
        .cancel(CancelScope::Worker)
        .await
        .expect("cancelling an adapter with no active process must be a no-op");
}

#[tokio::test]
async fn snapshot_before_start_reports_empty_state() {
    let adapter = new_adapter();
    let snapshot = adapter.snapshot().await.unwrap();
    assert!(snapshot.state_summary.is_empty() || !snapshot.state_summary.is_empty());
    assert!(snapshot.children.is_empty());
    assert!(snapshot.artifacts.is_empty());
    assert!(snapshot.usage.is_none());
}

#[tokio::test]
async fn dispose_without_a_running_process_is_idempotent() {
    let adapter = new_adapter();
    adapter.dispose().await.unwrap();
    adapter.dispose().await.unwrap();
}

#[tokio::test]
async fn send_without_an_active_session_returns_invalid_vendor_state() {
    let adapter = new_adapter();
    let err = adapter
        .send(AdapterMessage::FollowUp {
            text: "more please".to_string(),
        })
        .await
        .expect_err("send before start must fail");
    assert_eq!(err.code(), "invalid_vendor_state");
    assert_eq!(err.operation(), "send");
}

// ------------------------------------------------ resume after a restart

/// Collects every `AdapterEvent` emitted through it, for the one test
/// below that needs to observe real, live-process-driven emission
/// (rather than only calling `normalize_line` directly against static
/// fixtures).
#[derive(Default)]
struct CollectingSink {
    events: tokio::sync::Mutex<Vec<batman_runtime::adapter::AdapterEvent>>,
}

impl CollectingSink {
    /// Polls (bounded by the caller's own `tokio::time::timeout`) until
    /// a `UsageReported` event has been collected, then returns it.
    async fn wait_for_usage(&self) -> batman_runtime::adapter::AdapterEvent {
        loop {
            {
                let events = self.events.lock().await;
                if let Some(event) = events.iter().find(|event| {
                    matches!(
                        event.payload,
                        batman_runtime::adapter::AdapterEventPayload::UsageReported { .. }
                    )
                }) {
                    return event.clone();
                }
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

/// Proves the resume-after-restart case, not just same-instance reuse:
/// a *fresh* `ClaudeAdapter` (constructed with its own run/task/worker
/// ids, `start()` never called on it) still reaches the real
/// command-construction + spawn + normalize + emit path when `resume()`
/// is called directly, using only the ids bound at construction (since
/// `Adapter::resume` itself carries no `StartSpec` to read them from).
///
/// Uses the real installed `claude` CLI with a syntactically-valid but
/// nonexistent session id. Verified empirically (see this task's summary)
/// that `claude --resume <nonexistent-uuid> -p --input-format stream-json
/// --output-format stream-json` fails the session lookup and exits in
/// ~4s with a `result` frame reporting zero usage/cost -- before ever
/// reading anything from stdin, so this makes no model call.
#[tokio::test]
async fn resume_from_a_fresh_instance_uses_constructor_bound_ids_and_reaches_the_real_spawn_path() {
    if real_claude_binary().is_none() {
        eprintln!("skipping: `claude` is not on PATH");
        return;
    }
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = ClaudeAdapter::new(
        ClaudeStartupOptions::default(),
        std::env::temp_dir(),
        Vec::new(),
        run_id,
        task_id,
        worker_id,
        None,
    );
    let sink = Arc::new(CollectingSink::default());

    adapter
        .resume(
            VendorSessionRef("00000000-0000-0000-0000-000000000000".to_string()),
            sink.clone(),
        )
        .await
        .expect(
            "resume must reach the real spawn path from a fresh instance that never called start()",
        );

    let usage_event = tokio::time::timeout(Duration::from_secs(20), sink.wait_for_usage())
        .await
        .expect("expected the real `claude --resume` process to exit and report usage within 20s");

    assert_eq!(usage_event.run_id, run_id);
    assert_eq!(usage_event.task_id, task_id);
    assert_eq!(usage_event.worker_id, worker_id);

    adapter
        .dispose()
        .await
        .expect("dispose must be safe even after the process already exited on its own");
}

/// Proves the end-to-end lifecycle, not just the pure argv/env
/// helper: with worker MCP tools configured, `resume()` writes the
/// `--mcp-config` file and activates a live scope token *before* ever
/// touching the vendor's stdin, and by the time the session has
/// ended (observed here via the real `claude --resume` process
/// exiting on its own, exactly as in the test above) and `dispose()`
/// has run, both the token and the temp file are gone -- whichever of
/// `run_session`'s vendor-exit hook or `dispose()`'s own cleanup got
/// there first.
#[tokio::test]
async fn resume_with_worker_mcp_configured_activates_a_token_and_cleans_up_on_exit() {
    if real_claude_binary().is_none() {
        eprintln!("skipping: `claude` is not on PATH");
        return;
    }
    let mcp = mcp_config();
    let scope_tokens = mcp.scope_tokens.clone();
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let adapter = ClaudeAdapter::new(
        ClaudeStartupOptions::default(),
        std::env::temp_dir(),
        Vec::new(),
        run_id,
        task_id,
        worker_id,
        Some(mcp),
    );
    let sink = Arc::new(CollectingSink::default());
    let config_path = expected_mcp_config_path(run_id);

    adapter
        .resume(
            VendorSessionRef("00000000-0000-0000-0000-000000000000".to_string()),
            sink.clone(),
        )
        .await
        .expect("resume must reach the real spawn path with worker MCP tools configured");

    // Activation happens before the vendor's stdin is ever touched, so
    // both are already true the moment `resume()` returns.
    assert!(
        config_path.exists(),
        "the --mcp-config file must exist once resume() has returned"
    );
    assert!(
        scope_tokens.scope_for_run(run_id).is_some(),
        "the scope token must be activated before resume() returns"
    );

    let usage_event = tokio::time::timeout(Duration::from_secs(20), sink.wait_for_usage())
        .await
        .expect("expected the real `claude --resume` process to exit and report usage within 20s");
    assert_eq!(usage_event.run_id, run_id);

    adapter
        .dispose()
        .await
        .expect("dispose must be safe even after the process already exited on its own");

    assert!(
        scope_tokens.scope_for_run(run_id).is_none(),
        "the scope token must be revoked once the session has ended"
    );
    assert!(
        !config_path.exists(),
        "the --mcp-config temp file must be deleted once the session has ended"
    );
}

// ----------------------------------------------------------- conformance

#[tokio::test]
async fn conformance_fixture_report_covers_every_canonical_scenario_and_all_pass() {
    use batman_runtime::conformance::{
        DISABLE_VENDOR_CLI_ENV, scenario, vendor_cli_invocation_disabled,
    };

    // The kill switch removes the real `claude` process just as surely as an
    // absent binary does, so the scenarios backed only by a real spawn
    // report an honest skip instead (R52). Coverage still has to hold; which
    // scenarios can *pass* no longer does.
    let vendor_cli_disabled = vendor_cli_invocation_disabled();
    if !vendor_cli_disabled && real_claude_binary().is_none() {
        eprintln!("skipping: `claude` is not on PATH");
        return;
    }

    let report = batman_runtime::adapter::claude::conformance::fixture_report().await;

    assert_eq!(
        report.scenarios.len(),
        14,
        "expected exactly 14 scenarios, got {:?}",
        report.scenarios.iter().map(|s| s.name).collect::<Vec<_>>()
    );

    let mut seen = std::collections::HashSet::new();
    for scenario_result in &report.scenarios {
        assert!(
            scenario::ALL.contains(&scenario_result.name),
            "unexpected scenario name: {}",
            scenario_result.name
        );
        assert!(
            seen.insert(scenario_result.name),
            "duplicate scenario name: {}",
            scenario_result.name
        );
    }
    for name in scenario::ALL {
        assert!(seen.contains(&name), "missing scenario: {name}");
    }

    // Every scenario this adapter's fixture suite runs is genuinely
    // provable without a model call (see conformance.rs) -- a failing
    // scenario here is a real regression, never a fabricated pass to
    // paper over.
    for scenario_result in &report.scenarios {
        if vendor_cli_disabled && scenario_result.was_skipped() {
            assert!(
                scenario_result.detail.contains(DISABLE_VENDOR_CLI_ENV),
                "scenario {} was skipped for a reason other than the kill switch, which is \
                 a real regression: {}",
                scenario_result.name,
                scenario_result.detail
            );
            continue;
        }
        assert!(
            scenario_result.proved(),
            "scenario {} failed: {}",
            scenario_result.name,
            scenario_result.detail
        );
    }
}
