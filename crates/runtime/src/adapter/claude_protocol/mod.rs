//! The claude protocol adapter: drives `claude -p
//! --input-format stream-json --output-format stream-json` and its
//! control channel directly, rather than a PTY -- the terminal becomes a
//! self-rendered view, never the control surface. Selected by
//! `AdapterMode::Protocol`, claude only for this spike; every other
//! reserved kind under that mode stays typed-rejected the same way
//! `AdapterMode::Headless` is today.
//!
//! [`adapter::ClaudeProtocolAdapter`] is the real [`super::Adapter`]
//! implementation [`super::registry::gate_profile`]'s
//! `AdapterMode::Protocol` branch was built ahead of: subprocess spawn
//! with the settled fixed argv ([`launch::build_argv`]) -- plain `-p`,
//! both stream-json formats, explicit `--setting-sources`, no `--bare`
//! or hook-suppressing flag -- the workspace-trust pre-check
//! ([`trust::workspace_trust_accepted`]), the control-channel
//! reader ([`reader::drive_turn`]), the approval bridge
//! ([`approval_bridge`]), and reconciliation against claude's own
//! transcript ([`reconcile::find_gaps`]). Still not built: the pane
//! renderer beyond bare legibility, `resume`/`--continue`, and the
//! other three vendors.

pub(crate) mod adapter;
// `pub`, not `pub(crate)`: `ProtocolApprovalCallback`'s own doc comment
// explains why an external test needs to reach it (the same "widen to
// let a test prove a production property" precedent `reconcile` below
// already set, for a different property).
pub mod approval_bridge;
pub(crate) mod launch;
pub(crate) mod reader;
pub(crate) mod trust;

// `pub`, not `pub(crate)`: `crates/runtime/tests/claude_protocol_reconcile.rs`
// links this crate as an ordinary external dependency specifically to
// reach `find_gaps` under a compilation where `cfg(test)` is NOT applied
// to this crate's own code -- the one place able to prove the
// production (never-injected) parse path actually runs, on the same
// rule `adapter::tui::grid`'s own module doc comment states for
// `TerminalGrid`. The test-only injection seam itself
// (`reconcile::arm_drop_for_test`) stays private to this module: an
// external, production-shaped caller must never be able to reach it,
// only to prove it is not there.
pub mod reconcile;

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
