//! The worker adapter contract: capability declaration, the [`Adapter`]
//! trait every harness integration implements, the immutable
//! [`WorkerProfile`] a supervised process is launched from, the error
//! boundary every adapter operation returns through, and the event sink
//! adapters push normalized telemetry through rather than writing
//! [`crate::domain::DomainRepository`] directly.
mod activity;
mod capability;
mod error;
mod event_sink;
pub mod mcp_config;
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

pub use activity::{ActivityClock, due_timeouts, millis_since};
pub use capability::{
    AdapterCapabilities, ApprovalsCapability, CAPABILITY_FIELD_NAMES, DurabilityCapability,
    NativeViewCapability, NestedCapability, ProtocolKind, ResumeCapability, SteeringCapability,
    UsageCapability, WorkspaceControlCapability,
};
pub use error::{AdapterError, AdapterErrorCode};
pub use event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink, DomainAdapterEventSink};
pub use profile::{
    AdapterKind, AdapterMode, ClaudeStartupOptions, CodexStartupOptions, CopilotStartupOptions,
    EffectivePolicy, OmpRpcStartupOptions, ProfileError, ProfileId, StartupOptions,
    TerminalDegradedStartupOptions, WorkerProfile,
};
pub use profile_store::{ProfileStore, ProfileStoreError};
pub use registry::{
    AdapterAuthorization, AdapterRegistry, FixtureAuthorization, RegistryError, ResumeSupport,
};
pub use run_lifecycle::RunLifecycleSink;
pub use r#trait::{
    Adapter, AdapterMessage, AdapterSnapshot, CancelScope, ProbeResult, StartSpec, VendorSessionRef,
};
pub use tui::{
    ClaudeTuiVendor, LaunchSpec, TuiAdapter, TuiSupport, TuiTimings, TuiVendor, VersionVerdict,
};

/// A boxed future returned by every [`Adapter`]/[`AdapterEventSink`]
/// operation, resolving to `Result<T, AdapterError>`.
pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AdapterError>> + Send + 'a>>;
