//! The OMP CLI's [`TuiVendor`] implementation: interactive `omp` (never
//! `-p`/`--mode rpc`), OMP session JSONL tailing under
//! `~/.omp/agent/sessions/<cwd slug>/`, and this vendor's own
//! argv/compose-input conventions.
//!
//! Every flag here was validated against an installed `omp` CLI's own
//! `--help` output (`omp/18.0.5`: `--model=<value>`, `-r/--resume=<value>`,
//! `--allow-home`, `--session-dir=<value>`), and the transcript shapes
//! against real recorded sessions on this machine
//! (`~/.omp/agent/sessions/-Users-...-<repo>/<timestamp>_<uuid>.jsonl`)
//! plus the committed *synthetic* fixture
//! (`fixtures/adapters/omp-tui/session.jsonl`). Decisions the fixture
//! cannot itself prove are called out in this module's doc comments
//! rather than left implicit.
//!
//! Permission modes deliberately map to NO argv at all: unlike the other
//! vendors, `omp --help` exposes no approval-posture flags -- its
//! interactive REPL manages approvals in-session, and this adapter never
//! invents tool compatibility the CLI does not offer (the same rule the
//! headless adapter's probe enforces for unlisted models).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use crew_protocol::{Classified, ContentClass};

use crate::adapter::r#trait::{StartSpec, VendorSessionRef};
use crate::config::crew::{AdapterConfig, PermissionMode};
use crate::supervisor::EnvironmentPolicy;

use super::adapter::{LaunchSpec, TuiVendor, VersionVerdict};
use super::claude::slug_cwd;
use super::{Cursor, TranscriptFormat, TuiEvent, parse_jsonl_chunk};

/// The `omp --version` range this adapter's fixed argv and transcript-
/// format assumptions were built and tested against -- same policy as
/// Codex's gate ([`super::codex`]): one validated point (`18.0.5`, an
/// installed CLI probed for this module), everything else in the range
/// an untested extrapolation until WP29's live smoke widens it.
const MIN_TESTED_VERSION: (u32, u32, u32) = (18, 0, 0);
const MAX_TESTED_VERSION: (u32, u32, u32) = (18, 99, 99);

/// Parses the leading `MAJOR.MINOR.PATCH` out of an `omp --version`
/// string (`omp/18.0.5`). Returns `None` for anything that does not
/// carry three dot-separated integers --
/// [`OmpTuiVendor::version_gate`] treats an unparseable string as
/// incompatible, never as a silent pass.
fn parse_leading_version(probed: &str) -> Option<(u32, u32, u32)> {
    // The real CLI's own output is `omp/18.0.5`: unlike codex's
    // space-separated form, the version rides INSIDE a token whose head
    // is a non-digit name prefix -- so the search is for the first
    // dot-separated three-part run STARTING AT A DIGIT anywhere in each
    // whitespace token, never for a digit-led token.
    let version_token = probed.split_whitespace().find_map(|token| {
        let digits_at = token.chars().position(|c| c.is_ascii_digit())?;
        let candidate = &token[digits_at..];
        (candidate.split('.').count() == 3).then_some(candidate)
    })?;
    let mut parts = version_token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// The OMP CLI, driven interactively over a real PTY.
pub struct OmpTuiVendor {
    /// The working directory the vendor process is launched in, and the
    /// input [`slug_cwd`] slugs to find this session's transcript
    /// directory under `~/.omp/agent/sessions/`.
    cwd: PathBuf,
    /// `WorkerProfile::environmentAllowlist` -- variable *names* only,
    /// exactly like [`super::claude::ClaudeTuiVendor`]'s field.
    environment_allowlist: Vec<String>,
}

impl OmpTuiVendor {
    #[must_use]
    pub fn new(cwd: PathBuf, environment_allowlist: Vec<String>) -> Self {
        Self {
            cwd,
            environment_allowlist,
        }
    }

    /// Same policy as every other vendor:
    /// [`EnvironmentPolicy::baseline()`] plus this profile's allowlisted
    /// names. `HOME` is load-bearing here too: the real CLI resolves its
    /// own `~/.omp/agent/sessions/` root the same way
    /// [`OmpTuiVendor::transcript_root`] does.
    fn env(&self) -> HashMap<String, String> {
        let current: HashMap<String, String> = std::env::vars().collect();
        EnvironmentPolicy::baseline().build(&current, &self.environment_allowlist)
    }

    /// The base argv every launch (fresh or resumed) shares: `--model`
    /// when configured (the config default carries `"qwen"`; the selector
    /// itself is validated against `omp models --json` by the headless
    /// adapter's probe -- this vendor never invents one), `--allow-home`
    /// so a repo-rooted cwd never bounces to a temp dir before the PTY
    /// is even attached, and `cfg.extra_args` verbatim last.
    fn base_args(&self, cfg: &AdapterConfig) -> Vec<String> {
        let mut args = self.permission_args(cfg.permission_mode);
        if let Some(model) = &cfg.model {
            args.push("--model".to_string());
            args.push(model.clone());
        }
        args.push("--allow-home".to_string());
        args.extend(cfg.extra_args.iter().cloned());
        args
    }
}

impl TuiVendor for OmpTuiVendor {
    fn kind(&self) -> &'static str {
        "omp-rpc"
    }

    /// Interactive bare `omp`, deliberately never `-p/--print` (the
    /// headless one-shot mode) nor `--mode rpc` (the headless RPC server
    /// `OmpRpcAdapter` drives): a TUI session must launch the real
    /// interactive REPL so a human attached to its pane sees (and can
    /// type into) the exact same session this adapter tails.
    fn launch(&self, _spec: &StartSpec, cfg: &AdapterConfig) -> LaunchSpec {
        LaunchSpec {
            program: PathBuf::from(&cfg.bin),
            args: self.base_args(cfg),
            cwd: self.cwd.clone(),
            env: self.env(),
        }
    }

    /// `omp --resume <session-id>` (validated against the installed
    /// CLI's `--help`: `-r, --resume=<value>` takes a required value, so
    /// both token orders bind; the two-token form matches how every
    /// other flag here travels), plus the same model/extra-args base
    /// every fresh launch gets.
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

    /// `cfg.session_dir` overrides everything; otherwise
    /// `~/.omp/agent/sessions/<slug_cwd(canonicalized self.cwd)>` --
    /// observed on a real install, where each project directory nests
    /// its sessions (`-Personal-Repos-batman/<timestamp>_<uuid>.jsonl`)
    /// using the same `/`-and-`.`-to-`-` slug as Claude's projects root
    /// (confirmed against live dirs, including a worktree path whose
    /// leading `.claude` segment slugged to `-claude`). `$HOME`
    /// unresolved falls back to `/root`; canonicalization failure falls
    /// back to the raw cwd, exactly like Claude's vendor.
    fn transcript_root(&self, _spec: &StartSpec, cfg: &AdapterConfig) -> PathBuf {
        if let Some(dir) = &cfg.session_dir {
            return PathBuf::from(dir);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let canonical_cwd = std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone());
        PathBuf::from(home)
            .join(".omp")
            .join("agent")
            .join("sessions")
            .join(slug_cwd(&canonical_cwd))
    }

    /// Unlike Claude, an OMP session's *filename* is not its session id:
    /// it is `<timestamp>_<uuid>.jsonl` inside the cwd-slug directory,
    /// which a resumed session cannot re-derive from the id alone. Like
    /// Codex, a bounded walk of the transcript root finds the (unique)
    /// file whose name ends in the session id. This touches only
    /// directory metadata -- no process spawn, no content scanning.
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
                    // Bound the walk: a sessions root nests exactly one
                    // cwd-slug level below the root.
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
            // No matching session on disk (yet): fall back to a stable
            // shape so callers get a path to wait on rather than a
            // confusing empty option.
            root.join(format!("<timestamp>_{}.jsonl", session.0))
        })
    }

    fn format(&self) -> Arc<dyn TranscriptFormat> {
        Arc::new(OmpSessionFormat)
    }

    /// Text plus a bare carriage return -- the interactive REPL's Enter
    /// submit, same convention as every other vendored TUI here.
    fn compose_input(&self, message: &str) -> Vec<u8> {
        let mut bytes = message.as_bytes().to_vec();
        bytes.push(b'\r');
        bytes
    }

    /// A bare Escape byte: the interactive REPL's turn-interrupt key,
    /// same convention as the other vendored REPLs. [INFERENCE] not
    /// separately confirmed against a live 18.0.5 session -- WP29's
    /// live smoke owns that confirmation.
    fn interrupt_sequence(&self) -> Vec<u8> {
        vec![0x1b]
    }

    /// Deliberately empty for every posture: `omp --help` exposes no
    /// approval-posture flags at all (see this module's header doc).
    fn permission_args(&self, _mode: PermissionMode) -> Vec<String> {
        Vec::new()
    }

    fn version_gate(&self, probed: &str) -> VersionVerdict {
        match parse_leading_version(probed) {
            Some(version) if version >= MIN_TESTED_VERSION && version <= MAX_TESTED_VERSION => {
                VersionVerdict::Compatible
            }
            Some(version) => VersionVerdict::Incompatible {
                detail: format!(
                    "omp {version:?} is outside the tested range {MIN_TESTED_VERSION:?}..={MAX_TESTED_VERSION:?}"
                ),
            },
            None => VersionVerdict::Incompatible {
                detail: format!("could not parse a MAJOR.MINOR.PATCH version from {probed:?}"),
            },
        }
    }

    // `session_id_from_transcript_path` is not overridden, mirroring
    // codex: the filename stem (`<timestamp>_<uuid>`) alone is not the
    // id -- the initial value corrects the moment the tailed `session`
    // line (authoritative) arrives, exactly as codex's rollout meta does.
}

/// The real OMP CLI's session JSONL transcript format: one entry per
/// line, each `{"type": ..., "id", "parentId", "timestamp" ...}`.
/// Observed types: `session` (version 3, authoritative identity),
/// `message` (roles `user`/`assistant`/`toolResult`/`developer`/
/// `fileMention`; assistant `content` blocks of types `text`,
/// `thinking`, `toolCall`), plus telemetry housekeeping (`title`,
/// `model_change`, `thinking_level_change`, `service_tier_change`,
/// `compaction`, `custom`, `custom_message`, `credential_pin`).
struct OmpSessionFormat;

impl TranscriptFormat for OmpSessionFormat {
    fn parse(&self, raw: &[u8], cursor: &Cursor) -> (Vec<TuiEvent>, Cursor) {
        parse_jsonl_chunk(raw, cursor, map_entry)
    }
}

/// Entry types observed on real installs that are pure housekeeping --
/// skipped silently rather than degraded to `Raw`, mirroring codex's
/// treatment of its own `token_count`/`exec_*` event stream.
const TELEMETRY_ENTRY_TYPES: &[&str] = &[
    "title",
    "model_change",
    "thinking_level_change",
    "service_tier_change",
    "compaction",
    "custom",
    "custom_message",
    "credential_pin",
];

/// Maps one parsed transcript entry to its events plus its own `id`
/// (this format's `last_entry_id`).
fn map_entry(value: &Value) -> (Vec<TuiEvent>, Option<String>) {
    let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let ts = value
        .get("timestamp")
        .and_then(Value::as_str)
        .map(str::to_string);
    let entry_id = value.get("id").and_then(Value::as_str).map(str::to_string);

    match entry_type {
        // Authoritative session identity: the nonce-derived initial value
        // corrects to this the moment the meta line is tailed.
        "session" => (
            vec![TuiEvent::SessionMeta {
                vendor_session_id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }],
            entry_id,
        ),
        "message" => (map_message(value, ts.as_deref()), entry_id),
        t if TELEMETRY_ENTRY_TYPES.contains(&t) => (Vec::new(), None),
        // Unknown entries degrade to Raw rather than erroring.
        _ => (
            vec![TuiEvent::Raw {
                entry_type: entry_type.to_string(),
            }],
            entry_id,
        ),
    }
}

/// Maps one `message` entry to its events: only the assistant's own
/// output surfaces. User/toolResult/fileMention/developer roles are the
/// harness's or tools' own words, never the worker's -- surfacing them
/// would double-journal every injected prompt. Assistant content blocks
/// map like Claude's: `text` -> [`TuiEvent::AssistantText`] (question-
/// detected per the exact heuristic Claude uses: trimmed text ends in
/// `?` **and** no `toolCall` block follows later in this same array),
/// `toolCall` -> [`TuiEvent::ToolActivity`] (`detail` is the call's
/// `arguments` compactly re-serialized), `thinking` silently skipped --
/// the model's hidden reasoning must never surface, mirroring the
/// headless adapters' own thinking-block redaction exactly.
fn map_message(value: &Value, ts: Option<&str>) -> Vec<TuiEvent> {
    let message = value.get("message").cloned().unwrap_or(Value::Null);
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return Vec::new();
    }
    let Some(blocks) = message.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                let followed_by_tool_call = blocks[index + 1..]
                    .iter()
                    .any(|later| later.get("type").and_then(Value::as_str) == Some("toolCall"));
                let trimmed = text.trim_end();
                events.push(TuiEvent::AssistantText {
                    text: Classified {
                        class: ContentClass::Visible,
                        value: text.to_string(),
                    },
                    is_question: trimmed.ends_with('?') && !followed_by_tool_call,
                    ts: ts.map(str::to_string),
                });
            }
            Some("toolCall") => {
                let tool = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let detail = block.get("arguments").cloned().unwrap_or_else(|| json!({}));
                events.push(TuiEvent::ToolActivity {
                    tool: tool.to_string(),
                    detail: Classified {
                        class: ContentClass::Visible,
                        value: serde_json::to_string(&detail).unwrap_or_else(|_| "{}".to_string()),
                    },
                    ts: ts.map(str::to_string),
                });
            }
            // thinking and anything else: never surfaced.
            _ => {}
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn default_cfg() -> AdapterConfig {
        AdapterConfig {
            enabled: true,
            bin: "omp".to_string(),
            mode: crate::config::crew::AdapterMode::Tui,
            permission_mode: PermissionMode::Max,
            model: Some("qwen".to_string()),
            profile: "test".to_string(),
            session_dir: None,
            extra_args: vec!["--no-prewalk".to_string()],
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
    fn kind_is_the_omprpc_wire_name() {
        assert_eq!(
            OmpTuiVendor::new(PathBuf::from("/w"), vec![]).kind(),
            "omp-rpc"
        );
    }

    #[test]
    fn permission_modes_map_to_no_argv_at_all() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        for mode in [
            PermissionMode::Max,
            PermissionMode::Readonly,
            PermissionMode::Default,
        ] {
            assert_eq!(
                vendor.permission_args(mode),
                Vec::<String>::new(),
                "omp exposes no approval-posture flags; none may be invented for {mode:?}"
            );
        }
    }

    #[test]
    fn launch_is_interactive_bare_omp_with_model_and_allow_home() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new());
        let launch = vendor.launch(&spec(), &default_cfg());
        assert_eq!(launch.program, PathBuf::from("omp"));
        assert_eq!(launch.cwd, PathBuf::from("/workspace/crew"));
        for forbidden in ["-p", "--print"] {
            assert!(
                !launch.args.iter().any(|a| a == forbidden),
                "argv must never contain {forbidden}: {:?}",
                launch.args
            );
        }
        // The headless RPC output mode this adapter must never launch
        // interactively -- exact match and value form, never a prefix
        // test (`--model` legitimately shares a prefix with `--mode`).
        assert!(
            !launch
                .args
                .iter()
                .any(|a| a == "--mode" || a.starts_with("--mode=")),
            "argv must never select a headless output mode: {:?}",
            launch.args
        );
        for want in ["--model", "qwen", "--allow-home", "--no-prewalk"] {
            assert!(
                launch.args.iter().any(|a| a == want),
                "argv missing {want}: {:?}",
                launch.args
            );
        }
    }

    #[test]
    fn resume_uses_the_resume_flag_before_the_base_args() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let launch = vendor.resume_launch(
            &VendorSessionRef("01a0343b-70ab".to_string()),
            &spec(),
            &default_cfg(),
        );
        assert_eq!(launch.args[0], "--resume");
        assert_eq!(launch.args[1], "01a0343b-70ab");
        assert!(launch.args.contains(&"--allow-home".to_string()));
    }

    #[test]
    fn transcript_root_honors_session_dir_override_and_home_default() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let mut cfg = default_cfg();
        cfg.session_dir = Some("/tmp/sessions".to_string());
        assert_eq!(
            vendor.transcript_root(&spec(), &cfg),
            PathBuf::from("/tmp/sessions")
        );
        cfg.session_dir = None;
        cfg.extra_args.clear();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let expected_slug = slug_cwd(Path::new("/w"));
        assert_eq!(
            vendor.transcript_root(&spec(), &cfg),
            PathBuf::from(home)
                .join(".omp")
                .join("agent")
                .join("sessions")
                .join(expected_slug)
        );
    }

    #[test]
    fn transcript_path_for_session_finds_the_timestamp_partitioned_file() {
        let dir = tempfile::Builder::new()
            .prefix("bat-omp-tui-vendor-")
            .tempdir_in("/tmp")
            .expect("temp dir");
        let root = dir.path().join("-workspace-crew");
        std::fs::create_dir_all(&root).expect("mkdir");
        let older =
            root.join("2026-01-01T00-00-00-000Z_66666666-6666-4666-8666-000000000001.jsonl");
        std::fs::write(&older, "{}\n").expect("seed older");
        let newer =
            root.join("2026-06-01T00-00-00-000Z_66666666-6666-4666-8666-000000000001.jsonl");
        std::fs::write(&newer, "{}\n").expect("seed newer");

        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let mut cfg = default_cfg();
        cfg.session_dir = Some(root.to_string_lossy().into_owned());
        let found = vendor.transcript_path_for_session(
            &VendorSessionRef("66666666-6666-4666-8666-000000000001".to_string()),
            &spec(),
            &cfg,
        );
        assert_eq!(found, newer, "the newest matching partition wins");
    }

    #[test]
    fn version_gate_accepts_the_installed_version_shape_and_rejects_outside() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        assert_eq!(
            vendor.version_gate("omp/18.0.5"),
            VersionVerdict::Compatible
        );
        assert_eq!(
            vendor.version_gate("omp 18.1.2"),
            VersionVerdict::Compatible
        );
        assert!(matches!(
            vendor.version_gate("omp/17.9.9"),
            VersionVerdict::Incompatible { .. }
        ));
        assert!(matches!(
            vendor.version_gate("omp/19.0.0"),
            VersionVerdict::Incompatible { .. }
        ));
        assert!(matches!(
            vendor.version_gate("no version here"),
            VersionVerdict::Incompatible { .. }
        ));
    }

    #[test]
    fn compose_input_appends_a_carriage_return_and_interrupt_is_escape() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        assert_eq!(vendor.compose_input("hi"), b"hi\r".to_vec());
        assert_eq!(vendor.interrupt_sequence(), vec![0x1b]);
    }

    fn fixture_bytes() -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/adapters/omp-tui/session.jsonl");
        std::fs::read(&path).unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
    }

    #[test]
    fn the_full_fixture_parses_and_consumes_every_byte() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let (events, cursor) = vendor.format().parse(&fixture_bytes(), &Cursor::start());
        assert!(!events.is_empty());
        assert_eq!(cursor.offset as usize, fixture_bytes().len());
    }

    #[test]
    fn fixture_yields_session_meta_assistant_text_tool_activity() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let (events, _) = vendor.format().parse(&fixture_bytes(), &Cursor::start());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TuiEvent::SessionMeta { vendor_session_id }
                if vendor_session_id == "66666666-6666-4666-8666-000000000001")),
            "the session line must yield the authoritative SessionMeta: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TuiEvent::ToolActivity { tool, .. } if tool == "bash"))
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                TuiEvent::AssistantText {
                    is_question: true,
                    ..
                }
            )),
            "the fixture's closing standalone question must be question-detected"
        );
    }

    #[test]
    fn user_messages_tool_results_thinking_and_telemetry_never_surface() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let raw = concat!(
            "{\"type\":\"title\",\"v\":1,\"title\":\"t\"}\n",
            "{\"type\":\"model_change\",\"id\":\"mc1\"}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"echoed user text\"}]}}\n",
            "{\"type\":\"message\",\"id\":\"m2\",\"message\":{\"role\":\"toolResult\",\"content\":[{\"type\":\"text\",\"text\":\"tool output\"}]}}\n",
            "{\"type\":\"message\",\"id\":\"m3\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"hidden reasoning\"},{\"type\":\"text\",\"text\":\"visible reply?\"}]}}\n",
        );
        let (events, _) = vendor.format().parse(raw.as_bytes(), &Cursor::start());
        assert_eq!(
            events.len(),
            1,
            "only the visible reply surfaces: {events:?}"
        );
        assert!(
            matches!(&events[0], TuiEvent::AssistantText { text, is_question: true, .. }
                if text.value == "visible reply?")
        );
    }

    #[test]
    fn narration_followed_by_a_tool_call_is_not_a_question_even_when_it_ends_in_one() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let raw = concat!(
            "{\"type\":\"message\",\"id\":\"m1\",\"message\":{\"role\":\"assistant\",\"content\":[",
            "{\"type\":\"text\",\"text\":\"Shall I read the file next?\"},",
            "{\"type\":\"toolCall\",\"id\":\"c1\",\"name\":\"read\",\"partialArgs\":false,\"arguments\":{\"path\":\"a.rs\"}}]}}\n",
        );
        let (events, _) = vendor.format().parse(raw.as_bytes(), &Cursor::start());
        assert_eq!(events.len(), 2);
        assert!(
            matches!(
                &events[0],
                TuiEvent::AssistantText {
                    is_question: false,
                    ..
                }
            ),
            "text followed by a tool call in the same array is narration, never a question"
        );
        assert!(matches!(&events[1], TuiEvent::ToolActivity { tool, .. } if tool == "read"));
    }

    #[test]
    fn malformed_lines_degrade_to_raw_not_errors() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let raw = b"{not json at all\n{\"type\":\"brand_new_entry_type\"}\n";
        let (events, cursor) = vendor.format().parse(raw, &Cursor::start());
        assert!(matches!(&events[0], TuiEvent::Raw { .. }));
        assert!(
            matches!(&events[1], TuiEvent::Raw { entry_type } if entry_type == "brand_new_entry_type")
        );
        assert_eq!(cursor.offset as usize, raw.len());
    }

    #[test]
    fn partial_trailing_line_is_left_unconsumed() {
        let vendor = OmpTuiVendor::new(PathBuf::from("/w"), vec![]);
        let raw = b"{\"type\":\"session\",\"id\":\"x\"}\n{\"type\":\"mess";
        let (events, cursor) = vendor.format().parse(raw, &Cursor::start());
        assert!(matches!(&events[0], TuiEvent::SessionMeta { .. }));
        assert_eq!(
            cursor.offset as usize,
            raw.len() - b"{\"type\":\"mess".len()
        );
    }
}
