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

use crew_protocol::{
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
    /// A TUI adapter's transcript-tailer position reached by the batch
    /// this event concludes (WP12), persisted to `runs.transcript_cursor`
    /// in the same transaction as this event's journal insert. `None` for
    /// every non-TUI adapter, and for a TUI event that is not the last one
    /// emitted from its batch -- see
    /// `crate::adapter::tui::adapter::emit_tui_event`'s own doc comment
    /// for why only the batch's final emitted event carries it.
    pub cursor: Option<super::tui::Cursor>,
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
    /// A TUI adapter's transcript tailer classified an assistant message
    /// as a question awaiting a human answer (`TuiEvent::AssistantText {
    /// is_question: true, .. }`), rather than a completed message.
    QuestionDetected {
        text: Classified<String>,
    },
    /// A human typed directly into a TUI adapter's attached pane,
    /// bypassing the adapter's own input path. Carries no free text (the
    /// keystrokes themselves are never journaled) -- only that it
    /// happened, on which pane. Maps to [`RuntimeEvent::OutOfBandInput`]
    /// and additionally sets the run's `needsReconciliation` flag (see
    /// [`DomainAdapterEventSink::emit`]).
    OutOfBandInput {
        backend: crew_protocol::DisplayBackend,
        pane_ref: String,
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
            cursor: _,
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
            // WP12 lifecycle mapping decision: a detected question
            // journals `RuntimeEvent::WorkerQuestion` (the same event a
            // headless adapter's own question-detection would use), never
            // a run-state edge -- `waitingUser` is reserved for the
            // approval flow (ADR-0012), and idle/busy presentation is
            // derived by the extension, not tracked as a run state.
            // Raising an escalation for this (WP20) is deliberately not
            // done here; this call site only journals the question.
            AdapterEventPayload::QuestionDetected { text } => RuntimeEvent::WorkerQuestion {
                run_id,
                task_id,
                worker_id,
                question: self.sanitize(text),
            },
            AdapterEventPayload::OutOfBandInput { backend, pane_ref } => {
                RuntimeEvent::OutOfBandInput {
                    run_id,
                    backend,
                    pane_ref,
                }
            }
        }
    }
}

impl AdapterEventSink for DomainAdapterEventSink {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
        let run_id = event.run_id;
        let task_id = event.task_id;
        let worker_id = event.worker_id;
        // Extracted before `build_runtime_event` consumes `event` --
        // serialized here (rather than passed through as a typed
        // `Cursor`) so `DomainRepository::record_adapter_event` stays
        // oblivious to any specific adapter's cursor shape, storing only
        // the opaque JSON text `runs.transcript_cursor` was declared to
        // hold (migration 10). The `Result` is threaded into the async
        // block below (this function cannot `?` here -- it returns the
        // future itself, not a `Result`).
        let cursor_json = event.cursor.as_ref().map(serde_json::to_string).transpose();
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
        // A human bypassed the adapter and typed directly into the pane:
        // the run needs OMP's attention to reconcile whatever state that
        // produced. Set as a second, separate commit (mirroring the
        // nested-violation special-case below) rather than inside the
        // same `record_adapter_event` transaction, since flag mutation
        // is not itself evidence for `RunLifecycleSink`'s edges.
        let out_of_band = matches!(&runtime_event, RuntimeEvent::OutOfBandInput { .. });
        let violation_service = Arc::clone(&self.violation_service);
        Box::pin(async move {
            let cursor_json =
                cursor_json.map_err(|e| AdapterError::process("sink", "emit", e.to_string()))?;
            let mut result = db
                .run_domain_op(Box::new(move |conn| {
                    let mut repo = DomainRepository::new(conn, project_id);
                    repo.record_adapter_event(
                        &runtime_event,
                        task_id,
                        worker_id,
                        run_id,
                        cursor_json,
                    )
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

            if out_of_band {
                let mut flag_result = db
                    .run_domain_op(Box::new(move |conn| {
                        let mut repo = DomainRepository::new(conn, project_id);
                        repo.set_run_flag(run_id, crate::domain::RunFlag::NeedsReconciliation, true)
                            .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
                    }))
                    .await;
                match &mut flag_result {
                    Ok(value) => {
                        let _ = crate::domain::broadcast_committed(&events_tx, value);
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            run_id = %run_id,
                            "failed to set needsReconciliation after out-of-band input"
                        );
                    }
                }
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
            cursor: None,
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
            cursor: None,
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
            cursor: None,
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
            cursor: None,
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

#[cfg(test)]
mod out_of_band_input_tests {
    //! `AdapterEventPayload::OutOfBandInput` carries no free text at all
    //! (structurally -- the variant has no text field), so "redacted"
    //! here means the durable `RuntimeEvent` it produces never carries
    //! anything but the backend/pane_ref, and that emitting it also sets
    //! `needsReconciliation` -- both proved against a real database
    //! rather than the payload shape alone.

    use std::sync::Arc;

    use crew_protocol::{
        DisplayBackend, ProjectId, RunId, RunState, TaskId, Timestamp, Worker, WorkerId,
        WorkerProfileRef,
    };
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    use crate::config::NestedViolationAction;
    use crate::db::DatabaseHandle;
    use crate::domain::DomainRepository;
    use crate::policy::ViolationService;

    use super::*;

    async fn open_db() -> (TempDir, Arc<DatabaseHandle>) {
        let dir = tempfile::Builder::new()
            .prefix("bat-oob-sink-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            DatabaseHandle::start(db_path)
                .await
                .expect("start database"),
        );
        (dir, db)
    }

    /// Seeds one task + worker + `working` run (mirrors
    /// `super::super::run_lifecycle::tests::seed_run`, duplicated here
    /// since that helper is private to its own module).
    async fn seed_working_run(
        db: &DatabaseHandle,
        project_id: ProjectId,
    ) -> (TaskId, WorkerId, RunId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &crew_protocol::TaskRef {
                    owner_client_instance_id: "omp-1".to_string(),
                    revision: 1,
                },
            )?;
            let worker = Worker {
                worker_id,
                profile_ref: WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".to_string(),
                    adapter: "fake".to_string(),
                    model: "test".to_string(),
                    permission_envelope: serde_json::json!({}),
                },
                parent_worker_id: None,
                created_at: Timestamp::now(),
            };
            repo.create_worker(&worker)?;
            let run = crew_protocol::Run {
                run_id,
                task_id,
                worker_id,
                state: RunState::try_from("queued").expect("queued is a valid state"),
                flags: crew_protocol::RunFlags::default(),
                vendor_session_id: None,
                started_at: None,
                completed_at: None,
            };
            repo.submit_run(&run, None, None)?;
            for state in ["starting", "working"] {
                repo.transition_run(
                    run_id,
                    &RunState::try_from(state).expect("valid state"),
                    None,
                )?;
            }
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed working run");
        (task_id, worker_id, run_id)
    }

    fn sink(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
    ) -> DomainAdapterEventSink {
        let violation_service = Arc::new(ViolationService::new(
            Arc::clone(&db),
            project_id,
            events_tx.clone(),
            None,
            NestedViolationAction::QuarantineAndCancel,
        ));
        DomainAdapterEventSink::new(
            db,
            project_id,
            events_tx,
            Vec::new(),
            false,
            violation_service,
        )
        .expect("built-in redaction rules always compile")
    }

    #[tokio::test]
    async fn out_of_band_input_journals_no_free_text_and_sets_needs_reconciliation() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_working_run(&db, project_id).await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let sink = sink(Arc::clone(&db), project_id, events_tx);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::OutOfBandInput {
                backend: DisplayBackend::Tmux,
                pane_ref: "%3".to_string(),
            },
            cursor: None,
        })
        .await
        .expect("emit");

        let first = events_rx.try_recv().expect("OutOfBandInput must broadcast");
        match &first.event {
            RuntimeEvent::OutOfBandInput {
                run_id: got_run_id,
                backend,
                pane_ref,
            } => {
                assert_eq!(*got_run_id, run_id);
                assert_eq!(*backend, DisplayBackend::Tmux);
                assert_eq!(pane_ref, "%3");
            }
            other => panic!("expected OutOfBandInput, got {other:?}"),
        }
        // Structurally impossible to fail (the variant has no text
        // field), but pinned explicitly: nothing a human typed ever
        // appears in the durable event.
        let serialized = serde_json::to_string(&first.event).expect("serialize");
        assert!(
            !serialized.to_lowercase().contains("keystroke"),
            "no keystroke content may ever appear in an OutOfBandInput event: {serialized}"
        );

        let second = events_rx
            .try_recv()
            .expect("needsReconciliation must also broadcast");
        match &second.event {
            RuntimeEvent::RunFlagsEvent { run_id: got, flags } => {
                assert_eq!(*got, run_id);
                assert!(flags.needs_reconciliation, "flags: {flags:?}");
            }
            other => panic!("expected RunFlagsEvent, got {other:?}"),
        }

        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_failed_flag_write_is_logged_not_propagated_since_the_event_already_committed() {
        // A run that was never seeded has no row for `set_run_flag` to
        // update; the OutOfBandInput event itself must still succeed and
        // broadcast -- the flag side-effect is best-effort, never a
        // reason to fail an emit whose own journal write already
        // committed.
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let sink = sink(Arc::clone(&db), project_id, events_tx);
        let run_id = RunId::new();

        sink.emit(AdapterEvent {
            run_id,
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            payload: AdapterEventPayload::OutOfBandInput {
                backend: DisplayBackend::Hidden,
                pane_ref: String::new(),
            },
            cursor: None,
        })
        .await
        .expect("emit succeeds even though the flag write will fail");

        let first = events_rx
            .try_recv()
            .expect("OutOfBandInput must still broadcast");
        assert!(matches!(first.event, RuntimeEvent::OutOfBandInput { .. }));
        assert!(
            events_rx.try_recv().is_err(),
            "no RunFlagsEvent may broadcast when the run row does not exist"
        );

        db.shutdown().await.expect("shutdown database");
    }
}

#[cfg(test)]
mod question_detected_tests {
    //! WP12 step 3: `AdapterEventPayload::QuestionDetected` journals a
    //! durable `RuntimeEvent::WorkerQuestion` (not a bespoke
    //! `AdapterMessageEvent` kind) through the same redacted path every
    //! other free-text adapter field crosses, and never itself moves the
    //! run to `waitingUser` -- that state is reserved for the approval
    //! flow (ADR-0012). Raising an escalation for a detected question is
    //! WP20's job, out of scope here.

    use std::sync::Arc;

    use crew_protocol::{
        Classified, ContentClass, ProjectId, RunState, TaskId, Timestamp, Worker, WorkerId,
        WorkerProfileRef,
    };
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    use crate::config::NestedViolationAction;
    use crate::db::DatabaseHandle;
    use crate::domain::DomainRepository;
    use crate::policy::ViolationService;

    use super::*;

    async fn open_db() -> (TempDir, Arc<DatabaseHandle>) {
        let dir = tempfile::Builder::new()
            .prefix("bat-question-sink-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            DatabaseHandle::start(db_path)
                .await
                .expect("start database"),
        );
        (dir, db)
    }

    /// Seeds one task + worker + `working` run (mirrors
    /// `out_of_band_input_tests::seed_working_run`, duplicated here since
    /// that helper is private to its own module).
    async fn seed_working_run(
        db: &DatabaseHandle,
        project_id: ProjectId,
    ) -> (TaskId, WorkerId, RunId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &crew_protocol::TaskRef {
                    owner_client_instance_id: "omp-1".to_string(),
                    revision: 1,
                },
            )?;
            let worker = Worker {
                worker_id,
                profile_ref: WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".to_string(),
                    adapter: "fake".to_string(),
                    model: "test".to_string(),
                    permission_envelope: serde_json::json!({}),
                },
                parent_worker_id: None,
                created_at: Timestamp::now(),
            };
            repo.create_worker(&worker)?;
            let run = crew_protocol::Run {
                run_id,
                task_id,
                worker_id,
                state: RunState::try_from("queued").expect("queued is a valid state"),
                flags: crew_protocol::RunFlags::default(),
                vendor_session_id: None,
                started_at: None,
                completed_at: None,
            };
            repo.submit_run(&run, None, None)?;
            for state in ["starting", "working"] {
                repo.transition_run(
                    run_id,
                    &RunState::try_from(state).expect("valid state"),
                    None,
                )?;
            }
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed working run");
        (task_id, worker_id, run_id)
    }

    fn sink(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
    ) -> DomainAdapterEventSink {
        let violation_service = Arc::new(ViolationService::new(
            Arc::clone(&db),
            project_id,
            events_tx.clone(),
            None,
            NestedViolationAction::QuarantineAndCancel,
        ));
        DomainAdapterEventSink::new(
            db,
            project_id,
            events_tx,
            Vec::new(),
            false,
            violation_service,
        )
        .expect("built-in redaction rules always compile")
    }

    #[tokio::test]
    async fn question_detected_journals_a_redacted_worker_question_and_leaves_run_state_unchanged()
    {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_working_run(&db, project_id).await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let sink = sink(Arc::clone(&db), project_id, events_tx);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::QuestionDetected {
                text: Classified {
                    class: ContentClass::Visible,
                    value: "which branch should I target?".to_string(),
                },
            },
            cursor: None,
        })
        .await
        .expect("emit");

        let envelope = events_rx
            .try_recv()
            .expect("QuestionDetected must broadcast a WorkerQuestion");
        match &envelope.event {
            RuntimeEvent::WorkerQuestion {
                run_id: got_run_id,
                task_id: got_task_id,
                worker_id: got_worker_id,
                question,
            } => {
                assert_eq!(*got_run_id, run_id);
                assert_eq!(*got_task_id, task_id);
                assert_eq!(*got_worker_id, worker_id);
                assert_eq!(question.as_deref(), Some("which branch should I target?"));
            }
            other => panic!("expected WorkerQuestion, got {other:?}"),
        }
        assert!(
            events_rx.try_recv().is_err(),
            "a detected question must not also emit a run-state transition"
        );

        let state = db
            .run_domain_op(Box::new(move |conn| {
                let state: String = conn.query_row(
                    "SELECT state FROM runs WHERE run_id = ?1",
                    [run_id.to_string()],
                    |r| r.get(0),
                )?;
                Ok(serde_json::json!(state))
            }))
            .await
            .expect("read run state");
        assert_eq!(
            state.as_str().expect("state is a string"),
            "working",
            "QuestionDetected must not move the run toward waitingUser (ADR-0012 reserves \
             that state for the approval flow)"
        );

        db.shutdown().await.expect("shutdown database");
    }

    /// A `Thinking`/`Secret`-classified question fragment is dropped to
    /// `None`, exactly like every other free-text field this sink
    /// redacts -- never coerced to an empty string.
    #[tokio::test]
    async fn a_non_visible_question_fragment_is_dropped_to_none() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_working_run(&db, project_id).await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let sink = sink(Arc::clone(&db), project_id, events_tx);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::QuestionDetected {
                text: Classified {
                    class: ContentClass::Thinking,
                    value: "internal deliberation".to_string(),
                },
            },
            cursor: None,
        })
        .await
        .expect("emit");

        let envelope = events_rx.try_recv().expect("must still broadcast");
        match &envelope.event {
            RuntimeEvent::WorkerQuestion { question, .. } => {
                assert!(question.is_none(), "Thinking content must never be durable");
            }
            other => panic!("expected WorkerQuestion, got {other:?}"),
        }

        db.shutdown().await.expect("shutdown database");
    }
}

#[cfg(test)]
mod crash_resume_tests {
    //! WP12 step 2's crash-resume proof: a tailer that crashes mid-run and
    //! restarts from the cursor this sink persisted in `runs.transcript_cursor`
    //! re-tails with zero duplicate events -- proved against the real
    //! journal, not just the in-memory `Cursor` math `tui_tailer.rs`
    //! already covers.

    use std::io::Write;
    use std::sync::Arc;

    use crew_protocol::{
        ContentClass, ProjectId, RunFlags, RunState, TaskId, Timestamp, Worker, WorkerId,
        WorkerProfileRef,
    };
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    use crate::adapter::tui::{Cursor, TranscriptFormat, TranscriptTailer, TuiEvent};
    use crate::config::NestedViolationAction;
    use crate::db::DatabaseHandle;
    use crate::domain::DomainRepository;
    use crate::policy::ViolationService;

    use super::*;

    /// The same minimal vendor-shaped JSONL format `tui_tailer.rs` tests
    /// against, extended with a `"turn_end"` type that produces
    /// `TuiEvent::TurnEnded` -- the shape that exposed the cursor-
    /// placement bug this test suite guards against: a batch whose last
    /// entry emits nothing at all.
    struct TestFormat;

    impl TranscriptFormat for TestFormat {
        fn parse(&self, raw: &[u8], cursor: &Cursor) -> (Vec<TuiEvent>, Cursor) {
            crate::adapter::tui::parse_jsonl_chunk(raw, cursor, |value| {
                let entry_id = value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let event = match value.get("type").and_then(|v| v.as_str()) {
                    Some("turn_end") => TuiEvent::TurnEnded,
                    _ => TuiEvent::AssistantText {
                        text: Classified {
                            class: ContentClass::Visible,
                            value: value
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        },
                        is_question: false,
                        ts: None,
                    },
                };
                (vec![event], entry_id)
            })
        }
    }

    fn append_line(path: &std::path::Path, line: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open transcript for append");
        writeln!(file, "{line}").expect("append line");
    }

    async fn open_db() -> (TempDir, Arc<DatabaseHandle>) {
        let dir = tempfile::Builder::new()
            .prefix("bat-crash-resume-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            DatabaseHandle::start(db_path)
                .await
                .expect("start database"),
        );
        (dir, db)
    }

    /// Mirrors `question_detected_tests::seed_working_run` (duplicated --
    /// private to its own module).
    async fn seed_working_run(
        db: &DatabaseHandle,
        project_id: ProjectId,
    ) -> (TaskId, WorkerId, RunId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &crew_protocol::TaskRef {
                    owner_client_instance_id: "omp-1".to_string(),
                    revision: 1,
                },
            )?;
            let worker = Worker {
                worker_id,
                profile_ref: WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".to_string(),
                    adapter: "fake".to_string(),
                    model: "test".to_string(),
                    permission_envelope: serde_json::json!({}),
                },
                parent_worker_id: None,
                created_at: Timestamp::now(),
            };
            repo.create_worker(&worker)?;
            let run = crew_protocol::Run {
                run_id,
                task_id,
                worker_id,
                state: RunState::try_from("queued").expect("queued is a valid state"),
                flags: RunFlags::default(),
                vendor_session_id: None,
                started_at: None,
                completed_at: None,
            };
            repo.submit_run(&run, None, None)?;
            for state in ["starting", "working"] {
                repo.transition_run(
                    run_id,
                    &RunState::try_from(state).expect("valid state"),
                    None,
                )?;
            }
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed working run");
        (task_id, worker_id, run_id)
    }

    fn sink(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
    ) -> DomainAdapterEventSink {
        let violation_service = Arc::new(ViolationService::new(
            Arc::clone(&db),
            project_id,
            events_tx.clone(),
            None,
            NestedViolationAction::QuarantineAndCancel,
        ));
        DomainAdapterEventSink::new(
            db,
            project_id,
            events_tx,
            Vec::new(),
            false,
            violation_service,
        )
        .expect("built-in redaction rules always compile")
    }

    /// Emits one batch of `TestFormat` events through `sink`, attaching
    /// `cursor` to the batch's last *emitting* event -- calling the exact
    /// same `crate::adapter::tui::last_emitting_index` helper
    /// `crate::adapter::tui::adapter::emit_tui_event`'s production pump
    /// loop uses, rather than reimplementing the placement rule, so this
    /// test cannot silently drift from (and therefore cannot fail to
    /// catch a regression in) production's actual placement.
    /// `TuiEvent::TurnEnded` emits nothing, matching `emit_tui_event`.
    async fn emit_batch(
        sink: &DomainAdapterEventSink,
        run_id: RunId,
        task_id: TaskId,
        worker_id: WorkerId,
        events: Vec<TuiEvent>,
        cursor: Cursor,
    ) {
        let last_emitting = crate::adapter::tui::last_emitting_index(&events);
        for (index, event) in events.into_iter().enumerate() {
            let batch_cursor = if Some(index) == last_emitting {
                Some(cursor.clone())
            } else {
                None
            };
            match event {
                TuiEvent::AssistantText { text, .. } => {
                    sink.emit(AdapterEvent {
                        run_id,
                        task_id,
                        worker_id,
                        payload: AdapterEventPayload::MessageFinal {
                            role: "assistant".to_string(),
                            text,
                        },
                        cursor: batch_cursor,
                    })
                    .await
                    .expect("emit");
                }
                TuiEvent::TurnEnded => {
                    assert!(
                        batch_cursor.is_none(),
                        "TurnEnded emits nothing, so it must never be handed a cursor to lose"
                    );
                }
                other => panic!("TestFormat only produces AssistantText/TurnEnded: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_tailer_resumed_from_the_stored_cursor_re_tails_with_zero_duplicate_events() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_working_run(&db, project_id).await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let sink = sink(Arc::clone(&db), project_id, events_tx);

        let transcript_dir = tempfile::Builder::new()
            .prefix("bat-crash-resume-transcript-")
            .tempdir_in("/tmp")
            .expect("transcript dir");
        let transcript_path = transcript_dir.path().join("session.jsonl");

        // First pass: two lines are already on disk when the (first)
        // tailer starts.
        append_line(
            &transcript_path,
            r#"{"type":"text","text":"first","id":"1"}"#,
        );
        append_line(
            &transcript_path,
            r#"{"type":"text","text":"second","id":"2"}"#,
        );

        let mut tailer_one = TranscriptTailer::new(
            transcript_path.clone(),
            Arc::new(TestFormat),
            Cursor::start(),
            std::time::Duration::from_millis(10),
        );
        let (events, cursor_after_first_pass) = tailer_one
            .poll_once()
            .await
            .expect("the two pre-existing lines are consumed");
        assert_eq!(events.len(), 2);
        emit_batch(
            &sink,
            run_id,
            task_id,
            worker_id,
            events,
            cursor_after_first_pass.clone(),
        )
        .await;

        // The cursor committed in the same transaction as the batch's
        // last event is now durable.
        let stored_cursor_json: String = db
            .run_domain_op(Box::new(move |conn| {
                let json: String = conn.query_row(
                    "SELECT transcript_cursor FROM runs WHERE run_id = ?1",
                    [run_id.to_string()],
                    |r| r.get(0),
                )?;
                Ok(serde_json::json!(json))
            }))
            .await
            .expect("read stored cursor")
            .as_str()
            .expect("stored cursor is a string")
            .to_string();
        let stored_cursor: Cursor =
            serde_json::from_str(&stored_cursor_json).expect("stored cursor deserializes");
        assert_eq!(stored_cursor, cursor_after_first_pass);

        // Simulated crash: the first tailer is dropped (never told to
        // stop cleanly -- a crash gives no such chance) and the vendor
        // CLI keeps appending while the daemon is down.
        drop(tailer_one);
        append_line(
            &transcript_path,
            r#"{"type":"text","text":"third","id":"3"}"#,
        );

        // A fresh tailer, resumed from the *stored* cursor (not
        // `Cursor::start()`), re-tails.
        let mut tailer_two = TranscriptTailer::new(
            transcript_path.clone(),
            Arc::new(TestFormat),
            stored_cursor,
            std::time::Duration::from_millis(10),
        );
        let (events, cursor_after_second_pass) = tailer_two
            .poll_once()
            .await
            .expect("only the newly appended line is unconsumed");
        assert_eq!(
            events.len(),
            1,
            "resuming from the stored cursor must not re-deliver the first pass's lines"
        );
        emit_batch(
            &sink,
            run_id,
            task_id,
            worker_id,
            events,
            cursor_after_second_pass,
        )
        .await;

        // The journal holds exactly three `AdapterMessageFinal` events,
        // each with distinct text -- no duplicate from re-tailing.
        let texts = journaled_message_final_texts(&db, run_id).await;

        assert_eq!(
            texts,
            vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string()
            ],
            "the journal must hold exactly the three distinct lines, in order, with no duplicate \
             from re-tailing after the simulated crash"
        );

        db.shutdown().await.expect("shutdown database");
    }

    /// The exact regression this suite guards against: a batch whose
    /// *last* `TuiEvent` emits nothing (`TurnEnded`, the shape a worker's
    /// transcript takes right after finishing a turn and going idle --
    /// the common case, not an edge case). Before the fix, the cursor was
    /// attached unconditionally to the batch's last *index*, so it was
    /// dropped here even though `"first"` was already journaled; a crash
    /// then resumed from the pre-first-line cursor and re-journaled
    /// `"first"`. With the fix, the cursor rides the batch's last
    /// *emitting* event (`"first"`'s own commit) and covers the
    /// `TurnEnded` entry's bytes too, so resuming re-delivers nothing
    /// already seen.
    #[tokio::test]
    async fn a_batch_ending_in_turn_ended_still_persists_its_cursor_on_the_last_emitting_event() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_working_run(&db, project_id).await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let sink = sink(Arc::clone(&db), project_id, events_tx);

        let transcript_dir = tempfile::Builder::new()
            .prefix("bat-crash-resume-turn-ended-")
            .tempdir_in("/tmp")
            .expect("transcript dir");
        let transcript_path = transcript_dir.path().join("session.jsonl");

        // One assistant message, then the turn ends -- both already on
        // disk when the tailer first polls, so they land in one batch.
        append_line(
            &transcript_path,
            r#"{"type":"text","text":"first","id":"1"}"#,
        );
        append_line(&transcript_path, r#"{"type":"turn_end","id":"end-1"}"#);

        let mut tailer_one = TranscriptTailer::new(
            transcript_path.clone(),
            Arc::new(TestFormat),
            Cursor::start(),
            std::time::Duration::from_millis(10),
        );
        let (events, cursor_after_batch) = tailer_one
            .poll_once()
            .await
            .expect("both lines are consumed in one batch");
        assert_eq!(events.len(), 2, "AssistantText then TurnEnded");
        assert_eq!(
            crate::adapter::tui::last_emitting_index(&events),
            Some(0),
            "the AssistantText at index 0 is the batch's only emitting event"
        );
        emit_batch(
            &sink,
            run_id,
            task_id,
            worker_id,
            events,
            cursor_after_batch.clone(),
        )
        .await;

        // The stored cursor covers the *whole* batch (past the
        // TurnEnded entry too), even though TurnEnded itself never
        // carried it -- it rode the AssistantText commit instead.
        let stored_cursor_json: String = db
            .run_domain_op(Box::new(move |conn| {
                let json: String = conn.query_row(
                    "SELECT transcript_cursor FROM runs WHERE run_id = ?1",
                    [run_id.to_string()],
                    |r| r.get(0),
                )?;
                Ok(serde_json::json!(json))
            }))
            .await
            .expect("read stored cursor")
            .as_str()
            .expect("stored cursor is a string")
            .to_string();
        let stored_cursor: Cursor =
            serde_json::from_str(&stored_cursor_json).expect("stored cursor deserializes");
        assert_eq!(
            stored_cursor, cursor_after_batch,
            "the persisted cursor must cover the entire batch, including the trailing \
             TurnEnded entry that carried no cursor itself"
        );

        // Simulated crash + resume: the worker was still idle (no new
        // turn started) when the daemon came back, then said something
        // new.
        drop(tailer_one);
        append_line(
            &transcript_path,
            r#"{"type":"text","text":"second","id":"2"}"#,
        );
        let mut tailer_two = TranscriptTailer::new(
            transcript_path.clone(),
            Arc::new(TestFormat),
            stored_cursor,
            std::time::Duration::from_millis(10),
        );
        let (events, cursor_after_resume) = tailer_two
            .poll_once()
            .await
            .expect("only the newly appended line is unconsumed");
        assert_eq!(
            events.len(),
            1,
            "resuming must not re-deliver \"first\" or the TurnEnded entry"
        );
        emit_batch(
            &sink,
            run_id,
            task_id,
            worker_id,
            events,
            cursor_after_resume,
        )
        .await;

        let texts = journaled_message_final_texts(&db, run_id).await;
        assert_eq!(
            texts,
            vec!["first".to_string(), "second".to_string()],
            "\"first\" must be journaled exactly once, not re-journaled after the crash"
        );

        db.shutdown().await.expect("shutdown database");
    }

    /// Every `AdapterMessageFinal` text journaled for `run_id`, in
    /// commit order. Shared by every test in this module that asserts
    /// on the real journal contents rather than the in-memory broadcast.
    async fn journaled_message_final_texts(db: &DatabaseHandle, run_id: RunId) -> Vec<String> {
        db.run_domain_op(Box::new(move |conn| {
            let mut stmt =
                conn.prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
            let rows: Vec<String> = stmt
                .query_map([run_id.to_string()], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?;
            let texts = rows
                .into_iter()
                .filter_map(|raw| {
                    let event: RuntimeEvent = serde_json::from_str(&raw).ok()?;
                    match event {
                        RuntimeEvent::AdapterMessageEvent {
                            kind: RuntimeEventKind::AdapterMessageFinal,
                            text: Some(text),
                            ..
                        } => Some(text),
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            Ok(serde_json::json!(texts))
        }))
        .await
        .expect("read journal")
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_string())
        .collect()
    }
}
