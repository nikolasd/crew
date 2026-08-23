//! Terminal adapter: degraded-mode terminal harness.
//!
//! This adapter wraps any underlying terminal-capable harness (herdr, tmux,
//! or raw terminal) and declares explicitly-limited capabilities. It is
//! the fallback when structured protocols (Claude/Codex/Copilot/OMP-RPC)
//! are unavailable.

use std::sync::Arc;

use batman_protocol::{RunId, TaskId, WorkerId};

use super::AdapterFuture;
use super::capability::{
    AdapterCapabilities, ApprovalsCapability, DurabilityCapability, NativeViewCapability,
    NestedCapability, ProtocolKind, ResumeCapability, SteeringCapability, UsageCapability,
    WorkspaceControlCapability,
};
use super::error::AdapterError;
use super::event_sink::AdapterEventSink;
use super::event_sink::{AdapterEvent, AdapterEventPayload};
use super::r#trait::{
    Adapter, AdapterMessage, AdapterSnapshot, CancelScope, ProbeResult, StartSpec, VendorSessionRef,
};

/// A command runner trait for terminal adapter testing.
///
/// This allows injecting mock command runners for testing without requiring
/// actual terminal tools (tmux, herdr) to be installed.
pub trait CommandRunner: Send + Sync {
    fn run(
        &self,
        cmd: &str,
        args: &[&str],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<std::process::Output>> + Send>,
    >;
}

/// The sink and run identity a terminal-adapter `start` captures for
/// `cancel`'s synthetic settlement (R95).
type CapturedSession = (Arc<dyn AdapterEventSink>, RunId, TaskId, WorkerId);

/// A terminal adapter that wraps an underlying harness.
///
/// This adapter declares `ProtocolKind::Terminal` (degraded) and explicitly
/// limits its capabilities to what terminal automation can actually provide.
pub struct TerminalAdapter {
    /// The underlying harness kind (e.g., "herdr", "tmux", "raw").
    harness: String,
    /// Optional injected command runner for testing.
    command_runner: Option<Arc<dyn CommandRunner>>,
    /// The sink and run identity captured at `start` (R95): this adapter
    /// supervises no process of its own and so never observes a real
    /// exit; `cancel` emits a synthetic `ProcessExited` through this so
    /// the registry's settlement watcher frees the run's slot -- without
    /// it, a terminal-adapter run pinned its slot, the idle timer, and
    /// unforced shutdown for the life of the process.
    session: parking_lot::Mutex<Option<CapturedSession>>,
}

impl TerminalAdapter {
    /// Creates a new terminal adapter with the given harness.
    pub fn new(harness: String) -> Self {
        TerminalAdapter {
            harness,
            command_runner: None,
            session: parking_lot::Mutex::new(None),
        }
    }

    /// Creates a new terminal adapter with an injected command runner.
    pub fn with_command_runner(harness: String, command_runner: Arc<dyn CommandRunner>) -> Self {
        TerminalAdapter {
            harness,
            command_runner: Some(command_runner),
            session: parking_lot::Mutex::new(None),
        }
    }
}

impl Adapter for TerminalAdapter {
    fn kind(&self) -> &str {
        "terminal"
    }

    fn capabilities(&self) -> AdapterCapabilities {
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
            durability: DurabilityCapability::ParentScoped,
        }
    }

    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        Box::pin(async move {
            Ok(ProbeResult {
                version: Some(format!("{} (degraded)", self.harness)),
                auth_ready: true,
                capabilities: self.capabilities(),
                inventory_incomplete: true,
            })
        })
    }

    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        let harness = self.harness.clone();
        let session_name = format!("crew-{}", spec.run_id);
        let command_runner = self.command_runner.clone();
        // Capture the sink and run identity for `cancel`'s synthetic
        // settlement (R95) before the fallible spawn: a failed start
        // never reaches the registry map, so a stale capture is inert.
        *self.session.lock() = Some((sink, spec.run_id, spec.task_id, spec.worker_id));
        Box::pin(async move {
            // Dispatch based on backend name
            match harness.as_str() {
                "tmux" => {
                    // Use injected command runner if available, otherwise use system command
                    let result = if let Some(ref runner) = command_runner {
                        runner
                            .run("tmux", &["new-session", "-d", "-s", &session_name])
                            .await
                    } else {
                        tokio::process::Command::new("tmux")
                            .args(["new-session", "-d", "-s", &session_name])
                            .output()
                            .await
                    };
                    match result {
                        Ok(output) if output.status.success() => Ok(()),
                        Ok(_) => Err(AdapterError::new(
                            super::error::AdapterErrorCode::Process,
                            "terminal",
                            "start",
                            "tmux failed to create session",
                        )),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            Err(AdapterError::new(
                                super::error::AdapterErrorCode::Unavailable,
                                "terminal",
                                "start",
                                "tmux not found",
                            ))
                        }
                        Err(e) => Err(AdapterError::new(
                            super::error::AdapterErrorCode::Process,
                            "terminal",
                            "start",
                            format!("failed to spawn tmux: {}", e),
                        )),
                    }
                }
                "herdr" => {
                    // Herdr session command (placeholder - actual command depends on herdr CLI)
                    let result = if let Some(ref runner) = command_runner {
                        runner.run("herdr", &["new", &session_name]).await
                    } else {
                        tokio::process::Command::new("herdr")
                            .args(["new", &session_name])
                            .output()
                            .await
                    };

                    match result {
                        Ok(output) if output.status.success() => Ok(()),
                        Ok(_) => Err(AdapterError::new(
                            super::error::AdapterErrorCode::Process,
                            "terminal",
                            "start",
                            "herdr failed to create session",
                        )),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            Err(AdapterError::new(
                                super::error::AdapterErrorCode::Unavailable,
                                "terminal",
                                "start",
                                "herdr not found",
                            ))
                        }
                        Err(e) => Err(AdapterError::new(
                            super::error::AdapterErrorCode::Process,
                            "terminal",
                            "start",
                            format!("failed to spawn herdr: {}", e),
                        )),
                    }
                }
                _ => {
                    // Unknown backend - return unavailable
                    Err(AdapterError::new(
                        super::error::AdapterErrorCode::Unavailable,
                        "terminal",
                        "start",
                        format!("unknown terminal backend: {}", harness),
                    ))
                }
            }
        })
    }

    fn resume(
        &self,
        _session: VendorSessionRef,
        _sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Err(AdapterError::capability_unsupported("terminal", "resume")) })
    }

    fn send(&self, _message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Err(AdapterError::capability_unsupported("terminal", "send")) })
    }

    fn respond_to_approval(&self, _approval_id: &str, _decision: &str) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            Err(AdapterError::capability_unsupported(
                "terminal",
                "respond_to_approval",
            ))
        })
    }

    fn cancel(&self, _scope: CancelScope) -> AdapterFuture<'_, ()> {
        // There is no supervised vendor process to kill -- the "worker"
        // is a terminal pane a human drives. Cancelling means settling:
        // emit a synthetic ProcessExited so the registry's settlement
        // watcher evicts this adapter and frees its concurrency slot
        // (R95). `take()` makes a second cancel a clean no-op.
        let session = self.session.lock().take();
        Box::pin(async move {
            let Some((sink, run_id, task_id, worker_id)) = session else {
                // Already settled (or never started): a second cancel is
                // genuinely a no-op, the same judgement R13 made for an
                // absent adapter -- an Err here would read as "a live
                // vendor process a kill failed against" and raise a false
                // degradedControl (R93) on a run that settled cleanly.
                return Ok(());
            };
            sink.emit(AdapterEvent {
                run_id,
                task_id,
                worker_id,
                payload: AdapterEventPayload::ProcessExited {
                    exit_code: None,
                    signal: Some("cancelled".to_string()),
                },
            })
            .await
            .map_err(|e| {
                AdapterError::new(
                    super::error::AdapterErrorCode::Process,
                    "terminal",
                    "cancel",
                    format!("failed to journal the synthetic exit: {e}"),
                )
            })?;
            Ok(())
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async move {
            Ok(AdapterSnapshot {
                state_summary: format!("terminal adapter [{}]", self.harness),
                children: Vec::new(),
                usage: None,
                artifacts: Vec::new(),
            })
        })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_adapter_kind() {
        let adapter = TerminalAdapter::new("test".to_string());
        assert_eq!(adapter.kind(), "terminal");
    }

    #[test]
    fn test_terminal_adapter_capabilities() {
        let adapter = TerminalAdapter::new("test".to_string());
        let caps = adapter.capabilities();
        assert_eq!(caps.protocol, ProtocolKind::Terminal);
        assert_eq!(caps.resume, ResumeCapability::None);
        assert_eq!(caps.steering, SteeringCapability::None);
        assert_eq!(caps.approvals, ApprovalsCapability::None);
        assert!(!caps.structured_result);
        assert_eq!(caps.usage, UsageCapability::None);
        assert_eq!(caps.nested, NestedCapability::None);
        assert_eq!(caps.native_view, NativeViewCapability::None);
        assert_eq!(caps.workspace_control, WorkspaceControlCapability::ReadOnly);
        assert_eq!(caps.durability, DurabilityCapability::ParentScoped);
    }

    #[tokio::test]
    async fn test_terminal_adapter_probe() {
        let adapter = TerminalAdapter::new("herdr".to_string());
        let result = adapter.probe().await.unwrap();
        assert_eq!(result.version, Some("herdr (degraded)".to_string()));
        assert!(result.auth_ready);
        assert!(result.inventory_incomplete);
        assert_eq!(result.capabilities.protocol, ProtocolKind::Terminal);
    }

    #[tokio::test]
    async fn test_terminal_adapter_snapshot() {
        let adapter = TerminalAdapter::new("tmux".to_string());
        let snapshot = adapter.snapshot().await.unwrap();
        assert!(snapshot.state_summary.contains("tmux"));
        assert!(snapshot.children.is_empty());
        assert!(snapshot.usage.is_none());
        assert!(snapshot.artifacts.is_empty());
    }

    /// A minimal test event sink that captures events.
    struct TestSink;
    impl AdapterEventSink for TestSink {
        fn emit(&self, _event: super::super::event_sink::AdapterEvent) -> AdapterFuture<'_, u64> {
            Box::pin(async { Ok(0) })
        }
    }

    #[test]
    fn test_terminal_adapter_capabilities_declare_degraded() {
        let adapter = TerminalAdapter::new("test".to_string());
        let caps = adapter.capabilities();
        // Terminal adapter declares explicitly-limited capabilities
        assert_eq!(caps.protocol, ProtocolKind::Terminal);
        assert!(!caps.structured_result);
        assert_eq!(caps.workspace_control, WorkspaceControlCapability::ReadOnly);
        assert_eq!(caps.durability, DurabilityCapability::ParentScoped);
        // All control capabilities are None/absent
        assert_eq!(caps.resume, ResumeCapability::None);
        assert_eq!(caps.steering, SteeringCapability::None);
        assert_eq!(caps.approvals, ApprovalsCapability::None);
        assert_eq!(caps.usage, UsageCapability::None);
        assert_eq!(caps.nested, NestedCapability::None);
        assert_eq!(caps.native_view, NativeViewCapability::None);
    }

    #[tokio::test]
    async fn test_terminal_adapter_start_returns_error() {
        let adapter = TerminalAdapter::new("nonexistent-harness".to_string());
        let spec = StartSpec {
            run_id: batman_protocol::RunId::new(),
            task_id: batman_protocol::TaskId::new(),
            worker_id: batman_protocol::WorkerId::new(),
            prompt: "test prompt".to_string(),
            resume: None,
        };
        let sink = Arc::new(TestSink);
        let result = adapter.start(spec, sink).await;
        assert!(result.is_err());
        // When harness is not found, returns "unavailable"; when it exists but fails, returns "process"
        let err = result.unwrap_err();
        assert!(err.code() == "unavailable" || err.code() == "process");
    }

    #[tokio::test]
    async fn test_terminal_adapter_resume_returns_capability_unsupported() {
        let adapter = TerminalAdapter::new("test".to_string());
        let session = VendorSessionRef("test-session".to_string());
        let sink = Arc::new(TestSink);
        let result = adapter.resume(session, sink).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "capability_unsupported");
    }

    #[tokio::test]
    async fn test_terminal_adapter_send_returns_capability_unsupported() {
        let adapter = TerminalAdapter::new("test".to_string());
        let message = AdapterMessage::Steer {
            text: "test".to_string(),
        };
        let result = adapter.send(message).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "capability_unsupported");
    }

    #[tokio::test]
    async fn test_terminal_adapter_cancel_without_a_session_is_a_no_op_success() {
        // A never-started adapter is unreachable from the registry; a
        // settled one is not. Err here would read as a real kill failure
        // and raise a false degradedControl (R93).
        let adapter = TerminalAdapter::new("test".to_string());
        adapter
            .cancel(CancelScope::Worker)
            .await
            .expect("cancel with nothing to settle is a no-op");
    }

    #[tokio::test]
    async fn test_terminal_adapter_respond_to_approval_returns_capability_unsupported() {
        let adapter = TerminalAdapter::new("test".to_string());
        let result = adapter.respond_to_approval("approval-123", "approve").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "capability_unsupported");
    }

    #[tokio::test]
    async fn test_terminal_adapter_dispose() {
        let adapter = TerminalAdapter::new("test".to_string());
        let result = adapter.dispose().await;
        assert!(result.is_ok());
    }
}
