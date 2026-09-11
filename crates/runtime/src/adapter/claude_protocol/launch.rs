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
//!
//! **Departs from the maintainer-approved launch-posture wording**, which did not
//! name a `--permission-mode`: a live capture found that without one,
//! the effective mode is inherited from the operator's own
//! `~/.claude.json`/settings default, silently, including a value
//! (`bypassPermissions`) that would make `can_use_tool` never reach
//! this adapter's own bridge at all -- permission mode is load-bearing
//! for permissions, and the standing rule is that crew passes anything
//! load-bearing explicitly rather than inherits it. The maintainer
//! ruled on this departure directly: pin it, and pin a permissive
//! value. [`PINNED_PERMISSION_MODE`] is that value.

/// The permission mode this adapter pins explicitly -- `"auto"`, not
/// the `--allow-dangerously-skip-permissions` flag: that flag only
/// makes bypass *available* (its own `--help` text: "without it being
/// enabled by default"), it does not set a mode, so passing it alone
/// would still leave the effective mode inherited from the operator's
/// own settings -- exactly the defect this pin exists to close.
/// `"auto"`'s own semantics are themselves unmeasured by this spike (a
/// finding of their own, not assumed); what is pinned here is the
/// SPELLING crew passes, not a claim about what it permits.
///
/// A named constant, not an inline literal, specifically so
/// [`super::posture`]'s own direct assertion
/// (`system/init`'s reported `permissionMode` equals what crew passed)
/// compares against the SAME value crew actually sent, never a second,
/// independently-typed copy of the same string that could drift from
/// this one.
pub(crate) const PINNED_PERMISSION_MODE: &str = "auto";

/// `--permission-prompts` is pinned too, on the same ground as
/// `PINNED_PERMISSION_MODE` -- this is the flag that decides whether
/// the host is consulted about a permission decision AT ALL, load-
/// bearing for permissions by definition. Pinning it changes nothing
/// today (`"host"` is already the CLI's own default), which is exactly
/// the point: today's default staying the default is not something
/// this adapter should depend on staying true by accident.
pub(crate) const PINNED_PERMISSION_PROMPTS: &str = "host";

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
        "--permission-mode".to_string(),
        PINNED_PERMISSION_MODE.to_string(),
        "--permission-prompts".to_string(),
        PINNED_PERMISSION_PROMPTS.to_string(),
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

    /// The maintainer-ruled departure from the original launch-posture wording,
    /// proven directly: `--permission-mode` is always pinned to
    /// [`PINNED_PERMISSION_MODE`], never left to be inherited from the
    /// operator's own settings.
    #[test]
    fn permission_mode_is_always_pinned() {
        let argv = build_argv(None);
        assert!(
            argv.windows(2)
                .any(|w| w == ["--permission-mode", PINNED_PERMISSION_MODE])
        );
    }

    /// The same pin, for the flag that decides whether the host is
    /// consulted at all -- a consequence of the same rule, not a second decision (see this
    /// module's own doc comment).
    #[test]
    fn permission_prompts_is_always_pinned() {
        let argv = build_argv(None);
        assert!(
            argv.windows(2)
                .any(|w| w == ["--permission-prompts", PINNED_PERMISSION_PROMPTS])
        );
    }

    /// `--allow-dangerously-skip-permissions` only makes bypass
    /// available; it does not pin a mode. Never on this argv, on
    /// purpose -- see this module's own doc comment on why it would
    /// satisfy the letter of "pass a flag" and none of the actual rule.
    #[test]
    fn never_passes_the_skip_permissions_flag() {
        let argv = build_argv(None);
        assert!(!argv.iter().any(|arg| arg.contains("skip-permissions")));
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
