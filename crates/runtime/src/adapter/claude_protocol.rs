//! The claude protocol adapter (ADR-0037 spike): drives `claude -p
//! --input-format stream-json --output-format stream-json` and its
//! control channel directly, rather than a PTY -- the terminal becomes a
//! self-rendered view, never the control surface. Selected by
//! `AdapterMode::Protocol`, claude only for this spike; every other
//! reserved kind under that mode stays typed-rejected the same way
//! `AdapterMode::Headless` is today.
//!
//! Not yet a real [`super::Adapter`] implementation -- this module
//! currently exists only to give [`super::registry::gate_profile`]'s
//! new `AdapterMode::Protocol` branch something honest to declare, ahead
//! of the adapter itself. The fuller shape still to come: control
//! channel framing, a bridge from a vendor permission request to
//! `crate::approval::ApprovalService`'s existing ledger, reconciling the
//! vendor's own durable transcript against what this run journaled
//! (with a test-only seam to prove that reconciliation actually catches
//! a dropped event), and the pane renderer.

use super::capability::{
    AdapterCapabilities, ApprovalsCapability, DurabilityCapability, NativeViewCapability,
    NestedCapability, ProtocolKind, ResumeCapability, SteeringCapability, UsageCapability,
    WorkspaceControlCapability,
};

/// The capabilities this adapter *intends* to support, for
/// [`super::registry::GatedCapabilities::Unproven`] to carry until a real
/// conformance suite exists to prove them. Deliberately conservative --
/// committed scope only, not aspirational: this spike proves one worker,
/// one turn, one approval, no resume, so `resume`/`steering`/`usage` are
/// declared absent rather than guessed at. `approvals: Controllable` is
/// the one claim this spike is actually built to demonstrate: a real
/// vendor permission request answered through crew's own approval
/// ledger, not merely observed.
///
/// A free function, not a method on some disposable adapter instance
/// (the pattern `TuiAdapter::capabilities()` uses): there is no cheap,
/// side-effect-free way to construct this adapter yet (it will need
/// `Arc<ApprovalService>` and friends), and this value does not depend
/// on any of that -- it is a fact about what the module intends, not
/// about a particular instance's runtime state.
#[must_use]
pub fn declared_capabilities() -> AdapterCapabilities {
    AdapterCapabilities {
        protocol: ProtocolKind::Structured,
        resume: ResumeCapability::None,
        steering: SteeringCapability::None,
        approvals: ApprovalsCapability::Controllable,
        structured_result: true,
        usage: UsageCapability::None,
        nested: NestedCapability::None,
        native_view: NativeViewCapability::None,
        workspace_control: WorkspaceControlCapability::Write,
        durability: DurabilityCapability::ParentScoped,
    }
}
