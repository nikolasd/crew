//! UUIDv7-backed string identifiers used throughout the Crew wire protocol.
//!
//! Every identifier is a distinct newtype around [`uuid::Uuid`] so that, for
//! example, a [`TaskId`] can never be passed where a [`WorkerId`] is expected.
//! On the wire (JSON, JSON Schema, TypeScript) each identifier is a plain
//! string; the newtype wrapper only exists on the Rust side.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ts_rs::TS;
use uuid::Uuid;

/// Defines a UUIDv7-backed identifier newtype that serializes as a plain
/// string on the wire.
///
/// Generates: a constructor (`new`), a fallible parser (`parse`), and
/// `Display`, `FromStr`, `Serialize`, `Deserialize`, `JsonSchema`, and `TS`
/// implementations. Kept as a macro so the eight identifier types below do
/// not repeat this boilerplate.
macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, JsonSchema, TS)]
        #[schemars(with = "String")]
        #[ts(export, type = "string")]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a fresh, time-ordered (UUIDv7) identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Parses an identifier from its canonical string form.
            ///
            /// # Errors
            /// Returns [`uuid::Error`] if `value` is not a valid UUID.
            pub fn parse(value: &str) -> Result<Self, uuid::Error> {
                Uuid::parse_str(value).map(Self)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::parse(&raw).map_err(serde::de::Error::custom)
            }
        }
    };
}

uuid_id!(
    /// Identifies a repository/project managed by the Crew runtime.
    ProjectId
);
uuid_id!(
    /// Identifies a task tracked by the runtime.
    TaskId
);
uuid_id!(
    /// Identifies a worker process spawned by the runtime.
    WorkerId
);
uuid_id!(
    /// Identifies a single run of a task.
    RunId
);
uuid_id!(
    /// Identifies an in-flight runtime operation.
    OperationId
);
uuid_id!(
    /// Identifies a single message within a run's transcript.
    MessageId
);
uuid_id!(
    /// Identifies an approval request raised by the runtime.
    ApprovalId
);
uuid_id!(
    /// Identifies an artifact produced by a run.
    ArtifactId
);
uuid_id!(
    /// Identifies a mid-run nested-worker policy violation.
    PolicyViolationId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_distinct() {
        assert_ne!(ProjectId::new(), ProjectId::new());
    }

    #[test]
    fn round_trips_through_string() {
        let id = TaskId::new();
        let parsed = TaskId::parse(&id.to_string()).expect("valid uuid string round-trips");
        assert_eq!(id, parsed);
    }

    #[test]
    fn round_trips_through_json() {
        let id = WorkerId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        let back: WorkerId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn rejects_invalid_strings() {
        assert!(RunId::parse("not-a-uuid").is_err());
        assert!(serde_json::from_str::<RunId>("\"not-a-uuid\"").is_err());
    }
}
