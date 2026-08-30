//! The durable runtime event stream: envelopes, sanitized event payloads,
//! and the content-classification types used to keep unsanitized content out
//! of the durable log.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use ts_rs::TS;

use crate::approval::DecidedBy;
use crate::display::{DisplayBackend, DisplayPlacement};
use crate::ids::{
    ApprovalId, ArtifactId, MessageId, PolicyViolationId, ProjectId, RunId, TaskId, WorkerId,
};
use crate::workspace::WorkspaceEvent;

// Rather than expose `time::OffsetDateTime` across generated bindings,
// Crew normalizes every timestamp to a UTC RFC 3339 string at construction
// time, so schemars/ts-rs only ever see a plain string.
/// Canonical UTC RFC 3339 timestamp text, as carried on the wire.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema, TS)]
#[ts(export)]
pub struct Timestamp(String);

/// Error returned when a string cannot be parsed as an RFC 3339 timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampParseError(String);

impl fmt::Display for TimestampParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid RFC 3339 timestamp: {}", self.0)
    }
}

impl std::error::Error for TimestampParseError {}

impl Timestamp {
    /// Parses an RFC 3339 timestamp and normalizes it to UTC.
    ///
    /// # Errors
    /// Returns [`TimestampParseError`] if `input` is not a valid RFC 3339
    /// timestamp.
    pub fn parse(input: &str) -> Result<Self, TimestampParseError> {
        let parsed = OffsetDateTime::parse(input, &Rfc3339)
            .map_err(|err| TimestampParseError(err.to_string()))?;
        Self::from_offset_date_time(parsed)
    }

    /// Returns the current time as a normalized UTC timestamp.
    #[must_use]
    pub fn now() -> Self {
        Self::from_offset_date_time(OffsetDateTime::now_utc())
            .expect("formatting the current UTC time as RFC 3339 cannot fail")
    }

    fn from_offset_date_time(value: OffsetDateTime) -> Result<Self, TimestampParseError> {
        let utc = value.to_offset(time::UtcOffset::UTC);
        let formatted = utc
            .format(&Rfc3339)
            .map_err(|err| TimestampParseError(err.to_string()))?;
        Ok(Self(formatted))
    }

    /// Returns the canonical RFC 3339 string representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The sensitivity classification of a piece of raw content produced by a
/// worker, before it is sanitized for the durable event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum ContentClass {
    Visible,
    Thinking,
    Secret,
}

/// A value tagged with its [`ContentClass`]. Used for raw, in-memory event
/// fields before sanitization; the durable [`RuntimeEvent`] must never
/// contain a `Classified<T>` field, only plain sanitized values.
///
/// `Debug` is implemented manually (not derived): printing a
/// `Thinking`/`Secret`-classified value must never leak its raw content,
/// even via `{:?}`, so only `Visible` values are actually printed -- see
/// the `impl fmt::Debug` below.
#[derive(Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct Classified<T> {
    pub class: ContentClass,
    pub value: T,
}

// CREW-45: history and rationale below in `//`; the `///` block after it
// is the whole of what schemars lifts into crew.schema.json. A consumer of
// the schema sees a plain JSON string, so the shipped text says only what
// that string is; everything about *why* the Rust type exists is for the
// next reader of this file.
//
// Introduced by CREW-29, enforcing ADR-0006's boundary at the field.
// ADR-0028 covers the run-intent case.
//
// The field is private and there is no `From<String>` or `Deref`, so a
// `String` cannot become a `Redacted` implicitly. There are exactly two
// constructors and both are named as claims: `Redacted::from_sanitized`
// ("this came out of the redactor") and `Redacted::assert_runtime_authored`
// ("no caller wrote this").
//
// # The exact strength of the guarantee
//
// This is not "unconstructible without the redactor". The redactor lives
// in the runtime crate and `RuntimeEvent` lives here, so a constructor
// reachable from the runtime is unavoidable, and anything reachable from
// the runtime is reachable from anywhere. What the type actually
// guarantees is narrower and still worth having: a caller-carrying
// field cannot be populated without its author stating which of the two
// claims applies. The failure mode it eliminates is silence -- a new
// `String` field wired straight from request params, which is how all
// four leaks this work found came to exist. It does not stop someone
// asserting the wrong claim; it stops them asserting nothing, and it puts
// the assertion where a reviewer reads it.
//
// # Why this exists on fields rather than on the write path
//
// `DatabaseHandle::append_event` is guarded by `PersistableEvent`, a type
// only the redactor can construct. `DomainRepository::append_and_apply`
// takes a plain `RuntimeEvent`, and every domain event is written that
// way -- so redaction there was *convention*, which is the thing ADR-0006
// exists to eliminate. Most domain events carry no caller text at all
// (states, ids, lease refs), so gating the whole path would put ceremony
// on the safe majority to protect a handful of fields, and ceremony on
// safe cases is what gets skipped. Putting the obligation on the field
// instead means a new caller-carrying field is a compile error until
// its author decides how it gets sanitized.
//
// # The property, as an executable pair
//
// The two doctests proving this live on `RedactedBoundaryDoctests` below,
// not here: a doctest only runs from a `///` comment, and a `///` comment
// on this type is shipped schema text. Moving them to a `#[cfg(doctest)]`
// item keeps them running and keeps a wall of Rust out of the JSON Schema.
//
// # What it does not prevent
//
// `Deserialize` accepts a bare string, because stored events must be read
// back (`events/replay`, recovery, the audit export). So a determined
// caller could serialize and deserialize their way to a `Redacted`
// holding anything. That is deliberate and it is not the hole this closes:
// the failure mode being eliminated is *forgetting*, not laundering. A
// round trip through serde to bypass the redactor is not something anyone
// does by accident.
/// Text that has crossed Crew's redaction boundary: secret-shaped
/// substrings in it are masked before it is stored or sent, so what a
/// consumer reads here is the masked text, never the original.
///
/// Carried on the wire as an ordinary JSON string.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(transparent)]
#[ts(export, type = "string")]
pub struct Redacted(String);

/// Proves [`Redacted`]'s boundary as an executable pair. The two blocks
/// differ in exactly one token -- the constructor -- so together they show
/// the boundary rather than merely exercising it. The negative one alone
/// would be weak evidence: a `compile_fail` block passes on *any*
/// compilation error, including a typo, which is why the positive twin
/// sits beside it.
///
/// This lives on a `#[cfg(doctest)]` item rather than on `Redacted` itself
/// because a doctest only runs from a `///` comment, and a `///` comment on
/// `Redacted` is lifted verbatim into `crew.schema.json` (CREW-45). Here the
/// tests still run and the schema stays free of Rust.
///
/// A bare `String` cannot populate a caller-carrying field:
///
/// ```compile_fail
/// use crew_protocol::{RunId, RuntimeEvent, TaskId, WorkerId};
/// let _ = RuntimeEvent::RunPromptEvent {
///     run_id: RunId::new(),
///     task_id: TaskId::new(),
///     worker_id: WorkerId::new(),
///     prompt: "unredacted".to_string(),
/// };
/// ```
///
/// The same construction, with the claim stated, compiles:
///
/// ```
/// use crew_protocol::{Redacted, RunId, RuntimeEvent, TaskId, WorkerId};
/// let _ = RuntimeEvent::RunPromptEvent {
///     run_id: RunId::new(),
///     task_id: TaskId::new(),
///     worker_id: WorkerId::new(),
///     prompt: Redacted::assert_runtime_authored("a fixture, not caller text"),
/// };
/// ```
#[cfg(doctest)]
struct RedactedBoundaryDoctests;

impl Redacted {
    /// Asserts that `text` was authored by the runtime rather than by any
    /// caller or vendor, and therefore needs no redaction.
    ///
    /// The name is the point. This is the escape hatch, and an exemption
    /// should read as a claim its author is making — reviewable, and
    /// falsifiable by anyone who can see where the value came from. Use it
    /// for run states, ids, lease refs, signal names and the like; never
    /// for anything that reached the daemon from outside it.
    #[must_use]
    pub fn assert_runtime_authored(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The sanitized text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Asserts that `text` has already passed through the redactor.
    ///
    /// The one legitimate caller is `Redactor::redact`; everything else
    /// should be reaching for that instead. Kept `pub` because the
    /// redactor lives in the runtime crate and this type lives in the
    /// protocol crate — see the type-level note on what that costs.
    #[must_use]
    pub fn from_sanitized(text: String) -> Self {
        Self(text)
    }
}

/// Prints the text plainly: by construction it has already crossed the
/// redaction boundary, so unlike [`Classified`] there is nothing here that
/// a `{:?}` could leak.
impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// A placeholder printed in place of a redacted `Classified` value; has its
/// own `Debug` impl so it renders without the surrounding quotes a `&str`
/// placeholder would otherwise get.
struct RedactedPlaceholder;

impl fmt::Debug for RedactedPlaceholder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted>")
    }
}

impl<T: fmt::Debug> fmt::Debug for Classified<T> {
    /// Prints `value` only when `class` is [`ContentClass::Visible`];
    /// `Thinking`/`Secret` values print [`RedactedPlaceholder`] instead, so
    /// `{:?}` on a raw classified value can never leak secret or thinking
    /// content.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug_struct = f.debug_struct("Classified");
        debug_struct.field("class", &self.class);
        match self.class {
            ContentClass::Visible => {
                debug_struct.field("value", &self.value);
            }
            ContentClass::Thinking | ContentClass::Secret => {
                debug_struct.field("value", &RedactedPlaceholder);
            }
        }
        debug_struct.finish()
    }
}

/// Identifies which subsystem produced an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum EventSource {
    Runtime,
}

/// The severity of a `diagnostic` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

/// Who answered an `escalationRaised` escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum AnsweredBy {
    Leader,
    User,
}

/// Which liveness deadline a `workerTimeout` event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum TimeoutKind {
    Inactivity,
    Total,
}

/// One subtask within a `PlanSpec`, as proposed by `plan/propose`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct SubtaskSpec {
    /// The caller-assigned identifier for this subtask within its plan;
    /// distinct from a `TaskId`, since a proposed subtask is not yet a
    /// registered task until (and unless) the plan is approved.
    pub id: String,
    pub description: String,
    /// The adapter wire name (e.g. `claude`, `codex`) this subtask is
    /// intended to run under.
    pub adapter: String,
    /// Whether this subtask is expected to write to the workspace.
    pub writes: bool,
    /// The maximum number of turns this subtask may take; `null` means no
    /// explicit budget was proposed.
    pub turn_budget: Option<u32>,
}

/// A proposed decomposition of a run into subtasks, carried by
/// the `planProposed` event and returned by `plan/get`, awaiting
/// `plan/decide`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct PlanSpec {
    pub subtasks: Vec<SubtaskSpec>,
}

/// Independent boolean flags on a run.
///
/// `degradedControl`, `needsReconciliation`, `protocolUnhealthy`,
/// `policyQuarantined`, `workspaceDirty`, `childrenActive`, and
/// `turnSettled` are all independent booleans.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Default, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct RunFlags {
    pub degraded_control: bool,
    #[serde(rename = "needsReconciliation")]
    pub needs_reconciliation: bool,
    #[serde(rename = "protocolUnhealthy")]
    pub protocol_unhealthy: bool,
    #[serde(rename = "policyQuarantined")]
    pub policy_quarantined: bool,
    #[serde(rename = "workspaceDirty")]
    pub workspace_dirty: bool,
    #[serde(rename = "childrenActive")]
    pub children_active: bool,
    // CREW-45: history below, schema text above -- `///` is lifted into
    // crew.schema.json's `description`, `//` is not.
    //
    // ADR-0027 introduced this flag because a finished turn and a worker
    // question both land in `waitingUser`, so the run state alone cannot
    // tell a snapshot reader (`run/get`, the monitor) which happened.
    // `#[serde(default)]` is required because the journal is append-only:
    // `RunFlags` payloads written before this field existed must still
    // deserialize on replay.
    /// True when the run reached `waitingUser` because its vendor finished
    /// a turn, rather than because the worker asked a question. Both look
    /// the same from the run's state alone; this tells "the answer is
    /// ready" apart from "the worker needs you".
    ///
    /// Cleared when the run goes back to work, so it never outlives the
    /// pause it describes. Absent on events written before this field
    /// existed; treat absence as `false`.
    #[serde(rename = "turnSettled", default)]
    pub turn_settled: bool,
}

// ADR-0027. Exists so `run/finish` can settle a run as failed from durable
// evidence rather than by re-parsing message text.
/// How a vendor's turn ended.
///
/// Deliberately *not* a success/failure verdict on the task: only the
/// leader can judge that. It distinguishes an ordinary turn boundary from
/// one the vendor reached by reporting an API error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub enum TurnOutcome {
    /// The vendor finished its turn and is holding at its prompt.
    #[serde(rename = "normal")]
    Normal,
    /// The vendor ended the turn by reporting an API error (e.g. an
    /// unavailable model). The turn is genuinely over -- the CLI returns
    /// to its prompt -- so the boundary is still reported; this value is
    /// what makes it distinguishable.
    #[serde(rename = "apiError")]
    ApiError,
}

/// The semantic kind of an orchestration event stored in the durable journal.
///
/// Every record creation, lifecycle transition, flag change, message delivery
/// change, and approval request/decision produces one of these variants.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[ts(export)]
pub enum RuntimeEventKind {
    #[serde(rename = "taskCreated")]
    TaskCreated,
    #[serde(rename = "taskUpdated")]
    TaskUpdated,
    #[serde(rename = "workerCreated")]
    WorkerCreated,
    #[serde(rename = "runQueued")]
    RunQueued,
    #[serde(rename = "runStarting")]
    RunStarting,
    #[serde(rename = "runWorking")]
    RunWorking,
    #[serde(rename = "runWaitingUser")]
    RunWaitingUser,
    #[serde(rename = "runWaitingPeer")]
    RunWaitingPeer,
    #[serde(rename = "runPaused")]
    RunPaused,
    #[serde(rename = "runSucceeded")]
    RunSucceeded,
    #[serde(rename = "runFailed")]
    RunFailed,
    #[serde(rename = "runCancelled")]
    RunCancelled,
    #[serde(rename = "runLost")]
    RunLost,
    #[serde(rename = "runFlagsChanged")]
    RunFlagsChanged,
    #[serde(rename = "messageRecorded")]
    MessageRecorded,
    #[serde(rename = "messageSent")]
    MessageSent,
    #[serde(rename = "messageAcknowledged")]
    MessageAcknowledged,
    #[serde(rename = "messageFailed")]
    MessageFailed,
    #[serde(rename = "approvalRequested")]
    ApprovalRequested,
    #[serde(rename = "approvalDecided")]
    ApprovalDecided,
    #[serde(rename = "childWorkerRequested")]
    ChildWorkerRequested,
    // R83. Additive and forward-safe in the usual event-kind sense: a
    // binary predating this variant fails on it when replaying a journal
    // that contains it, exactly like every other event-kind addition.
    /// OMP accepted a pending child-worker request, binding the created
    /// child task/worker/run ids. Distinct from `childWorkerRequested` so
    /// a consumer never has to infer "accepted" from whether the child ids
    /// happen to be populated.
    #[serde(rename = "childWorkerAccepted")]
    ChildWorkerAccepted,
    #[serde(rename = "childWorkerRequestDenied")]
    ChildWorkerRequestDenied,
    #[serde(rename = "reconcileOwnershipChanged")]
    ReconcileOwnershipChanged,
    /// A worker adapter's supervised process started.
    #[serde(rename = "adapterProcessStarted")]
    AdapterProcessStarted,
    /// A worker adapter's supervised process exited.
    #[serde(rename = "adapterProcessExited")]
    AdapterProcessExited,
    /// A worker adapter established (or re-established) its vendor
    /// session/thread identifier.
    #[serde(rename = "adapterVendorSessionEstablished")]
    AdapterVendorSessionEstablished,
    /// A worker adapter streamed a partial visible-message chunk.
    #[serde(rename = "adapterMessageChunk")]
    AdapterMessageChunk,
    /// A worker adapter completed a visible message.
    #[serde(rename = "adapterMessageFinal")]
    AdapterMessageFinal,
    /// A worker adapter's tool call started.
    #[serde(rename = "adapterToolStarted")]
    AdapterToolStarted,
    /// A worker adapter's tool call reported progress.
    #[serde(rename = "adapterToolProgress")]
    AdapterToolProgress,
    /// A worker adapter's tool call finished.
    #[serde(rename = "adapterToolResult")]
    AdapterToolResult,
    /// A worker adapter reported usage/cost.
    #[serde(rename = "adapterUsageReported")]
    AdapterUsageReported,
    /// A worker adapter produced an artifact.
    #[serde(rename = "adapterArtifactProduced")]
    AdapterArtifactProduced,
    /// A worker adapter's protocol health changed.
    #[serde(rename = "adapterProtocolHealthChanged")]
    AdapterProtocolHealthChanged,
    /// A worker adapter observed a vendor-created child, regardless of its
    /// declared `nested` capability.
    #[serde(rename = "adapterNestedWorkerObserved")]
    AdapterNestedWorkerObserved,
    /// A TUI-mode worker adapter's transcript classified an assistant
    /// message as a question awaiting a human answer, rather than a
    /// completed message. Carried on the same `adapterMessageEvent`
    /// shape as `adapterMessageFinal` (role/text), distinguished
    /// only by this `kind`.
    #[serde(rename = "adapterQuestionDetected")]
    AdapterQuestionDetected,
    // ADR-0027.
    /// A TUI-mode worker adapter observed its vendor's own end-of-turn
    /// boundary: the worker has stopped working and is holding at its
    /// prompt. Evidence that the turn ended, never that the task
    /// succeeded -- the vendor markers behind it say only "this turn is
    /// over", and Codex's is literally `task_complete` whatever the
    /// outcome.
    #[serde(rename = "adapterTurnEnded")]
    AdapterTurnEnded,
    /// A display backend attached a Crew-owned pane to a run.
    #[serde(rename = "displayPaneAttached")]
    DisplayPaneAttached,
    /// A display backend detached (closed) a Crew-owned pane.
    #[serde(rename = "displayPaneDetached")]
    DisplayPaneDetached,
    /// A policy violation was recorded (model not allowed, concurrency
    /// ceiling exceeded, nested worker denied, or adapter not authorized).
    #[serde(rename = "policyViolation")]
    PolicyViolation {
        profile_id: String,
        adapter: String,
        model: String,
        violation_kind: String,
        reason: String,
        is_nested: bool,
    },
    // `Run` (crates/protocol/src/run.rs) carries this as `flags:
    // RunFlags`, but isn't itself reachable from ProtocolDocument's
    // fields, so it has no $defs entry a schema consumer could look
    // it up by -- named generically below instead.
    /// A policy violation was recorded for an already-running worker (mid-run
    /// violation, not pre-authorization). Quarantine/cancel state is tracked
    /// on the run's `flags.policyQuarantined`.
    #[serde(rename = "policyViolationRecorded")]
    PolicyViolationRecorded {
        violation_id: PolicyViolationId,
        /// The machine-readable violation code: `nested_worker_denied` or
        /// `cost_ceiling_exceeded`. New codes are added here, never invented
        /// at a call site.
        code: String,
        /// The sequence of the event that triggered this violation, so an
        /// operator can correlate the violation to its cause.
        #[ts(type = "number")]
        observed_event_sequence: u64,
        // The resolved policy this fingerprints is `crew_runtime`'s
        // `RuntimePolicy` (crates/runtime/src/config/mod.rs) -- a
        // runtime-crate type with no wire name, so the shipped
        // description below names it generically instead.
        /// The SHA-256 fingerprint of the resolved policy this run was
        /// authorized under, so the violation is auditable against a
        /// specific merge of org/repo/user/per-run layers.
        policy_fingerprint: String,
        /// Present (non-`null`) only for a nested-worker violation; `null`
        /// for any violation with no vendor child, such as a cost ceiling.
        vendor_child_id: Option<String>,
        vendor_parent_ref: Option<String>,
        action: String,
    },
    /// A policy violation was resolved (decided) by the owning OMP client.
    #[serde(rename = "policyViolationDecided")]
    PolicyViolationDecided {
        violation_id: PolicyViolationId,
        resolution: String,
        resolved_by: String,
    },
}

// Fields are plain, already-sanitized types -- never `Classified<T>` --
// so that raw thinking/secret content can never reach the durable log
// through this type. `Classified` itself is not a wire type: it's excluded
// from the shipped schema entirely (see NOT_WIRE_MESSAGE_ROOTS in
// crates/xtask/src/main.rs), so it has no name a schema consumer could
// look up; that's why the shipped description below doesn't say it.
/// A sanitized, durable runtime event. Fields are plain, already-sanitized
/// values, so raw thinking/secret content can never reach the durable log
/// through this type.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
#[ts(export)]
pub enum RuntimeEvent {
    RuntimeStarted,
    RuntimeStopping,
    Diagnostic {
        level: DiagnosticLevel,
        code: String,
        message: String,
    },
    /// A task was created or updated via `task/upsert`.
    TaskEvent {
        kind: RuntimeEventKind,
        task_id: TaskId,
        owner_client_instance_id: String,
        #[ts(type = "number")]
        revision: u64,
    },
    /// A worker was created via `worker/create`.
    WorkerEvent {
        kind: RuntimeEventKind,
        worker_id: WorkerId,
        profile_id: String,
    },
    /// A run entered a new lifecycle state.
    RunEvent {
        kind: RuntimeEventKind,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        state: String,
    },
    /// Flags on a run were changed.
    RunFlagsEvent {
        run_id: RunId,
        flags: RunFlags,
    },
    // ADR-0028 (durable run intent). `prompt` has already crossed the
    // ADR-0006 boundary: the submitting service classifies it `Visible` and
    // passes it through `Redactor::sanitize_fragment`, so it is masked
    // before this event is ever constructed. Carries no `kind`, like
    // `RunFlagsEvent` -- there is one thing it can mean.
    /// The prompt a run was submitted with, so every consumer of a run's
    /// journal can read the question its transcript answers. Already
    /// redacted: secret-shaped substrings are masked.
    ///
    /// A run submitted without a prompt produces no event at all rather
    /// than one carrying an empty string, so absence stays distinguishable
    /// from an empty prompt.
    RunPromptEvent {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        prompt: Redacted,
    },
    /// A message was recorded, sent, acknowledged, or failed.
    MessageEvent {
        kind: RuntimeEventKind,
        message_id: MessageId,
        run_id: RunId,
        task_id: TaskId,
        delivery_state: String,
    },
    /// An approval request was created.
    ApprovalEvent {
        kind: RuntimeEventKind,
        approval_id: ApprovalId,
        run_id: RunId,
        task_id: TaskId,
        action: String,
        decided_by: Option<DecidedBy>,
        // R59 added this field; optional in both directions so events
        // persisted before it existed still deserialize.
        /// The decision's rationale, when one was supplied. Absent on
        /// `approvalDecided` events written before this field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        reason: Option<Redacted>,
    },
    /// A child worker was requested or denied.
    ChildEvent {
        kind: RuntimeEventKind,
        parent_run_id: RunId,
        child_task_id: Option<TaskId>,
        child_worker_id: Option<WorkerId>,
        child_run_id: Option<RunId>,
        reason: Option<Redacted>,
    },
    /// Ownership of a task was rebound via `reconcile/omp`.
    ReconcileEvent {
        task_id: TaskId,
        old_owner_client_instance_id: String,
        new_owner_client_instance_id: String,
        #[ts(type = "number")]
        revision: u64,
    },
    /// A supervised adapter process started or exited.
    AdapterProcessEvent {
        kind: RuntimeEventKind,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        pid: Option<u32>,
        exit_code: Option<i32>,
        signal: Option<String>,
    },
    /// A worker adapter established (or re-established) its vendor
    /// session/thread identifier.
    AdapterVendorSessionEvent {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        vendor_session_id: String,
    },
    // ADR-0027.
    /// A TUI-mode worker adapter observed its vendor's end-of-turn
    /// boundary. Carries no free text: the turn's content was already
    /// journaled as its own message events, and this event exists to say
    /// only *that* the turn ended, and how.
    AdapterTurnEvent {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        outcome: TurnOutcome,
    },
    // Dropped rather than kept-but-masked because the redactor works on
    // substrings within Visible content; a fragment classified wholesale
    // as ContentClass::Thinking or ::Secret has nothing Visible to mask,
    // so there is no partial-text form to carry.
    /// A visible message chunk or final message from a worker adapter.
    /// `text` has already crossed the redaction boundary; `null` means
    /// the entire fragment was classified as sensitive and dropped, not
    /// that the message was empty.
    AdapterMessageEvent {
        kind: RuntimeEventKind,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        role: String,
        text: Option<Redacted>,
    },
    // See AdapterMessageEvent above: a wholesale-sensitive fragment has
    // nothing Visible left to mask, so it's dropped rather than redacted.
    /// A tool call lifecycle event from a worker adapter. `detail` has
    /// already crossed the redaction boundary; `null` means the detail
    /// fragment was classified as sensitive and dropped, not that it was
    /// empty.
    AdapterToolEvent {
        kind: RuntimeEventKind,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        tool_call_id: String,
        name: String,
        ok: Option<bool>,
        detail: Option<Redacted>,
    },
    /// Usage/cost reported by a worker adapter.
    AdapterUsageEvent {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        #[ts(type = "number")]
        input_tokens: u64,
        #[ts(type = "number")]
        output_tokens: u64,
        cost_usd: Option<f64>,
    },
    /// An artifact produced by a worker adapter.
    AdapterArtifactEvent {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        artifact_id: ArtifactId,
        artifact_kind: String,
    },
    // See AdapterMessageEvent above: a wholesale-sensitive fragment has
    // nothing Visible left to mask, so it's dropped rather than redacted.
    /// A worker adapter's protocol health changed. `detail` has already
    /// crossed the redaction boundary; `null` means the detail fragment
    /// was classified as sensitive and dropped, not that it was empty.
    AdapterProtocolHealthEvent {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        healthy: bool,
        detail: Option<Redacted>,
    },
    /// A workspace lease lifecycle event (lease acquire/release/inspect/apply/cleanup).
    WorkspaceEvent {
        kind: WorkspaceEvent,
        run_id: RunId,
        lease_id: String,
    },
    /// A worker adapter observed a vendor-created child worker, emitted
    /// even when the adapter declares `nested: none` -- emission alone
    /// never upgrades a declared capability; conformance/policy decide
    /// what an unexpected observation means.
    AdapterNestedWorkerEvent {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        vendor_child_id: String,
        vendor_parent_ref: String,
    },
    PolicyViolationRecorded {
        kind: RuntimeEventKind,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
    },
    PolicyViolationDecided {
        kind: RuntimeEventKind,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
    },
    /// A display backend attached or detached a Crew-owned pane.
    DisplayEvent {
        kind: RuntimeEventKind,
        run_id: RunId,
        backend: DisplayBackend,
        placement: DisplayPlacement,
        /// The vendor-assigned pane identifier only -- never terminal
        /// contents, never an absolute socket or filesystem path.
        pane_ref: String,
    },
    /// A human typed directly into a native pane, bypassing the
    /// adapter. Sets the run's `needsReconciliation` flag.
    OutOfBandInput {
        run_id: RunId,
        backend: DisplayBackend,
        pane_ref: String,
    },
    /// A leader proposed a decomposition of a run into subtasks via
    /// `plan/propose`, pending `plan/decide`.
    PlanProposed {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        plan: PlanSpec,
    },
    /// A previously proposed plan was approved or rejected via
    /// `plan/decide`.
    PlanDecided {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        approved: bool,
        /// `null` when no rationale was given for the decision.
        reason: Option<Redacted>,
    },
    // See AdapterMessageEvent above: a wholesale-sensitive fragment has
    // nothing Visible left to mask, so it's dropped rather than redacted.
    /// A worker asked a question that blocks its own progress, without
    /// escalating control (compare `escalationRaised`). `question`
    /// has already crossed the redaction boundary; `null` means the
    /// entire fragment was classified as sensitive and dropped, not
    /// that the question was empty.
    WorkerQuestion {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        question: Option<Redacted>,
    },
    /// A worker escalated a blocking condition to its leader or a human
    /// operator. `reason` is a plain, machine-assigned code (never raw
    /// worker content); `question` has already crossed the redaction
    /// boundary the same way `workerQuestion`'s field has.
    EscalationRaised {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        reason: String,
        question: Option<Redacted>,
    },
    /// An escalation was answered by the leader or a human user.
    /// `answer` has already crossed the redaction boundary the same way
    /// `workerQuestion`'s field has.
    EscalationAnswered {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        answered_by: AnsweredBy,
        answer: Option<Redacted>,
    },
    /// A worker exceeded its configured turn budget.
    BudgetExceeded {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        turns_used: u32,
        turn_limit: u32,
    },
    /// A worker's supervised process missed a liveness deadline.
    WorkerTimeout {
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        kind: TimeoutKind,
        #[ts(type = "number")]
        since_ms: u64,
    },
}

/// The envelope wrapping every durable runtime event, carrying its sequence
/// number and routing metadata.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct EventEnvelope {
    #[ts(type = "number")]
    pub sequence: u64,
    pub timestamp: Timestamp,
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    pub worker_id: Option<WorkerId>,
    pub run_id: Option<RunId>,
    pub parent_worker_id: Option<WorkerId>,
    pub source: EventSource,
    pub event: RuntimeEvent,
    pub vendor_event_ref: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_normalizes_offset_to_utc_z() {
        let ts = Timestamp::parse("2024-03-05T10:15:00+02:00").unwrap();
        assert_eq!(ts.as_str(), "2024-03-05T08:15:00Z");
    }

    #[test]
    fn timestamp_rejects_invalid_input() {
        assert!(Timestamp::parse("not a timestamp").is_err());
    }

    #[test]
    fn timestamp_serializes_as_plain_string() {
        let ts = Timestamp::parse("2024-01-01T00:00:00Z").unwrap();
        assert_eq!(
            serde_json::to_value(&ts).unwrap(),
            serde_json::json!("2024-01-01T00:00:00Z")
        );
    }

    #[test]
    fn runtime_event_unit_variant_is_adjacently_tagged() {
        let value = serde_json::to_value(RuntimeEvent::RuntimeStarted).unwrap();
        assert_eq!(value["type"], "runtimeStarted");
    }

    #[test]
    fn diagnostic_event_matches_exact_json_shape() {
        let event = RuntimeEvent::Diagnostic {
            level: DiagnosticLevel::Warning,
            code: "fixture".into(),
            message: "example".into(),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "type": "diagnostic",
                "payload": {
                    "level": "warning",
                    "code": "fixture",
                    "message": "example"
                }
            })
        );
    }

    #[test]
    fn classified_debug_redacts_secret_and_thinking_but_not_visible() {
        let visible = Classified {
            class: ContentClass::Visible,
            value: "plain narration".to_string(),
        };
        let secret = Classified {
            class: ContentClass::Secret,
            value: "sk-super-secret-value".to_string(),
        };
        let thinking = Classified {
            class: ContentClass::Thinking,
            value: "internal chain of thought".to_string(),
        };

        assert!(format!("{visible:?}").contains("plain narration"));
        assert!(!format!("{secret:?}").contains("sk-super-secret-value"));
        assert!(format!("{secret:?}").contains("<redacted>"));
        assert!(!format!("{thinking:?}").contains("internal chain of thought"));
        assert!(format!("{thinking:?}").contains("<redacted>"));
    }

    #[test]
    fn classified_is_not_reachable_from_runtime_event() {
        // Compile-time proof: RuntimeEvent's Diagnostic::message is a plain
        // String, not Classified<String>, so this construction is only
        // possible with sanitized content.
        let event = RuntimeEvent::Diagnostic {
            level: DiagnosticLevel::Info,
            code: "x".into(),
            message: "sanitized".into(),
        };
        match event {
            RuntimeEvent::Diagnostic { message, .. } => {
                let _: String = message;
            }
            _ => panic!("expected diagnostic"),
        }
    }

    fn fixture_ids() -> (RunId, TaskId, WorkerId) {
        (RunId::new(), TaskId::new(), WorkerId::new())
    }

    #[test]
    fn plan_proposed_round_trips_and_matches_shape() {
        let (run_id, task_id, worker_id) = fixture_ids();
        let event = RuntimeEvent::PlanProposed {
            run_id,
            task_id,
            worker_id,
            plan: PlanSpec {
                subtasks: vec![SubtaskSpec {
                    id: "sub-1".into(),
                    description: "write the tests".into(),
                    adapter: "claude".into(),
                    writes: true,
                    turn_budget: Some(10),
                }],
            },
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "planProposed");
        assert_eq!(value["payload"]["plan"]["subtasks"][0]["id"], "sub-1");
        assert_eq!(value["payload"]["plan"]["subtasks"][0]["writes"], true);
        assert_eq!(value["payload"]["plan"]["subtasks"][0]["turnBudget"], 10);

        let round_tripped: RuntimeEvent = serde_json::from_value(value).unwrap();
        match round_tripped {
            RuntimeEvent::PlanProposed { plan, .. } => assert_eq!(plan.subtasks.len(), 1),
            other => panic!("expected PlanProposed, got {other:?}"),
        }
    }

    #[test]
    fn plan_proposed_rejects_unknown_field_in_subtask() {
        let (run_id, task_id, worker_id) = fixture_ids();
        let value = serde_json::json!({
            "type": "planProposed",
            "payload": {
                "runId": run_id.to_string(),
                "taskId": task_id.to_string(),
                "workerId": worker_id.to_string(),
                "plan": {
                    "subtasks": [{
                        "id": "sub-1",
                        "description": "write the tests",
                        "adapter": "claude",
                        "writes": true,
                        "turnBudget": null,
                        "unexpected": "nope",
                    }]
                }
            }
        });
        assert!(serde_json::from_value::<RuntimeEvent>(value).is_err());
    }

    #[test]
    fn plan_decided_matches_exact_json_shape() {
        let (run_id, task_id, worker_id) = fixture_ids();
        let event = RuntimeEvent::PlanDecided {
            run_id,
            task_id,
            worker_id,
            approved: false,
            reason: Some(Redacted::assert_runtime_authored("scope too broad")),
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "planDecided");
        assert_eq!(value["payload"]["approved"], false);
        assert_eq!(value["payload"]["reason"], "scope too broad");
    }

    #[test]
    fn worker_question_none_means_fully_redacted_not_empty() {
        let (run_id, task_id, worker_id) = fixture_ids();
        let event = RuntimeEvent::WorkerQuestion {
            run_id,
            task_id,
            worker_id,
            question: None,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "workerQuestion");
        assert!(value["payload"]["question"].is_null());
    }

    #[test]
    fn escalation_raised_and_answered_round_trip() {
        let (run_id, task_id, worker_id) = fixture_ids();
        let raised = RuntimeEvent::EscalationRaised {
            run_id,
            task_id,
            worker_id,
            reason: "ambiguous_requirement".into(),
            question: Some(Redacted::assert_runtime_authored(
                "should this endpoint be idempotent?",
            )),
        };
        let value = serde_json::to_value(&raised).unwrap();
        assert_eq!(value["type"], "escalationRaised");
        assert_eq!(value["payload"]["reason"], "ambiguous_requirement");
        assert!(serde_json::from_value::<RuntimeEvent>(value).is_ok());

        let answered = RuntimeEvent::EscalationAnswered {
            run_id,
            task_id,
            worker_id,
            answered_by: AnsweredBy::User,
            answer: Some(Redacted::assert_runtime_authored("yes, make it idempotent")),
        };
        let value = serde_json::to_value(&answered).unwrap();
        assert_eq!(value["type"], "escalationAnswered");
        assert_eq!(value["payload"]["answeredBy"], "user");
        assert!(serde_json::from_value::<RuntimeEvent>(value).is_ok());
    }

    #[test]
    fn budget_exceeded_matches_exact_json_shape() {
        let (run_id, task_id, worker_id) = fixture_ids();
        let event = RuntimeEvent::BudgetExceeded {
            run_id,
            task_id,
            worker_id,
            turns_used: 12,
            turn_limit: 10,
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "budgetExceeded");
        assert_eq!(value["payload"]["turnsUsed"], 12);
        assert_eq!(value["payload"]["turnLimit"], 10);
    }

    #[test]
    fn worker_timeout_round_trips_both_kinds() {
        let (run_id, task_id, worker_id) = fixture_ids();
        for kind in [TimeoutKind::Inactivity, TimeoutKind::Total] {
            let event = RuntimeEvent::WorkerTimeout {
                run_id,
                task_id,
                worker_id,
                kind,
                since_ms: 90_000,
            };
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(value["type"], "workerTimeout");
            let round_tripped: RuntimeEvent = serde_json::from_value(value).unwrap();
            match round_tripped {
                RuntimeEvent::WorkerTimeout {
                    kind: k, since_ms, ..
                } => {
                    assert_eq!(k, kind);
                    assert_eq!(since_ms, 90_000);
                }
                other => panic!("expected WorkerTimeout, got {other:?}"),
            }
        }
    }

    #[test]
    fn classified_is_not_reachable_from_new_crew_v2_events() {
        // Compile-time proof mirroring `classified_is_not_reachable_from_runtime_event`:
        // every new free-text field on these variants is a plain
        // `Option<Redacted>`/`Redacted`, never `Classified<String>`, so raw
        // thinking/secret content can only reach these variants already
        // sanitized. CREW-29 strengthened this from `String`: the field type
        // now also names *which* boundary the text crossed, so the pin below
        // asserts something stricter than it used to rather than less.
        let (run_id, task_id, worker_id) = fixture_ids();
        let event = RuntimeEvent::WorkerQuestion {
            run_id,
            task_id,
            worker_id,
            question: Some(Redacted::assert_runtime_authored("sanitized")),
        };
        match event {
            RuntimeEvent::WorkerQuestion { question, .. } => {
                let _: Option<Redacted> = question;
            }
            _ => panic!("expected WorkerQuestion"),
        }
    }
}
