//! Raw terminal display backend (degraded capabilities).

use crew_protocol::{DisplayBackend, DisplayConfig, DisplayStatus};

use super::DisplayBackendTrait;

/// Raw terminal display backend (degraded capabilities).
///
/// Always available as a fallback. Does not require external tools.
#[allow(dead_code)]
pub struct TerminalDisplay {
    config: DisplayConfig,
}

impl TerminalDisplay {
    pub fn new(config: DisplayConfig) -> Self {
        TerminalDisplay { config }
    }

    /// Returns terminal dimensions if detectable.
    pub fn detect_dimensions() -> Option<(u16, u16)> {
        // Try to get terminal dimensions via environment or system calls
        // This is a simplified version - real implementation would use libc or termion
        None
    }
}

impl DisplayBackendTrait for TerminalDisplay {
    fn backend_name(&self) -> &str {
        "terminal"
    }

    fn is_available(&self) -> bool {
        // Terminal is always available as a fallback
        true
    }

    fn activate(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn status(&self) -> DisplayStatus {
        let dimensions = Self::detect_dimensions();
        DisplayStatus {
            backend: DisplayBackend::Terminal,
            available: true,
            active: true,
            dimensions,
        }
    }
}
