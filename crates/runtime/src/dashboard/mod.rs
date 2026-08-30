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
//!
//! # Two things not to "simplify"
//!
//! **The page re-fetches the server-side projection; do not add a client
//! reducer.** `page.rs` calls `/api/state` for the snapshot and uses
//! `/events` purely as a signal to re-fetch. That looks naive next to
//! applying events incrementally in the browser, and it is deliberate: a
//! second reducer would have to mirror the server's projection semantics
//! exactly, forever, and the two would drift. It also makes `broadcast`
//! lag self-healing -- a viewer that misses an event still re-reads the
//! authoritative snapshot on the next one, where an incremental reducer
//! would be silently wrong from then on.
//!
//! **The per-run transcript is served from the JOURNAL, never from the
//! vendor's transcript file.** Reading the file directly would look like a
//! shortcut and would route around the redaction boundary: journaled
//! content has crossed `Classified` and had secrets stripped (ADR-0006),
//! while the vendor's own file on disk has not. `/api/run/<id>/events`
//! therefore reads through the same domain ops as everything else here.
//!
//! # Access control
//!
//! A TCP listener cannot do what the IPC socket does. `ipc` enforces
//! same-user access twice -- `check_owner_only` on the socket directory
//! and `admit_same_uid` on every peer, using kernel-reported credentials
//! that TCP has no equivalent for. Binding to loopback keeps other *hosts*
//! out; it does nothing about other *local users or processes*.
//!
//! So every route requires a bearer token generated per daemon run and
//! printed once at startup. The token arrives either as `?token=` (the
//! URL an operator pastes) or as the `crew_dashboard` cookie; a valid
//! query token on `GET /` is exchanged for that cookie via a redirect, so
//! the secret leaves the address bar -- and browser history, and any
//! `Referer` -- after first load.

mod page;

/// The served page's markup. Public so the brand-compliance tests can
/// assert against it: BRAND.md §01 forbids redrawing the mark, and the
/// only way to enforce that mechanically is to compare the inlined copy
/// against the master SVG on disk.
pub use page::PAGE_HTML;

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
    /// The per-run bearer token every route requires. Exposed so the
    /// lifecycle can print the one URL that works.
    token: StdArc<str>,
}

/// Ceiling on concurrently open dashboard connections. Each held
/// connection (an SSE viewer especially) pins a task and a socket;
/// unbounded viewers would let one curious browser tab-farm exhaust the
/// daemon's task budget.
const MAX_CONNECTIONS: usize = 64;

/// Ceiling on events returned by one `/api/run/<id>/events` read. A
/// long-lived worker's transcript grows without bound; an HTTP response
/// should not. The page shows the oldest window and says so rather than
/// silently truncating.
const MAX_TRANSCRIPT_EVENTS: u32 = 2000;

/// The cookie the dashboard exchanges a valid `?token=` for.
const TOKEN_COOKIE: &str = "crew_dashboard";

/// Generates the per-run dashboard token: 32 hex characters from 16 bytes
/// of `/dev/urandom`.
///
/// Read from the OS CSPRNG directly rather than reaching for a new
/// dependency, and deliberately **not** from `uuid`, whose only feature
/// enabled in this workspace is `v7` -- a time-ordered identifier whose
/// leading bits are the clock. Predictable is disqualifying for a bearer
/// token. `/dev/urandom` is present on both supported platforms (macOS and
/// glibc Linux, per the platform invariant); if it cannot be read the
/// dashboard refuses to start rather than falling back to something
/// weaker, because a guessable token reads exactly like a real one.
fn generate_token() -> std::io::Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Compares two tokens without leaking their common prefix length through
/// timing. Overkill on loopback and cheap enough not to argue about.
fn tokens_match(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The token presented by a request, if any: `?token=` first (the pasted
/// URL), then the `crew_dashboard` cookie (every request after the
/// redirect).
fn presented_token(query: Option<&str>, cookie: Option<&str>) -> Option<String> {
    if let Some(query) = query
        && let Some(value) = query
            .split('&')
            .filter_map(|pair| pair.strip_prefix("token="))
            .next()
    {
        return Some(value.to_string());
    }
    cookie.and_then(|header| {
        header
            .split(';')
            .map(str::trim)
            .filter_map(|pair| pair.strip_prefix(&format!("{TOKEN_COOKIE}=")))
            .next()
            .map(str::to_string)
    })
}

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

    /// The per-run bearer token every route requires.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
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
        let token: StdArc<str> = StdArc::from(generate_token()?.as_str());
        let accept_token = StdArc::clone(&token);
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
                        let token = StdArc::clone(&accept_token);
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
                                &token,
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
            token,
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
    /// The raw request target, query string included.
    path: String,
    /// The `Cookie` header's value, if the request sent one. The only
    /// header this dashboard reads -- see the module's access-control note.
    cookie: Option<String>,
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

/// Reads the request line and the headers that follow, bounding each
/// line's size. Only `Cookie` is retained (the token may arrive there);
/// every other header is drained and ignored. Callers wrap this in a
/// [`tokio::time::timeout`]; it applies no timeout itself.
async fn read_request_head<R>(reader: &mut R) -> Result<RequestHead, HeadReadError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let request_line = read_bounded_line(reader).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut cookie = None;
    for _ in 0..MAX_HEADER_COUNT {
        let header = read_bounded_line(reader).await?;
        if header == "\r\n" || header == "\n" {
            break;
        }
        // Header names are case-insensitive; a browser sends `Cookie` but
        // nothing obliges it to.
        let lowered = header.to_ascii_lowercase();
        if let Some(value) = lowered.strip_prefix("cookie:") {
            cookie = Some(value.trim().to_string());
        }
    }

    Ok(RequestHead {
        method,
        path,
        cookie,
    })
}

async fn handle_connection(
    read_half: tokio::net::tcp::OwnedReadHalf,
    write_half: tokio::net::tcp::OwnedWriteHalf,
    deps: DashboardDeps,
    token: &str,
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

    // Split the request target once: the token may ride the query, and
    // every route below matches on the path alone.
    let (path, query) = match head.path.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (head.path.as_str(), None),
    };

    // Access control before routing, so an unauthenticated request cannot
    // reach a handler at all -- not even a 404, which would otherwise
    // confirm which paths exist.
    let presented = presented_token(query, head.cookie.as_deref());
    let authorized = presented
        .as_deref()
        .is_some_and(|candidate| tokens_match(candidate, token));
    if !authorized {
        return write_simple(
            &mut stream,
            "401 Unauthorized",
            "text/plain; charset=utf-8",
            "the dashboard requires the token printed in the daemon log at startup\n",
        )
        .await;
    }

    // A valid token in the query on the page route is exchanged for a
    // cookie and redirected, so the secret stops travelling in the address
    // bar (and out of browser history and any `Referer`). `HttpOnly` keeps
    // page scripts from reading it; `SameSite=Strict` keeps another site
    // from causing an authenticated request.
    if path == "/" && query.is_some_and(|q| q.contains("token=")) {
        let cookie = format!("{TOKEN_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict");
        return write_redirect(&mut stream, "/", &cookie).await;
    }

    match path {
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
        // The per-run transcript. Read from the journal through the same
        // domain ops as every other route here -- see the module doc on
        // why the vendor's own transcript file is never served.
        transcript if transcript.starts_with("/api/run/") && transcript.ends_with("/events") => {
            let raw_id = transcript
                .trim_start_matches("/api/run/")
                .trim_end_matches("/events");
            match crew_protocol::RunId::parse(raw_id) {
                Ok(run_id) => match deps
                    .db
                    .run_domain_op(query::run_events_op(run_id, MAX_TRANSCRIPT_EVENTS))
                    .await
                {
                    Ok(value) => {
                        write_simple(
                            &mut stream,
                            "200 OK",
                            "application/json",
                            &value.to_string(),
                        )
                        .await
                    }
                    Err(err) => {
                        write_simple(
                            &mut stream,
                            "500 Internal Server Error",
                            "text/plain; charset=utf-8",
                            &err.to_string(),
                        )
                        .await
                    }
                },
                // A malformed id is the caller's error, not the server's --
                // and saying so beats a 500 that looks like a daemon fault.
                Err(_) => {
                    write_simple(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain; charset=utf-8",
                        "not a valid run id\n",
                    )
                    .await
                }
            }
        }
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
    let workers_json = workers
        .get("workers")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let usage = deps
        .db
        .run_domain_op(query::usage_by_run_op(deps.project_id))
        .await
        .map_err(|e| e.to_string())?;

    let mut workers_json = workers_json;
    let mut runs_json = runs
        .get("runs")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    annotate_runs_with_worker_profile(&mut runs_json, &workers_json);
    annotate_runs_with_usage(&mut runs_json, usage.get("usageByRun"));
    annotate_workers_with_spend(&mut workers_json, &runs_json);

    let state = serde_json::json!({
        "runs": runs_json,
        "workers": workers_json,
        "budgets": budgets.get("budgets").cloned().unwrap_or_else(|| serde_json::json!([])),
        "pendingEscalations": escalations
            .get("pendingEscalations")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    });
    Ok(state.to_string())
}

/// Copies each worker's `adapter` and `model` onto its runs' rows.
///
/// The page colours a run by its runtime (BRAND.md §02) and labels it
/// `adapter · model`, and a run row does not carry either field --
/// `row_to_run_json` is shared with `run/get` and `run/list`, and widening
/// it for one viewer's benefit would change a hot RPC the monitor calls on
/// every event.
///
/// Joined here rather than in the page because the snapshot already holds
/// both lists: this is one pass over data in hand, no extra query, and no
/// second derivation in the browser to drift from this one.
///
/// An adapter with no [BRAND.md §02] colour is still copied verbatim. The
/// *page* decides a name is unrecognised and renders it neutral; if the
/// runtime made that call, adding a colour later would mean changing Rust.
/// A run whose worker row is absent gets no fields at all, so the page can
/// tell "unbranded" from "unknown" rather than displaying a guess.
fn annotate_runs_with_worker_profile(runs: &mut serde_json::Value, workers: &serde_json::Value) {
    let mut profile_by_worker: std::collections::HashMap<&str, (&str, &str)> =
        std::collections::HashMap::new();
    for worker in workers.as_array().into_iter().flatten() {
        if let (Some(id), Some(adapter), Some(model)) = (
            worker["workerId"].as_str(),
            worker["profileRef"]["adapter"].as_str(),
            worker["profileRef"]["model"].as_str(),
        ) {
            profile_by_worker.insert(id, (adapter, model));
        }
    }

    for run in runs.as_array_mut().into_iter().flatten() {
        let Some((adapter, model)) = run["workerId"]
            .as_str()
            .and_then(|id| profile_by_worker.get(id))
            .copied()
        else {
            continue;
        };
        run["adapter"] = serde_json::Value::String(adapter.to_string());
        run["model"] = serde_json::Value::String(model.to_string());
    }
}

/// Attaches each run's folded usage to its row, as an explicit `null`
/// when its vendor reported none.
///
/// The `null` is the point. Copilot reports no usage at all under ACP v1,
/// and Codex reports tokens but never a price -- so "no cost" is a real,
/// common answer, and it is not zero. A zero is a number somebody
/// reported; this is the absence of one. Writing the key with `null` says
/// that, where omitting the key would leave the page unable to tell
/// "nothing was reported" from "this build does not compute usage".
fn annotate_runs_with_usage(
    runs: &mut serde_json::Value,
    usage_by_run: Option<&serde_json::Value>,
) {
    for run in runs.as_array_mut().into_iter().flatten() {
        let folded = run["runId"]
            .as_str()
            .and_then(|id| usage_by_run?.get(id))
            .cloned();
        run["usage"] = folded.unwrap_or(serde_json::Value::Null);
    }
}

/// Sums each worker's runs into one spend figure that names its own
/// coverage.
///
/// A dollar total over a worker whose runs include non-reporting vendors
/// understates its real spend, and presenting it bare would be a lie by
/// omission -- the reader has no way to see that three of five runs
/// contributed nothing. So the total ships with `runsTotal` and
/// `runsReportingCost`, and the page qualifies the figure whenever
/// coverage is partial. A worker whose every run reported can show a clean
/// total, because there the number really is the whole story.
///
/// `costUsd` stays `None` when no run reported a cost: a worker running a
/// vendor that never prices its turns shows an em-dash, not `$0.00`.
fn annotate_workers_with_spend(workers: &mut serde_json::Value, runs: &serde_json::Value) {
    for worker in workers.as_array_mut().into_iter().flatten() {
        let Some(worker_id) = worker["workerId"].as_str().map(str::to_string) else {
            continue;
        };
        let mut runs_total = 0_u64;
        let mut runs_reporting_cost = 0_u64;
        let mut runs_reporting_tokens = 0_u64;
        let mut cost: Option<f64> = None;
        let mut input = 0_u64;
        let mut output = 0_u64;

        for run in runs.as_array().into_iter().flatten() {
            if run["workerId"].as_str() != Some(worker_id.as_str()) {
                continue;
            }
            runs_total += 1;
            let usage = &run["usage"];
            if usage.is_null() {
                continue;
            }
            runs_reporting_tokens += 1;
            input += usage["inputTokens"].as_u64().unwrap_or(0);
            output += usage["outputTokens"].as_u64().unwrap_or(0);
            if let Some(run_cost) = usage["costUsd"].as_f64() {
                runs_reporting_cost += 1;
                cost = Some(cost.unwrap_or(0.0) + run_cost);
            }
        }

        worker["spend"] = serde_json::json!({
            "costUsd": cost,
            "inputTokens": input,
            "outputTokens": output,
            "runsTotal": runs_total,
            "runsReportingCost": runs_reporting_cost,
            "runsReportingTokens": runs_reporting_tokens,
        });
    }
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

/// A 303 redirect that also sets a cookie -- the one non-GET-shaped
/// response this dashboard produces, used to exchange a valid `?token=`
/// for the `crew_dashboard` cookie. 303 rather than 302 so the follower is
/// unambiguously a GET.
async fn write_redirect<W>(stream: &mut W, location: &str, set_cookie: &str) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let head = format!(
        "HTTP/1.1 303 See Other\r\n\
         location: {location}\r\n\
         set-cookie: {set_cookie}\r\n\
         content-length: 0\r\n\
         connection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
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
