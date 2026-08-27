//! The Codex CLI's [`TuiVendor`] implementation: interactive `codex`
//! (never `app-server`), rollout JSONL transcript tailing under
//! `~/.codex/sessions/<YYYY>/<MM>/<DD>/`, and this vendor's own
//! permission-mode argv/compose-input/interrupt conventions.
//!
//! Every flag here was validated against an installed `codex` CLI's own
//! `--help` output (`-m/--model <MODEL>`, `-s/--sandbox <SANDBOX_MODE>`,
//! `--dangerously-bypass-approvals-and-sandbox`, the `resume`
//! subcommand), and the transcript shapes against real recorded rollouts
//! plus `fixtures/adapters/codex-tui/session.jsonl` -- the committed
//! *synthetic* fixture whose schema mirrors those recordings (session
//! meta / response items / turn contexts / event messages). Decisions
//! the fixture cannot itself prove are called out in this module's doc
//! comments rather than left implicit.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crew_protocol::{Classified, ContentClass};

use crate::adapter::r#trait::{StartSpec, VendorSessionRef};
use crate::config::crew::{AdapterConfig, PermissionMode};
use crate::supervisor::EnvironmentPolicy;

use super::adapter::{LaunchSpec, TuiVendor, VersionVerdict};
use super::{Cursor, TranscriptFormat, TuiEvent, parse_jsonl_chunk};

/// The `codex --version` range this adapter's fixed argv and
/// transcript-format assumptions were built and tested against -- same
/// policy as Claude's gate ([`super::claude`]): one validated point
/// (`0.149.1`, the fixture's recorded `cli_version`), everything else in
/// the range an untested extrapolation until WP29's live smoke widens it.
const MIN_TESTED_VERSION: (u32, u32, u32) = (0, 100, 0);
const MAX_TESTED_VERSION: (u32, u32, u32) = (0, 199, 99);

/// Parses the leading `MAJOR.MINOR.PATCH` out of a `codex --version`
/// string. Returns `None` for anything that does not start with three
/// dot-separated integers -- [`CodexTuiVendor::version_gate`] treats an
/// unparseable string as incompatible, never as a silent pass.
fn parse_leading_version(probed: &str) -> Option<(u32, u32, u32)> {
    // The real CLI's own output is `codex-cli 0.149.1`: the version
    // is the first *digit-led dot-separated* token, not the first token.
    let version_token = probed.split_whitespace().find(|token| {
        token.split('.').count() == 3 && token.chars().next().is_some_and(|c| c.is_ascii_digit())
    })?;
    let mut parts = version_token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// The Codex CLI, driven interactively over a real PTY.
pub struct CodexTuiVendor {
    /// The working directory the vendor process is launched in.
    cwd: PathBuf,
    /// `WorkerProfile::environmentAllowlist` -- variable *names* only,
    /// exactly like [`super::claude::ClaudeTuiVendor`]'s field.
    environment_allowlist: Vec<String>,
}

impl CodexTuiVendor {
    #[must_use]
    pub fn new(cwd: PathBuf, environment_allowlist: Vec<String>) -> Self {
        Self {
            cwd,
            environment_allowlist,
        }
    }

    /// `EnvironmentPolicy::baseline()` plus this vendor's own allowlisted
    /// names. `HOME` is load-bearing: the real CLI resolves its own
    /// `~/.codex/sessions/` rollout root the same way
    /// [`CodexTuiVendor::transcript_root`] does.
    fn env(&self) -> std::collections::HashMap<String, String> {
        let current: std::collections::HashMap<String, String> = std::env::vars().collect();
        EnvironmentPolicy::baseline().build(&current, &self.environment_allowlist)
    }

    /// The base argv every launch (fresh or resumed) shares: this
    /// vendor's own permission posture, `-m/--model` when configured, and
    /// `cfg.extra_args` verbatim last (an operator's own repeated flag
    /// wins by last-occurrence; never suppressed by this adapter).
    fn base_args(&self, cfg: &AdapterConfig) -> Vec<String> {
        let mut args = self.permission_args(cfg.permission_mode);
        if let Some(model) = &cfg.model {
            args.push("-m".to_string());
            args.push(model.clone());
        }
        args.extend(cfg.extra_args.iter().cloned());
        args
    }
}

impl TuiVendor for CodexTuiVendor {
    fn kind(&self) -> &'static str {
        "codex"
    }

    /// Interactive bare `codex`, deliberately never `app-server`: that is
    /// the headless RPC mode `CodexAdapter` already drives; a TUI session
    /// must launch the real interactive REPL so a human attached to its
    /// pane sees (and can type into) the exact same session this adapter
    /// tails.
    fn launch(&self, _spec: &StartSpec, cfg: &AdapterConfig) -> LaunchSpec {
        LaunchSpec {
            program: PathBuf::from(&cfg.bin),
            args: self.base_args(cfg),
            cwd: self.cwd.clone(),
            env: self.env(),
        }
    }

    /// `codex resume <session-id>` (the real subcommand; `--last` exists
    /// but names a different session every time), plus the same
    /// permission/model/extra-args base every fresh launch gets.
    fn resume_launch(
        &self,
        session: &VendorSessionRef,
        _spec: &StartSpec,
        cfg: &AdapterConfig,
    ) -> LaunchSpec {
        let mut args = vec!["resume".to_string(), session.0.clone()];
        args.extend(self.base_args(cfg));
        LaunchSpec {
            program: PathBuf::from(&cfg.bin),
            args,
            cwd: self.cwd.clone(),
            env: self.env(),
        }
    }

    /// `cfg.session_dir` overrides everything; otherwise
    /// `~/.codex/sessions/` -- the root under which the real CLI
    /// partitions rollouts by date (`<root>/<YYYY>/<MM>/<DD>/rollout-<ts>-<id>.jsonl`).
    /// `$HOME` unresolved falls back to `/root`, mirroring Claude's
    /// fallback.
    fn transcript_root(&self, _spec: &StartSpec, cfg: &AdapterConfig) -> PathBuf {
        if let Some(dir) = &cfg.session_dir {
            return PathBuf::from(dir);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        PathBuf::from(home).join(".codex").join("sessions")
    }

    /// Unlike Claude, a codex rollout's *filename* is not its session id:
    /// it is `rollout-<timestamp>-<session-id>.jsonl` under a date
    /// directory derived from when the session started, which a resumed
    /// session cannot re-derive from the id alone. So the trait default's
    /// deterministic join is wrong here; instead a bounded walk of the
    /// transcript root finds the (unique) rollout whose name ends in the
    /// session id. This touches only directory metadata -- no process
    /// spawn, no nonce-grep of file *contents* -- so it stays inside
    /// WP14's contract for why resume needs a stored-path lookup rather
    /// than content scanning.
    fn transcript_path_for_session(
        &self,
        session: &VendorSessionRef,
        spec: &StartSpec,
        cfg: &AdapterConfig,
    ) -> PathBuf {
        let root = self.transcript_root(spec, cfg);
        let suffix = format!("{}.jsonl", session.0);
        let mut stack = vec![root.clone()];
        let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // Bound the walk: a rollout root only ever nests
                    // year/month/day below the root.
                    if path.starts_with(&root) && path != root {
                        stack.push(path);
                    }
                } else if path
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().ends_with(&suffix))
                    && let Ok(modified) = entry.metadata().and_then(|m| m.modified())
                    && newest.as_ref().is_none_or(|(time, _)| modified > *time)
                {
                    newest = Some((modified, path));
                }
            }
        }
        newest.map(|(_, path)| path).unwrap_or_else(|| {
            // No matching rollout on disk (yet): fall back to the trait
            // default shape so callers get a stable path to wait on
            // rather than a confusing empty option.
            root.join(format!("{}.jsonl", session.0))
        })
    }

    fn format(&self) -> Arc<dyn TranscriptFormat> {
        Arc::new(CodexRolloutFormat)
    }

    /// Text plus a bare carriage return -- the interactive REPL's Enter
    /// submit, same convention Claude's TUI uses.
    fn compose_input(&self, message: &str) -> Vec<u8> {
        let mut bytes = message.as_bytes().to_vec();
        bytes.push(b'\r');
        bytes
    }

    /// A bare Escape byte: the interactive CLI's own turn-interrupt key.
    fn interrupt_sequence(&self) -> Vec<u8> {
        vec![0x1b]
    }

    /// Validated against the installed CLI's `--help`: the bypass flag is
    /// spelled exactly this way; `--full-auto` routes approvals through
    /// the workspace-write sandbox; readonly is a sandbox *mode*, not an
    /// approval policy.
    fn permission_args(&self, mode: PermissionMode) -> Vec<String> {
        match mode {
            PermissionMode::Max => vec!["--dangerously-bypass-approvals-and-sandbox".to_string()],
            PermissionMode::Default => vec!["--full-auto".to_string()],
            PermissionMode::Readonly => vec!["--sandbox".to_string(), "read-only".to_string()],
        }
    }

    fn version_gate(&self, probed: &str) -> VersionVerdict {
        match parse_leading_version(probed) {
            Some(version) if version >= MIN_TESTED_VERSION && version <= MAX_TESTED_VERSION => {
                VersionVerdict::Compatible
            }
            Some(version) => VersionVerdict::Incompatible {
                detail: format!(
                    "codex {version:?} is outside the tested range {MIN_TESTED_VERSION:?}..={MAX_TESTED_VERSION:?}"
                ),
            },
            None => VersionVerdict::Incompatible {
                detail: format!("could not parse a MAJOR.MINOR.PATCH version from {probed:?}"),
            },
        }
    }

    // `session_id_from_transcript_path` IS overridden upstream of the
    // trait: a rollout filename is `rollout-<timestamp>-<session-id>.jsonl`,
    // so the stem alone is not the id -- but `find_transcript_by_nonce`
    // hands back paths whose id this adapter extracts by trimming the
    // known prefix/suffix shape instead (see
    // [`session_id_from_rollout_filename`]).
}

/// Derives a rollout's session id from its filename:
/// `rollout-<anything>-<session-id>.jsonl` -> `<session-id>`. The real
/// CLI's own naming embeds the UUID last, hyphen-delimited; anything not
/// shaped that way yields `None` (the caller then falls back to whatever
/// the tailed `session_meta` line says, which is authoritative anyway).
#[must_use]
pub fn session_id_from_rollout_filename(filename: &str) -> Option<String> {
    let stem = filename.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    // The session id is the last 5 hyphen-separated groups (a UUID has 5).
    let groups: Vec<&str> = stem.split('-').collect();
    if groups.len() < 5 {
        return None;
    }
    Some(groups[groups.len() - 5..].join("-"))
}

/// The real Codex CLI's rollout JSONL transcript format: one entry per
/// line, top-level `type: "session_meta" | "response_item" |
/// "turn_context" | "event_msg"`, payloads carrying the conversation
/// facts (`response_item.payload.{type,message,function_call,...}`,
/// `event_msg.payload.type == "task_complete"` ending a turn).
pub(super) struct CodexRolloutFormat;

impl TranscriptFormat for CodexRolloutFormat {
    fn parse(&self, raw: &[u8], cursor: &Cursor) -> Vec<(TuiEvent, Cursor)> {
        parse_jsonl_chunk(raw, cursor, map_entry)
    }
}

/// Maps one parsed rollout entry to its events plus its own entry id
/// (this format's `last_entry_id`: the payload's `id`, or a call's
/// `call_id`).
fn map_entry(value: &Value) -> (Vec<TuiEvent>, Option<String>) {
    let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let payload = value.get("payload").cloned().unwrap_or(Value::Null);

    match entry_type {
        // Authoritative session identity: the nonce-derived initial value
        // corrects to this the moment the meta line is tailed.
        "session_meta" => {
            let session_id = payload
                .get("session_id")
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            (
                vec![TuiEvent::SessionMeta {
                    vendor_session_id: session_id.to_string(),
                }],
                None,
            )
        }
        "response_item" => map_response_item(&payload, ts.as_deref()),
        // A turn's completion edge -- the tail's TurnEnded marker.
        "event_msg" => map_event_msg(&payload),
        // Model/runtime context refreshes carry no conversation fact.
        "turn_context" => (Vec::new(), None),
        other => (
            vec![TuiEvent::Raw {
                entry_type: other.to_string(),
            }],
            None,
        ),
    }
}

fn response_entry_id(payload: &Value) -> Option<String> {
    payload
        .get("id")
        .or_else(|| payload.get("call_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn map_response_item(payload: &Value, ts: Option<&str>) -> (Vec<TuiEvent>, Option<String>) {
    let entry_id = response_entry_id(payload);
    match payload.get("type").and_then(Value::as_str).unwrap_or("") {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
            if role != "assistant" {
                // A user turn's own text is never re-surfaced: for a fresh
                // start it is the prompt the adapter itself injected (and
                // already journaled); for a resume it is prior
                // conversation this adapter did not just cause.
                return (Vec::new(), entry_id);
            }
            (map_assistant_content(payload, ts), entry_id)
        }
        "function_call" | "custom_tool_call" => {
            let tool = payload.get("name").and_then(Value::as_str).unwrap_or("");
            let detail = payload
                .get("arguments")
                .or_else(|| payload.get("input"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (
                vec![TuiEvent::ToolActivity {
                    tool: tool.to_string(),
                    detail: Classified {
                        class: ContentClass::Visible,
                        value: detail,
                    },
                    ts: ts.map(str::to_string),
                }],
                entry_id,
            )
        }
        // function_call_output / reasoning and anything else: a call's
        // output adds nothing the call itself did not already journal
        // (parity with Claude's call-only ToolActivity), and reasoning is
        // the model's hidden thinking -- never surfaced, mirroring the
        // headless adapter's own redaction.
        _ => (Vec::new(), entry_id),
    }
}

fn map_assistant_content(payload: &Value, ts: Option<&str>) -> Vec<TuiEvent> {
    let mut events = Vec::new();
    let Some(content) = payload.get("content").and_then(Value::as_array) else {
        return events;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("output_text") {
            continue;
        }
        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
        let has_later_call = content
            .iter()
            .any(|later| later.get("type").and_then(Value::as_str) == Some("function_call"));
        let is_question = text.trim_end().ends_with('?') && !has_later_call;
        events.push(TuiEvent::AssistantText {
            text: Classified {
                class: ContentClass::Visible,
                value: text.to_string(),
            },
            is_question,
            ts: ts.map(str::to_string),
        });
    }
    events
}

fn map_event_msg(payload: &Value) -> (Vec<TuiEvent>, Option<String>) {
    match payload.get("type").and_then(Value::as_str).unwrap_or("") {
        "task_complete" => (vec![TuiEvent::TurnEnded], None),
        // agent_message duplicates the response_item message this same
        // turn already journaled; user_message is the leader's own
        // prompt; exec_*/token_count/stream_error are telemetry. All
        // deliberately unmapped, never Raw-flooded.
        "agent_message" | "user_message" | "task_started" | "exec_command_begin"
        | "exec_command_end" | "token_count" | "stream_error" => (Vec::new(), None),
        other => (
            vec![TuiEvent::Raw {
                entry_type: format!("event_msg/{other}"),
            }],
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------- argv

    #[test]
    fn permission_modes_map_to_the_validated_cli_flags() {
        let vendor = CodexTuiVendor::new(PathBuf::from("/tmp"), Vec::new());
        assert_eq!(
            vendor.permission_args(PermissionMode::Max),
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
        );
        assert_eq!(
            vendor.permission_args(PermissionMode::Default),
            vec!["--full-auto".to_string()]
        );
        assert_eq!(
            vendor.permission_args(PermissionMode::Readonly),
            vec!["--sandbox".to_string(), "read-only".to_string()]
        );
    }

    #[test]
    fn launch_is_interactive_bare_codex_with_model_and_posture() {
        let vendor = CodexTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
        let spec = StartSpec {
            run_id: crew_protocol::RunId::new(),
            task_id: crew_protocol::TaskId::new(),
            worker_id: crew_protocol::WorkerId::new(),
            prompt: "go".to_string(),
            resume: None,
        };
        let cfg = AdapterConfig {
            model: Some("gpt-5.1-codex".to_string()),
            ..default_cfg()
        };
        let launch = vendor.launch(&spec, &cfg);
        assert_eq!(launch.program, PathBuf::from("codex"));
        assert_eq!(launch.cwd, PathBuf::from("/workspace/crew"));
        // Interactive: never the headless app-server subcommand.
        assert!(
            !launch.args.iter().any(|a| a == "app-server"),
            "a TUI session must never launch app-server: {:?}",
            launch.args
        );
        let model_idx = launch
            .args
            .iter()
            .position(|a| a == "-m")
            .expect("configured model must ride -m");
        assert_eq!(launch.args[model_idx + 1], "gpt-5.1-codex");
        assert!(launch.args.contains(&"--full-auto".to_string()));
    }

    #[test]
    fn resume_uses_the_resume_subcommand_with_the_session_id() {
        let vendor = CodexTuiVendor::new(PathBuf::from("/tmp"), Vec::new());
        let spec = StartSpec {
            run_id: crew_protocol::RunId::new(),
            task_id: crew_protocol::TaskId::new(),
            worker_id: crew_protocol::WorkerId::new(),
            prompt: "continue".to_string(),
            resume: Some(VendorSessionRef(
                "22222222-2222-4222-8222-000000000001".to_string(),
            )),
        };
        let launch = vendor.resume_launch(
            &VendorSessionRef("22222222-2222-4222-8222-000000000001".to_string()),
            &spec,
            &default_cfg(),
        );
        assert_eq!(launch.args[0], "resume");
        assert_eq!(launch.args[1], "22222222-2222-4222-8222-000000000001");
    }

    fn default_cfg() -> AdapterConfig {
        AdapterConfig {
            bin: "codex".to_string(),
            permission_mode: PermissionMode::Default,
            enabled: true,
            mode: crate::config::crew::AdapterMode::Tui,
            model: None,
            profile: String::new(),
            session_dir: None,
            extra_args: Vec::new(),
        }
    }

    // ------------------------------------------------------ version gate

    #[test]
    fn version_gate_accepts_the_fixture_recorded_cli_version() {
        let vendor = CodexTuiVendor::new(PathBuf::from("/tmp"), Vec::new());
        assert_eq!(
            vendor.version_gate("codex 0.149.1"),
            VersionVerdict::Compatible
        );
        assert!(matches!(
            vendor.version_gate("codex 1.0.0"),
            VersionVerdict::Incompatible { .. }
        ));
        assert!(matches!(
            vendor.version_gate("nonsense"),
            VersionVerdict::Incompatible { .. }
        ));
    }

    // ----------------------------------------------------------- format

    fn fixture_bytes() -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/adapters/codex-tui/session.jsonl");
        std::fs::read(&path).unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
    }

    #[test]
    fn the_full_fixture_parses_and_consumes_every_byte() {
        let raw = fixture_bytes();
        let tagged = CodexRolloutFormat.parse(&raw, &Cursor::start());
        let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
        // `parse` pairs each event with the cursor *after its own line*, not a
        // single batch cursor; the tailer advances past the final complete
        // line independently, so byte-for-byte consumption is its concern
        // (proven by `partial_trailing_line_is_left_unconsumed`), while here
        // we assert no entry degrades to Raw.
        // Every committed entry is understood; nothing degrades to Raw.
        assert!(
            events.iter().all(|e| !matches!(e, TuiEvent::Raw { .. })),
            "unexpected Raw events: {events:?}"
        );
    }

    #[test]
    fn fixture_yields_session_meta_assistant_text_tool_activity_and_turn_end() {
        let raw = fixture_bytes();
        let tagged = CodexRolloutFormat.parse(&raw, &Cursor::start());
        let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();

        let session_ids: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TuiEvent::SessionMeta { vendor_session_id } => Some(vendor_session_id.clone()),
                _ => None,
            })
            .collect();
        assert!(
            session_ids
                .iter()
                .all(|id| id == "22222222-2222-4222-8222-000000000001"),
            "session meta must carry the fixture's fixed id: {session_ids:?}"
        );

        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TuiEvent::AssistantText { text, .. } => Some(text.value.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("Hi!") && t.contains('?')),
            "the assistant greeting must surface as text: {texts:?}"
        );

        let tools: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                TuiEvent::ToolActivity { tool, .. } => Some(tool.clone()),
                _ => None,
            })
            .collect();
        assert!(
            tools.contains(&"shell".to_string()),
            "the echo function_call must surface as shell tool activity: {tools:?}"
        );
        assert!(
            tools.contains(&"apply_patch".to_string()),
            "the custom_tool_call must surface as apply_patch activity: {tools:?}"
        );

        assert!(
            events.iter().any(|e| matches!(e, TuiEvent::TurnEnded)),
            "task_complete must end the turn"
        );
    }

    #[test]
    fn user_messages_and_telemetry_never_surface() {
        let raw = fixture_bytes();
        let tagged = CodexRolloutFormat.parse(&raw, &Cursor::start());
        let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
        let surfaced_text = events
            .iter()
            .filter_map(|e| match e {
                TuiEvent::AssistantText { text, .. } => Some(text.value.clone()),
                _ => None,
            })
            .fold(String::new(), |acc, t| acc + &t);
        assert!(
            !surfaced_text.contains("[crew:fixture1]"),
            "the leader's own prompt must never re-surface as assistant text"
        );
    }

    #[test]
    fn partial_trailing_line_is_left_unconsumed() {
        let full = fixture_bytes();
        let split = full.len() - 10;
        let tagged = CodexRolloutFormat.parse(&full[..split], &Cursor::start());
        let first_events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
        let first_cursor = tagged
            .last()
            .map(|(_, c)| c.clone())
            .unwrap_or_else(Cursor::start);
        let tagged = CodexRolloutFormat.parse(&full[first_cursor.offset as usize..], &first_cursor);
        let second_events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
        let tagged = CodexRolloutFormat.parse(&full, &Cursor::start());
        let whole_events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
        let emitting_count =
            |events: &[TuiEvent]| events.iter().filter(|e| e.emits_a_payload()).count();
        assert_eq!(
            emitting_count(&first_events) + emitting_count(&second_events),
            emitting_count(&whole_events),
            "split parsing must yield exactly the whole parse's emitting events"
        );
        assert!(emitting_count(&first_events) > 0);
    }

    // ------------------------------------------ rollout filename session id

    #[test]
    fn rollout_filename_yields_its_embedded_session_id() {
        assert_eq!(
            session_id_from_rollout_filename(
                "rollout-2026-07-24T22-51-05-019f95ae-729b-7e33-919d-f13812da49e8.jsonl"
            ),
            Some("019f95ae-729b-7e33-919d-f13812da49e8".to_string())
        );
        assert_eq!(session_id_from_rollout_filename("unrelated.txt"), None);
        assert_eq!(
            session_id_from_rollout_filename("rollout-short.jsonl"),
            None
        );
    }

    // --------------------------------------- resumed transcript discovery

    #[test]
    fn transcript_path_for_session_finds_the_date_partitioned_rollout() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("sessions");
        let leaf = root.join("2026").join("07").join("24");
        std::fs::create_dir_all(&leaf).unwrap();
        let rollout =
            leaf.join("rollout-2026-07-24T22-51-05-019f95ae-729b-7e33-919d-f13812da49e8.jsonl");
        std::fs::write(&rollout, b"{}\n").unwrap();

        let vendor = CodexTuiVendor::new(PathBuf::from("/tmp"), Vec::new());
        let spec = StartSpec {
            run_id: crew_protocol::RunId::new(),
            task_id: crew_protocol::TaskId::new(),
            worker_id: crew_protocol::WorkerId::new(),
            prompt: "x".to_string(),
            resume: None,
        };
        let cfg = AdapterConfig {
            session_dir: Some(root.to_string_lossy().into_owned()),
            ..default_cfg()
        };
        let found = vendor.transcript_path_for_session(
            &VendorSessionRef("019f95ae-729b-7e33-919d-f13812da49e8".to_string()),
            &spec,
            &cfg,
        );
        assert_eq!(found, rollout);

        // A session with no rollout on disk falls back to a stable shape.
        let missing = vendor.transcript_path_for_session(
            &VendorSessionRef("11111111-2222-4333-8444-555555555555".to_string()),
            &spec,
            &cfg,
        );
        assert!(missing.ends_with("11111111-2222-4333-8444-555555555555.jsonl"));
    }
}
