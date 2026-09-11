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
use tokio::sync::broadcast;

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
    /// `type: "control_response"`: a reply to a control request THIS
    /// reader itself sent -- today, the only one it ever sends is
    /// [`INITIALIZE_REQUEST_ID`]'s own handshake, so `request_id` is
    /// checked against that one constant; a future second outbound
    /// request would need this reader to track more than one pending id
    /// to disambiguate, which nothing here does yet because nothing
    /// here sends a second one. `request_id` is read from
    /// `response.request_id` -- the same nesting claude's own reply to
    /// crew's `can_use_tool` answers uses
    /// ([`approval_bridge::build_permission_response`]'s own shape),
    /// confirmed live for the initialize handshake specifically in the
    /// same capture that confirmed `can_use_tool` itself.
    ControlResponse {
        request_id: Option<String>,
    },
    /// `type: "result"`: claude's own turn-complete marker. The read
    /// loop stops here rather than waiting for stdout to close --
    /// waiting for EOF as well would work too (claude exits after this),
    /// but would leave a turn's own completion depending on process
    /// exit timing rather than the protocol's own explicit signal.
    /// Carries `permission_denials` straight off the same message -- a
    /// live capture confirmed the shape (`tool_name`, `tool_use_id`,
    /// `tool_input` per entry) -- so [`drive_turn`] can check each one
    /// against BOTH of [`TurnOutcome::bridged_tool_use_ids`] and
    /// [`TurnOutcome::observed_permission_denials`] before ever
    /// reporting the turn complete: explained by either one is
    /// expected; explained by neither is the one case nothing on the
    /// stream accounts for.
    TurnComplete {
        permission_denials: Vec<PermissionDenial>,
    },
    /// `type: "system", subtype: "init"`: the one message this reader
    /// reads a `session_id` off of, for
    /// [`TurnOutcome::session_id`] -- claude's own transcript file is
    /// named `<session_id>.jsonl`, and reconciliation cannot find the
    /// right file without it. Also the one message
    /// [`super::posture::version_gate`]/[`super::posture::permission_mode_gate`]
    /// check `claude_code_version`/`permissionMode` against -- see that
    /// module's own doc comment for what a live capture confirmed
    /// `system/init` does and does not report.
    SystemInit {
        session_id: Option<String>,
        claude_code_version: Option<String>,
        permission_mode: Option<String>,
    },
    /// `type: "system", subtype: "permission_denied"`: a post-hoc
    /// notification that a tool call was denied WITHOUT ever raising a
    /// `can_use_tool` control request -- the shape
    /// `permission-denied-frame.jsonl` exists to document (a hard `deny`
    /// rule, or an `ask` rule matched with no host present, both deny
    /// and report rather than ask). Its own `tool_use_id` is what makes
    /// [`TurnOutcome::observed_permission_denials`] the second leg of
    /// the ledger-reconciliation check at [`StreamLine::TurnComplete`]:
    /// a denial explained by ONE of this or
    /// [`TurnOutcome::bridged_tool_use_ids`] is expected; a denial
    /// explained by NEITHER is the one case nothing on the whole stream
    /// accounts for.
    PermissionDeniedNotice {
        tool_use_id: Option<String>,
    },
    Other,
}

/// One entry off the final `result` message's own `permission_denials`
/// array -- a tool call claude denied without this reader's own
/// `StreamLine::ControlRequest` handling ever seeing it approved (a
/// hard `deny` rule denies without asking anyone; an `ask` rule with no
/// host present, per the documented hostless behavior, denies and
/// reports rather than silently allowing). Field names mirror the real
/// capture exactly; both optional, tolerant of a shape variation this
/// reader has not seen, matching every other parse in this module.
#[derive(Debug, PartialEq, Clone)]
struct PermissionDenial {
    tool_name: Option<String>,
    tool_use_id: Option<String>,
}

/// The fixed correlation id this reader sends its own `initialize`
/// control request under -- see [`drive_turn`]'s own doc comment for
/// why a literal constant is sufficient (this reader only ever sends
/// one outbound control request per turn, so nothing else could collide
/// with it).
const INITIALIZE_REQUEST_ID: &str = "crew-initialize";

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
    /// Every `tool_use_id` this reader actually bridged into crew's own
    /// approval ledger this turn, via
    /// [`approval_bridge::handle_permission_request`] -- checking
    /// membership in this set at [`StreamLine::TurnComplete`] time IS
    /// checking the ledger, not a proxy for it:
    /// `ApprovalService::request` durably commits the approval row
    /// before `handle_permission_request` ever returns, and nothing
    /// removes a row from `approvals` for a run this reader is
    /// currently driving, so this in-memory set and a fresh query
    /// against the database can never diverge within one call to
    /// [`drive_turn`].
    pub(crate) bridged_tool_use_ids: HashSet<String>,
    /// Every `tool_use_id` this reader saw named on a real
    /// `system`/`permission_denied` notification this turn -- the
    /// second leg of the ledger-reconciliation check at
    /// [`StreamLine::TurnComplete`]. This is what makes that check
    /// three-way rather than two: a denial explained by THIS set is an
    /// operator's own rule denying without asking anyone (legitimate,
    /// not a sentinel failure); a denial explained by neither this set
    /// nor [`Self::bridged_tool_use_ids`] is the one case nothing on
    /// the stream accounts for. Keying on the presence of this frame,
    /// not on its `decision_reason_type` field (an undocumented,
    /// single-observation vendor enum a future release could silently
    /// add a new value to), is deliberate.
    pub(crate) observed_permission_denials: HashSet<String>,
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
        Some("control_response") => StreamLine::ControlResponse {
            request_id: value
                .get("response")
                .and_then(|response| response.get("request_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        },
        Some("result") => StreamLine::TurnComplete {
            permission_denials: value
                .get("permission_denials")
                .and_then(serde_json::Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| PermissionDenial {
                            tool_name: entry
                                .get("tool_name")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            tool_use_id: entry
                                .get("tool_use_id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        Some("system")
            if value.get("subtype").and_then(serde_json::Value::as_str) == Some("init") =>
        {
            StreamLine::SystemInit {
                session_id: value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                claude_code_version: value
                    .get("claude_code_version")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                permission_mode: value
                    .get("permissionMode")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            }
        }
        Some("system")
            if value.get("subtype").and_then(serde_json::Value::as_str)
                == Some("permission_denied") =>
        {
            StreamLine::PermissionDeniedNotice {
                tool_use_id: value
                    .get("tool_use_id")
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
/// The very first thing written to `stdin` is an `initialize` control
/// request (`request_id` [`INITIALIZE_REQUEST_ID`]) -- "as the SDK
/// does," per the maintainer's own ruling on this: the SDK always sends
/// one, even though it declares nothing about permissions itself. Once
/// this reader sees the matching `control_response` (which may arrive
/// before or after `system/init` -- a live capture found both orderings
/// possible, and this loop's own dispatch does not depend on either),
/// `prompt` is delivered as the ordinary `stream-json` user message.
/// Any line arriving before that point (a `system`/`hook_*` line, the
/// operator's own SessionStart hooks, `system/init` itself) is still
/// classified and acted on through the exact same dispatch below -- this
/// is not a separate handshake-only loop bolted in front of the real
/// one.
///
/// Returns once the turn is complete (or the stream closed) -- it never
/// loops forever on a stream that legitimately ends, and it never treats
/// EOF as an error, with one exception: a stream that closes before the
/// initialize handshake ever completed means `prompt` was never sent at
/// all, so nothing this turn was meant to do ever ran -- that is
/// reported as a protocol error, not folded into an ordinary empty
/// [`TurnOutcome`]. Once the prompt has been sent, a stream that later
/// closes without an explicit `result` line is the ordinary case again
/// (claude exited after signalling completion, or crashed): `TurnOutcome`
/// is returned as `Ok`, matching this function's original stance,
/// leaving `Adapter::start`'s own caller to observe the process exit and
/// reconcile against the transcript regardless of how the stream ended.
///
/// `pane_output`, when given, gets one formatted line per normalized
/// event pushed into it as the turn progresses -- see
/// `claude_protocol::pane`'s own module doc comment for the format and
/// why it is a crew-authored report, never the vendor's own output. A
/// send failing (no receiver currently attached, the ordinary case when
/// nobody is watching) is silently ignored: the pane is a convenience
/// view, never load-bearing for the turn itself.
///
/// # Errors
/// Propagates a stdout read failure, a stdin write failure, or
/// [`approval_bridge::handle_permission_request`]'s own error (a run no
/// longer `working`, or this adapter torn down mid-approval) as a
/// [`crate::adapter::error::AdapterError::process`]/`::protocol`. Also
/// returns `::protocol` if the stream closes before the prompt was ever
/// sent, or if claude's own final `result` reports a permission denial
/// explained by NEITHER [`TurnOutcome::bridged_tool_use_ids`] (the host
/// was asked, over the control channel, and answered) NOR
/// [`TurnOutcome::observed_permission_denials`] (an operator's own rule
/// denied it without asking anyone, announced on the stream by its own
/// `system`/`permission_denied` notice) -- the one observable signal
/// that the host was never consulted for it AT ALL, which is what a
/// broken/renamed `--permission-prompt-tool` sentinel would cause. A
/// denial explained by either set is expected and does not fail the
/// turn -- an operator's own hard `deny` rule is legitimate, ordinary
/// behavior, not a sentinel failure.
// The `prompt` parameter added for the initialize handshake is the
// eighth -- matching the existing precedent elsewhere in this codebase
// (`build_tui_adapter`, `TuiAdapter::fail_start`) of allowing this
// specific lint on a function whose every parameter is load-bearing and
// already documented above, rather than introducing a params struct
// for one field.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive_turn<R, W>(
    stdout: R,
    mut stdin: W,
    sink: &Arc<dyn AdapterEventSink>,
    approval_service: &ApprovalService,
    callback: &ProtocolApprovalCallback,
    ids: RunIdentity,
    pane_output: Option<&broadcast::Sender<Vec<u8>>>,
    prompt: &str,
) -> Result<TurnOutcome, crate::adapter::error::AdapterError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use crate::adapter::error::AdapterError;

    let render = |text: &str| {
        if let Some(tx) = pane_output {
            let _ = tx.send(super::pane::render_line(text));
        }
    };

    let init_request = serde_json::json!({
        "type": "control_request",
        "request_id": INITIALIZE_REQUEST_ID,
        "request": {"subtype": "initialize"},
    });
    let mut init_line =
        serde_json::to_string(&init_request).expect("a constructed value always serializes");
    init_line.push('\n');
    stdin
        .write_all(init_line.as_bytes())
        .await
        .map_err(|e| AdapterError::process("claude", "write_stdin", e.to_string()))?;

    let mut outcome = TurnOutcome::default();
    let mut prompt_sent = false;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|e| AdapterError::process("claude", "read_stdout", e.to_string()))?;
        let Some(line) = line else {
            if !prompt_sent {
                // The stream closed before this adapter's own
                // initialize handshake ever completed -- the prompt was
                // never sent, so no turn ran at all. Distinct from the
                // ordinary "closed after result" case below: there,
                // something happened; here, nothing did.
                return Err(AdapterError::protocol(
                    "claude",
                    "start",
                    "claude's stdout closed before this adapter's own initialize handshake \
                     completed -- the prompt was never sent, so this turn never ran",
                ));
            }
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
            StreamLine::SystemInit {
                session_id,
                claude_code_version,
                permission_mode,
            } => {
                outcome.session_id = session_id;
                // Both checked as soon as `system/init` arrives, before
                // this adapter trusts anything the rest of the turn
                // reports -- they catch different classes (an
                // incompatible binary vs. a pinned setting that failed
                // to apply) and either one failing must abort the turn,
                // not merely be noted afterward.
                let reported_version = claude_code_version.as_deref().unwrap_or("<missing>");
                if let Err(detail) = super::posture::version_gate(reported_version) {
                    return Err(AdapterError::incompatible_version(
                        "claude", "start", detail,
                    ));
                }
                let reported_mode = permission_mode.as_deref().unwrap_or("<missing>");
                if let Err(detail) = super::posture::permission_mode_gate(reported_mode) {
                    return Err(AdapterError::protocol("claude", "start", detail));
                }
            }
            StreamLine::ControlResponse { request_id } if !prompt_sent => {
                if request_id.as_deref() != Some(INITIALIZE_REQUEST_ID) {
                    // Not the handshake's own response (or carries no
                    // `request_id` at all) -- this reader has nothing
                    // else pending to match it against yet, since the
                    // prompt (and anything that could raise a
                    // `can_use_tool` request) has not been sent.
                    continue;
                }
                let mut prompt_line = serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": prompt}],
                    },
                })
                .to_string();
                prompt_line.push('\n');
                stdin
                    .write_all(prompt_line.as_bytes())
                    .await
                    .map_err(|e| AdapterError::process("claude", "write_stdin", e.to_string()))?;
                prompt_sent = true;
            }
            // Once the handshake is done, any further `control_response`
            // is one this reader never sends a matching request for
            // today -- nothing to act on.
            StreamLine::ControlResponse { .. } => {}
            StreamLine::AssistantText(text) => {
                render(&format!("assistant: {text}"));
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
                render(&format!("tool: {name}"));
                sink.emit(AdapterEvent {
                    run_id: ids.run_id,
                    task_id: ids.task_id,
                    worker_id: ids.worker_id,
                    payload: AdapterEventPayload::ToolStarted { tool_call_id, name },
                    cursor: None,
                })
                .await?;
            }
            StreamLine::PermissionDeniedNotice { tool_use_id } => {
                render("permission denied without asking");
                // Recorded regardless of whether `tool_use_id` is
                // present -- a `None` here just means this particular
                // notice can never explain a denial by id, not that it
                // should be discarded outright (the same tolerant
                // stance every other optional field in this module
                // takes).
                if let Some(tool_use_id) = tool_use_id {
                    outcome.observed_permission_denials.insert(tool_use_id);
                }
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
                render(&format!("permission requested: {}", parsed.tool_name));
                // Recorded before the request is bridged, not after the
                // reply comes back -- `ApprovalService::request` (called
                // inside `handle_permission_request`) durably commits the
                // approval row before that call ever returns, so this
                // set and the ledger it stands in for agree at every
                // point after this line, not just once the whole request
                // finishes (which may block a long time on a human
                // decision).
                if let Some(tool_use_id) = parsed.tool_use_id.clone() {
                    outcome.bridged_tool_use_ids.insert(tool_use_id);
                }
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
                render("permission decided");
            }
            StreamLine::TurnComplete { permission_denials } => {
                render("turn complete");
                // The ledger-reconciliation check, three-way: a denial
                // is expected if EITHER this reader bridged it into
                // crew's own approval ledger (`bridged_tool_use_ids` --
                // the host was asked and answered, deny included) OR a
                // real `system`/`permission_denied` notice named it on
                // the stream (`observed_permission_denials` -- an
                // operator's own rule denied it without asking anyone,
                // which is legitimate and not a sentinel failure).
                // Explained by NEITHER is the one case nothing on the
                // whole stream accounts for -- the observable proof
                // that the host was never consulted for it at all.
                // Fail-closed, not merely logged: a run whose own
                // approval ledger cannot account for one of its own
                // permission decisions is not one this adapter can
                // vouch for having driven correctly.
                for denial in &permission_denials {
                    // Split deliberately: a missing `tool_use_id` is a
                    // *shape* problem (this entry cannot be correlated
                    // against either set at all), never a claim about
                    // the sentinel -- conflating the two would name a
                    // cause with no evidence for it, and would make a
                    // future CLI release that drops this field fail
                    // every denial in every repo with an ordinary deny
                    // rule while pointing whoever debugs it at the wrong
                    // flag. Both branches still fail closed; only the
                    // message differs.
                    let Some(id) = denial.tool_use_id.as_deref() else {
                        return Err(AdapterError::protocol(
                            "claude",
                            "start",
                            format!(
                                "claude's own result reported a permission denial for {} with \
                                 no tool_use_id at all, so it cannot be correlated against this \
                                 turn's own approval ledger or its observed \
                                 system/permission_denied notices -- the denial's shape, not the \
                                 host's participation, is what is unaccounted for here",
                                denial.tool_name.as_deref().unwrap_or("<unknown tool>")
                            ),
                        ));
                    };
                    let explained = outcome.bridged_tool_use_ids.contains(id)
                        || outcome.observed_permission_denials.contains(id);
                    if !explained {
                        return Err(AdapterError::protocol(
                            "claude",
                            "start",
                            format!(
                                "claude's own result reported a permission denial for {} with \
                                 no corresponding entry in this turn's own approval ledger, and \
                                 no matching system/permission_denied notice on the stream -- \
                                 the host was never consulted for it",
                                denial.tool_name.as_deref().unwrap_or("<unknown tool>")
                            ),
                        ));
                    }
                }
                return Ok(outcome);
            }
            StreamLine::Other => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use tokio::io::AsyncReadExt;

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
        assert_eq!(
            classify_line(&value),
            StreamLine::TurnComplete {
                permission_denials: Vec::new(),
            }
        );
    }

    /// `result`'s own `permission_denials` array, parsed off the exact
    /// shape a live capture confirmed (`tool_name`, `tool_use_id`,
    /// `tool_input` per entry -- `tool_input` itself unused here, this
    /// reader only needs the other two to reconcile against
    /// `TurnOutcome::bridged_tool_use_ids`).
    #[test]
    fn classifies_the_result_markers_own_permission_denials() {
        let value = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "permission_denials": [
                {"tool_name": "Bash", "tool_use_id": "toolu_1", "tool_input": {}},
            ],
        });
        assert_eq!(
            classify_line(&value),
            StreamLine::TurnComplete {
                permission_denials: vec![PermissionDenial {
                    tool_name: Some("Bash".to_string()),
                    tool_use_id: Some("toolu_1".to_string()),
                }],
            }
        );
    }

    /// A `permission_denials` entry missing `tool_use_id` entirely
    /// parses to `None` rather than failing the whole line -- this is
    /// the shape [`drive_turn`]'s own ledger-reconciliation check must
    /// treat as an uncorrelatable entry, not as evidence the host was
    /// never consulted (see the dedicated end-to-end test for that
    /// distinction).
    #[test]
    fn a_permission_denial_missing_tool_use_id_parses_to_none() {
        let value = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "permission_denials": [
                {"tool_name": "Bash", "tool_input": {}},
            ],
        });
        assert_eq!(
            classify_line(&value),
            StreamLine::TurnComplete {
                permission_denials: vec![PermissionDenial {
                    tool_name: Some("Bash".to_string()),
                    tool_use_id: None,
                }],
            }
        );
    }

    /// `control_response`'s own `request_id`, nested under `response` --
    /// the same nesting the reply this reader itself writes uses.
    #[test]
    fn classifies_a_control_response_and_reads_its_nested_request_id() {
        let value = serde_json::json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": "crew-initialize", "response": {}},
        });
        assert_eq!(
            classify_line(&value),
            StreamLine::ControlResponse {
                request_id: Some("crew-initialize".to_string()),
            }
        );
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
    fn classifies_system_init_and_carries_its_session_id_version_and_permission_mode() {
        let value = serde_json::json!({
            "type": "system",
            "subtype": "init",
            "session_id": "sess-1",
            "claude_code_version": "2.1.268",
            "permissionMode": "auto",
        });
        assert_eq!(
            classify_line(&value),
            StreamLine::SystemInit {
                session_id: Some("sess-1".to_string()),
                claude_code_version: Some("2.1.268".to_string()),
                permission_mode: Some("auto".to_string()),
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
    /// writes anything to stdin beyond its own initialize handshake when
    /// no `can_use_tool` control request ever arrives.
    #[tokio::test]
    async fn drives_a_turn_with_no_approval_needed() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, mut stdin_capture) = tokio::io::duplex(4096);

        let script = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sess-1","claude_code_version":"2.1.268","permissionMode":"auto"}"#,
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
            None,
            "hello",
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

        // The only thing ever written to stdin is this reader's own
        // initialize handshake -- this script never sends back a
        // matching `control_response`, so the prompt itself is never
        // sent either, and (with no control request arriving) nothing
        // else is written after that.
        let mut written = Vec::new();
        stdin_capture.read_to_end(&mut written).await.unwrap();
        let written = String::from_utf8(written).expect("valid utf-8");
        let mut written_lines = written.lines();
        let first_line: serde_json::Value =
            serde_json::from_str(written_lines.next().expect("at least one line")).unwrap();
        assert_eq!(first_line["type"], "control_request");
        assert_eq!(first_line["request_id"], INITIALIZE_REQUEST_ID);
        assert_eq!(first_line["request"]["subtype"], "initialize");
        assert!(written_lines.next().is_none(), "no second line expected");

        db.shutdown().await.ok();
    }

    /// An incompatible (or missing) `claude_code_version` aborts the
    /// turn as soon as `system/init` arrives -- before any assistant
    /// text or tool call is ever processed, not merely reported
    /// afterward. Proven by a script whose `system/init` claims a
    /// version far outside the tested range, then keeps sending
    /// content `drive_turn` must never reach.
    #[tokio::test]
    async fn an_incompatible_reported_version_aborts_the_turn_immediately() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, _stdin_capture) = tokio::io::duplex(4096);

        let script = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sess-1","claude_code_version":"0.1.0"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"should never be read"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success"}"#,
            "\n",
        );
        test_side.write_all(script.as_bytes()).await.unwrap();
        drop(test_side);

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
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

        let result = drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
            None,
            "should never be sent",
        )
        .await;

        let err = result.expect_err("an out-of-range version must abort the turn");
        assert_eq!(
            err.error_code(),
            crate::adapter::AdapterErrorCode::IncompatibleVersion
        );

        db.shutdown().await.ok();
    }

    /// The direct permission-mode assertion, proven the same way: a
    /// `system/init` reporting a mode other than the pinned one aborts
    /// the turn immediately, before any content is processed.
    #[tokio::test]
    async fn a_mismatched_reported_permission_mode_aborts_the_turn_immediately() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, _stdin_capture) = tokio::io::duplex(4096);

        let script = concat!(
            r#"{"type":"system","subtype":"init","session_id":"sess-1","claude_code_version":"2.1.268","permissionMode":"plan"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"should never be read"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success"}"#,
            "\n",
        );
        test_side.write_all(script.as_bytes()).await.unwrap();
        drop(test_side);

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
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

        let result = drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
            None,
            "should never be sent",
        )
        .await;

        let err = result.expect_err("a mismatched permission mode must abort the turn");
        assert_eq!(err.error_code(), crate::adapter::AdapterErrorCode::Protocol);
        assert!(err.detail().contains("plan"));

        db.shutdown().await.ok();
    }

    /// `pane_output`, when given, actually receives one formatted line
    /// per normalized event -- proven by subscribing to it before the
    /// turn runs and reading real lines back, not by inspecting
    /// `drive_turn`'s own source for a call site that looks right.
    #[tokio::test]
    async fn pane_output_receives_one_formatted_line_per_event() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, _stdin_capture) = tokio::io::duplex(4096);

        let script = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#,
            "\n",
            r#"{"type":"result","subtype":"success"}"#,
            "\n",
        );
        test_side.write_all(script.as_bytes()).await.unwrap();
        drop(test_side);

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
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

        let (pane_tx, mut pane_rx) = broadcast::channel(16);

        drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
            Some(&pane_tx),
            "hello",
        )
        .await
        .expect("must not error");

        let first = pane_rx.try_recv().expect("a line for the assistant text");
        assert_eq!(first, super::super::pane::render_line("assistant: hello"));
        let second = pane_rx.try_recv().expect("a line for turn completion");
        assert_eq!(second, super::super::pane::render_line("turn complete"));
        assert!(
            pane_rx.try_recv().is_err(),
            "no third line should have been sent"
        );

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
                    None,
                    "hello",
                )
                .await
            }
        });

        let request = concat!(
            r#"{"type":"control_request","request_id":"req-1","request":"#,
            r#"{"subtype":"can_use_tool","tool_name":"Write","input":{},"tool_use_id":"toolu_1"}}"#,
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
        // The first line is this reader's own initialize handshake
        // (never answered by this test's script), so the reply to
        // "req-1" is found by its own `request_id`, not by position.
        let reply = written
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid json line"))
            .find(|value| value["response"]["request_id"] == "req-1")
            .expect("a reply naming req-1 must have been written");
        assert_eq!(reply["response"]["response"]["behavior"], "allow");

        db.shutdown().await.ok();
    }

    fn noop_approval_service(
        db: &StdArc<DatabaseHandle>,
        project_id: ProjectId,
    ) -> ApprovalService {
        ApprovalService::new(
            StdArc::clone(db),
            project_id,
            StdArc::new(crate::approval::NoopApprovalCallback) as StdArc<dyn ApprovalCallback>,
            broadcast::channel(64).0,
        )
    }

    /// The initialize handshake actually gates when the prompt goes out:
    /// a hook-shaped line arriving first is processed and ignored (falls
    /// through to `Other`, like today), and the prompt itself is not
    /// written until the matching `control_response` arrives -- proven
    /// by reading stdin back in order, not by inspecting source.
    #[tokio::test]
    async fn the_prompt_is_sent_only_after_the_initialize_handshake_completes() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, mut stdin_capture) = tokio::io::duplex(4096);

        let script = concat!(
            r#"{"type":"system","subtype":"hook_started"}"#,
            "\n",
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"crew-initialize","response":{}}}"#,
            "\n",
            r#"{"type":"result","subtype":"success"}"#,
            "\n",
        );
        test_side.write_all(script.as_bytes()).await.unwrap();
        drop(test_side);

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
        });
        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = ProjectId::new();
        let callback = ProtocolApprovalCallback::new();
        let approval_service = noop_approval_service(&db, project_id);

        drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
            None,
            "the real prompt",
        )
        .await
        .expect("must not error");

        let mut written = Vec::new();
        stdin_capture.read_to_end(&mut written).await.unwrap();
        let written = String::from_utf8(written).expect("valid utf-8");
        let lines: Vec<serde_json::Value> = written
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid json line"))
            .collect();
        assert_eq!(lines.len(), 2, "expected the handshake, then the prompt");
        assert_eq!(lines[0]["request_id"], INITIALIZE_REQUEST_ID);
        assert_eq!(lines[1]["type"], "user");
        assert_eq!(lines[1]["message"]["content"][0]["text"], "the real prompt");

        db.shutdown().await.ok();
    }

    /// A stream that closes before the handshake's own `control_response`
    /// ever arrives means the prompt was never sent -- distinct from an
    /// ordinary post-`result` close, and reported as an error rather
    /// than an empty, ordinary-looking [`TurnOutcome`].
    #[tokio::test]
    async fn a_stream_closed_before_the_handshake_completes_is_an_error() {
        let (test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, _stdin_capture) = tokio::io::duplex(4096);
        drop(test_side);

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
        });
        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = ProjectId::new();
        let callback = ProtocolApprovalCallback::new();
        let approval_service = noop_approval_service(&db, project_id);

        let result = drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
            None,
            "never sent",
        )
        .await;

        let err = result.expect_err("closing before the handshake completes must error");
        assert_eq!(err.error_code(), crate::adapter::AdapterErrorCode::Protocol);

        db.shutdown().await.ok();
    }

    /// The ledger-reconciliation check, fail-closed direction: a
    /// `permission_denials` entry whose `tool_use_id` this reader never
    /// bridged into `ApprovalService` AND never saw named on a real
    /// `system`/`permission_denied` notice -- explained by neither leg
    /// of the three-way check -- aborts the turn: the observable proof
    /// the host was never consulted for it.
    #[tokio::test]
    async fn an_unexplained_permission_denial_fails_the_turn() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, _stdin_capture) = tokio::io::duplex(4096);

        let script = concat!(
            r#"{"type":"result","subtype":"success","permission_denials":"#,
            r#"[{"tool_name":"Bash","tool_use_id":"toolu_never_bridged"}]}"#,
            "\n",
        );
        test_side.write_all(script.as_bytes()).await.unwrap();
        drop(test_side);

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
        });
        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = ProjectId::new();
        let callback = ProtocolApprovalCallback::new();
        let approval_service = noop_approval_service(&db, project_id);

        let result = drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
            None,
            "hello",
        )
        .await;

        let err = result.expect_err("an unexplained denial must fail the turn");
        assert_eq!(err.error_code(), crate::adapter::AdapterErrorCode::Protocol);
        assert!(err.detail().contains("Bash"));

        db.shutdown().await.ok();
    }

    /// A `permission_denials` entry with NO `tool_use_id` at all still
    /// fails the turn closed, but the message must say this is a shape
    /// problem (nothing to correlate against), never claim the host was
    /// never consulted -- that claim has no evidence behind it here, and
    /// a message that made it would misdirect debugging toward the
    /// sentinel flag for what could just as well be a future CLI release
    /// dropping this field.
    #[tokio::test]
    async fn a_permission_denial_with_no_tool_use_id_fails_the_turn_with_a_shape_message() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, _stdin_capture) = tokio::io::duplex(4096);

        let script = concat!(
            r#"{"type":"result","subtype":"success","permission_denials":"#,
            r#"[{"tool_name":"Bash"}]}"#,
            "\n",
        );
        test_side.write_all(script.as_bytes()).await.unwrap();
        drop(test_side);

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
        });
        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = ProjectId::new();
        let callback = ProtocolApprovalCallback::new();
        let approval_service = noop_approval_service(&db, project_id);

        let result = drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
            None,
            "hello",
        )
        .await;

        let err = result.expect_err("a denial with no tool_use_id must still fail the turn");
        assert_eq!(err.error_code(), crate::adapter::AdapterErrorCode::Protocol);
        assert!(err.detail().contains("Bash"));
        assert!(err.detail().contains("no tool_use_id at all"));
        assert!(
            !err.detail().contains("the host was never consulted"),
            "must not claim the host was never consulted when the actual problem is a missing \
             correlation field: {}",
            err.detail()
        );

        db.shutdown().await.ok();
    }

    /// The real recorded shape, as a fixture rather than an inline
    /// literal -- `fixtures/adapters/claude-protocol/permission-denied-frame.jsonl`
    /// is the actual live capture (scrubbed) this reader's own
    /// `system`/`permission_denied` classification exists to parse.
    fn permission_denied_frame_fixture() -> String {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/adapters/claude-protocol/permission-denied-frame.jsonl");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
    }

    /// Classifies the real fixture, not a hand-typed lookalike.
    #[test]
    fn classifies_the_real_permission_denied_frame_fixture() {
        let line = permission_denied_frame_fixture();
        let line = line.lines().next().expect("fixture has at least one line");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid json line");
        assert_eq!(
            classify_line(&value),
            StreamLine::PermissionDeniedNotice {
                tool_use_id: Some("toolu_00000000000000000000000001".to_string()),
            }
        );
    }

    /// The ledger-reconciliation check's second leg, proven against the
    /// real fixture: a `permission_denials` entry whose `tool_use_id`
    /// this reader never bridged, but DID see named on a real
    /// `system`/`permission_denied` notice on the stream, is explained
    /// -- an operator's own rule denying without asking anyone, not a
    /// sentinel failure -- and the turn completes normally.
    #[tokio::test]
    async fn a_denial_matching_an_observed_permission_denied_notice_does_not_fail_the_turn() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, _stdin_capture) = tokio::io::duplex(4096);

        let mut script = permission_denied_frame_fixture();
        if !script.ends_with('\n') {
            script.push('\n');
        }
        script.push_str(concat!(
            r#"{"type":"result","subtype":"success","permission_denials":"#,
            r#"[{"tool_name":"Bash","tool_use_id":"toolu_00000000000000000000000001"}]}"#,
            "\n",
        ));
        test_side.write_all(script.as_bytes()).await.unwrap();
        drop(test_side);

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
        });
        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = ProjectId::new();
        let callback = ProtocolApprovalCallback::new();
        let approval_service = noop_approval_service(&db, project_id);

        let outcome = drive_turn(
            adapter_stdout,
            adapter_stdin,
            &sink,
            &approval_service,
            &callback,
            ids(),
            None,
            "hello",
        )
        .await
        .expect("a denial explained by an observed notice must not fail the turn");

        assert!(
            outcome
                .observed_permission_denials
                .contains("toolu_00000000000000000000000001")
        );

        db.shutdown().await.ok();
    }

    /// The other direction of the same check: a `permission_denials`
    /// entry whose `tool_use_id` DOES match a request this reader
    /// bridged through `ApprovalService` -- crew's own ledger accounts
    /// for it (a human, or crew's own policy, decided to deny it) -- and
    /// the turn completes normally rather than failing closed.
    #[tokio::test]
    async fn a_denial_matching_a_bridged_request_does_not_fail_the_turn() {
        let (mut test_side, adapter_stdout) = tokio::io::duplex(4096);
        let (adapter_stdin, _stdin_capture) = tokio::io::duplex(4096);

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

        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::new(parking_lot::Mutex::new(Vec::new())),
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
                    None,
                    "hello",
                )
                .await
            }
        });

        let request = concat!(
            r#"{"type":"control_request","request_id":"req-1","request":"#,
            r#"{"subtype":"can_use_tool","tool_name":"Bash","input":{},"tool_use_id":"toolu_denied"}}"#,
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
                "deny",
                &Redacted::assert_runtime_authored("denied for the test"),
                DecidedBy::Human,
            )
            .await
            .expect("decide must succeed");

        let result_line = concat!(
            r#"{"type":"result","subtype":"success","permission_denials":"#,
            r#"[{"tool_name":"Bash","tool_use_id":"toolu_denied"}]}"#,
            "\n",
        );
        test_side.write_all(result_line.as_bytes()).await.unwrap();
        drop(test_side);

        drive
            .await
            .expect("task must not panic")
            .expect("a denial crew itself decided must not fail the turn");

        db.shutdown().await.ok();
    }
}
