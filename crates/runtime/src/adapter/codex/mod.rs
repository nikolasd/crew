//! The Codex `app-server` worker adapter: launches `codex app-server`
//! over stdio, speaks its correlated JSON-RPC protocol
//! (`initialize` -> `initialized` -> `thread/start`|`thread/resume` ->
//! `turn/start`/`turn/steer`/`turn/interrupt`), and normalizes its
//! notifications into [`AdapterEvent`]s.
//!
//! Grounded against the real installed `codex-cli 0.145.0` binary: every
//! method name and required field this module depends on is checked by
//! [`schema::verify_against_installed_binary`] against
//! `codex app-server generate-json-schema`'s live output (see
//! `crates/runtime/tests/codex_adapter.rs`), not assumed from
//! documentation alone.

pub mod client;
pub mod conformance;
pub mod normalize;
pub mod schema;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::adapter::mcp_config::{
    AdapterMcpConfig, McpLaunchContext, codex_mcp_overrides, coordination_mcp_env,
};
use crate::adapter::{
    Adapter, AdapterCapabilities, AdapterError, AdapterEvent, AdapterEventPayload,
    AdapterEventSink, AdapterFuture, AdapterMessage, AdapterSnapshot, ApprovalsCapability,
    CancelScope, CodexStartupOptions, DurabilityCapability, NativeViewCapability, NestedCapability,
    ProbeResult, ProtocolKind, ResumeCapability, StartSpec, SteeringCapability, UsageCapability,
    VendorSessionRef, WorkspaceControlCapability,
};
use crate::coordination::ScopeTokenStore;
use crate::supervisor::{EnvironmentPolicy, SpawnSpec, Supervisor};

use client::{ClientError, CodexRpcClient, InboundMessage};
use normalize::PendingApproval;

const KIND: &str = "codex";

/// This adapter's declared capabilities. Every field's justification is
/// documented on [`CodexAdapter::capabilities`].
fn declared_capabilities() -> AdapterCapabilities {
    AdapterCapabilities {
        protocol: ProtocolKind::Structured,
        resume: ResumeCapability::Session,
        steering: SteeringCapability::ActiveTurn,
        approvals: ApprovalsCapability::Controllable,
        structured_result: true,
        usage: UsageCapability::PerTurn,
        nested: NestedCapability::None,
        native_view: NativeViewCapability::None,
        workspace_control: WorkspaceControlCapability::Write,
        durability: DurabilityCapability::VendorResumable,
    }
}

/// Mutable per-run state, guarded by one `tokio::sync::Mutex` so every
/// `Adapter` method can `await` while holding it.
struct RunState {
    run_id: batman_protocol::RunId,
    client: Arc<CodexRpcClient>,
    thread_id: String,
    current_turn_id: Option<String>,
    pending_approvals: HashMap<String, PendingApproval>,
    pump: JoinHandle<()>,
}

/// The Codex `app-server` worker adapter.
pub struct CodexAdapter {
    codex_bin: String,
    cwd: PathBuf,
    environment_allowlist: Vec<String>,
    startup_options: CodexStartupOptions,
    supervisor: Supervisor,
    mcp: Option<AdapterMcpConfig>,
    run: Mutex<Option<RunState>>,
}

impl CodexAdapter {
    /// Constructs a `CodexAdapter` that launches `codex` (resolved via
    /// `PATH`) with `startup_options` (`sandboxMode`/`approvalPolicy`/
    /// `configOverrides`, from the worker profile's own
    /// [`CodexStartupOptions`]) and `environment_allowlist` (the worker
    /// profile's `environmentAllowlist`).
    #[must_use]
    pub fn new(
        cwd: PathBuf,
        startup_options: CodexStartupOptions,
        environment_allowlist: Vec<String>,
        mcp: Option<AdapterMcpConfig>,
    ) -> Self {
        Self {
            codex_bin: "codex".to_string(),
            cwd,
            environment_allowlist,
            startup_options,
            supervisor: Supervisor::new(),
            mcp,
            run: Mutex::new(None),
        }
    }

    /// As [`Self::new`], but resolves `codex` from `codex_bin` instead of
    /// bare `PATH` lookup (used by tests to pin an exact binary).
    #[must_use]
    pub fn with_binary(
        codex_bin: impl Into<String>,
        cwd: PathBuf,
        startup_options: CodexStartupOptions,
        environment_allowlist: Vec<String>,
        mcp: Option<AdapterMcpConfig>,
    ) -> Self {
        Self {
            codex_bin: codex_bin.into(),
            ..Self::new(cwd, startup_options, environment_allowlist, mcp)
        }
    }

    /// Builds this run's `app-server` command line and environment.
    /// `mcp_injection`, when `Some`, is `(launch context, reserved scope
    /// token)` -- appends the two `-c mcp_servers.crew.*` overrides to
    /// `args` (alongside, never replacing, any `config_overrides`-derived
    /// `-c` pairs already present) and merges `CREW_WORKER_SCOPE_TOKEN`
    /// into `env`. Exposed `pub` so this adapter's own tests can assert
    /// on the exact injected shape without spawning a process.
    #[must_use]
    pub fn spawn_spec(&self, mcp_injection: Option<(&McpLaunchContext, &str)>) -> SpawnSpec {
        let current_env: HashMap<String, String> = std::env::vars().collect();
        let mut env =
            EnvironmentPolicy::baseline().build(&current_env, &self.environment_allowlist);
        let mut args = vec!["app-server".to_string()];
        for override_kv in self.startup_options.config_overrides.iter().flatten() {
            args.push("-c".to_string());
            args.push(override_kv.clone());
        }
        if let Some((context, token)) = mcp_injection {
            args.extend(codex_mcp_overrides(context));
            env.extend(coordination_mcp_env(token));
        }
        SpawnSpec {
            program: PathBuf::from(&self.codex_bin),
            args,
            cwd: self.cwd.clone(),
            env,
            ..SpawnSpec::minimal()
        }
    }

    /// Performs the `initialize` request + `initialized` notification
    /// handshake a fresh `codex app-server` process requires before any
    /// `thread/*`/`turn/*` method is valid.
    async fn handshake(client: &CodexRpcClient) -> Result<(), AdapterError> {
        client
            .call(
                "initialize",
                json!({
                    "clientInfo": {"name": "@nikolasd/crew", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {"experimentalApi": true}
                }),
            )
            .await
            .map_err(|e| client_error(e, "start"))?;
        client
            .notify("initialized", json!({}))
            .map_err(|e| client_error(e, "start"))?;
        Ok(())
    }

    /// Spawns `codex app-server` and completes the `initialize`/
    /// `initialized` handshake -- shared by `start`'s fresh-thread path
    /// and `resume`'s thread-resume path.
    async fn spawn_and_handshake(
        &self,
        mcp_injection: Option<(&McpLaunchContext, &str)>,
    ) -> Result<
        (
            Arc<CodexRpcClient>,
            mpsc::UnboundedReceiver<InboundMessage>,
            i32,
        ),
        AdapterError,
    > {
        let process = self
            .supervisor
            .spawn(self.spawn_spec(mcp_injection))
            .await
            .map_err(|e| AdapterError::process(KIND, "start", e.to_string()))?;
        let pid = process.pid();
        let (client, inbound_rx) = CodexRpcClient::spawn(process);
        let client = Arc::new(client);
        Self::handshake(&client).await?;
        Ok((client, inbound_rx, pid))
    }

    /// Drains `inbound_rx` for the life of the run: normalizes server
    /// notifications into `sink` events and files server-request
    /// approvals into `pending_approvals` (never emitted through `sink`
    /// -- see `normalize::PendingApproval`'s own doc comment). The driver
    /// loop sends a final [`InboundMessage::ProcessExited`] before closing
    /// the channel, which this pump emits through `sink` so the registry's
    /// completion watcher can dispose the adapter and release the slot.
    /// After that, revokes `scope_tokens`'s binding for `run_id` if a
    /// coordination MCP token was bound for this run.
    fn spawn_pump(
        mut inbound_rx: mpsc::UnboundedReceiver<InboundMessage>,
        run_id: batman_protocol::RunId,
        task_id: batman_protocol::TaskId,
        worker_id: batman_protocol::WorkerId,
        sink: Arc<dyn AdapterEventSink>,
        pending_approvals: Arc<std::sync::Mutex<HashMap<String, PendingApproval>>>,
        scope_tokens: Option<Arc<ScopeTokenStore>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            while let Some(message) = inbound_rx.recv().await {
                match message {
                    InboundMessage::Notification { method, params } => {
                        if let Some(payload) = normalize::notification_to_event(&method, &params) {
                            let _ = sink
                                .emit(AdapterEvent {
                                    run_id,
                                    task_id,
                                    worker_id,
                                    payload,
                                })
                                .await;
                        }
                    }
                    InboundMessage::Request { id, method, params } => {
                        if let Some(approval) =
                            normalize::server_request_to_pending_approval(&id, &method, &params)
                        {
                            pending_approvals
                                .lock()
                                .expect("pending approvals mutex never poisoned")
                                .insert(approval.call_id.clone(), approval);
                        }
                    }
                    InboundMessage::ProcessExited { exit_code, signal } => {
                        let _ = sink
                            .emit(AdapterEvent {
                                run_id,
                                task_id,
                                worker_id,
                                payload: AdapterEventPayload::ProcessExited { exit_code, signal },
                            })
                            .await;
                        break;
                    }
                }
            }
            if let Some(scope_tokens) = scope_tokens {
                scope_tokens.revoke_for_run(run_id);
            }
        })
    }
}

fn client_error(err: ClientError, operation: &str) -> AdapterError {
    match err {
        ClientError::RpcError(detail) => {
            AdapterError::protocol(KIND, operation, detail.to_string())
        }
        other => AdapterError::process(KIND, operation, other.to_string()),
    }
}

impl Adapter for CodexAdapter {
    fn kind(&self) -> &str {
        KIND
    }

    fn capabilities(&self) -> AdapterCapabilities {
        declared_capabilities()
    }

    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        let codex_bin = self.codex_bin.clone();
        Box::pin(async move {
            let output = std::process::Command::new(&codex_bin)
                .arg("--version")
                .output()
                .map_err(|e| AdapterError::unavailable(KIND, "probe", e.to_string()))?;
            if !output.status.success() {
                return Err(AdapterError::unavailable(
                    KIND,
                    "probe",
                    "codex --version exited non-zero",
                ));
            }
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

            // No-model-call auth-readiness heuristic: Codex persists its
            // login state under `$CODEX_HOME/auth.json` (defaulting to
            // `~/.codex/auth.json`); its mere presence is the same signal
            // the CLI's own `codex login status` reports without ever
            // invoking a model, checked here directly to avoid spawning a
            // second process.
            let codex_home = std::env::var("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".codex")))
                .unwrap_or_else(|_| PathBuf::from(".codex"));
            let auth_ready = codex_home.join("auth.json").is_file();

            Ok(ProbeResult {
                version: Some(version),
                auth_ready,
                capabilities: declared_capabilities(),
                inventory_incomplete: false,
            })
        })
    }

    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut run_guard = self.run.lock().await;
            if run_guard.is_some() {
                return Err(AdapterError::invalid_vendor_state(
                    KIND,
                    "start",
                    "adapter already has an active run",
                ));
            }

            let mcp_launch = self
                .mcp
                .as_ref()
                .map(|mcp| (mcp.launch_context(spec.run_id), mcp.reserve()));

            let (client, inbound_rx, pid) = self
                .spawn_and_handshake(
                    mcp_launch
                        .as_ref()
                        .map(|(context, token)| (context, token.as_str())),
                )
                .await?;

            if let Some(mcp) = &self.mcp {
                let (context, token) = mcp_launch
                    .as_ref()
                    .expect("mcp_launch is Some whenever self.mcp is Some");
                if let Err(err) = mcp.activate(
                    token.clone(),
                    context.run_id,
                    spec.task_id,
                    spec.worker_id,
                    pid,
                    AdapterMcpConfig::default_expiry(),
                ) {
                    let _ = client.terminate().await;
                    return Err(AdapterError::process(
                        KIND,
                        "start",
                        format!("failed to activate coordination MCP scope token: {err}"),
                    ));
                }
            }

            let thread_id = if let Some(resume) = &spec.resume {
                client
                    .call("thread/resume", json!({"threadId": resume.0.clone()}))
                    .await
                    .map_err(|e| client_error(e, "start"))?;
                resume.0.clone()
            } else {
                let mut thread_params = serde_json::json!({"cwd": self.cwd.to_string_lossy()});
                if let Some(approval_policy) = &self.startup_options.approval_policy {
                    thread_params["approvalPolicy"] = Value::String(approval_policy.clone());
                }
                let response = client
                    .call("thread/start", thread_params)
                    .await
                    .map_err(|e| client_error(e, "start"))?;
                response
                    .get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AdapterError::protocol(
                            KIND,
                            "start",
                            "thread/start response missing thread.id",
                        )
                    })?
                    .to_string()
            };

            let pending_approvals = Arc::new(std::sync::Mutex::new(HashMap::new()));
            let pump = Self::spawn_pump(
                inbound_rx,
                spec.run_id,
                spec.task_id,
                spec.worker_id,
                sink,
                Arc::clone(&pending_approvals),
                self.mcp.as_ref().map(|mcp| Arc::clone(&mcp.scope_tokens)),
            );

            let turn_response = client
                .call(
                    "turn/start",
                    json!({"threadId": thread_id, "input": [{"type": "text", "text": spec.prompt}]}),
                )
                .await
                .map_err(|e| client_error(e, "start"))?;
            let current_turn_id = turn_response
                .get("turn")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);

            *run_guard = Some(RunState {
                run_id: spec.run_id,
                client,
                thread_id,
                current_turn_id,
                pending_approvals: Arc::try_unwrap(pending_approvals)
                    .map(|m| {
                        m.into_inner()
                            .expect("pending approvals mutex never poisoned")
                    })
                    .unwrap_or_default(),
                pump,
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
            let mut run_guard = self.run.lock().await;
            if run_guard.is_some() {
                return Err(AdapterError::invalid_vendor_state(
                    KIND,
                    "resume",
                    "adapter already has an active run",
                ));
            }

            let placeholder_run = batman_protocol::RunId::new();
            let placeholder_task = batman_protocol::TaskId::new();
            let placeholder_worker = batman_protocol::WorkerId::new();

            let mcp_launch = self
                .mcp
                .as_ref()
                .map(|mcp| (mcp.launch_context(placeholder_run), mcp.reserve()));

            let (client, inbound_rx, pid) = self
                .spawn_and_handshake(
                    mcp_launch
                        .as_ref()
                        .map(|(context, token)| (context, token.as_str())),
                )
                .await?;

            if let Some(mcp) = &self.mcp {
                let (context, token) = mcp_launch
                    .as_ref()
                    .expect("mcp_launch is Some whenever self.mcp is Some");
                if let Err(err) = mcp.activate(
                    token.clone(),
                    context.run_id,
                    placeholder_task,
                    placeholder_worker,
                    pid,
                    AdapterMcpConfig::default_expiry(),
                ) {
                    let _ = client.terminate().await;
                    return Err(AdapterError::process(
                        KIND,
                        "resume",
                        format!("failed to activate coordination MCP scope token: {err}"),
                    ));
                }
            }

            client
                .call("thread/resume", json!({"threadId": session.0.clone()}))
                .await
                .map_err(|e| client_error(e, "resume"))?;

            let pending_approvals = Arc::new(std::sync::Mutex::new(HashMap::new()));
            let pump = Self::spawn_pump(
                inbound_rx,
                placeholder_run,
                placeholder_task,
                placeholder_worker,
                sink,
                Arc::clone(&pending_approvals),
                self.mcp.as_ref().map(|mcp| Arc::clone(&mcp.scope_tokens)),
            );

            *run_guard = Some(RunState {
                run_id: placeholder_run,
                client,
                thread_id: session.0,
                current_turn_id: None,
                pending_approvals: Arc::try_unwrap(pending_approvals)
                    .map(|m| {
                        m.into_inner()
                            .expect("pending approvals mutex never poisoned")
                    })
                    .unwrap_or_default(),
                pump,
            });
            Ok(())
        })
    }

    fn send(&self, message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let run_guard = self.run.lock().await;
            let Some(run) = run_guard.as_ref() else {
                return Err(AdapterError::invalid_vendor_state(
                    KIND,
                    "send",
                    "no active run",
                ));
            };
            match message {
                AdapterMessage::Steer { text } => {
                    let Some(turn_id) = run.current_turn_id.clone() else {
                        return Err(AdapterError::invalid_vendor_state(
                            KIND,
                            "send",
                            "no active turn to steer",
                        ));
                    };
                    run.client
                        .call(
                            "turn/steer",
                            json!({
                                "threadId": run.thread_id,
                                "expectedTurnId": turn_id,
                                "input": [{"type": "text", "text": text}]
                            }),
                        )
                        .await
                        .map_err(|e| client_error(e, "send"))?;
                    Ok(())
                }
                AdapterMessage::FollowUp { text } => {
                    run.client
                        .call(
                            "turn/start",
                            json!({"threadId": run.thread_id, "input": [{"type": "text", "text": text}]}),
                        )
                        .await
                        .map_err(|e| client_error(e, "send"))?;
                    Ok(())
                }
                AdapterMessage::Answer { .. } | AdapterMessage::PeerMessage { .. } => {
                    Err(AdapterError::capability_unsupported(KIND, "send"))
                }
            }
        })
    }

    fn respond_to_approval(&self, approval_id: &str, decision: &str) -> AdapterFuture<'_, ()> {
        let approval_id = approval_id.to_string();
        let decision = decision.to_string();
        Box::pin(async move {
            let mut run_guard = self.run.lock().await;
            let Some(run) = run_guard.as_mut() else {
                return Err(AdapterError::invalid_vendor_state(
                    KIND,
                    "respondToApproval",
                    "no active run",
                ));
            };
            let Some(approval) = run.pending_approvals.remove(&approval_id) else {
                return Err(AdapterError::invalid_vendor_state(
                    KIND,
                    "respondToApproval",
                    "no pending approval with that id",
                ));
            };
            let review_decision = normalize::decision_to_review_decision(&decision)
                .map_err(|detail| AdapterError::protocol(KIND, "respondToApproval", detail))?;
            run.client
                .respond(approval.request_id, json!({"decision": review_decision}))
                .map_err(|e| client_error(e, "respondToApproval"))?;
            Ok(())
        })
    }

    fn cancel(&self, scope: CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut run_guard = self.run.lock().await;
            let Some(run) = run_guard.as_mut() else {
                return Err(AdapterError::invalid_vendor_state(
                    KIND,
                    "cancel",
                    "no active run",
                ));
            };
            match scope {
                CancelScope::Turn => {
                    let Some(turn_id) = run.current_turn_id.clone() else {
                        return Ok(());
                    };
                    run.client
                        .call(
                            "turn/interrupt",
                            json!({"threadId": run.thread_id, "turnId": turn_id}),
                        )
                        .await
                        .map_err(|e| client_error(e, "cancel"))?;
                    Ok(())
                }
                CancelScope::Worker | CancelScope::Subtree => {
                    run.client
                        .terminate()
                        .await
                        .map_err(|e| client_error(e, "cancel"))?;
                    // The pump must not be aborted here: it is what reports
                    // the process exit through `InboundMessage::ProcessExited`.
                    // Dropping `run_guard` (set to `None` below) detaches the
                    // `JoinHandle` without cancelling the task, so the pump
                    // finishes on its own and the watcher's `dispose()` can
                    // proceed after the async mutex yields.
                    let run_id = run.run_id;
                    *run_guard = None;
                    if let Some(mcp) = &self.mcp {
                        mcp.scope_tokens.revoke_for_run(run_id);
                    }
                    Ok(())
                }
            }
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async move {
            let run_guard = self.run.lock().await;
            let Some(run) = run_guard.as_ref() else {
                return Ok(AdapterSnapshot::default());
            };
            let artifacts: Vec<serde_json::Value> = run
                .pending_approvals
                .values()
                .map(|approval| {
                    serde_json::json!({
                        "kind": approval.kind,
                        "summary": approval.summary,
                    })
                })
                .collect();
            Ok(AdapterSnapshot {
                state_summary: format!("thread {}", run.thread_id),
                children: Vec::new(),
                usage: None,
                artifacts,
            })
        })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let run = {
                let mut run_guard = self.run.lock().await;
                run_guard.take()
            };
            // Lock is dropped here. A concurrent `dispose()` would see `None`
            // and return immediately, so no double-dispose.
            let Some(run) = run else {
                return Ok(());
            };
            let _ = run.client.terminate().await;
            // Await the pump (not abort): it is what reports the process exit
            // through `InboundMessage::ProcessExited`. By this point the lock
            // is already dropped, so the watcher's `dispose()` can proceed.
            let _ = run.pump.await;
            if let Some(mcp) = &self.mcp {
                mcp.scope_tokens.revoke_for_run(run.run_id);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod pump_exit_tests {
    use std::sync::Mutex;

    use batman_protocol::{RunId, TaskId, WorkerId};
    use batman_runtime::adapter::{
        AdapterEvent, AdapterEventPayload, AdapterEventSink, AdapterFuture,
    };
    use batman_runtime::supervisor::{EscalationTimings, SpawnSpec, Supervisor};

    use super::*;

    /// A sink that records payloads for test assertions.
    struct RecordingSink(Mutex<Vec<AdapterEventPayload>>);

    impl RecordingSink {
        fn new() -> Arc<RecordingSink> {
            Arc::new(Self(Mutex::new(Vec::new())))
        }

        fn payloads(&self) -> Vec<AdapterEventPayload> {
            self.0.lock().expect("mutex is never poisoned").clone()
        }
    }

    impl AdapterEventSink for RecordingSink {
        fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
            let mut payloads = self.0.lock().expect("mutex is never poisoned");
            payloads.push(event.payload);
            Box::pin(async { Ok(0) })
        }
    }

    fn fast_escalation() -> EscalationTimings {
        EscalationTimings {
            sigint_to_sigterm: std::time::Duration::from_millis(50),
            sigterm_to_sigkill: std::time::Duration::from_millis(50),
        }
    }

    #[tokio::test]
    async fn a_driver_loop_exit_reaches_the_pump_as_process_exited() {
        let supervisor = Supervisor::with_escalation(fast_escalation());
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 3".into()],
            cwd: PathBuf::from("/tmp"),
            env: std::collections::HashMap::new(),
            max_stdout_frame_bytes: 8192,
            max_stderr_capture_bytes: 4096,
        };
        let process = supervisor.spawn(spec).await.expect("spawn /bin/sh");
        let (client, inbound_rx) = CodexRpcClient::spawn(process);
        // Keep `client` bound so the exit is observed via stdout close,
        // not `outbound_rx` closure.
        let _client = client;

        let sink = RecordingSink::new();
        let pump = CodexAdapter::spawn_pump(
            inbound_rx,
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            sink.clone() as Arc<dyn AdapterEventSink>,
            Arc::new(Mutex::new(HashMap::new())),
            None,
        );
        pump.await.expect("pump completed");

        let payloads = sink.payloads();
        let exited: Vec<_> = payloads
            .iter()
            .filter(|p| matches!(p, AdapterEventPayload::ProcessExited { .. }))
            .collect();
        assert_eq!(
            exited.len(),
            1,
            "expected exactly one ProcessExited, got {exited:?}"
        );
        assert!(
            matches!(
                &exited[0],
                AdapterEventPayload::ProcessExited {
                    exit_code: Some(3),
                    signal: None
                }
            ),
            "expected ProcessExited {{ exit_code: Some(3), signal: None }}, got {:?}",
            exited[0]
        );
    }
}
