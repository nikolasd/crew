//! Worker profiles: the immutable, validated configuration a managed
//! adapter's supervised process is launched from.
//!
//! A [`WorkerProfile`] is registered once (`profile/register`), validated
//! against an [`EffectivePolicy`], fingerprinted, and stored under a fresh
//! [`ProfileId`]. `worker/create` for a reserved [`AdapterKind`] resolves
//! that `profileId` and copies the resolved fields into the worker's
//! immutable `WorkerProfileRef` snapshot -- changing (or re-registering) the
//! source profile afterward never mutates an already-created worker's
//! snapshot, because nothing re-reads the profile store after that point.
//!
//! `environmentAllowlist` carries variable *names* only (`Vec<String>`):
//! there is no field anywhere in this module that could hold an inherited
//! variable's *value*, so a value can never reach the profile snapshot,
//! the durable journal, or a log line through this type, structurally --
//! not by convention.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::security::redaction::Redactor;

/// Identifies a registered [`WorkerProfile`]. Runtime-internal only: it
/// never crosses the wire as a typed, schema-generated value (the
/// `profile/register`/`worker/create` RPC methods parse it as a plain
/// string field, exactly like every other orchestration method's
/// hand-parsed JSON params -- see `crate::service::OrchestrationService`).
/// Deserializing a caller-supplied `id` is tolerated (defaulting to a
/// fresh, throwaway value) but never trusted: `profile/register` always
/// overwrites it with a server-generated id, exactly like `worker/create`
/// never trusts a caller-supplied `WorkerId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProfileId(Uuid);

impl ProfileId {
    /// Generates a fresh, time-ordered (UUIDv7) profile identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Parses a profile identifier from its canonical string form.
    ///
    /// # Errors
    /// Returns [`uuid::Error`] if `value` is not a valid UUID.
    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for ProfileId {
    type Err = uuid::Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ProfileId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

fn fresh_profile_id() -> ProfileId {
    ProfileId::new()
}

/// The four managed adapter kinds this milestone implements. Distinct from
/// `WorkerProfileRef.adapter` (a plain, unvalidated `String` retained for
/// backward compatibility with pre-adapter workers such as `"fake"` or
/// `"ompNative"`, which never require a profile): these four wire names are
/// exactly the ones `worker/create` requires a resolved `profileId` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AdapterKind {
    Claude,
    Codex,
    Copilot,
    OmpRpc,
}

impl AdapterKind {
    /// The exact reserved wire strings that require a validated profile.
    pub const RESERVED_NAMES: [&'static str; 4] = ["claude", "codex", "copilot", "ompRpc"];

    #[must_use]
    pub fn wire_name(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::OmpRpc => "ompRpc",
        }
    }

    #[must_use]
    pub fn from_wire_name(value: &str) -> Option<Self> {
        match value {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "copilot" => Some(Self::Copilot),
            "ompRpc" => Some(Self::OmpRpc),
            _ => None,
        }
    }
}

impl fmt::Display for AdapterKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// Strict, per-adapter startup options. Externally tagged by adapter name
/// (`{"codex": {...}}`), matching every other adapter-facing wire shape's
/// camelCase convention. Each inner struct is `deny_unknown_fields`, so an
/// unrecognized option key is a hard validation failure at deserialize
/// time -- never a silently-ignored field. `TerminalDegraded` is the
/// fallback identity used when a structured adapter's protocol becomes
/// unhealthy and control falls back to terminal-screen automation (Herdr
/// or tmux, wired by a later milestone); it does not correspond to one of
/// the four reserved [`AdapterKind`] values because it wraps *any*
/// underlying harness rather than replacing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StartupOptions {
    #[serde(rename = "claude")]
    Claude(ClaudeStartupOptions),
    #[serde(rename = "codex")]
    Codex(CodexStartupOptions),
    #[serde(rename = "copilot")]
    Copilot(CopilotStartupOptions),
    #[serde(rename = "ompRpc")]
    OmpRpc(OmpRpcStartupOptions),
    #[serde(rename = "terminalDegraded")]
    TerminalDegraded(TerminalDegradedStartupOptions),
}

impl StartupOptions {
    /// The reserved [`AdapterKind`] this variant maps to, or `None` for
    /// `TerminalDegraded` (which wraps an arbitrary underlying harness
    /// named in its own `underlyingAdapter` field instead).
    #[must_use]
    pub fn adapter_kind(&self) -> Option<AdapterKind> {
        match self {
            Self::Claude(_) => Some(AdapterKind::Claude),
            Self::Codex(_) => Some(AdapterKind::Codex),
            Self::Copilot(_) => Some(AdapterKind::Copilot),
            Self::OmpRpc(_) => Some(AdapterKind::OmpRpc),
            Self::TerminalDegraded(_) => None,
        }
    }

    /// The [`AdapterMode`] this variant declares, or `None` for
    /// `TerminalDegraded` (which has no `mode` field at all -- it wraps an
    /// arbitrary underlying harness rather than replacing one of the four
    /// reserved [`AdapterKind`]s).
    #[must_use]
    pub fn mode(&self) -> Option<AdapterMode> {
        match self {
            Self::Claude(opts) => Some(opts.mode),
            Self::Codex(opts) => Some(opts.mode),
            Self::Copilot(opts) => Some(opts.mode),
            Self::OmpRpc(opts) => Some(opts.mode),
            Self::TerminalDegraded(_) => None,
        }
    }
}

/// Whether a supervised adapter process runs attached to a terminal UI or
/// fully headless. Defaults to `Headless` so a profile serialized before
/// this field existed still deserializes -- the same wire-compat pattern
/// `ApprovalEvent.reason` uses in `crew_protocol::event`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AdapterMode {
    Tui,
    #[default]
    Headless,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeStartupOptions {
    pub allowed_tools: Option<Vec<String>>,
    pub permission_mode: Option<String>,
    pub max_turns: Option<u32>,
    /// Model selector resolved from the worker profile (`profile.model`);
    /// `Some` only when the profile carried a non-empty model. Headless
    /// launches turn this into `--model`; TUI launches read their own
    /// config layer instead (WP13/WP27).
    pub model: Option<String>,
    #[serde(default)]
    pub mode: AdapterMode,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexStartupOptions {
    pub sandbox_mode: Option<String>,
    pub approval_policy: Option<String>,
    pub config_overrides: Option<Vec<String>>,
    /// Model selector from the worker profile; headless launches turn it
    /// into a `model` config override (`-c model=...`) -- codex has no
    /// dedicated model flag (WP26).
    pub model: Option<String>,
    #[serde(default)]
    pub mode: AdapterMode,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CopilotStartupOptions {
    pub allow_tool: Option<Vec<String>>,
    pub deny_tool: Option<Vec<String>>,
    pub log_level: Option<String>,
    /// Model selector from the worker profile; headless launches turn it
    /// into `--model=<model>` (WP26).
    pub model: Option<String>,
    #[serde(default)]
    pub mode: AdapterMode,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OmpRpcStartupOptions {
    pub profile: Option<String>,
    pub host_tools: Option<Vec<String>>,
    #[serde(default)]
    pub mode: AdapterMode,
}

/// Startup options for the terminal-controlled degraded fallback mode.
/// `underlying_adapter` names the harness actually running underneath
/// (e.g. `"claude"`); this milestone defines the shape so it round-trips
/// and validates, but no adapter implements it yet -- see the Workspaces
/// and Displays plan for the Herdr/tmux backends that will.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalDegradedStartupOptions {
    pub backend: String,
    pub underlying_adapter: Option<String>,
}

/// A validated, immutable snapshot of one adapter worker's configuration.
///
/// `id` is assigned once, at registration, and never changes; a caller-
/// supplied value (or its absence) in a `profile/register` request is
/// ignored -- the server always overwrites it with a freshly generated
/// [`ProfileId`], exactly like `worker/create` never trusts a caller-
/// supplied `WorkerId`. `adapter` mirrors `startup_options`'s reserved
/// [`AdapterKind`] wire name when one applies, or names the underlying
/// harness for `TerminalDegraded`; [`WorkerProfile::validate`] enforces the
/// two stay consistent. `environmentAllowlist` is a plain list of variable
/// *names* the supervised process may inherit from the runtime's own
/// environment at spawn time -- never values. `permissionEnvelope` is
/// caller-supplied policy JSON; [`WorkerProfile::validate`] rejects it
/// outright if it contains a secret-shaped string (matched by the same
/// built-in rules `crate::security::redaction::Redactor` applies to every
/// other durable string) rather than silently persisting or redacting a
/// credential a caller mistakenly placed there.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerProfile {
    #[serde(default = "fresh_profile_id")]
    pub id: ProfileId,
    pub adapter: String,
    pub model: String,
    #[serde(default)]
    pub permission_envelope: serde_json::Value,
    pub startup_options: StartupOptions,
    #[serde(default)]
    pub environment_allowlist: Vec<String>,
    pub source: String,
}

/// Why a [`WorkerProfile`] failed validation.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProfileError {
    #[error("model must not be empty")]
    EmptyModel,
    #[error("adapter {declared:?} does not match startup options for {from_options:?}")]
    AdapterMismatch {
        declared: String,
        from_options: &'static str,
    },
    #[error("environment variable {0} is not in the effective policy allowlist")]
    EnvironmentNotAllowed(String),
    #[error(
        "permissionEnvelope contains a secret-shaped value; use environmentAllowlist for credentials instead"
    )]
    SecretShapedPermissionEnvelope,
    /// CREW-7: `mode: "headless"` (explicit, or omitted and defaulted by
    /// [`AdapterMode`]'s own wire-compat `#[default]`) was given at
    /// `profile/register` time. Rejected here, at registration, rather
    /// than only at dispatch (`RegistryError::HeadlessControlPlaneRetired`)
    /// -- that dispatch-time check stays exactly as it is (it must, so a
    /// pre-retirement historical profile still resumes and fails
    /// identically); this is the register-time backstop for a *new*
    /// profile, so a caller finds out immediately rather than after a
    /// confusing delay at first submit.
    #[error(
        "adapter {adapter:?} was registered with mode: \"headless\" (or no mode at all, which \
         defaults to it), which is retired in crew v2 -- register with mode: \"tui\" instead"
    )]
    RetiredHeadlessMode { adapter: String },
    /// `StartupOptions::TerminalDegraded` is a real, deserializable variant
    /// -- the documented fallback identity for when a structured adapter's
    /// protocol goes unhealthy -- but the milestone that would actually
    /// transition a run into it is not wired yet. Registering one directly
    /// would build a working adapter nothing else ever puts a run into, so
    /// `profile/register` refuses it explicitly rather than validating by
    /// omission (its `adapter_kind()`/`mode()` both return `None`, so
    /// neither the adapter-mismatch nor the retired-headless check above
    /// would otherwise ever see it). The variant stays deserializable --
    /// this is a registration-time gate, not a wire-format rejection -- so
    /// a historical row naming it still reads back exactly as journaled.
    #[error("terminalDegraded is not implemented; registration refused")]
    TerminalDegradedNotImplemented,
}

impl WorkerProfile {
    /// The reserved [`AdapterKind`] this profile's startup options declare,
    /// or `None` for `TerminalDegraded`.
    #[must_use]
    pub fn adapter_kind(&self) -> Option<AdapterKind> {
        self.startup_options.adapter_kind()
    }

    #[must_use]
    pub fn startup_options(&self) -> &StartupOptions {
        &self.startup_options
    }

    #[must_use]
    pub fn environment_allowlist(&self) -> &[String] {
        &self.environment_allowlist
    }

    /// Validates this profile against `policy`: the model must be
    /// non-empty, `adapter` must agree with `startup_options`, and every
    /// allowlisted environment variable *name* must be permitted by the
    /// effective policy.
    ///
    /// # Errors
    /// Returns [`ProfileError`] on the first violation found.
    pub fn validate(&self, policy: &EffectivePolicy) -> Result<(), ProfileError> {
        if self.model.trim().is_empty() {
            return Err(ProfileError::EmptyModel);
        }
        if matches!(self.startup_options, StartupOptions::TerminalDegraded(_)) {
            return Err(ProfileError::TerminalDegradedNotImplemented);
        }
        if let Some(kind) = self.startup_options.adapter_kind()
            && self.adapter != kind.wire_name()
        {
            return Err(ProfileError::AdapterMismatch {
                declared: self.adapter.clone(),
                from_options: kind.wire_name(),
            });
        }
        for name in &self.environment_allowlist {
            if !policy.is_env_name_allowed(name) {
                return Err(ProfileError::EnvironmentNotAllowed(name.clone()));
            }
        }
        if permission_envelope_contains_secret_shape(&self.permission_envelope) {
            return Err(ProfileError::SecretShapedPermissionEnvelope);
        }
        if self.startup_options.mode() == Some(AdapterMode::Headless) {
            return Err(ProfileError::RetiredHeadlessMode {
                adapter: self.adapter.clone(),
            });
        }
        Ok(())
    }

    /// A deterministic `sha256:<hex>` fingerprint over this profile's
    /// content -- everything except `id`, so two registrations of
    /// identical content share one fingerprint but always mint distinct
    /// ids. This workspace enables `preserve_order`, so `serde_json::Map` is
    /// insertion-ordered rather than sorted. The current field order is
    /// fixed by the struct declaration plus the canonical sanitized
    /// `permissionEnvelope`; the final
    /// [`crate::canonical_json::canonicalize_in_place`] is defense in depth
    /// for future free-form fields. Content is name-only/never-secret by
    /// construction (see module docs), so the fingerprint itself can never
    /// encode a secret value.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let redactor = Redactor::new();
        let mut canonical =
            serde_json::to_value(self).expect("WorkerProfile is a plain, always-serializable type");
        if let Some(map) = canonical.as_object_mut() {
            map.remove("id");
            // Defense in depth: `validate` already rejects a secret-shaped
            // `permissionEnvelope` outright, but the fingerprint itself
            // must never be able to encode raw secret-shaped text even if
            // `validate` was bypassed by a future call site.
            let sanitized = redactor.sanitize_json(&self.permission_envelope);
            map.insert(
                "permissionEnvelope".to_string(),
                serde_json::from_str(sanitized.as_str())
                    .expect("Redactor::sanitize_json always produces valid JSON text"),
            );
        }
        crate::canonical_json::canonicalize_in_place(&mut canonical);
        let bytes = serde_json::to_vec(&canonical)
            .expect("a canonicalized serde_json::Value always serializes");
        let digest = Sha256::digest(&bytes);
        format!("sha256:{digest:x}")
    }
}

/// Whether `value` contains any string (key or value, at any nesting
/// depth) matching a built-in secret-shaped pattern -- reuses exactly the
/// regex rules `crate::security::redaction::Redactor` applies to every
/// other durable string, by round-tripping through
/// [`Redactor::sanitize_json`] and comparing before/after text. A caller
/// that wants to persist a credential must use `environmentAllowlist`
/// (names only, resolved at spawn time, never stored) instead.
fn permission_envelope_contains_secret_shape(value: &serde_json::Value) -> bool {
    let redactor = Redactor::new();
    // Both sides must be canonical so only redaction changes the comparison.
    let raw =
        serde_json::to_string(&crate::canonical_json::canonicalize(value)).unwrap_or_default();
    let sanitized = redactor.sanitize_json(value);
    raw != sanitized.as_str()
}

/// The effective policy a [`WorkerProfile`] is validated against: which
/// environment variable *names* may be inherited by a supervised vendor
/// process. Organization/repository/engineer-local policy layering (per
/// the design spec's "Configuration precedence") is a later milestone's
/// concern; this is the seam that later policy resolution plugs into.
#[derive(Debug, Clone)]
pub struct EffectivePolicy {
    allowed_env_names: HashSet<String>,
}

impl EffectivePolicy {
    /// A conservative baseline: only variables needed for a process to run
    /// at all (never a secret-shaped name) are pre-allowed.
    #[must_use]
    pub fn baseline() -> Self {
        let mut allowed_env_names = HashSet::new();
        for name in [
            "HOME", "PATH", "LANG", "LC_ALL", "TERM", "TZ", "SHELL", "USER", "LOGNAME",
        ] {
            allowed_env_names.insert(name.to_string());
        }
        Self { allowed_env_names }
    }

    /// An empty policy: nothing is allowed until explicitly permitted.
    /// Useful for tests that want to prove the denial path precisely.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            allowed_env_names: HashSet::new(),
        }
    }

    /// Explicitly permits a variable name (e.g. an org-approved secret
    /// like `ANTHROPIC_API_KEY`) to be inherited by a supervised process.
    /// Never stores or accepts a *value* -- there is no such parameter.
    pub fn allow_env_name(&mut self, name: impl Into<String>) {
        self.allowed_env_names.insert(name.into());
    }

    #[must_use]
    pub fn is_env_name_allowed(&self, name: &str) -> bool {
        self.allowed_env_names.contains(name)
    }
}

impl Default for EffectivePolicy {
    fn default() -> Self {
        Self::baseline()
    }
}

#[cfg(test)]
mod retired_headless_registration_tests {
    use super::*;

    fn claude_profile(startup_options: ClaudeStartupOptions) -> WorkerProfile {
        WorkerProfile {
            id: ProfileId::new(),
            adapter: "claude".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            permission_envelope: serde_json::json!({}),
            startup_options: StartupOptions::Claude(startup_options),
            environment_allowlist: vec![],
            source: "test".to_string(),
        }
    }

    /// CREW-7: a *new* `profile/register` request must be rejected up
    /// front for a reserved adapter kind whose mode is explicitly the
    /// retired `Headless` -- not accepted only to fail later, at first
    /// submit, with a confusing delay (the existing
    /// `RegistryError::HeadlessControlPlaneRetired`, which stays reachable
    /// unchanged for a historical resumed profile; this is the
    /// register-time backstop for a brand new one).
    #[test]
    fn validate_rejects_an_explicitly_headless_profile() {
        let profile = claude_profile(ClaudeStartupOptions {
            mode: AdapterMode::Headless,
            ..Default::default()
        });
        let err = profile
            .validate(&EffectivePolicy::baseline())
            .expect_err("a headless-mode profile must not validate");
        assert!(
            matches!(err, ProfileError::RetiredHeadlessMode { .. }),
            "unexpected error: {err:?}"
        );
    }

    /// The same rejection must fire when `mode` is *omitted* entirely --
    /// serde's own wire-compat default (`AdapterMode::default() ==
    /// Headless`, kept for historical deserialization) would otherwise let
    /// a mode-less profile register fine today and only fail once a run
    /// actually tries to submit against it.
    #[test]
    fn validate_rejects_a_profile_with_mode_omitted() {
        let startup_options: ClaudeStartupOptions = serde_json::from_value(serde_json::json!({
            "allowedTools": null,
            "permissionMode": null,
            "maxTurns": null,
        }))
        .unwrap();
        let profile = claude_profile(startup_options);
        let err = profile
            .validate(&EffectivePolicy::baseline())
            .expect_err("a mode-less profile defaults to Headless and must not validate");
        assert!(
            matches!(err, ProfileError::RetiredHeadlessMode { .. }),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn validate_accepts_an_explicit_tui_profile() {
        let profile = claude_profile(ClaudeStartupOptions {
            mode: AdapterMode::Tui,
            ..Default::default()
        });
        profile
            .validate(&EffectivePolicy::baseline())
            .expect("an explicit tui-mode profile must validate");
    }

    /// `TerminalDegraded` carries no `mode` field at all (it wraps an
    /// arbitrary underlying harness rather than one of the four reserved
    /// [`AdapterKind`]s), so it slips past both the adapter-mismatch check
    /// (guarded by `adapter_kind()`, which returns `None` for it) and the
    /// retired-headless check (guarded by `mode()`, also `None`) -- that
    /// gap is exactly how a `terminalDegraded` profile used to validate
    /// successfully. The milestone that would actually drive a run into
    /// this fallback (a structured adapter's protocol going unhealthy) is
    /// not wired yet, so registering one today would build a working
    /// adapter nothing else ever transitions a run into -- refused here,
    /// explicitly, rather than left to validate by omission.
    #[test]
    fn validate_rejects_terminal_degraded_as_not_yet_implemented() {
        let profile = terminal_degraded_profile();
        let err = profile
            .validate(&EffectivePolicy::baseline())
            .expect_err("terminalDegraded must be refused at registration");
        assert!(
            matches!(err, ProfileError::TerminalDegradedNotImplemented),
            "unexpected error: {err:?}"
        );
    }

    fn terminal_degraded_profile() -> WorkerProfile {
        WorkerProfile {
            id: ProfileId::new(),
            adapter: "some-harness".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            permission_envelope: serde_json::json!({}),
            startup_options: StartupOptions::TerminalDegraded(TerminalDegradedStartupOptions {
                backend: "tmux".to_string(),
                underlying_adapter: Some("claude".to_string()),
            }),
            environment_allowlist: vec![],
            source: "test".to_string(),
        }
    }

    /// The register-time refusal above must never become a deserialization
    /// refusal: an already-stored profile row (from before this check
    /// existed, or a future one written directly by whatever milestone
    /// eventually wires this up) must still parse -- `validate` is a gate
    /// `profile/register` calls, not something `serde` enforces, so a
    /// stored row is read back exactly as journaled regardless of whether
    /// today's registration rules would have accepted it.
    #[test]
    fn terminal_degraded_startup_options_still_deserializes() {
        let json = serde_json::json!({
            "id": ProfileId::new(),
            "adapter": "some-harness",
            "model": "claude-sonnet-4-5",
            "permissionEnvelope": {},
            "startupOptions": {
                "terminalDegraded": { "backend": "tmux", "underlyingAdapter": "claude" }
            },
            "environmentAllowlist": [],
            "source": "historical-row",
        });
        let profile: WorkerProfile =
            serde_json::from_value(json).expect("a stored terminalDegraded row must still parse");
        assert!(matches!(
            profile.startup_options,
            StartupOptions::TerminalDegraded(_)
        ));
    }
}

#[cfg(test)]
mod adapter_mode_tests {
    use super::*;

    #[test]
    fn adapter_mode_defaults_to_headless() {
        assert_eq!(AdapterMode::default(), AdapterMode::Headless);
    }

    /// A profile serialized before `mode` existed on these structs must
    /// still deserialize, defaulting to `Headless` -- the same wire-compat
    /// requirement the runtime already applies to `ApprovalEvent.reason`.
    #[test]
    fn every_vendor_startup_options_defaults_mode_when_field_is_missing() {
        let claude: ClaudeStartupOptions = serde_json::from_value(serde_json::json!({
            "allowedTools": null,
            "permissionMode": null,
            "maxTurns": null,
        }))
        .unwrap();
        assert_eq!(claude.mode, AdapterMode::Headless);

        let codex: CodexStartupOptions = serde_json::from_value(serde_json::json!({
            "sandboxMode": null,
            "approvalPolicy": null,
            "configOverrides": null,
        }))
        .unwrap();
        assert_eq!(codex.mode, AdapterMode::Headless);

        let copilot: CopilotStartupOptions = serde_json::from_value(serde_json::json!({
            "allowTool": null,
            "denyTool": null,
            "logLevel": null,
        }))
        .unwrap();
        assert_eq!(copilot.mode, AdapterMode::Headless);

        let omp_rpc: OmpRpcStartupOptions = serde_json::from_value(serde_json::json!({
            "profile": null,
            "hostTools": null,
        }))
        .unwrap();
        assert_eq!(omp_rpc.mode, AdapterMode::Headless);
    }

    #[test]
    fn mode_round_trips_tui() {
        let options = ClaudeStartupOptions {
            mode: AdapterMode::Tui,
            ..Default::default()
        };
        let value = serde_json::to_value(&options).unwrap();
        assert_eq!(value["mode"], "tui");
        let round_tripped: ClaudeStartupOptions = serde_json::from_value(value).unwrap();
        assert_eq!(round_tripped.mode, AdapterMode::Tui);
    }

    #[test]
    fn startup_options_still_rejects_unknown_fields() {
        let value = serde_json::json!({
            "allowedTools": null,
            "permissionMode": null,
            "maxTurns": null,
            "mode": "headless",
            "unexpected": true,
        });
        assert!(serde_json::from_value::<ClaudeStartupOptions>(value).is_err());
    }
}
