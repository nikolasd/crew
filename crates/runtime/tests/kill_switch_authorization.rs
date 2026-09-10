//! Regression proof: with `CREW_DISABLE_VENDOR_CLI=1` set, a
//! development-only kill switch must never shrink the *effective*
//! capabilities a conformance report proves. An unattempted scenario is
//! reported [`crew_runtime::conformance::ScenarioOutcome::Skipped`] --
//! neither proof nor disproof -- so `effective_capabilities` equals
//! `declared_capabilities`. Before the fix, an unattempted
//! scenario was reported `Fail`, which a capability gate read as a
//! disproof and stripped `steering`/`resume`.
//!
//! This file used to carry a phase 3,
//! proving the *policy* consequence end to end -- that a
//! `PolicyEvaluator` policy requiring `steering`+`resume` (via
//! `required_capabilities`) still authorized Codex under the switch. That
//! config-sourced required-capability check is retired along with the
//! rest of org-governance enforcement (see `policy::evaluate`'s module
//! doc), so there is no longer an authorization path for it to prove.
//! What remains -- and is the actual, capability-downgrade-consuming
//! invariant this file proves -- is phase 1/2 below: the conformance report
//! itself, independent of any policy, never lets the switch corrupt
//! `effective_capabilities`.
//!
//! Fixture mode is TUI-sourced now (spec
//! §4.6) -- `run_fixture_conformance` only ever reaches
//! `adapter::tui::*_conformance`, whose golden-fixture scenarios need no
//! live vendor process for anything except the real `--version`/binary
//! check `probe` makes. So `PROBE` -- which gates no capability -- is the
//! *only* scenario the switch skips in fixture mode now; every capability-
//! gating scenario (`approval`, `follow_up`, `session_resume`,
//! `isolated_write`, `managed_nesting_rejection`) passes from golden data
//! regardless of the switch. Phase 2 below therefore proves a narrower
//! claim than it did against the headless control plane: that the one
//! scenario the switch *does* skip carries no capability consequence, so
//! `effective_capabilities` trivially equals `declared_capabilities`. The
//! deeper claim this invariant is actually about -- that a *gated*
//! scenario's skip, specifically, must never be read as a disproof -- has
//! no live end-to-end trigger left in fixture mode and is proven
//! synthetically instead, at the unit level: `conformance::report`'s
//! `a_skipped_scenario_leaves_its_gated_capability_declared` inline test
//! (the direct, single-gate proof; its sibling
//! `a_skip_never_masks_a_real_disproof_of_a_different_gate` additionally
//! proves a skip on one gate never masks a genuine disproof on another).
//!
//! This file contains exactly **one** test that touches the process
//! environment. It mutates the process-global `CREW_DISABLE_VENDOR_CLI`
//! variable, which `std::env::set_var` may only change soundly while no
//! other thread is running (edition 2024 makes it `unsafe` for precisely
//! this reason); `cargo test` runs `#[test]` functions in a binary
//! concurrently, so a single `#[tokio::test]` with the phases sequenced
//! inside it is the only sound shape for that mutation -- the same
//! argument as `vendor_cli_availability.rs`. `outcome_diff_tests` below
//! adds ordinary, environment-independent unit tests of `outcome_diff`
//! itself; none of them read or write `CREW_DISABLE_VENDOR_CLI`, so they
//! carry none of that hazard and may run concurrently with each other
//! and with the one test that does.
//!
//! Never invokes a model: `run_fixture_conformance` is the zero-model-call
//! fixture suite.

use crew_runtime::adapter::{AdapterKind, AdapterMode};
use crew_runtime::conformance::{
    ConformanceReport, DISABLE_VENDOR_CLI_ENV, ScenarioOutcome, run_fixture_conformance, scenario,
};

/// Every scenario in `report` whose outcome differs from what the kill
/// switch is expected to produce -- every scenario `Pass`es except
/// `PROBE`, which the switch forces to `Skipped` -- as one line per
/// scenario: `"name: expected X, observed Y -- detail"`. Empty when the
/// report matches expectation exactly.
///
/// This is what a failure here actually needs: which scenario broke and
/// why, not the two capability structs `effective_capabilities` and
/// `declared_capabilities` corrupt into differing -- a reader diffing
/// those by eye learns that SOMETHING failed, never which scenario or
/// what it observed.
fn outcome_diff(report: &ConformanceReport) -> String {
    let lines: Vec<String> = report
        .scenarios
        .iter()
        .filter_map(|s| {
            let expected = if s.name == scenario::PROBE {
                ScenarioOutcome::Skipped
            } else {
                ScenarioOutcome::Pass
            };
            (s.outcome != expected).then(|| {
                format!(
                    "{}: expected {expected:?}, observed {:?} -- {}",
                    s.name, s.outcome, s.detail
                )
            })
        })
        .collect();
    if lines.is_empty() {
        "(no scenario outcome differs from expectation)".to_string()
    } else {
        lines.join("\n")
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
        let report = run_fixture_conformance(kind, AdapterMode::Tui).await;
        let skipped: Vec<(&str, String)> = report
            .scenarios
            .iter()
            .filter(|s| s.was_skipped())
            .map(|s| (s.name, s.detail.clone()))
            .collect();
        assert_eq!(
            report.effective_capabilities,
            report.declared_capabilities,
            "{kind}: a skipped (unattempted) scenario must never downgrade a capability -- \
             scenarios that differ from expectation:\n{}",
            outcome_diff(&report)
        );
        if !skipped.is_empty() {
            any_skipped = true;
        }
        // TUI fixture mode's only live dependency is `probe`'s real
        // `--version`/binary check (see the module doc) -- confirming it is
        // genuinely the scenario the switch skipped, not some other one, is
        // what makes `any_skipped` below a real proof of *this* invariant
        // rather than a vacuous one that would also pass if the switch
        // skipped nothing at all.
        let skipped_names: std::collections::HashSet<&str> =
            skipped.iter().map(|(name, _)| *name).collect();
        assert!(
            skipped_names.contains(scenario::PROBE),
            "{kind}: PROBE must report skipped under the switch (its own real \
             vendor-CLI check), or the proof above is vacuous: skipped={skipped:?}"
        );
    }
    // At least one adapter must carry a genuinely skipped scenario -- not
    // merely `!report.passed`, which a real, unrelated Fail could also
    // trigger with zero skips present -- or the effective==declared
    // assertions above would be vacuous with respect to this invariant's
    // actual claim.
    assert!(
        any_skipped,
        "at least one adapter must report a was_skipped() scenario while the \
         kill switch is set, or this test proves nothing about Skipped \
         semantics specifically"
    );

    unsafe { std::env::remove_var(DISABLE_VENDOR_CLI_ENV) };
}

#[cfg(test)]
mod outcome_diff_tests {
    use super::*;
    use crew_runtime::adapter::{
        AdapterCapabilities, ApprovalsCapability, DurabilityCapability, NativeViewCapability,
        NestedCapability, ProtocolKind, ResumeCapability, SteeringCapability, UsageCapability,
        WorkspaceControlCapability,
    };
    use crew_runtime::conformance::report::AdapterKindLabel;
    use crew_runtime::conformance::{ConformanceMode, ScenarioResult};

    fn minimal_capabilities() -> AdapterCapabilities {
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

    fn report(scenarios: Vec<ScenarioResult>) -> ConformanceReport {
        ConformanceReport::new(
            AdapterKindLabel::from(AdapterKind::Claude),
            ConformanceMode::Fixture,
            None,
            minimal_capabilities(),
            scenarios,
        )
    }

    /// Verifies the instrument on a case known to match expectation
    /// exactly (PROBE skipped, everything else passing) before trusting
    /// it on a real failure -- a positive control.
    #[test]
    fn a_report_matching_expectation_exactly_diffs_to_nothing() {
        let report = report(vec![
            ScenarioResult::skip(scenario::PROBE, "vendor CLI probe skipped"),
            ScenarioResult::pass(scenario::FOLLOW_UP, "ok"),
        ]);
        assert_eq!(
            outcome_diff(&report),
            "(no scenario outcome differs from expectation)"
        );
    }

    /// The negative control this instrument exists for: a scenario that
    /// failed (not skipped) must show up by name, with its own detail,
    /// not be lost inside a capability-struct diff.
    #[test]
    fn a_failed_scenario_names_itself_and_carries_its_detail() {
        let report = report(vec![
            ScenarioResult::skip(scenario::PROBE, "vendor CLI probe skipped"),
            ScenarioResult::fail(
                scenario::FOLLOW_UP,
                "deadline elapsed waiting for the double's acknowledgement",
            ),
        ]);
        assert_eq!(
            outcome_diff(&report),
            format!(
                "{}: expected Pass, observed Fail -- deadline elapsed waiting for the double's \
                 acknowledgement",
                scenario::FOLLOW_UP
            )
        );
    }

    /// PROBE itself is held to the opposite expectation from every other
    /// scenario: passing it (not skipping it) is what must be flagged
    /// here, since that would mean the kill switch had no effect at all.
    #[test]
    fn probe_passing_instead_of_skipping_is_itself_a_diff() {
        let report = report(vec![ScenarioResult::pass(
            scenario::PROBE,
            "the real vendor CLI answered --version",
        )]);
        assert_eq!(
            outcome_diff(&report),
            format!(
                "{}: expected Skipped, observed Pass -- the real vendor CLI answered --version",
                scenario::PROBE
            )
        );
    }
}
