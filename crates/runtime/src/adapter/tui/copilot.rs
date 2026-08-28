//! The GitHub Copilot CLI's [`TuiVendor`] implementation: interactive
//! `copilot` (never `-p`), session JSONL transcript tailing under
//! `~/.copilot/session-state/`, and this vendor's own permission-mode
//! argv/compose-input/interrupt conventions.
//!
//! Every flag here was validated against an installed `copilot` CLI's
//! own `--help` output (version `1.0.80`, one of the exact versions in
//! `super::copilot_compatibility::COPILOT_KNOWN_CLI_VERSIONS`: `-m/--model
//! <model>`, `--allow-all-tools`, `--deny-tool[=tools...]`,
//! `-r/--resume[=value]`), and the transcript shapes against a real
//! recorded session on this machine (`~/.copilot/session-state/<session-
//! id>.jsonl`) plus the committed *synthetic* fixture
//! (`fixtures/adapters/copilot-tui/session.jsonl`). The plan's guessed
//! root (`history-session-state`) does not exist on a real install; the
//! observed layout wins (the brownfield rule). Decisions the fixture
//! cannot itself prove are called out in this module's doc comments
//! rather than left implicit.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use crew_protocol::{Classified, ContentClass};

use crate::adapter::r#trait::{StartSpec, VendorSessionRef};
use crate::adapter::tui::copilot_compatibility as compatibility;
use crate::config::crew::{AdapterConfig, PermissionMode};
use crate::supervisor::EnvironmentPolicy;

use super::adapter::{LaunchSpec, TuiVendor, VersionVerdict};
use super::{Cursor, TranscriptFormat, TuiEvent, parse_jsonl_chunk};

/// The Copilot CLI, driven interactively over a real PTY.
pub struct CopilotTuiVendor {
    /// The working directory the vendor process is launched in.
    cwd: PathBuf,
    /// `WorkerProfile::environmentAllowlist` -- variable *names* only,
    /// exactly like [`super::claude::ClaudeTuiVendor`]'s field.
    environment_allowlist: Vec<String>,
}

impl CopilotTuiVendor {
    #[must_use]
    pub fn new(cwd: PathBuf, environment_allowlist: Vec<String>) -> Self {
        Self {
            cwd,
            environment_allowlist,
        }
    }

    /// Same policy as Claude's and Codex's vendors:
    /// [`EnvironmentPolicy::baseline()`] plus this profile's allowlisted
    /// names -- never an inherited secret-shaped variable. `HOME` is
    /// load-bearing here too: the real CLI resolves its own
    /// `~/.copilot/session-state/` transcript root the same way
    /// [`CopilotTuiVendor::transcript_root`] does.
    fn env(&self) -> std::collections::HashMap<String, String> {
        let current: std::collections::HashMap<String, String> = std::env::vars().collect();
        EnvironmentPolicy::baseline().build(&current, &self.environment_allowlist)
    }

    /// The base argv every launch (fresh or resumed) shares: this
    /// vendor's own permission-mode flag, `--model` when configured
    /// (validated against the installed CLI's `--help`: `--model <model>`
    /// exists on 1.0.80), and `cfg.extra_args` verbatim -- appended last
    /// so an operator's own flag can still override one of these.
    fn base_args(&self, cfg: &AdapterConfig) -> Vec<String> {
        let mut args = self.permission_args(cfg.permission_mode);
        if let Some(model) = &cfg.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        args.extend(cfg.extra_args.iter().cloned());
        args
    }
}

impl TuiVendor for CopilotTuiVendor {
    fn kind(&self) -> &'static str {
        "copilot"
    }

    /// Interactive bare `copilot`, deliberately never `-p`/`--prompt`:
    /// that is the headless one-shot mode; a TUI session must launch the
    /// real interactive REPL so a human attached to its pane sees (and
    /// can type into) the exact same session this adapter tails.
    fn launch(&self, _spec: &StartSpec, cfg: &AdapterConfig) -> LaunchSpec {
        LaunchSpec {
            program: PathBuf::from(&cfg.bin),
            args: self.base_args(cfg),
            cwd: self.cwd.clone(),
            env: self.env(),
        }
    }

    /// `copilot --resume=<session-id>`. Validated against the installed
    /// CLI's `--help`: resume is spelled `-r, --resume[=value]` --
    /// clap-style *optional-value* syntax, which means the session id
    /// MUST travel inside the same argv token (`--resume=<id>`); a
    /// space-separated form would be parsed as "flag with no value",
    /// launching an interactive session picker instead. Plus the same
    /// permission/model/extra-args base every fresh launch gets -- a
    /// resumed session is still launched under whatever posture this
    /// run's config asks for.
    fn resume_launch(
        &self,
        session: &VendorSessionRef,
        _spec: &StartSpec,
        cfg: &AdapterConfig,
    ) -> LaunchSpec {
        let mut args = vec![format!("--resume={}", session.0)];
        args.extend(self.base_args(cfg));
        LaunchSpec {
            program: PathBuf::from(&cfg.bin),
            args,
            cwd: self.cwd.clone(),
            env: self.env(),
        }
    }

    /// `cfg.session_dir` overrides everything; otherwise
    /// `~/.copilot/session-state/` -- observed on a real install, where
    /// each session is a flat `<session-id>.jsonl` directly under that
    /// root (per-session *directories* also exist there, carrying
    /// checkpoints/workspace state, but the transcript itself is the
    /// flat file). `$HOME` unresolved falls back to `/root`, mirroring
    /// Claude's fallback.
    fn transcript_root(&self, _spec: &StartSpec, cfg: &AdapterConfig) -> PathBuf {
        if let Some(dir) = &cfg.session_dir {
            return PathBuf::from(dir);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        PathBuf::from(home).join(".copilot").join("session-state")
    }

    // `transcript_path_for_session` is not overridden: the real CLI's
    // transcript filename stem IS its session id (a UUID) -- the trait's
    // default `<root>/<session-id>.jsonl` is exactly the observed layout.

    fn format(&self) -> Arc<dyn TranscriptFormat> {
        Arc::new(CopilotSessionFormat)
    }

    /// Text plus a bare carriage return -- the interactive REPL's Enter
    /// submit, same convention Claude's and Codex's TUIs use.
    fn compose_input(&self, message: &str) -> Vec<u8> {
        let mut bytes = message.as_bytes().to_vec();
        bytes.push(b'\r');
        bytes
    }

    /// A bare Escape byte: the interactive CLI's turn-interrupt key,
    /// same convention as the other vendored REPLs. [INFERENCE] not
    /// separately confirmed against a live session -- WP29's live smoke
    /// only ever exercised `CancelScope::Worker` (process-kill); this
    /// turn-level interrupt remains unconfirmed live, tracked post-0.5.0.
    fn interrupt_sequence(&self) -> Vec<u8> {
        vec![0x1b]
    }

    /// Validated against the installed CLI's `--help`: max is
    /// `--allow-all-tools`; readonly denies the write tool via the
    /// optional-value flag (`--deny-tool=write`, same single-token rule
    /// as resume); Default passes no flag at all so the CLI keeps its
    /// own interactive approval prompts.
    fn permission_args(&self, mode: PermissionMode) -> Vec<String> {
        match mode {
            PermissionMode::Max => vec!["--allow-all-tools".to_string()],
            PermissionMode::Readonly => vec!["--deny-tool=write".to_string()],
            PermissionMode::Default => Vec::new(),
        }
    }

    /// The headless adapter's exact-match empirical gate
    /// ([`compatibility::COPILOT_KNOWN_CLI_VERSIONS`]), reused verbatim:
    /// a version is compatible only when it is one of the exact strings
    /// empirically verified with a real ACP handshake -- never a range
    /// extrapolation like the other vendors' gates, because this vendor
    /// ships breaking protocol changes between patch releases. The
    /// probed string embeds the version somewhere in prose ("GitHub
    /// Copilot CLI 1.0.80."), so the check is a substring match per
    /// known entry rather than a leading-token parse.
    fn version_gate(&self, probed: &str) -> VersionVerdict {
        let known = compatibility::COPILOT_KNOWN_CLI_VERSIONS
            .iter()
            .map(|entry| entry.cli_version)
            .find(|version| probed.contains(version));
        match known {
            Some(_) => VersionVerdict::Compatible,
            None => VersionVerdict::Incompatible {
                detail: format!(
                    "copilot probe {probed:?} matches none of the empirically verified \
                     versions {:?}; see tui::copilot_compatibility",
                    compatibility::COPILOT_KNOWN_CLI_VERSIONS
                        .iter()
                        .map(|entry| entry.cli_version)
                        .collect::<Vec<_>>()
                ),
            },
        }
    }

    // `session_id_from_transcript_path` is not overridden: the filename
    // stem IS the session id, exactly what the trait default derives
    // (and the tailed `session.start` line remains authoritative).
}

/// The real Copilot CLI's session JSONL transcript format: one entry per
/// line under `~/.copilot/session-state/<session-id>.jsonl`, each entry
/// `{"type": ..., "data": {...}, "id"/"timestamp"/"parentId" ...}`.
/// Observed types: `session.start` (authoritative session identity),
/// `user.message`, `assistant.message` (`content` prose plus
/// `toolRequests[]` calls), `assistant.turn_start`/`turn_end`,
/// `tool.execution_start`/`execution_complete`, `session.resume`,
/// `session.truncation`.
struct CopilotSessionFormat;

impl TranscriptFormat for CopilotSessionFormat {
    fn parse(&self, raw: &[u8], cursor: &Cursor) -> Vec<(TuiEvent, Cursor)> {
        parse_jsonl_chunk(raw, cursor, map_entry)
    }
}

/// Maps one parsed transcript entry to its events plus its own `id`
/// (this format's `last_entry_id`, when the entry carries one).
fn map_entry(value: &Value) -> (Vec<TuiEvent>, Option<String>) {
    let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let data = value.get("data").cloned().unwrap_or(Value::Null);

    match entry_type {
        // Authoritative session identity: the nonce-derived initial value
        // corrects to this the moment the meta line is tailed.
        "session.start" => (
            vec![TuiEvent::SessionMeta {
                vendor_session_id: data
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }],
            value.get("id").and_then(Value::as_str).map(str::to_string),
        ),
        "assistant.message" => {
            let events = map_assistant_message(&data, ts.as_deref());
            (
                events,
                value.get("id").and_then(Value::as_str).map(str::to_string),
            )
        }
        // The assistant's own turn boundary.
        "assistant.turn_end" => (vec![TuiEvent::TurnEnded], None),
        // Deliberately unmapped telemetry: the user's own echoed prompt,
        // turn starts, redundant per-tool execution bookkeeping (the
        // call itself already surfaced from `toolRequests`),
        // truncation/resume housekeeping. Never surfaced, never an
        // error -- mirroring codex's token_count/exec_* handling.
        _ => (Vec::new(), None),
    }
}

/// Maps one assistant message's payload to events, in order: the prose
/// `content` first (question-detected per Claude's heuristic -- trimmed
/// text ending in `?` **and** no tool request accompanying it, since a
/// message that also carries tool calls is narrating work, not asking),
/// then each `toolRequests[]` entry as [`TuiEvent::ToolActivity`]
/// (`detail` is the request's JSON arguments compactly re-serialized --
/// the transcript carries no separate result block this mapping trusts,
/// mirroring how the other vendors surface the call itself).
fn map_assistant_message(data: &Value, ts: Option<&str>) -> Vec<TuiEvent> {
    let mut events = Vec::new();
    let content = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let requests = data.get("toolRequests").and_then(Value::as_array);
    let has_tools = requests.is_some_and(|requests| !requests.is_empty());
    if !content.is_empty() {
        let trimmed = content.trim_end();
        events.push(TuiEvent::AssistantText {
            text: Classified {
                class: ContentClass::Visible,
                value: content.to_string(),
            },
            is_question: trimmed.ends_with('?') && !has_tools,
            ts: ts.map(str::to_string),
        });
    }
    for request in requests.into_iter().flatten() {
        let tool = request
            .get("name")
            .or_else(|| request.get("toolName"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let detail = request
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        events.push(TuiEvent::ToolActivity {
            tool: tool.to_string(),
            detail: Classified {
                class: ContentClass::Visible,
                value: serde_json::to_string(&detail).unwrap_or_else(|_| "{}".to_string()),
            },
            ts: ts.map(str::to_string),
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_cfg() -> AdapterConfig {
        AdapterConfig {
            enabled: true,
            bin: "copilot".to_string(),
            mode: crate::config::crew::AdapterMode::Tui,
            permission_mode: PermissionMode::Max,
            model: Some("gpt-5.4".to_string()),
            profile: "test".to_string(),
            session_dir: None,
            extra_args: vec!["--no-remote".to_string()],
        }
    }

    fn spec() -> StartSpec {
        StartSpec {
            run_id: crew_protocol::RunId::new(),
            task_id: crew_protocol::TaskId::new(),
            worker_id: crew_protocol::WorkerId::new(),
            prompt: "p".to_string(),
            resume: None,
        }
    }

    #[test]
    fn kind_is_the_copilot_wire_name() {
        assert_eq!(
            CopilotTuiVendor::new(PathBuf::from("/w"), vec![]).kind(),
            "copilot"
        );
    }

    #[test]
    fn permission_modes_map_to_the_validated_cli_flags() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        assert_eq!(
            vendor.permission_args(PermissionMode::Max),
            vec!["--allow-all-tools".to_string()]
        );
        assert_eq!(
            vendor.permission_args(PermissionMode::Readonly),
            vec!["--deny-tool=write".to_string()]
        );
        assert_eq!(
            vendor.permission_args(PermissionMode::Default),
            Vec::<String>::new()
        );
    }

    #[test]
    fn launch_is_interactive_bare_copilot_with_model_and_posture() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
        let launch = vendor.launch(&spec(), &default_cfg());
        assert_eq!(launch.program, PathBuf::from("copilot"));
        assert_eq!(launch.cwd, PathBuf::from("/workspace/crew"));
        // Interactive: never -p/--prompt/--print, never a subcommand.
        for forbidden in ["-p", "--prompt", "--print"] {
            assert!(
                !launch.args.iter().any(|a| a == forbidden),
                "argv must never contain {forbidden}: {:?}",
                launch.args
            );
        }
        let expected: &[&str] = &["--allow-all-tools", "--model", "gpt-5.4", "--no-remote"];
        for want in expected {
            assert!(
                launch.args.iter().any(|a| a == want),
                "argv missing {want}: {:?}",
                launch.args
            );
        }
    }

    #[test]
    fn resume_uses_the_optional_value_flag_in_a_single_token() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let launch = vendor.resume_launch(
            &VendorSessionRef("abc-def".to_string()),
            &spec(),
            &default_cfg(),
        );
        // The id MUST be inside the same token: --resume has clap-style
        // optional-value syntax, so a space-separated form would not bind.
        assert_eq!(launch.args[0], "--resume=abc-def");
        assert!(
            launch.args.windows(2).all(|w| w[0] != "--resume"),
            "no bare --resume flag may precede the id as a separate token"
        );
    }

    #[test]
    fn transcript_root_honors_session_dir_override_and_home_default() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let mut cfg = default_cfg();
        cfg.session_dir = Some("/tmp/sessions".to_string());
        assert_eq!(
            vendor.transcript_root(&spec(), &cfg),
            PathBuf::from("/tmp/sessions")
        );
        cfg.session_dir = None;
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        assert_eq!(
            vendor.transcript_root(&spec(), &cfg),
            PathBuf::from(home).join(".copilot").join("session-state")
        );
    }

    #[test]
    fn transcript_path_for_session_is_the_flat_root_layout() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let mut cfg = default_cfg();
        cfg.session_dir = Some("/tmp/sessions".to_string());
        let path = vendor.transcript_path_for_session(
            &VendorSessionRef("9447bcbe-43b1".to_string()),
            &spec(),
            &cfg,
        );
        assert_eq!(
            path,
            PathBuf::from("/tmp/sessions/9447bcbe-43b1.jsonl"),
            "observed real layout: <session-id>.jsonl directly under the root"
        );
    }

    #[test]
    fn version_gate_accepts_only_empirically_verified_versions() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        // The installed CLI's own prose form.
        assert_eq!(
            vendor.version_gate("GitHub Copilot CLI 1.0.80."),
            VersionVerdict::Compatible
        );
        assert_eq!(vendor.version_gate("1.0.73"), VersionVerdict::Compatible);
        // An unverifiable newer patch release must NOT pass the exact-
        // match gate (this vendor's documented hard-gate policy).
        assert!(matches!(
            vendor.version_gate("GitHub Copilot CLI 9.9.99."),
            VersionVerdict::Incompatible { .. }
        ));
        assert!(matches!(
            vendor.version_gate(""),
            VersionVerdict::Incompatible { .. }
        ));
    }

    #[test]
    fn compose_input_appends_a_carriage_return_and_interrupt_is_escape() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        assert_eq!(vendor.compose_input("hi"), b"hi\r".to_vec());
        assert_eq!(vendor.interrupt_sequence(), vec![0x1b]);
    }

    fn fixture_bytes() -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/adapters/copilot-tui/session.jsonl");
        std::fs::read(&path).unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
    }

    #[test]
    fn the_full_fixture_parses_and_consumes_every_byte() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let tagged = vendor.format().parse(&fixture_bytes(), &Cursor::start());
        let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
        let cursor = tagged
            .last()
            .map(|(_, c)| c.clone())
            .unwrap_or_else(Cursor::start);
        assert!(
            !events.is_empty(),
            "the fixture must normalize to at least one event"
        );
        assert_eq!(
            cursor.offset as usize,
            fixture_bytes().len(),
            "a fixture with no trailing partial line must consume every byte"
        );
    }

    #[test]
    fn fixture_yields_session_meta_assistant_text_tool_activity_and_turn_end() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let tagged = vendor.format().parse(&fixture_bytes(), &Cursor::start());
        let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TuiEvent::SessionMeta { vendor_session_id }
                if vendor_session_id == "55555555-5555-4555-8555-000000000001")),
            "session.start must yield the authoritative SessionMeta: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                TuiEvent::AssistantText {
                    is_question: true,
                    ..
                }
            )),
            "the fixture's closing question must be question-detected"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TuiEvent::ToolActivity { tool, .. } if tool == "bash")),
            "an assistant message's toolRequests must surface as ToolActivity"
        );
        assert!(
            events.iter().any(|e| matches!(e, TuiEvent::TurnEnded)),
            "assistant.turn_end must map to TurnEnded"
        );
    }

    #[test]
    fn user_messages_and_telemetry_never_surface() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let raw = concat!(
            "{\"type\":\"user.message\",\"data\":{\"content\":\"echoed user text\"},\"id\":\"u1\"}\n",
            "{\"type\":\"assistant.turn_start\",\"data\":{\"turnId\":\"0\"}}\n",
            "{\"type\":\"tool.execution_complete\",\"data\":{\"toolCallId\":\"t1\"}}\n",
            "{\"type\":\"session.truncation\",\"data\":{}}\n",
            "{\"type\":\"assistant.message\",\"data\":{\"content\":\"visible reply?\",\"toolRequests\":[]},\"id\":\"a1\"}\n",
        );
        let tagged = vendor.format().parse(raw.as_bytes(), &Cursor::start());
        let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
        assert_eq!(
            events.len(),
            1,
            "echoed user text, turn starts, redundant per-tool bookkeeping, and \
             truncation housekeeping must never surface: {events:?}"
        );
        assert!(
            matches!(&events[0], TuiEvent::AssistantText { text, is_question: true, .. }
                if text.value == "visible reply?")
        );
    }

    #[test]
    fn narration_with_tool_requests_is_not_a_question_even_when_it_ends_in_one() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let raw = concat!(
            "{\"type\":\"assistant.message\",\"data\":{\"content\":\"Shall I run git status next?\",",
            "\"toolRequests\":[{\"toolCallId\":\"t1\",\"name\":\"bash\",\"arguments\":{\"command\":\"git status\"}}]},\"id\":\"a1\"}\n",
        );
        let tagged = vendor.format().parse(raw.as_bytes(), &Cursor::start());
        let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();
        assert_eq!(events.len(), 2);
        assert!(
            matches!(
                &events[0],
                TuiEvent::AssistantText {
                    is_question: false,
                    ..
                }
            ),
            "text accompanied by tool calls is narration, never a question"
        );
        assert!(matches!(&events[1], TuiEvent::ToolActivity { tool, .. } if tool == "bash"));
    }

    #[test]
    fn malformed_lines_degrade_to_raw_not_errors() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let raw = b"{not json at all\n{\"type\":\"assistant.turn_end\",\"data\":{}}\n";
        let tagged = vendor.format().parse(raw, &Cursor::start());
        let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
        let cursor = tagged
            .last()
            .map(|(_, c)| c.clone())
            .unwrap_or_else(Cursor::start);
        assert!(matches!(&events[0], TuiEvent::Raw { .. }));
        assert!(matches!(&events[1], TuiEvent::TurnEnded));
        assert_eq!(cursor.offset as usize, raw.len());
    }

    #[test]
    fn partial_trailing_line_is_left_unconsumed() {
        let vendor = CopilotTuiVendor::new(PathBuf::from("/w"), vec![]);
        let raw = b"{\"type\":\"assistant.turn_end\",\"data\":{}}\n{\"type\":\"assistant.mess";
        let tagged = vendor.format().parse(raw, &Cursor::start());
        let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
        let cursor = tagged
            .last()
            .map(|(_, c)| c.clone())
            .unwrap_or_else(Cursor::start);
        assert!(matches!(&events[0], TuiEvent::TurnEnded));
        assert_eq!(
            cursor.offset as usize,
            raw.len() - b"{\"type\":\"assistant.mess".len(),
            "the partial trailing line must stay unconsumed for the next poll"
        );
    }
}
