//! The adapter error boundary: every failure crossing an [`super::Adapter`]
//! method carries a stable machine-readable code, the adapter kind, the
//! operation attempted, and a redacted (never secret-bearing) detail
//! string. Unsupported operations are never approximated -- they return
//! [`AdapterErrorCode::CapabilityUnsupported`] explicitly.

use std::fmt;

/// A stable, machine-readable classification of an [`AdapterError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterErrorCode {
    /// The adapter (its CLI, its process, or its account) is not usable
    /// right now.
    Unavailable,
    /// The adapter needs interactive authentication; the runtime never
    /// opens a browser or login flow on the adapter's behalf.
    AuthRequired,
    /// The installed vendor CLI/protocol version is incompatible with
    /// what this adapter was built against.
    IncompatibleVersion,
    /// The vendor protocol misbehaved (malformed frame, unexpected
    /// response shape, ...).
    Protocol,
    /// The operation attempted is not among this adapter's declared
    /// capabilities.
    CapabilityUnsupported,
    /// The supervised process failed mechanically (spawn failure,
    /// unexpected exit, ...).
    Process,
    /// The operation was cancelled before it completed.
    Cancelled,
    /// The vendor session/thread is in a state that makes the requested
    /// operation invalid (e.g. resuming a session that was never
    /// established).
    InvalidVendorState,
}

impl AdapterErrorCode {
    /// The wire/string form of this code, stable across releases.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::AuthRequired => "auth_required",
            Self::IncompatibleVersion => "incompatible_version",
            Self::Protocol => "protocol",
            Self::CapabilityUnsupported => "capability_unsupported",
            Self::Process => "process",
            Self::Cancelled => "cancelled",
            Self::InvalidVendorState => "invalid_vendor_state",
        }
    }
}

/// An error raised by a worker adapter operation. Always carries the
/// adapter kind and the operation attempted, so a caller (or a
/// conformance report) never has to guess which adapter or call failed.
///
/// `detail` must never contain a secret value or raw hidden-reasoning
/// content -- adapters are responsible for redacting `detail` themselves.
///
/// **This is a stated requirement, not a style preference: `detail` MUST
/// be a short, static description, and MUST NOT echo captured vendor
/// bytes (PTY output, transcript text, subprocess stderr) verbatim.**
/// CREW-78 made this load-bearing where it previously was not --
/// `fail_start` (`adapter/tui/adapter.rs`) now journals a TUI start
/// failure's `AdapterError::to_string()` as a durable
/// `ProtocolHealthChanged` diagnostic. Before that, `detail` reached only
/// the RPC response to the caller, which the journal's own redaction
/// boundary never has to answer for; now it crosses that boundary like
/// any other adapter-sourced text (`DomainAdapterEventSink::sanitize`
/// still runs the built-in and org secret-pattern scrubber over it even
/// though `Visible` content is kept, not dropped -- see
/// `crate::security::redaction::Redactor`), so a `detail` built by
/// interpolating raw vendor bytes would durably leak whatever the vendor
/// had just written, scrubber or not, if that text happened to contain
/// no secret-shaped substring the scrubber recognizes.
#[derive(Debug, Clone)]
pub struct AdapterError {
    code: AdapterErrorCode,
    adapter: String,
    operation: String,
    detail: String,
}

impl AdapterError {
    /// Constructs an [`AdapterError`] directly. Prefer the named
    /// constructors below where one fits.
    pub fn new(
        code: AdapterErrorCode,
        adapter: impl Into<String>,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            adapter: adapter.into(),
            operation: operation.into(),
            detail: detail.into(),
        }
    }

    /// The operation is not among this adapter's declared capabilities.
    #[must_use]
    pub fn capability_unsupported(
        adapter: impl Into<String>,
        operation: impl Into<String>,
    ) -> Self {
        Self::new(
            AdapterErrorCode::CapabilityUnsupported,
            adapter,
            operation,
            "the adapter does not declare this capability",
        )
    }

    #[must_use]
    pub fn unavailable(
        adapter: impl Into<String>,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(AdapterErrorCode::Unavailable, adapter, operation, detail)
    }

    #[must_use]
    pub fn auth_required(
        adapter: impl Into<String>,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(AdapterErrorCode::AuthRequired, adapter, operation, detail)
    }

    #[must_use]
    pub fn incompatible_version(
        adapter: impl Into<String>,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            AdapterErrorCode::IncompatibleVersion,
            adapter,
            operation,
            detail,
        )
    }

    #[must_use]
    pub fn protocol(
        adapter: impl Into<String>,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(AdapterErrorCode::Protocol, adapter, operation, detail)
    }

    #[must_use]
    pub fn process(
        adapter: impl Into<String>,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(AdapterErrorCode::Process, adapter, operation, detail)
    }

    #[must_use]
    pub fn cancelled(adapter: impl Into<String>, operation: impl Into<String>) -> Self {
        Self::new(
            AdapterErrorCode::Cancelled,
            adapter,
            operation,
            "operation cancelled",
        )
    }

    #[must_use]
    pub fn invalid_vendor_state(
        adapter: impl Into<String>,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            AdapterErrorCode::InvalidVendorState,
            adapter,
            operation,
            detail,
        )
    }

    /// The stable machine-readable code (e.g. `"capability_unsupported"`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code.as_str()
    }

    /// The typed code, for callers that want to match rather than compare
    /// strings.
    #[must_use]
    pub fn error_code(&self) -> AdapterErrorCode {
        self.code
    }

    /// The operation that was attempted (e.g. `"respondToApproval"`).
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// The adapter kind that raised this error (e.g. `"claude"`).
    #[must_use]
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// A redacted, human-readable detail string. Never contains secrets or
    /// hidden reasoning.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "adapter {} operation {} failed ({}): {}",
            self.adapter,
            self.operation,
            self.code(),
            self.detail
        )
    }
}

impl std::error::Error for AdapterError {}
