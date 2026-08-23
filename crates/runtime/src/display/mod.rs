//! Display backend implementations.
//!
//! Provides concrete display backends for rendering Crew output:
//! - [`HerdrDisplay`]: Herdr terminal multiplexer backend, with real
//!   client/server protocol compatibility gating and pane-level
//!   operations (split/run/move/close/report-agent).
//! - [`TmuxDisplay`]: tmux terminal multiplexer backend, with pane-level
//!   operations via `new-window`/`split-window`.
//! - [`TerminalDisplay`]: raw terminal backend (degraded capabilities),
//!   always available as a fallback.

mod herdr;
mod terminal;
mod tmux;

pub use herdr::{HerdrDisplay, HerdrStatus};
pub use terminal::TerminalDisplay;
pub use tmux::TmuxDisplay;

use batman_protocol::{
    DisplayBackend, DisplayConfig, DisplayPreference, DisplaySelection, DisplayStatus,
};
use std::io;
use std::process::Command;

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

    /// Selects the best available backend.
    pub fn select_best(&self) -> Option<&dyn DisplayBackendTrait> {
        self.backends
            .iter()
            .find(|b| b.is_available())
            .map(|b| b.as_ref())
    }

    /// Returns a mutable reference to a backend by index.
    pub fn backend_mut(
        &mut self,
        index: usize,
    ) -> Option<&mut (dyn DisplayBackendTrait + 'static)> {
        self.backends.get_mut(index).map(move |b| b.as_mut())
    }

    /// Registers every display backend this runtime knows how to build, in
    /// descending capability order (Herdr, tmux, raw terminal). Availability
    /// is *not* probed here -- each backend answers `is_available()` when
    /// asked, so constructing a registry never spawns a process.
    #[must_use]
    pub fn with_default_backends(config: DisplayConfig) -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(herdr::HerdrDisplay::new(config.clone())));
        registry.register(Box::new(tmux::TmuxDisplay::new(config.clone())));
        registry.register(Box::new(terminal::TerminalDisplay::new(config)));
        registry
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

        DisplaySelection {
            selected,
            placement: preference.placement,
            attempts,
        }
    }
}

impl Default for DisplayRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Display selector with ordered fallback.
pub struct DisplaySelector {
    preferred: Vec<DisplayBackend>,
}

impl DisplaySelector {
    pub fn new(preferred: Vec<DisplayBackend>) -> Self {
        DisplaySelector { preferred }
    }

    /// Selects the first available backend from the preferred list.
    pub fn select<'a>(&self, registry: &'a DisplayRegistry) -> Option<&'a dyn DisplayBackendTrait> {
        for backend in &self.preferred {
            if let Some(registered) = registry
                .backends()
                .iter()
                .find(|b| b.backend_name() == backend.to_string())
                && registered.is_available()
            {
                return Some(registered.as_ref());
            }
        }
        None
    }

    /// Returns the index of the first available backend from the preferred list.
    pub fn select_index(&self, registry: &DisplayRegistry) -> Option<usize> {
        for backend in &self.preferred {
            if let Some(index) = registry
                .backends
                .iter()
                .position(|b| b.backend_name() == backend.to_string() && b.is_available())
            {
                return Some(index);
            }
        }
        None
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
    use batman_protocol::{DisplayBackend, DisplayConfig, DisplayStatus};

    /// Fake backend for testing.
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
            DisplayStatus::new(DisplayBackend::Terminal, self.available, self.available)
        }
    }

    #[test]
    fn test_display_backend_traits() {
        let herdr = HerdrDisplay::new(DisplayConfig::default());
        assert_eq!(herdr.backend_name(), "herdr");

        let tmux = TmuxDisplay::new(DisplayConfig::default());
        assert_eq!(tmux.backend_name(), "tmux");

        let terminal = TerminalDisplay::new(DisplayConfig::default());
        assert_eq!(terminal.backend_name(), "terminal");
    }

    #[test]
    fn test_terminal_always_available() {
        let terminal = TerminalDisplay::new(DisplayConfig::default());
        assert!(terminal.is_available());
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
        assert!(registry.select_best().is_some());
        assert_eq!(registry.select_best().unwrap().backend_name(), "fake1");
    }

    #[test]
    fn test_display_selector_ordered_fallback() {
        let mut registry = DisplayRegistry::new();
        // Register in reverse order: terminal, herdr, tmux
        registry.register(Box::new(FakeBackend {
            name: "terminal".to_string(),
            available: true,
            activate_result: Ok(()),
        }));
        registry.register(Box::new(FakeBackend {
            name: "herdr".to_string(),
            available: false, // herdr not available
            activate_result: Err("not available".to_string()),
        }));
        registry.register(Box::new(FakeBackend {
            name: "tmux".to_string(),
            available: true,
            activate_result: Ok(()),
        }));

        // Selector prefers tmux first, then herdr, then terminal
        let selector = DisplaySelector::new(vec![
            DisplayBackend::Tmux,
            DisplayBackend::Herdr,
            DisplayBackend::Terminal,
        ]);

        // Should select tmux (first in preferred list that's available)
        let selected = selector.select(&registry);
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().backend_name(), "tmux");
    }

    #[test]
    fn test_display_selector_fallback_to_terminal() {
        let mut registry = DisplayRegistry::new();
        // Register only terminal (herdr/tmux not available)
        registry.register(Box::new(FakeBackend {
            name: "terminal".to_string(),
            available: true,
            activate_result: Ok(()),
        }));

        let selector = DisplaySelector::new(vec![
            DisplayBackend::Tmux,
            DisplayBackend::Herdr,
            DisplayBackend::Terminal,
        ]);

        // Should fall back to terminal and activate it
        let selected_index = selector.select_index(&registry);
        assert!(selected_index.is_some());
        let idx = selected_index.unwrap();
        let result = registry.backend_mut(idx).unwrap().activate();
        assert!(result.is_ok());
    }

    #[test]
    fn test_display_selector_no_backend_available() {
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(FakeBackend {
            name: "fake".to_string(),
            available: false,
            activate_result: Err("not available".to_string()),
        }));

        let selector = DisplaySelector::new(vec![
            DisplayBackend::Tmux,
            DisplayBackend::Herdr,
            DisplayBackend::Terminal,
        ]);

        // Should return None when no backend is available
        let selected = selector.select(&registry);
        assert!(selected.is_none());
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
