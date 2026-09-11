//! The init-time posture assertion: proves the process actually
//! launched matches what this adapter intended, rather than trusting
//! [`super::launch::build_argv`]'s own argv to have taken effect just
//! because nothing errored at spawn time.
//!
//! **What a live capture settled, and what it left open.** A real
//! `system/init` message (`claude_code_version` `2.1.268`, three
//! captures) carries no field naming which `--setting-sources` were
//! actually applied -- so there is nothing to assert about settings
//! application directly, and the CLI-version range check below is not
//! the fallback half of a choice, it is the only posture assertion
//! `system/init` offers for that question. `system/init` does carry
//! `permissionMode`, which the same capture found was NOT pinned by
//! this adapter's own launch argv originally -- an operator's personal
//! `~/.claude.json`/settings default silently governed it instead,
//! including a value that would make `can_use_tool` never reach this
//! adapter's own bridge at all. The maintainer ruled on that departure
//! from the original launch-posture wording directly:
//! [`super::launch::PINNED_PERMISSION_MODE`] is now always passed, and
//! [`permission_mode_gate`] asserts `system/init` actually reports it
//! back -- a DIRECT check, not a proxy: it verifies the pin took
//! effect, rather than inferring posture from a version being in
//! range. Both checks run; they catch different classes (an
//! incompatible binary vs. a setting that failed to apply), and
//! neither replaces the other.

/// The `claude --version` range this adapter's fixed argv was actually
/// launched against and observed to behave as expected -- mirrors
/// `crate::adapter::tui::claude`'s own `MIN_TESTED_VERSION`/
/// `MAX_TESTED_VERSION` precedent and its same honesty about what part
/// of the range is real: the one version this spike's own live capture
/// confirmed is `2.1.268`; the rest of the range is an untested
/// extrapolation, not a second data point.
const MIN_TESTED_VERSION: (u32, u32, u32) = (2, 1, 0);
const MAX_TESTED_VERSION: (u32, u32, u32) = (2, 99, 99);

/// Parses the leading `MAJOR.MINOR.PATCH` out of `system/init`'s own
/// `claude_code_version` field. Returns `None` for anything that does
/// not start with three dot-separated integers -- [`version_gate`]
/// treats an unparseable string as incompatible, never as a silent
/// pass, the same stance `tui::claude`'s own version parser takes.
fn parse_leading_version(reported: &str) -> Option<(u32, u32, u32)> {
    let mut parts = reported.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// Asserts `reported` (`system/init`'s own `claude_code_version`
/// field) falls within the tested range. `Err` names the reported
/// version and the tested range -- `reported` itself is safe to
/// interpolate here, unlike `tui::claude::version_gate`'s own probed
/// `--version` stdout: `system/init` is a structured JSON field this
/// adapter itself parsed, never raw untrusted CLI output that could
/// carry a stack trace or secret-shaped text.
pub(crate) fn version_gate(reported: &str) -> Result<(), String> {
    match parse_leading_version(reported) {
        Some(version) if version >= MIN_TESTED_VERSION && version <= MAX_TESTED_VERSION => Ok(()),
        Some(version) => Err(format!(
            "claude {version:?} is outside the tested range {MIN_TESTED_VERSION:?}..={MAX_TESTED_VERSION:?}"
        )),
        None => Err(format!(
            "system/init's own claude_code_version field ({reported:?}) does not look like a version"
        )),
    }
}

/// Asserts `system/init`'s own reported `permissionMode` equals
/// [`super::launch::PINNED_PERMISSION_MODE`] -- the direct check this
/// module's own doc comment describes: it proves the pin actually took
/// effect, never just that this adapter believes it sent one.
/// `reported` is safe to interpolate for the same reason
/// [`version_gate`]'s own doc comment gives: a structured field this
/// adapter itself parsed, never raw untrusted CLI output.
pub(crate) fn permission_mode_gate(reported: &str) -> Result<(), String> {
    if reported == super::launch::PINNED_PERMISSION_MODE {
        Ok(())
    } else {
        Err(format!(
            "system/init reported permissionMode {reported:?}, expected the pinned {:?} -- the \
             --permission-mode flag this adapter passes did not take effect",
            super::launch::PINNED_PERMISSION_MODE
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_one_version_this_spike_actually_captured() {
        assert_eq!(version_gate("2.1.268"), Ok(()));
    }

    #[test]
    fn accepts_a_version_inside_the_tested_range() {
        assert_eq!(version_gate("2.5.0"), Ok(()));
    }

    #[test]
    fn rejects_a_version_below_the_tested_range() {
        assert!(version_gate("1.9.9").is_err());
    }

    #[test]
    fn rejects_an_unparseable_string() {
        let err = version_gate("not-a-version").unwrap_err();
        assert!(err.contains("not-a-version"));
    }

    #[test]
    fn accepts_the_pinned_mode_reported_back() {
        assert_eq!(
            permission_mode_gate(super::super::launch::PINNED_PERMISSION_MODE),
            Ok(())
        );
    }

    #[test]
    fn rejects_a_different_reported_mode() {
        let err = permission_mode_gate("plan").unwrap_err();
        assert!(err.contains("plan"));
        assert!(err.contains(super::super::launch::PINNED_PERMISSION_MODE));
    }
}
