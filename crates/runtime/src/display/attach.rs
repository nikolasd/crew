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

/// Serves one connected viewer: replays the ring-buffer snapshot, then
/// pumps live output to the viewer and viewer bytes into both the target
/// and `on_user_input`, until the socket closes or errors. A single task
/// owns both halves of the split socket so aborting it (as `stop` does)
/// closes the connection immediately in both directions.
async fn serve_viewer(
    stream: UnixStream,
    snapshot: Vec<u8>,
    mut viewer_rx: broadcast::Receiver<Vec<u8>>,
    target: Arc<dyn AttachTarget>,
    on_user_input: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    let (mut read_half, mut write_half) = stream.into_split();

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
                        let _ = target.write_input(bytes).await;
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
