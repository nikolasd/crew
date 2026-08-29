//! OS-native terminal window display backend: opens a new terminal
//! window (or, where the target terminal's own scripting API supports
//! it, a real tab) running the pane's command directly through the
//! desktop's own terminal application, rather than a multiplexer.
//!
//! macOS dispatches on [`PaneRequest::launch_program`] (CREW-9), the
//! submitting caller's own `$TERM_PROGRAM`, to pick the right terminal
//! application instead of always assuming Terminal.app:
//!
//! * **iTerm2** (`HostProgramHint::ITerm2`): a real tab in the current
//!   window via iTerm2's long-stable AppleScript API (`create tab with
//!   default profile`, then `write text` into its session).
//! * **Terminal.app**, and the fallback for an absent or unrecognized
//!   hint: `do script` opens a new window (Terminal.app has no
//!   externally-triggerable tab-creation command), followed by an
//!   explicit `activate` -- fixing the pre-CREW-9 bug where the window
//!   opened un-foregrounded.
//! * **Ghostty**: a real tab via its AppleScript dictionary (shipped in
//!   1.3.0+, `ghostty.org/docs/features/applescript`) -- `new tab in
//!   window 1 with configuration {command:"..."}`, which runs the argv
//!   directly (no `write text` typing step, unlike Terminal.app/iTerm2)
//!   and auto-foregrounds the app on its own. A pre-1.3.0 install (no
//!   such command) is feature-detected by that attempt failing, not a
//!   version check, and falls back to a plain new window via `open -na
//!   Ghostty --args -e <cmd>`.
//!
//! Every one of these reports its *actual* placement on [`PaneHandle`]
//! (CREW-9) -- `DisplayPlacement::Tab` for iTerm2's real tab,
//! `DisplayPlacement::Window` for a plain new window -- never blindly
//! echoing back whatever `req.placement` asked for.
//!
//! `req.launch_program` is a closed, caller-untrusted enum (never a raw
//! string) precisely because its value selects and parameterizes an
//! `osascript` invocation: see [`crew_protocol::HostProgramHint`]'s own
//! doc comment for why raw `$TERM_PROGRAM` content must never reach
//! script text.
//!
//! `do script`/iTerm2's `write text` both message an already-running
//! application and return immediately (they do not wait for the launched
//! command to finish), so those paths run through the ordinary blocking
//! [`CommandExecutor::execute`] seam like every other backend. Their
//! return value -- printed to stdout by `osascript` -- is a live
//! AppleScript reference to the new tab/window, captured verbatim as the
//! pane ref and later spliced back into a `close (...)` script (or
//! iTerm2's own close idiom) to close that exact one. `open -na`
//! (Ghostty) and Linux's `x-terminal-emulator` do not have an analogous
//! reference to capture; see their own methods for how each is tracked
//! instead.
//!
//! Linux: `x-terminal-emulator -T <title> -e <cmd...>`. Unlike
//! `osascript`, this genuinely is the long-running GUI process (it does
//! not return until its window closes), so this path uses
//! [`CommandExecutor::spawn_detached`] instead -- the non-blocking seam
//! -- and tracks the spawned pid as the pane ref, closed later with
//! `kill`.
//!
//! Platform dispatch is a runtime field (not a `#[cfg(target_os)]`
//! split), so both code paths are unit-testable regardless of which
//! platform actually runs the test suite; production construction
//! ([`OsWindowDisplay::new`]) still only ever picks the real one.

use parking_lot::Mutex;
use std::sync::Arc;

use crew_protocol::{
    DisplayBackend, DisplayConfig, DisplayPlacement, DisplayStatus, HostProgramHint,
};

use super::{
    CommandExecutor, CommandResult, DisplayBackendTrait, DisplayFuture, PaneHandle, PaneRequest,
    RealCommandExecutor,
};

/// Which desktop this backend is targeting. See the module doc: this is
/// a runtime field precisely so tests can force either branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    MacOs,
    Linux,
}

fn detected_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Linux
    }
}

/// Which close mechanism a tracked pane ref needs -- recorded alongside
/// the ref itself at creation time so [`OsWindowDisplay::close_owned_pane`]
/// never has to guess a terminal's closing idiom from the shape of an
/// opaque string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OwnedPaneKind {
    /// Terminal.app: `close (<ref>) saving no`.
    TerminalApp,
    /// iTerm2: closes the session by id.
    ITerm2,
    /// Ghostty (1.3.0+, AppleScript dictionary present): closes the tab
    /// by re-finding it from its stored `id` -- see
    /// [`OsWindowDisplay::close_pane_ghostty`].
    Ghostty,
    /// A window this backend cannot close on its own (Ghostty's `open
    /// -na` fallback for installs predating its AppleScript dictionary,
    /// which returns nothing identifying the launched window).
    /// `close_pane` is a documented no-op for these -- the window
    /// persists until the user closes it, exactly like `CloseOnExit::
    /// Never` already behaves for every backend.
    Unclosable,
    /// Linux: `kill <pid>`.
    LinuxProcess,
}

/// OS-native terminal window display backend.
pub struct OsWindowDisplay {
    #[allow(dead_code)] // carried for parity with the other backends; no field of it is read yet
    config: DisplayConfig,
    executor: Arc<dyn CommandExecutor>,
    platform: Platform,
    owned_panes: Mutex<Vec<(String, OwnedPaneKind)>>,
}

impl OsWindowDisplay {
    #[must_use]
    pub fn new(config: DisplayConfig) -> Self {
        Self::with_executor(config, Arc::new(RealCommandExecutor::new()))
    }

    /// Creates an `OsWindowDisplay` with a custom command executor (for testing).
    #[must_use]
    pub fn with_executor(config: DisplayConfig, executor: Arc<dyn CommandExecutor>) -> Self {
        OsWindowDisplay {
            config,
            executor,
            platform: detected_platform(),
            owned_panes: Mutex::new(Vec::new()),
        }
    }

    /// The probe binary whose presence on `PATH` `is_available` checks
    /// for this platform.
    fn probe_binary(&self) -> &'static str {
        match self.platform {
            Platform::MacOs => "osascript",
            Platform::Linux => "x-terminal-emulator",
        }
    }

    fn create_pane_sync(&self, req: &PaneRequest) -> Result<(String, DisplayPlacement), String> {
        match self.platform {
            Platform::MacOs => self.create_pane_macos(req),
            Platform::Linux => self.create_pane_linux(req),
        }
    }

    fn create_pane_macos(&self, req: &PaneRequest) -> Result<(String, DisplayPlacement), String> {
        match req.launch_program {
            Some(HostProgramHint::ITerm2) => self.create_pane_iterm2(req),
            Some(HostProgramHint::Ghostty) => self.create_pane_ghostty(req),
            // Terminal.app, an unrecognized hint, and no hint at all --
            // there is nothing more specific to do for any of them than
            // the same Terminal.app fallback.
            Some(HostProgramHint::AppleTerminal) | Some(HostProgramHint::Other) | None => {
                self.create_pane_terminal_app(req)
            }
        }
    }

    /// Terminal.app has no externally-triggerable tab-creation command,
    /// so this always opens a new window -- `do script` on its own also
    /// does not foreground it, hence the explicit `activate` alongside it
    /// (both inside one `tell` block, so `do script` -- the tab/window
    /// reference this needs to capture -- stays the script's final
    /// evaluated value).
    fn create_pane_terminal_app(
        &self,
        req: &PaneRequest,
    ) -> Result<(String, DisplayPlacement), String> {
        let script = format!(
            "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
            applescript_escape(&shell_join(&req.command))
        );
        match self.executor.execute("osascript", &["-e", &script]) {
            Ok(CommandResult {
                success: true,
                stdout,
                ..
            }) => {
                let pane_ref = String::from_utf8_lossy(&stdout).trim().to_string();
                if pane_ref.is_empty() {
                    return Err("osascript do script produced no tab/window reference".to_string());
                }
                self.owned_panes
                    .lock()
                    .push((pane_ref.clone(), OwnedPaneKind::TerminalApp));
                Ok((pane_ref, DisplayPlacement::Window))
            }
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "osascript exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run osascript: {e}")),
        }
    }

    /// iTerm2's AppleScript API supports real tabs directly: a new tab in
    /// the current window, then `write text` to run the command in its
    /// session -- `activate` first so a not-yet-running iTerm2 has a
    /// window to add a tab to. The script's final value is the new
    /// session's own `id`, captured as the pane ref.
    fn create_pane_iterm2(&self, req: &PaneRequest) -> Result<(String, DisplayPlacement), String> {
        let script = format!(
            "tell application \"iTerm\"\n\
             activate\n\
             tell current window\n\
             create tab with default profile\n\
             tell current session\n\
             write text \"{}\"\n\
             return id\n\
             end tell\n\
             end tell\n\
             end tell",
            applescript_escape(&shell_join(&req.command))
        );
        match self.executor.execute("osascript", &["-e", &script]) {
            Ok(CommandResult {
                success: true,
                stdout,
                ..
            }) => {
                let pane_ref = String::from_utf8_lossy(&stdout).trim().to_string();
                if pane_ref.is_empty() {
                    return Err("osascript produced no iTerm2 session id".to_string());
                }
                self.owned_panes
                    .lock()
                    .push((pane_ref.clone(), OwnedPaneKind::ITerm2));
                Ok((pane_ref, DisplayPlacement::Tab))
            }
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "osascript exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run osascript: {e}")),
        }
    }

    /// Ghostty 1.3.0+ ships an AppleScript dictionary with a real tab
    /// command: `new tab in window 1 with configuration {command:"..."}`,
    /// returning a tab object whose `id` (e.g. `"tab-9578b4000"`) is
    /// stable across separate `osascript` invocations -- captured as the
    /// pane ref and later re-found by [`Self::close_pane_ghostty`].
    /// Creating a tab this way auto-foregrounds Ghostty, so no separate
    /// `activate` (doing so would fight the app's own focus handling).
    /// `command` runs the argv directly inside the new surface; there is
    /// no `write text`/typing step to race, unlike Terminal.app/iTerm2.
    ///
    /// A pre-1.3.0 Ghostty (no such command) fails this attempt with an
    /// Apple Event "doesn't understand" error rather than crashing --
    /// feature-detected here by that failure, not a version string, per
    /// [`Self::create_pane_ghostty_fallback`].
    fn create_pane_ghostty(&self, req: &PaneRequest) -> Result<(String, DisplayPlacement), String> {
        let script = format!(
            "tell application \"Ghostty\"\n\
             set t to new tab in window 1 with configuration {{command:\"{}\"}}\n\
             return id of t\n\
             end tell",
            applescript_escape(&shell_join(&req.command))
        );
        match self.executor.execute("osascript", &["-e", &script]) {
            Ok(CommandResult {
                success: true,
                stdout,
                ..
            }) => {
                let pane_ref = String::from_utf8_lossy(&stdout).trim().to_string();
                if pane_ref.is_empty() {
                    return Err("osascript produced no Ghostty tab id".to_string());
                }
                self.owned_panes
                    .lock()
                    .push((pane_ref.clone(), OwnedPaneKind::Ghostty));
                Ok((pane_ref, DisplayPlacement::Tab))
            }
            // Anything from a missing dictionary (<1.3.0) to no window to
            // target: fall back to a plain new window rather than fail
            // the run outright.
            Ok(CommandResult { .. }) => self.create_pane_ghostty_fallback(req),
            Err(e) => Err(format!("failed to run osascript: {e}")),
        }
    }

    /// Pre-1.3.0 Ghostty fallback: a new window via `open -na`, never
    /// AppleScript. `open` returns nothing identifying the window it
    /// asked Ghostty to open (unlike `spawn_detached`'s pid for Linux,
    /// `open`'s own pid is the short-lived launch helper, not the
    /// terminal window), so this pane is untracked and
    /// [`Self::close_owned_pane`] cannot close it later -- a synthetic,
    /// uniquely-numbered ref is used only to satisfy the
    /// non-empty-means-a-real-pane-opened convention ([`PaneHandle`]'s
    /// own doc comment), never to identify anything `open` could act on.
    fn create_pane_ghostty_fallback(
        &self,
        req: &PaneRequest,
    ) -> Result<(String, DisplayPlacement), String> {
        let mut argv: Vec<String> = vec![
            "-na".to_string(),
            "Ghostty".to_string(),
            "--args".to_string(),
            "-e".to_string(),
        ];
        argv.extend(req.command.iter().cloned());
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        match self.executor.execute("open", &argv_refs) {
            Ok(CommandResult { success: true, .. }) => {
                let pane_ref = format!("ghostty-window-{}", uuid::Uuid::now_v7());
                self.owned_panes
                    .lock()
                    .push((pane_ref.clone(), OwnedPaneKind::Unclosable));
                Ok((pane_ref, DisplayPlacement::Window))
            }
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "open -na Ghostty exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run open -na Ghostty: {e}")),
        }
    }

    fn create_pane_linux(&self, req: &PaneRequest) -> Result<(String, DisplayPlacement), String> {
        let mut argv: Vec<String> = vec!["-T".to_string(), req.title.clone(), "-e".to_string()];
        argv.extend(req.command.iter().cloned());
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();

        match self
            .executor
            .spawn_detached("x-terminal-emulator", &argv_refs)
        {
            Ok(pid) => {
                let pane_ref = pid.to_string();
                self.owned_panes
                    .lock()
                    .push((pane_ref.clone(), OwnedPaneKind::LinuxProcess));
                Ok((pane_ref, DisplayPlacement::Window))
            }
            Err(e) => Err(format!("failed to spawn x-terminal-emulator: {e}")),
        }
    }

    fn close_owned_pane(&self, pane_ref: &str) -> Result<(), String> {
        let kind = {
            let mut owned = self.owned_panes.lock();
            let Some(index) = owned.iter().position(|(r, _)| r == pane_ref) else {
                return Err(format!(
                    "refusing to close pane {pane_ref}: not tracked as owned by this backend"
                ));
            };
            owned.remove(index).1
        };
        match kind {
            OwnedPaneKind::TerminalApp => self.close_pane_terminal_app(pane_ref),
            OwnedPaneKind::ITerm2 => self.close_pane_iterm2(pane_ref),
            OwnedPaneKind::Ghostty => self.close_pane_ghostty(pane_ref),
            OwnedPaneKind::Unclosable => Ok(()),
            OwnedPaneKind::LinuxProcess => self.close_pane_linux(pane_ref),
        }
    }

    fn close_pane_terminal_app(&self, pane_ref: &str) -> Result<(), String> {
        // `pane_ref` is Terminal's own AppleScript reference syntax,
        // captured verbatim from `do script`'s return value (e.g. `tab 1
        // of window id 12345`) -- not a quoted string, so it splices
        // directly into the reference position of `close (...)` as CODE,
        // deliberately unescaped (unlike iTerm2's/Ghostty's close paths
        // below, which interpolate `pane_ref` as a string literal and so
        // do run it through `applescript_escape`). That is only safe
        // because `close_owned_pane` above never calls this with an
        // arbitrary string: it looks `pane_ref` up in `owned_panes` first
        // and refuses anything not already tracked there, so only a
        // reference this backend itself captured from `do script` and
        // recorded ever reaches this format string.
        let script = format!("tell application \"Terminal\" to close ({pane_ref}) saving no");
        match self.executor.execute("osascript", &["-e", &script]) {
            Ok(CommandResult { success: true, .. }) => Ok(()),
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "osascript close exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run osascript: {e}")),
        }
    }

    fn close_pane_iterm2(&self, pane_ref: &str) -> Result<(), String> {
        // `pane_ref` is our own captured session `id`, re-found by
        // searching every window's sessions rather than reconstructing an
        // object reference from the bare id string, which this dictionary
        // does not document a syntax for.
        //
        // Unlike Ghostty's own iterate-and-match close (verified working
        // against a live install across two separate osascript
        // invocations, id captured then re-found and closed by it), this
        // one has not been run against a live iTerm2 -- it is the same
        // shape applied to iTerm2's own long-stable, well-documented
        // `windows`/`sessions`/`close` primitives, which is why the risk
        // is low despite being unverified, not because it was checked.
        let script = format!(
            "tell application \"iTerm\"\n\
             repeat with w in windows\n\
             repeat with s in sessions of w\n\
             if id of s is \"{}\" then\n\
             close s\n\
             return\n\
             end if\n\
             end repeat\n\
             end repeat\n\
             end tell",
            applescript_escape(pane_ref)
        );
        match self.executor.execute("osascript", &["-e", &script]) {
            Ok(CommandResult { success: true, .. }) => Ok(()),
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "osascript close exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run osascript: {e}")),
        }
    }

    /// Re-finds the Ghostty tab by its stored `id` (searching every
    /// window's tabs, since this dictionary has no documented syntax for
    /// reconstructing a tab object reference from the bare id string
    /// alone) and closes it with the confirmed `close tab <ref>` command.
    fn close_pane_ghostty(&self, pane_ref: &str) -> Result<(), String> {
        let script = format!(
            "tell application \"Ghostty\"\n\
             repeat with w in windows\n\
             repeat with t in tabs of w\n\
             if id of t is \"{}\" then\n\
             close tab t\n\
             return\n\
             end if\n\
             end repeat\n\
             end repeat\n\
             end tell",
            applescript_escape(pane_ref)
        );
        match self.executor.execute("osascript", &["-e", &script]) {
            Ok(CommandResult { success: true, .. }) => Ok(()),
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "osascript close exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run osascript: {e}")),
        }
    }

    fn close_pane_linux(&self, pane_ref: &str) -> Result<(), String> {
        match self.executor.execute("kill", &[pane_ref]) {
            Ok(CommandResult { success: true, .. }) => Ok(()),
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "kill exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run kill: {e}")),
        }
    }

    /// The panes this backend currently tracks as owned, for tests and
    /// diagnostics.
    #[must_use]
    pub fn owned_pane_ids(&self) -> Vec<String> {
        self.owned_panes
            .lock()
            .iter()
            .map(|(r, _)| r.clone())
            .collect()
    }
}

/// Escapes `s` for embedding inside an AppleScript double-quoted string
/// literal (backslash, then double-quote).
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Whether `s` needs shell quoting at all: anything outside a
/// conservative safe set (alnum, `-_./:=`) -- in particular whitespace
/// and quote characters, the two things `do script`'s single command
/// string would otherwise misparse as word boundaries.
fn shell_word_needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '=')))
}

/// Quotes one shell word POSIX-`sh` style when it needs it: wraps in
/// single quotes, escaping any embedded single quote as `'\''`. Plain
/// words (`crewd`, `attach`, a UUID-ish run id) pass through unquoted,
/// so the common case reads as plain argv in a captured script rather
/// than every word wrapped for no reason. `do script` runs its argument
/// through a real shell, unlike every other backend's direct argv exec,
/// so this is the one place in the display layer a command's arguments
/// must be shell-escaped rather than passed as raw argv.
fn shell_quote(s: &str) -> String {
    if shell_word_needs_quoting(s) {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}

fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

impl DisplayBackendTrait for OsWindowDisplay {
    fn backend_name(&self) -> &str {
        "osWindow"
    }

    fn is_available(&self) -> bool {
        matches!(
            self.executor.execute("which", &[self.probe_binary()]),
            Ok(CommandResult { success: true, .. })
        )
    }

    fn activate(&mut self) -> Result<(), String> {
        // Nothing to pre-activate: each pane opens its own window on
        // `create_pane`, there is no shared session to establish first.
        Ok(())
    }

    fn status(&self) -> DisplayStatus {
        DisplayStatus {
            backend: DisplayBackend::OsWindow,
            available: self.is_available(),
            active: false,
            dimensions: None,
        }
    }

    fn create_pane(&self, req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
        Box::pin(async move {
            let (pane_ref, placement) = self.create_pane_sync(&req)?;
            Ok(PaneHandle {
                backend: DisplayBackend::OsWindow,
                pane_ref,
                placement,
            })
        })
    }

    fn close_pane(&self, handle: &PaneHandle) -> DisplayFuture<'_, ()> {
        let pane_ref = handle.pane_ref.clone();
        Box::pin(async move { self.close_owned_pane(&pane_ref) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_protocol::DisplayPlacement;
    use std::collections::HashMap;
    use std::io;
    use std::sync::Mutex as StdMutex;

    #[derive(Clone)]
    enum Response {
        Execute(CommandResult),
        SpawnPid(u32),
        SpawnError(String),
    }

    /// Records every `execute`/`spawn_detached` call (as `"program
    /// arg1 arg2..."`) and answers from a fixed table keyed the same way.
    struct RecordingExecutor {
        responses: HashMap<String, Response>,
        calls: StdMutex<Vec<(String, Vec<String>)>>,
    }

    impl RecordingExecutor {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
                calls: StdMutex::new(Vec::new()),
            }
        }

        fn with(mut self, key: &str, response: Response) -> Self {
            self.responses.insert(key.to_string(), response);
            self
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, program: &str, args: &[&str]) -> String {
            let key = format!("{program} {}", args.join(" "));
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            key
        }
    }

    fn ok(stdout: &str) -> CommandResult {
        CommandResult {
            success: true,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn err_result(stderr: &str) -> CommandResult {
        CommandResult {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, program: &str, args: &[&str]) -> io::Result<CommandResult> {
            let key = self.record(program, args);
            match self.responses.get(&key) {
                Some(Response::Execute(result)) => Ok(result.clone()),
                Some(Response::SpawnError(msg)) => {
                    Err(io::Error::new(io::ErrorKind::NotFound, msg.clone()))
                }
                Some(Response::SpawnPid(_)) | None => {
                    Err(io::Error::other(format!("no fixture response for: {key}")))
                }
            }
        }

        fn spawn_detached(&self, program: &str, args: &[&str]) -> io::Result<u32> {
            let key = self.record(program, args);
            match self.responses.get(&key) {
                Some(Response::SpawnPid(pid)) => Ok(*pid),
                Some(Response::SpawnError(msg)) => {
                    Err(io::Error::new(io::ErrorKind::NotFound, msg.clone()))
                }
                Some(Response::Execute(_)) | None => {
                    Err(io::Error::other(format!("no fixture response for: {key}")))
                }
            }
        }
    }

    fn make_display(platform: Platform, executor: Arc<RecordingExecutor>) -> OsWindowDisplay {
        OsWindowDisplay {
            config: DisplayConfig::default(),
            executor,
            platform,
            owned_panes: Mutex::new(Vec::new()),
        }
    }

    fn pane_request(command: Vec<&str>) -> PaneRequest {
        PaneRequest {
            title: "crew: worker-1 (claude)".to_string(),
            command: command.into_iter().map(str::to_string).collect(),
            placement: DisplayPlacement::Tab,
            launch_program: None,
        }
    }

    #[test]
    fn backend_name_is_os_window() {
        let executor = Arc::new(RecordingExecutor::new());
        let display = make_display(Platform::MacOs, executor);
        assert_eq!(display.backend_name(), "osWindow");
    }

    #[test]
    fn is_available_probes_which_osascript_on_macos() {
        let executor = Arc::new(RecordingExecutor::new().with(
            "which osascript",
            Response::Execute(ok("/usr/bin/osascript")),
        ));
        let display = make_display(Platform::MacOs, Arc::clone(&executor));
        assert!(display.is_available());
        assert_eq!(
            executor.calls(),
            vec![("which".to_string(), vec!["osascript".to_string()])]
        );
    }

    #[test]
    fn is_available_probes_which_x_terminal_emulator_on_linux() {
        let executor = Arc::new(RecordingExecutor::new().with(
            "which x-terminal-emulator",
            Response::Execute(ok("/usr/bin/x-terminal-emulator")),
        ));
        let display = make_display(Platform::Linux, Arc::clone(&executor));
        assert!(display.is_available());
        assert_eq!(
            executor.calls(),
            vec![("which".to_string(), vec!["x-terminal-emulator".to_string()])]
        );
    }

    #[test]
    fn is_available_false_when_the_probe_binary_is_missing() {
        let executor = Arc::new(RecordingExecutor::new().with(
            "which osascript",
            Response::Execute(err_result("not found")),
        ));
        let display = make_display(Platform::MacOs, executor);
        assert!(!display.is_available());
    }

    #[test]
    fn is_available_false_on_spawn_error() {
        let executor = Arc::new(RecordingExecutor::new().with(
            "which osascript",
            Response::SpawnError("no such file".to_string()),
        ));
        let display = make_display(Platform::MacOs, executor);
        assert!(!display.is_available());
    }

    #[tokio::test]
    async fn macos_create_pane_runs_do_script_and_captures_the_returned_tab_reference() {
        let script =
            "tell application \"Terminal\"\nactivate\ndo script \"crewd attach run-1\"\nend tell";
        let executor = Arc::new(RecordingExecutor::new().with(
            &format!("osascript -e {script}"),
            Response::Execute(ok("tab 1 of window id 12345")),
        ));
        let display = make_display(Platform::MacOs, Arc::clone(&executor));

        let handle = display
            .create_pane(pane_request(vec!["crewd", "attach", "run-1"]))
            .await
            .expect("do script must succeed");

        assert_eq!(handle.backend, DisplayBackend::OsWindow);
        assert_eq!(handle.pane_ref, "tab 1 of window id 12345");
        assert_eq!(handle.placement, DisplayPlacement::Window);
        assert_eq!(
            display.owned_pane_ids(),
            vec!["tab 1 of window id 12345".to_string()]
        );
    }

    #[tokio::test]
    async fn macos_create_pane_shell_escapes_arguments_containing_spaces_and_quotes() {
        let inner = "tell application \"Terminal\"\nactivate\ndo script \"crewd attach 'run \\\"1\\\"'\"\nend tell";
        let executor = Arc::new(RecordingExecutor::new().with(
            &format!("osascript -e {inner}"),
            Response::Execute(ok("tab 1")),
        ));
        let display = make_display(Platform::MacOs, Arc::clone(&executor));

        let handle = display
            .create_pane(pane_request(vec!["crewd", "attach", "run \"1\""]))
            .await
            .expect("shell-escaped argv must still match the fixture key");
        assert_eq!(handle.pane_ref, "tab 1");
    }

    #[tokio::test]
    async fn macos_create_pane_failure_creates_no_owned_pane() {
        let executor = Arc::new(RecordingExecutor::new()); // no fixture => spawn error
        let display = make_display(Platform::MacOs, executor);

        let result = display
            .create_pane(pane_request(vec!["crewd", "attach", "run-1"]))
            .await;
        assert!(result.is_err());
        assert!(display.owned_pane_ids().is_empty());
    }

    /// Like [`pane_request`], but with `launch_program` set -- CREW-9's
    /// per-terminal dispatch tests need to force a specific hint.
    fn pane_request_with_hint(command: Vec<&str>, launch_program: HostProgramHint) -> PaneRequest {
        PaneRequest {
            launch_program: Some(launch_program),
            ..pane_request(command)
        }
    }

    #[tokio::test]
    async fn iterm2_create_pane_uses_a_real_tab_and_reports_it_honestly() {
        let script = "tell application \"iTerm\"\nactivate\ntell current window\ncreate tab with default profile\ntell current session\nwrite text \"crewd attach run-1\"\nreturn id\nend tell\nend tell\nend tell";
        let executor = Arc::new(RecordingExecutor::new().with(
            &format!("osascript -e {script}"),
            Response::Execute(ok("42")),
        ));
        let display = make_display(Platform::MacOs, Arc::clone(&executor));

        let handle = display
            .create_pane(pane_request_with_hint(
                vec!["crewd", "attach", "run-1"],
                HostProgramHint::ITerm2,
            ))
            .await
            .expect("iTerm2 tab creation must succeed");

        assert_eq!(handle.pane_ref, "42");
        // A real tab, honestly reported -- not the fallback Window.
        assert_eq!(handle.placement, DisplayPlacement::Tab);
        assert_eq!(display.owned_pane_ids(), vec!["42".to_string()]);
    }

    #[tokio::test]
    async fn iterm2_close_pane_iterates_windows_and_sessions_to_find_the_id() {
        let create_script = "tell application \"iTerm\"\nactivate\ntell current window\ncreate tab with default profile\ntell current session\nwrite text \"crewd\"\nreturn id\nend tell\nend tell\nend tell";
        let close_script = "tell application \"iTerm\"\nrepeat with w in windows\nrepeat with s in sessions of w\nif id of s is \"42\" then\nclose s\nreturn\nend if\nend repeat\nend repeat\nend tell";
        let executor = Arc::new(
            RecordingExecutor::new()
                .with(
                    &format!("osascript -e {create_script}"),
                    Response::Execute(ok("42")),
                )
                .with(
                    &format!("osascript -e {close_script}"),
                    Response::Execute(ok("")),
                ),
        );
        let display = make_display(Platform::MacOs, Arc::clone(&executor));

        let handle = display
            .create_pane(pane_request_with_hint(
                vec!["crewd"],
                HostProgramHint::ITerm2,
            ))
            .await
            .expect("create must succeed");

        display
            .close_pane(&handle)
            .await
            .expect("close must succeed");
        assert!(display.owned_pane_ids().is_empty());
    }

    #[tokio::test]
    async fn ghostty_create_pane_uses_a_real_tab_when_the_applescript_dictionary_is_present() {
        let script = "tell application \"Ghostty\"\nset t to new tab in window 1 with configuration {command:\"crewd attach run-1\"}\nreturn id of t\nend tell";
        let executor = Arc::new(RecordingExecutor::new().with(
            &format!("osascript -e {script}"),
            Response::Execute(ok("tab-9578b4000")),
        ));
        let display = make_display(Platform::MacOs, Arc::clone(&executor));

        let handle = display
            .create_pane(pane_request_with_hint(
                vec!["crewd", "attach", "run-1"],
                HostProgramHint::Ghostty,
            ))
            .await
            .expect("Ghostty tab creation must succeed");

        assert_eq!(handle.pane_ref, "tab-9578b4000");
        assert_eq!(handle.placement, DisplayPlacement::Tab);
        assert_eq!(display.owned_pane_ids(), vec!["tab-9578b4000".to_string()]);
    }

    #[tokio::test]
    async fn ghostty_close_pane_iterates_windows_and_tabs_to_find_the_id() {
        let create_script = "tell application \"Ghostty\"\nset t to new tab in window 1 with configuration {command:\"crewd\"}\nreturn id of t\nend tell";
        let close_script = "tell application \"Ghostty\"\nrepeat with w in windows\nrepeat with t in tabs of w\nif id of t is \"tab-1\" then\nclose tab t\nreturn\nend if\nend repeat\nend repeat\nend tell";
        let executor = Arc::new(
            RecordingExecutor::new()
                .with(
                    &format!("osascript -e {create_script}"),
                    Response::Execute(ok("tab-1")),
                )
                .with(
                    &format!("osascript -e {close_script}"),
                    Response::Execute(ok("")),
                ),
        );
        let display = make_display(Platform::MacOs, Arc::clone(&executor));

        let handle = display
            .create_pane(pane_request_with_hint(
                vec!["crewd"],
                HostProgramHint::Ghostty,
            ))
            .await
            .expect("create must succeed");

        display
            .close_pane(&handle)
            .await
            .expect("close must succeed");
        assert!(display.owned_pane_ids().is_empty());
    }

    #[tokio::test]
    async fn ghostty_falls_back_to_open_na_when_the_applescript_dictionary_is_absent() {
        // No fixture for the `new tab` script -- RecordingExecutor answers
        // any unregistered key with a failure, simulating a pre-1.3.0
        // Ghostty that doesn't understand the command.
        let create_script = "tell application \"Ghostty\"\nset t to new tab in window 1 with configuration {command:\"crewd\"}\nreturn id of t\nend tell";
        let executor = Arc::new(
            RecordingExecutor::new()
                .with(
                    &format!("osascript -e {create_script}"),
                    Response::Execute(err_result(
                        "Ghostty got an error: doesn't understand the new tab message",
                    )),
                )
                .with(
                    "open -na Ghostty --args -e crewd",
                    Response::Execute(ok("")),
                ),
        );
        let display = make_display(Platform::MacOs, Arc::clone(&executor));

        let handle = display
            .create_pane(pane_request_with_hint(
                vec!["crewd"],
                HostProgramHint::Ghostty,
            ))
            .await
            .expect("fallback to open -na must succeed");

        // No real reference to close later -- an honest new-window report,
        // not a real tab.
        assert_eq!(handle.placement, DisplayPlacement::Window);
        assert!(!handle.pane_ref.is_empty());

        // The fallback's pane is untracked as closeable: close_pane must
        // succeed as a documented no-op, not error.
        display
            .close_pane(&handle)
            .await
            .expect("closing an Unclosable Ghostty fallback pane is a no-op, not an error");
    }

    #[tokio::test]
    async fn linux_create_pane_spawns_detached_with_title_and_command_and_captures_the_pid() {
        let executor = Arc::new(RecordingExecutor::new().with(
            "x-terminal-emulator -T crew: worker-1 (claude) -e crewd attach run-1",
            Response::SpawnPid(4242),
        ));
        let display = make_display(Platform::Linux, Arc::clone(&executor));

        let handle = display
            .create_pane(pane_request(vec!["crewd", "attach", "run-1"]))
            .await
            .expect("spawn_detached must succeed");

        assert_eq!(handle.backend, DisplayBackend::OsWindow);
        assert_eq!(handle.pane_ref, "4242");
        assert_eq!(display.owned_pane_ids(), vec!["4242".to_string()]);

        let calls = executor.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "x-terminal-emulator");
        assert_eq!(
            calls[0].1,
            vec![
                "-T".to_string(),
                "crew: worker-1 (claude)".to_string(),
                "-e".to_string(),
                "crewd".to_string(),
                "attach".to_string(),
                "run-1".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn linux_create_pane_never_blocks_via_execute_only_spawn_detached() {
        // No `Response::Execute` fixture registered at all -- if this
        // backend ever called `execute` (the blocking path) for the
        // Linux create_pane, the call would fail with "no fixture
        // response", proving spawn_detached (not execute) is what's used.
        let executor = Arc::new(
            RecordingExecutor::new()
                .with("x-terminal-emulator -T t -e crewd", Response::SpawnPid(1)),
        );
        let display = make_display(Platform::Linux, Arc::clone(&executor));
        let handle = display
            .create_pane(PaneRequest {
                title: "t".to_string(),
                command: vec!["crewd".to_string()],
                placement: DisplayPlacement::Tab,
                launch_program: None,
            })
            .await
            .expect("spawn_detached path must succeed without touching execute");
        assert_eq!(handle.pane_ref, "1");
    }

    #[tokio::test]
    async fn closing_an_untracked_pane_is_refused_on_both_platforms() {
        let macos = make_display(Platform::MacOs, Arc::new(RecordingExecutor::new()));
        let handle = PaneHandle {
            backend: DisplayBackend::OsWindow,
            pane_ref: "not-owned".to_string(),
            placement: DisplayPlacement::Window,
        };
        let result = macos.close_pane(&handle).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not tracked as owned"));

        let linux = make_display(Platform::Linux, Arc::new(RecordingExecutor::new()));
        let result = linux.close_pane(&handle).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn macos_close_pane_splices_the_tab_reference_into_a_close_script() {
        let create_key =
            "osascript -e tell application \"Terminal\"\nactivate\ndo script \"crewd\"\nend tell";
        let close_key = "osascript -e tell application \"Terminal\" to close (tab 1 of window id 999) saving no";
        let executor = Arc::new(
            RecordingExecutor::new()
                .with(create_key, Response::Execute(ok("tab 1 of window id 999")))
                .with(close_key, Response::Execute(ok(""))),
        );
        let display = make_display(Platform::MacOs, Arc::clone(&executor));

        let handle = display
            .create_pane(PaneRequest {
                title: "t".to_string(),
                command: vec!["crewd".to_string()],
                placement: DisplayPlacement::Tab,
                launch_program: None,
            })
            .await
            .expect("create must succeed");

        display
            .close_pane(&handle)
            .await
            .expect("close must succeed");
        assert!(display.owned_pane_ids().is_empty());
    }

    #[tokio::test]
    async fn linux_close_pane_kills_the_tracked_pid() {
        let executor = Arc::new(
            RecordingExecutor::new()
                .with("x-terminal-emulator -T t -e crewd", Response::SpawnPid(777))
                .with("kill 777", Response::Execute(ok(""))),
        );
        let display = make_display(Platform::Linux, Arc::clone(&executor));

        let handle = display
            .create_pane(PaneRequest {
                title: "t".to_string(),
                command: vec!["crewd".to_string()],
                placement: DisplayPlacement::Tab,
                launch_program: None,
            })
            .await
            .expect("create must succeed");
        assert_eq!(handle.pane_ref, "777");

        display
            .close_pane(&handle)
            .await
            .expect("close must succeed");
        assert!(display.owned_pane_ids().is_empty());
        assert!(
            executor
                .calls()
                .iter()
                .any(|(p, a)| p == "kill" && a == &vec!["777".to_string()])
        );
    }
}
