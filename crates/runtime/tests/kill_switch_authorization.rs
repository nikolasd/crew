//! The R68 regression proof: with `CREW_DISABLE_VENDOR_CLI=1` set, a
//! development-only kill switch must never shrink the *effective*
//! capabilities a conformance report proves. An unattempted scenario is
//! reported [`crew_runtime::conformance::ScenarioOutcome::Skipped`] --
//! neither proof nor disproof -- so `effective_capabilities` equals
//! `declared_capabilities`. Before the fix (REVIEW.md R68), an unattempted
//! scenario was reported `Fail`, which a capability gate read as a
//! disproof and stripped `steering`/`resume`.
//!
//! Crew-v2 gap-closure WP5 ruling: this file used to carry a phase 3,
//! proving the *policy* consequence end to end -- that a
//! `PolicyEvaluator` policy requiring `steering`+`resume` (via
//! `required_capabilities`) still authorized Codex under the switch. That
//! config-sourced required-capability check is retired along with the
//! rest of org-governance enforcement (see `policy::evaluate`'s module
//! doc), so there is no longer an authorization path for it to prove.
//! What remains -- and is the actual, capability-downgrade-consuming
//! invariant R68 is about -- is phase 1/2 below: the conformance report
//! itself, independent of any policy, never lets the switch corrupt
//! `effective_capabilities`.
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
//! fixture suite.

use crew_runtime::adapter::AdapterKind;
use crew_runtime::conformance::{DISABLE_VENDOR_CLI_ENV, run_fixture_conformance, scenario};

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
            // Codex's `requires_live_turn_scenario` skips `FOLLOW_UP`
            // (steering) and `SESSION_RESUME` (resume) under the switch --
            // confirming these two are genuinely exercised, not merely
            // present-but-passing or absent, is what makes the
            // effective==declared assertion above a real R68 proof for
            // Codex specifically rather than a vacuous one.
            let skipped_names: std::collections::HashSet<&str> =
                skipped.iter().map(|(name, _)| *name).collect();
            assert!(
                skipped_names.contains(scenario::FOLLOW_UP),
                "Codex must report FOLLOW_UP (steering) as skipped under the \
                 switch, or the R68 proof above is vacuous: skipped={skipped:?}"
            );
            assert!(
                skipped_names.contains(scenario::SESSION_RESUME),
                "Codex must report SESSION_RESUME (resume) as skipped under the \
                 switch, or the R68 proof above is vacuous: skipped={skipped:?}"
            );
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

    unsafe { std::env::remove_var(DISABLE_VENDOR_CLI_ENV) };
}
