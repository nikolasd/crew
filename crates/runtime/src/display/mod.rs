//! Display backend implementations.
//!
//! Provides concrete display backends for rendering Crew output:
//! - [`HerdrDisplay`]: Herdr terminal multiplexer backend, with real
//!   client/server protocol compatibility gating and pane-level
//!   operations (split/run/move/close/report-agent).
//! - [`TmuxDisplay`]: tmux terminal multiplexer backend, with pane-level
//!   operations via `new-window`/`split-window`.
//! - [`OsWindowDisplay`]: a new OS-native terminal window (`osascript`
//!   Terminal on macOS, `x-terminal-emulator` on Linux).
//! - [`HiddenDisplay`]: no pane at all -- the always-available fallback.
//!   Replaces the retired `TerminalDisplay` stub: unlike that always-
//!   available-but-inert backend, `Hidden` is a real, deliberate choice
//!   whose `create_pane` is a documented no-op, not a degraded terminal
//!   rendering.
//!
//! [`PaneCoordinator`] (in [`coordinator`]) is the higher-level piece
//! that resolves one of these per run and journals its
//! attach/detach -- not yet wired into any production call site (see its
//! module doc).

pub mod attach;
pub mod coordinator;
mod herdr;
mod hidden;
mod os_window;
pub mod pane_socket;
mod tmux;

pub use attach::{AttachError, AttachServer, AttachTarget, PumpOutcome};
pub use coordinator::{PaneAttachOutcome, PaneAttachRequest, PaneCoordinator};
pub use herdr::{HerdrDisplay, HerdrStatus};
pub use hidden::HiddenDisplay;
pub use os_window::OsWindowDisplay;
pub use tmux::TmuxDisplay;

use crew_protocol::{
    DisplayBackend, DisplayConfig, DisplayPlacement, DisplayPreference, DisplaySelection,
    DisplayStatus,
};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::Command;

/// A future returned by a [`DisplayBackendTrait`] pane operation.
/// Mirrors [`crate::adapter::AdapterFuture`]'s shape: every backend's
/// pane work today is a quick, synchronous process spawn wrapped in an
/// already-ready `async move` block, but the trait is async so a future
/// backend (or a real socket-based one) never has to fake synchrony.
pub type DisplayFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// What [`DisplayBackendTrait::create_pane`] needs to open a Crew-owned
/// pane: a human-readable title (`crew: <worker-id> (<adapter>)`, built
/// by [`PaneCoordinator`]), the argv to run inside it (`crewd attach
/// <run-id> ...`), and where to place it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRequest {
    pub title: String,
    pub command: Vec<String>,
    pub placement: DisplayPlacement,
    /// The submitting caller's own `$TERM_PROGRAM` hint (CREW-9). Read
    /// only by [`OsWindowDisplay`](crate::display::os_window::OsWindowDisplay)
    /// to target the right terminal application; every other backend
    /// ignores it.
    pub launch_program: Option<crew_protocol::HostProgramHint>,
}

/// A pane [`DisplayBackendTrait::create_pane`] created: which backend
/// owns it, the backend's own reference to it (a tmux/Herdr pane id;
/// empty for [`HiddenDisplay`], which owns nothing), and where it was
/// *actually* placed. The only thing [`DisplayBackendTrait::close_pane`]
/// needs to close it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneHandle {
    pub backend: DisplayBackend,
    pub pane_ref: String,
    /// What actually happened, which is not always what was requested
    /// (CREW-9): `OsWindowDisplay` reports `DisplayPlacement::Window` for
    /// a plain new window (Terminal.app, or Ghostty predating its
    /// AppleScript tab support) even when a caller asked for `Tab` --
    /// callers must never be told `Tab` happened when it did not. Every
    /// backend that genuinely honors the requested placement (tmux,
    /// Herdr) echoes it back unchanged.
    pub placement: DisplayPlacement,
}

/// Result of a command execution — platform-independent, fixture-friendly.
#[derive(Debug, Clone)]
pub struct CommandResult {
    /// Whether the process exited successfully.
    pub success: bool,
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
}

/// Abstracts process execution for display backends.
///
/// Real executor uses `std::process::Command`; test executors return
/// preconfigured `CommandResult` values so tests never spawn real processes.
pub trait CommandExecutor: Send + Sync {
    /// Execute `program` with `args`, returning a platform-independent result.
    fn execute(&self, program: &str, args: &[&str]) -> io::Result<CommandResult>;

    /// Spawns `program` with `args` and returns immediately without
    /// waiting for it to exit, yielding its pid.
    ///
    /// The default implementation delegates to [`Self::execute`]
    /// (blocking) and reports a placeholder pid of `0` -- correct enough
    /// for a fake/fixture executor, which never actually blocks.
    /// [`RealCommandExecutor`] overrides this with a genuine
    /// non-blocking spawn: [`crate::display::OsWindowDisplay`]'s Linux
    /// path launches a long-running GUI terminal process that must never
    /// block pane creation on the caller closing that window.
    fn spawn_detached(&self, program: &str, args: &[&str]) -> io::Result<u32> {
        self.execute(program, args).map(|_| 0)
    }
}

/// Real process executor wrapping `std::process::Command`.
pub struct RealCommandExecutor;

impl RealCommandExecutor {
    pub fn new() -> Self {
        RealCommandExecutor
    }
}

impl Default for RealCommandExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandExecutor for RealCommandExecutor {
    fn execute(&self, program: &str, args: &[&str]) -> io::Result<CommandResult> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandResult {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn spawn_detached(&self, program: &str, args: &[&str]) -> io::Result<u32> {
        // Deliberately never `.wait()`s: the spawned process (an OS
        // terminal window) is meant to outlive this call by design.
        // Never reaped, so it becomes a zombie once it exits until this
        // `crewd` process itself exits and `init` reaps it -- an
        // accepted, documented tradeoff of a fire-and-forget GUI launch,
        // not a leak of any live resource.
        let child = Command::new(program).args(args).spawn()?;
        Ok(child.id())
    }
}

/// Trait for display backends.
pub trait DisplayBackendTrait: Send + Sync {
    /// Returns the backend name.
    fn backend_name(&self) -> &str;

    /// Checks if the backend is available and compatible.
    fn is_available(&self) -> bool;

    /// Activates the backend (spawns session, attaches, etc.).
    fn activate(&mut self) -> Result<(), String>;

    /// Returns the current status.
    fn status(&self) -> DisplayStatus;

    /// Returns the backend's version if known.
    fn version(&self) -> Option<String> {
        None
    }

    /// Creates a Crew-owned pane running `req.command`, or errors
    /// without creating anything. Reachable through `Box<dyn
    /// DisplayBackendTrait>` -- before WP9 this was an inherent method
    /// per concrete backend, unreachable through the trait object the
    /// registry actually holds.
    fn create_pane(&self, req: PaneRequest) -> DisplayFuture<'_, PaneHandle>;

    /// This backend's own placement when a caller's [`DisplayPreference`]
    /// doesn't specify one (CREW-52, D27/D3). Replaces the deleted
    /// `DisplayPlacement::Embedded` default: rather than a server-hardcoded
    /// value every backend was equally (mis)fit for, each backend now
    /// answers with the placement it would naturally create -- the
    /// knowledge stays where the constraint lives, and the default can't
    /// be falsified by a stale caller. The default impl returns
    /// `SplitRight`, correct for both multiplexer backends (herdr, tmux);
    /// override where that's wrong (`OsWindowDisplay`, which computes its
    /// own actual `Tab`/`Window` after the fact regardless -- see its own
    /// `create_pane`).
    fn natural_placement(&self) -> DisplayPlacement {
        DisplayPlacement::SplitRight
    }

    /// Closes a pane this backend created (`handle.pane_ref`, exactly as
    /// returned by [`Self::create_pane`]). Implementations refuse to
    /// close a `pane_ref` they never created.
    fn close_pane(&self, handle: &PaneHandle) -> DisplayFuture<'_, ()>;
}

/// Display registry that manages available backends.
pub struct DisplayRegistry {
    backends: Vec<Box<dyn DisplayBackendTrait>>,
}

impl DisplayRegistry {
    pub fn new() -> Self {
        DisplayRegistry {
            backends: Vec::new(),
        }
    }

    /// Registers a display backend.
    pub fn register(&mut self, backend: Box<dyn DisplayBackendTrait>) {
        self.backends.push(backend);
    }

    /// Returns all registered backends.
    pub fn backends(&self) -> &[Box<dyn DisplayBackendTrait>] {
        &self.backends
    }

    /// Registers every display backend this runtime knows how to build, in
    /// descending capability order (Herdr, tmux, OS window, hidden).
    /// Availability is *not* probed here -- each backend answers
    /// `is_available()` when asked, so constructing a registry never
    /// spawns a process. `Hidden` is always available, so `resolve()`
    /// against a registry built this way never returns `selected: None`.
    #[must_use]
    pub fn with_default_backends(config: DisplayConfig) -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(herdr::HerdrDisplay::new(config.clone())));
        registry.register(Box::new(tmux::TmuxDisplay::new(config.clone())));
        registry.register(Box::new(os_window::OsWindowDisplay::new(config.clone())));
        registry.register(Box::new(hidden::HiddenDisplay::new(config)));
        registry
    }

    /// Looks up a registered backend by its [`DisplayBackend`] enum
    /// value (matching on `backend_name()`, the same string
    /// [`Self::resolve`] parses candidates against).
    #[must_use]
    pub fn find(&self, backend: DisplayBackend) -> Option<&dyn DisplayBackendTrait> {
        self.backends
            .iter()
            .find(|b| b.backend_name() == backend.to_string())
            .map(|b| b.as_ref())
    }

    /// Resolves a caller's ordered [`DisplayPreference`] against what is
    /// actually available on this machine.
    ///
    /// `attempts` records every backend tried, in order, so an operator can
    /// see why the preferred one lost. An empty `ordered` list means "any
    /// available", which falls back to registration order. A `selected` of
    /// `None` means every candidate was unavailable (headless CI) and is
    /// never an error -- runs proceed without a pane.
    #[must_use]
    pub fn resolve(&self, preference: &DisplayPreference) -> DisplaySelection {
        let mut attempts = Vec::new();
        let mut selected = None;

        let candidates: Vec<DisplayBackend> = if preference.ordered.is_empty() {
            self.backends
                .iter()
                .filter_map(|b| b.backend_name().parse().ok())
                .collect()
        } else {
            preference.ordered.clone()
        };

        for candidate in candidates {
            attempts.push(candidate);
            let available = self
                .backends
                .iter()
                .any(|b| b.backend_name() == candidate.to_string() && b.is_available());
            if available {
                selected = Some(candidate);
                break;
            }
        }

        // CREW-52 (D27/D3): an explicit caller placement is honored as-is;
        // an absent one resolves to the SELECTED backend's own natural
        // form. `selected: None` (headless, every candidate unavailable)
        // has no pane to place at all, so the placement value is moot --
        // `SplitRight` is as good as anything else there.
        let placement = preference.placement.unwrap_or_else(|| {
            selected
                .and_then(|backend| self.find(backend))
                .map(|b| b.natural_placement())
                .unwrap_or(DisplayPlacement::SplitRight)
        });

        DisplaySelection {
            selected,
            placement,
            attempts,
            launch_program: preference.launch_program,
        }
    }
}

impl Default for DisplayRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Simple version comparison: returns true if `current >= minimum`.
/// Shared by any backend that still gates on a minimum semver-ish
/// string (currently only tmux; Herdr gates on exact protocol equality
/// instead -- see [`HerdrDisplay::probe`]).
#[must_use]
pub(crate) fn version_gte(current: &str, minimum: &str) -> bool {
    let parse_version =
        |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse::<u32>().ok()).collect() };

    let current_parts = parse_version(current);
    let min_parts = parse_version(minimum);

    for i in 0..3 {
        let c = current_parts.get(i).copied().unwrap_or(0);
        let m = min_parts.get(i).copied().unwrap_or(0);
        if c > m {
            return true;
        }
        if c < m {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_protocol::{DisplayBackend, DisplayConfig, DisplayStatus};

    /// Fake backend for testing. Its pane operations are never exercised
    /// by these registry/selector-focused tests (see `coordinator`'s own
    /// tests for pane-flow coverage), so both always error.
    struct FakeBackend {
        name: String,
        available: bool,
        activate_result: Result<(), String>,
    }

    impl DisplayBackendTrait for FakeBackend {
        fn backend_name(&self) -> &str {
            &self.name
        }

        fn is_available(&self) -> bool {
            self.available
        }

        fn activate(&mut self) -> Result<(), String> {
            self.activate_result.clone()
        }

        fn status(&self) -> DisplayStatus {
            DisplayStatus::new(DisplayBackend::Hidden, self.available, self.available)
        }

        fn create_pane(&self, _req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
            Box::pin(async { Err("FakeBackend has no pane support".to_string()) })
        }

        fn close_pane(&self, _handle: &PaneHandle) -> DisplayFuture<'_, ()> {
            Box::pin(async { Err("FakeBackend has no pane support".to_string()) })
        }
    }

    #[test]
    fn test_display_backend_traits() {
        let herdr = HerdrDisplay::new(DisplayConfig::default());
        assert_eq!(herdr.backend_name(), "herdr");

        let tmux = TmuxDisplay::new(DisplayConfig::default());
        assert_eq!(tmux.backend_name(), "tmux");

        let hidden = HiddenDisplay::new(DisplayConfig::default());
        assert_eq!(hidden.backend_name(), "hidden");

        let os_window = OsWindowDisplay::new(DisplayConfig::default());
        assert_eq!(os_window.backend_name(), "osWindow");
    }

    #[test]
    fn test_hidden_always_available() {
        let hidden = HiddenDisplay::new(DisplayConfig::default());
        assert!(hidden.is_available());
    }

    #[test]
    fn test_version_comparison() {
        assert!(version_gte("0.1.0", "0.1.0"));
        assert!(version_gte("0.2.0", "0.1.0"));
        assert!(!version_gte("0.0.9", "0.1.0"));
        assert!(version_gte("1.0.0", "0.1.0"));
    }

    #[test]
    fn test_display_registry() {
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(FakeBackend {
            name: "fake1".to_string(),
            available: true,
            activate_result: Ok(()),
        }));
        registry.register(Box::new(FakeBackend {
            name: "fake2".to_string(),
            available: false,
            activate_result: Err("not available".to_string()),
        }));

        assert_eq!(registry.backends().len(), 2);
    }

    #[test]
    fn test_activate_failure_handling() {
        let mut backend = FakeBackend {
            name: "failing".to_string(),
            available: true,
            activate_result: Err("activation failed".to_string()),
        };

        assert!(backend.activate().is_err());
    }
}
