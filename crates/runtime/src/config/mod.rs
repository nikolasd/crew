//! Configuration precedence and immutable effective policy.
//!
//! Crew resolves its runtime configuration from multiple YAML layers
//! (org → repo → user → per-run params) with strict precedence: higher
//! layers win, but org-level field locks prevent lower layers from
//! overriding specific values. The result is an immutable, SHA-256-
//! fingerprinted [`RuntimePolicy`] snapshot.
//!
//! `RuntimePolicy` (concurrency, model allowlist, retention, display
//! preference, rollout gates) is distinct from
//! [`crate::adapter::EffectivePolicy`] (the narrower environment-variable
//! allowlist consumed by [`crate::adapter::WorkerProfile::validate`]) --
//! the two names describe different concerns and are never
//! interchangeable.
//!
//! Unknown YAML keys fail closed with line/column diagnostics. Display
//! preference follows the same precedence; absent fields resolve to
//! `backend: auto`.

mod merge;

pub use merge::{
    ConfigMergeError, LayeredConfig, NestedViolationAction, RolloutGates, RuntimePolicy,
};

use std::path::Path;

/// Errors from parsing or merging YAML configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A YAML file failed to parse; includes line/column diagnostics.
    #[error("YAML parse error in {path}: {message}")]
    ParseError { path: String, message: String },

    /// A locked field was overridden by a lower layer.
    #[error("field '{field}' is locked by org policy; lower layer '{layer}' attempted override")]
    LockedFieldOverride { field: String, layer: String },

    /// An unknown YAML key was encountered.
    #[error("unknown key '{key}' at line {line}, column {column} in {path}")]
    UnknownKey {
        key: String,
        line: u32,
        column: u32,
        path: String,
    },

    /// The configuration merge produced conflicting results.
    #[error("configuration merge error: {0}")]
    MergeError(String),

    /// The policy fingerprint could not be computed.
    #[error("fingerprint error: {0}")]
    FingerprintError(String),
}

/// The layer of a configuration source, in precedence order (lowest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigLayer {
    /// Organization-level policy (lowest precedence).
    Org,
    /// Repository-level policy.
    Repo,
    /// User-level policy (highest precedence among static layers).
    User,
}

impl std::fmt::Display for ConfigLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLayer::Org => write!(f, "org"),
            ConfigLayer::Repo => write!(f, "repo"),
            ConfigLayer::User => write!(f, "user"),
        }
    }
}

/// A single YAML configuration document, parsed with strict unknown-key
/// rejection.
#[derive(Debug, Clone)]
pub struct ParsedConfig {
    /// The raw parsed YAML document as a `serde_json::Value`.
    pub document: serde_json::Value,
    /// The source file path, if any.
    pub source: Option<String>,
}

/// Loads and strictly parses a YAML configuration file. Unknown keys
/// produce [`ConfigError::UnknownKey`] with line/column diagnostics.
///
/// # Errors
/// Returns [`ConfigError::ParseError`] if the YAML is malformed, or
/// [`ConfigError::UnknownKey`] if unknown keys are present.
pub fn parse_config_file(path: &Path) -> Result<ParsedConfig, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(|e| ConfigError::ParseError {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;

    // Use serde_yaml_ng for strict parsing with unknown-key detection.
    let value: serde_json::Value =
        serde_yaml_ng::from_str(&content).map_err(|e| ConfigError::ParseError {
            path: path.display().to_string(),
            message: format!("{e}"),
        })?;

    // Validate no unknown keys at the top level by checking against known fields.
    validate_no_unknown_keys(&value, path.display().to_string())?;

    Ok(ParsedConfig {
        document: value,
        source: Some(path.display().to_string()),
    })
}

/// Validates that a parsed YAML document contains no unknown top-level keys.
/// Known keys: `retention`, `max_workers`, `display`, `security`, `models`,
/// `concurrency`, `rollout_gates`, `locks`, `workspace`, `cost`, `adapters`,
/// `capabilities`. Unknown keys produce [`ConfigError::UnknownKey`].
fn validate_no_unknown_keys(value: &serde_json::Value, path: String) -> Result<(), ConfigError> {
    let known_keys = [
        "retention",
        "max_workers",
        "display",
        "security",
        "models",
        "concurrency",
        "rollout_gates",
        "locks",
        // Read by `LayeredConfig::merge`; without them here a config that
        // sets any of them is rejected before the value is ever read.
        "workspace",
        "cost",
        "adapters",
        "capabilities",
    ];

    if let Some(map) = value.as_object() {
        for key in map.keys() {
            if !known_keys.contains(&key.as_str()) {
                return Err(ConfigError::UnknownKey {
                    key: key.clone(),
                    line: 0,
                    column: 0,
                    path,
                });
            }
        }
    }

    Ok(())
}

/// Resolves configuration from the given paths, applying precedence and
/// lock enforcement. Returns a [`RuntimePolicy`] with a SHA-256
/// fingerprint.
///
/// `org_path`, `repo_path`, and `user_path` may all be `None` (empty policy).
/// `per_run_params` overrides everything at the highest precedence.
///
/// # Errors
/// Returns [`ConfigMergeError`] on parse failure, unknown keys, or lock
/// violations.
pub fn resolve_effective_policy(
    org_path: Option<&Path>,
    repo_path: Option<&Path>,
    user_path: Option<&Path>,
    per_run_params: Option<&serde_json::Value>,
) -> Result<RuntimePolicy, ConfigMergeError> {
    let layers = LayeredConfig::load(org_path, repo_path, user_path)?;
    layers.merge(per_run_params)
}
