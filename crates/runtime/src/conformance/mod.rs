//! The adapter conformance runner: fixture (default, always safe, zero
//! model calls) and live scenario suites that decide which of an adapter's
//! *declared* capabilities are actually *effective* -- the only set
//! `crate::adapter::registry::AdapterRegistry` and `crewd adapters --json`
//! may ever expose to OMP.
//!
//! Each vendor owns its own scenario implementations in a `*_conformance`
//! module beside `adapter::tui` (`adapter::tui::claude_conformance`,
//! `adapter::tui::codex_conformance`, and so on), covering every name
//! in [`scenario::ALL`] exactly once. This module only dispatches and
//! defines the shared report shape -- it never itself decides pass/fail
//! for any adapter's scenario.
//!
//! **Dispatch axis (crew-v2 gap-closure WP-C):** the headless control plane
//! this module used to dispatch to (`(`[`AdapterKind`]`, `[`AdapterMode`]`)`,
//! with `Headless` reaching each adapter's own headless `conformance`
//! submodule) is retired -- `mode: "headless"` stays deserializable but is
//! typed-rejected before it ever reaches conformance dispatch (spec §4.6).
//! [`run_fixture_conformance`] and [`run_live_conformance`] both only ever
//! reach `adapter::tui::{claude,codex,copilot,omp}_conformance` now; the
//! `AdapterMode` parameter each still takes exists solely so a caller-side
//! `Headless` request panics/errors loudly at the dispatch boundary rather
//! than silently degrading. `probe_availability` stays kind-only
//! (deliberately -- a vendor's installed CLI version does not vary by how
//! this runtime chooses to invoke it).
//!
//! **Gating model.** Vendor CLIs are ordinary installed dependencies, and
//! which adapters a run may use is decided by org policy
//! (`crate::config::RuntimePolicy::allowed_adapters`) plus the real
//! availability probe -- never by an environment variable a deployment has
//! to remember to set. So real invocation is the default, and the single
//! opt-out [`DISABLE_VENDOR_CLI_ENV`] forbids only the vendor processes
//! this runtime would spawn purely to *observe* a CLI.

pub mod report;
pub mod scenario;

pub use report::{ConformanceMode, ConformanceReport, ScenarioOutcome, ScenarioResult};

use crate::adapter::{AdapterKind, AdapterMode};
use crate::env_flag::env_flag;

/// Set to `"1"` to forbid every vendor-CLI process this runtime would
/// spawn purely to *observe* the CLI -- conformance live *and fixture*
/// suites and the availability probe. A development and CI switch only:
/// production leaves it unset, and which adapters a run may use is
/// decided by org policy (`RuntimePolicy::allowed_adapters`), not by
/// this variable.
///
/// It deliberately does **not** gate `Adapter::start()`: run execution is
/// authorized by policy, so a development switch must never be able to
/// silently stop production work.
pub const DISABLE_VENDOR_CLI_ENV: &str = "CREW_DISABLE_VENDOR_CLI";

/// The pre-rename name for [`DISABLE_VENDOR_CLI_ENV`], still honored as a
/// fallback so an existing shell or CI job keeps working unchanged.
pub const DISABLE_VENDOR_CLI_ENV_LEGACY: &str = "BATMAN_DISABLE_VENDOR_CLI";

/// Whether observation-only vendor-CLI invocation is disabled.
#[must_use]
pub fn vendor_cli_invocation_disabled() -> bool {
    env_flag(DISABLE_VENDOR_CLI_ENV, DISABLE_VENDOR_CLI_ENV_LEGACY).as_deref() == Some("1")
}

/// An honest, non-spawning result for a scenario that can only be proven by
/// a real vendor-CLI spawn, for use when [`vendor_cli_invocation_disabled`]
/// is set. The outcome is [`ScenarioOutcome::Skipped`]: neither proof nor
/// disproof. So a development kill switch can never downgrade the capability
/// a scenario gates (R68) and never fabricates one (R52).
#[must_use]
pub fn vendor_cli_required_scenario(name: &'static str) -> ScenarioResult {
    ScenarioResult::skip(
        name,
        format!(
            "skipped: real vendor CLI invocation is disabled ({DISABLE_VENDOR_CLI_ENV}=1); this \
             scenario has no fixture-only proof and can only run via live_report \
             ({DISABLE_VENDOR_CLI_ENV} unset)"
        ),
    )
}

/// The exact detail [`probe_availability`] uses when the kill switch skips a
/// PROBE. Like every other skipped scenario it is
/// [`ScenarioOutcome::Skipped`] -- an unattempted probe is neither evidence
/// for nor against the CLI, so it denies nothing (only a real disproof does)
/// and fabricates nothing.
#[must_use]
pub fn vendor_cli_skipped_probe() -> ScenarioResult {
    ScenarioResult::skip(
        scenario::PROBE,
        format!("vendor CLI probe skipped: {DISABLE_VENDOR_CLI_ENV}=1"),
    )
}
/// Why a scenario's real-vendor precondition could not be met: the runtime
/// deliberately declined to spawn, or a real attempt was made and failed.
///
/// Adapters carry this instead of a bare `String` so the distinction survives
/// the trip back to the [`ScenarioResult`] that a capability downgrade reads.
#[derive(Debug, Clone)]
pub enum VendorUnavailable {
    /// [`vendor_cli_invocation_disabled`] forbade the spawn: no evidence either way.
    Skipped(String),
    /// A real attempt was made and it failed.
    Failed(String),
}

impl VendorUnavailable {
    /// The kill-switch refusal, worded once for every adapter.
    #[must_use]
    pub fn disabled(what: &str) -> Self {
        Self::Skipped(format!(
            "skipped: {what} requires a real vendor-CLI spawn, which {DISABLE_VENDOR_CLI_ENV}=1 \
             forbids; run it via live_report ({DISABLE_VENDOR_CLI_ENV} unset)"
        ))
    }

    /// The human-readable detail, for whichever variant this is.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Skipped(d) | Self::Failed(d) => d,
        }
    }

    /// Turns this into the scenario outcome it actually justifies.
    #[must_use]
    pub fn into_scenario(self, name: &'static str) -> ScenarioResult {
        match self {
            Self::Skipped(detail) => ScenarioResult::skip(name, detail),
            Self::Failed(detail) => ScenarioResult::fail(name, detail),
        }
    }
}

/// How many times a fixture suite actually ran (WP26's cache must collapse
/// repeated submits onto one run). Test-only observability.
#[cfg(test)]
pub(crate) static FIXTURE_SUITE_RUNS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Every test in this crate's unit-test binary that reads OR mutates
/// [`FIXTURE_SUITE_RUNS`] (directly, or indirectly by calling
/// [`run_fixture_conformance`], which always increments it) must hold this
/// lock for the duration -- `cargo test` runs a lib target's unit tests
/// concurrently across threads by default, and this counter is one
/// process-global shared by more than one test module
/// (`adapter::registry::conformance_cache_tests` and this module's own
/// `tests`). A module-local guard is not enough: it only serializes that
/// module's own tests against each other, not against a different
/// module's concurrently-running ones.
#[cfg(test)]
pub(crate) static FIXTURE_SUITE_RUNS_SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Runs one adapter kind's full fixture conformance suite (never a model
/// call) and returns its report.
///
/// **Dispatched by `(kind, mode)` (crew-v2 gap-closure WP-B), but only
/// `AdapterMode::Tui` is reachable now (WP-C):** every call reaches
/// `adapter::tui::{claude,codex,copilot,omp}_conformance`'s fixture
/// suites -- the suites that feed `fixture-mode-baseline.json`'s
/// `claude-tui`/`codex-tui`/`copilot-tui`/`omp-tui` entries, and the only
/// ones CI's fixture-mode conformance signal has been sourced from since
/// WP-C. `mode` still takes the full `AdapterMode` enum (not just `Tui`)
/// so a caller-side `Headless` request fails loudly right here (see the
/// `unreachable!` arm below) rather than the parameter silently narrowing
/// away the caller's mistake at the type level.
pub async fn run_fixture_conformance(kind: AdapterKind, mode: AdapterMode) -> ConformanceReport {
    #[cfg(test)]
    FIXTURE_SUITE_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    match mode {
        // crew-v2 gap-closure WP-C: the headless control plane is retired
        // (spec §4.6) and its four adapters' `conformance` submodules are
        // deleted -- there is nothing left to dispatch a `Headless`
        // request to. Every caller must reject `Headless` before ever
        // reaching this function: `gate_profile` does so for a live
        // submit/resume, and `cli.rs`'s `run_conformance` does so for
        // `crewd conformance --fixture --mode headless` (a typed
        // rejection there, not silently accepted-and-discarded -- WP-B
        // M-1 rider). Reaching this arm at all means one of those callers
        // regressed; panicking loudly here is far more honest than
        // fabricating a report for a control plane that no longer exists.
        AdapterMode::Headless => unreachable!(
            "run_fixture_conformance called with the retired Headless mode for {kind} -- the \
             caller must reject Headless before calling this function"
        ),
        AdapterMode::Tui => {
            use crate::adapter::tui::{
                claude_conformance, codex_conformance, copilot_conformance, omp_conformance,
            };
            match kind {
                AdapterKind::Claude => claude_conformance::fixture_report().await,
                AdapterKind::Codex => codex_conformance::fixture_report().await,
                AdapterKind::Copilot => copilot_conformance::fixture_report().await,
                AdapterKind::OmpRpc => omp_conformance::fixture_report().await,
            }
        }
    }
}

/// Runs one adapter kind's live conformance suite against its installed
/// vendor CLI. Each adapter's `live_report` self-checks
/// [`vendor_cli_invocation_disabled`] and returns `Err` when the kill
/// switch is set, so this function needs no gating of its own.
///
/// # Errors
/// Returns a plain message when the kill switch is set or the installed
/// vendor CLI is unavailable.
pub async fn run_live_conformance(
    kind: AdapterKind,
    tui: bool,
) -> Result<ConformanceReport, String> {
    use crate::adapter::tui::{
        claude_conformance, codex_conformance, copilot_conformance, omp_conformance,
    };
    if tui {
        // The adapter now defaults to TUI mode: spawn the real vendor binary
        // on a PTY and prove transcript discovery + normalized tailing.
        match kind {
            AdapterKind::Claude => claude_conformance::live_report().await,
            AdapterKind::Codex => codex_conformance::live_report().await,
            AdapterKind::Copilot => copilot_conformance::live_report().await,
            AdapterKind::OmpRpc => omp_conformance::live_report().await,
        }
    } else {
        // crew-v2 gap-closure WP-C: the headless control plane is retired
        // (spec §4.6) and its four adapters' `conformance` submodules are
        // deleted. `cli.rs`'s `run_conformance` already rejects `--mode
        // headless` before ever calling this function with `tui: false`;
        // this `Err` is the defense-in-depth boundary for any other
        // caller that might still pass it.
        Err(format!(
            "adapter {kind} was requested with the retired headless control plane (spec §4.6) \
             -- the headless control plane has no adapter implementation to dispatch to; use \
             the TUI live report instead"
        ))
    }
}

/// How long a probe result stays fresh. Long enough that a burst of run
/// submits re-spawns no binary, short enough that installing or
/// authenticating a CLI is picked up without restarting the daemon.
const PROBE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Caches [`probe_availability`] results -- and the vendor-CLI version the
/// probe observed -- by adapter kind. Mirrors
/// `crate::display::herdr::HerdrDisplay::probe`'s cache exactly: the guard
/// is dropped before the `await` and re-taken to store, so it is never held
/// across a suspension point.
type ProbeCacheEntry = (std::time::Instant, ScenarioResult, Option<String>);

static PROBE_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<AdapterKind, ProbeCacheEntry>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Probes the installed vendor CLI for `kind` -- version handshake only,
/// never a model call -- cached for 60 seconds so repeated run submits do
/// not re-spawn the binary.
///
/// Honors [`DISABLE_VENDOR_CLI_ENV`] **permissively**: when the switch is
/// set this returns an honest *skipped* result without spawning anything,
/// and does not cache it. A skip rather than a fail is deliberate -- the
pub async fn probe_availability(kind: AdapterKind) -> ScenarioResult {
    probe_availability_with_version(kind).await.0
}

/// [`probe_availability`] plus the vendor-CLI version the probe (or a
/// fresh-enough cached probe) observed -- the stamp
/// `crate::adapter::registry`'s fixture-suite cache validates against.
/// `None` under the kill switch (nothing was spawned, nothing observed).
pub async fn probe_availability_with_version(
    kind: AdapterKind,
) -> (ScenarioResult, Option<String>) {
    if vendor_cli_invocation_disabled() {
        return (vendor_cli_skipped_probe(), None);
    }

    {
        let cache = PROBE_CACHE.lock();
        if let Some((observed_at, result, version)) = cache.get(&kind)
            && observed_at.elapsed() < PROBE_CACHE_TTL
        {
            return (result.clone(), version.clone());
        }
    }

    // crew-v2 gap-closure WP-C: dispatches to each TUI vendor's own
    // lightweight `--version` probe now (the headless ones, and the
    // adapters they belonged to, are deleted). Deliberately still
    // kind-only, not mode-aware: a vendor's installed CLI version does
    // not vary by how this runtime chooses to invoke it, and Tui is the
    // only mode that reaches live dispatch at all post-retirement.
    use crate::adapter::tui::{
        claude_conformance, codex_conformance, copilot_conformance, omp_conformance,
    };
    let (result, version) = match kind {
        AdapterKind::Claude => claude_conformance::probe_with_version().await,
        AdapterKind::Codex => codex_conformance::probe_with_version().await,
        AdapterKind::Copilot => copilot_conformance::probe_with_version().await,
        AdapterKind::OmpRpc => omp_conformance::probe_with_version().await,
    };

    PROBE_CACHE.lock().insert(
        kind,
        (std::time::Instant::now(), result.clone(), version.clone()),
    );
    (result, version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::NestedCapability;

    const ALL_KINDS: [AdapterKind; 4] = [
        AdapterKind::Claude,
        AdapterKind::Codex,
        AdapterKind::Copilot,
        AdapterKind::OmpRpc,
    ];

    /// crew-v2 gap-closure WP-C: `run_fixture_conformance(kind,
    /// Headless)` is now a defense-in-depth panic, not a working dispatch
    /// (the headless adapters it used to reach are deleted). Supersedes
    /// WP-B's `every_adapter_kind_produces_a_headless_fixture_report`,
    /// which asserted the opposite.
    #[tokio::test]
    #[should_panic(expected = "retired Headless mode")]
    async fn run_fixture_conformance_panics_on_headless_not_dispatches_to_it() {
        let _serial = FIXTURE_SUITE_RUNS_SERIAL.lock().await;
        let _ = run_fixture_conformance(AdapterKind::Claude, AdapterMode::Headless).await;
    }

    /// WP-B Task 1: the `Tui` half of the new dispatch axis reaches the
    /// tui conformance suites, which name their report with the exact
    /// `fixture-mode-baseline.json` keys -- `ompRpc`'s is `"omp-tui"`, not
    /// a mechanical `<wire_name>-tui` (verified against each
    /// `*_conformance.rs`'s own `AdapterKindLabel::custom` call).
    #[tokio::test]
    async fn every_adapter_kind_produces_a_tui_fixture_report() {
        let _serial = FIXTURE_SUITE_RUNS_SERIAL.lock().await;
        for (kind, expected_label) in [
            (AdapterKind::Claude, "claude-tui"),
            (AdapterKind::Codex, "codex-tui"),
            (AdapterKind::Copilot, "copilot-tui"),
            (AdapterKind::OmpRpc, "omp-tui"),
        ] {
            let report = run_fixture_conformance(kind, AdapterMode::Tui).await;
            assert_eq!(report.adapter, expected_label);
            assert_eq!(report.mode, ConformanceMode::Fixture);
            assert!(
                !report.scenarios.is_empty(),
                "{kind} tui fixture report must run at least one scenario"
            );
        }
    }

    /// WP-B ruling deliverable (b2): `nested != NestedCapability::Managed`
    /// gates `DomainAdapterEventSink` construction
    /// (`adapter::registry.rs:469`/`:792`), NOT authorization -- but it is
    /// still a load-bearing tautology today, since no in-tree adapter (in
    /// either mode) declares `Managed`. Pin it explicitly so the first
    /// vendor that flips it changes the nested-observation/write-violation
    /// paths visibly and deliberately, not silently.
    #[tokio::test]
    async fn no_in_tree_adapter_declares_managed_nested_in_either_mode() {
        // crew-v2 gap-closure WP-C: only `Tui` reaches live dispatch now
        // (`Headless` is a defense-in-depth panic, pinned separately by
        // `run_fixture_conformance_panics_on_headless_not_dispatches_to_it`).
        let _serial = FIXTURE_SUITE_RUNS_SERIAL.lock().await;
        for kind in ALL_KINDS {
            let report = run_fixture_conformance(kind, AdapterMode::Tui).await;
            assert_ne!(
                report.effective_capabilities.nested,
                NestedCapability::Managed,
                "{kind} declares Managed nested -- DomainAdapterEventSink's nested_not_managed \
                 flag construction must be revisited now that this is no longer vacuously true"
            );
        }
    }
}
