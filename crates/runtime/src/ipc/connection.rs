//! A single accepted connection: one reader loop plus one serialized writer
//! task, the bounded `initialize` handshake, and role-scoped dispatch.
//!
//! The reader never holds a database transaction across socket I/O -- it
//! awaits the [`crate::db::DatabaseHandle`] actor, which commits before
//! replying. Outbound frames are serialized through a single writer task so a
//! subscription's event notifications can never interleave mid-frame with a
//! request response.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use batman_protocol::{
    BatmanMethod, ClientAuth, ClientPrincipalSummary, EVENTS_EVENT_METHOD, EventEnvelope,
    EventSource, InitializeParams, InitializeResult, JsonRpcNotification, MessageId, MessageKind,
    ProtocolVersion, RunId, RuntimeCapabilities, RuntimeEvent, RuntimeInfo, RuntimeStatus, TaskId,
    WorkerId, error_code,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{broadcast, mpsc};
use tokio_util::codec::{Framed, FramedParts, LinesCodec};

use super::server::{ConnContext, Shared};
use super::{
    ClientPrincipal, PROTOCOL_MIN_FRAME_BYTES, RUNTIME_PROTOCOL_VERSION, SCHEMA_VERSION,
    runtime_supported_versions,
};

/// A message for the serialized writer task.
enum WriterMsg {
    /// A single NDJSON frame (without the trailing newline).
    Frame(String),
    /// Update the outbound frame-size limit to the negotiated maximum.
    SetMax(usize),
    /// Flush and close the writer.
    Shutdown,
}

/// Handles one accepted, admitted connection to completion.
pub(crate) async fn handle(stream: UnixStream, ctx: ConnContext, shared: Arc<Shared>) {
    let bootstrap_max = shared.config.runtime_max_frame_bytes as usize;

    let (read_half, write_half) = stream.into_split();
    let (writer_tx, writer_rx) = mpsc::channel::<WriterMsg>(64);
    let writer_task = tokio::spawn(writer_loop(write_half, writer_rx, bootstrap_max));

    let mut framed = Framed::new(read_half, LinesCodec::new_with_max_length(bootstrap_max));

    // ---- bootstrap: read frames until a successful `initialize`. ----
    let (principal, negotiated_version, negotiated_frame) = loop {
        let line = match framed.next().await {
            Some(Ok(line)) => line,
            Some(Err(err)) => {
                tracing::warn!(error = %err, "closing connection: invalid bootstrap frame");
                let _ = writer_tx.send(WriterMsg::Shutdown).await;
                let _ = writer_task.await;
                return;
            }
            None => {
                let _ = writer_tx.send(WriterMsg::Shutdown).await;
                let _ = writer_task.await;
                return;
            }
        };

        match handle_bootstrap(&line, &ctx, &shared).await {
            Bootstrap::Initialized {
                id,
                result,
                principal,
                frame,
                version,
            } => {
                let _ = writer_tx.send(WriterMsg::Frame(success(&id, result))).await;
                break (principal, version, frame);
            }
            Bootstrap::Reply { id, code, message } => {
                let _ = writer_tx
                    .send(WriterMsg::Frame(error(&id, code, &message)))
                    .await;
                // Stay open so the client may retry `initialize`.
            }
        }
    };

    // ---- switch both directions to the negotiated frame size. ----
    let _ = writer_tx.send(WriterMsg::SetMax(negotiated_frame)).await;
    let parts = framed.into_parts();
    let mut new_parts =
        FramedParts::new::<String>(parts.io, LinesCodec::new_with_max_length(negotiated_frame));
    new_parts.read_buf = parts.read_buf;
    let mut framed = Framed::from_parts(new_parts);

    // ---- initialized: dispatch until close. ----
    loop {
        let line = match framed.next().await {
            Some(Ok(line)) => line,
            Some(Err(err)) => {
                tracing::warn!(error = %err, "closing connection: frame exceeded negotiated maximum");
                break;
            }
            None => break,
        };

        let frame = dispatch(&line, &principal, negotiated_version, &shared, &writer_tx).await;
        if writer_tx.send(WriterMsg::Frame(frame)).await.is_err() {
            break;
        }
    }

    let _ = writer_tx.send(WriterMsg::Shutdown).await;
    let _ = writer_task.await;
}

/// The outcome of processing one bootstrap-phase frame.
enum Bootstrap {
    Initialized {
        id: Value,
        result: Value,
        principal: ClientPrincipal,
        frame: usize,
        version: ProtocolVersion,
    },
    Reply {
        id: Value,
        code: i32,
        message: String,
    },
}

async fn handle_bootstrap(line: &str, ctx: &ConnContext, shared: &Arc<Shared>) -> Bootstrap {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            return Bootstrap::Reply {
                id: Value::Null,
                code: error_code::PARSE_ERROR,
                message: "request is not valid JSON".to_string(),
            };
        }
    };

    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");

    if method != "initialize" {
        return Bootstrap::Reply {
            id,
            code: error_code::NOT_INITIALIZED,
            message: "the connection must call `initialize` before any other method".to_string(),
        };
    }

    let params_value = message.get("params").cloned().unwrap_or(Value::Null);
    let params: InitializeParams = match serde_json::from_value(params_value) {
        Ok(params) => params,
        Err(err) => {
            return Bootstrap::Reply {
                id,
                code: error_code::INVALID_PARAMS,
                message: format!("invalid initialize params: {err}"),
            };
        }
    };

    // Protocol-version negotiation.
    let runtime = RUNTIME_PROTOCOL_VERSION;
    if !(params.supported.min <= runtime && runtime <= params.supported.max) {
        let supported = runtime_supported_versions();
        return Bootstrap::Reply {
            id,
            code: error_code::INCOMPATIBLE_VERSION,
            message: format!(
                "no overlapping protocol version: client supports {}.{}-{}.{}, runtime supports {}.{}-{}.{}",
                params.supported.min.major,
                params.supported.min.minor,
                params.supported.max.major,
                params.supported.max.minor,
                supported.min.major,
                supported.min.minor,
                supported.max.major,
                supported.max.minor,
            ),
        };
    }

    // Frame-size negotiation.
    let offer = params.capabilities.max_frame_bytes;
    if offer < PROTOCOL_MIN_FRAME_BYTES {
        return Bootstrap::Reply {
            id,
            code: error_code::INVALID_PARAMS,
            message: format!(
                "maxFrameBytes {offer} is below the protocol minimum of {PROTOCOL_MIN_FRAME_BYTES}"
            ),
        };
    }
    let negotiated_frame = offer.min(shared.config.runtime_max_frame_bytes);

    // Authenticate the client into a ClientPrincipal from its ClientAuth.
    let principal = match authenticate(&params.auth, ctx, shared) {
        Ok(principal) => principal,
        Err(message) => {
            return Bootstrap::Reply {
                id,
                code: error_code::INVALID_PARAMS,
                message,
            };
        }
    };

    // Compute the next sequence the client should expect. A single indexed
    // MAX(sequence) read -- never a full event-log replay -- so `initialize`
    // stays O(1) regardless of journal size.
    let next_sequence = match shared.db.max_sequence().await {
        Ok(max) => max.unwrap_or(0) + 1,
        Err(err) => {
            return Bootstrap::Reply {
                id,
                code: error_code::INTERNAL_ERROR,
                message: format!("failed to read event tip: {err}"),
            };
        }
    };

    let result = InitializeResult {
        runtime: RuntimeInfo {
            name: "crew-runtime".to_string(),
            version: crate::VERSION.to_string(),
        },
        negotiated: runtime,
        project_id: shared.project_id,
        principal: ClientPrincipalSummary {
            role: principal.role,
            instance_id: principal.instance_id.clone(),
            scoped_run_id: principal.scoped_run_id,
            scoped_task_id: principal.scoped_task_id,
            scoped_worker_id: principal.scoped_worker_id,
        },
        allowed_methods: principal.allowed_methods(),
        capabilities: RuntimeCapabilities {
            max_frame_bytes: negotiated_frame,
            peer_credentials_verified: ctx.peer_credentials_verified,
        },
        next_sequence,
    };

    let result =
        serde_json::to_value(&result).expect("InitializeResult is a plain, serializable wire type");

    Bootstrap::Initialized {
        id,
        result,
        principal,
        frame: negotiated_frame as usize,
        version: runtime,
    }
}

/// Turns a validated [`ClientAuth`] into a [`ClientPrincipal`], applying the
/// role-specific admission checks. Returns a human-readable message on
/// failure (mapped to `INVALID_PARAMS` by the caller).
fn authenticate(
    auth: &ClientAuth,
    ctx: &ConnContext,
    shared: &Arc<Shared>,
) -> Result<ClientPrincipal, String> {
    match auth {
        ClientAuth::OmpExtension {
            instance_id,
            agent_directory,
        } => {
            validate_agent_directory(agent_directory, shared.config.euid)?;
            Ok(ClientPrincipal {
                role: batman_protocol::ClientRole::OmpExtension,
                instance_id: instance_id.clone(),
                scoped_run_id: None,
                scoped_task_id: None,
                scoped_worker_id: None,
            })
        }
        ClientAuth::Display { instance_id } => Ok(ClientPrincipal {
            role: batman_protocol::ClientRole::Display,
            instance_id: instance_id.clone(),
            scoped_run_id: None,
            scoped_task_id: None,
            scoped_worker_id: None,
        }),
        ClientAuth::WorkerMcp {
            instance_id,
            scope_token,
        } => {
            let scoped = shared
                .config
                .worker_verifier
                .verify(scope_token, ctx.peer_pid)
                .map_err(|err| format!("worker credential rejected: {err}"))?;
            Ok(ClientPrincipal {
                role: batman_protocol::ClientRole::WorkerMcp,
                instance_id: instance_id.clone(),
                scoped_run_id: Some(scoped.run_id),
                scoped_task_id: Some(scoped.task_id),
                scoped_worker_id: Some(scoped.worker_id),
            })
        }
    }
}

/// An ompExtension agent directory must be an absolute path that exists,
/// canonicalizes cleanly, and is owned by the current user.
fn validate_agent_directory(dir: &str, euid: u32) -> Result<(), String> {
    let path = Path::new(dir);
    if !path.is_absolute() {
        return Err(format!("agentDirectory {dir:?} must be an absolute path"));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|err| format!("agentDirectory {dir:?} does not resolve: {err}"))?;
    let metadata = std::fs::metadata(&canonical)
        .map_err(|err| format!("agentDirectory {dir:?} cannot be inspected: {err}"))?;
    if metadata.uid() != euid {
        return Err(format!(
            "agentDirectory {dir:?} is owned by uid {}, not the current uid {euid}",
            metadata.uid()
        ));
    }
    Ok(())
}

/// Dispatches one initialized-phase request against the principal's method
/// table, returning the serialized response frame.
async fn dispatch(
    line: &str,
    principal: &ClientPrincipal,
    negotiated_version: ProtocolVersion,
    shared: &Arc<Shared>,
    writer_tx: &mpsc::Sender<WriterMsg>,
) -> String {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            return error(
                &Value::Null,
                error_code::PARSE_ERROR,
                "request is not valid JSON",
            );
        }
    };

    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method_name = message.get("method").and_then(Value::as_str).unwrap_or("");

    // Resolve the method from the authenticated principal's table only.
    // Unknown or out-of-role methods are hidden as METHOD_NOT_FOUND.
    let method: Option<BatmanMethod> = serde_json::from_value(json!(method_name)).ok();
    let allowed = method.is_some_and(|m| principal.allowed_methods().contains(&m));
    if !allowed {
        return error(
            &id,
            error_code::METHOD_NOT_FOUND,
            &format!("method {method_name:?} is not available to this client"),
        );
    }

    match method.expect("allowed implies a known method") {
        BatmanMethod::RuntimeStatus => {
            let status = RuntimeStatus {
                running: true,
                protocol: negotiated_version,
                project_id: shared.project_id,
                // The live adapter count from the run driver (R87) -- never
                // a placeholder: /crew-status and crewd status report
                // this, and the idle-shutdown decision consumes the same
                // source.
                active_runs: shared.active_run_count() as u32,
                schema_version: SCHEMA_VERSION,
                protocol_healthy: is_protocol_healthy(negotiated_version),
                uptime_seconds: shared.started_at.elapsed().as_secs(),
                binary_source: shared.config.binary_source,
            };
            let value = serde_json::to_value(&status)
                .expect("RuntimeStatus is a plain, serializable wire type");
            success(&id, value)
        }
        BatmanMethod::EventsReplay => {
            let after = message
                .get("params")
                .and_then(|p| p.get("afterSequence"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            match replay(shared, after).await {
                Ok(events) => success(&id, events),
                Err(message) => error(&id, error_code::INTERNAL_ERROR, &message),
            }
        }
        BatmanMethod::EventsSubscribe => {
            spawn_subscription(shared, writer_tx.clone());
            success(&id, json!({ "active": true }))
        }
        BatmanMethod::RuntimeShutdown => {
            // Role-gated to ompExtension (see `ClientPrincipal::allowed_methods`).
            // Arbitrated (R82): stopping the daemon stops it for every
            // connected instance, so refuse while other work is live unless
            // the caller explicitly forces it. `active_connections` includes
            // this connection, hence `> 1`.
            let force = message
                .get("params")
                .and_then(|p| p.get("force"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let runs = shared.active_run_count();
            let connections = shared
                .active_connections
                .load(std::sync::atomic::Ordering::Relaxed);
            if !force && (runs > 0 || connections > 1) {
                return error(
                    &id,
                    error_code::INVALID_PARAMS,
                    &format!(
                        "refusing shutdown: {runs} active run(s) and {} other live connection(s); \
                         pass force: true to stop the daemon for every connected instance",
                        connections.saturating_sub(1)
                    ),
                );
            }
            if force {
                tracing::warn!(
                    active_runs = runs,
                    other_connections = connections.saturating_sub(1),
                    "runtime/shutdown forced while work was live"
                );
            }
            // Trigger a graceful shutdown of the accept loop; the serve driver
            // then journals the stop record and removes the socket. The
            // success frame is queued to the writer before the loop tears
            // down, so a cooperative client still sees its acknowledgement.
            shared.shutdown.notify_one();
            success(&id, json!({ "stopping": true }))
        }
        BatmanMethod::Initialize => error(
            &id,
            error_code::METHOD_NOT_FOUND,
            "the connection is already initialized",
        ),
        // Orchestration methods: routed through OrchestrationService.
        BatmanMethod::TaskUpsert
        | BatmanMethod::TaskGet
        | BatmanMethod::WorkerCreate
        | BatmanMethod::WorkerList
        | BatmanMethod::WorkerGet
        | BatmanMethod::RunSubmit
        | BatmanMethod::RunList
        | BatmanMethod::RunGet
        | BatmanMethod::RunResult
        | BatmanMethod::RunRetry
        | BatmanMethod::RunCancel
        | BatmanMethod::MessageSend
        | BatmanMethod::MessageList
        | BatmanMethod::ApprovalList
        | BatmanMethod::ApprovalDecide
        | BatmanMethod::CoordinationChildList
        | BatmanMethod::CoordinationChildDecide
        | BatmanMethod::ProfileRegister
        | BatmanMethod::ReconcileOmp
        | BatmanMethod::PolicyViolationDecide
        | BatmanMethod::PolicyViolationList => {
            let resolved = method.expect("allowed implies a known method");
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            match shared
                .orchestration
                .dispatch(resolved, principal, &params)
                .await
            {
                Ok(value) => success(&id, value),
                Err(err) => error(&id, err.code, &err.message),
            }
        }
        // Coordination methods: routed through CoordinationBroker, scoped
        // to the connection's bound run when the principal carries one.
        BatmanMethod::CoordinationTask
        | BatmanMethod::CoordinationPeers
        | BatmanMethod::CoordinationSend
        | BatmanMethod::CoordinationRequestChild
        | BatmanMethod::CoordinationPublishArtifact
        | BatmanMethod::CoordinationReportBlocked
        | BatmanMethod::CoordinationAskPolicy
        | BatmanMethod::CoordinationPeerWorkspace
        | BatmanMethod::CoordinationArtifactList
        | BatmanMethod::CoordinationArtifactFetch => {
            let resolved = method.expect("allowed implies a known method");
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            match dispatch_coordination(resolved, principal, &params, shared).await {
                Ok(value) => success(&id, value),
                Err(err) => error(&id, err.code, &err.message),
            }
        }
        // Workspace and artifact methods: routed through OrchestrationService.
        BatmanMethod::WorkspaceAcquire
        | BatmanMethod::WorkspaceGet
        | BatmanMethod::WorkspaceRelease
        | BatmanMethod::WorkspaceInspect
        | BatmanMethod::WorkspaceApply
        | BatmanMethod::ArtifactList
        | BatmanMethod::ArtifactFetch => {
            let resolved = method.expect("allowed implies a known method");
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            match shared
                .orchestration
                .dispatch(resolved, principal, &params)
                .await
            {
                Ok(value) => success(&id, value),
                Err(err) => error(&id, err.code, &err.message),
            }
        }
    }
}

/// Dispatches one worker-safe `coordination/*` method to the shared
/// [`crate::coordination::CoordinationBroker`], using the connection's
/// bound scope (`principal.scoped_run_id`) as the trusted run identity --
/// never a client-supplied one.
async fn dispatch_coordination(
    method: BatmanMethod,
    principal: &ClientPrincipal,
    params: &Value,
    shared: &Arc<Shared>,
) -> Result<Value, crate::coordination::CoordinationError> {
    let run_id = principal
        .scoped_run_id
        .ok_or_else(|| crate::coordination::CoordinationError {
            code: error_code::INVALID_PARAMS,
            message: "this connection has no bound scope".to_string(),
        })?;
    match method {
        BatmanMethod::CoordinationTask => shared.coordination.task(run_id).await,
        BatmanMethod::CoordinationPeers => shared.coordination.peers(run_id).await,
        BatmanMethod::CoordinationSend => {
            let sender_worker_id = parse_worker_field(params, "senderWorkerId")?;
            let task_id = parse_task_field(params, "taskId")?;
            if principal.scoped_worker_id != Some(sender_worker_id) {
                return Err(invalid_params(
                    "senderWorkerId does not match this connection's authenticated scope",
                ));
            }
            if principal.scoped_task_id != Some(task_id) {
                return Err(invalid_params(
                    "a run cannot address a task other than its own",
                ));
            }
            let kind = parse_message_kind_field(params)?;
            let payload = params
                .get("payload")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("payload is required"))?
                .to_string();
            let recipient_worker_id = params
                .get("recipientWorkerId")
                .and_then(Value::as_str)
                .map(WorkerId::parse)
                .transpose()
                .map_err(|_| invalid_params("recipientWorkerId is not a valid id"))?;
            let reply_to = params
                .get("replyTo")
                .and_then(Value::as_str)
                .map(MessageId::parse)
                .transpose()
                .map_err(|_| invalid_params("replyTo is not a valid id"))?;
            shared
                .coordination
                .send(
                    run_id,
                    sender_worker_id,
                    task_id,
                    kind,
                    payload,
                    recipient_worker_id,
                    reply_to,
                )
                .await
        }
        BatmanMethod::CoordinationRequestChild => {
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("reason is required"))?
                .to_string();
            shared.coordination.request_child(run_id, reason).await
        }
        BatmanMethod::CoordinationPublishArtifact => {
            let artifact_ref = params
                .get("artifactRef")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("artifactRef is required"))?
                .to_string();
            let description = params
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            shared
                .coordination
                .publish_artifact(run_id, artifact_ref, description)
                .await
        }
        BatmanMethod::CoordinationReportBlocked => {
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("reason is required"))?
                .to_string();
            shared.coordination.report_blocked(run_id, reason).await
        }
        BatmanMethod::CoordinationAskPolicy => {
            let question = params
                .get("question")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("question is required"))?
                .to_string();
            shared.coordination.ask_policy(run_id, question).await
        }
        BatmanMethod::CoordinationPeerWorkspace => {
            let peer_run_id = params
                .get("peerRunId")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_params("peerRunId is required"))?;
            let peer_run_id = RunId::parse(peer_run_id)
                .map_err(|_| invalid_params("peerRunId is not a valid id"))?;
            shared
                .coordination
                .peer_workspace(run_id, peer_run_id)
                .await
        }
        BatmanMethod::CoordinationArtifactList => {
            let kind = match params.get("kind").and_then(Value::as_str) {
                Some(raw) => Some(
                    serde_json::from_value(Value::String(raw.to_string()))
                        .map_err(|_| invalid_params("kind is not a valid artifact kind"))?,
                ),
                None => None,
            };
            shared.coordination.artifact_list(run_id, kind).await
        }
        BatmanMethod::CoordinationArtifactFetch => {
            let artifact_id = params
                .get("artifactId")
                .cloned()
                .ok_or_else(|| invalid_params("artifactId is required"))?;
            let artifact_id = serde_json::from_value(artifact_id)
                .map_err(|_| invalid_params("artifactId is not a valid id"))?;
            let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
            shared
                .coordination
                .artifact_fetch(run_id, artifact_id, offset)
                .await
        }
        _ => Err(crate::coordination::CoordinationError {
            code: error_code::METHOD_NOT_FOUND,
            message: "method is not routed through CoordinationBroker".to_string(),
        }),
    }
}

fn invalid_params(message: &str) -> crate::coordination::CoordinationError {
    crate::coordination::CoordinationError {
        code: error_code::INVALID_PARAMS,
        message: message.to_string(),
    }
}

fn parse_worker_field(
    params: &Value,
    field: &str,
) -> Result<WorkerId, crate::coordination::CoordinationError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params(&format!("{field} is required")))
        .and_then(|s| {
            WorkerId::parse(s).map_err(|_| invalid_params(&format!("{field} is not a valid id")))
        })
}

fn parse_task_field(
    params: &Value,
    field: &str,
) -> Result<TaskId, crate::coordination::CoordinationError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params(&format!("{field} is required")))
        .and_then(|s| {
            TaskId::parse(s).map_err(|_| invalid_params(&format!("{field} is not a valid id")))
        })
}

fn parse_message_kind_field(
    params: &Value,
) -> Result<MessageKind, crate::coordination::CoordinationError> {
    let raw = params
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params("kind is required"))?;
    match raw {
        "assign" => Ok(MessageKind::Assign),
        "steer" => Ok(MessageKind::Steer),
        "followUp" => Ok(MessageKind::FollowUp),
        "question" => Ok(MessageKind::Question),
        "answer" => Ok(MessageKind::Answer),
        "peerMessage" => Ok(MessageKind::PeerMessage),
        "approvalDecision" => Ok(MessageKind::ApprovalDecision),
        "cancel" => Ok(MessageKind::Cancel),
        "shutdown" => Ok(MessageKind::Shutdown),
        other => Err(invalid_params(&format!("unknown message kind {other:?}"))),
    }
}

/// Whether the negotiated protocol version lies within the runtime's declared
/// supported range (a self-check that always holds for a live session).
fn is_protocol_healthy(negotiated: ProtocolVersion) -> bool {
    let supported = runtime_supported_versions();
    supported.min <= negotiated && negotiated <= supported.max
}

/// Reads committed events with `sequence > after` and renders them as a JSON
/// array of [`EventEnvelope`]s.
async fn replay(shared: &Arc<Shared>, after: u64) -> Result<Value, String> {
    let rows = shared
        .db
        .replay_events(after)
        .await
        .map_err(|err| format!("failed to replay events: {err}"))?;

    let mut envelopes = Vec::with_capacity(rows.len());
    for row in rows {
        let event: RuntimeEvent = serde_json::from_str(&row.event_json)
            .map_err(|err| format!("stored event is not a valid RuntimeEvent: {err}"))?;
        envelopes.push(EventEnvelope {
            sequence: row.sequence,
            timestamp: row.timestamp,
            project_id: row.project_id,
            task_id: row.task_id,
            worker_id: row.worker_id,
            run_id: row.run_id,
            parent_worker_id: None,
            source: EventSource::Runtime,
            event,
            vendor_event_ref: None,
        });
    }

    Ok(serde_json::to_value(&envelopes)
        .expect("a Vec of EventEnvelope is a plain, serializable wire type"))
}

/// Registers this connection for live event notifications: forwards every
/// broadcast [`EventEnvelope`] to the writer as an `events/event`
/// notification until the connection closes.
fn spawn_subscription(shared: &Arc<Shared>, writer_tx: mpsc::Sender<WriterMsg>) {
    let events_rx = shared.events_tx.subscribe();
    tokio::spawn(forward_events(events_rx, writer_tx));
}

/// Forwards every broadcast [`EventEnvelope`] to the writer as an
/// `events/event` notification, returning as soon as either the broadcast
/// ends or the connection's writer half closes.
///
/// Split out of [`spawn_subscription`] so the shutdown behavior is
/// exercisable without constructing a full [`Shared`].
async fn forward_events(
    mut events_rx: broadcast::Receiver<EventEnvelope>,
    writer_tx: mpsc::Sender<WriterMsg>,
) {
    loop {
        let envelope = tokio::select! {
            // Reap eagerly: a closed writer half means the connection is
            // already gone, so stop now instead of waiting for the next
            // broadcast to discover it. `broadcast::Receiver::recv` is
            // cancel-safe, so losing this race drops no event.
            () = writer_tx.closed() => return,
            received = events_rx.recv() => match received {
                Ok(envelope) => envelope,
                // On a lag/closed error the loop ends; reconnect/replay is
                // the recovery path for a dropped notification.
                Err(_) => return,
            },
        };

        let params = serde_json::to_value(&envelope)
            .expect("EventEnvelope is a plain, serializable wire type");
        let notification = JsonRpcNotification::new(EVENTS_EVENT_METHOD, Some(params));
        let frame = serde_json::to_string(&notification)
            .expect("a notification of plain wire types serializes");
        if writer_tx.send(WriterMsg::Frame(frame)).await.is_err() {
            return;
        }
    }
}

/// The serialized writer task: writes NDJSON frames in order, enforcing the
/// current outbound frame-size limit in the write direction.
async fn writer_loop(mut half: OwnedWriteHalf, mut rx: mpsc::Receiver<WriterMsg>, mut max: usize) {
    while let Some(message) = rx.recv().await {
        match message {
            WriterMsg::SetMax(new_max) => max = new_max,
            WriterMsg::Shutdown => break,
            WriterMsg::Frame(line) => {
                if line.len() + 1 > max {
                    tracing::warn!(
                        frame_bytes = line.len() + 1,
                        max,
                        "refusing to write a frame above the negotiated maximum; closing"
                    );
                    break;
                }
                if half.write_all(line.as_bytes()).await.is_err()
                    || half.write_all(b"\n").await.is_err()
                    || half.flush().await.is_err()
                {
                    break;
                }
            }
        }
    }
    let _ = half.shutdown().await;
}

/// Builds a JSON-RPC success response frame echoing `id`.
fn success(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// Builds a JSON-RPC error response frame echoing `id`.
fn error(id: &Value, code: i32, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use batman_protocol::{EventSource, ProjectId, RuntimeEvent, Timestamp};

    fn envelope() -> EventEnvelope {
        EventEnvelope {
            sequence: 1,
            timestamp: Timestamp::now(),
            project_id: ProjectId::new(),
            task_id: None,
            worker_id: None,
            run_id: None,
            parent_worker_id: None,
            source: EventSource::Runtime,
            event: RuntimeEvent::RuntimeStarted,
            vendor_event_ref: None,
        }
    }

    #[tokio::test]
    async fn a_closed_writer_reaps_the_forwarder_without_waiting_for_a_broadcast() {
        let (events_tx, events_rx) = broadcast::channel(8);
        let (writer_tx, writer_rx) = mpsc::channel(8);

        let forwarder = tokio::spawn(forward_events(events_rx, writer_tx));

        // Close the connection's writer half and broadcast nothing at all.
        // Before eager reaping the forwarder parked on `recv()` indefinitely.
        drop(writer_rx);

        tokio::time::timeout(Duration::from_secs(5), forwarder)
            .await
            .expect("the forwarder must exit as soon as the writer closes")
            .expect("the forwarder must not panic");

        // The forwarder's broadcast receiver is dropped along with it.
        assert_eq!(events_tx.receiver_count(), 0);
    }

    #[tokio::test]
    async fn a_broadcast_event_still_reaches_the_writer_as_an_events_event_frame() {
        let (events_tx, events_rx) = broadcast::channel(8);
        let (writer_tx, mut writer_rx) = mpsc::channel(8);

        let forwarder = tokio::spawn(forward_events(events_rx, writer_tx));
        events_tx.send(envelope()).expect("a live receiver exists");

        let message = tokio::time::timeout(Duration::from_secs(5), writer_rx.recv())
            .await
            .expect("a broadcast event must be forwarded")
            .expect("the writer channel stays open");

        let WriterMsg::Frame(frame) = message else {
            panic!("expected a frame, got a control message");
        };
        let parsed: Value = serde_json::from_str(&frame).expect("the frame is JSON");
        assert_eq!(parsed["method"], EVENTS_EVENT_METHOD);
        assert_eq!(parsed["params"]["event"]["type"], "runtimeStarted");

        drop(writer_rx);
        let _ = forwarder.await;
    }
}
