//! The control-channel reader: the loop that actually turns claude's
//! `stream-json` stdout into normalized [`AdapterEventPayload`]s and
//! turns a `can_use_tool` control request into a reply written back to
//! its stdin -- the piece `claude_protocol::reconcile`'s `find_gaps` and
//! `claude_protocol::approval_bridge`'s whole module were built ahead
//! of, and were dead code without.
//!
//! Framing mirrors `crate::coordination::mcp::SocketConnection`'s own
//! idiom (newline-delimited JSON, no length prefix), role-reversed: that
//! type is the server side of an NDJSON socket, this is the client side
//! of an NDJSON pipe pair, but the framing itself -- one JSON value per
//! line, no other delimiter -- is identical.
//!
//! **The message shapes classified below are the same ones
//! `crate::adapter::tui::claude`'s transcript tailer already parses**
//! (`type: "assistant"` with a `message.content` block array) --
//! claude's own stream-json output and its durable transcript share one
//! wire format. What is genuinely new here, and still provisional
//! pending a live capture (this spike's own stop condition, the same
//! one `reconcile`'s and `approval_bridge`'s own module doc comments
//! carry): the `type: "control_request"`/`"control_response"` shapes
//! [`super::approval_bridge`] parses and builds, which only exist on
//! this streaming path, never in the vendor's own transcript file.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crew_protocol::{Classified, ContentClass, RunId, TaskId, WorkerId};

use crate::approval::ApprovalService;

use super::approval_bridge::{self, ProtocolApprovalCallback};
use crate::adapter::event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink};

/// The three correlated ids every emitted event and every approval this
/// turn raises must carry -- bundled so `drive_turn`'s own signature
/// does not repeat them three parameters at a time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunIdentity {
    pub(crate) run_id: RunId,
    pub(crate) task_id: TaskId,
    pub(crate) worker_id: WorkerId,
}

/// One classified `stream-json` line, reduced to what [`drive_turn`]
/// needs to act on. `Other` is deliberately one variant for every shape
/// this reader does not (yet) act on -- a `system`/`init` line, a
/// `thinking` block, a `user`-typed tool-result echo -- rather than one
/// variant per shape this module has no caller for yet: the same
/// "unknown/uninteresting collapses to one variant" choice
/// `claude_protocol::reconcile::TranscriptEntryKind::Other` already
/// makes, for the identical reason (a vendor addition should never force
/// a change here).
#[derive(Debug, PartialEq)]
enum StreamLine {
    AssistantText(String),
    ToolStarted {
        tool_call_id: String,
        name: String,
    },
    ControlRequest(serde_json::Value),
    /// `type: "result"`: claude's own turn-complete marker. The read
    /// loop stops here rather than waiting for stdout to close --
    /// waiting for EOF as well would work too (claude exits after this),
    /// but would leave a turn's own completion depending on process
    /// exit timing rather than the protocol's own explicit signal.
    TurnComplete,
    /// `type: "system", subtype: "init"`: the one message this reader
    /// reads a `session_id` off of, for
    /// [`TurnOutcome::session_id`] -- claude's own transcript file is
    /// named `<session_id>.jsonl`, and reconciliation cannot find the
    /// right file without it.
    SystemInit {
        session_id: Option<String>,
    },
    Other,
}

/// What one call to [`drive_turn`] actually observed, for its caller to
/// reconcile against the vendor's own durable transcript afterward.
#[derive(Debug, Default)]
pub(crate) struct TurnOutcome {
    /// Every entry id (`uuid`) seen on the live stream, regardless of
    /// whether this reader emitted a normalized event for it -- a
    /// `thinking` block or a `system` line contributes its id here even
    /// though neither produces an [`AdapterEventPayload`], because
    /// `claude_protocol::reconcile::find_gaps` must not call an entry
    /// this adapter genuinely saw a "gap" just because it chose not to
    /// surface it as a message.
    pub(crate) entry_ids: HashSet<String>,
    /// The vendor session id this turn ran under, if `system/init`
    /// reported one -- `None` when the stream closed before that
    /// message ever arrived (nothing to reconcile against without it).
    pub(crate) session_id: Option<String>,
}

/// Classifies one already-parsed JSON line from claude's stdout. Never
/// panics on an unexpected shape -- an unrecognized `type`, a present
/// `type` with a missing expected child field, or no `type` at all all
/// fall through to [`StreamLine::Other`], matching
/// `claude_protocol::reconcile`'s own tolerant-parse stance: a shape
/// this function does not recognize is a fact for the caller to decide
/// what to do with (here: nothing), never a crash.
fn classify_line(value: &serde_json::Value) -> StreamLine {
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("control_request") => StreamLine::ControlRequest(value.clone()),
        Some("result") => StreamLine::TurnComplete,
        Some("system")
            if value.get("subtype").and_then(serde_json::Value::as_str) == Some("init") =>
        {
            StreamLine::SystemInit {
                session_id: value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            }
        }
        Some("assistant") => {
            let Some(content) = value
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(serde_json::Value::as_array)
            else {
                return StreamLine::Other;
            };
            // The first recognized block wins: a `text` block becomes a
            // message, a `tool_use` block (absent any `text` block
            // before it) becomes a tool start. A `thinking` block is
            // skipped outright -- never surfaced as an
            // `AdapterEventPayload` at all, the same filtering this
            // module's own doc comment (quoting `event_sink.rs`'s)
            // requires of every adapter before content ever reaches the
            // sink.
            for block in content {
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(serde_json::Value::as_str) {
                            return StreamLine::AssistantText(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let id = block.get("id").and_then(serde_json::Value::as_str);
                        let name = block.get("name").and_then(serde_json::Value::as_str);
                        if let (Some(id), Some(name)) = (id, name) {
                            return StreamLine::ToolStarted {
                                tool_call_id: id.to_string(),
                                name: name.to_string(),
                            };
                        }
                    }
                    _ => {}
                }
            }
            StreamLine::Other
        }
        _ => StreamLine::Other,
    }
}

/// Reads `stdout` line by line until claude's own turn-complete marker
/// or a closed stream, normalizing what it recognizes through `sink` and
/// bridging any `can_use_tool` control request through `approval_service`/
/// `callback`, writing the reply back to `stdin`.
///
/// Returns once the turn is complete (or the stream closed) -- it never
/// loops forever on a stream that legitimately ends, and it never treats
/// EOF as an error: a process that exits after its own `result` line is
/// the ordinary case, not a fault.
///
/// # Errors
/// Propagates a stdout read failure, a stdin write failure, or
/// [`approval_bridge::handle_permission_request`]'s own error (a run no
/// longer `working`, or this adapter torn down mid-approval) as a
/// [`crate::adapter::error::AdapterError::process`]/`::protocol`.
pub(crate) async fn drive_turn<R, W>(
    stdout: R,
    mut stdin: W,
    sink: &Arc<dyn AdapterEventSink>,
    approval_service: &ApprovalService,
    callback: &ProtocolApprovalCallback,
    ids: RunIdentity,
) -> Result<TurnOutcome, crate::adapter::error::AdapterError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use crate::adapter::error::AdapterError;

    let mut outcome = TurnOutcome::default();
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|e| AdapterError::process("claude", "read_stdout", e.to_string()))?;
        let Some(line) = line else {
            // Stream closed without an explicit `result` line -- claude
            // exited (or crashed) before signalling turn completion.
            // Not this loop's own error to raise: `Adapter::start`'s
            // caller observes the process exit and reconciles against
            // the transcript regardless of how the stream ended.
            return Ok(outcome);
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            // A malformed line on claude's own stdout is a protocol
            // fact worth surfacing, not a parse error to swallow --
            // but never fatal to the turn: the next line may well be
            // well-formed.
            sink.emit(AdapterEvent {
                run_id: ids.run_id,
                task_id: ids.task_id,
                worker_id: ids.worker_id,
                payload: AdapterEventPayload::ProtocolHealthChanged {
                    healthy: false,
                    detail: Classified {
                        class: ContentClass::Visible,
                        value: "received a non-JSON line on claude's stdout".to_string(),
                    },
                },
                cursor: None,
            })
            .await?;
            continue;
        };
        // Recorded regardless of classification -- see `TurnOutcome::entry_ids`'s
        // own doc comment for why an entry crew chose not to emit an
        // event for must still count as "seen".
        if let Some(uuid) = value.get("uuid").and_then(serde_json::Value::as_str) {
            outcome.entry_ids.insert(uuid.to_string());
        }
        match classify_line(&value) {
            StreamLine::SystemInit { session_id } => {
                outcome.session_id = session_id;
            }
            StreamLine::AssistantText(text) => {
                sink.emit(AdapterEvent {
                    run_id: ids.run_id,
                    task_id: ids.task_id,
                    worker_id: ids.worker_id,
                    payload: AdapterEventPayload::MessageFinal {
                        role: "assistant".to_string(),
                        text: Classified {
                            class: ContentClass::Visible,
                            value: text,
                        },
                    },
                    cursor: None,
                })
                .await?;
            }
            StreamLine::ToolStarted { tool_call_id, name } => {
                sink.emit(AdapterEvent {
                    run_id: ids.run_id,
                    task_id: ids.task_id,
                    worker_id: ids.worker_id,
                    payload: AdapterEventPayload::ToolStarted { tool_call_id, name },
                    cursor: None,
                })
                .await?;
            }
            StreamLine::ControlRequest(request) => {
                let Some(parsed) = approval_bridge::parse_permission_request(&request) else {
                    // A control request this reader does not recognize
                    // (claude's control channel carries more request
                    // subtypes than just `can_use_tool` -- e.g.
                    // `set_model`/`interrupt` acknowledgements this
                    // spike never sends and so never expects a request
                    // for). Never guessed at: skipped, exactly like an
                    // unrecognized transcript entry is in `reconcile`.
                    continue;
                };
                let reply = approval_bridge::handle_permission_request(
                    approval_service,
                    callback,
                    ids.run_id,
                    ids.task_id,
                    parsed,
                )
                .await
                .map_err(|e| AdapterError::protocol("claude", "respond_to_approval", e))?;
                stdin
                    .write_all(&reply)
                    .await
                    .map_err(|e| AdapterError::process("claude", "write_stdin", e.to_string()))?;
            }
            StreamLine::TurnComplete => return Ok(outcome),
            StreamLine::Other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use tokio::io::AsyncReadExt;
    use tokio::sync::broadcast;

    use crew_protocol::{
        DecidedBy, ProjectId, Redacted, Run, RunFlags, RunState, TaskRef, Timestamp, Worker,
        WorkerProfileRef,
    };

    use crate::adapter::event_sink::{AdapterEvent, AdapterEventSink};
    use crate::approval::ApprovalCallback;
    use crate::db::DatabaseHandle;
    use crate::domain::DomainRepository;

    #[test]
    fn classifies_assistant_text() {
        let value = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "hi there"}]},
        });
        assert_eq!(
            classify_line(&value),
            StreamLine::AssistantText("hi there".to_string())
        );
    }

    #[test]
    fn classifies_tool_use() {
        let value = serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {}},
            ]},
        });
        assert_eq!(
            classify_line(&value),
            StreamLine::ToolStarted {
                tool_call_id: "t1".to_string(),
                name: "Read".to_string(),
            }
        );
    }

    /// A thinking block, alone, must never surface as a message -- the
    /// filtering this module's own doc comment requires.
    #[test]
    fn a_thinking_only_block_is_other() {
        let value = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "thinking", "thinking": "hmm"}]},
        });
        assert_eq!(classify_line(&value), StreamLine::Other);
    }

    #[test]
    fn classifies_the_result_marker() {
        let value = serde_json::json!({"type": "result", "subtype": "success"});
        assert_eq!(classify_line(&value), StreamLine::TurnComplete);
    }

    #[test]
    fn an_unrecognized_type_is_other() {
        let value = serde_json::json!({"type": "queued_command", "command": "noop"});
        assert_eq!(classify_line(&value), StreamLine::Other);
    }

    /// `system`'s own `subtype` gates recognition: only `"init"` is
    /// classified, any other subtype (a future addition this reader
    /// does not yet know about) falls through to `Other` like any
    /// other unrecognized shape.
    #[test]
    fn a_system_line_with_a_different_subtype_is_other() {
        let value = serde_json::json!({"type": "system", "subtype": "compact_boundary"});
        assert_eq!(classify_line(&value), StreamLine::Other);
    }

    #[test]
    fn classifies_system_init_and_carries_its_session_id() {
        let value =
            serde_json::json!({"type": "system", "subtype": "init", "session_id": "sess-1"});
        assert_eq!(
            classify_line(&value),
            StreamLine::SystemInit {
                session_id: Some("sess-1".to_string())
            }
        );
    }

    /// A minimal in-memory [`AdapterEventSink`] that just records every
    /// payload it was handed -- proving `drive_turn`'s own dispatch
    /// without a real `DomainAdapterEventSink`/database.
    struct RecordingSink {
        events: StdArc<parking_lot::Mutex<Vec<AdapterEventPayload>>>,
    }

    impl AdapterEventSink for RecordingSink {
        fn emit(&self, event: AdapterEvent) -> crate::adapter::AdapterFuture<'_, u64> {
            self.events.lock().push(event.payload);
            Box::pin(async { Ok(0) })
        }

        fn note_real_user_turn(&self, _run_id: RunId) -> crate::adapter::AdapterFuture<'_, ()> {
            // Unexercised by this module's own tests: `drive_turn` never
            // calls it (that signal belongs to a human typing directly
            // into a pane, which protocol mode has none of).
            Box::pin(async { Ok(()) })
        }
    }

    fn ids() -> RunIdentity {
        RunIdentity {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
        }
    }

    /// End to end against real in-memory pipes (`tokio::io::duplex`),
    /// no subprocess involved: proves the loop reads assistant text and
    /// a tool-start off a simulated stdout, stops at `result`, and never
    /// touches stdin when no control request arrives.
    #[tokio::test]
    async fn drives_a_turn_with_no_approval_needed() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, mut stdin_capture) = tokio::io::duplex(4096);

        let script = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sess-1"}"#,
            "\n",
            r#"{"type":"assistant","uuid":"u1","message":{"content":[{"type":"text","text":"hello"}]}}"#,
            "\n",
            r#"{"type":"assistant","uuid":"u2","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success"}"#,
            "\n",
        );
        test_side.write_all(script.as_bytes()).await.unwrap();
        drop(test_side);

        let events = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::clone(&events),
        });

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = ProjectId::new();
        let callback = ProtocolApprovalCallback::new();
        let approval_service = ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(crate::approval::NoopApprovalCallback) as StdArc<dyn ApprovalCallback>,
            broadcast::channel(64).0,
        );

        let outcome = drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
        )
        .await
        .expect("must not error");

        assert_eq!(outcome.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            outcome.entry_ids,
            ["u1".to_string(), "u2".to_string()].into_iter().collect()
        );

        {
            let recorded = events.lock();
            assert!(matches!(
                recorded[0],
                AdapterEventPayload::MessageFinal { .. }
            ));
            assert!(matches!(
                recorded[1],
                AdapterEventPayload::ToolStarted { .. }
            ));
        }

        // Nothing should have been written back to stdin: no control
        // request ever arrived to answer.
        let mut written = Vec::new();
        stdin_capture.read_to_end(&mut written).await.unwrap();
        assert!(written.is_empty());

        db.shutdown().await.ok();
    }

    /// A `can_use_tool` control request is bridged through a REAL
    /// `ApprovalService` and the reply is written back onto the
    /// adapter's own stdin pipe -- proving `drive_turn` actually
    /// completes the round trip end to end, not just that it calls into
    /// `approval_bridge`.
    #[tokio::test]
    async fn bridges_a_control_request_and_writes_the_reply_to_stdin() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, mut stdin_capture) = tokio::io::duplex(4096);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = ProjectId::new();

        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &TaskRef {
                    owner_client_instance_id: "omp-1".into(),
                    revision: 1,
                },
            )?;
            repo.create_worker(&Worker {
                worker_id,
                profile_ref: WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".into(),
                    adapter: "claude".into(),
                    model: "test".into(),
                    permission_envelope: serde_json::json!({}),
                },
                parent_worker_id: None,
                created_at: Timestamp::now(),
            })?;
            repo.submit_run(
                &Run {
                    run_id,
                    task_id,
                    worker_id,
                    state: RunState::try_from("queued").unwrap(),
                    flags: RunFlags::default(),
                    vendor_session_id: None,
                    started_at: None,
                    completed_at: None,
                },
                None,
                None,
            )?;
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed");
        for state in ["starting", "working"] {
            let to = RunState::try_from(state).unwrap();
            db.run_domain_op(Box::new(move |conn| {
                DomainRepository::new(conn, project_id)
                    .transition_run(run_id, &to, None)
                    .map(|_| serde_json::json!({}))
            }))
            .await
            .unwrap();
        }

        let events = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::clone(&events),
        });
        let callback = StdArc::new(ProtocolApprovalCallback::new());
        let approval_service = ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::clone(&callback) as StdArc<dyn ApprovalCallback>,
            broadcast::channel(64).0,
        );

        let drive = tokio::spawn({
            let callback = StdArc::clone(&callback);
            async move {
                drive_turn(
                    adapter_stdout,
                    adapter_stdin,
                    &sink,
                    &approval_service,
                    &callback,
                    RunIdentity {
                        run_id,
                        task_id,
                        worker_id,
                    },
                )
                .await
            }
        });

        let request = concat!(
            r#"{"type":"control_request","request_id":"req-1","request":"#,
            r#"{"subtype":"can_use_tool","tool_name":"Write","input":{}}}"#,
            "\n",
        );
        test_side.write_all(request.as_bytes()).await.unwrap();

        tokio::task::yield_now().await;
        let approval_id = {
            let value = db
                .run_domain_op(Box::new(move |conn| {
                    conn.query_row(
                        "SELECT approval_id FROM approvals WHERE run_id = ?1",
                        [run_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .map(|id| serde_json::json!({ "id": id }))
                    .map_err(Into::into)
                }))
                .await
                .expect("approval row must exist");
            crew_protocol::ApprovalId::parse(value["id"].as_str().unwrap()).unwrap()
        };
        let decide_service = ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            callback as StdArc<dyn ApprovalCallback>,
            broadcast::channel(64).0,
        );
        decide_service
            .decide(
                approval_id,
                "omp-1",
                "approve",
                &Redacted::assert_runtime_authored("approved for the test"),
                DecidedBy::Human,
            )
            .await
            .expect("decide must succeed");

        test_side
            .write_all(b"{\"type\":\"result\",\"subtype\":\"success\"}\n")
            .await
            .unwrap();
        drop(test_side);

        drive
            .await
            .expect("task must not panic")
            .expect("drive_turn must not error");

        let mut written = Vec::new();
        stdin_capture.read_to_end(&mut written).await.unwrap();
        let written = String::from_utf8(written).expect("valid utf-8");
        let first_line = written.lines().next().expect("at least one reply line");
        let reply: serde_json::Value = serde_json::from_str(first_line).expect("valid json line");
        assert_eq!(reply["response"]["request_id"], "req-1");
        assert_eq!(reply["response"]["response"]["behavior"], "allow");

        db.shutdown().await.ok();
    }
}
