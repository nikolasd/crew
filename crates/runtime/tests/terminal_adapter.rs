//! Terminal adapter tests using injected CommandRunner.

use std::io;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

use batman_protocol::{RunId, TaskId, WorkerId};
use batman_runtime::adapter::terminal::{CommandRunner, TerminalAdapter};
use batman_runtime::adapter::{
    Adapter, AdapterErrorCode, AdapterEvent, AdapterEventSink, ProtocolKind, StartSpec,
};

/// Mock command runner that records invocations and returns controlled outputs.
struct MockCommandRunner {
    results: Vec<MockOutput>,
    calls: Mutex<Vec<(String, Vec<String>)>>,
    call_count: std::sync::atomic::AtomicUsize,
}

#[derive(Clone)]
enum MockOutput {
    Success,
    Failure(String),
    SpawnError(String),
}

impl MockCommandRunner {
    fn new() -> Self {
        MockCommandRunner {
            results: Vec::new(),
            calls: Mutex::new(Vec::new()),
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn push_success(&mut self) {
        self.results.push(MockOutput::Success);
    }

    fn push_failure(&mut self, stderr: &str) {
        self.results.push(MockOutput::Failure(stderr.to_string()));
    }

    fn push_spawn_error(&mut self, msg: &str) {
        self.results.push(MockOutput::SpawnError(msg.to_string()));
    }

    fn recorded_calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }

    fn assert_command_invoked(&self, cmd: &str, expected_args: &[&str]) {
        let calls = self.recorded_calls();
        assert!(
            calls.iter().any(|(c, args)| {
                c == cmd
                    && args.len() == expected_args.len()
                    && args
                        .iter()
                        .zip(expected_args.iter())
                        .all(|(a, e)| *a == **e)
            }),
            "Expected command '{}' with args {:?}, but recorded calls were: {:?}",
            cmd,
            expected_args,
            calls
        );
    }
}

impl CommandRunner for MockCommandRunner {
    fn run(
        &self,
        cmd: &str,
        args: &[&str],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<std::process::Output>> + Send>,
    > {
        let idx = self.call_count.load(std::sync::atomic::Ordering::Relaxed);
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut calls = self.calls.lock().unwrap();
            calls.push((
                cmd.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
        }
        let result = self.results.get(idx).cloned();
        Box::pin(async move {
            match result {
                Some(MockOutput::Success) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        Ok(std::process::Output {
                            status: ExitStatus::from_raw(0),
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        })
                    }
                    #[cfg(not(unix))]
                    {
                        Ok(std::process::Output {
                            status: ExitStatus::success(),
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        })
                    }
                }
                Some(MockOutput::Failure(stderr)) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        Ok(std::process::Output {
                            status: ExitStatus::from_raw(1),
                            stdout: Vec::new(),
                            stderr: stderr.as_bytes().to_vec(),
                        })
                    }
                    #[cfg(not(unix))]
                    {
                        Ok(std::process::Output {
                            status: ExitStatus::failure(),
                            stdout: Vec::new(),
                            stderr: stderr.as_bytes().to_vec(),
                        })
                    }
                }
                Some(MockOutput::SpawnError(msg)) => {
                    Err(io::Error::new(io::ErrorKind::NotFound, msg.as_str()))
                }
                None => Err(io::Error::other("no more results")),
            }
        })
    }
}

fn make_spec() -> StartSpec {
    StartSpec {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        worker_id: WorkerId::new(),
        prompt: "test prompt".to_string(),
        resume: None,
    }
}

#[tokio::test]
async fn terminal_adapter_herdr_start_success() {
    let mut mock = MockCommandRunner::new();
    mock.push_success();

    let mock: Arc<MockCommandRunner> = Arc::new(mock);
    let runner: Arc<dyn CommandRunner> = mock.clone();
    let adapter = TerminalAdapter::with_command_runner("herdr".to_string(), runner);

    let spec = make_spec();
    let session = format!("crew-{}", spec.run_id);
    let result = adapter.start(spec, Arc::new(NullSink)).await;
    assert!(result.is_ok());
    mock.assert_command_invoked("herdr", &["new", &session]);
}

#[tokio::test]
async fn terminal_adapter_herdr_start_failure() {
    let mut mock = MockCommandRunner::new();
    mock.push_failure("session creation failed");

    let adapter = TerminalAdapter::with_command_runner("herdr".to_string(), Arc::new(mock));

    let result = adapter.start(make_spec(), Arc::new(NullSink)).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.error_code(), AdapterErrorCode::Process);
}

#[tokio::test]
async fn terminal_adapter_herdr_start_spawn_error() {
    let mut mock = MockCommandRunner::new();
    mock.push_spawn_error("tmux not found");

    let adapter = TerminalAdapter::with_command_runner("herdr".to_string(), Arc::new(mock));

    let result = adapter.start(make_spec(), Arc::new(NullSink)).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.error_code(), AdapterErrorCode::Unavailable);
}

#[tokio::test]
async fn terminal_adapter_tmux_start_success() {
    let mut mock = MockCommandRunner::new();
    mock.push_success();

    let mock: Arc<MockCommandRunner> = Arc::new(mock);
    let runner: Arc<dyn CommandRunner> = mock.clone();
    let adapter = TerminalAdapter::with_command_runner("tmux".to_string(), runner);

    let spec = make_spec();
    let session = format!("crew-{}", spec.run_id);
    let result = adapter.start(spec, Arc::new(NullSink)).await;
    assert!(result.is_ok());
    mock.assert_command_invoked("tmux", &["new-session", "-d", "-s", &session]);
}

#[tokio::test]
async fn terminal_adapter_tmux_start_failure() {
    let mut mock = MockCommandRunner::new();
    mock.push_failure("tmux error");

    let adapter = TerminalAdapter::with_command_runner("tmux".to_string(), Arc::new(mock));

    let result = adapter.start(make_spec(), Arc::new(NullSink)).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn terminal_adapter_kind() {
    let adapter = TerminalAdapter::new("herdr".to_string());
    assert_eq!(adapter.kind(), "terminal");
}

#[tokio::test]
async fn terminal_adapter_capabilities() {
    let adapter = TerminalAdapter::new("herdr".to_string());
    let caps = adapter.capabilities();
    assert_eq!(caps.protocol, ProtocolKind::Terminal);
}

/// R95: this adapter supervises no process of its own and never emits a
/// real `ProcessExited`, so a terminal-adapter run used to pin its
/// concurrency slot, the idle timer, and unforced `runtime/shutdown` for
/// the life of the daemon -- `watch_settlement`'s receiver never fired.
/// `cancel` must emit a synthetic exit through the sink captured at
/// `start`, exactly once, and stay a clean unsupported error when the
/// adapter never started.
#[tokio::test]
async fn terminal_adapter_cancel_settles_the_run_with_a_synthetic_exit() {
    use batman_runtime::adapter::{AdapterEventPayload, CancelScope};

    struct CollectingSink {
        events: parking_lot::Mutex<Vec<AdapterEvent>>,
    }
    impl AdapterEventSink for CollectingSink {
        fn emit(&self, event: AdapterEvent) -> batman_runtime::adapter::AdapterFuture<'_, u64> {
            self.events.lock().push(event);
            Box::pin(async { Ok(0) })
        }
    }

    let mut mock = MockCommandRunner::new();
    mock.push_success();
    let adapter = TerminalAdapter::with_command_runner("tmux".to_string(), Arc::new(mock));
    let sink = Arc::new(CollectingSink {
        events: parking_lot::Mutex::new(Vec::new()),
    });

    let spec = make_spec();
    let run_id = spec.run_id;
    adapter
        .start(spec, Arc::clone(&sink) as Arc<dyn AdapterEventSink>)
        .await
        .expect("start succeeds");

    adapter
        .cancel(CancelScope::Worker)
        .await
        .expect("cancel settles instead of refusing");
    let events = sink.events.lock().clone();
    assert_eq!(events.len(), 1, "exactly one synthetic exit: {events:?}");
    assert_eq!(events[0].run_id, run_id);
    assert!(
        matches!(
            &events[0].payload,
            AdapterEventPayload::ProcessExited { exit_code: None, signal: Some(signal) }
                if signal == "cancelled"
        ),
        "the synthetic exit must be visibly synthetic: {events:?}"
    );

    // A second cancel is a clean no-op success, never a second exit --
    // an Err would read as a real kill failure and raise a false
    // degradedControl (R93 x R95, batch-12 review W2).
    adapter
        .cancel(CancelScope::Worker)
        .await
        .expect("a settled adapter's cancel is a no-op success");
    assert_eq!(sink.events.lock().len(), 1);
}

/// Null event sink for testing.
struct NullSink;

impl AdapterEventSink for NullSink {
    fn emit(&self, _event: AdapterEvent) -> batman_runtime::adapter::AdapterFuture<'_, u64> {
        Box::pin(async { Ok(0) })
    }
}
