//! The OMP-RPC / local-model worker adapter: launches the installed `omp`
//! binary in `--mode rpc`, speaks its real (empirically grounded, not
//! invented) JSON stdio protocol, and normalizes its frames into
//! [`AdapterEvent`]s via [`normalize::normalize_frame`].
//!
//! Grounded against the installed `omp 17.1.1` binary (plan baseline:
//! 17.0.7 -- a newer minor version; nothing this adapter relies on
//! differed): `omp --mode rpc --help` documents `--mode=<value>` as
//! accepting `rpc` among `text|json|rpc|rpc-ui`, and every wire shape
//! [`client`] builds/parses was captured from real, no-model-call
//! `omp --mode rpc --model lm-studio/<selector> ...` runs (see
//! `client.rs`'s module doc and `tests/omp_rpc_adapter.rs`).
//!
//! For local models, [`OmpRpcAdapter::probe`] resolves selectors *only*
//! from `omp models --json`'s own catalog (never invents tool
//! compatibility for an unlisted model, never calls LM Studio/oMLX
//! directly itself); `omp models --json` is real on this installed
//! version and reports genuine `lm-studio` provider entries (e.g.
//! `lm-studio/bonsai`).

pub mod client;
pub mod conformance;
pub mod normalize;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use serde_json::Value;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use batman_protocol::{RunId, TaskId, WorkerId};

use self::client::{OmpRpcClient, abort_command, follow_up_command, steer_command};
use self::normalize::{PendingApproval, extension_ui_request_to_pending_approval, normalize_frame};
use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterError, AdapterEvent, AdapterEventPayload,
    AdapterEventSink, AdapterFuture, AdapterMessage, AdapterSnapshot, ApprovalsCapability,
    CancelScope, DurabilityCapability, NativeViewCapability, NestedCapability, ProbeResult,
    ProtocolKind, ResumeCapability, StartSpec, StartupOptions, SteeringCapability, UsageCapability,
    VendorSessionRef, WorkerProfile, WorkspaceControlCapability,
};
use batman_runtime::coordination::mcp_protocol::BoundScope;
use batman_runtime::coordination::{CoordinationBroker, mcp_protocol};
use batman_runtime::supervisor::{EnvironmentPolicy, SpawnSpec, Supervisor};

/// Adapter-owned startup toggles this milestone's frozen
/// [`batman_runtime::adapter::OmpRpcStartupOptions`] has no field for. `crates/runtime/src/
/// adapter/profile.rs` is explicitly off-limits to edit for this task (see
/// the shared adapter-task context's non-negotiable constraints, which
/// supersede that same file's own doc comment inviting additive fields),
/// so nested-visibility opt-in is threaded through this adapter's own
/// construction instead of a new `WorkerProfile` field.
#[derive(Debug, Clone, Default)]
pub struct OmpRpcAdapterOptions {
    /// Whether to establish a subagent subscription (`set_subagent_
    /// subscription`) before sending the initial prompt, so vendor-spawned
    /// subagents are observable via `NestedWorkerObserved` even though
    /// this adapter always declares `nested: none`.
    pub subscribe_subagents: bool,
    /// Host tools to register via `set_host_tools` before the prompt is
    /// sent (plan Task 6 Interfaces: "host tools"). The frozen
    /// `OmpRpcStartupOptions.host_tools: Option<Vec<String>>` only carries
    /// tool *names*, but the real `set_host_tools` command requires each
    /// tool's full description and JSON-Schema `parameters` -- those come
    /// from the runtime's coordination-MCP tool registry, not from
    /// `WorkerProfile`, hence this adapter-owned field.
    pub host_tools: Vec<client::HostToolDefinition>,
    /// Host URI schemes to register via `set_host_uri_schemes` before the
    /// prompt is sent (plan Task 6 Interfaces: "host URI schemes").
    pub host_uri_schemes: Vec<client::HostUriScheme>,
}

enum Inner {
    Idle,
    Running(RunHandle),
    Disposed,
}

struct RunHandle {
    outbound_tx: mpsc::UnboundedSender<Outbound>,
    pump: tokio::task::JoinHandle<()>,
    shared: Arc<SharedRunState>,
}

enum Outbound {
    Steer(String),
    FollowUp(String),
    Abort,
    Terminate,
}

#[derive(Default)]
struct SharedRunState {
    session_id: StdMutex<Option<String>>,
    subagents: StdMutex<Vec<String>>,
    last_usage: StdMutex<Option<Value>>,
    artifacts: StdMutex<Vec<serde_json::Value>>,
    /// Approvals observed via `extension_ui_request` (`confirm`/`select`)
    /// that have not yet been resolved by a matching
    /// `extension_ui_response`, keyed by request id. Backs this
    /// adapter's `ApprovalsCapability::Observable` declaration through
    /// `snapshot()`'s `state_summary` (see that method).
    pending_approvals: StdMutex<HashMap<String, PendingApproval>>,
}
fn record_shared_state(shared: &SharedRunState, payload: &AdapterEventPayload) {
    match payload {
        AdapterEventPayload::VendorSessionEstablished { vendor_session_id } => {
            *shared
                .session_id
                .lock()
                .expect("session_id mutex is never poisoned") = Some(vendor_session_id.clone());
        }
        AdapterEventPayload::NestedWorkerObserved {
            vendor_child_id, ..
        } => {
            shared
                .subagents
                .lock()
                .expect("subagents mutex is never poisoned")
                .push(vendor_child_id.clone());
        }
        AdapterEventPayload::UsageReported {
            input_tokens,
            output_tokens,
            cost_usd,
        } => {
            *shared
                .last_usage
                .lock()
                .expect("last_usage mutex is never poisoned") = Some(serde_json::json!({
                "inputTokens": input_tokens,
                "outputTokens": output_tokens,
                "costUsd": cost_usd,
            }));
        }
        AdapterEventPayload::ArtifactProduced {
            artifact_id,
            artifact_kind,
        } => {
            shared
                .artifacts
                .lock()
                .expect("artifacts mutex is never poisoned")
                .push(serde_json::json!({
                    "artifactId": artifact_id.to_string(),
                    "artifactKind": artifact_kind,
                }));
        }
        _ => {}
    }
}

/// The coordination-MCP tool registry's tools, converted into the wire
/// shape `set_host_tools` requires (plan Task 6 Interfaces: "host
/// tools"). Kept next to [`handle_host_tool_call`] -- the two must never
/// drift: whatever is registered here must be exactly what that function
/// can answer.
fn coordination_host_tool_definitions() -> Vec<client::HostToolDefinition> {
    mcp_protocol::tool_specs()
        .into_iter()
        .map(|spec| client::HostToolDefinition {
            name: spec.name.to_string(),
            description: spec.description.to_string(),
            parameters: spec.input_schema,
            label: None,
            hidden: false,
        })
        .collect()
}

/// Translates [`mcp_protocol::tool_result_from_success`]/
/// [`mcp_protocol::tool_result_from_error`]'s MCP `tools/call` result
/// shape (`{"content", "structuredContent"?, "isError"}`, all inside one
/// object) into OMP's own, differently-shaped `host_tool_result` wire
/// convention (`isError` a *sibling* of `result`, not nested inside it;
/// `result` itself carrying only `content` on success, or `content`
/// plus an empty `details` object on failure) -- see `client.rs`'s
/// module doc for exactly where that shape was read out of the
/// installed binary's own bundled source. Pure and synchronous so the
/// exact wire shape is unit-testable without a broker or a process.
fn mcp_result_to_host_tool_result_frame(id: &str, mcp_result: &Value) -> Value {
    let content = mcp_result
        .get("content")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    if mcp_result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        serde_json::json!({
            "type": "host_tool_result",
            "id": id,
            "result": { "content": content, "details": {} },
            "isError": true,
        })
    } else {
        serde_json::json!({
            "type": "host_tool_result",
            "id": id,
            "result": { "content": content },
        })
    }
}

/// Answers one `host_tool_call` frame in-process against `broker`,
/// returning the `host_tool_result` frame to write back verbatim, or
/// `None` if `frame` is not a `host_tool_call` at all (the ordinary
/// case, when the caller should fall through to
/// [`normalize::normalize_frame`] instead).
async fn handle_host_tool_call(
    frame: &Value,
    broker: Option<&CoordinationBroker>,
    scope: BoundScope,
) -> Option<Value> {
    if frame.get("type").and_then(Value::as_str) != Some("host_tool_call") {
        return None;
    }
    let id = frame.get("id").and_then(Value::as_str).unwrap_or_default();
    let tool_name = frame
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = frame
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let mcp_result = match broker {
        Some(broker) => broker.execute_tool_call(tool_name, &arguments, scope).await,
        None => mcp_protocol::tool_result_from_error(
            "this worker was not started with worker-coordination tools available",
        ),
    };
    Some(mcp_result_to_host_tool_result_frame(id, &mcp_result))
}

/// The `omp --mode rpc` / local-model worker adapter.
pub struct OmpRpcAdapter {
    omp_bin: String,
    profile: WorkerProfile,
    options: OmpRpcAdapterOptions,
    broker: Option<Arc<CoordinationBroker>>,
    inner: AsyncMutex<Inner>,
}

impl OmpRpcAdapter {
    /// `broker` is `Some` to give this adapter's supervised `omp`
    /// process access to the worker coordination tools via `set_host_tools`
    /// -- OMP-RPC has no separate MCP subprocess of its own to inject a
    /// scope-token-authenticated socket connection into (see
    /// `batman_runtime::adapter::mcp_config`'s module doc for why), so
    /// this adapter fulfills `host_tool_call` frames directly, in-process,
    /// against `broker` (see `run_pump`'s interception of that frame,
    /// before `normalize::normalize_frame` ever sees it). When `Some`,
    /// [`Self::new`] appends the [`crate::coordination::mcp_protocol`]
    /// tool registry's definitions to `options.host_tools`, first
    /// dropping any caller-supplied entry whose `name` collides with one
    /// -- registration and invocation-handling can never drift out of
    /// sync with each other since both derive from this one input, and a
    /// name collision can never silently register two tools OMP would
    /// dispatch ambiguously between.
    #[must_use]
    pub fn new(
        profile: WorkerProfile,
        mut options: OmpRpcAdapterOptions,
        broker: Option<Arc<CoordinationBroker>>,
    ) -> Self {
        if broker.is_some() {
            let coordination_tools = coordination_host_tool_definitions();
            let coordination_names: std::collections::HashSet<&str> =
                coordination_tools.iter().map(|t| t.name.as_str()).collect();
            options
                .host_tools
                .retain(|t| !coordination_names.contains(t.name.as_str()));
            options.host_tools.extend(coordination_tools);
        }
        Self {
            omp_bin: "omp".to_string(),
            profile,
            options,
            broker,
            inner: AsyncMutex::new(Inner::Idle),
        }
    }

    /// As [`Self::new`], but resolves `omp` from `omp_bin` instead of a
    /// bare `PATH` lookup (used by tests to pin an exact binary, e.g. a
    /// `fake-worker --mode omp-rpc-host-tool` stand-in).
    #[must_use]
    pub fn with_binary(
        omp_bin: impl Into<String>,
        profile: WorkerProfile,
        options: OmpRpcAdapterOptions,
        broker: Option<Arc<CoordinationBroker>>,
    ) -> Self {
        Self {
            omp_bin: omp_bin.into(),
            ..Self::new(profile, options, broker)
        }
    }

    /// The capabilities declared by this adapter -- see the module-level
    /// and `tests/omp_rpc_adapter.rs` doc comments for exactly which
    /// fixture/probe proves each field.
    #[must_use]
    pub fn declared_capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            protocol: ProtocolKind::Structured,
            // Proven structurally by ready_and_get_state_round_trip_against_installed_omp
            // (real `--resume <id>` flag exists) plus the real vendor
            // error surfaced for an unknown session id observed during
            // development (`Error: Session "<id>" not found.`); a fully
            // successful resume of *content* could not be proven without
            // a real model call establishing a persisted session, so this
            // is deliberately not upgraded to claim more than that.
            resume: ResumeCapability::Session,
            // `get_state`'s real `steeringMode`/`followUpMode` fields
            // report `"one-at-a-time"` by default on the installed
            // binary -- i.e. queued, not concurrent mid-turn steering.
            steering: SteeringCapability::Queued,
            // Approval requests (`extension_ui_request` `confirm`/
            // `select`) are observed and reflected into `snapshot()`'s
            // `state_summary` (see `record_shared_state`/`snapshot`), but
            // deliberately not resolved through this adapter:
            // `respond_to_approval` stays `capability_unsupported` (see
            // its own doc comment) since `omp://rpc.md`'s
            // `extension_ui_response` wire path is a separate capability
            // upgrade to `Controllable`, out of this milestone's scope.
            approvals: ApprovalsCapability::Observable,
            structured_result: true,
            // Proven by get_session_stats_response_normalizes_to_usage_reported
            // against the real `data.tokens.{input,output}` / `data.cost`
            // shape; session-lifetime aggregate, not per-turn/per-child.
            usage: UsageCapability::Aggregate,
            // Mandated by the plan's Global Constraints for every
            // foreign-adapter integration: `NestedWorkerObserved` is still
            // emitted (see `normalize.rs`), which never upgrades this.
            nested: NestedCapability::None,
            native_view: NativeViewCapability::None,
            // The real CLI's default tool set (`edit`, `write`, `bash`,
            // ...) is enabled unless a profile explicitly narrows it.
            workspace_control: WorkspaceControlCapability::Write,
            // A persisted OMP session file was observed to only exist
            // once real conversational content exists; proving genuine
            // vendor-side resumability would require a real model call,
            // which this milestone's tests must never make -- see the
            // shared context's instruction to test, not assume, before
            // declaring `VendorResumable`.
            durability: DurabilityCapability::RuntimeScoped,
        }
    }

    fn model_selector(&self) -> &str {
        &self.profile.model
    }

    fn profile_startup_options(&self) -> Option<&batman_runtime::adapter::OmpRpcStartupOptions> {
        match &self.profile.startup_options {
            StartupOptions::OmpRpc(options) => Some(options),
            _ => None,
        }
    }
}

impl Adapter for OmpRpcAdapter {
    fn kind(&self) -> &str {
        "ompRpc"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        Self::declared_capabilities()
    }

    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        Box::pin(async move {
            let version_output = tokio::process::Command::new(&self.omp_bin)
                .arg("--version")
                .output()
                .await
                .map_err(|e| {
                    AdapterError::unavailable(
                        self.kind(),
                        "probe",
                        format!("omp --version failed to run: {e}"),
                    )
                })?;
            if !version_output.status.success() {
                return Err(AdapterError::unavailable(
                    self.kind(),
                    "probe",
                    "omp --version exited non-zero",
                ));
            }
            let version = String::from_utf8_lossy(&version_output.stdout)
                .trim()
                .to_string();

            let models_output = tokio::process::Command::new(&self.omp_bin)
                .args(["models", "--json"])
                .output()
                .await
                .map_err(|e| {
                    AdapterError::unavailable(
                        self.kind(),
                        "probe",
                        format!("omp models --json failed to run: {e}"),
                    )
                })?;
            if !models_output.status.success() {
                return Err(AdapterError::incompatible_version(
                    self.kind(),
                    "probe",
                    "the installed omp binary does not support `models --json`",
                ));
            }
            let catalog: Value = serde_json::from_slice(&models_output.stdout).map_err(|e| {
                AdapterError::protocol(
                    self.kind(),
                    "probe",
                    format!("omp models --json produced invalid JSON: {e}"),
                )
            })?;
            let selector = self.model_selector();
            let known = catalog
                .get("models")
                .and_then(Value::as_array)
                .is_some_and(|models| {
                    models
                        .iter()
                        .any(|m| m.get("selector").and_then(Value::as_str) == Some(selector))
                });
            if !known {
                return Err(AdapterError::incompatible_version(
                    self.kind(),
                    "probe",
                    format!(
                        "model selector {selector:?} is not reported by `omp models --json`; \
                         this adapter never invents tool compatibility for an unlisted model"
                    ),
                ));
            }

            Ok(ProbeResult {
                version: Some(version).filter(|v| !v.is_empty()),
                auth_ready: true,
                capabilities: Self::declared_capabilities(),
                inventory_incomplete: false,
            })
        })
    }

    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut guard = self.inner.lock().await;
            if !matches!(*guard, Inner::Idle) {
                return Err(AdapterError::invalid_vendor_state(
                    self.kind(),
                    "start",
                    "this adapter instance has already been started or disposed",
                ));
            }

            let mut args = vec![
                "--mode".to_string(),
                "rpc".to_string(),
                "--model".to_string(),
                self.model_selector().to_string(),
                "--allow-home".to_string(),
            ];
            if let Some(options) = self.profile_startup_options()
                && let Some(profile_name) = &options.profile
            {
                args.push("--profile".to_string());
                args.push(profile_name.clone());
            }
            if let Some(resume) = &spec.resume {
                args.push("--resume".to_string());
                args.push(resume.0.clone());
            }

            let current_env: HashMap<String, String> = std::env::vars().collect();
            let env = EnvironmentPolicy::baseline()
                .build(&current_env, &self.profile.environment_allowlist);
            let spawn_spec = SpawnSpec {
                program: self.omp_bin.clone().into(),
                args,
                env,
                ..SpawnSpec::minimal()
            };
            let supervisor = Supervisor::new();
            let process = supervisor
                .spawn(spawn_spec)
                .await
                .map_err(|e| AdapterError::process(self.kind(), "start", e.to_string()))?;
            let pid = process.pid();

            sink.emit(AdapterEvent {
                run_id: spec.run_id,
                task_id: spec.task_id,
                worker_id: spec.worker_id,
                payload: AdapterEventPayload::ProcessStarted { pid: pid as u32 },
            })
            .await
            .map_err(|e| AdapterError::process(self.kind(), "start", e.to_string()))?;

            let mut rpc_client = OmpRpcClient::new(process);
            rpc_client.wait_for_ready().await?;

            // The `prompt` command is handled separately, deliberately
            // *not* `read_response`'d here: the installed binary's own
            // dispatch source awaits the full model turn before ever
            // responding to it (`case "prompt": { const H = await
            // kI1(A, E.message, E.streamingBehavior) }`), and that turn
            // can itself emit a `host_tool_call` frame this process must
            // answer before OMP will ever send that response. This
            // adapter's own `read_response` wait-loop only queues
            // anything that is not the awaited response -- it can never
            // answer one -- so blocking on it here would deadlock the
            // moment a model turn invokes one of `self.options.host_
            // tools`. Only `run_pump`'s frame loop answers `host_tool_
            // call` (see `handle_host_tool_call`), so every command that
            // can trigger one must be observed there instead. The
            // config commands above it are provably tool-call-free (each
            // one's real dispatch handler -- `fNw`/`setTools`/
            // `refreshRpcHostTools`/`setSchemes` -- never awaits a model
            // turn), so they keep the original synchronous, error-
            // propagating, before-the-prompt-is-sent sequencing.
            let mut commands = client::build_startup_commands(
                self.options.subscribe_subagents,
                &self.options.host_tools,
                &self.options.host_uri_schemes,
                &spec.prompt,
            );
            let (prompt_command, prompt_params) = commands
                .pop()
                .expect("build_startup_commands always pushes the prompt command last");
            debug_assert_eq!(prompt_command, "prompt");
            for (command, params) in commands {
                let id = rpc_client.send_command(&command, params).await?;
                let response = rpc_client.read_response(&id).await?;
                let frame = serde_json::json!({
                    "type": "response",
                    "id": response.id,
                    "command": response.command,
                    "success": response.success,
                    "data": response.data,
                    "error": response.error,
                });
                for payload in normalize_frame(&frame) {
                    let _ = sink
                        .emit(AdapterEvent {
                            run_id: spec.run_id,
                            task_id: spec.task_id,
                            worker_id: spec.worker_id,
                            payload,
                        })
                        .await;
                }
            }
            rpc_client
                .send_command(&prompt_command, prompt_params)
                .await?;

            let shared = Arc::new(SharedRunState::default());
            let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
            let pump = tokio::spawn(run_pump(
                rpc_client,
                sink,
                spec.run_id,
                spec.task_id,
                spec.worker_id,
                Arc::clone(&shared),
                outbound_rx,
                self.broker.clone(),
            ));

            *guard = Inner::Running(RunHandle {
                outbound_tx,
                pump,
                shared,
            });
            Ok(())
        })
    }

    fn resume(
        &self,
        session: VendorSessionRef,
        sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            self.start(
                StartSpec {
                    run_id: RunId::new(),
                    task_id: TaskId::new(),
                    worker_id: WorkerId::new(),
                    prompt: String::new(),
                    resume: Some(session),
                },
                sink,
            )
            .await
        })
    }

    fn send(&self, message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let guard = self.inner.lock().await;
            let Inner::Running(handle) = &*guard else {
                return Err(AdapterError::invalid_vendor_state(
                    self.kind(),
                    "send",
                    "no run is currently active",
                ));
            };
            let outbound = match message {
                AdapterMessage::Steer { text } => Outbound::Steer(text),
                AdapterMessage::FollowUp { text } => Outbound::FollowUp(text),
                // Neither a real RPC command name for a plain "answer" nor
                // an inter-worker "peer message" delivery path was
                // confirmed against the installed binary's dispatch
                // switch; approximating either through `steer`/`follow_up`
                // would silently misrepresent a distinct message kind, so
                // both report unsupported explicitly instead.
                AdapterMessage::Answer { .. } | AdapterMessage::PeerMessage { .. } => {
                    return Err(AdapterError::capability_unsupported(self.kind(), "send"));
                }
            };
            handle.outbound_tx.send(outbound).map_err(|_| {
                AdapterError::process(
                    self.kind(),
                    "send",
                    "run pump task is no longer accepting commands",
                )
            })
        })
    }

    fn respond_to_approval(&self, _approval_id: &str, _decision: &str) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            // Approvals are only Observable for this adapter (see
            // `declared_capabilities`): the shared adapter-task context
            // requires normalizing an observed approval request into
            // internal state (`snapshot()`), not wiring it through
            // `ApprovalService` end-to-end from here -- that RPC seam is
            // explicitly a follow-up integration point, not this task's
            // scope.
            Err(AdapterError::capability_unsupported(
                self.kind(),
                "respondToApproval",
            ))
        })
    }

    fn cancel(&self, scope: CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let guard = self.inner.lock().await;
            let Inner::Running(handle) = &*guard else {
                return Ok(());
            };
            let outbound = match scope {
                CancelScope::Turn => Outbound::Abort,
                CancelScope::Worker | CancelScope::Subtree => Outbound::Terminate,
            };
            handle.outbound_tx.send(outbound).map_err(|_| {
                AdapterError::process(
                    self.kind(),
                    "cancel",
                    "run pump task is no longer accepting commands",
                )
            })
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async move {
            let guard = self.inner.lock().await;
            let (state_summary, children, usage, artifacts) = match &*guard {
                Inner::Idle => ("idle".to_string(), Vec::new(), None, Vec::new()),
                Inner::Disposed => ("disposed".to_string(), Vec::new(), None, Vec::new()),
                Inner::Running(handle) => {
                    let session_id = handle
                        .shared
                        .session_id
                        .lock()
                        .expect("session_id mutex is never poisoned")
                        .clone();
                    let children = handle
                        .shared
                        .subagents
                        .lock()
                        .expect("subagents mutex is never poisoned")
                        .clone();
                    let usage = handle
                        .shared
                        .last_usage
                        .lock()
                        .expect("last_usage mutex is never poisoned")
                        .clone();
                    let artifacts = handle
                        .shared
                        .artifacts
                        .lock()
                        .expect("artifacts mutex is never poisoned")
                        .clone();
                    let pending_approvals = handle
                        .shared
                        .pending_approvals
                        .lock()
                        .expect("pending_approvals mutex is never poisoned")
                        .values()
                        .map(|approval| format!("{}:{}", approval.method, approval.title))
                        .collect::<Vec<_>>();
                    let mut summary = match session_id {
                        Some(id) => format!("running (session {id})"),
                        None => "running".to_string(),
                    };
                    if !pending_approvals.is_empty() {
                        summary.push_str(&format!(
                            ", {} pending approval(s): {}",
                            pending_approvals.len(),
                            pending_approvals.join(", ")
                        ));
                    }
                    (summary, children, usage, artifacts)
                }
            };
            Ok(AdapterSnapshot {
                state_summary,
                children,
                usage,
                artifacts,
            })
        })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut guard = self.inner.lock().await;
            if let Inner::Running(handle) = std::mem::replace(&mut *guard, Inner::Disposed) {
                let _ = handle.outbound_tx.send(Outbound::Terminate);
                let _ = handle.pump.await;
            }
            Ok(())
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_pump(
    mut client: OmpRpcClient,
    sink: Arc<dyn AdapterEventSink>,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    shared: Arc<SharedRunState>,
    mut outbound_rx: mpsc::UnboundedReceiver<Outbound>,
    broker: Option<Arc<CoordinationBroker>>,
) {
    let scope = BoundScope {
        run_id,
        task_id,
        worker_id,
    };
    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                match outbound {
                    Some(Outbound::Steer(text)) => {
                        let _ = client.send_command("steer", steer_command(&text)).await;
                    }
                    Some(Outbound::FollowUp(text)) => {
                        let _ = client.send_command("follow_up", follow_up_command(&text)).await;
                    }
                    Some(Outbound::Abort) => {
                        let _ = client.send_command("abort", abort_command()).await;
                    }
                    Some(Outbound::Terminate) | None => {
                        let (exit_code, signal) = client.process_mut().terminate().await.exit_signals();
                        let _ = sink
                            .emit(AdapterEvent {
                                run_id,
                                task_id,
                                worker_id,
                                payload: AdapterEventPayload::ProcessExited { exit_code, signal },
                            })
                            .await;
                        return;
                    }
                }
            }
            frame = client.next_frame() => {
                match frame {
                    Some(value) => {
                        if let Some(reply) =
                            handle_host_tool_call(&value, broker.as_deref(), scope).await
                        {
                            let _ = client.write_frame(&reply).await;
                            continue;
                        }
                        if let Some(approval) = extension_ui_request_to_pending_approval(&value) {
                            shared
                                .pending_approvals
                                .lock()
                                .unwrap()
                                .insert(approval.request_id.clone(), approval);
                        }
                        for payload in normalize_frame(&value) {
                            record_shared_state(&shared, &payload);
                            let _ = sink
                                .emit(AdapterEvent { run_id, task_id, worker_id, payload })
                                .await;
                        }
                    }
                    None => {
                        let (exit_code, signal) = client.process_mut().settle().await.exit_signals();
                        let _ = sink
                            .emit(AdapterEvent {
                                run_id,
                                task_id,
                                worker_id,
                                payload: AdapterEventPayload::ProcessExited { exit_code, signal },
                            })
                            .await;
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod host_tool_bridge_tests {
    use super::{
        BoundScope, CoordinationBroker, RunId, TaskId, WorkerId, handle_host_tool_call,
        mcp_result_to_host_tool_result_frame,
    };
    use batman_protocol::ProjectId;
    use batman_runtime::coordination::mcp_protocol;
    use batman_runtime::db::DatabaseHandle;

    fn scope() -> BoundScope {
        BoundScope {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
        }
    }

    #[test]
    fn success_result_carries_only_content_with_no_top_level_is_error() {
        let mcp_result = mcp_protocol::tool_result_from_success(
            "crew_peers",
            &serde_json::json!({ "peers": [] }),
        )
        .expect("a valid crew_peers result must match its own output schema");
        let frame = mcp_result_to_host_tool_result_frame("req-1", &mcp_result);
        assert_eq!(frame["type"], "host_tool_result");
        assert_eq!(frame["id"], "req-1");
        assert!(
            frame.get("isError").is_none(),
            "success frame must omit isError entirely"
        );
        assert!(frame["result"].get("details").is_none());
        assert_eq!(frame["result"]["content"], mcp_result["content"]);
    }

    #[test]
    fn error_result_moves_is_error_out_to_a_sibling_of_result_and_adds_empty_details() {
        let mcp_result = mcp_protocol::tool_result_from_error("boom");
        let frame = mcp_result_to_host_tool_result_frame("req-2", &mcp_result);
        assert_eq!(frame["isError"], true);
        assert!(
            frame["result"].get("isError").is_none(),
            "isError must not remain nested"
        );
        assert_eq!(frame["result"]["details"], serde_json::json!({}));
        assert_eq!(frame["result"]["content"], mcp_result["content"]);
    }

    #[tokio::test]
    async fn non_host_tool_call_frames_pass_through_as_none() {
        let frame = serde_json::json!({ "type": "agent_end", "id": "x" });
        assert!(handle_host_tool_call(&frame, None, scope()).await.is_none());
    }

    #[tokio::test]
    async fn a_host_tool_call_is_always_answered_even_without_a_broker() {
        // The specific property that prevents the startup deadlock: this
        // must return `Some` (a frame to write back) for every
        // `host_tool_call`, never leaving one unanswered, regardless of
        // whether worker-coordination tools are actually available.
        let frame = serde_json::json!({
            "type": "host_tool_call",
            "id": "htc-1",
            "toolCallId": "tc-1",
            "toolName": "crew_task",
            "arguments": {},
        });
        let reply = handle_host_tool_call(&frame, None, scope())
            .await
            .expect("a host_tool_call without a broker must still be answered, never dropped");
        assert_eq!(reply["type"], "host_tool_result");
        assert_eq!(reply["id"], "htc-1");
        assert_eq!(reply["isError"], true);
    }

    /// Proves the `Some(broker)` branch of [`handle_host_tool_call`]
    /// actually calls through to a real [`CoordinationBroker`] and
    /// returns its genuine success result -- the pure-translation tests
    /// above only ever exercise a hand-built [`mcp_protocol`] result or
    /// the no-broker error path, never this call itself.
    #[tokio::test]
    async fn a_host_tool_call_against_a_real_broker_returns_its_genuine_success_result() {
        let db_file = tempfile::NamedTempFile::new().expect("temp db file");
        let db = std::sync::Arc::new(
            DatabaseHandle::start(db_file.path().to_path_buf())
                .await
                .expect("database must start"),
        );
        let project_id = ProjectId::new();
        let run_id = RunId::new();
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        db.run_domain_op(Box::new({
            let (run_id, task_id, worker_id, project_id) =
                (run_id, task_id, worker_id, project_id);
            move |conn| {
                conn.execute(
                    "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                    rusqlite::params![task_id.to_string(), project_id.to_string(), "test-owner", 1, "2026-01-01T00:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![worker_id.to_string(), "sha256:x", "ompRpc", "m", "{}"],
                )?;
                conn.execute(
                    "INSERT INTO workers (worker_id, project_id, profile_id, created_at) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![worker_id.to_string(), project_id.to_string(), worker_id.to_string(), "2026-01-01T00:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO runs (run_id, task_id, worker_id, state, created_at) VALUES (?1, ?2, ?3, 'working', ?4)",
                    rusqlite::params![run_id.to_string(), task_id.to_string(), worker_id.to_string(), "2026-01-01T00:00:00Z"],
                )?;
                Ok::<_, batman_runtime::domain::DomainError>(serde_json::json!({}))
            }
        }))
        .await
        .expect("seeding a task/worker/run must succeed");

        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(16);
        let lease_service = std::sync::Arc::new(
            batman_runtime::workspace::LeaseService::open_in_memory(project_id)
                .expect("in-memory lease service must open"),
        );
        let broker = CoordinationBroker::new(
            db,
            project_id,
            events_tx,
            lease_service,
            std::sync::Arc::new(crate::workspace::ArtifactStore::new()),
        );
        let scope = BoundScope {
            run_id,
            task_id,
            worker_id,
        };
        let frame = serde_json::json!({
            "type": "host_tool_call",
            "id": "htc-real",
            "toolCallId": "tc-real",
            "toolName": "crew_task",
            "arguments": {},
        });

        let reply = handle_host_tool_call(&frame, Some(&broker), scope)
            .await
            .expect("a host_tool_call must always be answered");
        assert_eq!(reply["type"], "host_tool_result");
        assert_eq!(reply["id"], "htc-real");
        assert!(
            reply.get("isError").is_none(),
            "a real, successful crew_task call must not be reported as an error: {reply}"
        );
        let content = reply["result"]["content"]
            .as_array()
            .expect("a successful result always carries a content array");
        assert!(
            !content.is_empty(),
            "crew_task's real success content must not be empty"
        );
    }
}
