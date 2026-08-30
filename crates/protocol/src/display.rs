//! Display backend contracts.
//!
//! Defines the display backend types and configuration for rendering
//! Crew output in different environments (Herdr, Tmux, Terminal).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

// Reconciled with `crates/runtime`'s config-facing
// `crew::config::crew::DisplayBackend` (WP9): that enum additionally has
// `Auto`, meaning "no forced backend", which has no concrete backend of its
// own here -- every other variant of the config enum maps to exactly one of
// these (`crate::config::protocol_display_backend` in the runtime crate
// does that mapping). `Terminal` (an always-available, capability-free
// stub) was retired in the same change.
/// Supported display backends.
///
/// `hidden` is the one always-available fallback, and it is a real,
/// deliberate "no pane" choice rather than a degraded rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DisplayBackend {
    /// Herdr terminal multiplexer backend.
    Herdr,
    /// Tmux terminal multiplexer backend.
    Tmux,
    /// A new OS-native terminal window (`osascript`/Terminal on macOS,
    /// `x-terminal-emulator` on Linux).
    OsWindow,
    /// No pane at all -- the always-available fallback. Not degraded
    /// capability like the retired `Terminal`; a deliberate, always-safe
    /// choice for headless or opted-out runs.
    Hidden,
}

impl std::fmt::Display for DisplayBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayBackend::Herdr => write!(f, "herdr"),
            DisplayBackend::Tmux => write!(f, "tmux"),
            DisplayBackend::OsWindow => write!(f, "osWindow"),
            DisplayBackend::Hidden => write!(f, "hidden"),
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
            "osWindow" => Ok(DisplayBackend::OsWindow),
            "hidden" => Ok(DisplayBackend::Hidden),
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
    // CREW-9. `OsWindowDisplay` reports this rather than echoing `Tab`.
    /// A new, separate OS-level window -- not a tab or split of the
    /// caller's own terminal at all. Reported when the target terminal has
    /// no tab-creation mechanism Crew can drive (e.g. Terminal.app, or
    /// Ghostty versions predating its AppleScript support): callers never
    /// see `tab` echoed back for a placement that was, in fact, a whole
    /// new window.
    Window,
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
            backend: DisplayBackend::Hidden,
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
    // CREW-9. Used only by `OsWindowDisplay`, to target the right terminal
    // application instead of always assuming Terminal.app. tmux/herdr are
    // self-detecting (they query whatever server already exists,
    // independent of who spawned the daemon), so this only matters once
    // resolution has fallen through to a bare terminal window.
    //
    // A closed enum rather than a bare string for a structural reason, not
    // a stylistic one: this value ends up selecting and parameterizing an
    // `osascript` invocation, and an environment variable must never be
    // interpolated into script text.
    /// The program that launched the OMP session issuing this request, from
    /// its own `$TERM_PROGRAM`. Only the `osWindow` backend reads it; every
    /// other backend ignores it entirely.
    ///
    /// Deliberately NOT "the host terminal": `$TERM_PROGRAM` names whatever
    /// process launched the session, which may be a multiplexer (`tmux`) or
    /// something else entirely, not necessarily the terminal emulator the
    /// user is sitting in. Absent and `other` (present but unrecognized)
    /// are equivalent -- both mean "no specific hint", falling back to the
    /// default.
    #[serde(default)]
    pub launch_program: Option<HostProgramHint>,
}

/// A closed set of terminal/launcher programs `OsWindowDisplay` knows how
/// to target directly, resolved from the caller's own `$TERM_PROGRAM`. See
/// [`DisplayPreference::launch_program`] for why this is an enum and not a
/// bare string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum HostProgramHint {
    /// `$TERM_PROGRAM=Apple_Terminal` -- macOS's built-in Terminal.app.
    AppleTerminal,
    /// `$TERM_PROGRAM=iTerm.app` -- iTerm2.
    #[serde(rename = "iTerm2")]
    ITerm2,
    /// `$TERM_PROGRAM=ghostty` -- Ghostty.
    Ghostty,
    /// Anything else: a multiplexer (`tmux`, `screen`), an unrecognized or
    /// future terminal, or a value this build's extension doesn't (yet)
    /// know how to map. Never constructed by the extension directly --
    /// this is serde's catch-all for whatever a caller's `$TERM_PROGRAM`
    /// happens to be, so a value this enum doesn't recognize can still
    /// deserialize instead of rejecting the whole request.
    #[serde(other)]
    Other,
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
    // CREW-9. Carried here, rather than as a separate parameter threaded
    // alongside `Option<DisplaySelection>` through `build_adapter`'s
    // already-long argument list, because this struct is already the one
    // bundle that survives that exact call chain down to
    // `PaneAttachRequest`.
    /// Echoes [`DisplayPreference::launch_program`] through resolution.
    pub launch_program: Option<HostProgramHint>,
}

// ---------------------------------------------------------------------------
// PaneReopenResult
// ---------------------------------------------------------------------------

/// Result of `pane/reopen`: the pane freshly created for a live run's
/// attach socket. `pane_ref` is empty exactly when the resolved backend
/// was `Hidden` (nothing visible to reopen onto) -- not an error, mirroring
/// the submit-time pane semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PaneReopenResult {
    #[serde(rename = "runId")]
    pub run_id: crate::ids::RunId,
    pub backend: DisplayBackend,
    #[serde(rename = "paneRef")]
    pub pane_ref: String,
}

#[cfg(test)]
mod pane_reopen_tests {
    use super::*;

    #[test]
    fn pane_reopen_result_is_camel_case() {
        let result = PaneReopenResult {
            run_id: crate::ids::RunId::new(),
            backend: DisplayBackend::Tmux,
            pane_ref: "session:0.1".to_string(),
        };
        let value = serde_json::to_value(&result).unwrap();
        assert!(value["runId"].is_string());
        assert_eq!(value["paneRef"], "session:0.1");
        let parsed: PaneReopenResult = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, result);
    }
}
