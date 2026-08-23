//! The R68 regression proof: with `CREW_DISABLE_VENDOR_CLI=1` set, a
//! development-only kill switch must never shrink the *effective*
//! capabilities the production authorizer reads from a conformance report.
//! An unattempted scenario is reported
//! [`batman_runtime::conformance::ScenarioOutcome::Skipped`] -- neither proof
//! nor disproof -- so `effective_capabilities` equals `declared_capabilities`,
//! and a policy that requires the declared capabilities still authorizes the
//! run. Before the fix (REVIEW.md R68), an unattempted scenario was reported
//! `Fail`, which the capability gate read as a disproof and stripped
//! `steering`/`resume`, denying any run that required them -- the switch
//! itself never appearing in the error.
//!
//! This file deliberately contains exactly **one** test. It mutates the
//! process-global `CREW_DISABLE_VENDOR_CLI` variable, which
//! `std::env::set_var` may only change soundly while no other thread is
//! running (edition 2024 makes it `unsafe` for precisely this reason);
//! `cargo test` runs `#[test]` functions in a binary concurrently, so a
//! single `#[tokio::test]` with the phases sequenced inside it is the only
//! sound shape -- the same argument as `vendor_cli_availability.rs`.
//!
//! Never invokes a model: `run_fixture_conformance` is the zero-model-call
//! fixture suite, and the `authorize` call is pure policy evaluation.

use batman_runtime::adapter::{
    AdapterAuthorization, AdapterKind, CodexStartupOptions, ProfileId, StartupOptions,
    WorkerProfile,
};
use batman_runtime::config::{NestedViolationAction, RolloutGates, RuntimePolicy};
use batman_runtime::conformance::{DISABLE_VENDOR_CLI_ENV, run_fixture_conformance, scenario};
use batman_runtime::policy::PolicyEvaluator;
use batman_runtime::workspace::{DEFAULT_COPY_MAX_BYTES, DEFAULT_COPY_MAX_FILES};

/// A Codex profile. Codex is the strongest subject for this proof: its
/// `requires_live_turn` scenario cannot be proven in fixture mode (a bare
/// `thread/start` never invokes the model) *regardless of the switch*, so it
/// exercised the R68 defect -- stripping Codex's declared `steering`/`resume`
/// from `effective_capabilities` -- on every machine, switch set or not.
fn codex_profile() -> WorkerProfile {
    WorkerProfile {
        id: ProfileId::new(),
        adapter: "codex".to_string(),
        model: "gpt-5".to_string(),
        permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
        startup_options: StartupOptions::Codex(CodexStartupOptions::default()),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    }
}

/// A policy that requires the two capabilities R68 stripped under the switch
/// (`steering` + `resume`), with every other check permissive -- empty
/// allowlists, no cost ceilings, the discovery gate resolved, an uncapped
/// concurrency ceiling -- so the *only* way `authorize` can deny here is the
/// required-capability check against the conformance-proven effective set.
fn steering_and_resume_policy() -> RuntimePolicy {
    RuntimePolicy {
        merged: serde_json::json!({}),
        fingerprint: "test".to_string(),
        display_backend: "auto".to_string(),
        retention: "30d".to_string(),
        max_workers: 4,
        concurrency_ceiling: u32::MAX,
        allowed_models: vec![],
        allowed_adapters: vec![],
        cost_ceiling_per_run_usd: None,
        org_security_patterns: vec![],
        rollout_gates: RolloutGates {
            vendor_terms_accepted: true,
            retention_configured: true,
            model_allowlist_set: true,
            concurrency_explicit: true,
            native_discovery_reviewed: true,
            ornith_identity_set: true,
            nested_violation_action: NestedViolationAction::QuarantineAndCancel,
            allow_development_binary_override: false,
        },
        copy_max_bytes: DEFAULT_COPY_MAX_BYTES,
        copy_max_files: DEFAULT_COPY_MAX_FILES,
        required_capabilities: vec!["steering".to_string(), "resume".to_string()],
        cost_ceiling_daily_usd: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_kill_switch_never_shrinks_effective_capabilities() {
    // --- Phase 1: set the development kill switch -----------------------
    //
    // SAFETY: this binary holds exactly one test (see the module doc), and
    // `current_thread` keeps its async work on this same thread, so no other
    // thread can observe the environment mid-mutation.
    unsafe { std::env::set_var(DISABLE_VENDOR_CLI_ENV, "1") };

    let mut any_skipped = false;
    let mut codex_effective = None;

    // --- Phase 2: the switch must never strip a declared capability -----
    //
    // For every adapter, a scenario its fixture cannot attempt is `Skipped`,
    // so the effective set the registry gates on must equal the declared set.
    for kind in [
        AdapterKind::Claude,
        AdapterKind::Codex,
        AdapterKind::Copilot,
        AdapterKind::OmpRpc,
    ] {
        let report = run_fixture_conformance(kind).await;
        let skipped: Vec<(&str, String)> = report
            .scenarios
            .iter()
            .filter(|s| s.was_skipped())
            .map(|s| (s.name, s.detail.clone()))
            .collect();
        assert_eq!(
            report.effective_capabilities, report.declared_capabilities,
            "{kind}: a skipped (unattempted) scenario must never downgrade a \
             capability (R68); declared={:?} effective={:?} skipped={skipped:?}",
            report.declared_capabilities, report.effective_capabilities
        );
        if !skipped.is_empty() {
            any_skipped = true;
        }
        if kind == AdapterKind::Codex {
            // Phase 3 tests Codex's effective set against a policy requiring
            // `steering` + `resume`; that is only a real R68 proof if the two
            // scenarios gating those capabilities were actually skipped here
            // (Codex's `requires_live_turn_scenario` skips `FOLLOW_UP` and
            // `SESSION_RESUME` under the switch), not merely present-but-
            // passing or absent.
            let skipped_names: std::collections::HashSet<&str> =
                skipped.iter().map(|(name, _)| *name).collect();
            assert!(
                skipped_names.contains(scenario::FOLLOW_UP),
                "Codex must report FOLLOW_UP (steering) as skipped under the \
                 switch, or phase 3's policy check proves nothing: skipped={skipped:?}"
            );
            assert!(
                skipped_names.contains(scenario::SESSION_RESUME),
                "Codex must report SESSION_RESUME (resume) as skipped under the \
                 switch, or phase 3's policy check proves nothing: skipped={skipped:?}"
            );
            codex_effective = Some(report.effective_capabilities);
        }
    }
    // At least one adapter must carry a genuinely skipped scenario -- not
    // merely `!report.passed`, which a real, unrelated Fail could also
    // trigger with zero skips present -- or the effective==declared
    // assertions above would be vacuous with respect to R68's actual claim.
    assert!(
        any_skipped,
        "at least one adapter must report a was_skipped() scenario while the \
         kill switch is set, or this test proves nothing about Skipped \
         semantics specifically"
    );

    // --- Phase 3: a policy the switch once broke now still authorizes ---
    //
    // The end-to-end R68 proof: the effective set phase 2 proved is still
    // complete enough for the production authorizer to approve a run whose
    // policy requires the capabilities the switch used to strip.
    let effective = codex_effective.expect("the Codex report must be captured in phase 2");
    let policy = steering_and_resume_policy();
    let evaluator = PolicyEvaluator::new(policy.clone());
    let profile = codex_profile();
    let result = evaluator.authorize(&profile, &effective, Some(&policy));
    assert!(
        result.is_ok(),
        "with the switch set, Codex's effective capabilities must still satisfy a \
         policy requiring steering + resume (R68); got: {result:?}"
    );

    unsafe { std::env::remove_var(DISABLE_VENDOR_CLI_ENV) };
}
