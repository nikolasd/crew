//! The OMP-RPC stdio client: the `{"type":"ready",...}` handshake, request/
//! response correlation by `id`, and pure command-frame builders.
//!
//! The wire shapes below are grounded against the real installed `omp
//! 17.1.1` binary, not invented:
//! - The ready handshake (`omp --mode rpc --model <selector> --no-session
//!   --allow-home < /dev/null`) actually emits
//!   `{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],
//!   "maxFrameBytes":1048576,"maxReassembledFrameBytes":67108864}` before
//!   reading anything.
//! - A `{"type":"get_state","id":"1"}` request against the real binary
//!   returns `{"id":"1","type":"response","command":"get_state",
//!   "success":true,"data":{...,"sessionId":"<uuid>",
//!   "sessionFile":"<path>",...}}` -- the OMP session id/file the plan's
//!   Interfaces section calls out as this adapter's `VendorSessionRef`.
//! - `{"type":"get_session_stats","id":"1"}` returns
//!   `data.tokens.{input,output,...}` and `data.cost`, an aggregate
//!   (session-lifetime, not per-turn) usage shape.
//! - The command names and their parameter field names below are read
//!   directly out of the installed binary's own (minified) RPC dispatch
//!   switch, e.g. `case "prompt": { const H = await kI1(A, E.message,
//!   E.streamingBehavior) ... }`, `case "steer": { await A.steer(E.message,
//!   ...) }`, `case "follow_up": { await A.followUp(E.message, ...) }`,
//!   `case "set_model": { ... E.provider ... E.modelId ... }`,
//!   `case "set_subagent_subscription": { ... uNw(E.level) ...
//!   z.setSubscriptionLevel(E.level) ... }`, `case "set_host_tools": {
//!   const H = fNw(E.tools); ... return u(m, "set_host_tools", {
//!   toolNames: H.map((T) => T.name) }) }` (where `fNw` requires each
//!   tool to carry a non-empty `name`/`description` and a JSON-Schema
//!   object `parameters`), and `case "set_host_uri_schemes": { ...
//!   W.setSchemes(E.schemes) ... return u(m, "set_host_uri_schemes", {
//!   schemes: H }) }` (where each scheme entry is `{scheme, description?,
//!   writable?, immutable?}`, `scheme` matching `^[a-z][a-z0-9+.-]*$`) --
//!   confirming the real parameter names are `message`, `provider`/
//!   `modelId`, `level`, `tools`, and `schemes` respectively, not the
//!   plan text's unqualified prose.
//!
//! Real, unsolicited event frames (e.g. `extension_ui_request`,
//! `available_commands_update`) can arrive interleaved with a pending
//! response; [`OmpRpcClient::read_response`] queues anything that is not
//! the awaited response into `pending_events` rather than discarding it,
//! and a malformed (non-JSON) stdout line is always skipped, never fatal.
//!
//! `set_host_tools`' *invocation* callback (as opposed to registration,
//! which is the ordinary command/response above) is a separate,
//! unsolicited frame pair read directly out of the installed binary's own
//! bundled `packages/coding-agent/src/modes/rpc/host-tools.ts` (via
//! `strings` on the compiled binary -- this file ships enough of its own
//! original source, not just minified identifiers, to read directly):
//! `{"type":"host_tool_call","id":<string>,"toolCallId":<string>,
//! "toolName":<string>,"arguments":<object>}` on stdout, answered with
//! `{"type":"host_tool_result","id":<same id>,"result":{"content":[...],
//! "details":{}},"isError":true}` on failure or
//! `{"type":"host_tool_result","id":<same id>,"result":{"content":[...]}}`
//! (no `isError` key at all) on success, written to stdin. See
//! `super::run_pump`'s interception of this frame *before*
//! `normalize::normalize_frame` ever sees it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};

use crew_runtime::adapter::AdapterError;
use crew_runtime::supervisor::ManagedProcess;

/// A parsed `{"type":"response",...}` frame.
#[derive(Debug, Clone)]
pub struct RpcResponse {
    pub id: String,
    pub command: String,
    pub success: bool,
    pub data: Value,
    pub error: Option<String>,
}

/// The ready-frame handshake + command/response client over one
/// [`ManagedProcess`]'s stdio.
pub struct OmpRpcClient {
    process: ManagedProcess,
    next_id: AtomicU64,
    /// Unsolicited frames observed while waiting for a specific
    /// correlated response (or before the ready handshake completed),
    /// preserved in arrival order rather than discarded.
    pending_events: VecDeque<Value>,
}

impl OmpRpcClient {
    #[must_use]
    pub fn new(process: ManagedProcess) -> Self {
        Self {
            process,
            next_id: AtomicU64::new(1),
            pending_events: VecDeque::new(),
        }
    }

    fn fresh_id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::SeqCst).to_string()
    }

    /// Reads stdout frames until the `{"type":"ready"}` handshake frame
    /// arrives. A malformed (non-UTF8 or non-JSON) line is skipped, never
    /// fatal; any other well-formed frame seen before `ready` is queued
    /// into `pending_events` rather than discarded (the real binary can
    /// emit `extension_ui_request` immediately after `ready`, so this
    /// adapter treats "something before ready" as merely unusual, not a
    /// protocol violation).
    ///
    /// # Errors
    /// Returns [`AdapterError::process`] if stdout closes before a ready
    /// frame is ever observed.
    pub async fn wait_for_ready(&mut self) -> Result<Value, AdapterError> {
        loop {
            let Some(bytes) = self.process.next_stdout_frame().await else {
                return Err(AdapterError::process(
                    "ompRpc",
                    "waitForReady",
                    "process stdout closed before a ready frame was observed",
                ));
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) == Some("ready") {
                return Ok(value);
            }
            self.pending_events.push_back(value);
        }
    }

    /// Writes one `{"type": command, "id": <fresh>, ...params}` request
    /// frame to stdin and returns the id it was sent with.
    ///
    /// # Errors
    /// Returns [`AdapterError::process`] if the write fails (e.g. stdin
    /// already closed).
    pub async fn send_command(
        &mut self,
        command: &str,
        params: Map<String, Value>,
    ) -> Result<String, AdapterError> {
        let id = self.fresh_id();
        let mut frame = params;
        frame.insert("type".to_string(), Value::String(command.to_string()));
        frame.insert("id".to_string(), Value::String(id.clone()));
        let mut line = Value::Object(frame).to_string();
        line.push('\n');
        self.process
            .write_stdin(line.as_bytes())
            .await
            .map_err(|e| {
                AdapterError::process(
                    "ompRpc",
                    command,
                    format!("failed to write {command} command: {e}"),
                )
            })?;
        Ok(id)
    }

    /// Writes an arbitrary already-shaped frame verbatim (one JSON value
    /// per line) -- for a reply this client did not itself originate an
    /// id for, e.g. a `host_tool_result`/`host_tool_update` frame
    /// echoing back the `id` a `host_tool_call` the vendor sent arrived
    /// with (see the module doc's `host_tool_call`/`host_tool_result`
    /// wire shapes, empirically grounded against the installed binary's
    /// own bundled source).
    ///
    /// # Errors
    /// Returns [`AdapterError::process`] if the write fails (e.g. stdin
    /// already closed).
    pub async fn write_frame(&mut self, value: &Value) -> Result<(), AdapterError> {
        let mut line = value.to_string();
        line.push('\n');
        self.process
            .write_stdin(line.as_bytes())
            .await
            .map_err(|e| {
                AdapterError::process(
                    "ompRpc",
                    "writeFrame",
                    format!("failed to write frame: {e}"),
                )
            })
    }

    /// Reads frames until the `{"type":"response","id":<id>,...}` frame
    /// correlated to `id` is found, queuing every other well-formed frame
    /// seen along the way (drainable via [`Self::drain_events`]).
    /// Malformed lines are skipped, never fatal.
    ///
    /// # Errors
    /// Returns [`AdapterError::process`] if stdout closes before the
    /// correlated response is observed.
    pub async fn read_response(&mut self, id: &str) -> Result<RpcResponse, AdapterError> {
        if let Some(pos) = self
            .pending_events
            .iter()
            .position(|value| is_response_for(value, id))
        {
            let value = self
                .pending_events
                .remove(pos)
                .expect("position was just found in the same deque");
            return Ok(parse_response(&value));
        }
        loop {
            let Some(bytes) = self.process.next_stdout_frame().await else {
                return Err(AdapterError::process(
                    "ompRpc",
                    "readResponse",
                    format!("process stdout closed before a response for id {id} arrived"),
                ));
            };
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
                continue;
            };
            if is_response_for(&value, id) {
                return Ok(parse_response(&value));
            }
            self.pending_events.push_back(value);
        }
    }

    /// Reads and returns exactly the next well-formed frame, whatever it
    /// is (a response or an unsolicited event), pulling first from
    /// anything already queued. Malformed lines are skipped, never fatal.
    /// Returns `None` once stdout has closed.
    pub async fn next_frame(&mut self) -> Option<Value> {
        if let Some(value) = self.pending_events.pop_front() {
            return Some(value);
        }
        loop {
            let bytes = self.process.next_stdout_frame().await?;
            let Ok(text) = std::str::from_utf8(&bytes) else {
                continue;
            };
            if let Ok(value) = serde_json::from_str::<Value>(text.trim()) {
                return Some(value);
            }
        }
    }

    /// Drains every event queued while waiting for a correlated response,
    /// in arrival order.
    pub fn drain_events(&mut self) -> Vec<Value> {
        self.pending_events.drain(..).collect()
    }

    /// The underlying supervised process, for termination/signal control.
    pub fn process_mut(&mut self) -> &mut ManagedProcess {
        &mut self.process
    }
}

fn is_response_for(value: &Value, id: &str) -> bool {
    value.get("type").and_then(Value::as_str) == Some("response")
        && value.get("id").and_then(Value::as_str) == Some(id)
}

fn parse_response(value: &Value) -> RpcResponse {
    RpcResponse {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        command: value
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        success: value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        data: value.get("data").cloned().unwrap_or(Value::Null),
        error: value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

// --------------------------------------------------------- command builders

/// `case "prompt": { const H = await kI1(A, E.message, E.streamingBehavior) }`.
#[must_use]
pub fn prompt_command(message: &str) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("message".to_string(), Value::String(message.to_string()));
    params
}

/// `case "steer": { await A.steer(E.message, E.images) }`.
#[must_use]
pub fn steer_command(message: &str) -> Map<String, Value> {
    prompt_command(message)
}

/// `case "follow_up": { await A.followUp(E.message, E.images) }`.
#[must_use]
pub fn follow_up_command(message: &str) -> Map<String, Value> {
    prompt_command(message)
}

/// `case "abort": { await A.abort({ reason: Yj }) }` -- no caller-supplied
/// parameters.
#[must_use]
pub fn abort_command() -> Map<String, Value> {
    Map::new()
}

/// `case "get_state": { ... }` -- no parameters.
#[must_use]
pub fn get_state_command() -> Map<String, Value> {
    Map::new()
}

/// `case "get_messages": { ... }` -- no parameters.
#[must_use]
pub fn get_messages_command() -> Map<String, Value> {
    Map::new()
}

/// `case "get_session_stats"`-equivalent aggregate usage query -- no
/// parameters (the real dispatcher's exact case label for this was not
/// captured verbatim; the request/response shape was, via a direct probe:
/// `{"type":"get_session_stats","id":"1"}` -> `data.tokens.{input,output,
/// ...}`, `data.cost`).
#[must_use]
pub fn get_session_stats_command() -> Map<String, Value> {
    Map::new()
}

/// `case "get_subagents": { ... return u(m, "get_subagents", { subagents:
/// z.getSubagents() }) }` -- no parameters.
#[must_use]
pub fn get_subagents_command() -> Map<String, Value> {
    Map::new()
}

/// `switch_session": { const w = !await A.switchSession(j.sessionPath) }`.
#[must_use]
pub fn switch_session_command(session_path: &str) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert(
        "sessionPath".to_string(),
        Value::String(session_path.to_string()),
    );
    params
}

/// `case "set_model": { ... H.find((T) => T.provider === E.provider &&
/// T.id === E.modelId) ... }`.
#[must_use]
pub fn set_model_command(provider: &str, model_id: &str) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("provider".to_string(), Value::String(provider.to_string()));
    params.insert("modelId".to_string(), Value::String(model_id.to_string()));
    params
}

/// `case "set_subagent_subscription": { ... if (!uNw(E.level)) ...
/// z.setSubscriptionLevel(E.level) }`. The exact enum values `uNw`
/// validates against were not recoverable from the installed binary's
/// stripped symbol names; `"full"` is this adapter's own choice for
/// "subscribe to everything", consistent with the field name and the
/// dispatcher's boolean accept/reject shape.
#[must_use]
pub fn set_subagent_subscription_command(level: &str) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert("level".to_string(), Value::String(level.to_string()));
    params
}

/// One host tool definition registered with OMP so the vendor's own
/// model can invoke it without a second MCP subprocess, preserving
/// identical schemas/authorization (plan Task 6 Interfaces: "host
/// tools"). Grounded against the installed binary's own `fNw`
/// tool-normalization function: `name`/`description` must be non-empty,
/// `parameters` must be a JSON Schema object (never an array); `label`
/// defaults to `name` and `hidden` defaults to `false` when omitted.
#[derive(Debug, Clone)]
pub struct HostToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub label: Option<String>,
    pub hidden: bool,
}

impl HostToolDefinition {
    fn to_wire(&self) -> Value {
        let mut tool = Map::new();
        tool.insert("name".to_string(), Value::String(self.name.clone()));
        tool.insert(
            "description".to_string(),
            Value::String(self.description.clone()),
        );
        tool.insert("parameters".to_string(), self.parameters.clone());
        if let Some(label) = &self.label {
            tool.insert("label".to_string(), Value::String(label.clone()));
        }
        tool.insert("hidden".to_string(), Value::Bool(self.hidden));
        Value::Object(tool)
    }
}

/// `case "set_host_tools": { const H = fNw(E.tools); const D =
/// h.setTools(H); await A.refreshRpcHostTools(D); return u(m,
/// "set_host_tools", { toolNames: H.map((T) => T.name) }) }`.
#[must_use]
pub fn set_host_tools_command(tools: &[HostToolDefinition]) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert(
        "tools".to_string(),
        Value::Array(tools.iter().map(HostToolDefinition::to_wire).collect()),
    );
    params
}

/// One host URI scheme registered so `read`'s internal-URI resolution
/// recognizes it (plan Task 6 Interfaces: "host URI schemes"). Grounded
/// against the installed binary's own `setSchemes`: `scheme` must match
/// `^[a-z][a-z0-9+.-]*$` (lowercased); `description`/`writable`/
/// `immutable` are optional and default to unset/`false`.
#[derive(Debug, Clone)]
pub struct HostUriScheme {
    pub scheme: String,
    pub description: Option<String>,
    pub writable: bool,
    pub immutable: bool,
}

impl HostUriScheme {
    fn to_wire(&self) -> Value {
        let mut scheme = Map::new();
        scheme.insert("scheme".to_string(), Value::String(self.scheme.clone()));
        if let Some(description) = &self.description {
            scheme.insert(
                "description".to_string(),
                Value::String(description.clone()),
            );
        }
        scheme.insert("writable".to_string(), Value::Bool(self.writable));
        scheme.insert("immutable".to_string(), Value::Bool(self.immutable));
        Value::Object(scheme)
    }
}

/// `case "set_host_uri_schemes": { try { const H = W.setSchemes(E.schemes);
/// return u(m, "set_host_uri_schemes", { schemes: H }) } catch (H) { ... }
/// }`.
#[must_use]
pub fn set_host_uri_schemes_command(schemes: &[HostUriScheme]) -> Map<String, Value> {
    let mut params = Map::new();
    params.insert(
        "schemes".to_string(),
        Value::Array(schemes.iter().map(HostUriScheme::to_wire).collect()),
    );
    params
}

/// The ordered list of `(command, params)` pairs [`super::OmpRpcAdapter`]
/// sends to start one run, all established before work begins:
/// `set_subagent_subscription` (only if `subscribe_subagents` is true),
/// then `set_host_tools` (only if `host_tools` is non-empty), then
/// `set_host_uri_schemes` (only if `host_uri_schemes` is non-empty), and
/// finally `prompt` -- proving every startup command precedes the prompt
/// without depending on a live process.
#[must_use]
pub fn build_startup_commands(
    subscribe_subagents: bool,
    host_tools: &[HostToolDefinition],
    host_uri_schemes: &[HostUriScheme],
    prompt: &str,
) -> Vec<(String, Map<String, Value>)> {
    let mut commands = Vec::new();
    if subscribe_subagents {
        commands.push((
            "set_subagent_subscription".to_string(),
            set_subagent_subscription_command("full"),
        ));
    }
    if !host_tools.is_empty() {
        commands.push((
            "set_host_tools".to_string(),
            set_host_tools_command(host_tools),
        ));
    }
    if !host_uri_schemes.is_empty() {
        commands.push((
            "set_host_uri_schemes".to_string(),
            set_host_uri_schemes_command(host_uri_schemes),
        ));
    }
    commands.push(("prompt".to_string(), prompt_command(prompt)));
    commands
}
