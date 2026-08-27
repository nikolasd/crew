//! The adapter conformance runner: fixture (default, always safe, zero
//! model calls) and live scenario suites that decide which of an adapter's
//! *declared* capabilities are actually *effective* -- the only set
//! `crate::adapter::registry::AdapterRegistry` and `crewd adapters --json`
//! may ever expose to OMP.
//!
//! Each adapter owns its own scenario implementations in a `conformance`
//! submodule beside its `mod.rs` (`crate::adapter::claude::conformance`,
//! `crate::adapter::codex::conformance`, and so on), covering every name
//! in [`scenario::ALL`] exactly once. This module only dispatches and
//! defines the shared report shape -- it never itself decides pass/fail
//! for any adapter's scenario.
//!
//! **Dispatch axis (crew-v2 gap-closure WP-B):** [`run_fixture_conformance`]
//! dispatches by `(`[`AdapterKind`]`, `[`AdapterMode`]`)`, not `AdapterKind`
//! alone -- `Headless` reaches each adapter's own `conformance` submodule
//! (the four kept headless adapters); `Tui` reaches
//! `adapter::tui::{claude,codex,copilot,omp}_conformance` instead, whose
//! fixture suites declare a materially different profile (their vendor's
//! *TUI* adapter, not its headless one). [`run_live_conformance`] already
//! took a `tui: bool` before this WP; the two now agree on which control
//! plane's suite a caller means. `probe_availability` stays kind-only
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

pub mod capture;
pub mod report;
pub mod scenario;
pub mod scrub;

pub use report::{ConformanceMode, ConformanceReport, ScenarioOutcome, ScenarioResult};

use crate::adapter::{AdapterCapabilities, AdapterKind, AdapterMode};
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
/// call) for the given control plane and returns its report.
///
/// **Dispatched by `(kind, mode)`, not `kind` alone (crew-v2 gap-closure
/// WP-B):** `AdapterMode::Headless` reaches the four kept headless
/// adapters' own `conformance` submodules (unchanged); `AdapterMode::Tui`
/// reaches `adapter::tui::{claude,codex,copilot,omp}_conformance`'s
/// fixture suites instead -- the suites that actually feed
/// `fixture-mode-baseline.json`'s `claude-tui`/`codex-tui`/`copilot-tui`/
/// `omp-tui` entries. Before WP-B, this function dispatched by `kind`
/// only, so a `mode: "tui"` run was authorized (via
/// `adapter::registry::gate_profile`) against its vendor's *headless*
/// effective capabilities -- a materially different declared profile
/// (`ProtocolKind::Terminal` vs. `Structured`, for one) than the
/// `TuiAdapter` actually constructed for it. Every caller of this function
/// that is not gating a specific run's own requested control plane (the
/// `crewd conformance`/`adapters --json` CLI surfaces, the fixture
/// capture tool) still passes `AdapterMode::Headless` explicitly, keeping
/// their behavior unchanged -- CI's fixture-mode conformance signal stays
/// headless-sourced until WP-C deliberately substitutes it with the four
/// `*-tui` suites.
pub async fn run_fixture_conformance(kind: AdapterKind, mode: AdapterMode) -> ConformanceReport {
    #[cfg(test)]
    FIXTURE_SUITE_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    match mode {
        AdapterMode::Headless => match kind {
            AdapterKind::Claude => crate::adapter::claude::conformance::fixture_report().await,
            AdapterKind::Codex => crate::adapter::codex::conformance::fixture_report().await,
            AdapterKind::Copilot => crate::adapter::copilot::conformance::fixture_report().await,
            AdapterKind::OmpRpc => crate::adapter::omp_rpc::conformance::fixture_report().await,
        },
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
        // Headless path is still reachable (the four headless adapters are
        // kept): run each adapter's own headless live_report, labeled by its
        // plain kind name so the two modes never collide in one report set.
        match kind {
            AdapterKind::Claude => crate::adapter::claude::conformance::live_report().await,
            AdapterKind::Codex => crate::adapter::codex::conformance::live_report().await,
            AdapterKind::Copilot => crate::adapter::copilot::conformance::live_report().await,
            AdapterKind::OmpRpc => crate::adapter::omp_rpc::conformance::live_report().await,
        }
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

    let (result, version, _capabilities): (ScenarioResult, Option<String>, AdapterCapabilities) =
        match kind {
            AdapterKind::Claude => crate::adapter::claude::conformance::probe_scenario().await,
            AdapterKind::Codex => crate::adapter::codex::conformance::probe_scenario().await,
            AdapterKind::Copilot => crate::adapter::copilot::conformance::probe_scenario().await,
            AdapterKind::OmpRpc => crate::adapter::omp_rpc::conformance::probe().await,
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

    #[tokio::test]
    async fn every_adapter_kind_produces_a_headless_fixture_report() {
        // Every call here increments the process-global FIXTURE_SUITE_RUNS
        // -- held so a concurrently-running counter-sensitive test
        // elsewhere (`adapter::registry::conformance_cache_tests`) never
        // observes our increments as its own.
        let _serial = FIXTURE_SUITE_RUNS_SERIAL.lock().await;
        for kind in ALL_KINDS {
            let report = run_fixture_conformance(kind, AdapterMode::Headless).await;
            assert_eq!(report.adapter, kind.wire_name());
            assert_eq!(report.mode, ConformanceMode::Fixture);
            assert!(
                !report.scenarios.is_empty(),
                "{kind} headless fixture report must run at least one scenario"
            );
        }
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
        let _serial = FIXTURE_SUITE_RUNS_SERIAL.lock().await;
        for kind in ALL_KINDS {
            for mode in [AdapterMode::Headless, AdapterMode::Tui] {
                let report = run_fixture_conformance(kind, mode).await;
                assert_ne!(
                    report.effective_capabilities.nested,
                    NestedCapability::Managed,
                    "{kind} ({mode:?}) declares Managed nested -- DomainAdapterEventSink's \
                     nested_not_managed flag construction at registry.rs:469/:792 must be \
                     revisited now that this is no longer vacuously true"
                );
            }
        }
    }
}
