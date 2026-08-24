//! ACP JSON-RPC client over `copilot --acp`'s NDJSON stdio.
//!
//! Copilot's ACP server is reached exclusively through the supervised
//! process's stdin/stdout, one JSON-RPC 2.0 object per line (NDJSON).
//! **No code path in this module ever constructs a `--port` argument, a
//! host/port pair, or opens a TCP connection** -- even though public
//! Copilot docs describe a `--port` option, the installed 1.0.73 binary
//! opens no listener for `--port 0` (empirically verified; see
//! `tests/copilot_adapter.rs`'s `copilot_acp_client_never_builds_a_tcp_path`
//! and `real_binary_port_zero_opens_no_listener` tests). `CopilotAcpClient`
//! only ever calls [`crate::supervisor::Supervisor::spawn`] with `--acp`
//! and reads/writes the resulting `ManagedProcess`'s piped stdio.
//!
//! A single background task owns the [`ManagedProcess`] for this client's
//! entire lifetime and drives both directions with one `tokio::select!`
//! loop: it never blocks indefinitely on a read while a write is pending,
//! and never blocks indefinitely on a write while a read is pending,
//! because `ManagedProcess`'s stdin/stdout methods each take `&mut self`
//! and therefore cannot be driven from two independent tasks without a
//! lock that would otherwise starve one side.
//!
//! Incoming frames are classified by JSON-RPC shape:
//! - `{id, result|error}` (no `method`) resolves a pending call this
//!   client made (see [`CopilotAcpClient::call`]).
//! - `{id, method: "session/request_permission"}` is recorded as a
//!   pending permission and surfaced via [`CopilotClientEvent::PermissionRequested`]
//!   -- **not answered automatically**. Per the shared adapter contract,
//!   wiring this through to `crate::approval::ApprovalService` end-to-end
//!   is a later integration point; for now [`CopilotAcpClient::respond_permission`]
//!   is the only path that answers it (driven by `Adapter::respond_to_approval`).
//! - `{id, method: <anything else>}` (e.g. `fs/*`, `terminal/*`) is a
//!   real ACP request this client does not implement in this milestone
//!   (this adapter always advertises `fs`/`terminal` client capabilities
//!   as unsupported at `initialize`, so Copilot should not send these,
//!   but a well-behaved JSON-RPC peer is never left hanging): it gets an
//!   explicit JSON-RPC "method not found" error response.
//! - `{method, no id}` is a notification; `session/update` is normalized
//!   via [`super::normalize::copilot_normalize_session_update`] and
//!   surfaced as [`CopilotClientEvent::SessionUpdate`].

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use tokio::sync::{Mutex as TokioMutex, Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crew_runtime::adapter::{AdapterError, AdapterEventPayload};
use crew_runtime::supervisor::{SpawnSpec, Supervisor};

use super::compatibility::{
    COPILOT_MAX_ACP_PROTOCOL_VERSION, COPILOT_MIN_ACP_PROTOCOL_VERSION,
    copilot_acp_protocol_version_supported,
};
use super::normalize::copilot_normalize_session_update;

/// One permission option Copilot offered, as recorded from a real
/// `session/request_permission` request.
#[derive(Debug, Clone)]
pub struct CopilotPermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

/// A pending `session/request_permission` request this client has not yet
/// answered.
#[derive(Debug, Clone)]
pub struct CopilotPermissionRequest {
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_call_title: String,
    pub options: Vec<CopilotPermissionOption>,
}

/// Events this client surfaces to its caller (in addition to direct call
/// responses), in the order observed on the wire.
#[derive(Debug, Clone)]
pub enum CopilotClientEvent {
    /// A `session/update` notification, already normalized.
    SessionUpdate {
        session_id: String,
        payloads: Vec<AdapterEventPayload>,
    },
    /// A `session/request_permission` request awaiting
    /// [`CopilotAcpClient::respond_permission`].
    PermissionRequested {
        request_id: i64,
        request: CopilotPermissionRequest,
    },
    /// The supervised `copilot --acp` process's stdout closed; carries the
    /// supervised process's own exit status via
    /// [`crate::supervisor::TerminationOutcome::exit_signals`].
    ProcessExited {
        exit_code: Option<i32>,
        signal: Option<String>,
    },
}

/// The negotiated ACP capabilities from a real `initialize` response --
/// only the fields this adapter empirically observed the installed
/// 1.0.73 binary advertise (see `fixtures/adapters/copilot/initialize-v1.json`
/// and `tests/copilot_adapter.rs`).
#[derive(Debug, Clone, PartialEq)]
pub struct CopilotNegotiatedCapabilities {
    pub protocol_version: u64,
    pub agent_version: Option<String>,
    pub load_session: bool,
    pub session_list: bool,
    pub mcp_http: bool,
    pub mcp_sse: bool,
    pub image: bool,
    pub embedded_context: bool,
}

/// An ACP JSON-RPC client for `copilot --acp`, over NDJSON stdio only.
pub struct CopilotAcpClient {
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    #[allow(clippy::type_complexity)]
    pending_responses: Arc<StdMutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>>,
    pending_permissions: Arc<StdMutex<HashMap<i64, CopilotPermissionRequest>>>,
    events_rx: TokioMutex<mpsc::UnboundedReceiver<CopilotClientEvent>>,
    next_id: AtomicI64,
    reader_task: TokioMutex<Option<JoinHandle<()>>>,
    shutdown: Arc<Notify>,
    process_pid: i32,
}

impl CopilotAcpClient {
    /// Spawns `program --acp` (plus any additional `extra_args`, e.g.
    /// `--allow-tool`/`--deny-tool`/`--log-level` from
    /// [`super::super::profile::CopilotStartupOptions`]) under
    /// [`Supervisor`] and starts the single reader/writer task. Never adds
    /// `--port` or any TCP-facing flag.
    ///
    /// # Errors
    /// Returns [`AdapterError::process`] if the process cannot be spawned.
    pub async fn spawn(
        program: &Path,
        cwd: &Path,
        mut extra_args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Result<Self, AdapterError> {
        let mut args = vec!["--acp".to_string()];
        args.append(&mut extra_args);
        debug_assert!(
            !args
                .iter()
                .any(|a| a == "--port" || a.starts_with("--port=")),
            "the Copilot ACP client must never request a TCP listener"
        );
        Self::spawn_impl(program, cwd, args, env).await
    }

    /// Test-only entry point: spawns `program` with `args` verbatim, with
    /// **no** implicit `--acp` prefix, so a test can drive this client's
    /// JSON-RPC machinery (call/notify/event dispatch, permission
    /// handling) against a fixture-replaying fake process without
    /// needing the real CLI's exact flag surface. Production adapter
    /// code (`CopilotAdapter`) only ever calls [`Self::spawn`].
    #[doc(hidden)]
    pub async fn spawn_with_raw_args(
        program: &Path,
        cwd: &Path,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Result<Self, AdapterError> {
        Self::spawn_impl(program, cwd, args, env).await
    }

    async fn spawn_impl(
        program: &Path,
        cwd: &Path,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Result<Self, AdapterError> {
        let supervisor = Supervisor::new();
        let spec = SpawnSpec {
            program: program.to_path_buf(),
            args,
            cwd: cwd.to_path_buf(),
            env,
            ..SpawnSpec::minimal()
        };
        let mut process = supervisor
            .spawn(spec)
            .await
            .map_err(|source| AdapterError::process("copilot", "spawn", source.to_string()))?;
        let process_pid = process.pid();

        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        #[allow(clippy::type_complexity)]
        let (events_tx, events_rx) = mpsc::unbounded_channel::<CopilotClientEvent>();
        #[allow(clippy::type_complexity)]
        let pending_responses: Arc<
            StdMutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>,
        > = Arc::new(StdMutex::new(HashMap::new()));
        let pending_permissions: Arc<StdMutex<HashMap<i64, CopilotPermissionRequest>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let shutdown = Arc::new(Notify::new());

        let reader_pending_responses = pending_responses.clone();
        let reader_pending_permissions = pending_permissions.clone();
        let reader_write_tx = write_tx.clone();
        let reader_shutdown = shutdown.clone();
        let reader_task = tokio::spawn(async move {
            let stdout_closed = loop {
                tokio::select! {
                    biased;
                    () = reader_shutdown.notified() => break false,
                    written = write_rx.recv() => {
                        match written {
                            Some(bytes) => { let _ = process.write_stdin(&bytes).await; }
                            None => break false,
                        }
                    }
                    frame = process.next_stdout_frame() => {
                        match frame {
                            Some(bytes) => handle_frame(
                                &bytes,
                                &reader_pending_responses,
                                &reader_pending_permissions,
                                &events_tx,
                                &reader_write_tx,
                            ),
                            None => break true,
                        }
                    }
                }
            };
            process.close_stdin();
            let outcome = if stdout_closed {
                process.settle().await
            } else {
                process.terminate().await
            };
            if stdout_closed {
                let (exit_code, signal) = outcome.exit_signals();
                let _ = events_tx.send(CopilotClientEvent::ProcessExited { exit_code, signal });
            }
        });

        Ok(Self {
            write_tx,
            pending_responses,
            pending_permissions,
            events_rx: TokioMutex::new(events_rx),
            next_id: AtomicI64::new(1),
            reader_task: TokioMutex::new(Some(reader_task)),
            shutdown,
            process_pid,
        })
    }

    /// This client's supervised process pid.
    #[must_use]
    pub fn pid(&self) -> i32 {
        self.process_pid
    }

    /// Sends the ACP `initialize` request with every client capability
    /// declared `false`/absent -- this adapter never asks Copilot to
    /// delegate `fs/*`/`terminal/*` operations back to it, so those
    /// request kinds should never arrive on this client at all.
    ///
    /// # Errors
    /// Returns [`AdapterError::incompatible_version`] if the negotiated
    /// protocol version is outside what `normalize.rs` understands, or
    /// [`AdapterError::protocol`] if the response is malformed.
    pub async fn initialize(&self) -> Result<CopilotNegotiatedCapabilities, AdapterError> {
        let params = json!({
            "protocolVersion": COPILOT_MAX_ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false,
            },
        });
        let result = self.call("initialize", params).await?;
        parse_initialize_response(&result)
    }

    /// Creates a new ACP session rooted at `cwd`.
    ///
    /// # Errors
    /// Returns [`AdapterError::protocol`] if the response is missing
    /// `sessionId`.
    pub async fn session_new(&self, cwd: &str) -> Result<String, AdapterError> {
        let result = self
            .call("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                AdapterError::protocol("copilot", "session/new", "response missing sessionId")
            })
    }

    /// Loads a previously established session by its ACP `sessionId`,
    /// replaying its full history to this client.
    ///
    /// # Errors
    /// Returns [`AdapterError::process`] on transport failure.
    pub async fn session_load(&self, session_id: &str, cwd: &str) -> Result<(), AdapterError> {
        self.call(
            "session/load",
            json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
        )
        .await?;
        Ok(())
    }

    /// Lists sessions Copilot has persisted, exactly as `session/list`
    /// returns them (unmodified `sessions` array).
    ///
    /// # Errors
    /// Returns [`AdapterError::process`] on transport failure.
    pub async fn session_list(&self) -> Result<Value, AdapterError> {
        self.call("session/list", json!({})).await
    }

    /// Sends a user prompt and awaits the turn's `stopReason`. Real
    /// session updates streamed during the turn arrive as
    /// [`CopilotClientEvent::SessionUpdate`] through [`Self::next_event`]
    /// concurrently with this call.
    ///
    /// # Errors
    /// Returns [`AdapterError::protocol`] if the response is missing
    /// `stopReason`.
    pub async fn session_prompt(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<String, AdapterError> {
        let result = self
            .call(
                "session/prompt",
                json!({
                    "sessionId": session_id,
                    "prompt": [{ "type": "text", "text": text }],
                }),
            )
            .await?;
        result
            .get("stopReason")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                AdapterError::protocol("copilot", "session/prompt", "response missing stopReason")
            })
    }

    /// Sends `session/cancel` for `session_id`. A notification: Copilot
    /// sends no direct response, only further `session/update`s and
    /// eventually the pending `session/prompt`'s `Cancelled` stop reason.
    ///
    /// # Errors
    /// Returns [`AdapterError::process`] if the write cannot be queued.
    pub fn session_cancel(&self, session_id: &str) -> Result<(), AdapterError> {
        self.notify("session/cancel", json!({ "sessionId": session_id }))
    }

    /// Answers a pending `session/request_permission` request. `decision`
    /// is either the literal `optionId` Copilot offered (see
    /// [`CopilotPermissionRequest::options`]) or the literal string
    /// `"cancelled"`.
    ///
    /// # Errors
    /// Returns [`AdapterError::invalid_vendor_state`] if `request_id` has
    /// no pending permission request (already answered, or never sent).
    pub fn respond_permission(&self, request_id: i64, decision: &str) -> Result<(), AdapterError> {
        {
            let mut pending = self
                .pending_permissions
                .lock()
                .expect("pending permissions mutex is never poisoned");
            if pending.remove(&request_id).is_none() {
                return Err(AdapterError::invalid_vendor_state(
                    "copilot",
                    "respondToApproval",
                    "no pending permission request with this id",
                ));
            }
        }
        let outcome = if decision == "cancelled" {
            json!({ "outcome": "cancelled" })
        } else {
            json!({ "outcome": "selected", "optionId": decision })
        };
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": { "outcome": outcome },
        }))
    }

    /// The ids of every `session/request_permission` request not yet
    /// answered via [`Self::respond_permission`].
    #[must_use]
    pub fn pending_permission_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .pending_permissions
            .lock()
            .expect("pending permissions mutex is never poisoned")
            .keys()
            .copied()
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Waits for the next notification/permission-request/exit event.
    /// `None` once the reader task has shut down and every event has been
    /// drained.
    pub async fn next_event(&self) -> Option<CopilotClientEvent> {
        self.events_rx.lock().await.recv().await
    }

    /// Stops the reader/writer task and terminates the supervised
    /// process (escalating `SIGINT` -> `SIGTERM` -> `SIGKILL` via
    /// [`crate::supervisor::ManagedProcess::terminate`]). Idempotent.
    pub async fn shutdown(&self) {
        self.shutdown.notify_one();
        let handle = self.reader_task.lock().await.take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, AdapterError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending_responses
            .lock()
            .expect("pending responses mutex is never poisoned")
            .insert(id, tx);
        self.send_line(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        match rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error_value)) => Err(AdapterError::protocol(
                "copilot",
                method,
                format!(
                    "ACP error response: {}",
                    error_value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                ),
            )),
            Err(_) => Err(AdapterError::process(
                "copilot",
                method,
                "the ACP reader task exited before a response arrived",
            )),
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), AdapterError> {
        self.send_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn send_line(&self, value: &Value) -> Result<(), AdapterError> {
        let mut bytes = serde_json::to_vec(value)
            .map_err(|source| AdapterError::protocol("copilot", "encode", source.to_string()))?;
        bytes.push(b'\n');
        self.write_tx.send(bytes).map_err(|_| {
            AdapterError::process("copilot", "write", "the ACP reader/writer task has exited")
        })
    }
}

/// Parses a raw `initialize` JSON-RPC `result` into
/// [`CopilotNegotiatedCapabilities`], pure and independent of any live
/// process -- exercised directly against
/// `fixtures/adapters/copilot/initialize-v1.json` in
/// `tests/copilot_adapter.rs`.
///
/// # Errors
/// Returns [`AdapterError::protocol`] if `protocolVersion` is missing, or
/// [`AdapterError::incompatible_version`] if it is outside
/// `[COPILOT_MIN_ACP_PROTOCOL_VERSION, COPILOT_MAX_ACP_PROTOCOL_VERSION]`.
pub fn parse_initialize_response(
    result: &Value,
) -> Result<CopilotNegotiatedCapabilities, AdapterError> {
    let protocol_version = result
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            AdapterError::protocol("copilot", "initialize", "response missing protocolVersion")
        })?;
    if !copilot_acp_protocol_version_supported(protocol_version) {
        return Err(AdapterError::incompatible_version(
            "copilot",
            "initialize",
            format!(
                "negotiated ACP protocol version {protocol_version} is outside the versions this \
                 adapter's normalize.rs understands ({COPILOT_MIN_ACP_PROTOCOL_VERSION}..={COPILOT_MAX_ACP_PROTOCOL_VERSION})"
            ),
        ));
    }
    let empty = Value::Null;
    let caps = result.get("agentCapabilities").unwrap_or(&empty);
    let agent_version = result
        .get("agentInfo")
        .and_then(|info| info.get("version"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(CopilotNegotiatedCapabilities {
        protocol_version,
        agent_version,
        load_session: caps
            .get("loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        session_list: caps
            .get("sessionCapabilities")
            .and_then(|v| v.get("list"))
            .is_some(),
        mcp_http: caps
            .get("mcpCapabilities")
            .and_then(|v| v.get("http"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        mcp_sse: caps
            .get("mcpCapabilities")
            .and_then(|v| v.get("sse"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        image: caps
            .get("promptCapabilities")
            .and_then(|v| v.get("image"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        embedded_context: caps
            .get("promptCapabilities")
            .and_then(|v| v.get("embeddedContext"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_permission_request(msg: &Value) -> Option<CopilotPermissionRequest> {
    let params = msg.get("params")?;
    let session_id = params.get("sessionId")?.as_str()?.to_string();
    let tool_call = params.get("toolCall")?;
    let tool_call_id = tool_call.get("toolCallId")?.as_str()?.to_string();
    let tool_call_title = tool_call
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(&tool_call_id)
        .to_string();
    let options = params
        .get("options")?
        .as_array()?
        .iter()
        .filter_map(|option| {
            Some(CopilotPermissionOption {
                option_id: option.get("optionId")?.as_str()?.to_string(),
                name: option
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                kind: option
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect();
    Some(CopilotPermissionRequest {
        session_id,
        tool_call_id,
        tool_call_title,
        options,
    })
}

/// Classifies and dispatches one raw NDJSON frame from Copilot's stdout.
/// Never blocks: a request this client cannot answer immediately
/// (`session/request_permission`) is recorded and surfaced as an event
/// rather than answered inline; any other unimplemented incoming request
/// gets an explicit JSON-RPC error response so Copilot is never left
/// waiting indefinitely for one this client will never send.
#[allow(clippy::type_complexity)]
fn handle_frame(
    frame: &[u8],
    pending_responses: &Arc<StdMutex<HashMap<i64, oneshot::Sender<Result<Value, Value>>>>>,
    pending_permissions: &Arc<StdMutex<HashMap<i64, CopilotPermissionRequest>>>,
    events_tx: &mpsc::UnboundedSender<CopilotClientEvent>,
    write_tx: &mpsc::UnboundedSender<Vec<u8>>,
) {
    let Ok(text) = std::str::from_utf8(frame) else {
        return;
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
        return;
    };

    let id = msg.get("id").and_then(Value::as_i64);
    let method = msg.get("method").and_then(Value::as_str);

    match (id, method) {
        (Some(id), Some("session/request_permission")) => {
            if let Some(request) = parse_permission_request(&msg) {
                pending_permissions
                    .lock()
                    .expect("pending permissions mutex is never poisoned")
                    .insert(id, request.clone());
                let _ = events_tx.send(CopilotClientEvent::PermissionRequested {
                    request_id: id,
                    request,
                });
            }
        }
        (Some(id), Some(_unimplemented)) => {
            let error_response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not implemented by this ACP client" },
            });
            if let Ok(mut bytes) = serde_json::to_vec(&error_response) {
                bytes.push(b'\n');
                let _ = write_tx.send(bytes);
            }
        }
        (Some(id), None) => {
            if let Some(tx) = pending_responses
                .lock()
                .expect("pending responses mutex is never poisoned")
                .remove(&id)
            {
                if let Some(error) = msg.get("error") {
                    let _ = tx.send(Err(error.clone()));
                } else {
                    let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
                }
            }
        }
        (None, Some("session/update")) => {
            if let Some(params) = msg.get("params") {
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if let Some(update) = params.get("update") {
                    let payloads = copilot_normalize_session_update(update);
                    if !payloads.is_empty() {
                        let _ = events_tx.send(CopilotClientEvent::SessionUpdate {
                            session_id,
                            payloads,
                        });
                    }
                }
            }
        }
        _ => {}
    }
}
