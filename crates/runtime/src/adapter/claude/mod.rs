//! `ClaudeAdapter`: a thin protocol adapter over the installed `claude`
//! CLI's `stream-json` mode. Not a Claude Code re-implementation --
//! [`command`] builds the argv/stdin frames, [`protocol`] types the raw
//! wire shapes, and [`normalize`] turns them into [`AdapterEventPayload`]s
//! (see that module's doc for the thinking-block/approval-lifecycle
//! discipline).
//!
//! # Concurrency
//! Once [`ClaudeAdapter::start`]/[`ClaudeAdapter::resume`] spawns the
//! vendor process, a single background task owns the
//! [`ManagedProcess`] exclusively (its `write_stdin`/`next_stdout_frame`
//! both require `&mut self`, so no other caller may touch it directly).
//! [`ClaudeAdapter::send`]/[`ClaudeAdapter::cancel`]/[`ClaudeAdapter::dispose`]
//! talk to that task through an internal [`SessionCommand`] channel
//! instead. [`ClaudeAdapter::snapshot`] reads a small `Arc<Mutex<..>>` of
//! session facts (vendor session id, pending approvals, last usage) the
//! background task updates as it normalizes frames.
//!
//! # What is/isn't exercised by the default test run
//! `probe()` is exercised for real against the installed CLI (`claude
//! --version`, `claude auth status`) -- never a model call. `start()`/
//! `resume()`/`send()` are real, complete implementations, but actually
//! running one would write a real prompt to a real `claude -p` process's
//! stdin, which *would* invoke the model the moment the CLI reads it --
//! so the default `claude_adapter.rs` suite never calls them past their
//! own pre-start guard clauses. The optional, `#[ignore]`d
//! `claude_live.rs` end-to-end test is what actually exercises the
//! spawn+stdin+reader-task path; it is skipped unless explicitly run
//! with `--ignored` and `CREW_DISABLE_VENDOR_CLI` is unset.

pub mod command;
pub mod conformance;
pub mod normalize;
pub mod protocol;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use batman_protocol::{Classified, ContentClass, RunId, TaskId, WorkerId};
use batman_runtime::adapter::mcp_config::{
    AdapterMcpConfig, coordination_mcp_config_document, coordination_mcp_env,
};
use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterError, AdapterEvent, AdapterEventPayload,
    AdapterEventSink, AdapterFuture, AdapterMessage, AdapterSnapshot, ApprovalsCapability,
    CancelScope, ClaudeStartupOptions, DurabilityCapability, NativeViewCapability,
    NestedCapability, ProbeResult, ProtocolKind, ResumeCapability, StartSpec, SteeringCapability,
    UsageCapability, VendorSessionRef, WorkspaceControlCapability,
};
use batman_runtime::coordination::ScopeTokenStore;
use batman_runtime::supervisor::{EnvironmentPolicy, ManagedProcess, SpawnSpec, Supervisor};
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot};
use uuid::Uuid;

use normalize::{ClaudeEvent, ClaudeNormalizer};

/// A pending vendor-observed `PermissionRequest` hook, tracked only for
/// `snapshot()` to report -- see the crate-level doc.
#[derive(Debug, Clone)]
struct PendingApproval {
    hook_name: String,
}

/// Facts about the running (or most recently run) session, updated by the
/// background reader task and read synchronously by `snapshot()`.
#[derive(Debug, Default)]
struct SharedSessionInfo {
    vendor_session_id: Option<String>,
    pending_approvals: HashMap<String, PendingApproval>,
    last_usage: Option<serde_json::Value>,
    /// Path of the `--mcp-config` temp file written for the current
    /// session, if worker MCP tools were injected. Taken (set to
    /// `None`) by whichever of `run_session`'s post-loop cleanup or
    /// `dispose()` runs first, so the file is deleted exactly once.
    mcp_config_path: Option<PathBuf>,
}

/// A message the background session task acts on.
enum SessionCommand {
    /// The reply carries the real outcome of the write -- never
    /// discarded, so a broken pipe is never reported as a false success.
    WriteStdin(Vec<u8>, oneshot::Sender<std::io::Result<()>>),
    Terminate(oneshot::Sender<()>),
}

/// State guarded by `ClaudeAdapter::state`: the channel to the
/// background session task, if one is running, plus the shared facts it
/// updates.
#[derive(Default)]
struct ClaudeSessionState {
    commands: Option<mpsc::Sender<SessionCommand>>,
    shared: Arc<StdMutex<SharedSessionInfo>>,
}

/// A thin protocol adapter over the installed `claude` CLI's
/// `stream-json` mode. See the module doc for the concurrency model.
pub struct ClaudeAdapter {
    startup_options: ClaudeStartupOptions,
    cwd: PathBuf,
    environment_allowlist: Vec<String>,
    /// Bound to this adapter instance at construction (not read from
    /// `StartSpec`), so `resume()` -- which carries no `StartSpec` at
    /// all -- has a correlation to stamp on its `AdapterEvent`s even
    /// from a *fresh* instance (e.g. after a genuine runtime restart),
    /// not only when resuming on the same instance that previously
    /// called `start()`.
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    supervisor: Supervisor,
    state: TokioMutex<ClaudeSessionState>,
    /// `None` for a caller that never asked for worker MCP tools
    /// (every pre-existing test/call site); `Some` injects the
    /// coordination MCP server into every spawned session.
    mcp: Option<AdapterMcpConfig>,
}

impl ClaudeAdapter {
    /// `cwd` is the workspace directory the supervised `claude` process
    /// runs in; `environment_allowlist` names extra environment variables
    /// (beyond `EnvironmentPolicy::baseline()`) the process may inherit.
    /// Workspace assignment itself is a later milestone's concern (see
    /// the shared adapter context) -- this adapter is handed an already-
    /// resolved `cwd` rather than deriving one from a `WorkerProfile`.
    /// `run_id`/`task_id`/`worker_id` identify the one run this adapter
    /// instance is scoped to; `start()` uses the (matching) ids on its
    /// own `StartSpec` instead, but `resume()` has no `StartSpec` to
    /// read them from, so they are bound here unconditionally. `mcp` is
    /// `None` for a caller that never asked for worker MCP tools.
    #[must_use]
    pub fn new(
        startup_options: ClaudeStartupOptions,
        cwd: PathBuf,
        environment_allowlist: Vec<String>,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        mcp: Option<AdapterMcpConfig>,
    ) -> Self {
        Self {
            startup_options,
            cwd,
            environment_allowlist,
            run_id,
            task_id,
            worker_id,
            supervisor: Supervisor::new(),
            state: TokioMutex::new(ClaudeSessionState::default()),
            mcp,
        }
    }

    fn spawn_env(&self) -> HashMap<String, String> {
        EnvironmentPolicy::baseline()
            .build(&std::env::vars().collect(), &self.environment_allowlist)
    }

    /// Runs a short-lived, no-model-call probe subcommand (`--version` or
    /// `auth status`) to completion and returns its stdout as text.
    async fn run_probe_command(&self, args: &[&str]) -> Result<String, AdapterError> {
        let spawn_spec = SpawnSpec {
            program: PathBuf::from("claude"),
            args: args.iter().map(ToString::to_string).collect(),
            env: self.spawn_env(),
            ..SpawnSpec::minimal()
        };
        let mut process = self
            .supervisor
            .spawn(spawn_spec)
            .await
            .map_err(|err| AdapterError::process(self.kind(), "probe", err.to_string()))?;

        let mut output = Vec::new();
        while let Some(frame) = process.next_stdout_frame().await {
            output.extend_from_slice(&frame);
            output.push(b'\n');
        }
        process.wait().await.ok();

        String::from_utf8(output)
            .map_err(|err| AdapterError::protocol(self.kind(), "probe", err.to_string()))
    }

    /// Starts (new session) or resumes (existing vendor session) the
    /// supervised `claude` process and hands it off to a background
    /// reader/writer task. Shared by `start`/`resume`.
    async fn spawn_session(
        &self,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        spec_resume: Option<VendorSessionRef>,
        initial_stdin: Option<Vec<u8>>,
        sink: Arc<dyn AdapterEventSink>,
    ) -> Result<(), AdapterError> {
        let mut state = self.state.lock().await;
        if state.commands.is_some() {
            return Err(AdapterError::invalid_vendor_state(
                self.kind(),
                "start",
                "a vendor process is already running for this adapter instance",
            ));
        }

        let start_spec = StartSpec {
            run_id,
            task_id,
            worker_id,
            prompt: String::new(),
            resume: spec_resume,
        };
        let session_id = Uuid::now_v7();
        let mut args = command::build_args(&self.startup_options, &start_spec, &session_id);
        let mut env = self.spawn_env();

        // Worker MCP tool injection, additive alongside every native
        // discovery flag `command::build_args` already produced above --
        // see `build_mcp_injection`'s doc. Left completely untouched (no
        // env addition, no extra arg) when `self.mcp` is `None`.
        let mcp_injection = match &self.mcp {
            Some(mcp) => {
                let injection = build_mcp_injection(mcp, run_id)
                    .map_err(|err| AdapterError::process(self.kind(), "start", err.to_string()))?;
                env.extend(injection.extra_env.clone());
                args.extend(injection.extra_args.clone());
                Some(injection)
            }
            None => None,
        };

        let spawn_spec = SpawnSpec {
            program: PathBuf::from("claude"),
            args,
            cwd: self.cwd.clone(),
            env,
            ..SpawnSpec::minimal()
        };
        let mut process = match self.supervisor.spawn(spawn_spec).await {
            Ok(process) => process,
            Err(err) => {
                if let Some(injection) = &mcp_injection {
                    let _ = std::fs::remove_file(&injection.config_path);
                }
                return Err(AdapterError::process(self.kind(), "start", err.to_string()));
            }
        };

        let pid = process.pid();

        // The token cannot be verified until it is bound to the vendor's
        // real pid, known only now. On failure the vendor process must
        // never be left running with a scope token that never went
        // live -- terminate it and report an error rather than proceed.
        if let (Some(mcp), Some(injection)) = (&self.mcp, &mcp_injection)
            && let Err(err) = mcp.activate(
                injection.token.clone(),
                run_id,
                task_id,
                worker_id,
                pid,
                AdapterMcpConfig::default_expiry(),
            )
        {
            process.terminate().await;
            let _ = std::fs::remove_file(&injection.config_path);
            return Err(AdapterError::process(
                self.kind(),
                "start",
                format!("failed to activate worker MCP scope token: {err}"),
            ));
        }

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessStarted { pid: pid as u32 },
        })
        .await?;

        if let Some(bytes) = initial_stdin {
            process
                .write_stdin(&bytes)
                .await
                .map_err(|err| AdapterError::process(self.kind(), "start", err.to_string()))?;
        }

        let shared = Arc::new(StdMutex::new(SharedSessionInfo {
            mcp_config_path: mcp_injection
                .as_ref()
                .map(|injection| injection.config_path.clone()),
            ..SharedSessionInfo::default()
        }));
        let (commands_tx, commands_rx) = mpsc::channel(16);
        let kind = self.kind().to_string();
        let task_shared = shared.clone();
        let scope_tokens = self.mcp.as_ref().map(|mcp| mcp.scope_tokens.clone());
        tokio::spawn(run_session(
            process,
            commands_rx,
            ClaudeNormalizer::new(),
            sink,
            (run_id, task_id, worker_id),
            task_shared,
            kind,
            scope_tokens,
        ));

        state.commands = Some(commands_tx);
        state.shared = shared;
        Ok(())
    }
}

/// Everything one spawn needs to inject worker MCP tools into the
/// vendor process's argv/env, produced by [`build_mcp_injection`]: the
/// reserved (not yet activated) scope token, the argv suffix appended
/// after `command::build_args`'s output, the environment addition
/// merged into `spawn_env()`'s output, and the path of the
/// `--mcp-config` file just written -- tracked so the caller can clean
/// it up on any later failure, and so `run_session`/`dispose` can
/// delete it once the session ends.
pub struct McpInjection {
    pub token: String,
    pub config_path: PathBuf,
    pub extra_args: Vec<String>,
    pub extra_env: HashMap<String, String>,
}

/// Reserves a worker-MCP scope token and writes the Claude `--mcp-config`
/// file (owner-only `0600` permissions; the file names only the
/// `coordination-mcp` command/args, never the token itself -- see
/// `mcp_config`'s module doc for why the token only ever belongs in the
/// vendor process's own environment). Factored out of `spawn_session` so
/// the exact argv/env/file shape is unit-testable without spawning a
/// real `claude` process.
pub fn build_mcp_injection(mcp: &AdapterMcpConfig, run_id: RunId) -> std::io::Result<McpInjection> {
    let context = mcp.launch_context(run_id);
    let token = mcp.reserve();
    let extra_env = coordination_mcp_env(&token);
    let config_path = std::env::temp_dir().join(format!("crew-mcp-{run_id}.json"));
    let document = coordination_mcp_config_document(&context);
    write_mcp_config_file(&config_path, &document)?;
    Ok(McpInjection {
        token,
        extra_args: vec![
            "--mcp-config".to_string(),
            config_path.display().to_string(),
        ],
        extra_env,
        config_path,
    })
}

/// Writes `document` as pretty JSON to `path`, creating it (or
/// truncating an existing one) with owner-only (`0600`) permissions from
/// the moment it is created -- never briefly world-readable.
fn write_mcp_config_file(path: &Path, document: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let contents = serde_json::to_vec_pretty(document)
        .expect("an MCP config document is always representable as JSON");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&contents)
}

/// The background task that exclusively owns one `ManagedProcess`:
/// normalizes+emits every stdout frame, and serializes stdin
/// writes/termination requested through `commands`.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    mut process: ManagedProcess,
    mut commands: mpsc::Receiver<SessionCommand>,
    mut normalizer: ClaudeNormalizer,
    sink: Arc<dyn AdapterEventSink>,
    ids: (RunId, TaskId, WorkerId),
    shared: Arc<StdMutex<SharedSessionInfo>>,
    kind: String,
    scope_tokens: Option<Arc<ScopeTokenStore>>,
) {
    let (run_id, task_id, worker_id) = ids;
    let outcome = loop {
        tokio::select! {
            frame = process.next_stdout_frame() => {
                match frame {
                    Some(bytes) => {
                        if let Ok(events) = normalizer.normalize_line(&kind, &bytes) {
                            for event in events {
                                match event {
                                    ClaudeEvent::Emit(payload) => {
                                        if let AdapterEventPayload::VendorSessionEstablished { vendor_session_id } = &payload {
                                            shared.lock().expect("session info mutex is never poisoned").vendor_session_id = Some(vendor_session_id.clone());
                                        }
                                        if let AdapterEventPayload::UsageReported { input_tokens, output_tokens, cost_usd } = &payload {
                                            shared.lock().expect("session info mutex is never poisoned").last_usage = Some(serde_json::json!({
                                                "inputTokens": input_tokens,
                                                "outputTokens": output_tokens,
                                                "costUsd": cost_usd,
                                            }));
                                        }
                                        let _ = sink.emit(AdapterEvent { run_id, task_id, worker_id, payload }).await;
                                    }
                                    ClaudeEvent::ApprovalRequested { approval_id, hook_name } => {
                                        shared.lock().expect("session info mutex is never poisoned").pending_approvals.insert(approval_id, PendingApproval { hook_name });
                                    }
                                    ClaudeEvent::ApprovalResolved { approval_id, .. } => {
                                        shared.lock().expect("session info mutex is never poisoned").pending_approvals.remove(&approval_id);
                                    }
                                }
                            }
                        }
                        // A single malformed line never kills the whole
                        // session's stream -- it is simply skipped.
                    }
                    None => break process.settle().await,
                }
            }
            cmd = commands.recv() => {
                match cmd {
                    Some(SessionCommand::WriteStdin(bytes, reply)) => {
                        let outcome = process.write_stdin(&bytes).await;
                        if let Err(err) = &outcome {
                            let _ = sink
                                .emit(AdapterEvent {
                                    run_id,
                                    task_id,
                                    worker_id,
                                    payload: AdapterEventPayload::ProtocolHealthChanged {
                                        healthy: false,
                                        detail: Classified {
                                            class: ContentClass::Visible,
                                            value: format!("stdin write failed: {err}"),
                                        },
                                    },
                                })
                                .await;
                        }
                        let _ = reply.send(outcome);
                    }
                    Some(SessionCommand::Terminate(reply)) => {
                        let outcome = process.terminate().await;
                        let _ = reply.send(());
                        break outcome;
                    }
                    None => break process.settle().await,
                }
            }
        }
    };

    // Vendor-exit hook: the loop above only ever breaks once the
    // supervised process has exited (stdout closed, cancelled, or the
    // commands channel closed) -- revoke the scope token and delete the
    // `--mcp-config` temp file right here, regardless of which arm broke
    // the loop. `mcp_config_path.take()` (guarded by the same mutex
    // `dispose()` reads) ensures the file is deleted at most once even
    // if `dispose()` raced this same cleanup.
    if let Some(scope_tokens) = scope_tokens {
        scope_tokens.revoke_for_run(run_id);
    }
    let path_to_delete = shared
        .lock()
        .expect("session info mutex is never poisoned")
        .mcp_config_path
        .take();
    if let Some(path) = path_to_delete {
        let _ = std::fs::remove_file(path);
    }

    // Emit the process exit event so the registry's completion watcher
    // can dispose the adapter and release the concurrency slot. This is
    // the only normal-path trigger for `authorization.release()`.
    let (exit_code, signal) = outcome.exit_signals();
    let _ = sink
        .emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited { exit_code, signal },
        })
        .await;
}

impl Adapter for ClaudeAdapter {
    fn kind(&self) -> &str {
        "claude"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            protocol: ProtocolKind::Structured,
            // Proven by `command::build_args`'s resume test: a
            // `VendorSessionRef` becomes `--resume <id>`.
            resume: ResumeCapability::Session,
            // Proven by `command::build_stdin_user_message` plus
            // `send`'s pre-start guard: every follow-up/steer/answer/
            // peer message becomes another queued `user` turn on the
            // same `stream-json` stdin stream (see
            // <https://code.claude.com/docs/en/agent-sdk/typescript>'s
            // streaming-input docs) rather than replacing an in-flight
            // turn.
            steering: SteeringCapability::Queued,
            // The vendor CLI's `PermissionRequest` hook lifecycle is
            // observable (see `normalize`'s `ApprovalRequested`/
            // `ApprovalResolved` and the approval fixture test), but
            // resolving one end-to-end through `ApprovalService` is out
            // of this milestone's scope -- `respond_to_approval` always
            // returns `capability_unsupported`.
            approvals: ApprovalsCapability::Observable,
            // Proven by the `result.jsonl`/`initialize.jsonl` fixture
            // tests: the `result` frame's `result` text normalizes to a
            // structured `MessageFinal`.
            structured_result: true,
            // Only the final `result` frame's aggregate usage/cost is
            // normalized (never per-message `usage`), so `Aggregate`,
            // not `PerTurn`.
            usage: UsageCapability::Aggregate,
            // Mandated for every foreign adapter regardless of what the
            // vendor protocol itself observes -- see the shared context.
            nested: NestedCapability::None,
            native_view: NativeViewCapability::None,
            workspace_control: WorkspaceControlCapability::Write,
            // Claude persists sessions to disk independently of this
            // runtime (`--resume`/`--session-id`/`--continue`), proven at
            // the command-construction level by the resume test above.
            durability: DurabilityCapability::VendorResumable,
        }
    }

    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        Box::pin(async move {
            let version_output = self.run_probe_command(&["--version"]).await?;
            let version = version_output
                .split_whitespace()
                .next()
                .map(str::to_string)
                .filter(|s| !s.is_empty());

            let auth_output = self.run_probe_command(&["auth", "status"]).await?;
            let auth_ready = serde_json::from_str::<serde_json::Value>(&auth_output)
                .ok()
                .and_then(|value| value.get("loggedIn").and_then(serde_json::Value::as_bool))
                .unwrap_or(false);

            Ok(ProbeResult {
                version,
                auth_ready,
                capabilities: self.capabilities(),
                // Ambient skills/plugins/hooks/MCP servers are only
                // enumerable from a live `system/init` frame (a real
                // session), never from `--version`/`--help`/`auth
                // status` alone.
                inventory_incomplete: true,
            })
        })
    }

    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let initial_stdin = command::build_stdin_user_message(&spec.prompt);
            self.spawn_session(
                spec.run_id,
                spec.task_id,
                spec.worker_id,
                spec.resume,
                Some(initial_stdin),
                sink,
            )
            .await
        })
    }

    fn resume(
        &self,
        session: VendorSessionRef,
        sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            // `Adapter::resume` carries no `StartSpec`, so there is no
            // per-call `run_id`/`task_id`/`worker_id` to stamp on the
            // `AdapterEvent`s this resumed session emits -- unlike
            // `start()`. This adapter is bound to its run/task/worker at
            // construction (see `ClaudeAdapter::new`) precisely so this
            // path works from a *fresh* instance too (e.g. after a
            // genuine runtime restart), not only when resuming on the
            // same instance that previously called `start()`.
            self.spawn_session(
                self.run_id,
                self.task_id,
                self.worker_id,
                Some(session),
                None,
                sink,
            )
            .await
        })
    }

    fn send(&self, message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let text = match &message {
                AdapterMessage::Steer { text }
                | AdapterMessage::FollowUp { text }
                | AdapterMessage::Answer { text }
                | AdapterMessage::PeerMessage { text } => text.clone(),
            };
            let state = self.state.lock().await;
            let Some(commands) = state.commands.clone() else {
                return Err(AdapterError::invalid_vendor_state(
                    self.kind(),
                    "send",
                    "no active vendor session to send this message to",
                ));
            };
            drop(state);
            let bytes = command::build_stdin_user_message(&text);
            let (reply_tx, reply_rx) = oneshot::channel();
            commands
                .send(SessionCommand::WriteStdin(bytes, reply_tx))
                .await
                .map_err(|_| {
                    AdapterError::invalid_vendor_state(
                        self.kind(),
                        "send",
                        "the vendor session's background task has already exited",
                    )
                })?;
            // Wait for the real write outcome rather than the command
            // merely having been enqueued -- a broken pipe must not be
            // reported to the caller as a successful send.
            match reply_rx.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(AdapterError::process(
                    self.kind(),
                    "send",
                    format!("stdin write failed: {err}"),
                )),
                Err(_) => Err(AdapterError::invalid_vendor_state(
                    self.kind(),
                    "send",
                    "the vendor session's background task exited before confirming delivery",
                )),
            }
        })
    }

    fn respond_to_approval(&self, _approval_id: &str, _decision: &str) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            Err(AdapterError::capability_unsupported(
                self.kind(),
                "respondToApproval",
            ))
        })
    }

    fn cancel(&self, _scope: CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let Some(commands) = state.commands.take() else {
                return Ok(());
            };
            let (reply_tx, reply_rx) = oneshot::channel();
            if commands
                .send(SessionCommand::Terminate(reply_tx))
                .await
                .is_ok()
            {
                let _ = reply_rx.await;
            }
            Ok(())
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async move {
            let state = self.state.lock().await;
            let shared = state
                .shared
                .lock()
                .expect("session info mutex is never poisoned");
            let mut state_summary = match &shared.vendor_session_id {
                Some(session_id) => format!("claude session {session_id}"),
                None => String::new(),
            };
            if !shared.pending_approvals.is_empty() {
                let hook_names: Vec<&str> = shared
                    .pending_approvals
                    .values()
                    .map(|approval| approval.hook_name.as_str())
                    .collect();
                if !state_summary.is_empty() {
                    state_summary.push_str(", ");
                }
                state_summary.push_str(&format!(
                    "{} pending approval(s): {}",
                    hook_names.len(),
                    hook_names.join(", ")
                ));
            }
            Ok(AdapterSnapshot {
                state_summary,
                children: Vec::new(),
                usage: shared.last_usage.clone(),
                artifacts: Vec::new(),
            })
        })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            if let Some(commands) = state.commands.take() {
                let (reply_tx, reply_rx) = oneshot::channel();
                if commands
                    .send(SessionCommand::Terminate(reply_tx))
                    .await
                    .is_ok()
                {
                    let _ = reply_rx.await;
                }
            }
            if let Some(mcp) = &self.mcp {
                mcp.scope_tokens.revoke_for_run(self.run_id);
                let path_to_delete = state
                    .shared
                    .lock()
                    .expect("session info mutex is never poisoned")
                    .mcp_config_path
                    .take();
                if let Some(path) = path_to_delete {
                    let _ = std::fs::remove_file(path);
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod session_exit_tests {
    use std::sync::Mutex;

    use batman_protocol::{RunId, TaskId, WorkerId};
    use batman_runtime::adapter::{
        AdapterEvent, AdapterEventPayload, AdapterEventSink, AdapterFuture,
    };
    use batman_runtime::supervisor::{EscalationTimings, SpawnSpec, Supervisor};
    use tokio::sync::mpsc;

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
    async fn a_vendor_process_exit_emits_process_exited_with_its_code() {
        let supervisor = Supervisor::with_escalation(fast_escalation());
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "printf \"not-json\\n\"; exit 7".into()],
            cwd: PathBuf::from("/tmp"),
            env: std::collections::HashMap::new(),
            max_stdout_frame_bytes: 8192,
            max_stderr_capture_bytes: 4096,
        };
        let process = supervisor.spawn(spec).await.expect("spawn /bin/sh");
        let (commands_tx, commands_rx) = mpsc::channel(4);
        // Keep `commands_tx` bound so the channel-closed arm is not the
        // one taken — we want the stdout-closed path.
        let _commands_tx = commands_tx;

        let sink = RecordingSink::new();
        run_session(
            process,
            commands_rx,
            ClaudeNormalizer::new(),
            sink.clone(),
            (RunId::new(), TaskId::new(), WorkerId::new()),
            Arc::new(StdMutex::new(SharedSessionInfo::default())),
            "claude".to_string(),
            None,
        )
        .await;

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
                    exit_code: Some(7),
                    signal: None
                }
            ),
            "expected ProcessExited {{ exit_code: Some(7), signal: None }}, got {:?}",
            exited[0]
        );
    }

    #[tokio::test]
    async fn a_terminate_command_still_emits_process_exited() {
        let supervisor = Supervisor::with_escalation(fast_escalation());
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            cwd: PathBuf::from("/tmp"),
            env: std::collections::HashMap::new(),
            max_stdout_frame_bytes: 8192,
            max_stderr_capture_bytes: 4096,
        };
        let process = supervisor.spawn(spec).await.expect("spawn /bin/sh");
        let (commands_tx, commands_rx) = mpsc::channel(4);

        let sink = RecordingSink::new();
        let handle = tokio::spawn(run_session(
            process,
            commands_rx,
            ClaudeNormalizer::new(),
            sink.clone(),
            (RunId::new(), TaskId::new(), WorkerId::new()),
            Arc::new(StdMutex::new(SharedSessionInfo::default())),
            "claude".to_string(),
            None,
        ));

        // Send the terminate command
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        commands_tx
            .send(SessionCommand::Terminate(reply_tx))
            .await
            .expect("send terminate");
        reply_rx.await.expect("terminate reply");
        handle.await.expect("run_session completed");

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
    }

    /// A stdin write that fails (here: deterministically, by closing the
    /// process's stdin before the session ever sees it, rather than racing
    /// a real process's exit) used to be discarded with `let _ = ...`,
    /// reporting a false success to both the command's own reply and the
    /// journal. It must instead surface as a `ProtocolHealthChanged`
    /// diagnostic and a failed reply.
    #[tokio::test]
    async fn a_failed_stdin_write_emits_a_protocol_health_diagnostic_and_replies_with_the_error() {
        let supervisor = Supervisor::with_escalation(fast_escalation());
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            cwd: PathBuf::from("/tmp"),
            env: std::collections::HashMap::new(),
            max_stdout_frame_bytes: 8192,
            max_stderr_capture_bytes: 4096,
        };
        let mut process = supervisor.spawn(spec).await.expect("spawn /bin/sh");
        // Deterministically force every subsequent `write_stdin` to fail,
        // instead of racing a real process's exit to close the pipe.
        process.close_stdin();
        let (commands_tx, commands_rx) = mpsc::channel(4);

        let sink = RecordingSink::new();
        let handle = tokio::spawn(run_session(
            process,
            commands_rx,
            ClaudeNormalizer::new(),
            sink.clone(),
            (RunId::new(), TaskId::new(), WorkerId::new()),
            Arc::new(StdMutex::new(SharedSessionInfo::default())),
            "claude".to_string(),
            None,
        ));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        commands_tx
            .send(SessionCommand::WriteStdin(b"hello\n".to_vec(), reply_tx))
            .await
            .expect("send write_stdin");
        let outcome = reply_rx.await.expect("write_stdin reply is delivered");
        assert!(
            outcome.is_err(),
            "a closed stdin must be reported as a failed write, not a false success"
        );

        let (term_tx, term_rx) = tokio::sync::oneshot::channel();
        commands_tx
            .send(SessionCommand::Terminate(term_tx))
            .await
            .expect("send terminate");
        term_rx.await.expect("terminate reply");
        handle.await.expect("run_session completed");

        let payloads = sink.payloads();
        assert!(
            payloads.iter().any(|p| matches!(
                p,
                AdapterEventPayload::ProtocolHealthChanged { healthy: false, .. }
            )),
            "expected a ProtocolHealthChanged(healthy: false) diagnostic for the failed stdin \
             write: {payloads:?}"
        );
    }
}

#[cfg(test)]
mod run_state_tests {
    //! R69: `run_session` drives a seeded run's durable `RunState` through
    //! the *production* sink chain from the evidence it emits, with a real
    //! supervised `/bin/sh` child standing in for `claude` (no real CLI or
    //! model on PATH).
    //!
    //! The child prints the first line of
    //! `fixtures/adapters/claude/initialize.jsonl` verbatim, so the real
    //! `RawFrame::parse`/`ClaudeNormalizer` path produces the
    //! `VendorSessionEstablished` that is the first non-exit payload the
    //! sink sees — which walks a `queued` run `queued -> starting ->
    //! working` — and then exits with a fixed status, which `run_session`
    //! reads back from the OS into `ProcessExited` and which terminalizes
    //! the run. The seeded `RunQueued` event (`DomainRepository::submit_run`)
    //! is the walk's starting point in the journal, so the
    //! exact-sequence assertions include it.

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use batman_protocol::{
        EventEnvelope, ProjectId, Run, RunFlags, RunId, RunState, RuntimeEvent, TaskId, TaskRef,
        Timestamp, Worker, WorkerId, WorkerProfileRef,
    };
    use batman_runtime::adapter::{AdapterEventSink, DomainAdapterEventSink, RunLifecycleSink};
    use batman_runtime::config::NestedViolationAction;
    use batman_runtime::db::DatabaseHandle;
    use batman_runtime::domain::DomainRepository;
    use batman_runtime::policy::ViolationService;
    use batman_runtime::supervisor::{EscalationTimings, SpawnSpec, Supervisor};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    use super::*;

    /// The first line of the vendored initialize fixture — emitted
    /// verbatim by the fake vendor process so the real `RawFrame::parse`
    /// path produces the `SystemInit` the normalizer turns into
    /// `VendorSessionEstablished`.
    fn init_line() -> &'static str {
        static LINE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            let json = include_str!("../../../../../fixtures/adapters/claude/initialize.jsonl");
            json.lines()
                .next()
                .expect("the fixture's first line is the init frame")
                .to_string()
        });
        LINE.as_str()
    }

    /// A real, migrated database on a throwaway file: the same pattern
    /// `run_lifecycle.rs`'s unit tests use (per-test `TempDir`, explicit
    /// `shutdown` so the database actor thread never outlives the test).
    async fn open_db() -> (TempDir, Arc<DatabaseHandle>) {
        let dir = tempfile::Builder::new()
            .prefix("bat-claude-run-state-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            DatabaseHandle::start(db_path)
                .await
                .expect("start database"),
        );
        (dir, db)
    }

    /// Seeds one task + worker + `queued` run through the real
    /// `DomainRepository` API (copied from `run_lifecycle.rs`'s unit-test
    /// harness) — the run row and its journaled `RunQueued` event are the
    /// walk's starting point.
    async fn seed_run(db: &DatabaseHandle, project_id: ProjectId) -> (TaskId, WorkerId, RunId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &TaskRef {
                    owner_client_instance_id: "omp-1".to_string(),
                    revision: 1,
                },
            )?;
            let worker = Worker {
                worker_id,
                profile_ref: WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".to_string(),
                    adapter: "claude".to_string(),
                    model: "test".to_string(),
                    permission_envelope: serde_json::json!({}),
                },
                parent_worker_id: None,
                created_at: Timestamp::now(),
            };
            repo.create_worker(&worker)?;
            let run = Run {
                run_id,
                task_id,
                worker_id,
                state: RunState::try_from("queued").expect("queued is a valid state"),
                flags: RunFlags::default(),
                vendor_session_id: None,
                started_at: None,
                completed_at: None,
            };
            repo.submit_run(&run, None, None)?;
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed run");
        (task_id, worker_id, run_id)
    }

    /// The production sink chain for this run — `DomainAdapterEventSink`
    /// (sanitize + journal + broadcast) wrapped in `RunLifecycleSink` (the
    /// evidence-driven `RunState` edges under test) — mirroring
    /// `registry::run_one` minus the settlement layer, which only observes
    /// the terminal edge this suite proves `RunLifecycleSink` commits.
    fn production_sink_chain(
        db: &Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
        run_id: RunId,
    ) -> Arc<dyn AdapterEventSink> {
        let violation = Arc::new(ViolationService::new(
            Arc::clone(db),
            project_id,
            events_tx.clone(),
            None,
            NestedViolationAction::default(),
        ));
        let domain_sink = Arc::new(
            DomainAdapterEventSink::new(
                Arc::clone(db),
                project_id,
                events_tx.clone(),
                Vec::new(),
                false,
                violation,
                None,
            )
            .expect("built-in patterns always compile"),
        );
        RunLifecycleSink::wrap(domain_sink, Arc::clone(db), project_id, events_tx, run_id)
    }

    /// Reads a run's current projected state directly, for assertions.
    async fn run_state(db: &DatabaseHandle, run_id: RunId) -> String {
        db.run_domain_op(Box::new(move |conn| {
            let state: String = conn.query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            Ok(serde_json::json!(state))
        }))
        .await
        .expect("read run state")
        .as_str()
        .expect("state is a string")
        .to_string()
    }

    /// Every journaled run-state event for `run_id`, in sequence order:
    /// the `state` each `RunEvent` recorded, so the exact walk the sink
    /// committed is readable back out of the durable journal.
    async fn run_states(db: &DatabaseHandle, run_id: RunId) -> Vec<String> {
        let raw: Vec<String> = db
            .run_domain_op(Box::new(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
                let rows: Vec<String> = stmt
                    .query_map([run_id.to_string()], |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok(serde_json::json!(rows))
            }))
            .await
            .expect("read journaled events")
            .as_array()
            .expect("rows are an array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        raw.into_iter()
            .filter_map(|raw| {
                let event: RuntimeEvent =
                    serde_json::from_str(&raw).expect("parse a journaled event");
                match event {
                    RuntimeEvent::RunEvent { state, .. } => Some(state),
                    _ => None,
                }
            })
            .collect()
    }

    /// Runs the whole session: a real supervised `/bin/sh` child stands in
    /// for `claude` — it prints the fixture's init frame and exits with
    /// `code`; `run_session` normalizes the frame through the real
    /// normalizer, drives the production sink chain, and settles the
    /// process when stdout closes, emitting the OS-observed exit status.
    async fn drive_fake_claude(code: i32) -> (TempDir, Arc<DatabaseHandle>, RunId) {
        let (dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let sink = production_sink_chain(&db, project_id, events_tx, run_id);

        let script = format!("printf '%s\\n' '{}'; exit {code}\n", init_line());
        let spec = SpawnSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script],
            cwd: PathBuf::from("/tmp"),
            env: HashMap::new(),
            max_stdout_frame_bytes: 8192,
            max_stderr_capture_bytes: 4096,
        };
        let supervisor = Supervisor::with_escalation(EscalationTimings {
            sigint_to_sigterm: Duration::from_millis(50),
            sigterm_to_sigkill: Duration::from_millis(50),
        });
        let process = supervisor.spawn(spec).await.expect("spawn /bin/sh");
        let (commands_tx, commands_rx) = mpsc::channel(4);
        // Keep `commands_tx` bound so the channel-closed arm is not the
        // one taken — we want the stdout-closed path.
        let _commands_tx = commands_tx;

        run_session(
            process,
            commands_rx,
            ClaudeNormalizer::new(),
            sink,
            (run_id, task_id, worker_id),
            Arc::new(StdMutex::new(SharedSessionInfo::default())),
            "claude".to_string(),
            None,
        )
        .await;
        (dir, db, run_id)
    }

    #[tokio::test]
    async fn a_vendor_session_that_exits_cleanly_walks_its_run_to_succeeded() {
        let (dir, db, run_id) = drive_fake_claude(0).await;
        assert_eq!(run_state(&db, run_id).await, "succeeded");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "succeeded".to_string(),
            ],
            "the walked edges must be exactly queued -> starting -> working -> succeeded"
        );
        db.shutdown().await.expect("shutdown database");
        drop(dir);
    }

    #[tokio::test]
    async fn a_vendor_session_that_exits_nonzero_walks_its_run_to_failed() {
        let (dir, db, run_id) = drive_fake_claude(7).await;
        assert_eq!(run_state(&db, run_id).await, "failed");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "failed".to_string(),
            ],
            "the walked edges must be exactly queued -> starting -> working -> failed"
        );
        db.shutdown().await.expect("shutdown database");
        drop(dir);
    }
}
