//! Declared adapter capabilities.
//!
//! OMP schedules only against a worker's *declared* capabilities (and,
//! once the registry exists, only against the subset a conformance
//! scenario actually proved -- see `crate::conformance`). Every field here
//! is a closed, strict enum: an unknown wire value is a hard deserialize
//! error, never silently coerced to a default.
//!
//! Plain `serde` derives only (no `schemars`/`ts-rs`): these types never
//! cross the extension-facing wire directly. `crewd adapters --json`
//! and `crewd conformance ... --output` serialize them ad hoc, the same
//! way `crates/runtime/src/lifecycle.rs`'s `AlreadyRunning`/`LockContents`
//! already do for CLI-facing JSON that isn't part of the generated
//! protocol schema.

use serde::{Deserialize, Serialize};

/// Whether an adapter speaks a structured vendor protocol, or is only
/// reachable through terminal-screen automation (degraded control).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProtocolKind {
    Structured,
    Terminal,
}

/// What kind of session/turn resumption an adapter supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResumeCapability {
    None,
    Session,
    Turn,
}

/// Whether follow-up input can be delivered mid-turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SteeringCapability {
    None,
    Queued,
    ActiveTurn,
}

/// Whether tool/permission approval requests are observable and/or
/// resolvable through the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalsCapability {
    None,
    Observable,
    Controllable,
}

/// The granularity of usage/cost reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageCapability {
    None,
    Aggregate,
    PerTurn,
    PerChild,
}

/// Whether nested/child workers are controllable through the adapter.
/// Only `Managed` permits nesting at all; see
/// `docs/architecture.md`/the design spec's "Nested workers" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NestedCapability {
    None,
    Observable,
    Managed,
}

/// Whether a native vendor TUI can be attached to, alongside the
/// structured adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NativeViewCapability {
    None,
    Attach,
    IndependentTui,
}

/// Whether the adapter's workspace access is read-only or write-capable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkspaceControlCapability {
    ReadOnly,
    Write,
}

/// How durable a run is across process/runtime restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DurabilityCapability {
    ParentScoped,
    RuntimeScoped,
    VendorResumable,
}

/// The full set of capabilities a worker adapter declares.
///
/// Declaring a capability is not the same as it being production-approved
/// -- the (later) conformance runner strips any capability whose fixture
/// scenario failed before OMP ever sees it (see `crate::conformance`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterCapabilities {
    pub protocol: ProtocolKind,
    pub resume: ResumeCapability,
    pub steering: SteeringCapability,
    pub approvals: ApprovalsCapability,
    pub structured_result: bool,
    pub usage: UsageCapability,
    pub nested: NestedCapability,
    pub native_view: NativeViewCapability,
    pub workspace_control: WorkspaceControlCapability,
    pub durability: DurabilityCapability,
}

/// Every field name of [`AdapterCapabilities`], in its serialized form.
/// This is the vocabulary `capabilities.required` accepts: an org names a
/// capability with the same string the adapter catalog prints, and no
/// second vocabulary exists to drift from this one.
pub const CAPABILITY_FIELD_NAMES: &[&str] = &[
    "protocol",
    "resume",
    "steering",
    "approvals",
    "structuredResult",
    "usage",
    "nested",
    "nativeView",
    "workspaceControl",
    "durability",
];

impl AdapterCapabilities {
    /// Whether the named capability is *present* in this set.
    ///
    /// "Present" means the adapter actually offers it: `false` for a
    /// boolean field and the `none` variant of an enum both mean absent.
    /// Fields with no `none` variant (`protocol`, `workspaceControl`,
    /// `durability`) always have some value and are therefore always
    /// present -- requiring them is satisfied by every adapter.
    ///
    /// Returns `false` for an unrecognized name. Config validation rejects
    /// those up front (see `CAPABILITY_FIELD_NAMES`), so reaching this with
    /// one means an unenforceable requirement, and denying is the safe read.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        match name {
            "protocol" | "workspaceControl" | "durability" => true,
            "resume" => self.resume != ResumeCapability::None,
            "steering" => self.steering != SteeringCapability::None,
            "approvals" => self.approvals != ApprovalsCapability::None,
            "structuredResult" => self.structured_result,
            "usage" => self.usage != UsageCapability::None,
            "nested" => self.nested != NestedCapability::None,
            "nativeView" => self.native_view != NativeViewCapability::None,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_serializes_camel_case() {
        let caps = AdapterCapabilities {
            protocol: ProtocolKind::Structured,
            resume: ResumeCapability::Session,
            steering: SteeringCapability::ActiveTurn,
            approvals: ApprovalsCapability::Controllable,
            structured_result: true,
            usage: UsageCapability::PerTurn,
            nested: NestedCapability::Observable,
            native_view: NativeViewCapability::IndependentTui,
            workspace_control: WorkspaceControlCapability::Write,
            durability: DurabilityCapability::VendorResumable,
        };
        let value = serde_json::to_value(caps).unwrap();
        assert_eq!(value["steering"], "activeTurn");
        assert_eq!(value["usage"], "perTurn");
        assert_eq!(value["nativeView"], "independentTui");
        assert_eq!(value["workspaceControl"], "write");
        assert_eq!(value["durability"], "vendorResumable");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let mut value = serde_json::to_value(AdapterCapabilities {
            protocol: ProtocolKind::Structured,
            resume: ResumeCapability::None,
            steering: SteeringCapability::None,
            approvals: ApprovalsCapability::None,
            structured_result: false,
            usage: UsageCapability::None,
            nested: NestedCapability::None,
            native_view: NativeViewCapability::None,
            workspace_control: WorkspaceControlCapability::ReadOnly,
            durability: DurabilityCapability::ParentScoped,
        })
        .unwrap();
        value["unknownField"] = serde_json::json!(true);
        let result: Result<AdapterCapabilities, _> = serde_json::from_value(value);
        assert!(result.is_err());
    }
}
