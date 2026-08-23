//! Policy evaluation: the `PolicyEvaluator` implementing the
//! `AdapterAuthorization` trait, and [`ViolationService`] for mid-run
//! nested-worker policy violations.
//!
//! The evaluator enforces:
//! - Concurrency ceiling (block runs exceeding the ceiling)
//! - Nested worker policy (deny unexpected child workers)
//!
//! Config-sourced org-governance enforcement (model/adapter allowlists,
//! required capabilities, cost ceilings, the `native_discovery_reviewed`
//! gate) is retired -- see `evaluate`'s module doc.

mod evaluate;
mod violation;

pub use evaluate::{
    PolicyError, PolicyEvaluation, PolicyEvaluator, PolicyViolation, PolicyViolationKind,
};
pub use violation::{DecideOutcome, ViolationError, ViolationService};
