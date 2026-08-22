//! Adapts crew's strict JSON configuration ([`crew::CrewConfig`]) to the
//! field set the runtime's redaction, workspace, concurrency, retention,
//! and diagnostic consumers read.
//!
//! Before crew-v2 gap-closure WP5, this crate loaded three YAML layers
//! (org/repo/user) into a `RuntimePolicy` with a wider field set:
//! deny-by-default model/adapter allowlists, per-run/daily cost ceilings,
//! a required-capability list, and a bag of six advisory "rollout gate"
//! booleans plus a nested-worker-violation action, all subject to an
//! org-lock mechanism. That system was never actually wired up end to end
//! -- the extension passed no config-path flags -- so every deployment
//! already ran with each of those fields at its default, empty/off value.
//! crew.json's schema (spec §10, [`crew::CrewConfig`]) does not model any
//! of that surface, so this module fixes the fields with no `CrewConfig`
//! equivalent at that same default value permanently: this changes
//! nothing about production behavior (already the default everywhere),
//! but crew.json can no longer *become* a way to set them. See the WP5
//! report for the decision record if any of that enforcement needs a real
//! config surface later.
//!
//! [`NestedViolationAction`] is the one carry-over worth calling out
//! explicitly: unlike the six pure rollout-readiness booleans (deleted
//! outright -- nothing but the doctor's advisory report ever read them),
//! it is a real, load-bearing default for [`crate::policy::violation`]'s
//! nested-worker-violation handling, not a gate. It keeps its own type and
//! default (`QuarantineAndCancel`), just with no config-file path to
//! override it yet.

pub mod crew;

use std::path::Path;

pub use crew::ConfigError;

/// How to handle a mid-run nested-worker policy violation. A real runtime
/// default consumed by [`crate::policy::violation`], not a rollout gate --
/// see the module doc. `crew.json` has no field for this yet, so every
/// policy adapted from it resolves to [`NestedViolationAction::default`];
/// tests that need another value construct [`RuntimePolicy`] directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NestedViolationAction {
    /// Quarantine the nested worker (blocks all side effects, requires explicit release).
    Quarantine,
    /// Cancel the nested worker (audited adapter path).
    Cancel,
    /// Quarantine then cancel (default).
    #[default]
    QuarantineAndCancel,
}

impl std::fmt::Display for NestedViolationAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quarantine => write!(f, "quarantine"),
            Self::Cancel => write!(f, "cancel"),
            Self::QuarantineAndCancel => write!(f, "quarantineAndCancel"),
        }
    }
}

/// A thin adapter from [`crew::CrewConfig`] to the field set every existing
/// policy/redaction/workspace/doctor consumer reads. See the module doc for
/// which fields are fixed at a permanently inert default because
/// `CrewConfig` has no equivalent.
///
/// Distinct from [`crate::adapter::EffectivePolicy`], which is the
/// narrower environment-variable allowlist consumed by
/// [`crate::adapter::WorkerProfile::validate`] -- the two types describe
/// unrelated concerns despite the similar name.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimePolicy {
    /// SHA-256 fingerprint of the merged `CrewConfig` this was adapted
    /// from (see [`crew::fingerprint`]).
    pub fingerprint: String,
    pub display_backend: crew::DisplayBackend,
    pub retention: String,
    pub concurrency_ceiling: u32,
    /// Always empty: `CrewConfig` has no model allowlist. An empty list
    /// permits every model -- exactly what every current deployment
    /// already does, since no config path could ever have set this.
    pub allowed_models: Vec<String>,
    /// Always empty: `CrewConfig` has no adapter allowlist. See `allowed_models`.
    pub allowed_adapters: Vec<String>,
    /// Always empty: `CrewConfig` has no required-capability list.
    pub required_capabilities: Vec<String>,
    /// Always `None`: `CrewConfig` has no per-run cost ceiling.
    pub cost_ceiling_per_run_usd: Option<f64>,
    /// Always `None`: `CrewConfig` has no daily cost ceiling.
    pub cost_ceiling_daily_usd: Option<f64>,
    /// Always `false`: `CrewConfig` has no native-discovery acknowledgement
    /// field. Fail-closed, matching every current deployment's default --
    /// and presently inert either way, since no shipped adapter declares a
    /// nested capability above `none`.
    pub native_discovery_reviewed: bool,
    pub org_security_patterns: Vec<String>,
    pub copy_max_bytes: u64,
    pub copy_max_files: u64,
    pub nested_violation_action: NestedViolationAction,
}

impl RuntimePolicy {
    /// Builds the field-adapted [`RuntimePolicy`] view of `cfg`.
    #[must_use]
    pub fn from_crew_config(cfg: &crew::CrewConfig) -> Self {
        Self {
            fingerprint: crew::fingerprint(cfg),
            display_backend: cfg.display.backend,
            retention: cfg.retention.period.clone(),
            concurrency_ceiling: cfg.limits.max_concurrent_workers,
            allowed_models: Vec::new(),
            allowed_adapters: Vec::new(),
            required_capabilities: Vec::new(),
            cost_ceiling_per_run_usd: None,
            cost_ceiling_daily_usd: None,
            native_discovery_reviewed: false,
            org_security_patterns: cfg.security.patterns.clone(),
            copy_max_bytes: cfg
                .workspace
                .copy_max_bytes
                .unwrap_or(crate::workspace::DEFAULT_COPY_MAX_BYTES),
            copy_max_files: cfg
                .workspace
                .copy_max_files
                .unwrap_or(crate::workspace::DEFAULT_COPY_MAX_FILES),
            nested_violation_action: NestedViolationAction::default(),
        }
    }
}

/// Loads and merges the crew config layers at `paths` (lowest precedence
/// first), applies `per_run` on top if given, and adapts the result into a
/// [`RuntimePolicy`]. The single entry point `serve`, `doctor`, and a run's
/// `policyOverrides` re-merge all call.
///
/// # Errors
/// Returns [`ConfigError`] on a read/parse failure, an unknown key at any
/// depth, or a value that fails to deserialize into `CrewConfig` shape.
pub fn resolve_policy(
    paths: &[&Path],
    per_run: Option<&serde_json::Value>,
) -> Result<RuntimePolicy, ConfigError> {
    let cfg = crew::load_layers(paths, per_run)?;
    Ok(RuntimePolicy::from_crew_config(&cfg))
}

/// Maps `CrewConfig`'s display backend to the protocol's narrower enum
/// (`Herdr`/`Tmux`/`Terminal` only -- no `Auto`, `OsWindow`, or `Hidden`).
/// `Auto` means "no forced backend" everywhere it's read, so it maps to
/// `None`; `OsWindow`/`Hidden` have no protocol-side backend to force yet,
/// so they also map to `None` rather than a wrong guess.
#[must_use]
pub fn protocol_display_backend(
    backend: crew::DisplayBackend,
) -> Option<batman_protocol::DisplayBackend> {
    match backend {
        crew::DisplayBackend::Auto
        | crew::DisplayBackend::OsWindow
        | crew::DisplayBackend::Hidden => None,
        crew::DisplayBackend::Herdr => Some(batman_protocol::DisplayBackend::Herdr),
        crew::DisplayBackend::Tmux => Some(batman_protocol::DisplayBackend::Tmux),
    }
}
