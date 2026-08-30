//! Integration tests for the per-worker attach server
//! (`crew_runtime::display::attach`): the socket/ring-buffer/fan-out
//! logic against a fake [`AttachTarget`] (no real PTY needed), plus one
//! test running the whole composed path against a real `PtyProcess`
//! running `cat`.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crew_runtime::display::attach::{self, AttachError, AttachServer, AttachTarget, PumpOutcome};
use crew_runtime::supervisor::{EscalationTimings, PtyProcess, SpawnSpec};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
use tokio::net::UnixStream;
use tokio::sync::broadcast;

/// The Ctrl+] byte `crewd attach` uses to detach, matching `cli.rs`'s
/// `ATTACH_DETACH_BYTE`.
const DETACH_BYTE: u8 = 0x1d;

/// A fake [`AttachTarget`]: records every byte written to it and lets the
/// test push simulated "worker output" through `output_tx`, without a
/// real PTY.
struct FakeTarget {
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
    output_tx: broadcast::Sender<Vec<u8>>,
}

/// A freshly constructed [`FakeTarget`] alongside the two handles a test
/// needs to drive and observe it: `writes` records every byte
/// `write_input` received, `output_tx` lets the test push simulated
/// worker output.
type FakeTargetHandles = (
    Arc<FakeTarget>,
    Arc<Mutex<Vec<Vec<u8>>>>,
    broadcast::Sender<Vec<u8>>,
);

impl FakeTarget {
    fn new() -> FakeTargetHandles {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (output_tx, _) = broadcast::channel(64);
        let target = Arc::new(Self {
            writes: Arc::clone(&writes),
            output_tx: output_tx.clone(),
        });
        (target, writes, output_tx)
    }
}

impl AttachTarget for FakeTarget {
    fn write_input<'a>(
        &'a self,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), AttachError>> + Send + 'a>> {
        Box::pin(async move {
            self.writes.lock().unwrap().push(bytes);
            Ok(())
        })
    }

    fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }
}

/// A fresh socket path under a throwaway temp directory, so tests never
/// collide on the filesystem.
fn temp_socket_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("run.sock");
    (dir, path)
}

/// Reads exactly `n` bytes from `stream`, or panics after `deadline`.
async fn read_exact_within(
    stream: &mut tokio::net::UnixStream,
    n: usize,
    deadline: Duration,
) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    tokio::time::timeout(deadline, stream.read_exact(&mut buf))
        .await
        .expect("read did not complete before the deadline")
        .expect("read_exact must succeed");
    buf
}

/// Connects to `path` and consumes CREW-30's liveness marker, which
/// every `AttachServer` in this test file's own binary always sends
/// first -- these tests exercise real pane content arriving *after* it,
/// not the marker itself (see `pane_socket.rs`'s own tests, and
/// `attach.rs`'s own `consume_marker_or_reclaim` tests, for that).
async fn connect_past_marker(path: &std::path::Path) -> tokio::net::UnixStream {
    let mut stream = attach::connect(path).await.unwrap();
    let marker = read_exact_within(
        &mut stream,
        crew_runtime::display::attach::LIVENESS_MARKER.len(),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        marker,
        crew_runtime::display::attach::LIVENESS_MARKER,
        "this test's own AttachServer must always send the current marker"
    );
    stream
}

// --------------------------------------------------------------- basics

#[tokio::test]
async fn a_connected_viewer_receives_live_output() {
    let (target, _writes, output_tx) = FakeTarget::new();
    let (_dir, path) = temp_socket_path();
    let server = AttachServer::start(path.clone(), target, Box::new(|_| {})).unwrap();

    let mut viewer = connect_past_marker(&path).await;

    output_tx.send(b"hello viewer".to_vec()).unwrap();

    let seen = read_exact_within(&mut viewer, b"hello viewer".len(), Duration::from_secs(5)).await;
    assert_eq!(seen, b"hello viewer");

    server.stop();
}

#[tokio::test]
async fn a_late_viewer_receives_the_ring_buffer_replay() {
    let (target, _writes, output_tx) = FakeTarget::new();
    let (_dir, path) = temp_socket_path();
    let server = AttachServer::start(path.clone(), target, Box::new(|_| {})).unwrap();

    output_tx.send(b"already happened".to_vec()).unwrap();
    // Give the collector task a moment to drain the broadcast into the
    // ring buffer before the late viewer connects.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut late_viewer = connect_past_marker(&path).await;
    let seen = read_exact_within(
        &mut late_viewer,
        b"already happened".len(),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        seen, b"already happened",
        "a late-joining viewer must see the ring buffer replay"
    );

    server.stop();
}

#[tokio::test]
async fn viewer_bytes_reach_both_the_target_and_on_user_input() {
    let (target, writes, _output_tx) = FakeTarget::new();
    let (_dir, path) = temp_socket_path();

    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_cb = Arc::clone(&captured);

    let server = AttachServer::start(
        path.clone(),
        target,
        Box::new(move |bytes| captured_for_cb.lock().unwrap().push(bytes)),
    )
    .unwrap();

    let mut viewer = connect_past_marker(&path).await;
    viewer.write_all(b"typed keystrokes").await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if !writes.lock().unwrap().is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "viewer bytes never reached the target"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(writes.lock().unwrap()[0], b"typed keystrokes");
    assert_eq!(captured.lock().unwrap()[0], b"typed keystrokes");

    server.stop();
}

#[tokio::test]
async fn two_concurrent_viewers_both_receive_the_same_output() {
    let (target, _writes, output_tx) = FakeTarget::new();
    let (_dir, path) = temp_socket_path();
    let server = AttachServer::start(path.clone(), target, Box::new(|_| {})).unwrap();

    let mut viewer_a = connect_past_marker(&path).await;
    let mut viewer_b = connect_past_marker(&path).await;
    // Let both connections register their subscription before the send,
    // so this test asserts fan-out rather than incidentally relying on
    // ring-buffer replay.
    tokio::time::sleep(Duration::from_millis(50)).await;

    output_tx.send(b"fanout".to_vec()).unwrap();

    let seen_a = read_exact_within(&mut viewer_a, b"fanout".len(), Duration::from_secs(5)).await;
    let seen_b = read_exact_within(&mut viewer_b, b"fanout".len(), Duration::from_secs(5)).await;
    assert_eq!(seen_a, b"fanout");
    assert_eq!(seen_b, b"fanout");

    server.stop();
}

#[tokio::test]
async fn stop_closes_connected_clients_and_unlinks_the_socket_file() {
    let (target, _writes, _output_tx) = FakeTarget::new();
    let (_dir, path) = temp_socket_path();
    let server = AttachServer::start(path.clone(), target, Box::new(|_| {})).unwrap();

    let mut viewer = connect_past_marker(&path).await;

    server.stop();

    let mut buf = [0u8; 16];
    let read = tokio::time::timeout(Duration::from_secs(5), viewer.read(&mut buf))
        .await
        .expect("the viewer's read must resolve promptly after stop()");
    // A clean EOF (Ok(0)) is the tidy close. On Linux, aborting the
    // server-side task while the viewer still has unread ring-replay
    // bytes queued surfaces as ECONNRESET rather than EOF -- equally
    // proof that stop() closed the socket. Either is acceptable; a byte
    // (Ok(n > 0)) is not.
    match read {
        Ok(0) => {}
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => {
            panic!("stop() must close every connected viewer's socket, got {other:?}")
        }
    }
    assert!(
        !path.exists(),
        "stop() must remove the socket file from disk"
    );
}

#[tokio::test]
async fn a_disconnecting_viewer_never_affects_a_second_viewer() {
    let (target, _writes, output_tx) = FakeTarget::new();
    let (_dir, path) = temp_socket_path();
    let server = AttachServer::start(path.clone(), target, Box::new(|_| {})).unwrap();

    let viewer_a = connect_past_marker(&path).await;
    let mut viewer_b = connect_past_marker(&path).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    drop(viewer_a);
    tokio::time::sleep(Duration::from_millis(50)).await;

    output_tx.send(b"still here".to_vec()).unwrap();
    let seen = read_exact_within(&mut viewer_b, b"still here".len(), Duration::from_secs(5)).await;
    assert_eq!(seen, b"still here");

    server.stop();
}

// --------------------------------------------------------------- pump()
//
// `attach::pump` is the only logic `crewd attach` itself runs (the raw
// terminal-mode setup around it is a thin, deliberately untested shell --
// see `cli.rs::RawModeGuard`). These tests drive `pump` directly: a real
// connected `UnixStream` pair stands in for the attach socket (one end
// passed to `pump`, the other held by the test as "the server/worker
// side"), and `tokio::io::duplex` pipes stand in for the terminal's
// stdin/stdout.

#[tokio::test]
async fn pump_forwards_socket_bytes_to_the_output_side() {
    let (socket, mut remote) = UnixStream::pair().unwrap();
    let (_input_writer, input_reader) = duplex(64);
    let (output_writer, mut output_reader) = duplex(64);

    let pump_task = tokio::spawn(attach::pump(
        socket,
        input_reader,
        output_writer,
        DETACH_BYTE,
    ));

    remote.write_all(b"from the worker").await.unwrap();

    let mut buf = [0u8; 32];
    let n = tokio::time::timeout(Duration::from_secs(5), output_reader.read(&mut buf))
        .await
        .expect("read from the output side must not hang")
        .unwrap();
    assert_eq!(&buf[..n], b"from the worker");

    drop(remote);
    let outcome = tokio::time::timeout(Duration::from_secs(5), pump_task)
        .await
        .expect("pump must not hang once the socket closes")
        .unwrap();
    assert_eq!(outcome.unwrap(), PumpOutcome::SocketClosed);
}

#[tokio::test]
async fn pump_forwards_input_bytes_to_the_socket() {
    let (socket, mut remote) = UnixStream::pair().unwrap();
    let (mut input_writer, input_reader) = duplex(64);
    let (output_writer, _output_reader) = duplex(64);

    let pump_task = tokio::spawn(attach::pump(
        socket,
        input_reader,
        output_writer,
        DETACH_BYTE,
    ));

    input_writer.write_all(b"typed keystrokes").await.unwrap();

    let mut buf = [0u8; 32];
    let n = tokio::time::timeout(Duration::from_secs(5), remote.read(&mut buf))
        .await
        .expect("the remote side must see the forwarded bytes")
        .unwrap();
    assert_eq!(&buf[..n], b"typed keystrokes");

    drop(input_writer);
    let outcome = tokio::time::timeout(Duration::from_secs(5), pump_task)
        .await
        .expect("pump must not hang once input closes")
        .unwrap();
    assert_eq!(outcome.unwrap(), PumpOutcome::InputClosed);
}

#[tokio::test]
async fn pump_detaches_on_a_ctrl_bracket_arriving_in_its_own_read() {
    let (socket, mut remote) = UnixStream::pair().unwrap();
    let (mut input_writer, input_reader) = duplex(64);
    let (output_writer, _output_reader) = duplex(64);

    let pump_task = tokio::spawn(attach::pump(
        socket,
        input_reader,
        output_writer,
        DETACH_BYTE,
    ));

    // Written and drained as a first, separate read, before the detach
    // byte arrives in a second one -- the split case.
    input_writer.write_all(b"abc").await.unwrap();
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(5), remote.read(&mut buf))
        .await
        .expect("the pre-detach bytes must be forwarded first")
        .unwrap();
    assert_eq!(&buf[..n], b"abc");

    input_writer.write_all(&[DETACH_BYTE]).await.unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(5), pump_task)
        .await
        .expect("pump must not hang on a lone detach byte")
        .unwrap();
    assert_eq!(outcome.unwrap(), PumpOutcome::Detached);
}

#[tokio::test]
async fn pump_never_forwards_bytes_that_follow_the_detach_byte_in_the_same_read() {
    let (socket, mut remote) = UnixStream::pair().unwrap();
    let (mut input_writer, input_reader) = duplex(64);
    let (output_writer, _output_reader) = duplex(64);

    let pump_task = tokio::spawn(attach::pump(
        socket,
        input_reader,
        output_writer,
        DETACH_BYTE,
    ));

    // A single write containing bytes before AND after the detach byte,
    // so it is very likely delivered to `pump` as one `read()` call.
    input_writer
        .write_all(&[b'a', b'b', DETACH_BYTE, b'c', b'd'])
        .await
        .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(5), pump_task)
        .await
        .expect("pump must not hang on a mid-buffer detach byte")
        .unwrap();
    assert_eq!(outcome.unwrap(), PumpOutcome::Detached);

    // The bytes before the detach byte in the same buffer are real typed
    // characters and are forwarded; whatever followed the detach byte
    // (`cd`) must never reach the socket.
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_millis(500), remote.read(&mut buf))
        .await
        .unwrap_or(Ok(0))
        .unwrap_or(0);
    assert_eq!(
        &buf[..n],
        b"ab",
        "only the bytes preceding the detach byte may have been forwarded"
    );
}

#[tokio::test]
async fn pump_ends_cleanly_when_the_socket_side_closes() {
    let (socket, remote) = UnixStream::pair().unwrap();
    let (_input_writer, input_reader) = duplex(64);
    let (output_writer, _output_reader) = duplex(64);

    let pump_task = tokio::spawn(attach::pump(
        socket,
        input_reader,
        output_writer,
        DETACH_BYTE,
    ));

    drop(remote);

    let outcome = tokio::time::timeout(Duration::from_secs(5), pump_task)
        .await
        .expect("pump must not hang once the remote side closes the socket")
        .unwrap();
    assert_eq!(outcome.unwrap(), PumpOutcome::SocketClosed);
}

// ------------------------------------------------------ real PtyProcess

#[tokio::test]
async fn composed_path_against_a_real_pty_process_running_cat() {
    let escalation = EscalationTimings {
        sigint_to_sigterm: Duration::from_millis(200),
        sigterm_to_sigkill: Duration::from_millis(200),
    };
    let spec = SpawnSpec {
        program: PathBuf::from("/bin/cat"),
        args: vec![],
        cwd: std::env::temp_dir(),
        env: std::collections::HashMap::new(),
        ..SpawnSpec::minimal()
    };
    let process = PtyProcess::spawn(&spec, escalation).expect("spawn cat on a real pty");
    // `AttachServer` only ever holds a *clone* of this `Arc` -- the real
    // `PtyProcess` (and therefore the `cat` worker) is kept alive by this
    // test's own clone for the whole test, independent of the server's
    // lifecycle, exactly like an orchestrator would keep its own handle.
    let target: Arc<dyn AttachTarget> = Arc::new(process);

    let (_dir, path) = temp_socket_path();
    let server = AttachServer::start(path.clone(), Arc::clone(&target), Box::new(|_| {})).unwrap();

    let mut viewer = connect_past_marker(&path).await;
    viewer.write_all(b"crew-attach-roundtrip\r").await.unwrap();

    // `cat` echoes stdin back to stdout on the pty, so the viewer's own
    // keystrokes must come back through the output side of the same
    // connection: viewer input -> AttachServer -> PtyProcess::write_input
    // -> the real pty -> `cat` -> pty output -> the collector -> back to
    // the viewer.
    let mut acc = Vec::new();
    let saw_roundtrip = tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 256];
        loop {
            let n = viewer.read(&mut buf).await.expect("viewer read");
            if n == 0 {
                break false;
            }
            acc.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&acc).contains("crew-attach-roundtrip") {
                break true;
            }
        }
    })
    .await
    .expect("the roundtrip must complete before the deadline");

    assert!(
        saw_roundtrip,
        "viewer input must round-trip through the real pty and back: {:?}",
        String::from_utf8_lossy(&acc)
    );

    server.stop();
    drop(target);
}

// ------------------------------------------------ consume_marker_or_reclaim

/// Binds a bare `UnixListener` at `path` synchronously (so a test can
/// connect immediately after, with no bind-race), then spawns a task that
/// accepts the first connection, writes `bytes`, and holds the socket open
/// well past any test's bounded read -- simulating an *older* `AttachServer`
/// (pre-CREW-30) that never sends [`attach::LIVENESS_MARKER`] and whose
/// first bytes are real pane output the client must reclaim, not discard.
fn spawn_raw_server_writing(path: &std::path::Path, bytes: &'static [u8]) {
    let listener = tokio::net::UnixListener::bind(path).expect("bind raw server");
    tokio::spawn(async move {
        if let Ok((mut stream, _addr)) = listener.accept().await {
            let _ = stream.write_all(bytes).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

/// The marker-present leg: a real `AttachServer` always sends
/// `LIVENESS_MARKER` first, so consuming it must return `None` -- nothing
/// left to reclaim, `pump` can start straight away.
#[tokio::test]
async fn consume_marker_or_reclaim_returns_none_when_the_marker_arrives_in_full() {
    let (target, _writes, _output_tx) = FakeTarget::new();
    let (_dir, path) = temp_socket_path();
    let server = AttachServer::start(path.clone(), target, Box::new(|_| {})).unwrap();

    let mut socket = attach::connect(&path).await.unwrap();
    let reclaimed = attach::consume_marker_or_reclaim(&mut socket, Duration::from_secs(5)).await;
    assert_eq!(
        reclaimed, None,
        "a real AttachServer's marker must be consumed in full, nothing left to reclaim"
    );

    server.stop();
}

/// The backward-compatibility leg: an older daemon predating this marker
/// never sends one at all, so its first bytes are real pane output,
/// indistinguishable at the wire level from "not the marker". Those bytes
/// must come back verbatim for the caller to replay, never be discarded.
///
/// The probe only ever reads exactly `LIVENESS_MARKER.len()` bytes -- it is
/// a fixed-size handshake read, not a drain-everything-available read -- so
/// this test's fake server writes exactly that many bytes. Any output an
/// old daemon sends beyond that stays in the kernel's socket buffer
/// untouched, for `pump`'s own forward loop to pick up right after.
#[tokio::test]
async fn consume_marker_or_reclaim_reclaims_bytes_from_a_server_that_never_sends_the_marker() {
    let (_dir, path) = temp_socket_path();
    let old_daemon_bytes: &[u8] = b"totally-old!";
    assert_eq!(old_daemon_bytes.len(), attach::LIVENESS_MARKER.len());
    spawn_raw_server_writing(&path, old_daemon_bytes);

    let mut socket = attach::connect(&path).await.unwrap();
    let reclaimed =
        attach::consume_marker_or_reclaim(&mut socket, Duration::from_millis(200)).await;
    assert_eq!(
        reclaimed.as_deref(),
        Some(old_daemon_bytes),
        "bytes that aren't the marker must be handed back verbatim, never discarded"
    );
}

/// The partial-read leg: the peer sends fewer bytes than the marker's
/// length and then stalls (a slow write, or a connection that never
/// completes the handshake either way). The bounded timeout must still
/// fire and reclaim exactly what was captured, not hang forever waiting
/// for a full marker that will never arrive.
#[tokio::test]
async fn consume_marker_or_reclaim_reclaims_a_partial_read_when_the_peer_stalls_mid_marker() {
    let (_dir, path) = temp_socket_path();
    // Fewer bytes than LIVENESS_MARKER.len() (12), so the read can never
    // complete a full match.
    spawn_raw_server_writing(&path, b"CR");

    let mut socket = attach::connect(&path).await.unwrap();
    let reclaimed =
        attach::consume_marker_or_reclaim(&mut socket, Duration::from_millis(200)).await;
    assert_eq!(
        reclaimed.as_deref(),
        Some(&b"CR"[..]),
        "a partial read that never completes must reclaim exactly what was captured before the timeout"
    );
}
