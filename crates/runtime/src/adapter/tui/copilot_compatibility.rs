//! The Copilot CLI version compatibility table.
//!
//! An installed `copilot` CLI version is not trusted just because it
//! answers `--version`: [`super::copilot::CopilotTuiVendor`]'s own
//! `version_gate` only proceeds past `initialize` for a CLI version
//! empirically verified against (exact match, never a "nearby" patch
//! version assumed compatible), and only for a negotiated ACP protocol
//! version this crate understands the field names of. `1.0.73` was the
//! version installed on the build machine at the start of this work; the
//! CLI's own background auto-updater (`copilot update`) then moved it to
//! `1.0.75`, `1.0.78`, `1.0.80`, and later `1.0.81`. All five are listed
//! below because each was empirically reprobed with a real `initialize`
//! handshake and confirmed to negotiate `protocolVersion: 1` with
//! identical ACP v1 `agentCapabilities`/`agentInfo` field names -- adding
//! a newer CLI release here always requires that same kind of empirical
//! verification, not a guess.
//!
//! crew-v2 gap-closure WP-C: moved here from the (now-deleted) headless
//! `adapter::copilot` module -- this table gates the TUI vendor's version
//! check now that the headless one it originally served is gone. The
//! empirical reprobe evidence this table's own doc comment describes
//! (the `initialize`/`agentInfo` handshake) now lives in
//! `super::copilot_conformance`'s live/fixture suites instead of a
//! standalone headless-adapter test.

/// One verified `(CLI version, negotiated ACP protocol version)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopilotCompatibilityEntry {
    pub cli_version: &'static str,
    pub acp_protocol_version: u64,
}

/// The exact CLI versions this adapter has been verified against, and the
/// ACP protocol version each negotiates.
pub const COPILOT_KNOWN_CLI_VERSIONS: &[CopilotCompatibilityEntry] = &[
    CopilotCompatibilityEntry {
        cli_version: "1.0.73",
        acp_protocol_version: 1,
    },
    CopilotCompatibilityEntry {
        cli_version: "1.0.75",
        acp_protocol_version: 1,
    },
    CopilotCompatibilityEntry {
        cli_version: "1.0.78",
        acp_protocol_version: 1,
    },
    CopilotCompatibilityEntry {
        cli_version: "1.0.80",
        acp_protocol_version: 1,
    },
    CopilotCompatibilityEntry {
        cli_version: "1.0.81",
        acp_protocol_version: 1,
    },
];

/// The inclusive range of ACP protocol versions this adapter's
/// `normalize.rs` understands the v1 field names of. A negotiated version
/// outside this range is refused with `AdapterError::incompatible_version`.
pub const COPILOT_MIN_ACP_PROTOCOL_VERSION: u64 = 1;
pub const COPILOT_MAX_ACP_PROTOCOL_VERSION: u64 = 1;

/// Whether `cli_version` is a CLI version this adapter has verified
/// end-to-end (exact string match only).
#[must_use]
pub fn copilot_cli_version_known(cli_version: &str) -> bool {
    COPILOT_KNOWN_CLI_VERSIONS
        .iter()
        .any(|entry| entry.cli_version == cli_version)
}

/// The single decision `ensure_client` and `probe()` must agree on:
/// whether a negotiated `agentInfo.version` is verified. A missing
/// version is **unknown, not implicitly verified** — the vendor omitting
/// an ordinary optional field must not bypass the empirical-verification
/// gate (R57).
#[must_use]
pub fn copilot_negotiated_version_verified(agent_version: Option<&str>) -> bool {
    agent_version.is_some_and(copilot_cli_version_known)
}

/// Whether `protocol_version` is one this adapter's client/normalize code
/// understands the field names of.
#[must_use]
pub fn copilot_acp_protocol_version_supported(protocol_version: u64) -> bool {
    (COPILOT_MIN_ACP_PROTOCOL_VERSION..=COPILOT_MAX_ACP_PROTOCOL_VERSION)
        .contains(&protocol_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Moved from the deleted `tests/copilot_adapter.rs` (crew-v2
    /// gap-closure WP-C): the pure compatibility-table check has no
    /// dependency on the (deleted) headless adapter at all, so it moves
    /// verbatim rather than being lost with the rest of that file.
    #[test]
    fn known_cli_version_is_exact_match_against_every_empirically_verified_version() {
        assert!(copilot_cli_version_known("1.0.73"));
        assert!(copilot_cli_version_known("1.0.75"));
        assert!(copilot_cli_version_known("1.0.78"));
        assert!(copilot_cli_version_known("1.0.80"));
        assert!(copilot_cli_version_known("1.0.81"));
        // A neighbouring patch release is never assumed compatible, and a
        // prefix is never a match.
        assert!(!copilot_cli_version_known("1.0.74"));
        assert!(!copilot_cli_version_known("1.0.77"));
        assert!(!copilot_cli_version_known("1.0.79"));
        assert!(!copilot_cli_version_known("1.0.82"));
        assert!(!copilot_cli_version_known("1.0.7"));
        assert!(!copilot_cli_version_known(""));
    }

    #[test]
    fn negotiated_version_verified_requires_a_present_and_known_version() {
        assert!(copilot_negotiated_version_verified(Some("1.0.81")));
        assert!(!copilot_negotiated_version_verified(Some("9.9.9")));
        assert!(
            !copilot_negotiated_version_verified(None),
            "a missing version is unknown, never implicitly verified (R57)"
        );
    }

    #[test]
    fn only_acp_protocol_v1_is_supported() {
        assert!(copilot_acp_protocol_version_supported(1));
        assert!(!copilot_acp_protocol_version_supported(0));
        assert!(!copilot_acp_protocol_version_supported(2));
    }
}
