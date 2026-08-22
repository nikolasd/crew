//! Policy evaluation: the `PolicyEvaluator` implementing
//! `AdapterAuthorization`.
//!
//! Enforces:
//! - Model allowlist (deny by default when allowlist is non-empty)
//! - Adapter allowlist (deny by default when allowlist is non-empty)
//! - Required capabilities, against the conformance-proven effective set
//! - The `native_discovery_reviewed` rollout gate, blocking rather than advisory
//! - Per-run and daily cost ceilings, including refusing an adapter that
//!   cannot report usage while a ceiling is configured
//! - Concurrency ceiling (block runs exceeding the ceiling)
//! - Nested worker policy (deny unexpected child workers)

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::adapter::{AdapterAuthorization, AdapterCapabilities, WorkerProfile};
use crate::config::RuntimePolicy;

/// A policy violation recorded as a runtime event.
#[derive(Debug, Clone)]
pub struct PolicyViolation {
    /// The worker profile that was denied.
    pub profile_id: String,
    /// The adapter kind (e.g. "claude", "codex").
    pub adapter: String,
    /// The model that was requested.
    pub model: String,
    /// The kind of violation.
    pub kind: PolicyViolationKind,
    /// Human-readable explanation.
    pub reason: String,
    /// Whether this is a nested/child worker violation.
    pub is_nested: bool,
}

/// The kind of policy violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyViolationKind {
    /// The model is not in the allowlist.
    ModelNotAllowed,
    /// The concurrency ceiling has been reached.
    ConcurrencyCeilingExceeded,
    /// A nested/child worker was denied by policy.
    NestedWorkerDenied,
    /// The adapter kind is not authorized.
    AdapterNotAllowed,
    /// A capability the org requires is absent from the adapter.
    CapabilityMissing,
    /// Native vendor-worker discovery is unacknowledged by rollout gates.
    NativeDiscoveryUnacknowledged,
    /// A configured spend ceiling is already reached or unmeasurable.
    CostCeiling,
}

impl std::fmt::Display for PolicyViolationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyViolationKind::ModelNotAllowed => write!(f, "model not allowed"),
            PolicyViolationKind::ConcurrencyCeilingExceeded => {
                write!(f, "concurrency ceiling exceeded")
            }
            PolicyViolationKind::NestedWorkerDenied => write!(f, "nested worker denied"),
            PolicyViolationKind::AdapterNotAllowed => write!(f, "adapter not allowed"),
            PolicyViolationKind::CapabilityMissing => write!(f, "required capability missing"),
            PolicyViolationKind::NativeDiscoveryUnacknowledged => {
                write!(f, "native discovery unacknowledged")
            }
            PolicyViolationKind::CostCeiling => write!(f, "cost ceiling"),
        }
    }
}

/// Errors from policy evaluation.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// The model is not in the allowlist.
    #[error("model '{model}' is not in the allowlist; allowed: {allowed:?}")]
    ModelNotAllowed { model: String, allowed: Vec<String> },

    /// The concurrency ceiling has been reached.
    #[error("concurrency ceiling {ceiling} reached; {active} active runs")]
    ConcurrencyCeilingExceeded { ceiling: u32, active: u32 },

    /// A nested/child worker was denied by policy.
    #[error("nested worker denied: {reason}")]
    NestedWorkerDenied { reason: String },

    /// The adapter kind is not authorized.
    #[error("adapter '{adapter}' is not authorized")]
    AdapterNotAllowed { adapter: String },

    /// A capability the org requires is absent from the adapter's
    /// effective (conformance-proven) capability set.
    #[error("adapter '{adapter}' does not provide required capability '{capability}'")]
    CapabilityMissing { adapter: String, capability: String },

    /// The profile can act on vendor-discovered child workers, but the
    /// org has not resolved the rollout gate that governs doing so.
    #[error(
        "adapter '{adapter}' declares native worker discovery, but rollout gate \
         'native_discovery_reviewed' is unresolved"
    )]
    NativeDiscoveryUnacknowledged { adapter: String },

    /// A spend ceiling is already reached.
    #[error("{scope} cost ceiling ${ceiling:.2} reached; ${spent:.2} already spent")]
    CostCeilingExceeded {
        scope: &'static str,
        ceiling: f64,
        spent: f64,
    },

    /// A spend ceiling is configured but this adapter cannot report usage,
    /// so the ceiling could never be observed, let alone enforced.
    #[error(
        "adapter '{adapter}' reports no usage, so the configured cost ceiling \
         cannot be enforced for it"
    )]
    CostCeilingUnenforceable { adapter: String },
}

/// Reads how much the project has already spent today, in USD.
///
/// Separate from the evaluator because authorization is synchronous while
/// the runtime's journal is behind an async actor: this reads the same
/// SQLite file directly, on the authorizing thread, rather than making the
/// whole authorization path async for a check most deployments never
/// configure.
pub trait DailySpend: Send + Sync {
    /// Returns today's (UTC) total `costUsd` across the project's
    /// `adapterUsageEvent` records.
    ///
    /// # Errors
    /// Returns a message if the journal cannot be read. A ceiling that
    /// cannot be measured must deny, so the caller treats this as a denial.
    fn spent_today_usd(&self) -> Result<f64, String>;
}

/// The production [`DailySpend`]: sums `adapterUsageEvent.costUsd` from the
/// runtime journal for the current UTC day.
pub struct JournalDailySpend {
    database: std::path::PathBuf,
    project_id: batman_protocol::ProjectId,
}

impl JournalDailySpend {
    #[must_use]
    pub fn new(database: std::path::PathBuf, project_id: batman_protocol::ProjectId) -> Self {
        Self {
            database,
            project_id,
        }
    }
}

impl DailySpend for JournalDailySpend {
    fn spent_today_usd(&self) -> Result<f64, String> {
        // Timestamps are stored as RFC3339 text, so the first ten
        // characters are `YYYY-MM-DD` and a prefix match is a day filter --
        // the same text-comparison approach `crate::audit::retention` uses.
        let today = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| e.to_string())?;
        let today = &today[..10];
        let conn = rusqlite::Connection::open(&self.database).map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT event_json FROM events \
                 WHERE project_id = ?1 AND timestamp LIKE ?2 \
                 AND event_json LIKE '%adapterUsageEvent%'",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(
                rusqlite::params![self.project_id.to_string(), format!("{today}%")],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| e.to_string())?;

        let mut total = 0.0_f64;
        for row in rows {
            let json = row.map_err(|e| e.to_string())?;
            let value: serde_json::Value =
                serde_json::from_str(&json).map_err(|e| e.to_string())?;
            if value.get("type").and_then(serde_json::Value::as_str) != Some("adapterUsageEvent") {
                continue;
            }
            total += value
                .get("payload")
                .and_then(|p| p.get("costUsd"))
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
        }
        Ok(total)
    }
}

/// The policy evaluator: implements [`AdapterAuthorization`] and enforces
/// model allowlists, concurrency ceilings, and nested worker policies.
///
/// Constructed once at daemon startup from a [`RuntimePolicy`]. The
/// concurrency counter uses atomic check-and-increment (`fetch_update`)
/// to avoid TOCTOU races between concurrent `authorize()` calls.
pub struct PolicyEvaluator {
    /// The merged runtime policy.
    policy: RuntimePolicy,
    /// Active run count (atomic for lock-free ceiling checks).
    active_runs: Arc<AtomicU32>,
    /// Whether nested workers are allowed (from policy).
    allow_nested: bool,
    /// Where today's project spend is read from. `None` in embeddings that
    /// have no journal to read; a daily ceiling configured without one is
    /// unmeasurable and therefore denies.
    daily_spend: Option<Arc<dyn DailySpend>>,
}

impl PolicyEvaluator {
    /// Creates a new `PolicyEvaluator` from a [`RuntimePolicy`].
    #[must_use]
    pub fn new(policy: RuntimePolicy) -> Self {
        Self {
            policy,
            active_runs: Arc::new(AtomicU32::new(0)),
            allow_nested: false, // default: deny nested
            daily_spend: None,
        }
    }

    /// Attaches the source the daily cost ceiling is measured against.
    #[must_use]
    pub fn with_daily_spend(mut self, source: Arc<dyn DailySpend>) -> Self {
        self.daily_spend = Some(source);
        self
    }

    /// Returns the effective runtime policy.
    #[must_use]
    pub fn policy(&self) -> &RuntimePolicy {
        &self.policy
    }

    /// Returns the current active run count.
    #[must_use]
    pub fn active_runs(&self) -> u32 {
        self.active_runs.load(Ordering::Relaxed)
    }

    /// Decrements the active run count. Returns the new count. Saturates
    /// at zero rather than wrapping if called without a matching booking.
    pub fn decrement_runs(&self) -> u32 {
        self.active_runs
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                Some(active.saturating_sub(1))
            })
            .map(|prev| prev.saturating_sub(1))
            .unwrap_or(0)
    }

    /// Evaluates whether a worker profile is authorized for a run,
    /// considering model allowlists, the adapter allowlist, concurrency
    /// ceilings, and nested worker policies.
    ///
    /// `policy` is the run's *own* resolved policy -- the startup policy
    /// re-merged with that run's `policyOverrides`. `None` means "use the
    /// startup policy this evaluator was constructed with", which is what
    /// every run without overrides relies on. The concurrency counter is
    /// always this evaluator's, since the daemon-wide number of active
    /// runs is a property of the daemon, not of one run's overrides.
    ///
    /// On success, books a concurrency slot using an atomic
    /// check-and-increment (`fetch_update` with a CAS loop) so that two
    /// concurrent `authorize()` calls cannot both read `active < ceiling`
    /// and both increment past the ceiling. Call [`PolicyEvaluator::release`]
    /// to free the slot when the run completes.
    ///
    /// # Errors
    /// Returns [`PolicyError`] if the profile is denied.
    pub fn evaluate(
        &self,
        profile: &WorkerProfile,
        effective_capabilities: &AdapterCapabilities,
        is_nested: bool,
        policy: Option<&RuntimePolicy>,
    ) -> Result<(), PolicyError> {
        let policy = policy.unwrap_or(&self.policy);

        // Check model allowlist.
        if !policy.allowed_models.is_empty() && !policy.allowed_models.contains(&profile.model) {
            return Err(PolicyError::ModelNotAllowed {
                model: profile.model.clone(),
                allowed: policy.allowed_models.clone(),
            });
        }

        // Check nested worker policy.
        if is_nested && !self.allow_nested {
            return Err(PolicyError::NestedWorkerDenied {
                reason: "nested workers are not allowed by policy".to_string(),
            });
        }

        // Check the adapter allowlist. An empty list permits every adapter
        // this runtime can build; a non-empty list is deny-by-default, and
        // is the only supported way for an org to forbid a vendor.
        if !policy.allowed_adapters.is_empty()
            && !policy.allowed_adapters.contains(&profile.adapter)
        {
            return Err(PolicyError::AdapterNotAllowed {
                adapter: profile.adapter.clone(),
            });
        }

        // Required capabilities. The effective set is the conformance-proven
        // one, so this denies an adapter that merely *claims* a capability
        // its fixture never demonstrated.
        for capability in &policy.required_capabilities {
            if !effective_capabilities.has(capability) {
                return Err(PolicyError::CapabilityMissing {
                    adapter: profile.adapter.clone(),
                    capability: capability.clone(),
                });
            }
        }

        // Native vendor-worker discovery is the one rollout gate that
        // blocks rather than advises: it governs whether this runtime may
        // act on workers it did not create.
        if effective_capabilities.nested != crate::adapter::NestedCapability::None
            && !policy.native_discovery_reviewed
        {
            return Err(PolicyError::NativeDiscoveryUnacknowledged {
                adapter: profile.adapter.clone(),
            });
        }

        // Cost ceilings. A ceiling that cannot be measured is worse than no
        // ceiling, because it reads as enforced -- so an adapter that
        // reports no usage is denied outright whenever one is configured.
        let cost_configured =
            policy.cost_ceiling_per_run_usd.is_some() || policy.cost_ceiling_daily_usd.is_some();
        if cost_configured && effective_capabilities.usage == crate::adapter::UsageCapability::None
        {
            return Err(PolicyError::CostCeilingUnenforceable {
                adapter: profile.adapter.clone(),
            });
        }
        if let Some(ceiling) = policy.cost_ceiling_daily_usd {
            // An unreadable journal makes the ceiling unmeasurable, which
            // is exactly the `Unenforceable` case -- never a silent allow.
            let Some(source) = self.daily_spend.as_ref() else {
                return Err(PolicyError::CostCeilingUnenforceable {
                    adapter: profile.adapter.clone(),
                });
            };
            let spent =
                source
                    .spent_today_usd()
                    .map_err(|_| PolicyError::CostCeilingUnenforceable {
                        adapter: profile.adapter.clone(),
                    })?;
            if spent >= ceiling {
                return Err(PolicyError::CostCeilingExceeded {
                    scope: "daily",
                    ceiling,
                    spent,
                });
            }
        }

        // Atomic check-and-increment: CAS loop to avoid TOCTOU race
        // between reading `active` and booking a slot.
        //
        // A per-run override may *tighten* the daemon-wide ceiling, never
        // loosen it: the counter is shared, so honoring a higher per-run
        // number would let one run raise the limit for every other.
        let ceiling = policy
            .concurrency_ceiling
            .min(self.policy.concurrency_ceiling);
        let booked =
            self.active_runs
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                    if active < ceiling {
                        Some(active + 1)
                    } else {
                        None
                    }
                });

        match booked {
            Ok(_) => Ok(()),
            Err(active) => Err(PolicyError::ConcurrencyCeilingExceeded { ceiling, active }),
        }
    }

    /// Releases a previously-booked concurrency slot (decrements the active
    /// run counter). Safe to call even if no slot was booked (saturates at
    /// zero).
    pub fn release(&self) {
        self.decrement_runs();
    }

    /// Records a policy violation as a structured event.
    #[must_use]
    pub fn record_violation(
        &self,
        profile: &WorkerProfile,
        kind: PolicyViolationKind,
        is_nested: bool,
    ) -> PolicyViolation {
        PolicyViolation {
            profile_id: profile.id.to_string(),
            adapter: profile.adapter.clone(),
            model: profile.model.clone(),
            kind,
            reason: format!("{kind}"),
            is_nested,
        }
    }
}

impl AdapterAuthorization for PolicyEvaluator {
    fn authorize(
        &self,
        profile: &WorkerProfile,
        effective_capabilities: &AdapterCapabilities,
        policy: Option<&RuntimePolicy>,
    ) -> Result<(), String> {
        self.evaluate(profile, effective_capabilities, false, policy)
            .map_err(|e| e.to_string())
    }

    fn release(&self) {
        self.decrement_runs();
    }
}

/// A policy evaluation result, including any violations.
#[derive(Debug, Clone)]
pub struct PolicyEvaluation {
    /// Whether the evaluation passed.
    pub allowed: bool,
    /// Any violations recorded.
    pub violations: Vec<PolicyViolation>,
    /// The effective policy that was evaluated.
    pub policy: RuntimePolicy,
}

impl PolicyEvaluation {
    /// Returns `true` if there are no violations.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{
        AdapterCapabilities, ApprovalsCapability, ClaudeStartupOptions, DurabilityCapability,
        NativeViewCapability, NestedCapability, ProfileId, ProtocolKind, ResumeCapability,
        StartupOptions, SteeringCapability, UsageCapability, WorkspaceControlCapability,
    };
    fn test_policy() -> RuntimePolicy {
        RuntimePolicy {
            fingerprint: "test".to_string(),
            display_backend: crate::config::crew::DisplayBackend::Auto,
            retention: "30d".to_string(),
            concurrency_ceiling: 2,
            allowed_models: vec!["gpt-4".to_string()],
            allowed_adapters: vec![],
            cost_ceiling_per_run_usd: None,
            cost_ceiling_daily_usd: None,
            required_capabilities: vec![],
            native_discovery_reviewed: true,
            org_security_patterns: vec![],
            copy_max_bytes: crate::workspace::DEFAULT_COPY_MAX_BYTES,
            copy_max_files: crate::workspace::DEFAULT_COPY_MAX_FILES,
            nested_violation_action: crate::config::NestedViolationAction::QuarantineAndCancel,
        }
    }

    fn test_profile(model: &str) -> WorkerProfile {
        WorkerProfile {
            id: ProfileId::new(),
            adapter: "claude".to_string(),
            model: model.to_string(),
            permission_envelope: serde_json::json!({}),
            startup_options: StartupOptions::Claude(ClaudeStartupOptions::default()),
            environment_allowlist: vec![],
            source: "test".to_string(),
        }
    }

    fn test_capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            protocol: ProtocolKind::Structured,
            resume: ResumeCapability::Session,
            steering: SteeringCapability::ActiveTurn,
            approvals: ApprovalsCapability::Controllable,
            structured_result: true,
            usage: UsageCapability::PerTurn,
            nested: NestedCapability::None,
            native_view: NativeViewCapability::None,
            workspace_control: WorkspaceControlCapability::ReadOnly,
            durability: DurabilityCapability::ParentScoped,
        }
    }

    #[test]
    fn test_policy_evaluator_allows_allowed_model() {
        let policy = test_policy();
        let evaluator = PolicyEvaluator::new(policy);
        let profile = test_profile("gpt-4");
        let caps = test_capabilities();

        assert!(evaluator.evaluate(&profile, &caps, false, None).is_ok());
        assert_eq!(evaluator.active_runs(), 1);

        evaluator.release();
        assert_eq!(evaluator.active_runs(), 0);
    }

    #[test]
    fn test_policy_evaluator_denies_disallowed_model() {
        let policy = test_policy();
        let evaluator = PolicyEvaluator::new(policy);
        let profile = test_profile("gpt-3.5");
        let caps = test_capabilities();

        let result = evaluator.evaluate(&profile, &caps, false, None);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            PolicyError::ModelNotAllowed { .. }
        ));
        assert_eq!(evaluator.active_runs(), 0);
    }

    #[test]
    fn test_policy_evaluator_empty_allowlist_allows_all() {
        let mut policy = test_policy();
        policy.allowed_models = vec![];
        let evaluator = PolicyEvaluator::new(policy);
        let profile = test_profile("any-model");
        let caps = test_capabilities();

        assert!(evaluator.evaluate(&profile, &caps, false, None).is_ok());
        assert_eq!(evaluator.active_runs(), 1);

        evaluator.release();
        assert_eq!(evaluator.active_runs(), 0);
    }

    #[test]
    fn test_policy_evaluator_concurrency_ceiling() {
        let policy = test_policy();
        let evaluator = PolicyEvaluator::new(policy);
        let caps = test_capabilities();

        let profile1 = test_profile("gpt-4");
        assert!(evaluator.evaluate(&profile1, &caps, false, None).is_ok());
        assert_eq!(evaluator.active_runs(), 1);

        let profile2 = test_profile("gpt-4");
        assert!(evaluator.evaluate(&profile2, &caps, false, None).is_ok());
        assert_eq!(evaluator.active_runs(), 2);

        // Third should be denied — ceiling is 2.
        let profile3 = test_profile("gpt-4");
        let result = evaluator.evaluate(&profile3, &caps, false, None);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            PolicyError::ConcurrencyCeilingExceeded { .. }
        ));
        assert_eq!(evaluator.active_runs(), 2);

        evaluator.release();
        evaluator.release();
        assert_eq!(evaluator.active_runs(), 0);
    }

    #[test]
    fn test_policy_evaluator_nested_denied_by_default() {
        let policy = test_policy();
        let evaluator = PolicyEvaluator::new(policy);
        let profile = test_profile("gpt-4");
        let caps = test_capabilities();

        let result = evaluator.evaluate(&profile, &caps, true, None);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            PolicyError::NestedWorkerDenied { .. }
        ));
        assert_eq!(evaluator.active_runs(), 0);
    }

    #[test]
    fn test_record_violation() {
        let policy = test_policy();
        let evaluator = PolicyEvaluator::new(policy);
        let profile = test_profile("gpt-3.5");

        let violation =
            evaluator.record_violation(&profile, PolicyViolationKind::ModelNotAllowed, false);

        assert_eq!(violation.model, "gpt-3.5");
        assert!(!violation.is_nested);
    }

    #[test]
    fn required_capability_absent_from_the_effective_set_denies() {
        let mut policy = test_policy();
        policy.required_capabilities = vec!["nativeView".to_string()];
        let evaluator = PolicyEvaluator::new(policy);
        let profile = test_profile("gpt-4");
        // `test_capabilities` declares `nativeView: none`.
        let caps = test_capabilities();

        let err = evaluator
            .evaluate(&profile, &caps, false, None)
            .expect_err("a missing required capability must deny");
        assert!(
            matches!(err, PolicyError::CapabilityMissing { .. }),
            "{err}"
        );
        assert_eq!(evaluator.active_runs(), 0, "a denial must book no slot");
    }

    #[test]
    fn required_capability_present_allows() {
        let mut policy = test_policy();
        policy.required_capabilities = vec!["structuredResult".to_string(), "usage".to_string()];
        let evaluator = PolicyEvaluator::new(policy);

        assert!(
            evaluator
                .evaluate(&test_profile("gpt-4"), &test_capabilities(), false, None)
                .is_ok()
        );
    }

    #[test]
    fn nested_capable_adapter_denied_until_the_discovery_gate_is_resolved() {
        let mut policy = test_policy();
        policy.native_discovery_reviewed = false;
        let evaluator = PolicyEvaluator::new(policy);
        let mut caps = test_capabilities();
        caps.nested = NestedCapability::Observable;

        let err = evaluator
            .evaluate(&test_profile("gpt-4"), &caps, false, None)
            .expect_err("an unresolved discovery gate must block, not merely advise");
        assert!(
            matches!(err, PolicyError::NativeDiscoveryUnacknowledged { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_cost_ceiling_denies_an_adapter_that_cannot_report_usage() {
        let mut policy = test_policy();
        policy.cost_ceiling_per_run_usd = Some(5.0);
        let evaluator = PolicyEvaluator::new(policy);
        let mut caps = test_capabilities();
        caps.usage = UsageCapability::None;

        let err = evaluator
            .evaluate(&test_profile("gpt-4"), &caps, false, None)
            .expect_err("an unmeasurable ceiling must deny rather than read as enforced");
        assert!(
            matches!(err, PolicyError::CostCeilingUnenforceable { .. }),
            "{err}"
        );
    }

    struct FixedSpend(f64);
    impl DailySpend for FixedSpend {
        fn spent_today_usd(&self) -> Result<f64, String> {
            Ok(self.0)
        }
    }

    struct UnreadableSpend;
    impl DailySpend for UnreadableSpend {
        fn spent_today_usd(&self) -> Result<f64, String> {
            Err("journal unreadable".to_string())
        }
    }

    #[test]
    fn daily_ceiling_denies_once_reached_and_allows_below_it() {
        let mut policy = test_policy();
        policy.cost_ceiling_daily_usd = Some(10.0);

        let spent_over = PolicyEvaluator::new(policy.clone())
            .with_daily_spend(Arc::new(FixedSpend(10.0)))
            .evaluate(&test_profile("gpt-4"), &test_capabilities(), false, None)
            .expect_err("spend at the ceiling must deny");
        assert!(
            matches!(
                spent_over,
                PolicyError::CostCeilingExceeded { scope: "daily", .. }
            ),
            "{spent_over}"
        );

        assert!(
            PolicyEvaluator::new(policy)
                .with_daily_spend(Arc::new(FixedSpend(9.99)))
                .evaluate(&test_profile("gpt-4"), &test_capabilities(), false, None)
                .is_ok(),
            "spend below the ceiling must be allowed"
        );
    }

    #[test]
    fn an_unreadable_journal_denies_rather_than_silently_allowing() {
        let mut policy = test_policy();
        policy.cost_ceiling_daily_usd = Some(10.0);
        let evaluator = PolicyEvaluator::new(policy).with_daily_spend(Arc::new(UnreadableSpend));

        let err = evaluator
            .evaluate(&test_profile("gpt-4"), &test_capabilities(), false, None)
            .expect_err("an unreadable spend source must deny");
        assert!(
            matches!(err, PolicyError::CostCeilingUnenforceable { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_per_run_override_may_tighten_the_concurrency_ceiling_but_never_loosen_it() {
        let startup = test_policy(); // concurrency_ceiling: 2
        let mut loosened = startup.clone();
        loosened.concurrency_ceiling = 10;
        let evaluator = PolicyEvaluator::new(startup);

        // The counter is daemon-wide, so honoring the higher per-run number
        // would raise the limit for every other run too.
        for _ in 0..2 {
            evaluator
                .evaluate(
                    &test_profile("gpt-4"),
                    &test_capabilities(),
                    false,
                    Some(&loosened),
                )
                .expect("the first two slots are within the startup ceiling");
        }
        let err = evaluator
            .evaluate(
                &test_profile("gpt-4"),
                &test_capabilities(),
                false,
                Some(&loosened),
            )
            .expect_err("an override must not raise the daemon-wide ceiling");
        assert!(
            matches!(
                err,
                PolicyError::ConcurrencyCeilingExceeded { ceiling: 2, .. }
            ),
            "{err}"
        );
    }
}
