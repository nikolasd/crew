//! Protocol version negotiation types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A Crew protocol version, expressed as `major.minor` with no patch
/// component (patch-level changes must be backward compatible).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema, TS,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}

/// An inclusive range of protocol versions a client (or runtime) supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct VersionRange {
    pub min: ProtocolVersion,
    pub max: ProtocolVersion,
}

/// The wire protocol version this workspace implements.
///
/// Lives here rather than in the runtime because it is a property of the
/// protocol itself, and two crates need it independently: the runtime
/// negotiates against it on every `initialize`, and `crew-xtask` records
/// it as release provenance in each leaf manifest. A second copy would let
/// a shipped manifest claim a version the runtime does not speak.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(1, 0);

/// The inclusive range of protocol versions this workspace supports.
/// Foundation scope implements exactly `1.0`.
#[must_use]
pub const fn supported_versions() -> VersionRange {
    VersionRange {
        min: PROTOCOL_VERSION,
        max: PROTOCOL_VERSION,
    }
}

/// The supported range as the `"<min>-<max>"` text a leaf manifest records,
/// e.g. `"1.0-1.0"`.
#[must_use]
pub fn supported_range_text() -> String {
    let VersionRange { min, max } = supported_versions();
    format!("{}.{}-{}.{}", min.major, min.minor, max.major, max.minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_expected_fields() {
        let version = ProtocolVersion::new(1, 2);
        assert_eq!(version.major, 1);
        assert_eq!(version.minor, 2);
    }

    #[test]
    fn orders_by_major_then_minor() {
        assert!(ProtocolVersion::new(1, 9) < ProtocolVersion::new(2, 0));
        assert!(ProtocolVersion::new(1, 1) < ProtocolVersion::new(1, 2));
    }
}
