//! Builds the `claude` CLI argv (and stdin `stream-json` frames) from a
//! [`ClaudeStartupOptions`]/[`StartSpec`].
//!
//! Every flag here is grounded against the installed `claude` 2.1.219
//! CLI's own `--help` output and
//! <https://code.claude.com/docs/en/headless>, not invented. Deviation
//! worth flagging: `claude --help` has **no `--max-turns` flag** on this
//! CLI at all (it exists only as a programmatic `Options.maxTurns` field
//! in the TS/Python Agent SDK) -- `ClaudeStartupOptions::max_turns`
//! (already defined upstream, Task 1/2) is accepted but deliberately not
//! passed as a CLI flag, since there is no flag to pass it as.

use uuid::Uuid;

use crew_runtime::adapter::{ClaudeStartupOptions, StartSpec, VendorSessionRef};

/// Builds the full argv (excluding the `claude` program name itself) for
/// one `start`/`resume` invocation.
///
/// Always includes `-p`, `--input-format stream-json`, `--output-format
/// stream-json`, `--verbose`, `--include-partial-messages`,
/// `--include-hook-events`, and `--forward-subagent-text` -- and never
/// `--bare`, `--disable-slash-commands`, `--safe-mode`, or any other
/// ignore-user-config flag, so native skill/agent/plugin/hook/MCP
/// discovery stays on exactly as it would for an interactive session
/// (Task 3's "Interfaces" section).
#[must_use]
pub fn build_args(
    options: &ClaudeStartupOptions,
    spec: &StartSpec,
    session_id: &Uuid,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--include-hook-events",
        "--forward-subagent-text",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    match &spec.resume {
        Some(VendorSessionRef(session)) => {
            args.push("--resume".to_string());
            args.push(session.clone());
        }
        None => {
            args.push("--session-id".to_string());
            args.push(session_id.to_string());
        }
    }

    if let Some(allowed) = &options.allowed_tools
        && !allowed.is_empty()
    {
        args.push("--allowedTools".to_string());
        args.extend(allowed.iter().cloned());
    }
    if let Some(mode) = &options.permission_mode {
        args.push("--permission-mode".to_string());
        args.push(mode.clone());
    }
    // `options.max_turns` is intentionally never turned into a flag --
    // see the module doc.

    args
}

/// Builds one newline-delimited `stream-json` `user` message frame for
/// the given text, suitable for `ManagedProcess::write_stdin` under
/// `--input-format stream-json`. Used for both the initial prompt
/// (`start`) and every later `send` (steer/follow-up/answer/peer
/// message all become another queued user turn on the same stdin
/// stream -- see <https://code.claude.com/docs/en/agent-sdk/typescript>'s
/// `SDKUserMessage`/streaming-input documentation).
#[must_use]
pub fn build_stdin_user_message(text: &str) -> Vec<u8> {
    let frame = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": text }],
        },
    });
    let mut bytes = serde_json::to_vec(&frame).expect("a json! object always serializes");
    bytes.push(b'\n');
    bytes
}
