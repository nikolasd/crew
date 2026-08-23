//! Integration tests for the Codex `app-server` worker adapter: schema
//! compatibility against the real installed `codex-cli` binary, fixture
//! normalization of a realistic thread/turn transcript (text, tool,
//! usage, and artifact events, all correlated to one run, with hidden
//! `reasoning` content dropped before it ever reaches an event), and
//! approval-request normalization from a dedicated fixture.
//!
//! The tests that require the real `codex` binary degrade gracefully --
//! printing a message and returning early -- when `codex` isn't
//! resolvable on `PATH`, e.g. on CI runners that don't have it installed.
//! See `real_codex_binary` below.

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use batman_protocol::{ContentClass, RunId, TaskId, WorkerId};
use batman_runtime::adapter::{
    Adapter, AdapterEvent, AdapterEventPayload, AdapterEventSink, AdapterFuture,
    CodexStartupOptions,
};
use batman_runtime::supervisor::{EnvironmentPolicy, SpawnSpec, Supervisor};
use serde_json::Value;

use batman_runtime::adapter::codex::CodexAdapter;
use batman_runtime::adapter::codex::client::CodexRpcClient;
use batman_runtime::adapter::codex::normalize;
use batman_runtime::adapter::codex::schema::{SchemaManifest, verify_against_installed_binary};
use batman_runtime::adapter::mcp_config::McpLaunchContext;

// ------------------------------------------------------------ real binary

/// The real `codex` CLI, when it's installed and resolvable on `PATH`
/// (e.g. on a developer's machine). `None` on machines without it -- a CI
/// runner never installs the vendor CLI -- where the real-binary tests
/// print a note and return early instead of failing. Same posture as
/// `real_claude_binary` in `claude_adapter.rs` and `real_copilot_binary`
/// in `copilot_adapter.rs`.
fn real_codex_binary() -> Option<PathBuf> {
    let output = Command::new("which").arg("codex").output().ok()?;
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

// --------------------------------------------------------------- fixtures

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/codex")
        .join(name)
}

fn read_jsonl(name: &str) -> Vec<Value> {
    let raw = std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|e| panic!("failed to read fixture {name}: {e}"));
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("bad fixture line {line:?}: {e}"))
        })
        .collect()
}

// ------------------------------------------------------------ recording sink

/// An in-memory [`AdapterEventSink`] that records every emitted event, so
/// fixture tests can assert on correlation and payload shape without a
/// real domain/journal/broadcast stack.
#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<AdapterEvent>>,
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

// --------------------------------------------------------------- Step 1

#[test]
fn schema_manifest_required_surface_is_present_on_the_installed_binary() {
    if real_codex_binary().is_none() {
        eprintln!("skipping: `codex` is not on PATH");
        return;
    }
    let manifest = SchemaManifest::load(&fixture_path("schema-version.json"))
        .expect("committed schema-version.json manifest must parse");
    verify_against_installed_binary(&manifest, "codex")
        .expect("installed codex-cli 0.145.0 app-server schema must still cover this adapter's required surface");
}

/// A vendor-side turn failure must reach the event stream. Captured
/// verbatim from `codex-cli 0.146.0` (an exhausted workspace quota): the
/// CLI emits this notification and then completes the turn with
/// `status: "failed"`, never producing a final assistant message. Dropping
/// it made every such failure look like an unexplained 60s timeout.
#[test]
fn a_vendor_error_notification_normalizes_to_an_unhealthy_protocol_event() {
    let params = serde_json::json!({
        "error": {
            "message": "Your workspace is out of credits. Ask your workspace owner to refill in order to continue.",
            "codexErrorInfo": "usageLimitExceeded",
            "additionalDetails": null
        },
        "willRetry": false,
        "threadId": "019fd1e8-2a4f-7b31-b78b-f7d6863f541c",
        "turnId": "019fd1e8-2bb6-7280-83c6-fc8132cf2d85"
    });

    let payload = normalize::notification_to_event("error", &params)
        .expect("a vendor error must normalize to an event, not be dropped");

    match payload {
        AdapterEventPayload::ProtocolHealthChanged { healthy, detail } => {
            assert!(!healthy, "a vendor error is not a healthy protocol state");
            // Both the machine-readable code and the operator-readable
            // reason survive; either alone would force a guess.
            assert!(
                detail.value.contains("usageLimitExceeded"),
                "the vendor's error code must survive: {:?}",
                detail.value
            );
            assert!(
                detail.value.contains("out of credits"),
                "the vendor's own message must survive: {:?}",
                detail.value
            );
        }
        other => panic!("expected ProtocolHealthChanged, got {other:?}"),
    }

    // A malformed error notification is ignored rather than panicking.
    assert!(
        normalize::notification_to_event("error", &serde_json::json!({ "error": {} })).is_none(),
        "an error notification with no message must be skipped, not fabricated"
    );
}

#[test]
fn fixture_thread_turn_transcript_normalizes_to_correlated_events() {
    let lines = read_jsonl("thread-turn.jsonl");
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();

    let mut payloads = Vec::new();
    for line in &lines {
        let method = line
            .get("method")
            .and_then(Value::as_str)
            .expect("fixture line has method");
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

    // Every event this turn produced correlates to the same run/task/worker.
    assert!(
        !events.is_empty(),
        "the fixture transcript must normalize to at least one event"
    );
    for event in &events {
        assert_eq!(event.run_id, run_id);
        assert_eq!(event.task_id, task_id);
        assert_eq!(event.worker_id, worker_id);
    }

    let has = |pred: fn(&AdapterEventPayload) -> bool| events.iter().any(|e| pred(&e.payload));

    assert!(
        has(|p| matches!(p, AdapterEventPayload::MessageChunk { .. })),
        "expected at least one MessageChunk (item/agentMessage/delta)"
    );
    assert!(
        has(|p| matches!(p, AdapterEventPayload::MessageFinal { role, .. } if role == "assistant")),
        "expected a MessageFinal for the completed agentMessage item"
    );
    assert!(
        has(
            |p| matches!(p, AdapterEventPayload::ToolStarted { name, .. } if name == "commandExecution")
        ),
        "expected a ToolStarted for the commandExecution item"
    );
    assert!(
        has(|p| matches!(p, AdapterEventPayload::ToolResult { ok: true, .. })),
        "expected a successful ToolResult for the completed commandExecution item"
    );
    assert!(
        has(|p| matches!(
            p,
            AdapterEventPayload::UsageReported {
                input_tokens: 1200,
                output_tokens: 180,
                ..
            }
        )),
        "expected UsageReported from thread/tokenUsage/updated matching the fixture's token counts"
    );
    assert!(
        has(
            |p| matches!(p, AdapterEventPayload::ArtifactProduced { artifact_kind, .. } if artifact_kind == "fileChange")
        ),
        "expected ArtifactProduced for the completed fileChange item"
    );

    // The `reasoning` item's hidden chain-of-thought must never surface as
    // a MessageChunk/MessageFinal (or in any other visible text field).
    for event in &events {
        if let AdapterEventPayload::MessageChunk { text, .. }
        | AdapterEventPayload::MessageFinal { text, .. } = &event.payload
        {
            assert_eq!(text.class, ContentClass::Visible);
            assert!(!text.value.contains("chain of thought"));
        }
    }
}

#[test]
fn fixture_approvals_normalize_to_pending_approvals_not_sink_events() {
    let lines = read_jsonl("approval.jsonl");
    assert_eq!(lines.len(), 2);

    let mut kinds = Vec::new();
    for line in &lines {
        let id = line
            .get("id")
            .cloned()
            .expect("approval fixture line has id");
        let method = line
            .get("method")
            .and_then(Value::as_str)
            .expect("fixture line has method");
        let params = line.get("params").cloned().unwrap_or(Value::Null);
        let approval = normalize::server_request_to_pending_approval(&id, method, &params)
            .unwrap_or_else(|| panic!("expected {method} to normalize to a pending approval"));
        assert_eq!(approval.request_id, id);
        assert!(!approval.call_id.is_empty());
        kinds.push(approval.kind);
    }
    assert_eq!(kinds, vec!["execCommand", "applyPatch"]);
}

#[test]
fn decision_mapping_matches_the_verified_review_decision_shape() {
    assert_eq!(
        normalize::decision_to_review_decision("approve").unwrap(),
        Value::String("approved".to_string())
    );
    let denied = normalize::decision_to_review_decision("deny").unwrap();
    assert!(denied.get("denied").is_some());
    assert!(normalize::decision_to_review_decision("nonsense").is_err());
}

// --------------------------------------------------------------- Step 3/4

#[tokio::test]
async fn capabilities_match_the_verified_protocol_surface() {
    let adapter = CodexAdapter::new(
        std::env::temp_dir(),
        CodexStartupOptions::default(),
        Vec::new(),
        None,
    );
    let caps = adapter.capabilities();
    assert_eq!(
        caps.protocol,
        batman_runtime::adapter::ProtocolKind::Structured
    );
    assert_eq!(
        caps.resume,
        batman_runtime::adapter::ResumeCapability::Session
    );
    assert_eq!(
        caps.steering,
        batman_runtime::adapter::SteeringCapability::ActiveTurn
    );
    assert_eq!(
        caps.approvals,
        batman_runtime::adapter::ApprovalsCapability::Controllable
    );
    assert_eq!(
        caps.usage,
        batman_runtime::adapter::UsageCapability::PerTurn
    );
    assert_eq!(caps.nested, batman_runtime::adapter::NestedCapability::None);
    assert_eq!(
        caps.workspace_control,
        batman_runtime::adapter::WorkspaceControlCapability::Write
    );
    assert_eq!(
        caps.durability,
        batman_runtime::adapter::DurabilityCapability::VendorResumable
    );
}

#[tokio::test]
async fn probe_reports_the_installed_codex_version_without_a_model_call() {
    if real_codex_binary().is_none() {
        eprintln!("skipping: `codex` is not on PATH");
        return;
    }
    let adapter = CodexAdapter::with_binary(
        "codex",
        std::env::temp_dir(),
        CodexStartupOptions::default(),
        Vec::new(),
        None,
    );
    let probe = adapter
        .probe()
        .await
        .expect("probe against the installed codex-cli must succeed");
    assert!(
        probe
            .version
            .as_deref()
            .unwrap_or_default()
            .contains("codex-cli")
    );
}
#[tokio::test]
async fn real_transport_completes_initialize_and_thread_start_with_zero_model_calls() {
    // Exercises `CodexRpcClient` directly against a real spawned
    // `codex app-server` process: the `initialize` request, the
    // `initialized` notification, and a bare `thread/start` (session
    // creation only, no `input`/turn). None of these three methods ever
    // reach the model -- Codex only calls out to the model once a turn
    // actually starts with input (`turn/start`), which this test
    // deliberately never issues.
    if real_codex_binary().is_none() {
        eprintln!("skipping: `codex` is not on PATH");
        return;
    }
    let current_env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let env = EnvironmentPolicy::baseline().build(&current_env, &[]);
    let spec = SpawnSpec {
        program: PathBuf::from("codex"),
        args: vec!["app-server".to_string()],
        cwd: std::env::temp_dir(),
        env,
        ..SpawnSpec::minimal()
    };
    let process = Supervisor::new()
        .spawn(spec)
        .await
        .expect("spawning the real installed codex app-server must succeed");
    let (client, _inbound_rx) = CodexRpcClient::spawn(process);

    let init = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.call(
            "initialize",
            serde_json::json!({
                "clientInfo": {"name": "@nikolasd/crew", "version": "0.0.0-test"},
                "capabilities": {"experimentalApi": true}
            }),
        ),
    )
    .await
    .expect("initialize must not hang")
    .expect("initialize must succeed against the real installed binary");
    assert!(
        init.get("userAgent").is_some(),
        "InitializeResponse must carry userAgent"
    );

    client
        .notify("initialized", serde_json::json!({}))
        .expect("initialized notification must send");

    let thread = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.call(
            "thread/start",
            serde_json::json!({"cwd": std::env::temp_dir().to_string_lossy()}),
        ),
    )
    .await
    .expect("thread/start must not hang")
    .expect("thread/start must succeed against the real installed binary");
    assert!(
        thread.get("thread").and_then(|t| t.get("id")).is_some(),
        "ThreadStartResponse must carry thread.id"
    );

    client
        .terminate()
        .await
        .expect("terminating the real app-server process must succeed");
    // `shutdown` is a separate, idempotent hard-stop escape hatch (abort
    // the driver task outright, without a graceful process wait) --
    // exercised here to prove it never panics once the driver has
    // already exited on its own.
    client.shutdown();
}

/// Exercises the full [`Adapter::start`] lifecycle -- including
/// `turn/start`, which genuinely does invoke the model once Codex begins
/// working the turn -- against a real authenticated Codex account.
/// **Never run this in CI or by an agent**: it is `#[ignore]`d by
/// default specifically because it is the one path in this test file
/// that is not free of model calls. An explicit `--ignored` run is
/// itself the signal that a human wants the live call; the only thing
/// that still skips it is `CREW_DISABLE_VENDOR_CLI=1`, which forbids
/// observation-only vendor invocation. A human wanting to exercise it
/// locally: `cargo test -p batman-runtime --test codex_adapter --
/// --ignored live_start_actually_runs_a_turn_against_a_real_model`.
#[tokio::test]
#[ignore = "invokes a real model turn against an authenticated Codex account; human-run only, see doc comment"]
async fn live_start_actually_runs_a_turn_against_a_real_model() {
    // An explicit `--ignored` run already means the human wants this
    // live call -- the only remaining reason to refuse is the kill
    // switch, which forbids observation-only vendor invocation.
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        eprintln!("skipping: CREW_DISABLE_VENDOR_CLI=1 forbids live vendor-CLI invocation");
        return;
    }
    let adapter = CodexAdapter::new(
        std::env::temp_dir(),
        CodexStartupOptions::default(),
        Vec::new(),
        None,
    );
    let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink::default());
    let spec = batman_runtime::adapter::StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "reply with exactly the word done".to_string(),
        resume: None,
    };
    adapter
        .start(spec, sink)
        .await
        .expect("live start must succeed");
    adapter.dispose().await.expect("dispose must succeed");
}

// --------------------------------------------------------------- Task 7 (MCP)

#[test]
fn spawn_spec_with_no_mcp_config_injects_nothing() {
    let adapter = CodexAdapter::with_binary(
        "codex",
        std::env::temp_dir(),
        CodexStartupOptions::default(),
        Vec::new(),
        None,
    );
    let spec = adapter.spawn_spec(None);
    assert_eq!(spec.args, vec!["app-server".to_string()]);
    assert!(
        !spec.env.contains_key("CREW_WORKER_SCOPE_TOKEN"),
        "no scope token env without an AdapterMcpConfig"
    );
    assert!(
        spec.args.iter().all(|a| !a.contains("mcp_servers.crew")),
        "no crew MCP override without an AdapterMcpConfig"
    );
}

#[test]
fn spawn_spec_injects_crew_mcp_overrides_alongside_existing_config_overrides() {
    let startup_options = CodexStartupOptions {
        config_overrides: Some(vec!["model=\"o3\"".to_string()]),
        ..CodexStartupOptions::default()
    };
    let adapter = CodexAdapter::with_binary(
        "codex",
        std::env::temp_dir(),
        startup_options,
        Vec::new(),
        None,
    );
    let context = McpLaunchContext {
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: std::env::temp_dir(),
        repository: std::env::temp_dir(),
        run_id: RunId::new(),
    };
    let token = "super-secret-scope-token";
    let spec = adapter.spawn_spec(Some((&context, token)));

    // The pre-existing `-c` override from `config_overrides` survives,
    // never replaced by the crew MCP injection.
    let model_idx = spec
        .args
        .iter()
        .position(|a| a == "model=\"o3\"")
        .expect("pre-existing config override must survive MCP injection");
    assert_eq!(spec.args[model_idx - 1], "-c");

    // The crew MCP server override is additive.
    let command_idx = spec
        .args
        .iter()
        .position(|a| a == "mcp_servers.crew.command=\"/opt/crew/bin/crewd\"")
        .expect("crew command override must be present");
    assert_eq!(spec.args[command_idx - 1], "-c");
    assert!(
        spec.args
            .iter()
            .any(|a| a.starts_with("mcp_servers.crew.args=[\"coordination-mcp\", ")),
        "crew args override must be present"
    );

    // The scope token lives only in the vendor process's own env, never
    // in argv (checkable via `ps` on a real spawned process).
    assert_eq!(
        spec.env.get("CREW_WORKER_SCOPE_TOKEN"),
        Some(&token.to_string())
    );
    assert!(
        spec.args.iter().all(|a| !a.contains(token)),
        "the scope token must never appear in argv"
    );
}

#[test]
fn spawn_spec_with_mcp_config_leaves_native_discovery_flags_untouched() {
    let adapter = CodexAdapter::with_binary(
        "codex",
        std::env::temp_dir(),
        CodexStartupOptions::default(),
        Vec::new(),
        None,
    );
    let context = McpLaunchContext {
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: std::env::temp_dir(),
        repository: std::env::temp_dir(),
        run_id: RunId::new(),
    };
    let spec = adapter.spawn_spec(Some((&context, "a-token")));
    // The crew MCP server is additive alongside the base `app-server`
    // invocation -- no flag suppressing or replacing Codex's own native
    // MCP/config discovery is ever introduced.
    assert_eq!(spec.args[0], "app-server");
    for disallowed in ["--bare", "--strict-mcp-config", "--disable-builtin-mcps"] {
        assert!(
            !spec.args.iter().any(|a| a == disallowed),
            "must never add {disallowed}"
        );
    }
}

// --------------------------------------------------------------- Task 8 (conformance)

#[tokio::test]
async fn fixture_conformance_report_covers_every_canonical_scenario_exactly_once() {
    use batman_runtime::conformance::scenario;
    use std::collections::HashSet;

    let report = batman_runtime::adapter::codex::conformance::fixture_report().await;
    assert_eq!(report.scenarios.len(), 14);
    let mut seen = HashSet::new();
    for result in &report.scenarios {
        assert!(
            scenario::ALL.contains(&result.name),
            "unexpected scenario name: {}",
            result.name
        );
        assert!(
            seen.insert(result.name),
            "duplicate scenario name: {}",
            result.name
        );
    }
    for name in scenario::ALL {
        assert!(seen.contains(name), "missing scenario: {name}");
    }

    // Every scenario genuinely provable without a model call must pass;
    // FOLLOW_UP/CANCELLATION_SCOPE/SESSION_RESUME/RUNTIME_RESTART are the
    // legitimate exceptions -- codex only persists a thread's resumable
    // rollout once a turn actually runs, and turn/start is what invokes
    // the model, so none of these four are provable here; each is
    // honestly reported `passed: false` (proven instead under
    // live_report, which runs by default unless CREW_DISABLE_VENDOR_CLI=1 is set).
    let requires_live_turn = [
        scenario::FOLLOW_UP,
        scenario::CANCELLATION_SCOPE,
        scenario::SESSION_RESUME,
        scenario::RUNTIME_RESTART,
    ];
    for result in &report.scenarios {
        if requires_live_turn.contains(&result.name) {
            assert!(
                result.was_skipped(),
                "{} is not provable without a model call in fixture_report; it must be an \
                 honest skip, not a fabricated pass or a disproof",
                result.name
            );
            continue;
        }
        // Under the kill switch, READ_ONLY_START_AND_PROGRESS's real
        // `codex app-server` spawn is forbidden too, so it reports an
        // honest skip rather than spawning (R52). Any *other* reason for
        // failing here is still a real regression.
        if batman_runtime::conformance::vendor_cli_invocation_disabled() && !result.proved() {
            assert!(
                result
                    .detail
                    .contains(batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV),
                "scenario {} failed for a reason other than the kill switch: {}",
                result.name,
                result.detail
            );
            continue;
        }
        assert!(
            result.proved(),
            "expected scenario {} to pass, got: {}",
            result.name,
            result.detail
        );
    }
}
