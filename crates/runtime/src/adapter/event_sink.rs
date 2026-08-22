//! The adapter event sink: the sole path from a worker adapter's raw,
//! possibly-classified output into the durable, correlated `RuntimeEvent`
//! journal.
//!
//! Adapters call [`AdapterEventSink::emit`] rather than writing
//! [`crate::domain::DomainRepository`] directly, per the Worker Adapters
//! plan's Global Constraints. Any free-text field (`role`+`text`, tool
//! `detail`, protocol-health `detail`) is carried as `Classified<String>`
//! and crosses the redaction boundary
//! (`crate::security::redaction::Redactor`) before it becomes part of the
//! plain `RuntimeEvent` the journal accepts -- reusing exactly the
//! "raw classified -> sanitized" shape `crate::security::redaction`
//! already enforces for foundation events, rather than re-implementing it.
//!
//! **Adapters should filter out fragments classified as `Thinking` before
//! ever constructing an [`AdapterEvent`]** (e.g. the Claude adapter's
//! normalization discards thinking blocks before they reach the sink at
//! all -- see the Claude adapter task). This sink's own `None`-on-drop
//! handling below is a defensive backstop, not the primary mechanism: if a
//! non-`Visible` fragment does reach `emit`, its text/detail becomes
//! `None` in the durable event, never an empty string that could be
//! mistaken for genuinely empty content.

use std::sync::Arc;

use batman_protocol::{
    ArtifactId, Classified, EventEnvelope, ProjectId, RunId, RuntimeEvent, RuntimeEventKind,
    TaskId, WorkerId,
};
use serde_json::json;
use tokio::sync::{broadcast, oneshot};

use crate::db::DatabaseHandle;
use crate::domain::{DomainRepository, embed_envelope};
use crate::security::redaction::Redactor;

use super::AdapterFuture;
use super::error::AdapterError;

/// One normalized event a worker adapter reports for a single run, before
/// it crosses the redaction boundary.
#[derive(Debug, Clone)]
pub struct AdapterEvent {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub worker_id: WorkerId,
    pub payload: AdapterEventPayload,
}

/// The raw, pre-redaction payload of an [`AdapterEvent`]. Free-text fields
/// are [`Classified<String>`]; everything else is already narrow,
/// structured, vendor-assigned data (ids, counters, short labels) that
/// carries no narrative content and therefore needs no classification.
#[derive(Debug, Clone)]
pub enum AdapterEventPayload {
    ProcessStarted {
        pid: u32,
    },
    ProcessExited {
        exit_code: Option<i32>,
        signal: Option<String>,
    },
    VendorSessionEstablished {
        vendor_session_id: String,
    },
    MessageChunk {
        role: String,
        text: Classified<String>,
    },
    MessageFinal {
        role: String,
        text: Classified<String>,
    },
    ToolStarted {
        tool_call_id: String,
        name: String,
    },
    ToolProgress {
        tool_call_id: String,
        name: String,
        detail: Classified<String>,
    },
    ToolResult {
        tool_call_id: String,
        name: String,
        ok: bool,
        detail: Classified<String>,
    },
    UsageReported {
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: Option<f64>,
    },
    ArtifactProduced {
        artifact_id: ArtifactId,
        artifact_kind: String,
    },
    ProtocolHealthChanged {
        healthy: bool,
        detail: Classified<String>,
    },
    /// Emitted even when the reporting adapter declares `nested: none` --
    /// emission alone never upgrades a declared capability. Never carries
    /// classified content: vendor child/parent references are opaque
    /// vendor-assigned identifiers, not narrative text.
    NestedWorkerObserved {
        vendor_child_id: String,
        vendor_parent_ref: String,
    },
}

/// Adapters push ordered normalized events into the runtime journal
/// through this trait rather than touching [`DomainRepository`] directly.
/// `emit` resolves to the durably committed runtime sequence number.
pub trait AdapterEventSink: Send + Sync {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64>;
}

/// The production [`AdapterEventSink`]: sanitizes, journals (correlated to
/// task/worker/run), and broadcasts to live `events/subscribe` listeners,
/// mirroring exactly what every `OrchestrationService` mutation already
/// does after `append_and_apply` (see `docs/architecture.md` §18 item 3 --
/// a mutation that doesn't broadcast breaks the monitor silently).
pub struct DomainAdapterEventSink {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    events_tx: broadcast::Sender<EventEnvelope>,
    redactor: Arc<Redactor>,
    /// Whether this run's effective `nested` capability is anything other
    /// than `NestedCapability::Managed` -- per that type's own doc
    /// comment, "only `Managed` permits nesting at all", so `None` and
    /// `Observable` are both ungoverned: `NestedWorkerObserved` firing
    /// while this is `true` is a mid-run policy violation (Hardening
    /// plan Task 1), not merely a journaled observation.
    nested_not_managed: bool,
    violation_service: Arc<crate::policy::ViolationService>,
}

impl DomainAdapterEventSink {
    /// Builds the sink, compiling the org redaction patterns. Fails
    /// closed: a sink that cannot build its redactor must not exist,
    /// because content is redacted before it becomes durable (invariant
    /// 4). Unreachable from today's startup path -- `lifecycle.rs`
    /// validates `policy.org_security_patterns` and refuses to serve --
    /// but a future path (config reload, an alternate constructor) that
    /// feeds unvalidated patterns must get an error, not a silently
    /// weaker built-in-rules-only redactor (R14).
    ///
    /// # Errors
    /// Returns the pattern-compilation error verbatim.
    pub fn new(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
        org_security_patterns: Vec<String>,
        nested_not_managed: bool,
        violation_service: Arc<crate::policy::ViolationService>,
    ) -> Result<Self, String> {
        let redactor = Arc::new(Redactor::with_org_rules(&org_security_patterns)?);
        Ok(Self {
            db,
            project_id,
            events_tx,
            redactor,
            nested_not_managed,
            violation_service,
        })
    }

    /// Sanitizes a single classified fragment: `Visible` text is
    /// redaction-scanned and kept; `Thinking`/`Secret` fragments are
    /// dropped to `None`, never coerced to an empty string.
    fn sanitize(&self, fragment: Classified<String>) -> Option<String> {
        self.redactor.sanitize_fragment(&fragment)
    }

    /// Applies the same built-in regex rules to a short, always-visible,
    /// vendor-sourced label (never dropped for classification -- see
    /// [`Redactor::redact_text`]).
    fn label(&self, text: String) -> String {
        self.redactor.redact_text(&text)
    }

    fn build_runtime_event(&self, event: AdapterEvent) -> RuntimeEvent {
        let AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload,
        } = event;
        match payload {
            AdapterEventPayload::ProcessStarted { pid } => RuntimeEvent::AdapterProcessEvent {
                kind: RuntimeEventKind::AdapterProcessStarted,
                run_id,
                task_id,
                worker_id,
                pid: Some(pid),
                exit_code: None,
                signal: None,
            },
            AdapterEventPayload::ProcessExited { exit_code, signal } => {
                RuntimeEvent::AdapterProcessEvent {
                    kind: RuntimeEventKind::AdapterProcessExited,
                    run_id,
                    task_id,
                    worker_id,
                    pid: None,
                    exit_code,
                    signal: signal.map(|s| self.label(s)),
                }
            }
            AdapterEventPayload::VendorSessionEstablished { vendor_session_id } => {
                RuntimeEvent::AdapterVendorSessionEvent {
                    run_id,
                    task_id,
                    worker_id,
                    vendor_session_id: self.label(vendor_session_id),
                }
            }
            AdapterEventPayload::MessageChunk { role, text } => RuntimeEvent::AdapterMessageEvent {
                kind: RuntimeEventKind::AdapterMessageChunk,
                run_id,
                task_id,
                worker_id,
                role: self.label(role),
                text: self.sanitize(text),
            },
            AdapterEventPayload::MessageFinal { role, text } => RuntimeEvent::AdapterMessageEvent {
                kind: RuntimeEventKind::AdapterMessageFinal,
                run_id,
                task_id,
                worker_id,
                role: self.label(role),
                text: self.sanitize(text),
            },
            AdapterEventPayload::ToolStarted { tool_call_id, name } => {
                RuntimeEvent::AdapterToolEvent {
                    kind: RuntimeEventKind::AdapterToolStarted,
                    run_id,
                    task_id,
                    worker_id,
                    tool_call_id: self.label(tool_call_id),
                    name: self.label(name),
                    ok: None,
                    detail: None,
                }
            }
            AdapterEventPayload::ToolProgress {
                tool_call_id,
                name,
                detail,
            } => RuntimeEvent::AdapterToolEvent {
                kind: RuntimeEventKind::AdapterToolProgress,
                run_id,
                task_id,
                worker_id,
                tool_call_id: self.label(tool_call_id),
                name: self.label(name),
                ok: None,
                detail: self.sanitize(detail),
            },
            AdapterEventPayload::ToolResult {
                tool_call_id,
                name,
                ok,
                detail,
            } => RuntimeEvent::AdapterToolEvent {
                kind: RuntimeEventKind::AdapterToolResult,
                run_id,
                task_id,
                worker_id,
                tool_call_id: self.label(tool_call_id),
                name: self.label(name),
                ok: Some(ok),
                detail: self.sanitize(detail),
            },
            AdapterEventPayload::UsageReported {
                input_tokens,
                output_tokens,
                cost_usd,
            } => RuntimeEvent::AdapterUsageEvent {
                run_id,
                task_id,
                worker_id,
                input_tokens,
                output_tokens,
                cost_usd,
            },
            AdapterEventPayload::ArtifactProduced {
                artifact_id,
                artifact_kind,
            } => RuntimeEvent::AdapterArtifactEvent {
                run_id,
                task_id,
                worker_id,
                artifact_id,
                artifact_kind: self.label(artifact_kind),
            },
            AdapterEventPayload::ProtocolHealthChanged { healthy, detail } => {
                RuntimeEvent::AdapterProtocolHealthEvent {
                    run_id,
                    task_id,
                    worker_id,
                    healthy,
                    detail: self.sanitize(detail),
                }
            }
            AdapterEventPayload::NestedWorkerObserved {
                vendor_child_id,
                vendor_parent_ref,
            } => RuntimeEvent::AdapterNestedWorkerEvent {
                run_id,
                task_id,
                worker_id,
                vendor_child_id: self.label(vendor_child_id),
                vendor_parent_ref: self.label(vendor_parent_ref),
            },
        }
    }
}

impl AdapterEventSink for DomainAdapterEventSink {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
        let run_id = event.run_id;
        let task_id = event.task_id;
        let worker_id = event.worker_id;
        let runtime_event = self.build_runtime_event(event);
        let project_id = self.project_id;
        let events_tx = self.events_tx.clone();
        let db = self.db.clone();
        // Extract the already-redacted (`self.label`-passed) vendor
        // fields from the just-built event, rather than the original
        // raw `AdapterEventPayload` -- the durable `policy_violations`
        // table must never receive unsanitized vendor-subprocess text,
        // matching every other adapter-sourced field this sink journals.
        let nested_violation = if self.nested_not_managed {
            match &runtime_event {
                RuntimeEvent::AdapterNestedWorkerEvent {
                    vendor_child_id,
                    vendor_parent_ref,
                    ..
                } => Some((vendor_child_id.clone(), vendor_parent_ref.clone())),
                _ => None,
            }
        } else {
            None
        };
        let violation_service = Arc::clone(&self.violation_service);
        Box::pin(async move {
            let mut result = db
                .run_domain_op(Box::new(move |conn| {
                    let mut repo = DomainRepository::new(conn, project_id);
                    repo.record_adapter_event(&runtime_event, task_id, worker_id, run_id)
                        .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
                }))
                .await
                .map_err(|e| AdapterError::process("sink", "emit", e.to_string()))?;
            let sequence = crate::domain::broadcast_committed(&events_tx, &mut result)
                .ok_or_else(|| AdapterError::process("sink", "emit", "no envelope committed"))?;

            if let Some((vendor_child_id, vendor_parent_ref)) = nested_violation
                && let Err(err) = violation_service
                    .record_nested_worker(
                        run_id,
                        task_id,
                        worker_id,
                        &vendor_child_id,
                        &vendor_parent_ref,
                        sequence,
                    )
                    .await
            {
                tracing::warn!(
                    error = %err,
                    run_id = %run_id,
                    "failed to record mid-run nested-worker policy violation"
                );
            }

            Ok(sequence)
        })
    }
}

/// Wraps a run's [`AdapterEventSink`] and reports terminal settlement
/// exactly once: the first [`AdapterEventPayload::ProcessExited`] this run
/// emits fires the paired receiver, after the inner sink has finished
/// journaling it -- which, in the production chain where the inner sink is
/// [`super::run_lifecycle::RunLifecycleSink`], also means after the run's
/// terminal `RunState` edge is durable, ordering the state before the
/// concurrency-slot release this signal triggers. Observing the payload here
/// rather than re-reading it off the shared `events/subscribe` broadcast makes
/// the signal per-run, immune to a lagged broadcast receiver, and impossible
/// to miss by subscribing too late.
pub(crate) struct SettlementSink {
    inner: Arc<dyn AdapterEventSink>,
    settled: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

impl SettlementSink {
    /// Wraps `inner`, returning the sink to hand to the adapter and the
    /// receiver that resolves once this run's vendor process has exited.
    #[must_use]
    pub(crate) fn wrap(
        inner: Arc<dyn AdapterEventSink>,
    ) -> (Arc<dyn AdapterEventSink>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Arc::new(Self {
                inner,
                settled: std::sync::Mutex::new(Some(tx)),
            }),
            rx,
        )
    }
}

impl AdapterEventSink for SettlementSink {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
        // Match by reference: `event` is moved into the inner `emit`
        // below, so a by-value match would consume the payload before
        // the inner sink needs it.
        let is_exit = matches!(&event.payload, AdapterEventPayload::ProcessExited { .. });
        Box::pin(async move {
            let result = self.inner.emit(event).await;
            // Fired even when the journal write failed: the process has
            // exited either way, and a lost journal row must never also
            // cost the run its concurrency slot. Liveness over durability
            // here: a leaked slot would permanently block the system.
            if is_exit
                && let Some(tx) = self
                    .settled
                    .lock()
                    .expect("settlement mutex is never poisoned")
                    .take()
            {
                let _ = tx.send(());
            }
            result
        })
    }
}

#[cfg(test)]
mod settlement_sink_tests {
    use super::*;

    struct StubSink;

    impl AdapterEventSink for StubSink {
        fn emit(&self, _event: AdapterEvent) -> AdapterFuture<'_, u64> {
            Box::pin(async { Ok(0) })
        }
    }

    #[tokio::test]
    async fn a_non_exit_payload_leaves_the_receiver_pending() {
        let (sink, mut rx) = SettlementSink::wrap(Arc::new(StubSink));
        sink.emit(AdapterEvent {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            payload: AdapterEventPayload::ProcessStarted { pid: 1 },
        })
        .await
        .expect("emit");
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "receiver should still be pending after non-exit payload"
        );
    }

    #[tokio::test]
    async fn an_exit_payload_resolves_the_receiver() {
        let (sink, mut rx) = SettlementSink::wrap(Arc::new(StubSink));
        sink.emit(AdapterEvent {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
        })
        .await
        .expect("emit");
        assert!(
            rx.try_recv().is_ok(),
            "receiver should resolve after ProcessExited"
        );
    }

    #[tokio::test]
    async fn a_duplicate_exit_payload_fires_the_receiver_only_once() {
        let (sink, mut rx) = SettlementSink::wrap(Arc::new(StubSink));
        // First emit fires the receiver
        sink.emit(AdapterEvent {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
        })
        .await
        .expect("first emit");
        assert!(rx.try_recv().is_ok(), "first emit should fire receiver");

        // Second emit still succeeds but does not fire again
        sink.emit(AdapterEvent {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(1),
                signal: None,
            },
        })
        .await
        .expect("second emit");
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed)
            ),
            "second emit must not fire receiver again (channel already consumed)"
        );
    }
}
