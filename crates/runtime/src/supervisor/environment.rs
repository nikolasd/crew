//! Environment policy for supervised vendor processes.
//!
//! The environment builder starts from a minimal safe base (`HOME`,
//! `PATH`, locale, terminal identity, approved `XDG_*`) and adds
//! adapter-profile keys only after explicit allowlisting -- an inherited
//! secret-shaped variable (e.g. `ANTHROPIC_API_KEY`) is absent from a
//! supervised process's environment unless the worker profile's
//! `environmentAllowlist` names it explicitly. Any diagnostic snapshot of
//! an environment map redacts every value, never a name.

use std::collections::{HashMap, HashSet};

/// The literal string every redacted environment value renders as.
pub const REDACTED_PLACEHOLDER: &str = "[REDACTED]";

/// Builds a supervised process's environment from a safe base plus an
/// explicit per-spawn allowlist.
#[derive(Debug, Clone)]
pub struct EnvironmentPolicy {
    base_names: HashSet<String>,
}

impl EnvironmentPolicy {
    /// The minimal safe base: variables needed for a process to run and
    /// locate its own files at all, never a secret-shaped name.
    #[must_use]
    pub fn baseline() -> Self {
        let mut base_names = HashSet::new();
        for name in [
            "HOME", "PATH", "LANG", "LC_ALL", "TERM", "TZ", "SHELL", "USER", "LOGNAME",
        ] {
            base_names.insert(name.to_string());
        }
        Self { base_names }
    }

    /// Builds the environment a supervised process may inherit:
    /// `current_env`'s base-allowlisted and approved-`XDG_*` names, plus
    /// any name in `extra_allowed` (typically a validated
    /// `WorkerProfile::environmentAllowlist`). Every other name -- in
    /// particular any secret-shaped variable not explicitly named -- is
    /// excluded.
    #[must_use]
    pub fn build(
        &self,
        current_env: &HashMap<String, String>,
        extra_allowed: &[String],
    ) -> HashMap<String, String> {
        let extra: HashSet<&str> = extra_allowed.iter().map(String::as_str).collect();
        let mut env: HashMap<String, String> = current_env
            .iter()
            .filter(|(name, _)| {
                self.base_names.contains(name.as_str())
                    || is_approved_xdg(name)
                    || extra.contains(name.as_str())
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        // A supervised TUI vendor (claude/codex/copilot/omp-rpc) needs a real
        // terminal type to render and accept input. `TERM=dumb` (or unset)
        // makes Ink-based REPLs establish no session file, so substitute a
        // safe default rather than propagating the degenerate value.
        match env.get("TERM").map(String::as_str) {
            Some(t) if t.is_empty() || t == "dumb" || t == "unknown" => {
                env.insert("TERM".to_string(), "xterm-256color".to_string());
            }
            None => {
                env.insert("TERM".to_string(), "xterm-256color".to_string());
            }
            _ => {}
        }
        env
    }
}

impl Default for EnvironmentPolicy {
    fn default() -> Self {
        Self::baseline()
    }
}

/// Approved `XDG_*` base-directory variables. Only these four are ever
/// inherited automatically; an arbitrary `XDG_`-prefixed name is not
/// assumed safe just because of its prefix.
fn is_approved_xdg(name: &str) -> bool {
    matches!(
        name,
        "XDG_CONFIG_HOME" | "XDG_DATA_HOME" | "XDG_CACHE_HOME" | "XDG_STATE_HOME"
    )
}

/// Redacts every value in an environment map for logging/diagnostics,
/// preserving names (so a diagnostic can still show *which* variables
/// were set) but never a value. The only sound way to log a supervised
/// process's environment.
#[must_use]
pub fn redacted_env_snapshot(env: &HashMap<String, String>) -> HashMap<String, String> {
    env.keys()
        .map(|name| (name.clone(), REDACTED_PLACEHOLDER.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unapproved_xdg_name_is_excluded() {
        let mut env = HashMap::new();
        env.insert("XDG_RUNTIME_DIR".to_string(), "/run/user/1000".to_string());
        env.insert("XDG_CONFIG_HOME".to_string(), "/home/u/.config".to_string());
        let built = EnvironmentPolicy::baseline().build(&env, &[]);
        assert!(!built.contains_key("XDG_RUNTIME_DIR"));
        assert!(built.contains_key("XDG_CONFIG_HOME"));
    }
}
