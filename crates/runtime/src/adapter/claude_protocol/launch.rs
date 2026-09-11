//! The fixed argv this adapter launches `claude` with -- the launch
//! posture settled before any code here existed: plain `-p`, both stream-json
//! formats, an explicit `--setting-sources` naming every source claude
//! would already consult on its own, and nothing that suppresses the
//! vendor's own hooks or hides its settings from it. No `--bare`, no
//! `disableAllHooks`: this spike drives a real, normally-configured
//! claude, not a stripped-down one -- the terminal-path adapters make
//! the identical choice for the same reason (`ClaudeTuiVendor`'s own
//! module doc comment: "interactive `claude`, never a mode that hides
//! its own settings from it").
//!
//! A pure function, deliberately: every flag this adapter passes is
//! assertable against a `Vec<String>` without spawning anything, the
//! same shape `claude_protocol::approval_bridge`'s parse/build pair and
//! `claude_protocol::reconcile::find_gaps` already take.

/// Builds the argv `claude` is launched with, everything after the
/// binary name itself. `model`, when given, is placed exactly where
/// [`crate::adapter::tui::claude::ClaudeTuiVendor::base_args`] places its
/// own `--model` -- but where that function reads `cfg.model` (the
/// boot-loaded config's value only; a run's own resolved model choice
/// is a known, separately-tracked gap that function still has, one this
/// adapter is known to inherit until a fix for it lands on the shared
/// construction path and is merged into this branch too), this one
/// takes it as a plain parameter: the precedence between a run's own
/// resolved model and the boot config is that fix's to establish, not
/// this adapter's to reinvent, and this function has no opinion on
/// which value it was handed -- it only places it.
#[must_use]
pub(crate) fn build_argv(model: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "-p".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--setting-sources".to_string(),
        "user,project,local".to_string(),
    ];
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_d6_posture_flags_are_always_present_and_never_bare() {
        let argv = build_argv(None);
        assert!(argv.contains(&"-p".to_string()));
        assert!(
            argv.windows(2)
                .any(|w| w == ["--input-format", "stream-json"])
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(
            argv.windows(2)
                .any(|w| w == ["--setting-sources", "user,project,local"])
        );
        assert!(!argv.contains(&"--bare".to_string()));
        assert!(!argv.iter().any(|arg| arg.contains("disableAllHooks")));
    }

    #[test]
    fn no_model_flag_when_none_is_given() {
        let argv = build_argv(None);
        assert!(!argv.contains(&"--model".to_string()));
    }

    #[test]
    fn a_given_model_is_placed_verbatim() {
        let argv = build_argv(Some("claude-sonnet-5"));
        assert!(argv.windows(2).any(|w| w == ["--model", "claude-sonnet-5"]));
    }
}
