// Adapter submodules (claude/codex/copilot/omp_rpc) reference the crate's
// own public API via its external path (`batman_runtime::adapter::...`,
// `batman_runtime::supervisor::...`) rather than `crate::...`, so that the
// exact same source compiles unchanged both here (inside the library
// itself) and when pulled into a standalone integration test binary via
// `#[path = "..."] mod x;` (where `batman_runtime::` is the only path that
// resolves, since the test binary has no `crate::adapter` of its own).
// This standard 2018+-edition idiom makes the crate's own external name
// resolve to itself from the inside.
extern crate self as batman_runtime;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod adapter;
pub mod approval;
pub mod audit;
pub mod canonical_json;
pub mod config;
pub mod conformance;
pub mod coordination;
pub mod db;
pub mod display;
pub mod doctor;
pub mod domain;
pub mod env_flag;
pub mod ipc;
pub mod lifecycle;
pub mod paths;
pub mod policy;
pub mod recovery;
pub mod security;
pub mod service;
pub mod supervisor;
pub mod workspace;

pub use approval::{
    ApprovalCallback, ApprovalError, ApprovalService, DecideOutcome, NoopApprovalCallback,
};
pub use audit::{Export, Retention};
pub use coordination::{CoordinationBroker, CoordinationError, ScopeTokenStore};
pub use db::{DatabaseHandle, DbError};
pub use doctor::{Doctor, DoctorError, DoctorResult, FailedCheck};
pub use domain::{Committed, DomainError, DomainRepository, TransitionError};
pub use ipc::{IpcError, Server, ServerConfig};
pub use lifecycle::should_idle_shutdown;
pub use paths::{PathError, RuntimePaths, repository_id_from_canonical_root};
pub use recovery::{
    DEFAULT_STALE_RUN_THRESHOLD, RecoveryConfig, RecoveryCoordinator, RecoveryError, RecoveryResult,
};
pub use security::{SecurityError, StateRoot};
pub use service::{FakeRunDriver, OrchestrationService, RunDriver, RunDriverContext, ServiceError};
