//! Display backend contracts.
//!
//! Defines the display backend types and configuration for rendering
//! Crew output in different environments (Herdr, Tmux, Terminal).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Supported display backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DisplayBackend {
    /// Herdr terminal multiplexer backend.
    Herdr,
    /// Tmux terminal multiplexer backend.
    Tmux,
    /// Raw terminal backend (degraded capabilities).
    Terminal,
}

impl std::fmt::Display for DisplayBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayBackend::Herdr => write!(f, "herdr"),
            DisplayBackend::Tmux => write!(f, "tmux"),
            DisplayBackend::Terminal => write!(f, "terminal"),
        }
    }
}

/// Parses a backend's wire name -- the exact string
/// [`DisplayBackend`]'s `Display` impl produces, and the same one
/// `DisplayBackendTrait::backend_name` returns.
impl std::str::FromStr for DisplayBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "herdr" => Ok(DisplayBackend::Herdr),
            "tmux" => Ok(DisplayBackend::Tmux),
            "terminal" => Ok(DisplayBackend::Terminal),
            other => Err(format!("unknown display backend '{other}'")),
        }
    }
}

/// Where a display backend places a pane relative to the caller's own
/// terminal. Changes presentation only; never run ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DisplayPlacement {
    /// Rendered inside the caller's own OMP session, no separate pane.
    Embedded,
    /// A new pane split to the right of the current one.
    SplitRight,
    /// A new pane split below the current one.
    SplitDown,
    /// A new tab.
    Tab,
    /// A new workspace (Herdr only; unsupported by tmux).
    Workspace,
}

/// Display configuration.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct DisplayConfig {
    /// The backend to use.
    pub backend: DisplayBackend,
    /// Optional width override (None = auto-detect).
    pub width: Option<u16>,
    /// Optional height override (None = auto-detect).
    pub height: Option<u16>,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        DisplayConfig {
            backend: DisplayBackend::Terminal,
            width: None,
            height: None,
        }
    }
}

/// Display status information.
#[derive(Debug, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct DisplayStatus {
    /// The backend in use.
    pub backend: DisplayBackend,
    /// Whether the backend is available.
    pub available: bool,
    /// Whether the backend is currently active.
    pub active: bool,
    /// Terminal dimensions if known.
    pub dimensions: Option<(u16, u16)>,
}

impl DisplayStatus {
    pub fn new(backend: DisplayBackend, available: bool, active: bool) -> Self {
        DisplayStatus {
            backend,
            available,
            active,
            dimensions: None,
        }
    }
}

/// A caller's ordered display-backend preference, resolved against what is
/// actually available on the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct DisplayPreference {
    /// Backends to try, most-preferred first. Empty means "any available".
    pub ordered: Vec<DisplayBackend>,
    /// Where to put the pane once a backend is chosen.
    pub placement: DisplayPlacement,
}

/// The outcome of resolving a [`DisplayPreference`] against the live registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct DisplaySelection {
    /// The backend that was attached, or `None` when every candidate was
    /// unavailable (headless CI). `None` is not an error.
    pub selected: Option<DisplayBackend>,
    pub placement: DisplayPlacement,
    /// Every backend tried, in order, so an operator can see why the
    /// preferred one lost.
    pub attempts: Vec<DisplayBackend>,
}
