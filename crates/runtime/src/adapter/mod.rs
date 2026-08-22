//! The worker adapter contract: capability declaration, the [`Adapter`]
//! trait every harness integration implements, the immutable
//! [`WorkerProfile`] a supervised process is launched from, the error
//! boundary every adapter operation returns through, and the event sink
//! adapters push normalized telemetry through rather than writing
//! [`crate::domain::DomainRepository`] directly.
mod capability;
pub mod claude;
pub mod codex;
pub mod copilot;
mod error;
mod event_sink;
pub mod mcp_config;
pub mod omp_rpc;
mod profile;
mod profile_store;
pub mod registry;
mod run_lifecycle;
pub mod terminal;
#[path = "trait.rs"]
mod r#trait;
pub mod tui;

use std::future::Future;
use std::pin::Pin;

pub use capability::{
    AdapterCapabilities, ApprovalsCapability, CAPABILITY_FIELD_NAMES, DurabilityCapability,
    NativeViewCapability, NestedCapability, ProtocolKind, ResumeCapability, SteeringCapability,
    UsageCapability, WorkspaceControlCapability,
};
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use copilot::CopilotAdapter;
pub use error::{AdapterError, AdapterErrorCode};
pub use event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink, DomainAdapterEventSink};
pub use omp_rpc::{OmpRpcAdapter, OmpRpcAdapterOptions};
pub use profile::{
    AdapterKind, ClaudeStartupOptions, CodexStartupOptions, CopilotStartupOptions, EffectivePolicy,
    OmpRpcStartupOptions, ProfileError, ProfileId, StartupOptions, TerminalDegradedStartupOptions,
    WorkerProfile,
};
pub use profile_store::{ProfileStore, ProfileStoreError};
pub use registry::{AdapterAuthorization, AdapterRegistry, FixtureAuthorization, RegistryError};
pub use run_lifecycle::RunLifecycleSink;
pub use r#trait::{
    Adapter, AdapterMessage, AdapterSnapshot, CancelScope, ProbeResult, StartSpec, VendorSessionRef,
};

/// A boxed future returned by every [`Adapter`]/[`AdapterEventSink`]
/// operation, resolving to `Result<T, AdapterError>`.
pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AdapterError>> + Send + 'a>>;
