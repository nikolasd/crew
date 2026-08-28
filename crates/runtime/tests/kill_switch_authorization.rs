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
//! Crew-v2 gap-closure WP-C ruling: fixture mode is TUI-sourced now (spec
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
//! deeper claim R68 is actually about -- that a *gated* scenario's skip,
//! specifically, must never be read as a disproof -- has no live end-to-end
//! trigger left in fixture mode and is proven synthetically instead, at
//! the unit level: `conformance::report`'s
//! `a_skip_never_masks_a_real_disproof_of_a_different_gate` inline test.
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

use crew_runtime::adapter::{AdapterKind, AdapterMode};
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
        let report = run_fixture_conformance(kind, AdapterMode::Tui).await;
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
             vendor-CLI check), or the R68 proof above is vacuous: skipped={skipped:?}"
        );
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
