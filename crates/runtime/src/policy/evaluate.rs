//! Policy evaluation: the `PolicyEvaluator` implementing
//! `AdapterAuthorization`.
//!
//! Enforces:
//! - Concurrency ceiling (block runs exceeding the ceiling)
//! - Nested worker policy (deny unexpected child workers)
//!
//! Crew-v2 gap-closure WP5 (ruling): the org-governance enforcement this
//! evaluator used to also apply -- model/adapter allowlists, a
//! required-capability list, per-run/daily cost ceilings, and the
//! `native_discovery_reviewed` rollout gate -- is retired along with the
//! YAML org config layer that was its only source (`crew.json`, spec §10,
//! has no equivalent surface; see `docs/superpowers/specs/2026-08-22-crew-v2-design.md`
//! §2.2/§12). That layer was never actually wired up end to end (the
//! extension passed no config-path flags), so this changes nothing about
//! production behavior. See `docs/future-features.md` for the decision
//! trigger if org governance returns.
//!
//! Nested-worker *safety* does not regress: the per-child
//! record-intent-until-accepted/denied flow (`coordination`'s child
//! request + `policy/violation/decide`) is untouched and remains the real
//! enforcement for vendor-discovered children. What's retired here is only
//! the pre-authorization gate that blocked authorizing a profile whose
//! *declared* nested capability was unacknowledged by org config -- a
//! config-sourced check, not a capability-downgrade one.

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
    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5). Kept declared,
    /// unconstructable in production, because a journaled event from
    /// before this retirement could carry it.
    ModelNotAllowed,
    /// The concurrency ceiling has been reached.
    ConcurrencyCeilingExceeded,
    /// A nested/child worker was denied by policy.
    NestedWorkerDenied,
    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
    AdapterNotAllowed,
    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
    CapabilityMissing,
    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
    NativeDiscoveryUnacknowledged,
    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
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
    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5). Kept declared,
    /// unconstructable in production, because a journaled event from
    /// before this retirement could carry it.
    #[error("model '{model}' is not in the allowlist; allowed: {allowed:?}")]
    ModelNotAllowed { model: String, allowed: Vec<String> },

    /// The concurrency ceiling has been reached.
    #[error("concurrency ceiling {ceiling} reached; {active} active runs")]
    ConcurrencyCeilingExceeded { ceiling: u32, active: u32 },

    /// A nested/child worker was denied by policy.
    #[error("nested worker denied: {reason}")]
    NestedWorkerDenied { reason: String },

    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
    #[error("adapter '{adapter}' is not authorized")]
    AdapterNotAllowed { adapter: String },

    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
    #[error("adapter '{adapter}' does not provide required capability '{capability}'")]
    CapabilityMissing { adapter: String, capability: String },

    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
    #[error(
        "adapter '{adapter}' declares native worker discovery, but rollout gate \
         'native_discovery_reviewed' is unresolved"
    )]
    NativeDiscoveryUnacknowledged { adapter: String },

    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
    #[error("{scope} cost ceiling ${ceiling:.2} reached; ${spent:.2} already spent")]
    CostCeilingExceeded {
        scope: &'static str,
        ceiling: f64,
        spent: f64,
    },

    /// Deprecated: no longer produced since crew v2 (org-governance
    /// enforcement retired, crew-v2 gap-closure WP5).
    #[error(
        "adapter '{adapter}' reports no usage, so the configured cost ceiling \
         cannot be enforced for it"
    )]
    CostCeilingUnenforceable { adapter: String },
}

/// The policy evaluator: implements [`AdapterAuthorization`] and enforces
/// concurrency ceilings and nested worker policies.
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
}

impl PolicyEvaluator {
    /// Creates a new `PolicyEvaluator` from a [`RuntimePolicy`].
    #[must_use]
    pub fn new(policy: RuntimePolicy) -> Self {
        Self {
            policy,
            active_runs: Arc::new(AtomicU32::new(0)),
            allow_nested: false, // default: deny nested
        }
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
    /// considering the concurrency ceiling and nested worker policy.
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
        // Retained for signature stability and for any future check that
        // consumes the worker profile or the conformance-downgraded
        // effective capability set -- unused today because the checks
        // that read them (model allowlist, required capabilities) were
        // config-sourced org governance, now retired (crew-v2
        // gap-closure WP5 ruling; see the module doc).
        _profile: &WorkerProfile,
        _effective_capabilities: &AdapterCapabilities,
        is_nested: bool,
        policy: Option<&RuntimePolicy>,
    ) -> Result<(), PolicyError> {
        let policy = policy.unwrap_or(&self.policy);

        // Check nested worker policy.
        if is_nested && !self.allow_nested {
            return Err(PolicyError::NestedWorkerDenied {
                reason: "nested workers are not allowed by policy".to_string(),
            });
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
    fn test_policy_evaluator_allows_any_model_org_governance_retired() {
        let policy = test_policy();
        let evaluator = PolicyEvaluator::new(policy);
        let profile = test_profile("any-model-at-all");
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

    /// The maximally-capable end of every field -- deliberately a
    /// different literal from [`test_capabilities`], so this reads
    /// unambiguously as "as much declared as an adapter could possibly
    /// claim", not a coincidental match.
    fn fully_declared_capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            protocol: ProtocolKind::Structured,
            resume: ResumeCapability::Turn,
            steering: SteeringCapability::ActiveTurn,
            approvals: ApprovalsCapability::Controllable,
            structured_result: true,
            usage: UsageCapability::PerChild,
            nested: NestedCapability::Managed,
            native_view: NativeViewCapability::IndependentTui,
            workspace_control: WorkspaceControlCapability::Write,
            durability: DurabilityCapability::VendorResumable,
        }
    }

    /// The maximally-stripped end of every field that has a "none"
    /// variant; the two fields with no such variant (`workspace_control`,
    /// `durability`) and the two-valued `protocol` are simply set to
    /// whichever value differs from [`fully_declared_capabilities`]'s
    /// choice -- as far apart as the type allows.
    fn fully_stripped_capabilities() -> AdapterCapabilities {
        AdapterCapabilities {
            protocol: ProtocolKind::Terminal,
            resume: ResumeCapability::None,
            steering: SteeringCapability::None,
            approvals: ApprovalsCapability::None,
            structured_result: false,
            usage: UsageCapability::None,
            nested: NestedCapability::None,
            native_view: NativeViewCapability::None,
            workspace_control: WorkspaceControlCapability::ReadOnly,
            durability: DurabilityCapability::ParentScoped,
        }
    }

    /// WP-B (b1): `authorize()` must be invariant to `effective_capabilities`
    /// -- with NO exception clause, not even for `nested`. This holds today
    /// only because `authorize()` (via `evaluate()`) ignores the parameter
    /// entirely, and this test exists to FAIL the instant that stops being
    /// true: the moment someone adds the first real capability check here,
    /// this test breaks, which is exactly when the deny-on-unproven
    /// constraint documented on [`crate::adapter::AdapterAuthorization::authorize`]'s
    /// doc comment becomes binding.
    #[test]
    fn authorize_is_invariant_to_effective_capabilities_with_no_exception() {
        let declared_evaluator = PolicyEvaluator::new(test_policy());
        let stripped_evaluator = PolicyEvaluator::new(test_policy());
        let profile = test_profile("any-model-at-all");

        let declared_result =
            declared_evaluator.authorize(&profile, &fully_declared_capabilities(), None);
        let stripped_result =
            stripped_evaluator.authorize(&profile, &fully_stripped_capabilities(), None);

        assert!(declared_result.is_ok(), "{declared_result:?}");
        assert_eq!(
            declared_result, stripped_result,
            "authorize() must return the identical result for a fully-declared and a \
             fully-stripped AdapterCapabilities -- it reads capabilities zero times today"
        );
    }
}
