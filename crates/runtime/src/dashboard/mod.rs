//! The daemon-hosted dashboard: a read-only, localhost-only HTTP listener
//! serving one self-contained HTML status page (`GET /`), an orchestration
//! state snapshot (`GET /api/state`), and a live event stream
//! (`GET /events`, SSE fed from the same broadcast every committed
//! mutation already fans out to).
//!
//! Deliberately hand-rolled HTTP/1.1 over `tokio::net::TcpListener` -- the
//! daemon takes no web-framework dependency for three GET routes. The
//! dashboard is a projection, never a control surface: every non-GET
//! method is rejected with 405, and all reads go through the same
//! [`DatabaseHandle`] domain ops the RPC layer uses -- never a second
//! connection to the journal.

mod page;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crew_protocol::{EventEnvelope, ProjectId};
use std::sync::Arc as StdArc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

use crate::db::DatabaseHandle;
use crate::service::query;

/// Everything a dashboard connection needs to answer requests.
#[derive(Clone)]
pub struct DashboardDeps {
    pub db: Arc<DatabaseHandle>,
    pub project_id: ProjectId,
    /// The server's live event broadcast; `/events` subscribes to it.
    pub events_tx: broadcast::Sender<EventEnvelope>,
}

/// A running dashboard listener. [`DashboardServer::stop`] closes it; the
/// lifecycle stops it before the journal drain on shutdown.
pub struct DashboardServer {
    local_addr: SocketAddr,
    /// WP22 minor, closed: a `watch` channel rather than `Notify`, so a
    /// connection task spawned AFTER `stop()` still observes the shutdown
    /// (a `notify_waiters` race left exactly that leak window).
    shutdown: watch::Sender<bool>,
    accept_task: tokio::task::JoinHandle<()>,
}

/// Ceiling on concurrently open dashboard connections. Each held
/// connection (an SSE viewer especially) pins a task and a socket;
/// unbounded viewers would let one curious browser tab-farm exhaust the
/// daemon's task budget.
const MAX_CONNECTIONS: usize = 64;

impl DashboardServer {
    /// Binds `127.0.0.1:<port>` (never a routable interface; `0` picks an
    /// ephemeral port, used by tests) and starts serving, using the
    /// production [`HEADER_READ_TIMEOUT`] for every connection's
    /// request-line/header phase.
    ///
    /// # Errors
    /// Returns the bind error (e.g. the port is taken). Callers treat this
    /// as non-fatal: a daemon without its dashboard still orchestrates.
    pub async fn bind(port: u16, deps: DashboardDeps) -> std::io::Result<Self> {
        Self::bind_with_header_timeout(port, deps, HEADER_READ_TIMEOUT).await
    }

    /// Same as [`Self::bind`], but with an explicit header-read timeout --
    /// exists so tests can exercise the timeout path without waiting out
    /// the production duration.
    ///
    /// # Errors
    /// Returns the bind error (e.g. the port is taken). Callers treat this
    /// as non-fatal: a daemon without its dashboard still orchestrates.
    pub async fn bind_with_header_timeout(
        port: u16,
        deps: DashboardDeps,
        header_read_timeout: std::time::Duration,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let local_addr = listener.local_addr()?;
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut accept_shutdown = shutdown_rx.clone();
        let permits = StdArc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        let accept_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_shutdown.changed() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else { continue };
                        let deps = deps.clone();
                        let mut shutdown = shutdown_rx.clone();
                        let permit = match permits.clone().acquire_owned().await {
                            // A capped-out dashboard drops the new
                            // connection instead of piling tasks; the
                            // permit returns when the connection task ends.
                            Ok(permit) => permit,
                            Err(_) => continue,
                        };
                        tokio::spawn(async move {
                            let (read_half, write_half) = stream.into_split();
                            // Hold the cap permit for the connection's
                            // whole lifetime; dropping releases the slot.
                            let _permit = permit;
                            let _ = handle_connection(
                                read_half,
                                write_half,
                                deps,
                                &mut shutdown,
                                header_read_timeout,
                            )
                            .await;
                        });
                    }
                }
            }
        });
        Ok(Self {
            local_addr,
            shutdown: shutdown_tx,
            accept_task,
        })
    }

    /// The bound address (always loopback).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stops accepting and closes live SSE streams.
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
        self.accept_task.abort();
    }
}

/// Longest accepted request head line/header; anything larger is not a
/// dashboard request. Enforced as the connection is read (never after the
/// fact): [`read_bounded_line`] refuses to buffer past this many bytes
/// while waiting for a line's terminator, so a peer cannot grow an
/// unbounded buffer by never sending `\n`.
const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_COUNT: usize = 100;

/// Ceiling on how long a connection may take to finish sending its
/// request line and headers. A peer that connects and sends nothing, or
/// trickles bytes without ever completing the head, is disconnected
/// rather than tying up a task (and a `BufReader`) indefinitely.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Why [`read_request_head`] did not produce a head.
enum HeadReadError {
    /// The underlying connection errored or closed before a full head was
    /// read; the caller closes without responding.
    Io,
    /// A line (the request line or a header) exceeded [`MAX_LINE_BYTES`]
    /// while still buffering for its terminator.
    TooLong,
}

impl From<std::io::Error> for HeadReadError {
    fn from(_err: std::io::Error) -> Self {
        HeadReadError::Io
    }
}

struct RequestHead {
    method: String,
    path: String,
}

/// Reads one line terminated by `\n`, refusing to buffer more than
/// [`MAX_LINE_BYTES`] while waiting for the terminator. Uses the reader's
/// own `fill_buf`/`consume` rather than `read_line`, so bytes belonging to
/// the *next* line (already buffered by one `read` syscall) are never
/// discarded, and so the cap is enforced as bytes arrive rather than only
/// once a full line has been buffered.
async fn read_bounded_line<R>(reader: &mut R) -> Result<String, HeadReadError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut collected: Vec<u8> = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Err(HeadReadError::Io);
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            collected.extend_from_slice(&buf[..=pos]);
            reader.consume(pos + 1);
            return if collected.len() > MAX_LINE_BYTES {
                Err(HeadReadError::TooLong)
            } else {
                Ok(String::from_utf8_lossy(&collected).into_owned())
            };
        }
        collected.extend_from_slice(buf);
        let consumed = buf.len();
        reader.consume(consumed);
        if collected.len() > MAX_LINE_BYTES {
            return Err(HeadReadError::TooLong);
        }
    }
}

/// Reads the request line and drains (and ignores -- the dashboard needs
/// none) the headers that follow, bounding each line's size. Callers wrap
/// this in a [`tokio::time::timeout`]; it applies no timeout itself.
async fn read_request_head<R>(reader: &mut R) -> Result<RequestHead, HeadReadError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let request_line = read_bounded_line(reader).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    for _ in 0..MAX_HEADER_COUNT {
        let header = read_bounded_line(reader).await?;
        if header == "\r\n" || header == "\n" {
            break;
        }
    }

    Ok(RequestHead { method, path })
}

async fn handle_connection(
    read_half: tokio::net::tcp::OwnedReadHalf,
    write_half: tokio::net::tcp::OwnedWriteHalf,
    deps: DashboardDeps,
    shutdown: &mut watch::Receiver<bool>,
    header_read_timeout: std::time::Duration,
) -> std::io::Result<()> {
    // The caller holds the connection-cap permit for this task's lifetime.
    let mut reader = BufReader::new(read_half);

    let head = match tokio::time::timeout(header_read_timeout, read_request_head(&mut reader)).await
    {
        Ok(Ok(head)) => head,
        Ok(Err(HeadReadError::TooLong)) => {
            let mut stream = write_half;
            let _ = reader;
            return write_simple(
                &mut stream,
                "431 Request Header Fields Too Large",
                "text/plain; charset=utf-8",
                "request line or header exceeded the size limit\n",
            )
            .await;
        }
        // Connection errored or closed mid-head, or the peer took too
        // long: close without responding, same as any other malformed
        // connection.
        Ok(Err(HeadReadError::Io)) | Err(_) => return Ok(()),
    };

    let mut stream = write_half;
    if head.method != "GET" {
        return write_simple(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "the dashboard is read-only\n",
        )
        .await;
    }

    match head.path.as_str() {
        "/" => {
            write_simple(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                page::PAGE_HTML,
            )
            .await
        }
        "/api/state" => match state_snapshot(&deps).await {
            Ok(body) => write_simple(&mut stream, "200 OK", "application/json", &body).await,
            Err(message) => {
                write_simple(
                    &mut stream,
                    "500 Internal Server Error",
                    "text/plain; charset=utf-8",
                    &message,
                )
                .await
            }
        },
        "/events" => serve_sse(&mut stream, &deps, shutdown).await,
        _ => {
            write_simple(
                &mut stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                "not found\n",
            )
            .await
        }
    }
}

/// The `/api/state` snapshot, read through the same domain ops the RPC
/// layer's `run/list` and `worker/list` use. `budgets` (WP19) and
/// `pendingEscalations` (WP20) come from their own list ops over the same
/// handle -- never a second connection.
async fn state_snapshot(deps: &DashboardDeps) -> Result<String, String> {
    let runs = deps
        .db
        .run_domain_op(query::run_list_op(None, deps.project_id))
        .await
        .map_err(|e| e.to_string())?;
    let workers = deps
        .db
        .run_domain_op(query::worker_list_op(deps.project_id))
        .await
        .map_err(|e| e.to_string())?;
    let budgets = deps
        .db
        .run_domain_op(query::budget_list_op(deps.project_id))
        .await
        .map_err(|e| e.to_string())?;
    let escalations = deps
        .db
        .run_domain_op(query::pending_escalation_list_op(deps.project_id))
        .await
        .map_err(|e| e.to_string())?;
    let state = serde_json::json!({
        "runs": runs.get("runs").cloned().unwrap_or_else(|| serde_json::json!([])),
        "workers": workers.get("workers").cloned().unwrap_or_else(|| serde_json::json!([])),
        "budgets": budgets.get("budgets").cloned().unwrap_or_else(|| serde_json::json!([])),
        "pendingEscalations": escalations
            .get("pendingEscalations")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    });
    Ok(state.to_string())
}

/// One SSE viewer: one `data:` frame per broadcast [`EventEnvelope`],
/// until the viewer disconnects, the daemon shuts down, or (on lag) the
/// subscription skips ahead -- a dashboard that misses frames re-fetches
/// state; it must never exert backpressure on the daemon.
async fn serve_sse(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    deps: &DashboardDeps,
    shutdown: &mut watch::Receiver<bool>,
) -> std::io::Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\n\
              content-type: text/event-stream\r\n\
              cache-control: no-cache\r\n\
              connection: keep-alive\r\n\r\n",
        )
        .await?;
    stream.flush().await?;

    let mut rx = deps.events_tx.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            received = rx.recv() => match received {
                Ok(envelope) => {
                    let json = serde_json::to_string(&envelope)
                        .unwrap_or_else(|_| "{}".to_string());
                    stream.write_all(format!("data: {json}\n\n").as_bytes()).await?;
                    stream.flush().await?;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

async fn write_simple<W>(
    stream: &mut W,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         content-type: {content_type}\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}
