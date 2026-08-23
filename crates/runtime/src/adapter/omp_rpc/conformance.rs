//! The OMP-RPC adapter's fixture/live conformance scenario suite. See
//! `batman_runtime::conformance` for the shared report/scenario contract
//! this module fills in.
//!
//! `fixture_report()` is reachable from a real, deployed `crewd` binary
//! (`crewd adapters --json` / `crewd conformance --fixture`, see
//! `crate::cli`), never only from `cargo test` -- so every scenario here
//! must be safe to run with no test-only tooling available (no
//! `fake-worker`, no `cargo`): each is either (a) pure, zero-process
//! in-memory logic (fixture replay through `normalize::normalize_frame`,
//! command-builder calls, capability comparisons), or (b) a real,
//! zero-model-call probe against the installed `omp` binary itself --
//! this adapter's own, always-legitimate runtime dependency, gated
//! gracefully (an honest `fail`, never a panic or a fabricated pass) when
//! unreachable. No scenario here ever sends a real `prompt` command, so none
//! of them ever actually invokes a model backend, paid or local.

use std::process::Stdio;

use serde_json::Value;

use batman_protocol::{RunId, TaskId, WorkerId};
use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterEventPayload, AdapterKind, NestedCapability,
    OmpRpcStartupOptions, ProfileId, StartupOptions, WorkerProfile,
};
use batman_runtime::conformance::report::AdapterKindLabel;
use batman_runtime::conformance::{
    ConformanceMode, ConformanceReport, ScenarioResult, VendorUnavailable, scenario,
};
use batman_runtime::coordination::mcp_protocol::BoundScope;
use batman_runtime::supervisor::{EnvironmentPolicy, SpawnSpec, Supervisor};

use super::client::{self, OmpRpcClient};
use super::normalize::{
    PROMPT_ACCEPTED_MARKER, PROMPT_COMPLETED_MARKER, extension_ui_request_to_pending_approval,
    normalize_frame,
};

pub fn conformance_profile(model: impl Into<String>) -> WorkerProfile {
    WorkerProfile {
        id: ProfileId::new(),
        adapter: "ompRpc".to_string(),
        model: model.into(),
        permission_envelope: serde_json::json!({}),
        startup_options: StartupOptions::OmpRpc(OmpRpcStartupOptions {
            profile: None,
            host_tools: None,
        }),
        environment_allowlist: Vec::new(),
        source: "conformance".to_string(),
    }
}

fn new_adapter(model: impl Into<String>) -> super::OmpRpcAdapter {
    super::OmpRpcAdapter::new(
        conformance_profile(model),
        super::OmpRpcAdapterOptions::default(),
        None,
    )
}

// ------------------------------------------------------------- fixtures

fn load_fixture_lines(name: &str) -> Vec<String> {
    let path = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/adapters/omp-rpc"
    ))
    .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()))
        .lines()
        .map(str::to_string)
        .collect()
}

/// Mirrors `OmpRpcClient`'s own recovery discipline: a line that fails to
/// parse as JSON is skipped, never fatal.
fn normalize_fixture_lines(lines: &[String]) -> Vec<AdapterEventPayload> {
    lines
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .flat_map(|frame| normalize_frame(&frame))
        .collect()
}

// -------------------------------------------------------- local selector

/// The availability probe `batman_runtime::conformance::probe_availability`
/// calls for this adapter kind: does the installed `omp` binary answer a
/// `--version` handshake? Never a model call.
///
/// Deliberately weaker than [`probe_scenario`], which additionally
/// requires a *local* model selector to be catalogued. That is a
/// conformance question -- which of this adapter's declared capabilities
/// are provable here -- not an availability one. A run whose profile
/// names a hosted model is perfectly startable on a machine with no local
/// provider listed, so gating `run/submit` on the catalog would deny work
/// the vendor CLI can plainly do.
pub async fn probe() -> (ScenarioResult, Option<String>, AdapterCapabilities) {
    let declared_capabilities = super::OmpRpcAdapter::declared_capabilities();
    // Deliberately not `OmpRpcAdapter::probe()`: that additionally
    // requires *this instance's* model selector to be catalogued, which an
    // availability check has no selector to supply.
    let output = tokio::process::Command::new("omp")
        .arg("--version")
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (
                ScenarioResult::pass(
                    scenario::PROBE,
                    format!("omp --version reported {version:?}"),
                ),
                Some(version).filter(|v| !v.is_empty()),
                declared_capabilities,
            )
        }
        Ok(_) => (
            ScenarioResult::fail(scenario::PROBE, "omp --version exited non-zero"),
            None,
            declared_capabilities,
        ),
        Err(err) => (
            ScenarioResult::fail(
                scenario::PROBE,
                format!("omp --version failed to run: {err}"),
            ),
            None,
            declared_capabilities,
        ),
    }
}

/// Resolves any selector the installed `omp` reports, for scenarios that
/// need a *syntactically real* model without ever running inference.
///
/// **Why not a local (`lm-studio`/`omlx`) selector, which this used to
/// require.** `OmpRpcAdapter::probe` verifies that the profile's model
/// appears in `omp models --json` -- a correct production rule, since a
/// worker should never start against a model `omp` does not know. But
/// conformance has no worker profile, so it has to supply a stand-in, and
/// picking the first *local* one made three scenarios (`probe`,
/// `cancellation_scope`, `follow_up`) depend on whether a separate local
/// inference server happened to be running, which models it advertised,
/// and whether `omp`'s provider catalog was warm. None of that has
/// anything to do with the adapter under test: these scenarios exercise
/// stdio framing (`ready`, `follow_up`, `abort`, `get_state`) and never
/// send a `prompt`.
///
/// Taking the first selector of any provider is deterministic instead.
/// Measured: `omp models --json` reports 583 models with the same first
/// entry under a full *and* a sanitized environment, with or without a
/// local server, because the cloud catalog ships with the binary. A cloud
/// selector costs nothing here precisely because no scenario prompts --
/// the same property the module doc already relies on.
///
/// Returns `None` (never invents a selector) when `omp` itself is
/// unreachable, reports an empty catalog, or --- since resolving a selector
/// is itself a real `omp models --json` spawn --- when
/// `batman_runtime::conformance::vendor_cli_invocation_disabled` forbids
/// observing the CLI at all.
async fn resolve_conformance_selector() -> Option<String> {
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return None;
    }
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
        .find_map(|m| m.get("selector").and_then(Value::as_str))
        .map(str::to_string)
}

// ------------------------------------------------------------------ PROBE

/// Fixed: this used to hardcode a fake `"lm-studio/x"` selector that
/// `omp models --json` never reports, so PROBE always failed, even on a
/// machine with a real local model server running. Now resolves a real
/// selector first, exactly as `tests/omp_rpc_adapter.rs` already does.
async fn probe_scenario(
    selector: Option<&str>,
) -> (ScenarioResult, Option<String>, AdapterCapabilities) {
    let declared_capabilities = super::OmpRpcAdapter::declared_capabilities();
    // `OmpRpcAdapter::probe` spawns `omp --version` and `omp models --json`
    // itself, so a `None` selector alone is not enough to keep this
    // scenario off the real binary -- and a skipped probe must report the
    // skip, never a fabricated catalog complaint.
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return (
            batman_runtime::conformance::vendor_cli_skipped_probe(),
            None,
            declared_capabilities,
        );
    }
    let Some(selector) = selector else {
        return (
            ScenarioResult::fail(
                scenario::PROBE,
                "`omp models --json` reported no usable model selector at all -- either `omp` is \
                 not installed, or its catalog is empty. This scenario needs one selector purely \
                 to satisfy `probe`'s catalog check; it never runs inference, and it no longer \
                 requires a *local* selector (which used to couple it to a separate inference \
                 server for no benefit).",
            ),
            None,
            declared_capabilities,
        );
    };
    let adapter = new_adapter(selector);
    match adapter.probe().await {
        Ok(result) => (
            ScenarioResult::pass(
                scenario::PROBE,
                format!(
                    "omp --version reported {:?}; authReady={}; model selector {selector:?} \
                     was confirmed present in `omp models --json`'s own catalog.",
                    result.version, result.auth_ready
                ),
            ),
            result.version,
            declared_capabilities,
        ),
        Err(err) => (
            ScenarioResult::fail(
                scenario::PROBE,
                format!("probe failed even though {selector:?} was listed by omp: {err}"),
            ),
            None,
            declared_capabilities,
        ),
    }
}

// ------------------------------------------- READ_ONLY_START_AND_PROGRESS

/// Fixture-only: `turn.jsonl`'s prompt-acceptance `MessageChunk` precedes
/// its turn-completion `MessageFinal` (starting a run, observed as
/// distinct from completion), and `subagents.jsonl`'s
/// `toolcall_start`/`toolcall_end` normalize into `ToolStarted`/
/// `ToolResult` mid-run progress. Both derive purely from
/// `normalize_frame`'s in-memory transformation, which never performs a
/// filesystem write of any kind -- observing start/progress this way is
/// trivially confined to no writes at all, let alone writes outside a
/// worker's own workspace.
fn read_only_start_and_progress_scenario() -> ScenarioResult {
    let turn = normalize_fixture_lines(&load_fixture_lines("turn.jsonl"));
    let accepted = turn.iter().position(|e| {
        matches!(e, AdapterEventPayload::MessageChunk { text, .. } if text.value == PROMPT_ACCEPTED_MARKER)
    });
    let completed = turn.iter().position(|e| {
        matches!(e, AdapterEventPayload::MessageFinal { text, .. } if text.value == PROMPT_COMPLETED_MARKER)
    });
    let (Some(accepted), Some(completed)) = (accepted, completed) else {
        return ScenarioResult::fail(
            scenario::READ_ONLY_START_AND_PROGRESS,
            "turn.jsonl no longer yields a distinguishable prompt-acceptance/turn-completion \
             pair",
        );
    };
    if accepted >= completed {
        return ScenarioResult::fail(
            scenario::READ_ONLY_START_AND_PROGRESS,
            "prompt acceptance did not precede turn completion in turn.jsonl",
        );
    }

    let subagents = normalize_fixture_lines(&load_fixture_lines("subagents.jsonl"));
    let started = subagents
        .iter()
        .any(|e| matches!(e, AdapterEventPayload::ToolStarted { name, .. } if name == "grep"));
    let progressed = subagents.iter().any(
        |e| matches!(e, AdapterEventPayload::ToolResult { name, ok, .. } if name == "grep" && *ok),
    );
    if !started || !progressed {
        return ScenarioResult::fail(
            scenario::READ_ONLY_START_AND_PROGRESS,
            "subagents.jsonl no longer yields a ToolStarted/ToolResult mid-run progress pair",
        );
    }

    ScenarioResult::pass(
        scenario::READ_ONLY_START_AND_PROGRESS,
        "turn.jsonl's prompt-acceptance MessageChunk precedes its turn-completion MessageFinal \
         (starting a run, observed as distinct from completion), and subagents.jsonl's \
         toolcall_start/toolcall_end normalize into ToolStarted/ToolResult mid-run progress; \
         both derive purely from normalize_frame's pure in-memory transformation, which never \
         performs a filesystem write, so observing start/progress this way never writes \
         anything, let alone outside the worker's own workspace.",
    )
}

// -------------------------------------------------------- ISOLATED_WRITE

/// `OmpRpcAdapter::start()` builds its `SpawnSpec` from
/// `..SpawnSpec::minimal()` and never overrides `.cwd` (see `mod.rs`'s
/// `spawn_spec` construction), and `Supervisor::spawn` applies that
/// `cwd` verbatim via `Command::current_dir(&spec.cwd)` -- a real,
/// zero-process fact checkable by calling the same production
/// `SpawnSpec::minimal()` this adapter's own `start()` spreads from and
/// comparing it against the real OS temp directory. Every relative-path
/// write the vendor's own edit/write tools perform is therefore confined
/// to this scratch directory, never this runtime's own working
/// directory or repository.
fn isolated_write_scenario() -> ScenarioResult {
    let spawn_cwd = SpawnSpec::minimal().cwd;
    let canonical_spawn_cwd =
        std::fs::canonicalize(&spawn_cwd).unwrap_or_else(|_| spawn_cwd.clone());
    let canonical_temp_dir =
        std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    if canonical_spawn_cwd == canonical_temp_dir {
        ScenarioResult::pass(
            scenario::ISOLATED_WRITE,
            format!(
                "OmpRpcAdapter::start() spawns `omp` via a SpawnSpec built from \
                 `..SpawnSpec::minimal()` without ever overriding `.cwd`, and Supervisor::spawn \
                 applies that cwd verbatim via Command::current_dir(&spec.cwd); \
                 SpawnSpec::minimal().cwd resolves to {} on this machine, confirmed equal to the \
                 real OS temp directory rather than this runtime's own working directory or \
                 repository.",
                canonical_spawn_cwd.display()
            ),
        )
    } else {
        ScenarioResult::fail(
            scenario::ISOLATED_WRITE,
            format!(
                "expected OmpRpcAdapter::start()'s spawn cwd ({}) to resolve to the OS temp \
                 directory ({}) -- SpawnSpec::minimal()'s own default apparently changed",
                canonical_spawn_cwd.display(),
                canonical_temp_dir.display()
            ),
        )
    }
}

// ---------------------------------------------------------------- APPROVAL

/// Backs `ApprovalsCapability::Observable` with real, checkable
/// state: `turn.jsonl` carries three `extension_ui_request` frames --
/// `confirm`, `select`, and `setWidget` -- in the exact shapes
/// `omp://rpc.md` documents. Only the two decision-shaped methods
/// (`confirm`/`select`) must produce a `PendingApproval` via
/// `extension_ui_request_to_pending_approval`; `setWidget` (and every
/// other `extension_ui_request` method) must produce none.
/// `normalize_frame` itself must keep returning zero events for all
/// three -- approvals are surfaced through `snapshot()`'s
/// `state_summary`, never through the event sink; upgrading that would
/// silently promise a capability (`AdapterEventPayload` has no approval
/// variant) this adapter does not have.
fn approval_scenario() -> ScenarioResult {
    let lines = load_fixture_lines("turn.jsonl");
    let frames: Vec<Value> = lines
        .iter()
        .filter(|line| line.contains("\"extension_ui_request\""))
        .map(|line| serde_json::from_str(line).expect("fixture line is valid JSON"))
        .collect();
    if frames.len() < 3 {
        return ScenarioResult::fail(
            scenario::APPROVAL,
            format!(
                "turn.jsonl must contain 3 extension_ui_request frames (confirm, select, \
                 setWidget) to exercise this scenario; found {}",
                frames.len()
            ),
        );
    }

    let mut seen = std::collections::HashSet::new();
    for frame in &frames {
        let events = normalize_frame(frame);
        if !events.is_empty() {
            return ScenarioResult::fail(
                scenario::APPROVAL,
                format!(
                    "normalize_frame must return zero events for every extension_ui_request; \
                     got {events:?} for {frame}"
                ),
            );
        }
        let approval = extension_ui_request_to_pending_approval(frame);
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            return ScenarioResult::fail(scenario::APPROVAL, "fixture frame has no method field");
        };
        match method {
            "confirm" | "select" => {
                let Some(approval) = approval else {
                    return ScenarioResult::fail(
                        scenario::APPROVAL,
                        format!("a {method} extension_ui_request must produce a PendingApproval"),
                    );
                };
                if approval.method != method {
                    return ScenarioResult::fail(
                        scenario::APPROVAL,
                        format!(
                            "expected PendingApproval.method {method:?}, got {:?}",
                            approval.method
                        ),
                    );
                }
            }
            "setWidget" => {
                if approval.is_some() {
                    return ScenarioResult::fail(
                        scenario::APPROVAL,
                        "setWidget must never produce a PendingApproval",
                    );
                }
            }
            other => {
                return ScenarioResult::fail(
                    scenario::APPROVAL,
                    format!("unexpected extension_ui_request method in fixture: {other}"),
                );
            }
        }
        seen.insert(method.to_string());
    }

    if seen.contains("confirm") && seen.contains("select") && seen.contains("setWidget") {
        ScenarioResult::pass(
            scenario::APPROVAL,
            "confirm and select extension_ui_request frames each produce a PendingApproval \
             (backing ApprovalsCapability::Observable via snapshot()'s state_summary); \
             setWidget produces none; normalize_frame returns zero events for all three",
        )
    } else {
        ScenarioResult::fail(
            scenario::APPROVAL,
            "turn.jsonl did not exercise all three extension_ui_request methods (confirm, \
             select, setWidget)",
        )
    }
}

// ---------------------------------- CANCELLATION_SCOPE and FOLLOW_UP (live)

/// Environment names passed through to a spawned `omp`, on top of
/// [`EnvironmentPolicy::baseline`]'s nine.
///
/// **This is not needed for any scenario to pass.** Selector resolution no
/// longer depends on a local provider (see
/// [`resolve_conformance_selector`]), so the suite is green with an empty
/// local catalog. The single reason this exists is to avoid a *side effect*
/// on the operator's own tooling:
///
/// `omp` persists its provider discovery. An invocation that cannot reach
/// the local model server records that absence in the shared catalog, so a
/// stripped-environment spawn leaves the operator's `omp models` listing
/// empty until their next `omp models refresh`. Measured A/A vs A/B/A:
/// three consecutive full-environment reads give `10, 10, 10`; interposing
/// one stripped-environment call gives `10, 0, 0`. Passing the server's
/// address keeps a conformance run from quietly degrading state Crew does
/// not own -- verified: 10 local providers before a full live run, 10
/// after.
///
/// `LM_STUDIO_BASE_URL` is an *address*, not a credential, so allowing it
/// does not widen the redaction boundary. Real runs never consult this
/// constant: `OmpRpcAdapter` builds its environment from
/// `WorkerProfile::environment_allowlist`, where an operator lists whatever
/// their model needs, exactly as they would an API key.
const OMP_LOCAL_PROVIDER_ENV: &[&str] = &["LM_STUDIO_BASE_URL"];

/// Spawns the installed `omp` binary against `selector` and waits for the
/// real `ready` handshake, never sending a `prompt` -- mirrors
/// `tests/omp_rpc_adapter.rs::spawn_ready_client` exactly (duplicated
/// here for the same reason as [`resolve_conformance_selector`]). Returns
/// `None` (never panics) if spawning/handshaking fails for any reason.
async fn spawn_ready_client(selector: &str) -> Option<(OmpRpcClient, std::path::PathBuf)> {
    let workdir = std::env::temp_dir().join(format!(
        "omp-rpc-conformance-{}-{}",
        std::process::id(),
        selector.replace('/', "-")
    ));
    std::fs::create_dir_all(&workdir).ok()?;
    let extra: Vec<String> = OMP_LOCAL_PROVIDER_ENV
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let env = EnvironmentPolicy::baseline().build(&std::env::vars().collect(), &extra);
    let spec = SpawnSpec {
        program: "omp".into(),
        args: vec![
            "--mode".into(),
            "rpc".into(),
            "--model".into(),
            selector.to_string(),
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

/// Real, zero-model-call proof for both `CANCELLATION_SCOPE` and
/// `FOLLOW_UP` against one shared spawned `omp` process (never sent a
/// `prompt`, so no model backend is ever invoked -- confirmed manually:
/// `follow_up`/`abort`/`get_state` sent without an active turn all
/// return immediately with `success: true` against the real installed
/// binary).
async fn cancellation_scope_and_follow_up_scenarios(
    selector: Option<&str>,
) -> (ScenarioResult, ScenarioResult) {
    // A `None` selector already short-circuits the spawn below, but its
    // detail would blame an empty catalog; when the kill switch is what
    // forbade the spawn, say so instead.
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        use batman_runtime::conformance::vendor_cli_required_scenario;
        return (
            vendor_cli_required_scenario(scenario::CANCELLATION_SCOPE),
            vendor_cli_required_scenario(scenario::FOLLOW_UP),
        );
    }
    let Some(selector) = selector else {
        let detail = "`omp models --json` reported no usable model selector at all, so no \
                       `omp --mode rpc` process could be started to exercise the stdio \
                       handshake. Either `omp` is not installed or its catalog is empty.";
        return (
            ScenarioResult::fail(scenario::CANCELLATION_SCOPE, detail),
            ScenarioResult::fail(scenario::FOLLOW_UP, detail),
        );
    };
    let Some((mut client, workdir)) = spawn_ready_client(selector).await else {
        let detail = "a selector was listed by `omp models --json` but spawning or handshaking \
                       against the installed omp binary failed";
        return (
            ScenarioResult::fail(scenario::CANCELLATION_SCOPE, detail),
            ScenarioResult::fail(scenario::FOLLOW_UP, detail),
        );
    };

    // FOLLOW_UP: deliver a follow-up-shaped command to the live, running
    // vendor session, using the exact command builder
    // `AdapterMessage::FollowUp` dispatches to (`Outbound::FollowUp` ->
    // `client::follow_up_command`), without ever sending a prompt.
    let follow_up_result = match client
        .send_command(
            "follow_up",
            client::follow_up_command("also update the docs"),
        )
        .await
    {
        Ok(id) => match client.read_response(&id).await {
            Ok(resp) if resp.success && resp.command == "follow_up" => ScenarioResult::pass(
                scenario::FOLLOW_UP,
                "the real installed omp binary accepted a follow_up-shaped command (built by \
                 client::follow_up_command, the exact builder OmpRpcAdapter's run_pump dispatches \
                 Outbound::FollowUp to) over the same live RPC channel a running session already \
                 owns, without ever sending a prompt; success=true was returned immediately.",
            ),
            Ok(resp) => ScenarioResult::fail(
                scenario::FOLLOW_UP,
                format!("real omp binary rejected follow_up: {:?}", resp.error),
            ),
            Err(e) => ScenarioResult::fail(
                scenario::FOLLOW_UP,
                format!("reading follow_up response failed: {e}"),
            ),
        },
        Err(e) => ScenarioResult::fail(
            scenario::FOLLOW_UP,
            format!("writing follow_up command failed: {e}"),
        ),
    };

    // CANCELLATION_SCOPE / Turn: abort must not kill the vendor process.
    let turn_accepted = match client.send_command("abort", client::abort_command()).await {
        Ok(id) => match client.read_response(&id).await {
            Ok(resp) => resp.success && resp.command == "abort",
            Err(_) => false,
        },
        Err(_) => false,
    };
    let still_alive = if turn_accepted {
        match client
            .send_command("get_state", client::get_state_command())
            .await
        {
            Ok(id) => client.read_response(&id).await.is_ok(),
            Err(_) => false,
        }
    } else {
        false
    };

    // CANCELLATION_SCOPE / Worker and Subtree (identical dispatch: both
    // map to Outbound::Terminate in OmpRpcAdapter::cancel()): terminate
    // must actually end the process. `ManagedProcess::terminate()` --
    // the exact call that arm makes -- "returns only after the
    // directly-owned leader process has actually exited (and been
    // reaped)", so a returned outcome at all is the proof.
    let termination_outcome = client.process_mut().terminate().await;
    let _ = std::fs::remove_dir_all(&workdir);

    let cancellation_scope_result = if turn_accepted && still_alive {
        ScenarioResult::pass(
            scenario::CANCELLATION_SCOPE,
            format!(
                "against the real installed omp binary: CancelScope::Turn's underlying command \
                 (`abort`, via client::abort_command -- exactly what OmpRpcAdapter::cancel()'s \
                 Turn arm dispatches Outbound::Abort to) was accepted (success=true), and the \
                 session remained alive and responsive afterward (a follow-up get_state round \
                 trip still succeeded), proving Turn scope never kills the vendor process; \
                 CancelScope::Worker and CancelScope::Subtree (identical dispatch: both map to \
                 Outbound::Terminate) then genuinely terminated that same live process via \
                 ManagedProcess::terminate(), returning {termination_outcome:?} only once the \
                 leader had actually exited and been reaped."
            ),
        )
    } else {
        ScenarioResult::fail(
            scenario::CANCELLATION_SCOPE,
            format!(
                "could not confirm Turn-scope abort left the vendor session alive against the \
                 real installed omp binary (abort accepted={turn_accepted}, still responsive \
                 afterward={still_alive})"
            ),
        )
    };

    (cancellation_scope_result, follow_up_result)
}

// ------------------------------------------------------- VENDOR_RECONNECT

/// OMP-RPC-specific: unlike Claude/Codex/Copilot, this adapter has no
/// separate worker-MCP subprocess at all -- `host_tool_call`/
/// `host_tool_result` round-trips over the SAME RPC channel this adapter
/// already owns to the one supervised `omp` process (see
/// `super::handle_host_tool_call`, intercepted inside `run_pump`'s own
/// frame loop before `normalize::normalize_frame` ever sees it). Calls
/// that exact in-process function directly with a synthetic
/// `host_tool_call` frame -- zero process spawn, zero model call -- to
/// prove it always answers rather than needing anything to "reconnect"
/// to. The same mechanism is additionally exercised end to end against a
/// real (fake) child process by
/// `tests/omp_rpc_adapter.rs::a_host_tool_call_during_the_prompt_turn_never_deadlocks_start`.
async fn vendor_reconnect_scenario() -> ScenarioResult {
    let frame = serde_json::json!({
        "type": "host_tool_call",
        "id": "htc-conformance-1",
        "toolCallId": "tc-1",
        "toolName": "crew_task",
        "arguments": {},
    });
    let scope = BoundScope {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
    };
    match super::handle_host_tool_call(&frame, None, scope).await {
        Some(reply) => {
            let is_error = reply
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            ScenarioResult::pass(
                scenario::VENDOR_RECONNECT,
                format!(
                    "not applicable to OMP-RPC as a literal reconnect: this adapter has no \
                     separate worker-MCP subprocess to reconnect at all -- host_tool_call/\
                     host_tool_result round-trips over the same RPC channel this adapter \
                     already owns to the one supervised omp process. Verified in-process: \
                     handle_host_tool_call answered a synthetic host_tool_call frame with a \
                     real host_tool_result frame (isError={is_error}) with no second \
                     connection or process involved; the same bridge is additionally exercised \
                     end to end against a real (fake) child process by \
                     tests/omp_rpc_adapter.rs::a_host_tool_call_during_the_prompt_turn_never_deadlocks_start."
                ),
            )
        }
        None => ScenarioResult::fail(
            scenario::VENDOR_RECONNECT,
            "handle_host_tool_call did not recognize a well-formed host_tool_call frame",
        ),
    }
}

// --------------------------------- SESSION_RESUME and RUNTIME_RESTART (live)

/// Spawns the real installed `omp` binary with a guaranteed-nonexistent
/// `--resume <id>` and confirms the real, documented vendor error fires
/// (grounded manually against the installed 17.1.1 binary: `Error:
/// Session "<id>" not found.`), proving `--resume <id>` -- the exact
/// flag `OmpRpcAdapter::start()` appends when `spec.resume` is set -- is
/// genuinely dispatched and interpreted by the real vendor, not silently
/// ignored or rejected as an unrecognized argument. This fires before
/// any model-selector validation (confirmed manually: the same error
/// appears with no `--model` flag at all), so unlike PROBE/
/// CANCELLATION_SCOPE/FOLLOW_UP this needs no locally-reachable model
/// selector -- only the `omp` binary itself.
async fn resume_flag_probe() -> Result<(), VendorUnavailable> {
    // Spawns the real `omp` binary regardless of any selector, so this is
    // the only place that can keep `SESSION_RESUME`/`RUNTIME_RESTART` off
    // the vendor CLI when the kill switch is set.
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return Err(VendorUnavailable::disabled(
            "the `omp --resume <id>` flag probe",
        ));
    }
    let bogus_id = format!(
        "crew-conformance-nonexistent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let workdir =
        std::env::temp_dir().join(format!("omp-rpc-conformance-resume-{}", std::process::id()));
    std::fs::create_dir_all(&workdir)
        .map_err(|e| VendorUnavailable::Failed(format!("creating scratch workdir failed: {e}")))?;

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("omp")
            .args(["--mode", "rpc", "--resume", &bogus_id, "--allow-home"])
            .current_dir(&workdir)
            .stdin(Stdio::null())
            .output(),
    )
    .await;
    let _ = std::fs::remove_dir_all(&workdir);

    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Err(VendorUnavailable::Failed(format!(
                "the omp binary is unavailable to run: {e}"
            )));
        }
        Err(_) => {
            return Err(VendorUnavailable::Failed(
                "omp --resume <bogus id> did not exit within 10s".to_string(),
            ));
        }
    };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if combined.contains(&bogus_id) && combined.to_lowercase().contains("not found") {
        Ok(())
    } else {
        Err(VendorUnavailable::Failed(format!(
            "expected the real vendor's own \"Session ... not found\" error for an unknown \
             --resume id; got: {combined}"
        )))
    }
}

/// Fixture half (`get_state`'s real `sessionId` is this adapter's
/// `VendorSessionRef`) plus the real `--resume` flag probe above.
async fn session_resume_scenario() -> ScenarioResult {
    let events = normalize_fixture_lines(&load_fixture_lines("turn.jsonl"));
    let established = events.iter().find_map(|e| match e {
        AdapterEventPayload::VendorSessionEstablished { vendor_session_id } => {
            Some(vendor_session_id.clone())
        }
        _ => None,
    });
    let Some(session_id) = established else {
        return ScenarioResult::fail(
            scenario::SESSION_RESUME,
            "turn.jsonl's get_state response no longer normalizes to VendorSessionEstablished",
        );
    };
    match resume_flag_probe().await {
        Ok(()) => ScenarioResult::pass(
            scenario::SESSION_RESUME,
            format!(
                "get_state's real data.sessionId field normalizes to VendorSessionEstablished \
                 (observed {session_id:?} from turn.jsonl -- this adapter's VendorSessionRef, \
                 carried back via StartSpec.resume); against the real installed omp binary, \
                 `--resume <id>` (the exact flag start() appends when spec.resume is set) for \
                 an unknown id produced the real, documented vendor error, proving the flag is \
                 genuinely dispatched and interpreted, not silently ignored. A fully successful \
                 resume of prior *content* would additionally require a real model call \
                 establishing a persisted session, which this fixture suite never makes, so \
                 resume is proven only at the session-reference/flag level -- matching \
                 declared_capabilities()'s own ResumeCapability::Session, not a stronger claim."
            ),
        ),
        Err(unavailable) => unavailable.into_scenario(scenario::SESSION_RESUME),
    }
}

/// A "restart" is simulated the only way it honestly can be without a
/// real runtime: a brand-new `OmpRpcAdapter` instance, which structurally
/// carries zero memory of any prior run (`inner` always starts `Idle`).
/// Confirms that structurally, then reuses the same real `--resume`
/// probe as [`session_resume_scenario`] to prove the *only* continuity
/// mechanism available after a restart -- the caller re-supplying a
/// previously observed vendor session id -- is genuinely wired through
/// to the real vendor, consistent with declared_capabilities()'s
/// `DurabilityCapability::RuntimeScoped` (not `VendorResumable`).
async fn runtime_restart_scenario() -> ScenarioResult {
    let fresh = new_adapter("unresolved/none");
    let snapshot = fresh.snapshot().await;
    let idle_confirmed = matches!(&snapshot, Ok(s) if s.state_summary == "idle");
    if !idle_confirmed {
        return ScenarioResult::fail(
            scenario::RUNTIME_RESTART,
            format!(
                "a freshly constructed OmpRpcAdapter (simulating a post-restart instance) did \
                 not report an idle snapshot: {snapshot:?}"
            ),
        );
    }
    match resume_flag_probe().await {
        Ok(()) => ScenarioResult::pass(
            scenario::RUNTIME_RESTART,
            "a freshly constructed OmpRpcAdapter instance (simulating the runtime restarting, \
             since this struct carries zero cross-instance state -- inner always starts Idle) \
             reported an idle snapshot with no memory of any prior run, confirming restart \
             recovery cannot rely on any in-process adapter state; the only continuity \
             mechanism is the caller re-supplying a previously observed vendor session id via \
             StartSpec.resume, and the real installed omp binary genuinely dispatches that id \
             through --resume (same real-process probe as SESSION_RESUME) -- consistent with \
             this adapter's declared DurabilityCapability::RuntimeScoped (not VendorResumable): \
             resuming the session *reference* is real, resuming prior *content* without a model \
             call is not claimed.",
        ),
        Err(unavailable) => unavailable.into_scenario(scenario::RUNTIME_RESTART),
    }
}

// -------------------------------------------------------- NATIVE_DISCOVERY

/// This adapter's own startup-command ordering never suppresses
/// anything, only adds: `client::build_startup_commands` only ever
/// inserts config commands (`set_subagent_subscription`/
/// `set_host_tools`/`set_host_uri_schemes`) ahead of `prompt`, never
/// removes or replaces one. Its real CLI argv (`--mode rpc --model
/// <selector> --allow-home [--profile ...] [--resume ...]`, see
/// `start()`) likewise never adds a flag disabling user/project
/// skill/plugin/hook/MCP discovery -- no `--bare`/`--strict-mcp-config`/
/// `--disable-builtin-mcps` equivalent exists anywhere in this adapter's
/// own argv construction.
fn native_discovery_scenario() -> ScenarioResult {
    let tools = [client::HostToolDefinition {
        name: "conformance_probe_tool".to_string(),
        description: "A no-op tool used only to exercise set_host_tools ordering".to_string(),
        parameters: serde_json::json!({ "type": "object", "properties": {} }),
        label: None,
        hidden: false,
    }];
    let schemes = [client::HostUriScheme {
        scheme: "battest".to_string(),
        description: None,
        writable: false,
        immutable: true,
    }];
    let commands = client::build_startup_commands(true, &tools, &schemes, "review this diff");
    let names: Vec<&str> = commands.iter().map(|(c, _)| c.as_str()).collect();
    let prompt_idx = names.iter().position(|n| *n == "prompt");
    let subs_idx = names.iter().position(|n| *n == "set_subagent_subscription");
    let tools_idx = names.iter().position(|n| *n == "set_host_tools");
    let schemes_idx = names.iter().position(|n| *n == "set_host_uri_schemes");
    match (prompt_idx, subs_idx, tools_idx, schemes_idx) {
        (Some(p), Some(s), Some(t), Some(u)) if s < p && t < p && u < p && names.len() == 4 => {
            ScenarioResult::pass(
                scenario::NATIVE_DISCOVERY,
                "build_startup_commands only ever adds config commands \
                 (set_subagent_subscription/set_host_tools/set_host_uri_schemes) ahead of \
                 prompt, never removes or suppresses anything; this adapter's real CLI argv \
                 (--mode rpc --model <selector> --allow-home[ --profile ...][ --resume ...], \
                 see start()) likewise never adds a flag disabling user/project \
                 skill/plugin/hook/MCP discovery.",
            )
        }
        _ => ScenarioResult::fail(
            scenario::NATIVE_DISCOVERY,
            format!(
                "expected exactly 4 startup commands (3 config + prompt), all config commands \
                 preceding prompt; got {names:?}"
            ),
        ),
    }
}

// -------------------------------------------------------------- REDACTION

/// This adapter's own hidden-reasoning frame types (`thinking_start`/
/// `thinking_delta`/`thinking_end`) normalize to zero
/// `AdapterEventPayload`s -- `normalize_frame` drops them before ever
/// constructing an event, so a secret-looking string embedded in a
/// `thinking_delta`'s `text` field never becomes any payload, classified
/// or not, and can never reach a journaled event. This is the primary
/// mechanism, not merely a defensive backstop at the sink.
fn redaction_scenario() -> ScenarioResult {
    let frames = [
        serde_json::json!({ "type": "thinking_start" }),
        serde_json::json!({
            "type": "thinking_delta",
            "text": "the user's API key is sk-fake-secret-marker-12345",
        }),
        serde_json::json!({ "type": "thinking_end" }),
    ];
    let events: Vec<AdapterEventPayload> = frames.iter().flat_map(normalize_frame).collect();
    if events.is_empty() {
        ScenarioResult::pass(
            scenario::REDACTION,
            "thinking_start/thinking_delta/thinking_end frames (this adapter's own hidden- \
             reasoning frame types) normalize to zero AdapterEventPayloads -- normalize_frame \
             drops them before ever constructing an event, so a secret-looking string embedded \
             in a thinking_delta's text field never becomes any payload, classified or not, and \
             can never reach a journaled event.",
        )
    } else {
        ScenarioResult::fail(
            scenario::REDACTION,
            format!(
                "expected thinking_* frames to normalize to zero events; got {} event(s): {events:?}",
                events.len()
            ),
        )
    }
}

// ------------------------------------------------ MANAGED_NESTING_REJECTION

/// A foreign adapter never advertises `nested: managed`; only OMP-native
/// nesting may.
fn managed_nesting_rejection_scenario() -> ScenarioResult {
    match super::OmpRpcAdapter::declared_capabilities().nested {
        NestedCapability::None => ScenarioResult::pass(
            scenario::MANAGED_NESTING_REJECTION,
            "declared_capabilities().nested == NestedCapability::None -- this foreign adapter \
             never advertises managed nesting; only OMP-native nesting may, per the plan's \
             Global Constraints.",
        ),
        other => ScenarioResult::fail(
            scenario::MANAGED_NESTING_REJECTION,
            format!("declared_capabilities().nested == {other:?}, not None"),
        ),
    }
}

// -------------------------------------------------- RESULT_USAGE_ARTIFACTS

/// `turn.jsonl`'s `get_state` response normalizes into
/// `VendorSessionEstablished`, its `get_session_stats` response into
/// `UsageReported`, its `prompt` response (with `agentInvoked: false`)
/// into a completing `MessageFinal`, and its real captured
/// `tool_execution_end` mutation frame into `ArtifactProduced` -- all
/// correlating to the one session the fixture replays.
///
/// The two `tool_execution_end` frames at the tail of `turn.jsonl` were
/// captured verbatim from a real `omp --mode rpc` 17.2.7 session against a
/// local `lm-studio` model told to rewrite a file. They are a matched
/// pair on purpose: the rejected edit (`isError: true`, `details: {}`)
/// must yield **no** artifact, and the accepted one (`op`/`path` under
/// `result.details`) must yield exactly one. Asserting only the positive
/// half would pass just as well against a normalizer that emitted an
/// artifact for every tool call.
fn result_usage_artifacts_scenario() -> ScenarioResult {
    let events = normalize_fixture_lines(&load_fixture_lines("turn.jsonl"));

    let session = events.iter().find_map(|e| match e {
        AdapterEventPayload::VendorSessionEstablished { vendor_session_id } => {
            Some(vendor_session_id.clone())
        }
        _ => None,
    });
    let usage = events.iter().find_map(|e| match e {
        AdapterEventPayload::UsageReported {
            input_tokens,
            output_tokens,
            cost_usd,
        } => Some((*input_tokens, *output_tokens, *cost_usd)),
        _ => None,
    });
    let completed = events
        .iter()
        .any(|e| matches!(e, AdapterEventPayload::MessageFinal { role, .. } if role == "system"));
    let artifacts = events
        .iter()
        .filter(
            |e| matches!(e, AdapterEventPayload::ArtifactProduced { artifact_kind, .. } if artifact_kind == "fileChange"),
        )
        .count();
    let failed_tool_reported = events
        .iter()
        .any(|e| matches!(e, AdapterEventPayload::ToolResult { ok, .. } if !ok));

    match (session, usage, completed, artifacts, failed_tool_reported) {
        (Some(session_id), Some((input, output, cost)), true, 1, true) => ScenarioResult::pass(
            scenario::RESULT_USAGE_ARTIFACTS,
            format!(
                "turn.jsonl's one session ({session_id}) normalized a VendorSessionEstablished, a UsageReported ({input} in / {output} out tokens, cost={cost:?}), a completing MessageFinal(role=\"system\"), and exactly one ArtifactProduced(fileChange) from the captured tool_execution_end mutation -- while the rejected edit in the same transcript reported ok=false and produced no artifact"
            ),
        ),
        (_, _, _, artifacts, failed_tool_reported) => ScenarioResult::fail(
            scenario::RESULT_USAGE_ARTIFACTS,
            format!(
                "expected VendorSessionEstablished + UsageReported + a completing MessageFinal + exactly one ArtifactProduced(fileChange) from turn.jsonl, plus a failed ToolResult for the rejected edit; saw {artifacts} artifact(s) and failed_tool_reported={failed_tool_reported}"
            ),
        ),
    }
}

// -------------------------------------------- UNEXPECTED_CHILD_OBSERVATION

/// A vendor-reported subagent normalizes into `NestedWorkerObserved`
/// even though this adapter always declares `nested: none` -- emission
/// never upgrades the declared capability.
fn unexpected_child_observation_scenario() -> ScenarioResult {
    let events = normalize_fixture_lines(&load_fixture_lines("subagents.jsonl"));
    let nested = events.iter().find_map(|e| match e {
        AdapterEventPayload::NestedWorkerObserved {
            vendor_child_id,
            vendor_parent_ref,
        } => Some((vendor_child_id.clone(), vendor_parent_ref.clone())),
        _ => None,
    });
    let Some((child, parent)) = nested else {
        return ScenarioResult::fail(
            scenario::UNEXPECTED_CHILD_OBSERVATION,
            "subagents.jsonl no longer normalizes a subagent_started frame into \
             NestedWorkerObserved",
        );
    };
    let declared_none = matches!(
        super::OmpRpcAdapter::declared_capabilities().nested,
        NestedCapability::None
    );
    if child != "sub-1" || parent != "main" || !declared_none {
        return ScenarioResult::fail(
            scenario::UNEXPECTED_CHILD_OBSERVATION,
            format!(
                "unexpected values observing the subagent fixture: child={child:?} \
                 parent={parent:?} declared_nested_none={declared_none}"
            ),
        );
    }
    ScenarioResult::pass(
        scenario::UNEXPECTED_CHILD_OBSERVATION,
        format!(
            "subagents.jsonl's subagent_started frame normalizes into \
             NestedWorkerObserved{{vendor_child_id: {child:?}, vendor_parent_ref: {parent:?}}} \
             even though declared_capabilities().nested stays NestedCapability::None -- \
             emission never upgrades the declared capability; the same fixture's agent_end \
             still completes the turn despite a subagent having run."
        ),
    )
}

// -------------------------------------------------------------- assembly

async fn build_scenarios(
    selector: Option<&str>,
) -> (Vec<ScenarioResult>, Option<String>, AdapterCapabilities) {
    let (probe_result, version, declared_capabilities) = probe_scenario(selector).await;
    let (cancellation_scope_result, follow_up_result) =
        cancellation_scope_and_follow_up_scenarios(selector).await;
    let scenarios = vec![
        probe_result,
        read_only_start_and_progress_scenario(),
        isolated_write_scenario(),
        follow_up_result,
        approval_scenario(),
        cancellation_scope_result,
        session_resume_scenario().await,
        vendor_reconnect_scenario().await,
        runtime_restart_scenario().await,
        result_usage_artifacts_scenario(),
        native_discovery_scenario(),
        redaction_scenario(),
        managed_nesting_rejection_scenario(),
        unexpected_child_observation_scenario(),
    ];
    (scenarios, version, declared_capabilities)
}

/// Runs every scenario this adapter can prove without a model call.
pub async fn fixture_report() -> ConformanceReport {
    let selector = resolve_conformance_selector().await;
    let (scenarios, version, declared_capabilities) = build_scenarios(selector.as_deref()).await;
    ConformanceReport::new(
        AdapterKindLabel::from(AdapterKind::OmpRpc),
        ConformanceMode::Fixture,
        version,
        declared_capabilities,
        scenarios,
    )
}

/// Runs the live conformance suite against the installed `omp` CLI.
///
/// Real invocation is the default. Every scenario here remains
/// zero-model-call (identical to [`fixture_report`]) -- this adapter's own
/// conformance is already bottlenecked on a real local selector being
/// reachable (see [`probe_scenario`]). Set `CREW_DISABLE_VENDOR_CLI=1`
/// to keep these real-process probes out of a CI run.
///
/// # Errors
/// Returns a message if `CREW_DISABLE_VENDOR_CLI=1` is set.
pub async fn live_report() -> Result<ConformanceReport, String> {
    if batman_runtime::conformance::vendor_cli_invocation_disabled() {
        return Err(format!(
            "live OMP-RPC conformance is disabled by {}=1",
            batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV
        ));
    }
    let selector = resolve_conformance_selector().await;
    let (scenarios, version, declared_capabilities) = build_scenarios(selector.as_deref()).await;
    Ok(ConformanceReport::new(
        AdapterKindLabel::from(AdapterKind::OmpRpc),
        ConformanceMode::Live,
        version,
        declared_capabilities,
        scenarios,
    ))
}
