//! Integration tests for the daemon-hosted dashboard: a hand-rolled,
//! read-only, localhost-only HTTP listener serving the status page,
//! `/api/state` (through the same DatabaseHandle read ops the RPC layer
//! uses), and `/events` (SSE fed from the live event broadcast). Drives a
//! real TCP client against a real listener bound to an ephemeral port.

use std::sync::Arc;

use crew_protocol::{
    EventEnvelope, EventSource, ProjectId, Run, RunFlags, RunState, RuntimeEvent, TaskId, TaskRef,
    Timestamp, WorkerId, WorkerProfileRef,
};
use crew_runtime::dashboard::{DashboardDeps, DashboardServer};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::DomainRepository;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

/// Seeds one task + one worker + one run through the real
/// `DomainRepository`, mirroring `tests/recovery.rs`'s helper.
async fn seed_run(db: &DatabaseHandle, project_id: ProjectId) -> crew_protocol::RunId {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = crew_protocol::RunId::new();

    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.upsert_task(
            task_id,
            &TaskRef {
                owner_client_instance_id: "omp-1".into(),
                revision: 1,
            },
        )?;
        let worker = crew_protocol::Worker {
            worker_id,
            profile_ref: WorkerProfileRef {
                id: worker_id,
                fingerprint: "sha256:fake".into(),
                adapter: "fake".into(),
                model: "test".into(),
                permission_envelope: serde_json::json!({}),
            },
            parent_worker_id: None,
            created_at: Timestamp::now(),
        };
        repo.create_worker(&worker)?;
        let run = Run {
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
        // WP19/WP20 projections the dashboard snapshot must now surface.
        repo.attach_turn_budget(run_id, task_id, None, 10)?;
        repo.record_escalation_raised(
            run_id,
            "question",
            Some("why did the run stall?".to_string()),
        )?;
        Ok(serde_json::json!({}))
    }))
    .await
    .expect("seed run");
    run_id
}

struct Harness {
    _dir: TempDir,
    server: DashboardServer,
    events_tx: broadcast::Sender<EventEnvelope>,
    project_id: ProjectId,
    db: Arc<DatabaseHandle>,
}

async fn start_dashboard() -> Harness {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(
        DatabaseHandle::start(dir.path().join("journal.db"))
            .await
            .unwrap(),
    );
    let project_id = ProjectId::new();
    let (events_tx, _) = broadcast::channel(64);
    let server = DashboardServer::bind(
        0,
        DashboardDeps {
            db: Arc::clone(&db),
            project_id,
            events_tx: events_tx.clone(),
        },
    )
    .await
    .expect("dashboard binds an ephemeral localhost port");
    Harness {
        _dir: dir,
        server,
        events_tx,
        project_id,
        db,
    }
}

/// A `Cookie` header carrying the dashboard's per-run token -- the form
/// every request takes after the page's first load exchanges `?token=`
/// for it. Tests use the cookie rather than the query so they exercise the
/// same path the page does for all but one request.
fn authed(token: &str) -> String {
    format!("cookie: crew_dashboard={token}\r\n")
}

/// One plain HTTP/1.1 request; returns the full raw response.
async fn http_request(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stream.read_to_string(&mut response),
    )
    .await
    .expect("response must arrive promptly")
    .unwrap();
    response
}

fn body_of(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

#[test]
fn dashboard_is_disabled_by_default() {
    let config = crew_runtime::config::crew::CrewConfig::default();
    assert!(
        !config.dashboard.enabled,
        "the dashboard must be strictly opt-in"
    );
    assert_eq!(config.dashboard.port, 4747);
}

#[tokio::test]
async fn binds_localhost_only() {
    let harness = start_dashboard().await;
    let addr = harness.server.local_addr();
    assert!(
        addr.ip().is_loopback(),
        "the dashboard must never bind a routable interface: {addr}"
    );
    harness.server.stop();
}

#[tokio::test]
async fn api_state_returns_seeded_runs_and_workers() {
    let harness = start_dashboard().await;
    let run_id = seed_run(&harness.db, harness.project_id).await;

    let response = http_request(
        harness.server.local_addr(),
        &format!(
            "GET /api/state HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
            authed(harness.server.token())
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    let state: serde_json::Value = serde_json::from_str(body_of(&response)).unwrap();
    let runs = state["runs"].as_array().expect("runs array");
    assert!(
        runs.iter()
            .any(|r| r["runId"].as_str() == Some(&run_id.to_string())),
        "the seeded run must be visible: {state}"
    );
    assert_eq!(
        state["workers"].as_array().map(Vec::len),
        Some(1),
        "the seeded worker must be visible: {state}"
    );
    // WP19/WP20 projections: the seeded run's budget and its open
    // question escalation must surface, not just be present as arrays.
    let budgets = state["budgets"].as_array().expect("budgets array");
    assert!(
        budgets.iter().any(|b| {
            b["runId"].as_str() == Some(&run_id.to_string())
                && b["turnsUsed"].as_i64() == Some(0)
                && b["turnLimit"].as_i64() == Some(10)
        }),
        "the seeded run's turn budget must be visible: {state}"
    );
    let escalations = state["pendingEscalations"]
        .as_array()
        .expect("pendingEscalations array");
    assert!(
        escalations.iter().any(|e| {
            e["runId"].as_str() == Some(&run_id.to_string())
                && e["kind"].as_str() == Some("question")
                && e["question"].as_str() == Some("why did the run stall?")
        }),
        "the seeded open escalation must be visible: {state}"
    );

    harness.server.stop();
}

#[tokio::test]
async fn index_serves_the_html_page() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        &format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
            authed(harness.server.token())
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        response.contains("content-type: text/html"),
        "the index must be html: {}",
        response.lines().take(8).collect::<Vec<_>>().join(" | ")
    );
    assert!(body_of(&response).contains("<html"));
    harness.server.stop();
}

#[tokio::test]
async fn non_get_methods_are_rejected_with_405() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        "POST /api/state HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 405"),
        "the dashboard is read-only; non-GET must be 405: {response}"
    );
    harness.server.stop();
}

/// A request line with no newline, at or past the size cap, must be
/// refused rather than buffered without bound -- and the connection that
/// sent it must not affect the server's ability to serve a normal request
/// afterward.
#[tokio::test]
async fn an_oversized_line_with_no_newline_is_refused_and_the_server_stays_healthy() {
    let harness = start_dashboard().await;
    let addr = harness.server.local_addr();

    {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        // One byte past the cap, never terminated: a peer that either
        // never intends to send `\n` or is trying to grow the server's
        // buffer without bound.
        let oversized = vec![b'a'; 8 * 1024 + 1];
        stream.write_all(&oversized).await.unwrap();

        let mut response = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_string(&mut response),
        )
        .await
        .expect("the server must respond promptly, not hang reading forever")
        .unwrap();
        assert!(
            response.starts_with("HTTP/1.1 431"),
            "an oversized, unterminated line must be refused with 431: {response}"
        );
    }

    // The server must still serve a normal request on a fresh connection.
    let response = http_request(
        addr,
        &format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
            authed(harness.server.token())
        ),
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "the server must stay healthy after an oversized request: {response}"
    );

    harness.server.stop();
}

/// A connection that sends nothing at all must be dropped once the
/// header-read timeout elapses, not held open indefinitely.
#[tokio::test]
async fn a_silent_connection_is_disconnected_after_the_header_timeout() {
    let dir = TempDir::new().unwrap();
    let db = Arc::new(
        DatabaseHandle::start(dir.path().join("journal.db"))
            .await
            .unwrap(),
    );
    let project_id = ProjectId::new();
    let (events_tx, _) = broadcast::channel(64);
    let server = crew_runtime::dashboard::DashboardServer::bind_with_header_timeout(
        0,
        DashboardDeps {
            db,
            project_id,
            events_tx,
        },
        std::time::Duration::from_millis(200),
    )
    .await
    .expect("dashboard binds an ephemeral localhost port");
    let addr = server.local_addr();

    let mut stream = TcpStream::connect(addr).await.unwrap();
    // Send nothing. The server must close the connection once its
    // (short, test-configured) header-read timeout elapses.
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read_to_end(&mut response),
    )
    .await
    .expect("the silent connection must be closed within the timeout, not held open")
    .unwrap();
    assert!(
        response.is_empty(),
        "a timed-out connection gets no response, just a close: {response:?}"
    );

    server.stop();
}

#[tokio::test]
async fn unknown_paths_are_404() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        &format!(
            "GET /api/does-not-exist HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
            authed(harness.server.token())
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    harness.server.stop();
}

#[tokio::test]
async fn sse_stream_receives_a_broadcast_envelope() {
    let harness = start_dashboard().await;

    let mut stream = TcpStream::connect(harness.server.local_addr())
        .await
        .unwrap();
    stream
        .write_all(
            format!(
                "GET /events HTTP/1.1\r\nHost: localhost\r\n{}\r\n",
                authed(harness.server.token())
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut reader = BufReader::new(stream);

    // Read the response head; it must declare an event stream.
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    loop {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        if line == "\r\n" {
            break;
        }
        if line.to_ascii_lowercase().starts_with("content-type:") {
            assert!(line.contains("text/event-stream"), "{line}");
        }
    }

    // A committed mutation broadcasts its envelope; the SSE viewer must
    // receive exactly that JSON as a data frame.
    let run_id = crew_protocol::RunId::new();
    let envelope = EventEnvelope {
        sequence: 42,
        timestamp: Timestamp::now(),
        project_id: harness.project_id,
        task_id: None,
        worker_id: None,
        run_id: Some(run_id),
        parent_worker_id: None,
        source: EventSource::Runtime,
        event: RuntimeEvent::RunEvent {
            kind: crew_protocol::RuntimeEventKind::RunWorking,
            run_id,
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            state: "working".to_string(),
        },
        vendor_event_ref: None,
    };
    harness
        .events_tx
        .send(envelope)
        .expect("sse subscriber listening");

    let data_line = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if line.starts_with("data:") {
                return line;
            }
        }
    })
    .await
    .expect("the broadcast envelope must arrive over SSE");

    let payload: serde_json::Value =
        serde_json::from_str(data_line.trim_start_matches("data:").trim()).unwrap();
    assert_eq!(payload["sequence"], 42);
    assert_eq!(payload["runId"].as_str(), Some(run_id.to_string().as_str()));

    harness.server.stop();
}

// ----------------------------------------- access control (CREW-12)

/// The property that matters: loopback is not access control. A TCP
/// listener cannot check peer credentials the way the IPC socket does, so
/// without a token any local process -- as any local user -- could read the
/// whole projection.
#[tokio::test]
async fn every_route_refuses_a_request_with_no_token() {
    let harness = start_dashboard().await;
    let addr = harness.server.local_addr();

    for path in ["/", "/api/state", "/events", "/api/run/whatever/events"] {
        let response = http_request(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(
            response.starts_with("HTTP/1.1 401"),
            "{path} must refuse an untokenized request: {response}"
        );
    }

    harness.server.stop();
}

#[tokio::test]
async fn a_wrong_token_is_refused_like_no_token() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        "GET /api/state HTTP/1.1\r\nHost: localhost\r\ncookie: crew_dashboard=not-the-token\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "a wrong token must not be accepted: {response}"
    );
    harness.server.stop();
}

/// An unauthenticated request must not even learn which paths exist -- the
/// auth gate runs before routing, so an unknown path answers 401, not 404.
#[tokio::test]
async fn an_unauthenticated_unknown_path_does_not_reveal_itself_as_unknown() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        "GET /api/secret-thing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 401"),
        "routing must happen after auth, so this is 401 rather than 404: {response}"
    );
    harness.server.stop();
}

/// The pasted URL carries `?token=`; the page must not keep serving it from
/// the address bar. A valid query token is exchanged for a cookie and
/// redirected, so the secret leaves the URL after first load.
#[tokio::test]
async fn a_query_token_on_the_page_route_is_exchanged_for_a_cookie() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        &format!(
            "GET /?token={} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            harness.server.token()
        ),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 303"),
        "a valid query token must redirect rather than render: {response}"
    );
    let lowered = response.to_ascii_lowercase();
    assert!(
        lowered.contains("location: /"),
        "must redirect to the bare page: {response}"
    );
    assert!(
        lowered.contains("set-cookie: crew_dashboard="),
        "must hand back the cookie: {response}"
    );
    assert!(
        lowered.contains("httponly"),
        "the cookie must be HttpOnly: {response}"
    );
    assert!(
        lowered.contains("samesite=strict"),
        "the cookie must be SameSite=Strict: {response}"
    );

    harness.server.stop();
}

/// The transcript reads the journal through domain ops -- never the vendor's
/// own transcript file, which has not crossed the redaction boundary.
#[tokio::test]
async fn the_transcript_route_returns_that_runs_journaled_events() {
    let harness = start_dashboard().await;
    let run_id = seed_run(&harness.db, harness.project_id).await;

    let response = http_request(
        harness.server.local_addr(),
        &format!(
            "GET /api/run/{run_id}/events HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
            authed(harness.server.token())
        ),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "a seeded run's transcript must be readable: {response}"
    );
    let body = response.split("\r\n\r\n").nth(1).unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(body).expect("transcript body is JSON");
    assert_eq!(parsed["runId"], run_id.to_string());
    let events = parsed["events"].as_array().expect("events array");
    assert!(
        !events.is_empty(),
        "seeding a run journals events, so its transcript must not be empty"
    );
    assert!(
        events[0]["sequence"].is_number() && events[0]["event"].is_object(),
        "each entry carries its sequence and the parsed event: {:?}",
        events[0]
    );

    harness.server.stop();
}

/// A malformed id is the caller's mistake; saying so beats a 500 that reads
/// as a daemon fault.
#[tokio::test]
async fn a_malformed_run_id_on_the_transcript_route_is_a_400() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        &format!(
            "GET /api/run/not-a-uuid/events HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
            authed(harness.server.token())
        ),
    )
    .await;
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "a malformed run id must be a client error: {response}"
    );
    harness.server.stop();
}
