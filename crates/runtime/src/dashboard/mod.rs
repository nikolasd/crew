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

use batman_protocol::{EventEnvelope, ProjectId};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, broadcast};

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
    shutdown: Arc<Notify>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl DashboardServer {
    /// Binds `127.0.0.1:<port>` (never a routable interface; `0` picks an
    /// ephemeral port, used by tests) and starts serving.
    ///
    /// # Errors
    /// Returns the bind error (e.g. the port is taken). Callers treat this
    /// as non-fatal: a daemon without its dashboard still orchestrates.
    pub async fn bind(port: u16, deps: DashboardDeps) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(Notify::new());
        let accept_shutdown = Arc::clone(&shutdown);
        let accept_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = accept_shutdown.notified() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else { continue };
                        let deps = deps.clone();
                        let shutdown = Arc::clone(&accept_shutdown);
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, deps, shutdown).await;
                        });
                    }
                }
            }
        });
        Ok(Self {
            local_addr,
            shutdown,
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
        self.shutdown.notify_waiters();
        self.accept_task.abort();
    }
}

/// Longest accepted request head line/header; anything larger is not a
/// dashboard request.
const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_COUNT: usize = 100;

async fn handle_connection(
    stream: TcpStream,
    deps: DashboardDeps,
    shutdown: Arc<Notify>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);

    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    if request_line.len() > MAX_LINE_BYTES {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    // Drain (and ignore) the request headers; the dashboard needs none.
    for _ in 0..MAX_HEADER_COUNT {
        let mut header = String::new();
        reader.read_line(&mut header).await?;
        if header.len() > MAX_LINE_BYTES {
            return Ok(());
        }
        if header == "\r\n" || header == "\n" || header.is_empty() {
            break;
        }
    }

    let mut stream = reader.into_inner();
    if method != "GET" {
        return write_simple(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "the dashboard is read-only\n",
        )
        .await;
    }

    match path.as_str() {
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
        "/events" => serve_sse(&mut stream, &deps, &shutdown).await,
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
/// layer's `run/list` and `worker/list` use. `budgets` and
/// `pendingEscalations` are part of the stable shape already; they fill
/// in when the budgets (WP19) and escalations (WP20) tables land.
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
    let state = serde_json::json!({
        "runs": runs.get("runs").cloned().unwrap_or_else(|| serde_json::json!([])),
        "workers": workers.get("workers").cloned().unwrap_or_else(|| serde_json::json!([])),
        "budgets": [],
        "pendingEscalations": [],
    });
    Ok(state.to_string())
}

/// One SSE viewer: one `data:` frame per broadcast [`EventEnvelope`],
/// until the viewer disconnects, the daemon shuts down, or (on lag) the
/// subscription skips ahead -- a dashboard that misses frames re-fetches
/// state; it must never exert backpressure on the daemon.
async fn serve_sse(
    stream: &mut TcpStream,
    deps: &DashboardDeps,
    shutdown: &Notify,
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
            () = shutdown.notified() => return Ok(()),
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

async fn write_simple(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
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
