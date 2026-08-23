//! OS-native terminal window display backend: opens a new terminal
//! window running the pane's command directly through the desktop's own
//! terminal application, rather than a multiplexer.
//!
//! macOS: `osascript -e 'tell application "Terminal" to do script "..."'`.
//! `do script` messages the already-running Terminal.app and returns
//! immediately (it does not wait for the launched command to finish), so
//! this runs through the ordinary blocking [`CommandExecutor::execute`]
//! seam like every other backend. Its own return value -- printed to
//! stdout by `osascript` -- is a live AppleScript reference to the new
//! tab (e.g. `tab 1 of window id 12345`), captured verbatim as the pane
//! ref and later spliced back into a `close (...)` script to close that
//! exact tab.
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

use crew_protocol::{DisplayBackend, DisplayConfig, DisplayStatus};

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

/// OS-native terminal window display backend.
pub struct OsWindowDisplay {
    #[allow(dead_code)] // carried for parity with the other backends; no field of it is read yet
    config: DisplayConfig,
    executor: Arc<dyn CommandExecutor>,
    platform: Platform,
    owned_panes: Mutex<Vec<String>>,
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

    fn create_pane_sync(&self, req: &PaneRequest) -> Result<String, String> {
        match self.platform {
            Platform::MacOs => self.create_pane_macos(req),
            Platform::Linux => self.create_pane_linux(req),
        }
    }

    fn create_pane_macos(&self, req: &PaneRequest) -> Result<String, String> {
        let script = format!(
            "tell application \"Terminal\" to do script \"{}\"",
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
                self.owned_panes.lock().push(pane_ref.clone());
                Ok(pane_ref)
            }
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "osascript exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run osascript: {e}")),
        }
    }

    fn create_pane_linux(&self, req: &PaneRequest) -> Result<String, String> {
        let mut argv: Vec<String> = vec!["-T".to_string(), req.title.clone(), "-e".to_string()];
        argv.extend(req.command.iter().cloned());
        let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();

        match self
            .executor
            .spawn_detached("x-terminal-emulator", &argv_refs)
        {
            Ok(pid) => {
                let pane_ref = pid.to_string();
                self.owned_panes.lock().push(pane_ref.clone());
                Ok(pane_ref)
            }
            Err(e) => Err(format!("failed to spawn x-terminal-emulator: {e}")),
        }
    }

    fn close_owned_pane(&self, pane_ref: &str) -> Result<(), String> {
        {
            let mut owned = self.owned_panes.lock();
            let Some(index) = owned.iter().position(|p| p == pane_ref) else {
                return Err(format!(
                    "refusing to close pane {pane_ref}: not tracked as owned by this backend"
                ));
            };
            owned.remove(index);
        }
        match self.platform {
            Platform::MacOs => self.close_pane_macos(pane_ref),
            Platform::Linux => self.close_pane_linux(pane_ref),
        }
    }

    fn close_pane_macos(&self, pane_ref: &str) -> Result<(), String> {
        // `pane_ref` is Terminal's own AppleScript reference syntax,
        // captured verbatim from `do script`'s return value (e.g. `tab 1
        // of window id 12345`) -- not a quoted string, so it splices
        // directly into the reference position of `close (...)`.
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
        self.owned_panes.lock().clone()
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
            let pane_ref = self.create_pane_sync(&req)?;
            Ok(PaneHandle {
                backend: DisplayBackend::OsWindow,
                pane_ref,
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
        let script = "tell application \"Terminal\" to do script \"crewd attach run-1\"";
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
        assert_eq!(
            display.owned_pane_ids(),
            vec!["tab 1 of window id 12345".to_string()]
        );
    }

    #[tokio::test]
    async fn macos_create_pane_shell_escapes_arguments_containing_spaces_and_quotes() {
        let inner = "tell application \"Terminal\" to do script \"crewd attach 'run \\\"1\\\"'\"";
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
        let create_key = "osascript -e tell application \"Terminal\" to do script \"crewd\"";
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
