//! The adapter conformance runner: fixture (default, always safe, zero
//! model calls) and live scenario suites that decide which of an adapter's
//! *declared* capabilities are actually *effective* -- the only set
//! `crate::adapter::registry::AdapterRegistry` and `crewd adapters --json`
//! may ever expose to OMP.
//!
//! Each adapter owns its own scenario implementations in a `conformance`
//! submodule beside its `mod.rs` (`crate::adapter::claude::conformance`,
//! `crate::adapter::codex::conformance`, and so on), covering every name
//! in [`scenario::ALL`] exactly once. This module only dispatches by
//! [`crate::adapter::AdapterKind`] and defines the shared report shape --
//! it never itself decides pass/fail for any adapter's scenario.
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

use crate::adapter::{AdapterCapabilities, AdapterKind};
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

/// Runs one adapter kind's full fixture conformance suite (never a model
/// call) and returns its report.
pub async fn run_fixture_conformance(kind: AdapterKind) -> ConformanceReport {
    match kind {
        AdapterKind::Claude => crate::adapter::claude::conformance::fixture_report().await,
        AdapterKind::Codex => crate::adapter::codex::conformance::fixture_report().await,
        AdapterKind::Copilot => crate::adapter::copilot::conformance::fixture_report().await,
        AdapterKind::OmpRpc => crate::adapter::omp_rpc::conformance::fixture_report().await,
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
pub async fn run_live_conformance(kind: AdapterKind) -> Result<ConformanceReport, String> {
    match kind {
        AdapterKind::Claude => crate::adapter::claude::conformance::live_report().await,
        AdapterKind::Codex => crate::adapter::codex::conformance::live_report().await,
        AdapterKind::Copilot => crate::adapter::copilot::conformance::live_report().await,
        AdapterKind::OmpRpc => crate::adapter::omp_rpc::conformance::live_report().await,
    }
}

/// How long a probe result stays fresh. Long enough that a burst of run
/// submits re-spawns no binary, short enough that installing or
/// authenticating a CLI is picked up without restarting the daemon.
const PROBE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Caches [`probe_availability`] results by adapter kind. Mirrors
/// `crate::display::herdr::HerdrDisplay::probe`'s cache exactly: the guard
/// is dropped before the `await` and re-taken to store, so it is never held
/// across a suspension point.
static PROBE_CACHE: std::sync::LazyLock<
    parking_lot::Mutex<
        std::collections::HashMap<AdapterKind, (std::time::Instant, ScenarioResult)>,
    >,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// Probes the installed vendor CLI for `kind` -- version handshake only,
/// never a model call -- cached for 60 seconds so repeated run submits do
/// not re-spawn the binary.
///
/// Honors [`DISABLE_VENDOR_CLI_ENV`] **permissively**: when the switch is
/// set this returns an honest *skipped* result without spawning anything,
/// and does not cache it. A skip rather than a fail is deliberate -- the
/// switch is a development and CI convenience, and only a real disproof
/// (an actual failed probe) may deny; a skip never does. A skip rather
/// than a pass is equally deliberate: a probe that never ran is no
/// evidence the CLI works.
pub async fn probe_availability(kind: AdapterKind) -> ScenarioResult {
    if vendor_cli_invocation_disabled() {
        return vendor_cli_skipped_probe();
    }

    {
        let cache = PROBE_CACHE.lock();
        if let Some((observed_at, result)) = cache.get(&kind)
            && observed_at.elapsed() < PROBE_CACHE_TTL
        {
            return result.clone();
        }
    }

    let (result, _version, _capabilities): (ScenarioResult, Option<String>, AdapterCapabilities) =
        match kind {
            AdapterKind::Claude => crate::adapter::claude::conformance::probe_scenario().await,
            AdapterKind::Codex => crate::adapter::codex::conformance::probe_scenario().await,
            AdapterKind::Copilot => crate::adapter::copilot::conformance::probe_scenario().await,
            AdapterKind::OmpRpc => crate::adapter::omp_rpc::conformance::probe().await,
        };

    PROBE_CACHE
        .lock()
        .insert(kind, (std::time::Instant::now(), result.clone()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_adapter_kind_produces_a_fixture_report() {
        for kind in [
            AdapterKind::Claude,
            AdapterKind::Codex,
            AdapterKind::Copilot,
            AdapterKind::OmpRpc,
        ] {
            let report = run_fixture_conformance(kind).await;
            assert_eq!(report.adapter, kind.wire_name());
            assert_eq!(report.mode, ConformanceMode::Fixture);
            assert!(
                !report.scenarios.is_empty(),
                "{kind} fixture report must run at least one scenario"
            );
        }
    }
}
