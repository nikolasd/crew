//! Crew's strict JSON configuration (spec §10).
//!
//! Replaces the YAML layering in [`super::merge`] for the crew-v2 surface
//! (that module stays in place until the removal work package retires it).
//! Layers merge in order: built-in defaults → each path in
//! [`load_layers`]'s `paths` (lowest to highest precedence, e.g. user file
//! then project file) → an optional per-run override document. Later
//! layers win field-by-field via a recursive deep merge over
//! `serde_json::Value`, with one exception: `security.patterns` is
//! **additive** across layers (concatenated, never replaced), so a lower
//! layer's redaction patterns can never be silently dropped by a higher
//! one.
//!
//! Every struct in this module derives `deny_unknown_fields`, but that
//! alone only reports serde's own unknown-field error, which has no
//! notion of a full JSON path once nested a few levels (particularly
//! inside the `adapters` map, whose keys are unconstrained vendor names).
//! [`load_layers`] therefore walks the merged value against the known
//! schema shape *before* deserializing, so an unknown key at any depth
//! fails closed with the exact JSON path that named it
//! (`"adapters.claude.notAField"`, `"limits.bogusField"`, ...).
//!
//! Controller override (crew-v2 gap-closure WP4, ledgered): every
//! adapter's `mode` defaults to `headless`, not the `tui` shown in the
//! spec's example -- no TUI adapter existed yet. Later work packages flip
//! each vendor's default to `tui` as its TUI adapter lands. WP13 landed
//! Claude's, WP27 Codex's, and WP28 Copilot's and OMP's (all four
//! `TuiVendor` impls pass fixture-mode conformance), so every built-in
//! adapter defaults to `tui` here -- the spec §10 end state.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Errors from loading or merging crew's JSON configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A layer file could not be read (permissions, etc. -- a missing
    /// file is not an error, see [`load_layers`]).
    #[error("failed to read crew config file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A layer file's contents were not valid JSON.
    #[error("invalid JSON in crew config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    /// An unknown key was found anywhere in the merged configuration.
    #[error("unknown key '{path}' in crew config")]
    UnknownKey { path: String },

    /// The merged, shape-validated document still failed to deserialize
    /// into [`CrewConfig`] (a value's type didn't match its field, e.g. a
    /// string where a number was expected).
    #[error("crew config failed to deserialize: {0}")]
    Deserialize(String),
}

/// When the leader is allowed to act without a human approval gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalMode {
    Always,
    Never,
    Auto,
}

/// Concurrency and time ceilings for a crew run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Limits {
    pub max_concurrent_workers: u32,
    pub inactivity_timeout_sec: u64,
    pub total_timeout_sec: u64,
    pub turn_budget_per_subtask: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_concurrent_workers: 4,
            inactivity_timeout_sec: 300,
            total_timeout_sec: 1800,
            turn_budget_per_subtask: 10,
        }
    }
}

/// Which display surface hosts worker panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DisplayBackend {
    Auto,
    Herdr,
    Tmux,
    OsWindow,
    Hidden,
}

/// When a worker's pane closes relative to its own completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CloseOnExit {
    Never,
    OnSuccess,
    Always,
}

/// Display surface preferences.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DisplayConfig {
    pub backend: DisplayBackend,
    pub close_on_exit: CloseOnExit,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            backend: DisplayBackend::Auto,
            close_on_exit: CloseOnExit::OnSuccess,
        }
    }
}

/// Whether a vendor adapter runs its worker attached to a real TUI pane
/// or drives a headless protocol adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AdapterMode {
    Tui,
    Headless,
}

/// Abstract permission posture, mapped to each vendor's own flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Max,
    Default,
    Readonly,
}

/// Per-adapter configuration. The outer map key is the adapter/vendor
/// name and is unconstrained (new vendors need no schema change); each
/// value's own shape is strict.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AdapterConfig {
    pub enabled: bool,
    pub bin: String,
    pub mode: AdapterMode,
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub model: Option<String>,
    pub profile: String,
    #[serde(default)]
    pub session_dir: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// The workspace isolation strategy for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceMode {
    Shared,
    GitWorktree,
    Copy,
}

/// Workspace isolation defaults and copy-mode ceilings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkspaceConfig {
    pub default_mode: WorkspaceMode,
    #[serde(default)]
    pub copy_max_bytes: Option<u64>,
    #[serde(default)]
    pub copy_max_files: Option<u64>,
    /// Ceiling on the artifact store's total bytes; `None` uses
    /// [`crate::workspace::DEFAULT_ARTIFACT_STORE_MAX_BYTES`].
    #[serde(default)]
    pub artifact_max_bytes: Option<u64>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            default_mode: WorkspaceMode::Shared,
            copy_max_bytes: None,
            copy_max_files: None,
            artifact_max_bytes: None,
        }
    }
}

/// The embedded `/crew`-style monitor's own listener.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DashboardConfig {
    pub enabled: bool,
    pub port: u16,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 4747,
        }
    }
}

/// Run history retention policy.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RetentionConfig {
    pub max_runs: u32,
    pub period: String,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            max_runs: 20,
            period: "30d".to_string(),
        }
    }
}

/// Additional redaction patterns, additive across every layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SecurityConfig {
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// The fully resolved crew configuration for a repository.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CrewConfig {
    pub approval: ApprovalMode,
    pub limits: Limits,
    pub display: DisplayConfig,
    pub adapters: BTreeMap<String, AdapterConfig>,
    pub workspace: WorkspaceConfig,
    pub dashboard: DashboardConfig,
    pub retention: RetentionConfig,
    pub security: SecurityConfig,
}

impl Default for CrewConfig {
    fn default() -> Self {
        Self {
            approval: ApprovalMode::Always,
            limits: Limits::default(),
            display: DisplayConfig::default(),
            adapters: default_adapters(),
            workspace: WorkspaceConfig::default(),
            dashboard: DashboardConfig::default(),
            retention: RetentionConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

/// The four built-in adapters, per spec §10. All four default to
/// `mode: tui` now that every vendor's TUI adapter has landed and passes
/// fixture-mode conformance (WP13 / WP27 / WP28) -- the spec's end
/// state, completing the WP4 controller override (see module docs).
fn default_adapters() -> BTreeMap<String, AdapterConfig> {
    let mut adapters = BTreeMap::new();
    adapters.insert(
        "claude".to_string(),
        AdapterConfig {
            enabled: true,
            bin: "claude".to_string(),
            mode: AdapterMode::Tui,
            permission_mode: PermissionMode::Max,
            model: None,
            profile: "complex analysis, investigation, deep debugging".to_string(),
            session_dir: None,
            extra_args: Vec::new(),
        },
    );
    adapters.insert(
        "codex".to_string(),
        AdapterConfig {
            enabled: true,
            bin: "codex".to_string(),
            mode: AdapterMode::Tui,
            permission_mode: PermissionMode::Max,
            model: None,
            profile: "code review, finding defects".to_string(),
            session_dir: None,
            extra_args: Vec::new(),
        },
    );
    adapters.insert(
        "copilot".to_string(),
        AdapterConfig {
            enabled: true,
            bin: "copilot".to_string(),
            mode: AdapterMode::Tui,
            permission_mode: PermissionMode::Max,
            model: None,
            profile: "documentation, explanations".to_string(),
            session_dir: None,
            extra_args: Vec::new(),
        },
    );
    adapters.insert(
        "omp".to_string(),
        AdapterConfig {
            enabled: true,
            bin: "omp".to_string(),
            mode: AdapterMode::Tui,
            permission_mode: PermissionMode::Max,
            model: Some("qwen".to_string()),
            profile: "implementation, coding tasks".to_string(),
            session_dir: None,
            extra_args: Vec::new(),
        },
    );
    adapters
}

/// The editor-annotation key. Accepted at the top level so a crew.json
/// can point at its JSON Schema (VS Code and friends autocomplete and
/// validate from it), but stripped before deserialization: it is an
/// annotation, never configuration, and must not reach [`CrewConfig`]
/// (which is `deny_unknown_fields`) or the [`fingerprint`] that
/// authorizes a run.
pub const SCHEMA_ANNOTATION_KEY: &str = "$schema";

const TOP_LEVEL_KEYS: &[&str] = &[
    SCHEMA_ANNOTATION_KEY,
    "approval",
    "limits",
    "display",
    "adapters",
    "workspace",
    "dashboard",
    "retention",
    "security",
];
const LIMITS_KEYS: &[&str] = &[
    "maxConcurrentWorkers",
    "inactivityTimeoutSec",
    "totalTimeoutSec",
    "turnBudgetPerSubtask",
];
const DISPLAY_KEYS: &[&str] = &["backend", "closeOnExit"];
const ADAPTER_KEYS: &[&str] = &[
    "enabled",
    "bin",
    "mode",
    "permissionMode",
    "model",
    "profile",
    "sessionDir",
    "extraArgs",
];
const WORKSPACE_KEYS: &[&str] = &[
    "defaultMode",
    "copyMaxBytes",
    "copyMaxFiles",
    "artifactMaxBytes",
];
const DASHBOARD_KEYS: &[&str] = &["enabled", "port"];
const RETENTION_KEYS: &[&str] = &["maxRuns", "period"];
const SECURITY_KEYS: &[&str] = &["patterns"];

/// Fails with [`ConfigError::UnknownKey`] naming `path.key` if `value` is
/// an object containing a key outside `allowed`. Non-objects are not this
/// function's concern (a type mismatch surfaces later, from the final
/// deserialize).
fn check_object_keys(value: &Value, allowed: &[&str], path: &str) -> Result<(), ConfigError> {
    let Value::Object(map) = value else {
        return Ok(());
    };
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            let full_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            return Err(ConfigError::UnknownKey { path: full_path });
        }
    }
    Ok(())
}

/// Walks the merged value against the known schema shape, so an unknown
/// key at any depth -- including inside an arbitrarily-named adapter
/// entry -- fails with the exact JSON path that named it.
fn validate_shape(merged: &Value) -> Result<(), ConfigError> {
    check_object_keys(merged, TOP_LEVEL_KEYS, "")?;
    if let Some(v) = merged.get("limits") {
        check_object_keys(v, LIMITS_KEYS, "limits")?;
    }
    if let Some(v) = merged.get("display") {
        check_object_keys(v, DISPLAY_KEYS, "display")?;
    }
    if let Some(v) = merged.get("workspace") {
        check_object_keys(v, WORKSPACE_KEYS, "workspace")?;
    }
    if let Some(v) = merged.get("dashboard") {
        check_object_keys(v, DASHBOARD_KEYS, "dashboard")?;
    }
    if let Some(v) = merged.get("retention") {
        check_object_keys(v, RETENTION_KEYS, "retention")?;
    }
    if let Some(v) = merged.get("security") {
        check_object_keys(v, SECURITY_KEYS, "security")?;
    }
    if let Some(Value::Object(adapters)) = merged.get("adapters") {
        for (name, adapter_value) in adapters {
            check_object_keys(adapter_value, ADAPTER_KEYS, &format!("adapters.{name}"))?;
        }
    }
    Ok(())
}

/// Deep-merges `overlay` onto `base` in place: objects recurse
/// key-by-key, `security.patterns` concatenates instead of replacing,
/// and every other leaf (scalars, arrays elsewhere, type mismatches)
/// has the overlay's value replace the base's.
fn deep_merge(base: &mut Value, overlay: &Value, path: &str) {
    let (Value::Object(base_map), Value::Object(overlay_map)) = (&mut *base, overlay) else {
        *base = overlay.clone();
        return;
    };

    for (key, overlay_val) in overlay_map {
        let child_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };

        if child_path == "security.patterns" {
            let overlay_arr = match overlay_val {
                Value::Array(arr) => arr.clone(),
                other => {
                    base_map.insert(key.clone(), other.clone());
                    continue;
                }
            };
            match base_map.get_mut(key) {
                Some(Value::Array(base_arr)) => base_arr.extend(overlay_arr),
                _ => {
                    base_map.insert(key.clone(), Value::Array(overlay_arr));
                }
            }
            continue;
        }

        match base_map.get_mut(key) {
            Some(existing) => deep_merge(existing, overlay_val, &child_path),
            None => {
                base_map.insert(key.clone(), overlay_val.clone());
            }
        }
    }
}

/// Loads and merges crew's JSON configuration: built-in defaults, then
/// each of `paths` in order (lowest precedence first -- e.g. the user
/// file before the project file), then `per_run` if given.
///
/// A path that does not exist is treated as an absent layer, not an
/// error -- the user and project config files are both optional. A path
/// that exists but is not valid JSON, or a merged document with an
/// unknown key at any depth, fails with [`ConfigError`].
pub fn load_layers(paths: &[&Path], per_run: Option<&Value>) -> Result<CrewConfig, ConfigError> {
    let mut merged =
        serde_json::to_value(CrewConfig::default()).expect("CrewConfig::default() serializes");

    for path in paths {
        if !path.exists() {
            continue;
        }
        let contents = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let layer: Value =
            serde_json::from_str(&contents).map_err(|source| ConfigError::Parse {
                path: path.display().to_string(),
                source,
            })?;
        deep_merge(&mut merged, &layer, "");
    }

    if let Some(overrides) = per_run {
        deep_merge(&mut merged, overrides, "");
    }

    validate_shape(&merged)?;

    // Strip the editor annotation after validation but before deserializing:
    // `CrewConfig` is `deny_unknown_fields`, and keeping `$schema` would also
    // leak an editor detail into `fingerprint`, making two structurally
    // identical policies authorize as different ones.
    if let Value::Object(map) = &mut merged {
        map.remove(SCHEMA_ANNOTATION_KEY);
    }

    serde_json::from_value(merged).map_err(|source| ConfigError::Deserialize(source.to_string()))
}

/// The file name a generated crew.json's `$schema` points at, and the
/// name `crewd config init` writes the schema out under, so the two land
/// side by side and the relative reference resolves in an editor.
pub const SCHEMA_FILE_NAME: &str = "crew-config.schema.json";

/// The full default snapshot document, `$schema` first so the annotation
/// is the first thing a reader (and an editor) sees.
///
/// This is what `crewd config init` writes and `crewd config print
/// --defaults` emits, and it is byte-for-byte the committed
/// `crew.default.json` at the repository root.
#[must_use]
pub fn default_document() -> Value {
    let mut doc = serde_json::Map::new();
    doc.insert(
        SCHEMA_ANNOTATION_KEY.to_string(),
        Value::String(format!("./{SCHEMA_FILE_NAME}")),
    );
    let defaults =
        serde_json::to_value(CrewConfig::default()).expect("CrewConfig::default() serializes");
    let Value::Object(default_map) = defaults else {
        unreachable!("CrewConfig serializes to a JSON object")
    };
    for (key, value) in default_map {
        doc.insert(key, value);
    }
    Value::Object(doc)
}

/// Pretty-printed, newline-terminated bytes of [`default_document`].
#[must_use]
pub fn render_default_document() -> Vec<u8> {
    let mut text =
        serde_json::to_string_pretty(&default_document()).expect("default document serializes");
    text.push('\n');
    text.into_bytes()
}

/// Pretty-printed, newline-terminated JSON Schema for [`CrewConfig`].
///
/// Mirrors `crew_protocol::render_schema`'s shape so both generated
/// schema documents are produced and compared the same way.
#[must_use]
pub fn render_config_schema() -> Vec<u8> {
    let schema = schemars::schema_for!(CrewConfig);
    let mut text = serde_json::to_string_pretty(&schema).expect("config schema serializes");
    text.push('\n');
    text.into_bytes()
}

/// One key a config layer sets to a value that is not the current
/// built-in default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultOverride {
    /// Full JSON path, e.g. `limits.totalTimeoutSec`.
    pub path: String,
    /// What the layer sets it to.
    pub configured: Value,
    /// What [`CrewConfig::default`] currently says.
    pub default: Value,
}

/// Reports every leaf in `layer` whose value differs from the current
/// built-in default, in stable path order.
///
/// This is deliberately "differs from today's default", not "the user did
/// not mean it" -- the two are indistinguishable on disk, because a value
/// frozen by `crewd config init` and a value typed on purpose are byte
/// identical. Reporting the difference is what makes a stale pin visible
/// at all: a file generated against an older `CrewConfig::default` starts
/// reporting the moment a default moves under it.
///
/// A path with no default counterpart (the `$schema` annotation, a custom
/// adapter entry) is skipped: there is nothing to have drifted from.
#[must_use]
pub fn diff_against_defaults(layer: &Value) -> Vec<DefaultOverride> {
    let defaults =
        serde_json::to_value(CrewConfig::default()).expect("CrewConfig::default() serializes");
    let mut found = Vec::new();
    collect_overrides(layer, &defaults, "", &mut found);
    // `serde_json` is built with `preserve_order` here, so map iteration is
    // insertion order -- sort explicitly rather than inheriting whatever
    // order the operator happened to type their file in.
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Recursive worker for [`diff_against_defaults`]. Descends only where
/// both sides are objects; a key absent from `defaults` has nothing to
/// have drifted from and is skipped along with its whole subtree.
fn collect_overrides(layer: &Value, defaults: &Value, path: &str, out: &mut Vec<DefaultOverride>) {
    let (Value::Object(layer_map), Value::Object(default_map)) = (layer, defaults) else {
        if layer != defaults {
            out.push(DefaultOverride {
                path: path.to_string(),
                configured: layer.clone(),
                default: defaults.clone(),
            });
        }
        return;
    };

    for (key, layer_val) in layer_map {
        let Some(default_val) = default_map.get(key) else {
            continue;
        };
        let child_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        collect_overrides(layer_val, default_val, &child_path, out);
    }
}

/// Computes a SHA-256 fingerprint of `cfg` over canonical JSON bytes, so
/// two structurally-equal configs fingerprint identically regardless of
/// the key order in whatever layer files produced them.
#[must_use]
pub fn fingerprint(cfg: &CrewConfig) -> String {
    use sha2::{Digest, Sha256};
    let value = serde_json::to_value(cfg).expect("CrewConfig serializes to JSON");
    let mut hasher = Sha256::new();
    hasher.update(
        crate::canonical_json::canonicalize(&value)
            .to_string()
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}
