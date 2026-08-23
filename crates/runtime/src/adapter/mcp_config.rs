//! Per-adapter coordination MCP launch helpers: the argv/env/config each
//! adapter's command builder injects to give its supervised vendor
//! process access to the worker coordination tools (`crew_task`,
//! `crew_send`, etc.) via a `crewd coordination-mcp` subprocess the
//! vendor CLI itself spawns as its own MCP server -- see
//! `crate::coordination::mcp` for that subprocess, and
//! `crate::coordination::mcp_protocol` for the tool schemas it serves.
//!
//! Every adapter's own native MCP/plugin/skill/hook discovery stays on:
//! nothing here ever adds a flag that suppresses or replaces it, only
//! one additional named server (`"crew"`) alongside whatever the
//! vendor CLI already loads from the user/project's own config.
//!
//! OMP-RPC has no separate MCP subprocess of its own to inject this
//! into at all: `omp --mode rpc`'s "host tools" are invoked over the
//! *same* RPC channel the adapter already owns (a `host_tool_call`
//! frame on its stdout, answered with a `host_tool_result` on its
//! stdin -- see `crate::adapter::omp_rpc`'s own host-tool bridge), so
//! it never goes through this module or the scope-token-authenticated
//! socket at all: the runtime process making that in-process call is
//! the vendor's own parent, never a descendant of it, so it could not
//! authenticate over that socket even if it tried (ancestry is checked
//! in the wrong direction). `CoordinationBroker::execute_tool_call`
//! (`crate::coordination::broker`) is the shared, in-process
//! counterpart both paths ultimately resolve to.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use batman_protocol::{ProjectId, RunId, TaskId, Timestamp, WorkerId};
use serde_json::{Value, json};

use crate::coordination::{BindError, ScopeBinding, ScopeTokenStore, VendorProcessIdentity};

/// Everything a supervised vendor process's command builder needs to
/// wire up the coordination MCP server: where the verified `crewd`
/// binary lives, this run's state/repository paths, and the run it's
/// scoped to.
#[derive(Debug, Clone)]
pub struct McpLaunchContext {
    pub crewd_path: PathBuf,
    pub state_dir: PathBuf,
    pub repository: PathBuf,
    pub run_id: RunId,
}

/// Everything an adapter needs to mint a worker-MCP scope token and
/// inject the coordination MCP server into its supervised vendor
/// process's launch, bundled once per adapter instance rather than
/// threaded through every method individually. An adapter holds this
/// as `Option<AdapterMcpConfig>` -- `None` for a caller (chiefly
/// existing tests) that never asked for worker MCP tools at all; every
/// existing constructor keeps working unchanged.
#[derive(Clone)]
pub struct AdapterMcpConfig {
    pub scope_tokens: Arc<ScopeTokenStore>,
    pub project_id: ProjectId,
    pub crewd_path: PathBuf,
    pub state_dir: PathBuf,
    pub repository: PathBuf,
}

impl AdapterMcpConfig {
    /// The launch context for one run, for
    /// [`coordination_mcp_argv`]/[`coordination_mcp_config_document`]/
    /// [`codex_mcp_overrides`].
    #[must_use]
    pub fn launch_context(&self, run_id: RunId) -> McpLaunchContext {
        McpLaunchContext {
            crewd_path: self.crewd_path.clone(),
            state_dir: self.state_dir.clone(),
            repository: self.repository.clone(),
            run_id,
        }
    }

    /// Step 1 of 2: reserves a bearer token, safe to put in the vendor
    /// process's own environment (via [`coordination_mcp_env`]) *before*
    /// spawning it -- [`Self::activate`] is what actually makes it live.
    #[must_use]
    pub fn reserve(&self) -> String {
        self.scope_tokens.reserve_token()
    }

    /// Step 2 of 2: activates a token [`Self::reserve`] returned, once
    /// (and only once) the vendor process has actually spawned and its
    /// real pid is known. Call this immediately after spawn returns,
    /// before any interaction with the process.
    ///
    /// # Errors
    /// Returns [`BindError`] if `token` is already bound (should never
    /// happen for a freshly reserved token; treat any `Err` here as
    /// fatal). **The caller must terminate the just-spawned vendor
    /// process and report an error rather than proceed** -- a vendor
    /// process running with a token that never activated has no way to
    /// authenticate its own coordination MCP subprocess, and letting it
    /// keep running would leave that failure silent until the vendor
    /// itself tries to use a worker tool.
    pub fn activate(
        &self,
        token: String,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        vendor_pid: i32,
        expires_at: Timestamp,
    ) -> Result<(), BindError> {
        self.scope_tokens.bind(
            token,
            ScopeBinding {
                project_id: self.project_id,
                task_id,
                worker_id,
                run_id,
                vendor_process: VendorProcessIdentity { pid: vendor_pid },
                expires_at,
            },
        )
    }

    /// A sensible default expiry for [`Self::activate`]: 24 hours from
    /// now. Generous relative to any single run's expected lifetime,
    /// but never unbounded -- the real defense against a vendor process
    /// that outlives its usefulness is prompt [`ScopeTokenStore::revoke_for_run`]
    /// on observed vendor exit (see `crate::coordination::scope_token`'s
    /// module doc), not this expiry.
    #[must_use]
    pub fn default_expiry() -> Timestamp {
        let later = time::OffsetDateTime::now_utc() + time::Duration::hours(24);
        Timestamp::parse(
            &later
                .format(&time::format_description::well_known::Rfc3339)
                .expect("formatting a computed UTC time as RFC 3339 cannot fail"),
        )
        .expect("a freshly formatted RFC 3339 string always parses")
    }
}

/// The `coordination-mcp` subcommand argv, as separate arguments --
/// never shell-joined, so no argument can be split or injected by
/// embedded whitespace in a path.
#[must_use]
pub fn coordination_mcp_argv(context: &McpLaunchContext) -> Vec<String> {
    vec![
        "coordination-mcp".to_string(),
        "--state-dir".to_string(),
        context.state_dir.display().to_string(),
        "--repo".to_string(),
        context.repository.display().to_string(),
        "--run-id".to_string(),
        context.run_id.to_string(),
    ]
}

/// The environment addition for the supervised vendor process (never
/// this runtime's own): only `CREW_WORKER_SCOPE_TOKEN`. The vendor
/// process inherits it into whatever MCP-server child it spawns for
/// `coordination-mcp`; that subprocess reads and removes the variable
/// from its own environment immediately (see
/// `crate::coordination::mcp::ScopeTokenSource`), before it ever
/// touches the socket.
#[must_use]
pub fn coordination_mcp_env(scope_token: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "CREW_WORKER_SCOPE_TOKEN".to_string(),
        scope_token.to_string(),
    );
    env
}

/// The MCP server config block (`{"command":...,"args":[...]}`) every
/// stdio-MCP-consuming adapter embeds under a `"crew"` server name --
/// shaped identically for both Claude and Copilot; only how each
/// adapter *delivers* the surrounding document (a file path vs. an
/// inline JSON argument) differs.
#[must_use]
fn coordination_mcp_server_config(context: &McpLaunchContext) -> Value {
    json!({
        "command": context.crewd_path.display().to_string(),
        "args": coordination_mcp_argv(context),
    })
}

/// The full MCP config document `{"mcpServers":{"crew":{...}}}` both
/// Claude's `--mcp-config` file and Copilot's `--additional-mcp-config`
/// inline argument carry -- identical shape, different delivery.
#[must_use]
pub fn coordination_mcp_config_document(context: &McpLaunchContext) -> Value {
    json!({ "mcpServers": { "crew": coordination_mcp_server_config(context) } })
}

/// The two `-c` override arguments Codex's `codex app-server` command
/// line receives to register the same server as `mcp_servers.crew`
/// without a config file, preserving every other loaded Codex config.
/// Codex's `-c key=value` overrides parse `value` as a TOML value, not
/// JSON -- a TOML basic string for the command, a TOML array of basic
/// strings for args.
#[must_use]
pub fn codex_mcp_overrides(context: &McpLaunchContext) -> Vec<String> {
    let command_value = toml_basic_string(&context.crewd_path.display().to_string());
    let args_value = toml_basic_string_array(&coordination_mcp_argv(context));
    vec![
        "-c".to_string(),
        format!("mcp_servers.crew.command={command_value}"),
        "-c".to_string(),
        format!("mcp_servers.crew.args={args_value}"),
    ]
}

/// TOML's basic-string escape table (spec: every control character must
/// be escaped; a basic string cannot contain one literally). A binary
/// path is exceptionally unlikely to contain a raw newline or other
/// control character, but this never assumes it can't -- every value
/// this module ever needs to embed (a filesystem path, an argv value)
/// is escaped completely, not just for the two characters common paths
/// happen to use.
fn toml_basic_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\u{8}' => escaped.push_str("\\b"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\u{c}' => escaped.push_str("\\f"),
            '\r' => escaped.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                escaped.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    escaped
}

/// A TOML array of basic string literals (`["a", "b"]`).
fn toml_basic_string_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| toml_basic_string(v)).collect();
    format!("[{}]", items.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> McpLaunchContext {
        McpLaunchContext {
            crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
            state_dir: PathBuf::from("/tmp/crew-state"),
            repository: PathBuf::from("/tmp/my-repo"),
            run_id: RunId::new(),
        }
    }

    fn adapter_mcp_config() -> AdapterMcpConfig {
        AdapterMcpConfig {
            scope_tokens: Arc::new(ScopeTokenStore::new()),
            project_id: ProjectId::new(),
            crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
            state_dir: PathBuf::from("/tmp/crew-state"),
            repository: PathBuf::from("/tmp/my-repo"),
        }
    }

    #[test]
    fn argv_is_separate_arguments_never_shell_joined() {
        let context = context();
        let argv = coordination_mcp_argv(&context);
        assert_eq!(
            argv,
            vec![
                "coordination-mcp",
                "--state-dir",
                "/tmp/crew-state",
                "--repo",
                "/tmp/my-repo",
                "--run-id",
                &context.run_id.to_string(),
            ]
        );
    }

    #[test]
    fn env_carries_only_the_scope_token() {
        let env = coordination_mcp_env("a-token");
        assert_eq!(env.len(), 1);
        assert_eq!(
            env.get("CREW_WORKER_SCOPE_TOKEN"),
            Some(&"a-token".to_string())
        );
    }

    #[test]
    fn config_document_matches_the_mcp_server_config_shape_claude_and_copilot_both_expect() {
        let context = context();
        let document = coordination_mcp_config_document(&context);
        assert_eq!(
            document["mcpServers"]["crew"]["command"],
            "/opt/crew/bin/crewd"
        );
        let args = document["mcpServers"]["crew"]["args"].as_array().unwrap();
        assert_eq!(args[0], "coordination-mcp");
        assert_eq!(args.len(), 7);
        // Exactly one server entry -- an adapter merges this under its
        // own already-loaded servers, never replacing them.
        assert_eq!(document["mcpServers"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn codex_overrides_are_two_dash_c_pairs_with_toml_value_syntax() {
        let context = context();
        let overrides = codex_mcp_overrides(&context);
        assert_eq!(overrides.len(), 4);
        assert_eq!(overrides[0], "-c");
        assert_eq!(
            overrides[1],
            "mcp_servers.crew.command=\"/opt/crew/bin/crewd\""
        );
        assert_eq!(overrides[2], "-c");
        assert!(overrides[3].starts_with("mcp_servers.crew.args=[\"coordination-mcp\", "));
        assert!(overrides[3].ends_with(']'));
    }

    #[test]
    fn codex_overrides_escape_toml_special_characters_in_paths() {
        let context = McpLaunchContext {
            crewd_path: PathBuf::from("/opt/crew \"quoted\"/bin/crewd"),
            state_dir: PathBuf::from("/tmp/crew-state"),
            repository: PathBuf::from("/tmp/my-repo"),
            run_id: RunId::new(),
        };
        let overrides = codex_mcp_overrides(&context);
        // The escaped value must still be a single valid TOML basic
        // string: exactly one unescaped opening and one unescaped
        // closing quote around the whole path.
        assert_eq!(
            overrides[1],
            "mcp_servers.crew.command=\"/opt/crew \\\"quoted\\\"/bin/crewd\""
        );
    }

    #[test]
    fn codex_overrides_escape_control_characters_not_just_backslash_and_quote() {
        let context = McpLaunchContext {
            crewd_path: PathBuf::from("/opt/crew\n\t/bin/crewd"),
            state_dir: PathBuf::from("/tmp/crew-state"),
            repository: PathBuf::from("/tmp/my-repo"),
            run_id: RunId::new(),
        };
        let overrides = codex_mcp_overrides(&context);
        assert_eq!(
            overrides[1],
            "mcp_servers.crew.command=\"/opt/crew\\n\\t/bin/crewd\""
        );
        // The escaped value never contains a raw control character --
        // every byte from here on is a printable TOML basic-string body.
        assert!(!overrides[1].contains('\n'));
        assert!(!overrides[1].contains('\t'));
    }

    #[test]
    fn reserve_then_activate_makes_the_token_verifiable() {
        let config = adapter_mcp_config();
        let run_id = RunId::new();
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();

        let token = config.reserve();
        // Not yet live: this is exactly the pre-spawn window a real
        // vendor process's env carries the reserved token through.
        assert!(
            config
                .scope_tokens
                .verify(&token, Some(std::process::id() as i32))
                .is_err()
        );

        let vendor_pid = std::process::id() as i32;
        config
            .activate(
                token.clone(),
                run_id,
                task_id,
                worker_id,
                vendor_pid,
                AdapterMcpConfig::default_expiry(),
            )
            .expect("activating a freshly reserved token succeeds");

        let scoped = config
            .scope_tokens
            .verify(&token, Some(vendor_pid))
            .expect("now live and verifiable");
        assert_eq!(scoped.run_id, run_id);
        assert_eq!(scoped.task_id, task_id);
        assert_eq!(scoped.worker_id, worker_id);
    }

    #[test]
    fn activating_an_already_bound_token_fails_so_the_caller_can_kill_the_vendor() {
        let config = adapter_mcp_config();
        let token = config.reserve();
        config
            .activate(
                token.clone(),
                RunId::new(),
                TaskId::new(),
                WorkerId::new(),
                std::process::id() as i32,
                AdapterMcpConfig::default_expiry(),
            )
            .unwrap();

        let err = config.activate(
            token,
            RunId::new(),
            TaskId::new(),
            WorkerId::new(),
            std::process::id() as i32,
            AdapterMcpConfig::default_expiry(),
        );
        assert!(
            err.is_err(),
            "re-activating a live token must never succeed"
        );
    }

    #[test]
    fn launch_context_carries_the_adapter_configs_paths_for_one_run() {
        let config = adapter_mcp_config();
        let run_id = RunId::new();
        let context = config.launch_context(run_id);
        assert_eq!(context.crewd_path, config.crewd_path);
        assert_eq!(context.state_dir, config.state_dir);
        assert_eq!(context.repository, config.repository);
        assert_eq!(context.run_id, run_id);
    }

    #[test]
    fn default_expiry_is_in_the_future() {
        assert!(AdapterMcpConfig::default_expiry() > Timestamp::now());
    }
}
