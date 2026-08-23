//! `crewd coordination-mcp`: the stdio Model Context Protocol server a
//! supervised vendor process (or its own MCP-launching child) speaks to
//! reach the worker coordination tools -- see [`super::mcp_protocol`] for
//! the tool schemas and translation, and this module's `run` for the
//! process itself: read `CREW_WORKER_SCOPE_TOKEN` from the inherited
//! environment and remove it immediately, connect back to the owner-only
//! repository socket authenticated as `workerMcp`, then proxy MCP
//! `initialize`/`tools/list`/`tools/call` on stdio to the corresponding
//! `coordination/*` JSON-RPC call over that connection.
//!
//! Never reads the SQLite database directly -- every operation goes
//! through the authenticated socket connection, exactly as any other
//! `workerMcp` client would.

use std::path::PathBuf;
use std::time::Duration;

use batman_protocol::{
    ClientAuth, ClientCapabilities, ClientInfo, InitializeParams, InitializeResult,
    RepositoryIdentity, RunId, TaskId, WorkerId, error_code,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::paths::RuntimePaths;

/// The environment variable a supervised vendor process's own environment
/// carries the reconnect credential in. Read and removed from this
/// process's own environment before the socket connects -- never
/// forwarded to anything this process might itself spawn (it spawns
/// nothing).
pub const SCOPE_TOKEN_ENV_VAR: &str = "CREW_WORKER_SCOPE_TOKEN";

/// How long the initial socket connection retries an `InvalidToken`-shaped
/// `initialize` rejection before giving up. A token reserved by
/// [`super::ScopeTokenStore::reserve_token`] is deliberately unverifiable
/// until [`super::ScopeTokenStore::bind`] activates it with the vendor's
/// real pid -- unavoidably *after* that vendor process (and therefore,
/// potentially, this MCP subprocess) has already started. This bridges
/// that unavoidable startup race without weakening the check itself: a
/// token that is genuinely wrong, expired, or outside ancestry still
/// fails, just after this same bounded wait rather than instantly.
const BIND_RACE_RETRY_TOTAL: Duration = Duration::from_secs(2);
const BIND_RACE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// Errors serving `coordination-mcp`.
#[derive(Debug, thiserror::Error)]
pub enum McpProxyError {
    #[error("CREW_WORKER_SCOPE_TOKEN is not set in the environment")]
    MissingScopeToken,
    #[error("resolving repository paths: {0}")]
    Paths(#[from] crate::paths::PathError),
    #[error("connecting to the runtime socket at {path}: {source}")]
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the runtime socket closed before completing initialize")]
    ClosedDuringInitialize,
    #[error("initialize was rejected: {0}")]
    InitializeRejected(String),
    #[error(
        "initialize returned scopedRunId {returned}, which does not match \
         the --run-id {expected} this process was launched with"
    )]
    RunIdMismatch { expected: RunId, returned: String },
    #[error("reading a line from the runtime socket: {0}")]
    SocketRead(std::io::Error),
    #[error("writing to the runtime socket: {0}")]
    SocketWrite(std::io::Error),
    #[error("reading a line from stdin: {0}")]
    StdinRead(std::io::Error),
    #[error("writing to stdout: {0}")]
    StdoutWrite(std::io::Error),
}

/// Where `run` reads `CREW_WORKER_SCOPE_TOKEN` from and removes it --
/// injectable so tests never need a real process environment.
pub trait ScopeTokenSource {
    fn take_scope_token(&self) -> Option<String>;
}

/// The real source: the current process's own environment.
pub struct ProcessEnvironment;

impl ScopeTokenSource for ProcessEnvironment {
    fn take_scope_token(&self) -> Option<String> {
        let token = std::env::var(SCOPE_TOKEN_ENV_VAR).ok();
        // SAFETY: single-threaded at process startup, before any other
        // task has been spawned; matches the documented removal contract.
        unsafe {
            std::env::remove_var(SCOPE_TOKEN_ENV_VAR);
        }
        token
    }
}

/// Runs the proxy to completion: connect and authenticate (with the
/// bind-race retry), then serve stdio until stdin closes or the socket
/// disconnects.
///
/// # Errors
/// Returns [`McpProxyError`] for any failure -- a missing scope token,
/// a rejected `initialize`, a `scopedRunId` mismatch, or an I/O failure
/// on either the socket or stdio.
pub async fn run(
    state_dir: &std::path::Path,
    repo: &std::path::Path,
    run_id: RunId,
    token_source: &dyn ScopeTokenSource,
) -> Result<(), McpProxyError> {
    let scope_token = token_source
        .take_scope_token()
        .ok_or(McpProxyError::MissingScopeToken)?;
    let paths = RuntimePaths::resolve(state_dir, repo)?;
    let repository = RepositoryIdentity {
        canonical_path: paths.root.display().to_string(),
        vcs_root: paths.root.display().to_string(),
    };

    let socket = connect_and_authenticate(&paths.socket, &repository, run_id, &scope_token).await?;
    let stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    serve_stdio(socket, stdin, stdout).await
}

/// One line-oriented connection to the runtime socket, already past
/// `initialize`. `task_id`/`worker_id` are this proxy's own bound
/// identity, learned from `initialize`'s response -- never generated or
/// guessed -- and reused for every tool call this connection makes.
struct SocketConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: i64,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
}

impl SocketConnection {
    fn scope(&self) -> super::mcp_protocol::BoundScope {
        super::mcp_protocol::BoundScope {
            run_id: self.run_id,
            task_id: self.task_id,
            worker_id: self.worker_id,
        }
    }

    /// Sends one JSON-RPC request and reads frames until a response
    /// carrying the same id arrives, skipping any notification (e.g. an
    /// `events/event` push) in between.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, McpProxyError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.write_line(&request).await?;
        loop {
            let line = self.read_line().await?;
            let message: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Ok(
                    json!({ "__error": true, "code": error["code"], "message": error["message"] }),
                );
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn write_line(&mut self, value: &Value) -> Result<(), McpProxyError> {
        let mut line =
            serde_json::to_string(value).expect("a constructed JSON value always serializes");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(McpProxyError::SocketWrite)
    }

    async fn read_line(&mut self) -> Result<String, McpProxyError> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(McpProxyError::SocketRead)?;
        if read == 0 {
            return Err(McpProxyError::ClosedDuringInitialize);
        }
        Ok(line)
    }
}

/// Connects to `socket_path` and performs `initialize` as `workerMcp`,
/// retrying only an `InvalidToken`-shaped rejection (the bind-race
/// window) up to [`BIND_RACE_RETRY_TOTAL`]. Every other rejection --
/// outside ancestry, no credential store, a malformed result, or a
/// `scopedRunId` mismatch -- returns immediately: none of those are
/// transient, and masking them behind a multi-second retry would only
/// delay a real failure, not fix one.
async fn connect_and_authenticate(
    socket_path: &std::path::Path,
    repository: &RepositoryIdentity,
    run_id: RunId,
    scope_token: &str,
) -> Result<SocketConnection, McpProxyError> {
    let deadline = tokio::time::Instant::now() + BIND_RACE_RETRY_TOTAL;
    loop {
        match try_connect_and_authenticate(socket_path, repository, run_id, scope_token).await {
            Ok(connection) => return Ok(connection),
            Err(McpProxyError::InitializeRejected(message))
                if is_invalid_token_rejection(&message)
                    && tokio::time::Instant::now() < deadline =>
            {
                tracing::debug!(
                    message,
                    "invalid/not-yet-bound token, retrying (possible bind race)"
                );
                tokio::time::sleep(BIND_RACE_RETRY_INTERVAL).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Whether an `initialize` rejection message matches
/// `batman_runtime::ipc::VerifyError::InvalidToken`'s exact `Display`
/// text -- the *only* rejection reason this proxy ever retries. A
/// reserved-but-not-yet-bound token (see `ScopeTokenStore::reserve_token`)
/// is indistinguishable, from the client's side, from a genuinely wrong
/// or expired one; both produce this exact message. Every other
/// rejection reason (`NoCredentialStore`, `OutsideAncestry`, `RunNotLive`)
/// is never transient and must fail immediately.
fn is_invalid_token_rejection(message: &str) -> bool {
    message.contains("invalid or expired scope token")
}

async fn try_connect_and_authenticate(
    socket_path: &std::path::Path,
    repository: &RepositoryIdentity,
    run_id: RunId,
    scope_token: &str,
) -> Result<SocketConnection, McpProxyError> {
    let stream =
        UnixStream::connect(socket_path)
            .await
            .map_err(|source| McpProxyError::Connect {
                path: socket_path.to_path_buf(),
                source,
            })?;
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    let params = InitializeParams {
        client: ClientInfo {
            name: "@nikolasd/crew-coordination-mcp".to_string(),
            version: crate::VERSION.to_string(),
        },
        supported: batman_protocol::VersionRange {
            min: batman_protocol::ProtocolVersion::new(1, 0),
            max: batman_protocol::ProtocolVersion::new(1, 0),
        },
        repository: repository.clone(),
        auth: ClientAuth::WorkerMcp {
            instance_id: "coordination-mcp".to_string(),
            scope_token: scope_token.to_string(),
        },
        capabilities: ClientCapabilities {
            event_replay: false,
            max_frame_bytes: crate::ipc::PROTOCOL_MIN_FRAME_BYTES,
        },
        last_sequence: None,
    };
    let request = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": serde_json::to_value(&params).expect("InitializeParams always serializes"),
    });
    let mut request_line =
        serde_json::to_string(&request).expect("a constructed JSON value always serializes");
    request_line.push('\n');
    writer
        .write_all(request_line.as_bytes())
        .await
        .map_err(McpProxyError::SocketWrite)?;

    let mut line = String::new();
    let read = reader
        .read_line(&mut line)
        .await
        .map_err(McpProxyError::SocketRead)?;
    if read == 0 {
        return Err(McpProxyError::ClosedDuringInitialize);
    }
    let response: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    if let Some(error) = response.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("initialize failed");
        return Err(McpProxyError::InitializeRejected(message.to_string()));
    }
    let result: InitializeResult =
        serde_json::from_value(response["result"].clone()).map_err(|err| {
            McpProxyError::InitializeRejected(format!("malformed initialize result: {err}"))
        })?;

    let (task_id, worker_id) = match (
        result.principal.scoped_run_id,
        result.principal.scoped_task_id,
        result.principal.scoped_worker_id,
    ) {
        (Some(scoped_run), Some(task_id), Some(worker_id)) if scoped_run == run_id => {
            (task_id, worker_id)
        }
        (Some(scoped_run), _, _) => {
            return Err(McpProxyError::RunIdMismatch {
                expected: run_id,
                returned: scoped_run.to_string(),
            });
        }
        _ => {
            return Err(McpProxyError::InitializeRejected(
                "initialize succeeded but returned no scoped worker-mcp identity".to_string(),
            ));
        }
    };

    Ok(SocketConnection {
        reader,
        writer,
        next_id: 1,
        run_id,
        task_id,
        worker_id,
    })
}

/// The main stdio loop: reads one MCP JSON-RPC message per line from
/// `stdin`, dispatches it, and writes one response per line to `stdout`
/// (never for a notification, which by JSON-RPC 2.0 convention expects
/// none). Returns once `stdin` reaches EOF.
async fn serve_stdio(
    mut socket: SocketConnection,
    mut stdin: impl AsyncBufRead + Unpin,
    mut stdout: impl AsyncWrite + Unpin,
) -> Result<(), McpProxyError> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = stdin
            .read_line(&mut line)
            .await
            .map_err(McpProxyError::StdinRead)?;
        if read == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(message) => message,
            Err(_) => continue, // a malformed frame is skipped, never fatal.
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        let Some(id) = id else {
            // A notification (`notifications/initialized`, etc.) never
            // gets a reply.
            continue;
        };

        let response = dispatch_mcp_request(&mut socket, method, &params, id).await;
        let mut out =
            serde_json::to_string(&response).expect("a constructed JSON value always serializes");
        out.push('\n');
        stdout
            .write_all(out.as_bytes())
            .await
            .map_err(McpProxyError::StdoutWrite)?;
        stdout.flush().await.map_err(McpProxyError::StdoutWrite)?;
    }
}

async fn dispatch_mcp_request(
    socket: &mut SocketConnection,
    method: &str,
    params: &Value,
    id: Value,
) -> Value {
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "crew-coordination", "version": crate::VERSION },
            },
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": super::mcp_protocol::tool_specs().into_iter().map(|spec| json!({
                    "name": spec.name,
                    "description": spec.description,
                    "inputSchema": spec.input_schema,
                    "outputSchema": spec.output_schema,
                })).collect::<Vec<_>>(),
            },
        }),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match super::mcp_protocol::translate_tool_call(name, &arguments, socket.scope()) {
                Ok((socket_method, socket_params)) => match socket
                    .call(socket_method, socket_params)
                    .await
                {
                    Ok(value) if value.get("__error").is_some() => {
                        let message = value["message"]
                            .as_str()
                            .unwrap_or("coordination call failed");
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": super::mcp_protocol::tool_result_from_error(message),
                        })
                    }
                    Ok(value) => {
                        let result = super::mcp_protocol::tool_result_from_success(name, &value)
                            .unwrap_or_else(|err| {
                                super::mcp_protocol::tool_result_from_error(&err.to_string())
                            });
                        json!({ "jsonrpc": "2.0", "id": id, "result": result })
                    }
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": error_code::INTERNAL_ERROR, "message": err.to_string() },
                    }),
                },
                Err(err) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": super::mcp_protocol::tool_result_from_error(&err.to_string()),
                }),
            }
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": error_code::METHOD_NOT_FOUND, "message": format!("unknown method {other:?}") },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedToken(Option<String>);

    impl ScopeTokenSource for FixedToken {
        fn take_scope_token(&self) -> Option<String> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn run_fails_fast_when_the_scope_token_env_var_is_absent() {
        let err = run(
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp"),
            RunId::new(),
            &FixedToken(None),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, McpProxyError::MissingScopeToken));
    }

    #[tokio::test]
    async fn process_environment_reads_and_removes_the_scope_token() {
        // SAFETY: test-only, single-threaded within this test's own scope;
        // no other test in this binary reads/writes this exact var name.
        unsafe {
            std::env::set_var(SCOPE_TOKEN_ENV_VAR, "a-real-token");
        }
        let source = ProcessEnvironment;
        assert_eq!(source.take_scope_token(), Some("a-real-token".to_string()));
        assert!(std::env::var(SCOPE_TOKEN_ENV_VAR).is_err());
        // A second read finds nothing left behind.
        assert_eq!(source.take_scope_token(), None);
    }
}
