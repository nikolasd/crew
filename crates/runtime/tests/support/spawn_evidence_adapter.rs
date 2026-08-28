//! `SpawnEvidenceAdapter`: a minimal, protocol-agnostic [`Adapter`] that
//! spawns a real OS process and forwards only its spawn/exit evidence
//! (`ProcessStarted`/`ProcessExited`, plus one synthetic non-exit signal --
//! see `start()`) through the sink. It implements no vendor wire protocol
//! at all.
//!
//! ## Why this exists
//!
//! `tests/run_lifecycle.rs` and `tests/orchestration_rpc.rs` both need a
//! REAL spawned OS process driven through the REAL [`Adapter`] trait to
//! prove `RunLifecycleSink`/`AdapterRegistry`/cancellation behavior against
//! actual process-lifecycle evidence -- a fake/stub `Adapter` impl that
//! only emits pre-scripted events would prove nothing about the
//! evidence-driven contract those files exist to guard. Before crew-v2
//! gap-closure WP-C, both files used the real, production `OmpRpcAdapter`
//! for exactly this, even though neither test cares about OMP-RPC's own
//! wire protocol -- `OmpRpcAdapter` was simply the only real-process
//! adapter available to reach for. WP-C deleted the entire headless
//! control plane (`OmpRpcAdapter` included) and left no drop-in
//! substitute: `crate::adapter::terminal::TerminalAdapter` "supervises no
//! process of its own" by its own doc comment, and `TuiAdapter` is
//! PTY/vendor-specific and considerably heavier than either file needs.
//! This module is the batman-4e-ruled replacement: intentionally scoped to
//! exactly what these two files need (real spawn evidence, one synthetic
//! non-exit signal, and real termination), never a wire-protocol
//! implementation of anything.
//!
//! **`docs/engineering-lessons.md`'s "RunLifecycleSink" entry cites
//! `tests/run_lifecycle.rs`'s two tests by name as the regression guard for
//! a real historical bug: a `RunState` machine whose only exerciser was a
//! test-fake read as covered in review, when the fake was never wired into
//! a live path.** "A real process, not a test-fake" is the exact property
//! this module exists to keep true for both files -- it is deliberately a
//! real [`Supervisor`]-spawned OS process (real pid, real signals, real
//! exit status via [`ManagedProcess::terminate`], the same escalation path
//! every production adapter uses), never a hand-rolled event-only stub.
//!
//! ## Never a shipped adapter
//!
//! This is not a registry kind, not constructible from config or a stored
//! profile, and not reachable by any `AdapterKind`/wire name -- it has no
//! home under `crate::adapter` at all. It is `#[path]`-included directly
//! into the two test binaries that need it (the same idiom
//! `crates/runtime/src/lib.rs`'s `extern crate self as crew_runtime` doc
//! comment describes for adapter submodules: reference the crate's own
//! public API via `crew_runtime::...`, never `crate::...`, so the same
//! source compiles unchanged inside a standalone test binary). It only
//! ever needs `crew_runtime`'s public API, so keeping it out of the
//! library's own product surface costs nothing.

use std::path::PathBuf;
use std::sync::Arc;

use crew_protocol::{Classified, ContentClass};
use crew_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterError, AdapterEvent, AdapterEventPayload,
    AdapterEventSink, AdapterFuture, AdapterMessage, AdapterSnapshot, ApprovalsCapability,
    CancelScope, DurabilityCapability, NativeViewCapability, NestedCapability, ProbeResult,
    ProtocolKind, ResumeCapability, StartSpec, SteeringCapability, UsageCapability,
    VendorSessionRef, WorkspaceControlCapability,
};
use crew_runtime::supervisor::{ManagedProcess, SpawnSpec, Supervisor, TerminationOutcome};
use tokio::sync::mpsc;

/// A minimal `Adapter` that spawns `binary` (with `args`) as a real,
/// process-group-scoped OS process and forwards only spawn/exit evidence.
/// See the module doc for what this is and why it exists.
pub struct SpawnEvidenceAdapter {
    binary: PathBuf,
    args: Vec<String>,
    /// Signals the background watcher task (spawned by `start()`, owning
    /// the real `ManagedProcess`) to terminate the process now. `None`
    /// before `start()` succeeds.
    terminate_tx: tokio::sync::Mutex<Option<mpsc::Sender<()>>>,
    /// Resolves once the watcher task has fully finished: the process is
    /// reaped and `ProcessExited` has been emitted. `dispose()` awaits
    /// this so callers observe a fully released adapter, not merely a
    /// requested one.
    watcher: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SpawnEvidenceAdapter {
    /// `binary` is spawned with `args` verbatim -- this module has no
    /// opinion on what the target process is. Both callers point it at
    /// the workspace's own `fake-worker --mode jsonl`: a real, long-lived,
    /// protocol-agnostic process (reads newline-delimited JSON from
    /// stdin, echoes one line back per input line, blocks on stdin until
    /// closed or signaled) that needs no vendor-specific handshake to
    /// stay alive and be killable.
    pub fn new(binary: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            binary: binary.into(),
            args,
            terminate_tx: tokio::sync::Mutex::new(None),
            watcher: tokio::sync::Mutex::new(None),
        }
    }
}

impl Adapter for SpawnEvidenceAdapter {
    fn kind(&self) -> &str {
        "spawn-evidence-test-vehicle"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        // Deliberately the least-capable declaration possible: this
        // vehicle proves process-lifecycle mechanics, never a protocol
        // capability -- nothing here should ever be read as "this
        // adapter can do X" beyond spawning a process and reporting its
        // exit.
        AdapterCapabilities {
            protocol: ProtocolKind::Terminal,
            resume: ResumeCapability::None,
            steering: SteeringCapability::None,
            approvals: ApprovalsCapability::None,
            structured_result: false,
            usage: UsageCapability::None,
            nested: NestedCapability::None,
            native_view: NativeViewCapability::None,
            workspace_control: WorkspaceControlCapability::ReadOnly,
            // The least-durable option, matching `TerminalAdapter` (the
            // codebase's other minimal, no-real-protocol adapter) --
            // `DurabilityCapability` has no "none" variant.
            durability: DurabilityCapability::ParentScoped,
        }
    }

    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        Box::pin(async { Err(AdapterError::capability_unsupported(self.kind(), "probe")) })
    }

    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let supervisor = Supervisor::new();
            let spawn_spec = SpawnSpec {
                program: self.binary.clone(),
                args: self.args.clone(),
                ..SpawnSpec::minimal()
            };
            let mut process: ManagedProcess = supervisor
                .spawn(spawn_spec)
                .await
                .map_err(|err| AdapterError::process(self.kind(), "start", err.to_string()))?;
            // Real pid, real process -- `Supervisor::spawn` only returns
            // successfully once it has observed one.
            let pid = u32::try_from(process.pid()).map_err(|_| {
                AdapterError::process(self.kind(), "start", "spawned pid is negative")
            })?;

            // `ProcessStarted` before this call returns, matching every
            // production adapter (and the deleted `OmpRpcAdapter`'s own
            // documented contract, which `orchestration_rpc.rs`'s driver
            // relies on being synchronous-before-return).
            sink.emit(AdapterEvent {
                run_id: spec.run_id,
                task_id: spec.task_id,
                worker_id: spec.worker_id,
                payload: AdapterEventPayload::ProcessStarted { pid },
                cursor: None,
            })
            .await
            .map_err(|err| AdapterError::process(self.kind(), "start", err.to_string()))?;

            // A synthetic, protocol-agnostic "the worker is up" signal:
            // per `crate::adapter::run_lifecycle`'s evidence table, any
            // payload other than `ProcessExited` walks a queued run to
            // `working`. This vehicle proves the sink's reaction to real
            // process evidence, not any particular vendor's wire content
            // -- there is deliberately no attempt to read or interpret
            // anything the spawned process actually writes to stdout.
            sink.emit(AdapterEvent {
                run_id: spec.run_id,
                task_id: spec.task_id,
                worker_id: spec.worker_id,
                payload: AdapterEventPayload::MessageChunk {
                    role: "assistant".to_string(),
                    text: Classified {
                        class: ContentClass::Visible,
                        value: "spawn-evidence-test-vehicle: process is up".to_string(),
                    },
                },
                cursor: None,
            })
            .await
            .map_err(|err| AdapterError::process(self.kind(), "start", err.to_string()))?;

            // Hand the real `ManagedProcess` to a background watcher that
            // owns it for the rest of its life: it resolves the moment
            // the process exits on its own OR `cancel()`/`dispose()`
            // signals termination, whichever comes first, and emits
            // `ProcessExited` exactly once either way. `cancel()` sends
            // the signal and returns immediately without waiting on this
            // task, matching the fire-and-forget contract the two test
            // files depend on; `dispose()` additionally awaits it, so a
            // disposed adapter is genuinely, fully released.
            let (terminate_tx, mut terminate_rx) = mpsc::channel::<()>(1);
            let run_id = spec.run_id;
            let task_id = spec.task_id;
            let worker_id = spec.worker_id;
            let watcher_sink = Arc::clone(&sink);
            let handle = tokio::spawn(async move {
                let outcome = tokio::select! {
                    status = process.wait() => match status {
                        Ok(status) => TerminationOutcome::Exited { code: status.code() },
                        // `wait()` itself failed (outcome unknown, not a
                        // confirmed graceful exit) -- terminate for real
                        // rather than report a fabricated exit.
                        Err(_) => process.terminate().await,
                    },
                    _ = terminate_rx.recv() => process.terminate().await,
                };
                let (exit_code, signal) = outcome.exit_signals();
                let _ = watcher_sink
                    .emit(AdapterEvent {
                        run_id,
                        task_id,
                        worker_id,
                        payload: AdapterEventPayload::ProcessExited { exit_code, signal },
                        cursor: None,
                    })
                    .await;
            });

            *self.terminate_tx.lock().await = Some(terminate_tx);
            *self.watcher.lock().await = Some(handle);

            Ok(())
        })
    }

    fn resume(
        &self,
        _session: VendorSessionRef,
        _sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async { Err(AdapterError::capability_unsupported(self.kind(), "resume")) })
    }

    fn send(&self, _message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async { Err(AdapterError::capability_unsupported(self.kind(), "send")) })
    }

    fn respond_to_approval(&self, _approval_id: &str, _decision: &str) -> AdapterFuture<'_, ()> {
        Box::pin(async {
            Err(AdapterError::capability_unsupported(
                self.kind(),
                "respond_to_approval",
            ))
        })
    }

    fn cancel(&self, _scope: CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            // Fire-and-forget, matching the real adapters this replaces:
            // signals the watcher task and returns immediately, without
            // awaiting `ManagedProcess::terminate()` completing. The
            // caller observes real process death asynchronously (poll the
            // OS pid, or poll the run's projected state), never from this
            // call's own return.
            if let Some(tx) = self.terminate_tx.lock().await.as_ref() {
                let _ = tx.try_send(());
            }
            Ok(())
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async { Ok(AdapterSnapshot::default()) })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            // Idempotent by construction: a second call finds an empty
            // channel send (harmless `Err`, discarded) and `None` already
            // taken from `watcher`, so it's a clean no-op.
            if let Some(tx) = self.terminate_tx.lock().await.as_ref() {
                let _ = tx.try_send(());
            }
            if let Some(handle) = self.watcher.lock().await.take() {
                let _ = handle.await;
            }
            Ok(())
        })
    }
}
