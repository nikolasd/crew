//! The Copilot ACP adapter: launches `copilot --acp` over NDJSON stdio,
//! negotiates the ACP protocol version, and normalizes ACP session
//! updates into [`AdapterEvent`]s. Never opens a TCP path (see
//! `client.rs`'s module doc).
//!
//! `start`'s prompt turn (`session/prompt`) is a genuine model-invoking
//! call once actually run against a real Copilot account; no test in
//! this crate ever calls [`Adapter::start`]/[`Adapter::send`] on a real
//! `CopilotAdapter` for exactly that reason. Every test instead exercises
//! the deterministic pieces directly: `initialize`/`session/list` against
//! the real binary (a structured-protocol handshake, never a model
//! call), and fixture-driven negotiation/normalization/no-TCP checks.

pub mod client;
pub mod compatibility;
pub mod conformance;
pub mod normalize;

pub use client::{
    CopilotAcpClient, CopilotClientEvent, CopilotNegotiatedCapabilities, CopilotPermissionOption,
    CopilotPermissionRequest, parse_initialize_response as copilot_parse_initialize_response,
};
pub use compatibility::{
    COPILOT_KNOWN_CLI_VERSIONS, COPILOT_MAX_ACP_PROTOCOL_VERSION, COPILOT_MIN_ACP_PROTOCOL_VERSION,
    copilot_acp_protocol_version_supported, copilot_cli_version_known,
    copilot_negotiated_version_verified,
};
pub use normalize::StopOutcome;
pub use normalize::copilot_normalize_session_update;
pub use normalize::copilot_normalize_stop_reason;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

use batman_runtime::adapter::mcp_config::{
    AdapterMcpConfig, coordination_mcp_config_document, coordination_mcp_env,
};
use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterError, AdapterEvent, AdapterEventPayload,
    AdapterEventSink, AdapterFuture, AdapterMessage, AdapterSnapshot, ApprovalsCapability,
    CancelScope, CopilotStartupOptions, DurabilityCapability, NativeViewCapability,
    NestedCapability, ProbeResult, ProtocolKind, ResumeCapability, StartSpec, SteeringCapability,
    UsageCapability, VendorSessionRef, WorkspaceControlCapability,
};
use batman_runtime::supervisor::EnvironmentPolicy;

/// This adapter's fixed declared capabilities. `resume` narrows to
/// [`ResumeCapability::None`] at runtime if a live `initialize` probe
/// ever observes `agentCapabilities.loadSession: false` (see
/// [`CopilotAdapter::capabilities_for`]) -- untested here since the only
/// installed/verified CLI (1.0.73) always advertises `loadSession: true`.
const DECLARED_CAPABILITIES: AdapterCapabilities = AdapterCapabilities {
    protocol: ProtocolKind::Structured,
    resume: ResumeCapability::Session,
    steering: SteeringCapability::None,
    approvals: ApprovalsCapability::Controllable,
    structured_result: false,
    usage: UsageCapability::None,
    nested: NestedCapability::None,
    native_view: NativeViewCapability::None,
    workspace_control: WorkspaceControlCapability::Write,
    durability: DurabilityCapability::VendorResumable,
};

#[derive(Default)]
struct AdapterState {
    client: Option<Arc<CopilotAcpClient>>,
    vendor_session_id: Option<String>,
    event_drain: Option<JoinHandle<()>>,
    sink: Option<Arc<dyn AdapterEventSink>>,
}

/// The `copilot` worker adapter: one supervised `copilot --acp` process
/// per [`CopilotAdapter`] instance, reached only over NDJSON stdio.
///
/// `run_id`/`task_id`/`worker_id` are fixed at construction (mirroring
/// how a [`WorkerProfileRef`](crate::adapter::WorkerProfile)-backed
/// worker is 1:1 with its adapter instance for its whole lifetime,
/// including across a runtime restart that calls [`Adapter::resume`]
/// instead of [`Adapter::start`]): [`Adapter::resume`]/[`Adapter::send`]/
/// [`Adapter::cancel`] have no [`StartSpec`] of their own to draw these
/// correlation ids from, so this adapter carries them itself rather than
/// only ever learning them from a prior `start()` call in the same
/// process.
pub struct CopilotAdapter {
    program: PathBuf,
    cwd: PathBuf,
    startup_options: CopilotStartupOptions,
    environment_allowlist: Vec<String>,
    run_id: batman_protocol::RunId,
    task_id: batman_protocol::TaskId,
    worker_id: batman_protocol::WorkerId,
    state: TokioMutex<AdapterState>,
    /// Worker-MCP coordination tool injection for this adapter's
    /// supervised `copilot` process, or `None` for a caller that never
    /// asked for worker MCP tools (see `crate::adapter::mcp_config`'s
    /// module doc).
    mcp: Option<AdapterMcpConfig>,
}

/// The argv/env/reserved-token plan [`CopilotAdapter::spawn_plan`]
/// computes for a single `ensure_client` spawn. `#[doc(hidden)]`: an
/// internal testing seam (mirroring `CopilotAcpClient::spawn_with_raw_args`'s
/// test-only visibility), not part of this adapter's public contract.
#[doc(hidden)]
#[derive(Debug)]
pub struct CopilotSpawnPlan {
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub reserved_token: Option<String>,
}

impl CopilotAdapter {
    /// Constructs a `copilot` adapter that will launch `program` (e.g.
    /// `PathBuf::from("copilot")`, resolved via `PATH` in `env`) with
    /// `cwd` as its working directory, correlating every event it emits
    /// (across `start`/`resume`/`send`/`cancel`) to `run_id`/`task_id`/
    /// `worker_id`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        program: PathBuf,
        cwd: PathBuf,
        startup_options: CopilotStartupOptions,
        environment_allowlist: Vec<String>,
        run_id: batman_protocol::RunId,
        task_id: batman_protocol::TaskId,
        worker_id: batman_protocol::WorkerId,
        mcp: Option<AdapterMcpConfig>,
    ) -> Self {
        Self {
            program,
            cwd,
            startup_options,
            environment_allowlist,
            run_id,
            task_id,
            worker_id,
            state: TokioMutex::new(AdapterState::default()),
            mcp,
        }
    }

    /// The `--allow-tool=`/`--deny-tool=`/`--log-level` arguments implied
    /// by this adapter's immutable [`CopilotStartupOptions`]. Fixed for
    /// the process's entire lifetime -- there is no ACP method to change
    /// allowed tools mid-session, so this adapter never attempts one.
    fn startup_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for tool in self.startup_options.allow_tool.iter().flatten() {
            args.push(format!("--allow-tool={tool}"));
        }
        for tool in self.startup_options.deny_tool.iter().flatten() {
            args.push(format!("--deny-tool={tool}"));
        }
        if let Some(level) = &self.startup_options.log_level {
            args.push("--log-level".to_string());
            args.push(level.clone());
        }
        args
    }

    fn build_env(&self) -> std::collections::HashMap<String, String> {
        let current: std::collections::HashMap<String, String> = std::env::vars().collect();
        EnvironmentPolicy::baseline().build(&current, &self.environment_allowlist)
    }

    /// The argv/env/reserved-token plan this adapter's single spawn path
    /// (`ensure_client`) builds for [`CopilotAcpClient::spawn`], factored
    /// out pure -- never spawns anything -- so this crate's own test
    /// suite can assert on the injected `--additional-mcp-config`/
    /// `CREW_WORKER_SCOPE_TOKEN` shape without spawning a real vendor
    /// process. `reserved_token` is `Some` only when this adapter was
    /// constructed with `mcp: Some(_)`; the caller must
    /// [`AdapterMcpConfig::activate`] it once the real vendor pid is
    /// known.
    #[doc(hidden)]
    #[must_use]
    pub fn spawn_plan(&self) -> CopilotSpawnPlan {
        let mut args = self.startup_args();
        let mut env = self.build_env();
        let reserved_token = self.mcp.as_ref().map(|mcp| {
            let context = mcp.launch_context(self.run_id);
            let token = mcp.reserve();
            args.push("--additional-mcp-config".to_string());
            args.push(
                serde_json::to_string(&coordination_mcp_config_document(&context))
                    .expect("serializing a JSON value to a string never fails"),
            );
            env.extend(coordination_mcp_env(&token));
            token
        });
        CopilotSpawnPlan {
            args,
            env,
            reserved_token,
        }
    }

    /// Spawns and initializes a fresh [`CopilotAcpClient`] if this
    /// instance does not already own a live one. Refuses to proceed past
    /// `initialize` for a CLI version this adapter has not been
    /// empirically verified against (see `compatibility.rs`) — including
    /// a response that omits `agentInfo.version` entirely, which is
    /// treated as unverified rather than implicitly trusted (R57). There
    /// is currently no `CopilotStartupOptions` field to opt into an
    /// unverified version, so this refusal is unconditional. When
    /// `self.mcp` is `Some`, also injects and activates the coordination
    /// MCP scope token via [`Self::spawn_plan`] before returning.
    async fn ensure_client(&self) -> Result<Arc<CopilotAcpClient>, AdapterError> {
        let mut state = self.state.lock().await;
        if let Some(client) = &state.client {
            return Ok(client.clone());
        }
        let plan = self.spawn_plan();
        let client = CopilotAcpClient::spawn(&self.program, &self.cwd, plan.args, plan.env)
            .await
            .map_err(|source| {
                AdapterError::unavailable("copilot", "ensureClient", source.detail().to_string())
            })?;
        if let Some(token) = plan.reserved_token {
            let mcp = self
                .mcp
                .as_ref()
                .expect("reserved_token is Some only when self.mcp is Some");
            if let Err(err) = mcp.activate(
                token,
                self.run_id,
                self.task_id,
                self.worker_id,
                client.pid(),
                AdapterMcpConfig::default_expiry(),
            ) {
                client.shutdown().await;
                return Err(AdapterError::process(
                    "copilot",
                    "ensureClient",
                    format!("failed to activate coordination MCP scope token: {err}"),
                ));
            }
        }
        let negotiated = client.initialize().await?;
        // A missing `agentInfo.version` is unknown, not implicitly
        // verified: refuse it exactly like a known-bad version (R57).
        // `probe()` reports the same condition as `inventory_incomplete`.
        if !compatibility::copilot_negotiated_version_verified(negotiated.agent_version.as_deref())
        {
            client.shutdown().await;
            let installed = negotiated
                .agent_version
                .as_deref()
                .map_or_else(|| "an unreported version".to_string(), str::to_string);
            return Err(AdapterError::incompatible_version(
                "copilot",
                "initialize",
                format!(
                    "installed Copilot CLI {installed} has not been verified by this adapter \
                         (known versions: {:?}); a response omitting agentInfo.version is \
                         treated as unverified; no CopilotStartupOptions field currently opts \
                         into an unverified version",
                    COPILOT_KNOWN_CLI_VERSIONS
                        .iter()
                        .map(|entry| entry.cli_version)
                        .collect::<Vec<_>>()
                ),
            ));
        }
        let client = Arc::new(client);
        state.client = Some(client.clone());
        Ok(client)
    }

    /// Spawns the background task that drains [`CopilotAcpClient::next_event`]
    /// for this adapter's whole `client`/session lifetime, emitting every
    /// normalized `session/update` payload through `sink` correlated to
    /// this adapter's own fixed `run_id`/`task_id`/`worker_id`. Shared by
    /// [`Adapter::start`] and [`Adapter::resume`] so a resumed session
    /// streams events exactly like a freshly started one.
    fn spawn_event_drain(
        &self,
        client: Arc<CopilotAcpClient>,
        sink: Arc<dyn AdapterEventSink>,
    ) -> JoinHandle<()> {
        let run_id = self.run_id;
        let task_id = self.task_id;
        let worker_id = self.worker_id;
        let mcp = self.mcp.clone();
        tokio::spawn(async move {
            while let Some(event) = client.next_event().await {
                match event {
                    CopilotClientEvent::SessionUpdate { payloads, .. } => {
                        for payload in payloads {
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
                    CopilotClientEvent::PermissionRequested { .. } => {
                        // Recorded inside `CopilotAcpClient`'s own
                        // pending-permissions table; surfaced via
                        // `Adapter::snapshot` and resolved via
                        // `Adapter::respond_to_approval`. Full
                        // `ApprovalService` wiring is a follow-up
                        // integration point (see the shared adapter
                        // contract's approvals section).
                    }
                    CopilotClientEvent::ProcessExited { exit_code, signal } => {
                        let _ = sink
                            .emit(AdapterEvent {
                                run_id,
                                task_id,
                                worker_id,
                                payload: AdapterEventPayload::ProcessExited { exit_code, signal },
                            })
                            .await;
                        if let Some(mcp) = &mcp {
                            mcp.scope_tokens.revoke_for_run(run_id);
                        }
                        break;
                    }
                }
            }
        })
    }

    /// Emits the events an ACP `stopReason` implies and converts a
    /// non-success reason into an `AdapterError`.
    async fn settle_turn(
        &self,
        stop_reason: &str,
        sink: &Arc<dyn AdapterEventSink>,
        run_id: batman_protocol::RunId,
        task_id: batman_protocol::TaskId,
        worker_id: batman_protocol::WorkerId,
    ) -> Result<(), AdapterError> {
        let outcome =
            crate::adapter::copilot::normalize::copilot_normalize_stop_reason(stop_reason);
        for payload in outcome.events {
            sink.emit(AdapterEvent {
                run_id,
                task_id,
                worker_id,
                payload,
            })
            .await?;
        }
        if let Some(detail) = outcome.failure {
            return Err(AdapterError::protocol("copilot", "session/prompt", &detail));
        }
        Ok(())
    }
}

impl Adapter for CopilotAdapter {
    fn kind(&self) -> &str {
        "copilot"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        DECLARED_CAPABILITIES
    }

    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        Box::pin(async move {
            let client = CopilotAcpClient::spawn(
                &self.program,
                &self.cwd,
                self.startup_args(),
                self.build_env(),
            )
            .await
            .map_err(|source| {
                AdapterError::unavailable("copilot", "probe", source.detail().to_string())
            })?;
            let negotiated = client.initialize().await;
            let negotiated = match negotiated {
                Ok(negotiated) => negotiated,
                Err(error) => {
                    client.shutdown().await;
                    return Err(error);
                }
            };
            let known = compatibility::copilot_negotiated_version_verified(
                negotiated.agent_version.as_deref(),
            );
            // A real, no-model-call structured probe: `session/list`
            // requires the CLI to be authenticated (it lists Copilot's
            // own persisted session history), so its success/failure is
            // this adapter's `auth_ready` signal without ever invoking a
            // model.
            let auth_ready = client.session_list().await.is_ok();
            client.shutdown().await;

            let mut capabilities = DECLARED_CAPABILITIES;
            if !negotiated.load_session {
                capabilities.resume = ResumeCapability::None;
            }

            Ok(ProbeResult {
                version: negotiated.agent_version,
                auth_ready,
                capabilities,
                inventory_incomplete: !known,
            })
        })
    }

    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let client = self.ensure_client().await?;
            let cwd = self.cwd.to_string_lossy().to_string();
            let session_id = if let Some(resume) = &spec.resume {
                client.session_load(&resume.0, &cwd).await?;
                resume.0.clone()
            } else {
                client.session_new(&cwd).await?
            };

            {
                let mut state = self.state.lock().await;
                state.vendor_session_id = Some(session_id.clone());
            }

            sink.emit(AdapterEvent {
                run_id: spec.run_id,
                task_id: spec.task_id,
                worker_id: spec.worker_id,
                payload: AdapterEventPayload::ProcessStarted {
                    pid: client.pid() as u32,
                },
            })
            .await?;
            sink.emit(AdapterEvent {
                run_id: spec.run_id,
                task_id: spec.task_id,
                worker_id: spec.worker_id,
                payload: AdapterEventPayload::VendorSessionEstablished {
                    vendor_session_id: session_id.clone(),
                },
            })
            .await?;

            let event_drain = self.spawn_event_drain(client.clone(), sink.clone());

            {
                let mut state = self.state.lock().await;
                state.event_drain = Some(event_drain);
                state.sink = Some(sink.clone());
            }

            // Runs the initial turn to completion. Real model-invoking
            let stop = client.session_prompt(&session_id, &spec.prompt).await?;
            self.settle_turn(&stop, &sink, spec.run_id, spec.task_id, spec.worker_id)
                .await?;
            Ok(())
        })
    }

    fn resume(
        &self,
        session: VendorSessionRef,
        sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let client = self.ensure_client().await?;
            let cwd = self.cwd.to_string_lossy().to_string();
            client.session_load(&session.0, &cwd).await?;
            {
                let mut state = self.state.lock().await;
                state.vendor_session_id = Some(session.0.clone());
            }
            sink.emit(AdapterEvent {
                run_id: self.run_id,
                task_id: self.task_id,
                worker_id: self.worker_id,
                payload: AdapterEventPayload::VendorSessionEstablished {
                    vendor_session_id: session.0,
                },
            })
            .await?;

            let event_drain = self.spawn_event_drain(client, sink.clone());
            {
                let mut state = self.state.lock().await;
                state.event_drain = Some(event_drain);
                state.sink = Some(sink);
            }
            Ok(())
        })
    }

    fn send(&self, message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            match message {
                AdapterMessage::FollowUp { text } => {
                    let (client, session_id, sink) = {
                        let state = self.state.lock().await;
                        (
                            state.client.clone(),
                            state.vendor_session_id.clone(),
                            state.sink.clone(),
                        )
                    };
                    let Some(client) = client else {
                        return Err(AdapterError::invalid_vendor_state(
                            "copilot",
                            "send",
                            "no active vendor session to follow up on",
                        ));
                    };
                    let Some(session_id) = session_id else {
                        return Err(AdapterError::invalid_vendor_state(
                            "copilot",
                            "send",
                            "no active vendor session to follow up on",
                        ));
                    };
                    let stop = client.session_prompt(&session_id, &text).await?;
                    if let Some(sink) = sink {
                        self.settle_turn(&stop, &sink, self.run_id, self.task_id, self.worker_id)
                            .await?;
                    } else {
                        // No sink available (shouldn't happen in normal flow),
                        // but still fail for non-success stop reasons.
                        let outcome =
                            crate::adapter::copilot::normalize::copilot_normalize_stop_reason(
                                &stop,
                            );
                        if let Some(detail) = outcome.failure {
                            return Err(AdapterError::protocol(
                                "copilot",
                                "session/prompt",
                                &detail,
                            ));
                        }
                    }
                    Ok(())
                }
                // ACP v1 has no mid-turn steering distinct from a
                // follow-up prompt after the current turn ends, and no
                // free-form "answer" or peer-message concept (peer
                // messages are an OMP-native, `nested: managed`-only
                // concept this adapter never declares).
                AdapterMessage::Steer { .. } => Err(AdapterError::capability_unsupported(
                    "copilot",
                    "send.steer",
                )),
                AdapterMessage::Answer { .. } => Err(AdapterError::capability_unsupported(
                    "copilot",
                    "send.answer",
                )),
                AdapterMessage::PeerMessage { .. } => Err(AdapterError::capability_unsupported(
                    "copilot",
                    "send.peerMessage",
                )),
            }
        })
    }

    fn respond_to_approval(&self, approval_id: &str, decision: &str) -> AdapterFuture<'_, ()> {
        let approval_id = approval_id.to_string();
        let decision = decision.to_string();
        Box::pin(async move {
            let request_id: i64 = approval_id.parse().map_err(|_| {
                AdapterError::invalid_vendor_state(
                    "copilot",
                    "respondToApproval",
                    "approval id is not a valid ACP request id",
                )
            })?;
            let client = {
                let state = self.state.lock().await;
                state.client.clone()
            };
            let Some(client) = client else {
                return Err(AdapterError::invalid_vendor_state(
                    "copilot",
                    "respondToApproval",
                    "no active vendor session",
                ));
            };
            client.respond_permission(request_id, &decision)
        })
    }

    fn cancel(&self, _scope: CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let (client, session_id) = {
                let state = self.state.lock().await;
                (state.client.clone(), state.vendor_session_id.clone())
            };
            let Some(client) = client else {
                return Err(AdapterError::invalid_vendor_state(
                    "copilot",
                    "cancel",
                    "no active vendor session",
                ));
            };
            let Some(session_id) = session_id else {
                return Err(AdapterError::invalid_vendor_state(
                    "copilot",
                    "cancel",
                    "no active vendor session",
                ));
            };
            client.session_cancel(&session_id)
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async move {
            let state = self.state.lock().await;
            let Some(client) = &state.client else {
                return Ok(AdapterSnapshot {
                    state_summary: "not started".to_string(),
                    ..AdapterSnapshot::default()
                });
            };
            let pending = client.pending_permission_ids();
            let state_summary = match &state.vendor_session_id {
                Some(session_id) if pending.is_empty() => {
                    format!("session {session_id} active")
                }
                Some(session_id) => {
                    format!("session {session_id} active, pending permissions: {pending:?}")
                }
                None => "initialized, no session yet".to_string(),
            };
            Ok(AdapterSnapshot {
                state_summary,
                ..AdapterSnapshot::default()
            })
        })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            if let Some(event_drain) = state.event_drain.take() {
                event_drain.abort();
            }
            if let Some(client) = state.client.take() {
                client.shutdown().await;
            }
            state.vendor_session_id = None;
            if let Some(mcp) = &self.mcp {
                mcp.scope_tokens.revoke_for_run(self.run_id);
            }
            Ok(())
        })
    }
}
