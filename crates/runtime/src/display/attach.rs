//! Per-worker attach server: a Unix domain socket that fans a PTY
//! worker's raw output out to any number of viewers and forwards viewer
//! keystrokes back into the worker, without the worker's own lifecycle
//! ever depending on whether anyone is attached.
//!
//! [`AttachServer`] owns the socket end-to-end: it binds and secures the
//! socket file (mirroring [`crate::ipc::Server::bind`]'s discipline --
//! stale-file removal, `sun_path` length guard, `0600` tightening), keeps
//! a 64 KiB ring buffer of the most recent output so a late-joining viewer
//! sees roughly a screenful of context instead of a bare cursor, and
//! forwards every byte a viewer types both into the worker (via
//! [`AttachTarget::write_input`]) and into an `on_user_input` callback.
//! Viewer disconnects are never fatal to the worker: each viewer
//! connection is its own task, and dropping/aborting it never touches
//! [`AttachTarget`] itself.
//!
//! [`AttachTarget`] is a small trait seam over the byte-in/byte-out
//! surface [`crate::supervisor::PtyProcess`] exposes, so the socket,
//! ring-buffer, and fan-out logic below can be exercised against a fake
//! in tests without a real PTY. At least one test still runs the whole
//! composed path against a real `PtyProcess`.
//!
//! `AttachServer` holds the target as `Arc<dyn AttachTarget>`: the caller
//! keeps its own clone (e.g. the orchestrator, for its own supervision of
//! the worker), and `AttachServer` only ever drops *its* clone -- the
//! worker's lifecycle is never coupled to whether an attach server is
//! running, or to any single viewer connecting or disconnecting.
//!
//! The client half ([`connect`], [`pump`]) is the plumbing `crewd attach`
//! drives: connecting to a resolved socket path and pumping bytes
//! bidirectionally between the socket and an abstract input/output pair.
//! Raw terminal mode itself (putting the real stdin fd into raw mode) is
//! deliberately not here -- it lives as a thin, untested wrapper in
//! `cli.rs` around [`pump`], which is generic over any `AsyncRead`/
//! `AsyncWrite` pair and so is fully testable with in-memory pipes.
//!
//! WP8 delivers the `on_user_input` callback as a seam only: this module
//! proves bytes typed by a viewer reach the callback, but nothing yet
//! constructs a `RuntimeEvent::OutOfBandInput` from it. That production
//! wiring (through the adapter's event sink and the `Redactor`) lands in
//! WP11, once a TUI adapter exists to own the run context the event needs.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use crate::ipc::{PeerCredentialReader, socket_path_within_limit};
use crate::supervisor::{PtyProcess, SupervisorError};

/// Ring buffer capacity replayed to a newly-connected viewer.
const RING_BUFFER_CAPACITY: usize = 64 * 1024;

/// Broadcast capacity for the server's own viewer fan-out channel (distinct
/// from [`PtyProcess`]'s internal output channel): a lagging viewer skips
/// ahead rather than exerting backpressure on the collector.
const VIEWER_CHANNEL_CAPACITY: usize = 256;

/// Read chunk size for both the socket and the input/output sides of
/// [`pump`].
const READ_CHUNK_BYTES: usize = 4096;

/// CREW-30: the fixed byte sequence [`serve_viewer`] writes to a newly
/// accepted connection *before* replaying the ring-buffer snapshot or
/// forwarding any real pane output -- the one thing a bare `connect()`
/// can never prove on its own. A completed connect shows a listening fd
/// exists at the socket path; it does not show a real `AttachServer` is
/// behind it. An fd a racing `fork()`'d child inherited (CREW-30: macOS
/// has no atomic `SOCK_CLOEXEC`, so `socket()` then
/// `fcntl(FD_CLOEXEC)` leaves a window) answers that same connect and
/// then never speaks -- proven with a 40,000-iteration reproducer that
/// found 0 false positives without concurrent forking and 452 with it,
/// every one unable to complete even a single write/read round trip.
/// [`pane_socket::is_live`] requires this marker, not just a completed
/// connect, before calling a pane live.
///
/// Versioned (`ATTACH1`) so a future protocol change can bump it without
/// silently breaking either direction of cross-version compatibility.
/// `crewd attach`'s own client path (see [`pump`]'s caller in `cli.rs`)
/// must tolerate an *older* daemon's `AttachServer` that predates this
/// marker and never sends it at all -- never blocking on it forever, and
/// never discarding real pane output it mistook for an absent marker.
pub const LIVENESS_MARKER: &[u8] = b"CREWATTACH1\n";

/// Errors from the attach server and client plumbing.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// `path` exceeds the platform `sun_path` bound; binding it would
    /// silently truncate the socket path.
    #[error("attach socket path {path:?} exceeds the platform sun_path limit")]
    SocketPathTooLong { path: PathBuf },
    /// The socket could not be bound at `path`.
    #[error("failed to bind attach socket at {path:?}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A filesystem operation on the socket (removing a stale socket,
    /// tightening permissions, a read/write during pumping) failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// Writing to the [`AttachTarget`] failed (e.g. the worker exited and
    /// its PTY closed).
    #[error(transparent)]
    Target(#[from] SupervisorError),
}

/// What an [`AttachServer`] fans output out from and forwards viewer input
/// into. [`PtyProcess`] is the production implementation; tests use a fake
/// so the socket/ring-buffer/broadcast logic never needs a real PTY.
///
/// `write_input` returns a boxed future rather than being a native
/// `async fn` so this trait stays object-safe -- [`AttachServer`] holds it
/// as `Arc<dyn AttachTarget>`, shared (never owned outright) with every
/// viewer connection.
pub trait AttachTarget: Send + Sync {
    /// Writes viewer-typed bytes into the target's input (e.g. a PTY
    /// master).
    fn write_input<'a>(
        &'a self,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), AttachError>> + Send + 'a>>;

    /// Subscribes to this target's live output. Lagging viewers skip
    /// ahead, matching [`PtyProcess::subscribe_output`]'s own discipline.
    fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>>;
}

impl AttachTarget for PtyProcess {
    fn write_input<'a>(
        &'a self,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<(), AttachError>> + Send + 'a>> {
        // `self.write_input(..)` here resolves to `PtyProcess`'s own
        // inherent method (inherent methods always win over trait methods
        // of the same name), not a recursive call into this trait method.
        Box::pin(async move { self.write_input(&bytes).await.map_err(AttachError::from) })
    }

    fn subscribe_output(&self) -> broadcast::Receiver<Vec<u8>> {
        self.subscribe_output()
    }
}

/// A fixed-capacity byte ring: the most recent [`RING_BUFFER_CAPACITY`]
/// bytes of output, replayed in full to each newly-attached viewer.
struct RingBuffer {
    buf: VecDeque<u8>,
    capacity: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend(chunk.iter().copied());
        while self.buf.len() > self.capacity {
            self.buf.pop_front();
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

/// Shared state the accept loop, output collector, and every viewer
/// connection all read or write through `Arc`s.
struct Shared {
    ring: StdMutex<RingBuffer>,
    output_tx: broadcast::Sender<Vec<u8>>,
    target: Arc<dyn AttachTarget>,
    on_user_input: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    /// This process's effective uid, checked against each connecting
    /// peer's own uid before a single byte is read from it -- the same
    /// same-user boundary [`crate::ipc::server::Server::admit`] enforces
    /// on the main runtime socket.
    euid: u32,
    /// Whether the socket's parent directory (`RuntimePaths::panes`) is
    /// verified owner-only, computed once at [`AttachServer::start`]. The
    /// fallback admission signal for a peer whose credentials this
    /// platform cannot report.
    owner_only_verified: bool,
}

impl Shared {
    /// Atomically takes a ring-buffer snapshot and subscribes to future
    /// output, so a viewer sees every byte exactly once: nothing already
    /// in the snapshot is redelivered by the subscription, and nothing
    /// sent by the collector between the two steps is lost, because both
    /// steps happen under the same lock the collector also holds while it
    /// appends to the ring and broadcasts in the same critical section.
    fn snapshot_and_subscribe(&self) -> (Vec<u8>, broadcast::Receiver<Vec<u8>>) {
        let ring = self.ring.lock().expect("attach ring buffer lock");
        let snapshot = ring.snapshot();
        let rx = self.output_tx.subscribe();
        (snapshot, rx)
    }
}

/// A running per-worker attach server. Bind with [`AttachServer::start`];
/// [`AttachServer::stop`] (or dropping the server) closes every connected
/// viewer and removes the socket file. Never affects the supervised
/// worker itself -- only viewer connections (and this server's own
/// `Arc<dyn AttachTarget>` clone) are torn down.
pub struct AttachServer {
    socket_path: PathBuf,
    accept_handle: AbortHandle,
    collector_handle: AbortHandle,
    clients: Arc<StdMutex<Vec<AbortHandle>>>,
}

impl AttachServer {
    /// Binds `path` (removing any stale socket left by a previous run,
    /// then tightening it to mode `0600`) and starts serving viewers.
    ///
    /// # Errors
    /// Returns [`AttachError`] if `path` exceeds the platform `sun_path`
    /// bound, or the socket cannot be bound or secured.
    pub fn start(
        path: PathBuf,
        target: Arc<dyn AttachTarget>,
        on_user_input: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    ) -> Result<Self, AttachError> {
        if !socket_path_within_limit(&path) {
            return Err(AttachError::SocketPathTooLong { path });
        }

        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(AttachError::Io(err)),
        }

        let listener = UnixListener::bind(&path).map_err(|source| AttachError::Bind {
            path: path.clone(),
            source,
        })?;

        let mut permissions = std::fs::metadata(&path)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&path, permissions)?;

        let euid = nix::unistd::Uid::effective().as_raw();
        let owner_only_verified = crate::security::parent_dir_is_owner_only(&path, euid);

        let pty_rx = target.subscribe_output();
        let (output_tx, _) = broadcast::channel(VIEWER_CHANNEL_CAPACITY);

        let shared = Arc::new(Shared {
            ring: StdMutex::new(RingBuffer::new(RING_BUFFER_CAPACITY)),
            output_tx,
            target,
            on_user_input: Arc::from(on_user_input),
            euid,
            owner_only_verified,
        });

        let clients: Arc<StdMutex<Vec<AbortHandle>>> = Arc::new(StdMutex::new(Vec::new()));

        let collector_handle =
            tokio::spawn(run_collector(pty_rx, Arc::clone(&shared))).abort_handle();
        let accept_handle =
            tokio::spawn(run_accept_loop(listener, shared, Arc::clone(&clients))).abort_handle();

        Ok(Self {
            socket_path: path,
            accept_handle,
            collector_handle,
            clients,
        })
    }

    /// The bound socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Stops the server: aborts every connected viewer's task (closing
    /// its socket), aborts the accept loop and output collector, and
    /// removes the socket file. Idempotent -- safe to call more than
    /// once, and safe even if nothing ever connected. Never touches the
    /// worker: this only drops `AttachServer`'s own `Arc` clone of the
    /// [`AttachTarget`]; as long as the caller kept its own clone (e.g.
    /// the orchestrator supervising the worker), the target -- and the
    /// worker it fronts -- lives on.
    pub fn stop(&self) {
        self.accept_handle.abort();
        self.collector_handle.abort();
        let clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for client in clients.iter() {
            client.abort();
        }
        drop(clients);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

impl Drop for AttachServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Drains the target's live output into the ring buffer and the viewer
/// fan-out channel, forever (until the target's output channel closes,
/// e.g. the worker exited and dropped its sender).
async fn run_collector(mut pty_rx: broadcast::Receiver<Vec<u8>>, shared: Arc<Shared>) {
    loop {
        match pty_rx.recv().await {
            Ok(chunk) => {
                let mut ring = shared.ring.lock().expect("attach ring buffer lock");
                ring.push(&chunk);
                // Sent while still holding the ring lock: this is the
                // other half of the atomicity `snapshot_and_subscribe`
                // relies on to guarantee a viewer sees every byte exactly
                // once.
                let _ = shared.output_tx.send(chunk);
                drop(ring);
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Accepts viewer connections until the listener errors (e.g. this task
/// itself is aborted by [`AttachServer::stop`]), applies the same-user
/// peer-credential boundary [`crate::ipc::server::Server::admit`] enforces
/// on the main runtime socket, and spawns one task per admitted
/// connection, recording its abort handle so `stop` can close it. A
/// rejected connection is dropped here, before a single byte is read from
/// it, never handed to [`serve_viewer`].
async fn run_accept_loop(
    listener: UnixListener,
    shared: Arc<Shared>,
    clients: Arc<StdMutex<Vec<AbortHandle>>>,
) {
    let credential_reader = crate::ipc::SystemPeerCredentialReader;
    loop {
        let Ok((stream, _addr)) = listener.accept().await else {
            return;
        };

        let creds = credential_reader.read(&stream);
        if !crate::security::admit_same_uid(creds.uid, shared.euid, shared.owner_only_verified) {
            tracing::warn!(
                peer_uid = creds.uid,
                expected = shared.euid,
                "rejecting attach connection from a different uid before reading any bytes"
            );
            drop(stream);
            continue;
        }

        let (snapshot, viewer_rx) = shared.snapshot_and_subscribe();
        let target = Arc::clone(&shared.target);
        let on_user_input = Arc::clone(&shared.on_user_input);
        let handle = tokio::spawn(serve_viewer(
            stream,
            snapshot,
            viewer_rx,
            target,
            on_user_input,
        ));
        clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(handle.abort_handle());
    }
}

/// Serves one connected viewer: writes the CREW-30 [`LIVENESS_MARKER`],
/// replays the ring-buffer snapshot, then pumps live output to the viewer
/// and viewer bytes into both the target and `on_user_input`, until the
/// socket closes or errors. A single task owns both halves of the split
/// socket so aborting it (as `stop` does) closes the connection
/// immediately in both directions.
async fn serve_viewer(
    stream: UnixStream,
    snapshot: Vec<u8>,
    mut viewer_rx: broadcast::Receiver<Vec<u8>>,
    target: Arc<dyn AttachTarget>,
    on_user_input: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    let (mut read_half, mut write_half) = stream.into_split();

    // Written first, always, before a single byte of real pane output:
    // this is what lets a connect that merely completed (CREW-30 -- an fd
    // a raced fork()'d child inherited also does this much) be told apart
    // from a connect answered by this function actually running.
    if write_half.write_all(LIVENESS_MARKER).await.is_err() {
        return;
    }

    if !snapshot.is_empty() && write_half.write_all(&snapshot).await.is_err() {
        return;
    }

    let mut buf = [0u8; READ_CHUNK_BYTES];
    loop {
        tokio::select! {
            read = read_half.read(&mut buf) => {
                match read {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        let bytes = buf[..n].to_vec();
                        on_user_input(bytes.clone());
                        // A failed keystroke delivery is degraded control,
                        // never silence: the viewer typed and nothing
                        // reached the vendor process (WP8 deferred minor).
                        if let Err(err) = target.write_input(bytes).await {
                            tracing::warn!(error = %err, "attach write_input failed; \
                                keystrokes may not reach the vendor process");
                        }
                    }
                }
            }
            recv = viewer_rx.recv() => {
                match recv {
                    Ok(chunk) => {
                        if write_half.write_all(&chunk).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

// ------------------------------------------------------------- client

/// How a [`pump`] session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpOutcome {
    /// The attach socket closed (the server stopped, or the connection
    /// otherwise dropped).
    SocketClosed,
    /// The input side closed (e.g. stdin EOF).
    InputClosed,
    /// The viewer typed the detach byte.
    Detached,
}

/// Connects to an already-bound attach socket at `path`.
///
/// # Errors
/// Returns [`AttachError::Io`] if the connection fails.
pub async fn connect(path: &Path) -> Result<UnixStream, AttachError> {
    UnixStream::connect(path).await.map_err(AttachError::from)
}

/// CREW-30: consumes [`LIVENESS_MARKER`] from a freshly-connected `socket`
/// if it's there, bounded by `timeout`. Returns `None` when the marker
/// was read in full -- nothing left to reclaim, [`pump`] can start
/// straight away. Returns `Some(bytes)` for every other outcome (a
/// partial read, a timeout, or bytes that don't match): `bytes` is
/// exactly what was actually read off the wire and the caller **must**
/// write it to its own output before pumping, never discard it.
///
/// That second case is not a failure path to special-case away -- it is
/// the ordinary way `crewd attach` talks to an *older* daemon whose
/// `AttachServer` predates this marker and never sends one at all. Such
/// a daemon's first bytes are real pane output, indistinguishable at the
/// wire level from "not the marker", so a client that unconditionally
/// blocked on the marker would hang forever attaching to it, and one
/// that read-with-timeout-and-discarded would silently drop that
/// output. Bounding the read and reclaiming whatever it captured is what
/// makes an old daemon a merely-slower-by-one-timeout attach instead of
/// either failure.
pub async fn consume_marker_or_reclaim(
    socket: &mut UnixStream,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; LIVENESS_MARKER.len()];
    let mut filled = 0;
    let deadline = tokio::time::Instant::now() + timeout;
    while filled < buf.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.read(&mut buf[filled..])).await {
            Ok(Ok(0)) => break, // EOF: the peer closed before sending anything more.
            Ok(Ok(n)) => filled += n,
            Ok(Err(_)) | Err(_) => break, // read error or the bounded timeout elapsed.
        }
    }
    if filled == buf.len() && buf == LIVENESS_MARKER {
        return None;
    }
    buf.truncate(filled);
    Some(buf)
}

/// CREW-18: the crew identity shown in a pane's title bar/tab, set exactly
/// once when `crewd attach`'s pump starts. `worker_id` is truncated to its
/// first 8 hex characters, matching the short-id convention already used
/// for run ids in the `/crew` widget and this codebase's own manual-test
/// walkthrough -- a full UUID would swamp a narrow tab.
#[must_use]
pub fn pane_title(worker_id: &str, adapter: &str) -> String {
    let short = worker_id.get(..8).unwrap_or(worker_id);
    format!("crew: {short} ({adapter})")
}

/// Wraps `title` in the OSC title-setting escape sequence (`OSC 0`, BEL
/// terminator) that Terminal.app, iTerm2, Ghostty, and tmux all
/// understand -- setting both the window and tab/icon title in one write.
/// Deliberately a single, one-shot write: the vendor's own later title
/// sequences (if it emits any) simply overwrite this one and win, exactly
/// like any other program sharing a terminal would -- there is no
/// re-assert loop fighting to keep this value pinned.
///
/// `title` is spliced into an escape sequence, which makes this a
/// terminal-escape-injection sink: a stray control byte in it (BEL
/// terminates the sequence early, ESC starts a new one) would let
/// whatever supplied `title` inject arbitrary escapes into the pane. An
/// OSC title string legitimately never contains a C0 control character
/// (`< 0x20`) or DEL (`0x7f`), so every such byte is stripped here,
/// structurally, rather than trusted from the caller -- `pane_title`'s
/// `adapter` is bounded to the reserved wire-names today, but this sink
/// must stay safe regardless of what future caller feeds it.
#[must_use]
pub fn osc_set_title(title: &str) -> String {
    let sanitized: String = title.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b]0;{sanitized}\x07")
}

/// Pumps bytes bidirectionally between `input`/`output` (a terminal's
/// stdin/stdout in production, or an in-memory pipe in tests) and an
/// already-connected attach `socket`, until the socket closes, `input`
/// closes, or the viewer types `detach_byte`.
///
/// Generic over `input`/`output` so this is fully testable without a real
/// TTY: putting the real stdin fd into raw mode is a thin, untested
/// wrapper in `cli.rs` around this function, not part of it.
///
/// # Errors
/// Returns [`AttachError::Io`] on a read or write failure on either side.
pub async fn pump<R, W>(
    socket: UnixStream,
    mut input: R,
    mut output: W,
    detach_byte: u8,
) -> Result<PumpOutcome, AttachError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut sock_read, mut sock_write) = socket.into_split();
    let mut in_buf = [0u8; READ_CHUNK_BYTES];
    let mut sock_buf = [0u8; READ_CHUNK_BYTES];

    loop {
        tokio::select! {
            read = input.read(&mut in_buf) => {
                match read {
                    Ok(0) => return Ok(PumpOutcome::InputClosed),
                    Err(err) => return Err(AttachError::Io(err)),
                    Ok(n) => {
                        let chunk = &in_buf[..n];
                        if let Some(pos) = chunk.iter().position(|&b| b == detach_byte) {
                            if pos > 0 {
                                sock_write.write_all(&chunk[..pos]).await?;
                            }
                            return Ok(PumpOutcome::Detached);
                        }
                        sock_write.write_all(chunk).await?;
                    }
                }
            }
            read = sock_read.read(&mut sock_buf) => {
                match read {
                    Ok(0) => return Ok(PumpOutcome::SocketClosed),
                    Err(err) => return Err(AttachError::Io(err)),
                    Ok(n) => {
                        output.write_all(&sock_buf[..n]).await?;
                        output.flush().await?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod title_tests {
    use super::*;

    #[test]
    fn pane_title_truncates_the_worker_id_to_8_chars() {
        assert_eq!(
            pane_title("7a8b9c0d-1e2f-4a3b-8c4d-5e6f7a8b9c0d", "claude"),
            "crew: 7a8b9c0d (claude)"
        );
    }

    #[test]
    fn pane_title_uses_the_whole_worker_id_when_shorter_than_8_chars() {
        assert_eq!(pane_title("abc", "codex"), "crew: abc (codex)");
    }

    /// Terminal-escape-injection sink: `title` ends up spliced verbatim
    /// into an escape sequence, so a stray control byte in it (`\x07` BEL
    /// would terminate the sequence early, `\x1b` ESC would start a new
    /// one) lets whatever supplied `title` inject arbitrary escapes into
    /// the pane. `adapter` is bounded to the reserved wire-names today, but
    /// that is exactly the kind of upstream-bounded assumption this whole
    /// wave found unsafe to rely on -- so this is enforced structurally at
    /// the sink, not by trusting the caller.
    #[test]
    fn osc_set_title_strips_control_bytes_so_injection_cannot_reach_the_output() {
        let malicious = "crew: \x07\x1b]0;pwned\x07 (claude)";
        let escaped = osc_set_title(malicious);
        assert_eq!(escaped, "\x1b]0;crew: ]0;pwned (claude)\x07");
        assert_eq!(
            escaped.matches('\x1b').count(),
            1,
            "the only ESC must be the sequence's own opener, not one smuggled in via the title: {escaped:?}"
        );
        assert_eq!(
            escaped.matches('\x07').count(),
            1,
            "the only BEL must be the sequence's own terminator: {escaped:?}"
        );
    }

    #[test]
    fn osc_set_title_wraps_in_the_osc_0_escape_with_a_bel_terminator() {
        assert_eq!(
            osc_set_title("crew: 7a8b9c0d (claude)"),
            "\x1b]0;crew: 7a8b9c0d (claude)\x07"
        );
    }
}
