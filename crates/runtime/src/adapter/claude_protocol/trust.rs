//! The one-time workspace-trust pre-check this adapter must run before
//! ever spawning `claude`: unlike the TUI path (which discovers a trust
//! gate by pattern-matching text claude itself painted, after already
//! spawning it), protocol mode has no terminal to observe -- the check
//! has to happen first, by reading claude's own trust record directly.
//!
//! Crew never writes `~/.claude.json`. This module only ever reads it,
//! and treats every way that read can go wrong (missing file, unreadable,
//! unparseable, no entry for this repo, an entry present but not `true`)
//! identically: not yet trusted. A first-run prompt claude has not yet
//! answered and a config file this process cannot read are the same fact
//! from this adapter's own perspective -- it must not launch either way.

use std::path::Path;

/// Reads `claude_json_path` (in production, `~/.claude.json`) and reports
/// whether `repo_root` has already had its one-time workspace-trust
/// dialog accepted, per claude's own
/// `.projects["<repo_root>"].hasTrustDialogAccepted` record.
///
/// `repo_root` must already be canonicalized by the caller -- this
/// function does no path resolution of its own, matching
/// `crate::adapter::tui::claude::slug_cwd`'s own precondition on its
/// `cwd` argument. Fails closed on every error shape (missing file,
/// unreadable, malformed JSON, no entry for this repo, or an entry whose
/// flag is present but not literally `true`): the caller must treat
/// anything other than a proven `true` as "not yet trusted," never guess
/// in claude's favor.
#[must_use]
pub(crate) fn workspace_trust_accepted(claude_json_path: &Path, repo_root: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(claude_json_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    let key = repo_root.to_string_lossy();
    value
        .get("projects")
        .and_then(|projects| projects.get(key.as_ref()))
        .and_then(|project| project.get("hasTrustDialogAccepted"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// The real, per-user `~/.claude.json` path this check reads in
/// production -- the same file [`super::super::event_sink`]'s
/// `first_run_gate_question` already names for the TUI-path equivalent
/// of this same one-time step. `None` when `HOME` cannot be resolved;
/// the caller must treat that the same as "not trusted" rather than
/// guessing a path.
#[must_use]
pub(crate) fn default_claude_json_path() -> Option<std::path::PathBuf> {
    dirs_home().map(|home| home.join(".claude.json"))
}

/// The one place this module reads `HOME` -- a thin seam so tests never
/// have to mutate real process environment (which races every other
/// test in the same binary) to prove the "cannot resolve HOME" arm.
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_claude_json(dir: &tempfile::TempDir, contents: &str) -> PathBuf {
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn a_missing_file_is_not_trusted() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert!(!workspace_trust_accepted(&path, Path::new("/repo")));
    }

    #[test]
    fn malformed_json_is_not_trusted() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_claude_json(&dir, "not json at all");
        assert!(!workspace_trust_accepted(&path, Path::new("/repo")));
    }

    #[test]
    fn no_entry_for_this_repo_is_not_trusted() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_claude_json(
            &dir,
            r#"{"projects":{"/some/other/repo":{"hasTrustDialogAccepted":true}}}"#,
        );
        assert!(!workspace_trust_accepted(&path, Path::new("/repo")));
    }

    /// A `false` flag is a claude-authored fact ("asked, and declined" or
    /// "asked, not yet answered persistently"), not an absence -- still
    /// not trusted, on the same fail-closed basis.
    #[test]
    fn a_present_but_false_flag_is_not_trusted() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_claude_json(
            &dir,
            r#"{"projects":{"/repo":{"hasTrustDialogAccepted":false}}}"#,
        );
        assert!(!workspace_trust_accepted(&path, Path::new("/repo")));
    }

    #[test]
    fn a_true_flag_for_this_exact_repo_is_trusted() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_claude_json(
            &dir,
            r#"{"projects":{"/repo":{"hasTrustDialogAccepted":true},"/other":{"hasTrustDialogAccepted":false}}}"#,
        );
        assert!(workspace_trust_accepted(&path, Path::new("/repo")));
    }
}
