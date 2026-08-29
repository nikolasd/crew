//! tmux display backend: real session/pane operations via `tmux
//! new-window`/`split-window`/`select-pane`/`kill-pane`, using tmux's own
//! well-established `-P -F` print-format convention to recover created
//! pane/window identifiers without parsing free-form terminal output.

use crew_protocol::{DisplayBackend, DisplayConfig, DisplayPlacement, DisplayStatus};
use parking_lot::Mutex;
use std::sync::Arc;

use super::{
    CommandExecutor, CommandResult, DisplayBackendTrait, DisplayFuture, PaneHandle, PaneRequest,
    RealCommandExecutor, version_gte,
};

/// Tmux display backend.
///
/// Compatibility gate: checks tmux is installed and parses version.
/// Minimum required version: 3.0. Additionally requires a real,
/// already-running tmux session (`tmux display-message`) -- tmux is
/// enabled only inside a valid session; this backend never starts an
/// ambient server as a side effect of a mere availability check.
pub struct TmuxDisplay {
    #[allow(dead_code)]
    // carried for parity with HerdrDisplay/HiddenDisplay; no field of it is read yet
    config: DisplayConfig,
    min_version: String,
    session_active: bool,
    session_name: Option<String>,
    executor: Arc<dyn CommandExecutor>,
    owned_panes: Mutex<Vec<String>>,
}

impl TmuxDisplay {
    #[must_use]
    pub fn new(config: DisplayConfig) -> Self {
        TmuxDisplay {
            config,
            min_version: "3.0".to_string(),
            session_active: false,
            session_name: None,
            executor: Arc::new(RealCommandExecutor::new()),
            owned_panes: Mutex::new(Vec::new()),
        }
    }

    /// Creates a TmuxDisplay with a custom command executor (for testing).
    #[must_use]
    pub fn with_executor(config: DisplayConfig, executor: Arc<dyn CommandExecutor>) -> Self {
        TmuxDisplay {
            config,
            min_version: "3.0".to_string(),
            session_active: false,
            session_name: None,
            executor,
            owned_panes: Mutex::new(Vec::new()),
        }
    }

    /// Checks if tmux is available and compatible using the injected executor.
    fn check_tmux(&self, min_version: &str) -> bool {
        match self.executor.execute("tmux", &["--version"]) {
            Ok(result) if result.success => {
                let version_str = String::from_utf8_lossy(&result.stdout);
                let version = version_str
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .trim()
                    .split(|c: char| !c.is_ascii_digit() && c != '.')
                    .next()
                    .unwrap_or("");
                version_gte(version, min_version)
            }
            _ => false,
        }
    }

    /// Whether a real tmux session is currently attached/reachable.
    /// Never starts a server: a missing session answers `false`, it does
    /// not create one.
    fn inside_a_real_session(&self) -> bool {
        matches!(
            self.executor
                .execute("tmux", &["display-message", "-p", "#{session_id}"]),
            Ok(CommandResult { success: true, .. })
        )
    }

    /// Activates tmux by attaching to a session using the injected executor.
    fn activate_tmux(&self, session_name: &str) -> Result<(), String> {
        match self
            .executor
            .execute("tmux", &["new-session", "-d", "-s", session_name])
        {
            Ok(result) if result.success => Ok(()),
            Ok(result) => {
                let stderr = String::from_utf8_lossy(&result.stderr);
                Err(format!("tmux exited with error: {stderr}"))
            }
            Err(e) => Err(format!("failed to spawn tmux session: {e}")),
        }
    }

    /// Creates a new Crew-owned pane running `req.command` at
    /// `req.placement`, titled `req.title`, using tmux's `-P -F
    /// '#{pane_id}'` print-format convention to recover the created
    /// pane's id directly, without parsing free-form terminal output.
    ///
    /// # Errors
    /// Returns a `capability_unsupported`-shaped message for
    /// `DisplayPlacement::Workspace` (tmux has no workspace concept) and
    /// `DisplayPlacement::Embedded` (nothing to split); a message
    /// naming "no active tmux session" if [`Self::inside_a_real_session`]
    /// fails (this call never starts one); or a message if the
    /// split/window/title command itself fails.
    fn create_pane_sync(&self, req: &PaneRequest) -> Result<String, String> {
        if req.placement == DisplayPlacement::Workspace {
            return Err(
                "capability_unsupported: tmux has no workspace concept; use Tab, SplitRight, or \
                 SplitDown"
                    .to_string(),
            );
        }
        if req.placement == DisplayPlacement::Embedded {
            return Err(
                "DisplayPlacement::Embedded creates no separate pane; tmux has nothing to split"
                    .to_string(),
            );
        }
        if req.placement == DisplayPlacement::Window {
            return Err(
                "DisplayPlacement::Window is OsWindowDisplay's own actual-outcome placement, \
                 never a valid request for tmux"
                    .to_string(),
            );
        }
        if !self.inside_a_real_session() {
            return Err(
                "no active tmux session; refusing to start an ambient one for pane creation"
                    .to_string(),
            );
        }

        let subcommand = match req.placement {
            DisplayPlacement::Tab => "new-window",
            DisplayPlacement::SplitRight | DisplayPlacement::SplitDown => "split-window",
            DisplayPlacement::Embedded | DisplayPlacement::Workspace | DisplayPlacement::Window => {
                unreachable!("handled above")
            }
        };
        let mut argv: Vec<String> = vec![subcommand.to_string()];
        match req.placement {
            DisplayPlacement::SplitRight => argv.push("-h".to_string()),
            DisplayPlacement::SplitDown => argv.push("-v".to_string()),
            DisplayPlacement::Tab
            | DisplayPlacement::Embedded
            | DisplayPlacement::Workspace
            | DisplayPlacement::Window => {}
        }
        argv.push("-P".to_string());
        argv.push("-F".to_string());
        argv.push("#{pane_id}".to_string());
        argv.push("--".to_string());
        argv.extend(req.command.iter().cloned());
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();

        let pane_id = match self.executor.execute("tmux", &argv_refs) {
            Ok(CommandResult {
                success: true,
                stdout,
                ..
            }) => String::from_utf8_lossy(&stdout).trim().to_string(),
            Ok(CommandResult { stderr, .. }) => {
                return Err(format!(
                    "tmux {subcommand} exited with error: {}",
                    String::from_utf8_lossy(&stderr)
                ));
            }
            Err(e) => return Err(format!("failed to run tmux {subcommand}: {e}")),
        };
        if pane_id.is_empty() {
            return Err(format!("tmux {subcommand} produced no pane id"));
        }

        if let Err(err) = self.execute_or_err(
            &["select-pane", "-t", &pane_id, "-T", &req.title],
            "select-pane",
        ) {
            let _ = self.execute_or_err(&["kill-pane", "-t", &pane_id], "cleanup kill-pane");
            return Err(err);
        }

        self.owned_panes.lock().push(pane_id.clone());
        Ok(pane_id)
    }

    /// Closes `pane_id` only if this backend recorded it as owned, in
    /// the same session it was created in; existing unrelated panes are
    /// never modified or closed.
    ///
    /// # Errors
    /// Returns a message if `pane_id` is not tracked as Crew-owned, or
    /// the close command itself fails.
    fn close_owned_pane(&self, pane_id: &str) -> Result<(), String> {
        {
            let mut panes = self.owned_panes.lock();
            let Some(index) = panes.iter().position(|p| p == pane_id) else {
                return Err(format!(
                    "refusing to close pane {pane_id}: not tracked as owned by this backend"
                ));
            };
            panes.remove(index);
        }
        self.execute_or_err(&["kill-pane", "-t", pane_id], "kill-pane")
    }

    /// The panes this backend currently tracks as owned, for tests and
    /// diagnostics.
    #[must_use]
    pub fn owned_pane_ids(&self) -> Vec<String> {
        self.owned_panes.lock().clone()
    }

    fn execute_or_err(&self, args: &[&str], what: &str) -> Result<(), String> {
        match self.executor.execute("tmux", args) {
            Ok(CommandResult { success: true, .. }) => Ok(()),
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "tmux {what} exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run tmux {what}: {e}")),
        }
    }
}

impl DisplayBackendTrait for TmuxDisplay {
    fn backend_name(&self) -> &str {
        "tmux"
    }

    fn is_available(&self) -> bool {
        self.check_tmux(&self.min_version) && self.inside_a_real_session()
    }

    fn activate(&mut self) -> Result<(), String> {
        if !self.is_available() {
            return Err("tmux not found, incompatible version, or no active session".to_string());
        }
        match self.activate_tmux("crew-session") {
            Ok(()) => {
                self.mark_session_active("crew-session".to_string());
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn status(&self) -> DisplayStatus {
        DisplayStatus {
            backend: DisplayBackend::Tmux,
            available: self.is_available(),
            active: self.session_active,
            dimensions: None,
        }
    }

    fn create_pane(&self, req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
        Box::pin(async move {
            let pane_ref = self.create_pane_sync(&req)?;
            Ok(PaneHandle {
                backend: DisplayBackend::Tmux,
                pane_ref,
                placement: req.placement,
            })
        })
    }

    fn close_pane(&self, handle: &PaneHandle) -> DisplayFuture<'_, ()> {
        let pane_ref = handle.pane_ref.clone();
        Box::pin(async move { self.close_owned_pane(&pane_ref) })
    }
}

impl TmuxDisplay {
    /// Marks a session as active.
    pub fn mark_session_active(&mut self, session_name: String) {
        self.session_active = true;
        self.session_name = Some(session_name);
    }

    /// Marks a session as inactive.
    pub fn mark_session_inactive(&mut self) {
        self.session_active = false;
        self.session_name = None;
    }
}
