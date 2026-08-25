//! The Claude CLI's [`TuiVendor`] implementation: interactive `claude`
//! (never `-p`), session JSONL transcript tailing under
//! `~/.claude/projects/<cwd slug>`, and this vendor's own permission-mode
//! argv/compose-input/interrupt conventions.
//!
//! Validated against a real captured session where possible (see
//! `fixtures/adapters/claude-tui/`'s own provenance header); every
//! decision below that the fixture cannot itself prove is called out in
//! this module's own doc comments rather than left implicit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crew_protocol::{Classified, ContentClass};

use crate::adapter::r#trait::{StartSpec, VendorSessionRef};
use crate::config::crew::{AdapterConfig, PermissionMode};
use crate::supervisor::EnvironmentPolicy;

use super::adapter::{LaunchSpec, TuiVendor, VersionVerdict};
use super::{Cursor, TranscriptFormat, TuiEvent, parse_jsonl_chunk};

/// The `claude --version` range this adapter's fixed argv and transcript-
/// format assumptions were built and tested against. A probed version
/// outside this range is reported [`VersionVerdict::Incompatible`] rather
/// than assumed compatible -- widen it only after a live smoke run
/// (WP29) confirms a newer/older release still matches this module's
/// argv and JSONL assumptions.
///
/// The one version this range is actually validated against is
/// `2.1.241` -- `fixtures/adapters/claude-tui/session.jsonl`'s own
/// recorded `claude --version` (see that fixture's `README.md`). The
/// rest of the range is an untested extrapolation, not a second data
/// point.
const MIN_TESTED_VERSION: (u32, u32, u32) = (1, 0, 0);
const MAX_TESTED_VERSION: (u32, u32, u32) = (2, 99, 99);

/// Parses the leading `MAJOR.MINOR.PATCH` out of a `claude --version`
/// string (e.g. `"1.2.3 (Claude Code)"` -> `(1, 2, 3)`). Returns `None`
/// for anything that does not start with three dot-separated integers --
/// [`ClaudeTuiVendor::version_gate`] treats an unparseable string as
/// incompatible, never as a silent pass.
fn parse_leading_version(probed: &str) -> Option<(u32, u32, u32)> {
    let first_token = probed.split_whitespace().next()?;
    let mut parts = first_token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// `/` and `.` in an absolute cwd path both become `-`, matching the real
/// `claude` CLI's own project-directory naming under
/// `~/.claude/projects/`. Pure string transformation -- never touches
/// the filesystem itself.
#[must_use]
pub fn slug_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// The Claude CLI, driven interactively over a real PTY.
pub struct ClaudeTuiVendor {
    /// The working directory the vendor process is launched in, and the
    /// input [`slug_cwd`] slugs to find this session's transcript
    /// directory under `~/.claude/projects/`.
    cwd: PathBuf,
    /// `WorkerProfile::environmentAllowlist` -- variable *names* only,
    /// mirroring `ClaudeAdapter::new`'s own field exactly (see its own
    /// doc comment on why a value can never reach this type).
    environment_allowlist: Vec<String>,
}

impl ClaudeTuiVendor {
    #[must_use]
    pub fn new(cwd: PathBuf, environment_allowlist: Vec<String>) -> Self {
        Self {
            cwd,
            environment_allowlist,
        }
    }

    /// `EnvironmentPolicy::baseline()` (`HOME`, `PATH`, locale, terminal
    /// identity, approved `XDG_*`) plus this vendor's own allowlisted
    /// names -- never an inherited secret-shaped variable the profile did
    /// not explicitly name. `HOME` in particular is load-bearing here:
    /// the real `claude` CLI resolves its own `~/.claude/projects/`
    /// transcript root the same way [`ClaudeTuiVendor::transcript_root`]
    /// does, so a process launched without it would write its transcript
    /// somewhere this adapter's own discovery/tailing would never look.
    fn env(&self) -> HashMap<String, String> {
        let current: HashMap<String, String> = std::env::vars().collect();
        EnvironmentPolicy::baseline().build(&current, &self.environment_allowlist)
    }

    /// The base argv every launch (fresh or resumed) shares: this
    /// vendor's own permission-mode flag, `--model` when configured, and
    /// `cfg.extra_args` verbatim -- appended last, so an operator's own
    /// flag can still override one of these if the real CLI allows
    /// repeated flags to win by last-occurrence (never suppressed by this
    /// adapter to make room).
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

impl TuiVendor for ClaudeTuiVendor {
    fn kind(&self) -> &'static str {
        "claude"
    }

    /// Interactive `claude`, deliberately never `-p`/`--print`: `-p` is
    /// the headless one-shot mode `ClaudeAdapter` already drives; a TUI
    /// session must launch the real interactive REPL so a human attached
    /// to its pane sees (and can type into) the exact same session this
    /// adapter tails.
    fn launch(&self, _spec: &StartSpec, cfg: &AdapterConfig) -> LaunchSpec {
        LaunchSpec {
            program: PathBuf::from(&cfg.bin),
            args: self.base_args(cfg),
            cwd: self.cwd.clone(),
            env: self.env(),
        }
    }

    /// `claude --resume <session-id>`, plus the same permission/model/
    /// extra-args base every fresh launch gets -- a resumed session is
    /// still launched under whatever posture this run's config asks for,
    /// not whatever the original session happened to use.
    fn resume_launch(
        &self,
        session: &VendorSessionRef,
        _spec: &StartSpec,
        cfg: &AdapterConfig,
    ) -> LaunchSpec {
        let mut args = vec!["--resume".to_string(), session.0.clone()];
        args.extend(self.base_args(cfg));
        LaunchSpec {
            program: PathBuf::from(&cfg.bin),
            args,
            cwd: self.cwd.clone(),
            env: self.env(),
        }
    }

    /// `cfg.session_dir` overrides everything (an operator-configured
    /// path); otherwise `~/.claude/projects/<slug_cwd(canonicalized
    /// self.cwd)>`, the real CLI's own on-disk layout. The real CLI
    /// slugs the *canonicalized* cwd, not whatever path string it was
    /// launched with -- confirmed against a real recording, where `/tmp`
    /// (a symlink to `/private/tmp` on macOS) produced the slug
    /// `-private-tmp-...`, never `-tmp-...`. `fs::canonicalize` failing
    /// (the path does not exist, unlikely for a real supervised
    /// process's own cwd, but never assumed) falls back to `self.cwd`
    /// uncanonicalized rather than erroring: a wrong root just means
    /// discovery times out with a clear, typed error, never a crash.
    /// `$HOME` unresolved falls back to `/root` for the same reason.
    fn transcript_root(&self, _spec: &StartSpec, cfg: &AdapterConfig) -> PathBuf {
        if let Some(dir) = &cfg.session_dir {
            return PathBuf::from(dir);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let canonical_cwd = std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone());
        PathBuf::from(home)
            .join(".claude")
            .join("projects")
            .join(slug_cwd(&canonical_cwd))
    }

    fn format(&self) -> Arc<dyn TranscriptFormat> {
        Arc::new(ClaudeTranscriptFormat)
    }

    /// Text plus a bare carriage return -- the real CLI's own submit
    /// convention for its interactive prompt (never a trailing `\n`,
    /// which the terminal-raw-mode REPL does not treat as submit).
    fn compose_input(&self, message: &str) -> Vec<u8> {
        let mut bytes = message.as_bytes().to_vec();
        bytes.push(b'\r');
        bytes
    }

    /// A bare Escape byte: the real CLI's own turn-interrupt key.
    fn interrupt_sequence(&self) -> Vec<u8> {
        vec![0x1b]
    }

    fn permission_args(&self, mode: PermissionMode) -> Vec<String> {
        match mode {
            PermissionMode::Max => vec!["--dangerously-skip-permissions".to_string()],
            PermissionMode::Readonly => {
                vec!["--permission-mode".to_string(), "plan".to_string()]
            }
            PermissionMode::Default => Vec::new(),
        }
    }

    fn version_gate(&self, probed: &str) -> VersionVerdict {
        match parse_leading_version(probed) {
            Some(version) if version >= MIN_TESTED_VERSION && version <= MAX_TESTED_VERSION => {
                VersionVerdict::Compatible
            }
            Some(version) => VersionVerdict::Incompatible {
                detail: format!(
                    "claude {version:?} is outside the tested range {MIN_TESTED_VERSION:?}..={MAX_TESTED_VERSION:?}"
                ),
            },
            None => VersionVerdict::Incompatible {
                detail: format!("could not parse a MAJOR.MINOR.PATCH version from {probed:?}"),
            },
        }
    }

    // `session_id_from_transcript_path` is not overridden: the real
    // CLI's own transcript filename stem *is* its session id (a UUID),
    // exactly what the trait's default implementation derives.
}

/// The real `claude` CLI's session JSONL transcript format: one entry
/// per line, `type: "user" | "assistant" | "summary"`, an assistant
/// entry's `message.content` an array of blocks (`text`, `tool_use`, and
/// others this adapter deliberately never surfaces -- see
/// [`map_assistant_content`]'s own doc comment), a `sessionId` field
/// (present on at least user/assistant entries), and a `timestamp`
/// field.
struct ClaudeTranscriptFormat;

impl TranscriptFormat for ClaudeTranscriptFormat {
    fn parse(&self, raw: &[u8], cursor: &Cursor) -> (Vec<TuiEvent>, Cursor) {
        parse_jsonl_chunk(raw, cursor, map_entry)
    }
}

/// Maps one parsed transcript entry to its events plus its own `uuid`
/// (this format's `last_entry_id`, when the entry carries one).
fn map_entry(value: &Value) -> (Vec<TuiEvent>, Option<String>) {
    let entry_id = value
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let session_id = value.get("sessionId").and_then(Value::as_str);

    let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let mut events: Vec<TuiEvent> = Vec::new();
    if let Some(session_id) = session_id {
        events.push(TuiEvent::SessionMeta {
            vendor_session_id: session_id.to_string(),
        });
    }

    match entry_type {
        // A user turn's own text is never re-surfaced here: for a fresh
        // start it is exactly the prompt `TuiAdapter::run_pipeline`
        // itself already injected (and, for `send()`, already journaled
        // separately by the adapter shell before writing to the pty);
        // for a resumed session it is prior conversation this adapter
        // did not just cause -- neither belongs on this tail. Extracting
        // `sessionId` above (the one thing a user entry usefully carries
        // for this adapter) already happened.
        "user" => {}
        "assistant" => {
            if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
                events.extend(map_assistant_content(content, ts.as_deref()));
            }
        }
        // A vendor-generated conversation summary (compaction); this
        // adapter has no `AdapterEventPayload` shape for one and nothing
        // downstream consumes it yet -- a known, intentionally-unmapped
        // entry type, not an unrecognized one.
        "summary" => {}
        other => events.push(TuiEvent::Raw {
            entry_type: other.to_string(),
        }),
    }

    (events, entry_id)
}

/// Maps one assistant entry's content blocks to events, in order.
///
/// `text` -> [`TuiEvent::AssistantText`], question-detected per the exact
/// heuristic this vendor was specified against: the block's text (right-
/// trimmed) ends in `?` **and** no `tool_use` block follows it later in
/// this *same* content array -- a text block immediately followed by a
/// tool call is the model narrating its next action, not asking the
/// human anything, even when that narration happens to end in a
/// question mark. `tool_use` -> [`TuiEvent::ToolActivity`] (`detail` is
/// the tool's JSON `input`, compactly re-serialized -- the transcript
/// carries no separate completion/result block this format maps, so
/// `detail` describes the call itself). Every other block type
/// (`thinking` in particular) is silently skipped: real Claude Code
/// sessions carry the model's hidden reasoning as its own content block,
/// and this format must never surface it, mirroring the headless
/// adapter's own thinking-block redaction exactly.
fn map_assistant_content(content: &[Value], ts: Option<&str>) -> Vec<TuiEvent> {
    let mut events = Vec::new();
    for (index, block) in content.iter().enumerate() {
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
        match block_type {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                let has_later_tool_use = content[index + 1..]
                    .iter()
                    .any(|later| later.get("type").and_then(Value::as_str) == Some("tool_use"));
                let is_question = text.trim_end().ends_with('?') && !has_later_tool_use;
                events.push(TuiEvent::AssistantText {
                    text: Classified {
                        class: ContentClass::Visible,
                        value: text.to_string(),
                    },
                    is_question,
                    ts: ts.map(str::to_string),
                });
            }
            "tool_use" => {
                let tool = block.get("name").and_then(Value::as_str).unwrap_or("");
                let detail = block
                    .get("input")
                    .map(|input| serde_json::to_string(input).unwrap_or_default())
                    .unwrap_or_default();
                events.push(TuiEvent::ToolActivity {
                    tool: tool.to_string(),
                    detail: Classified {
                        class: ContentClass::Visible,
                        value: detail,
                    },
                    ts: ts.map(str::to_string),
                });
            }
            // `thinking` and anything else: never surfaced (see this
            // function's own doc comment).
            _ => {}
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------- slug_cwd

    #[test]
    fn slug_cwd_replaces_every_slash_and_dot_with_a_hyphen() {
        assert_eq!(
            slug_cwd(Path::new("/Users/nik/batman")),
            "-Users-nik-batman"
        );
        assert_eq!(
            slug_cwd(Path::new("/Users/nik/my.repo.git")),
            "-Users-nik-my-repo-git"
        );
    }

    #[test]
    fn slug_cwd_of_root_is_a_single_hyphen() {
        assert_eq!(slug_cwd(Path::new("/")), "-");
    }

    // --------------------------------------------------- permission_args

    fn vendor() -> ClaudeTuiVendor {
        ClaudeTuiVendor::new(PathBuf::from("/tmp"), Vec::new())
    }

    #[test]
    fn permission_args_max_is_dangerously_skip_permissions() {
        assert_eq!(
            vendor().permission_args(PermissionMode::Max),
            vec!["--dangerously-skip-permissions".to_string()]
        );
    }

    #[test]
    fn permission_args_readonly_is_permission_mode_plan() {
        assert_eq!(
            vendor().permission_args(PermissionMode::Readonly),
            vec!["--permission-mode".to_string(), "plan".to_string()]
        );
    }

    #[test]
    fn permission_args_default_is_empty() {
        assert!(vendor().permission_args(PermissionMode::Default).is_empty());
    }

    // ------------------------------------------------------------ argv

    fn cfg(bin: &str, mode: PermissionMode, model: Option<&str>) -> AdapterConfig {
        AdapterConfig {
            enabled: true,
            bin: bin.to_string(),
            mode: crate::config::crew::AdapterMode::Tui,
            permission_mode: mode,
            model: model.map(str::to_string),
            profile: "test".to_string(),
            session_dir: None,
            extra_args: Vec::new(),
        }
    }

    fn spec() -> StartSpec {
        StartSpec {
            run_id: crew_protocol::RunId::new(),
            task_id: crew_protocol::TaskId::new(),
            worker_id: crew_protocol::WorkerId::new(),
            prompt: "hello".to_string(),
            resume: None,
        }
    }

    #[test]
    fn launch_never_includes_the_headless_print_flag() {
        let launch = vendor().launch(&spec(), &cfg("claude", PermissionMode::Max, None));
        assert!(
            !launch.args.iter().any(|a| a == "-p" || a == "--print"),
            "a TUI launch must never pass -p/--print: {:?}",
            launch.args
        );
        assert_eq!(launch.program, PathBuf::from("claude"));
    }

    #[test]
    fn launch_appends_model_flag_when_configured() {
        let launch = vendor().launch(
            &spec(),
            &cfg("claude", PermissionMode::Default, Some("opus")),
        );
        assert_eq!(launch.args, vec!["--model".to_string(), "opus".to_string()]);
    }

    #[test]
    fn launch_appends_extra_args_after_permission_and_model_flags() {
        let mut config = cfg("claude", PermissionMode::Max, Some("opus"));
        config.extra_args = vec!["--verbose".to_string()];
        let launch = vendor().launch(&spec(), &config);
        assert_eq!(
            launch.args,
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--model".to_string(),
                "opus".to_string(),
                "--verbose".to_string(),
            ]
        );
    }

    #[test]
    fn resume_launch_passes_resume_and_the_session_id() {
        let session = VendorSessionRef("abc-123".to_string());
        let launch = vendor().resume_launch(
            &session,
            &spec(),
            &cfg("claude", PermissionMode::Default, None),
        );
        assert_eq!(
            launch.args,
            vec!["--resume".to_string(), "abc-123".to_string()]
        );
    }

    // --------------------------------------------------- transcript_root

    #[test]
    fn transcript_root_prefers_the_configured_session_dir() {
        let mut config = cfg("claude", PermissionMode::Default, None);
        config.session_dir = Some("/custom/sessions".to_string());
        let root = vendor().transcript_root(&spec(), &config);
        assert_eq!(root, PathBuf::from("/custom/sessions"));
    }

    #[test]
    fn transcript_root_defaults_to_home_claude_projects_slug() {
        // A nonexistent path: `fs::canonicalize` fails and falls back to
        // the path as given -- this test exercises exactly that
        // fallback (see `transcript_root_canonicalizes_a_symlinked_cwd_first`
        // for the real-directory, real-symlink case).
        let v = ClaudeTuiVendor::new(PathBuf::from("/Users/nik/batman"), Vec::new());
        let root = v.transcript_root(&spec(), &cfg("claude", PermissionMode::Default, None));
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        assert_eq!(
            root,
            PathBuf::from(home)
                .join(".claude")
                .join("projects")
                .join("-Users-nik-batman")
        );
    }

    /// The real CLI slugs the *canonicalized* cwd, not the literal path
    /// string it was launched with: a real recorded capture showed `/tmp`
    /// (a symlink to `/private/tmp` on macOS) producing the slug
    /// `-private-tmp-...`, never `-tmp-...`. Proven here with a real
    /// temp directory and a real symlink to it, not just against `/tmp`
    /// itself (which would only prove the case macOS happens to set up
    /// system-wide).
    #[test]
    fn transcript_root_canonicalizes_a_symlinked_cwd_first() {
        let dir = tempfile::Builder::new()
            .prefix("bat-tui-claude-real-")
            .tempdir_in("/tmp")
            .expect("create a real temp dir");
        let real_path = dir
            .path()
            .canonicalize()
            .expect("canonicalize the real dir itself");

        let link_path =
            std::env::temp_dir().join(format!("bat-tui-claude-symlink-{}", uuid::Uuid::now_v7()));
        std::os::unix::fs::symlink(&real_path, &link_path).expect("create symlink");

        let v = ClaudeTuiVendor::new(link_path.clone(), Vec::new());
        let root = v.transcript_root(&spec(), &cfg("claude", PermissionMode::Default, None));

        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        assert_eq!(
            root,
            PathBuf::from(home)
                .join(".claude")
                .join("projects")
                .join(slug_cwd(&real_path)),
            "must slug the symlink's real target, not the symlink path itself"
        );

        let _ = std::fs::remove_file(&link_path);
    }

    // ------------------------------------------------------ compose/interrupt

    #[test]
    fn compose_input_appends_a_bare_carriage_return() {
        assert_eq!(vendor().compose_input("hi"), b"hi\r".to_vec());
    }

    #[test]
    fn interrupt_sequence_is_a_bare_escape_byte() {
        assert_eq!(vendor().interrupt_sequence(), vec![0x1b]);
    }

    // ------------------------------------------------------- version_gate

    #[test]
    fn version_gate_accepts_a_version_inside_the_tested_range() {
        assert_eq!(
            vendor().version_gate("1.5.2 (Claude Code)"),
            VersionVerdict::Compatible
        );
    }

    #[test]
    fn version_gate_rejects_an_unparseable_string() {
        assert!(matches!(
            vendor().version_gate("not-a-version"),
            VersionVerdict::Incompatible { .. }
        ));
    }

    #[test]
    fn version_gate_rejects_a_version_outside_the_tested_range() {
        assert!(matches!(
            vendor().version_gate("0.0.1"),
            VersionVerdict::Incompatible { .. }
        ));
    }

    // ------------------------------------------------- session id default

    #[test]
    fn session_id_from_transcript_path_uses_the_file_stem() {
        let path = Path::new("/home/x/.claude/projects/-x/1e2d-session.jsonl");
        assert_eq!(
            vendor().session_id_from_transcript_path(path),
            Some("1e2d-session".to_string())
        );
    }

    // ------------------------------------------------ transcript format

    fn line(value: serde_json::Value) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn user_entry_extracts_only_the_session_id() {
        let raw = line(serde_json::json!({
            "type": "user",
            "sessionId": "sess-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "message": {"role": "user", "content": "hi"},
        }));
        let (events, cursor) = ClaudeTranscriptFormat.parse(&raw, &Cursor::start());
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            TuiEvent::SessionMeta { vendor_session_id } if vendor_session_id == "sess-1"
        ));
        assert_eq!(cursor.offset, raw.len() as u64);
    }

    #[test]
    fn assistant_text_ending_in_question_mark_with_no_later_tool_use_is_a_question() {
        let raw = line(serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "timestamp": "t",
            "message": {"content": [{"type": "text", "text": "Should I proceed?"}]},
        }));
        let (events, _) = ClaudeTranscriptFormat.parse(&raw, &Cursor::start());
        let question = events
            .iter()
            .find_map(|e| match e {
                TuiEvent::AssistantText {
                    text, is_question, ..
                } => Some((text.value.clone(), *is_question)),
                _ => None,
            })
            .expect("expected an AssistantText event");
        assert_eq!(question, ("Should I proceed?".to_string(), true));
    }

    #[test]
    fn assistant_text_ending_in_question_mark_followed_by_tool_use_is_not_a_question() {
        let raw = line(serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "timestamp": "t",
            "message": {"content": [
                {"type": "text", "text": "Let me check the config file?"},
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "config.toml"}},
            ]},
        }));
        let (events, _) = ClaudeTranscriptFormat.parse(&raw, &Cursor::start());
        let is_question = events.iter().find_map(|e| match e {
            TuiEvent::AssistantText { is_question, .. } => Some(*is_question),
            _ => None,
        });
        assert_eq!(is_question, Some(false));
    }

    #[test]
    fn assistant_tool_use_maps_to_tool_activity_with_json_input_as_detail() {
        let raw = line(serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "timestamp": "t",
            "message": {"content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "echo crew-fixture"}},
            ]},
        }));
        let (events, _) = ClaudeTranscriptFormat.parse(&raw, &Cursor::start());
        let tool = events.iter().find_map(|e| match e {
            TuiEvent::ToolActivity { tool, detail, .. } => {
                Some((tool.clone(), detail.value.clone()))
            }
            _ => None,
        });
        assert_eq!(
            tool,
            Some((
                "Bash".to_string(),
                serde_json::json!({"command": "echo crew-fixture"}).to_string()
            ))
        );
    }

    #[test]
    fn assistant_thinking_block_never_produces_an_event() {
        let raw = line(serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "timestamp": "t",
            "message": {"content": [
                {"type": "thinking", "thinking": "secret reasoning", "signature": "sig"},
            ]},
        }));
        let (events, _) = ClaudeTranscriptFormat.parse(&raw, &Cursor::start());
        // Only the SessionMeta extracted from the entry's own `sessionId`
        // -- never anything derived from the thinking block's content.
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], TuiEvent::SessionMeta { .. }));
    }

    #[test]
    fn summary_entry_produces_no_event_when_it_carries_no_session_id() {
        let raw = line(serde_json::json!({
            "type": "summary",
            "summary": "condensed prior turns",
        }));
        let (events, _) = ClaudeTranscriptFormat.parse(&raw, &Cursor::start());
        assert!(events.is_empty());
    }

    #[test]
    fn an_unrecognized_type_degrades_to_raw() {
        let raw = line(serde_json::json!({"type": "future_entry_type"}));
        let (events, _) = ClaudeTranscriptFormat.parse(&raw, &Cursor::start());
        assert!(matches!(
            &events[0],
            TuiEvent::Raw { entry_type } if entry_type == "future_entry_type"
        ));
    }

    #[test]
    fn cursor_records_the_entry_uuid_as_last_entry_id() {
        let raw = line(serde_json::json!({
            "type": "user",
            "uuid": "line-uuid-1",
            "sessionId": "sess-1",
        }));
        let (_, cursor) = ClaudeTranscriptFormat.parse(&raw, &Cursor::start());
        assert_eq!(cursor.last_entry_id, Some("line-uuid-1".to_string()));
    }

    #[test]
    fn parsing_is_idempotent_at_an_arbitrary_mid_line_byte_split() {
        let full = line(serde_json::json!({
            "type": "assistant",
            "sessionId": "sess-1",
            "timestamp": "t",
            "message": {"content": [{"type": "text", "text": "hello there"}]},
        }));
        // Split mid-line: the first half alone parses to nothing (no
        // complete line yet) and consumes zero bytes.
        let split = full.len() / 2;
        let (first_events, first_cursor) =
            ClaudeTranscriptFormat.parse(&full[..split], &Cursor::start());
        assert!(first_events.is_empty());
        assert_eq!(first_cursor.offset, 0);

        // Re-parsing from the *start* against the full buffer produces
        // exactly the same result a single full parse would -- proving
        // a crash-restart that re-tails from byte 0 after only ever
        // observing a partial line is safe.
        let (whole_events, whole_cursor) = ClaudeTranscriptFormat.parse(&full, &Cursor::start());
        assert_eq!(whole_cursor.offset, full.len() as u64);
        assert_eq!(whole_events.len(), 2); // SessionMeta + AssistantText
    }
}
