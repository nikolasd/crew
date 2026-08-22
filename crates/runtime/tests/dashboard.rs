//! Integration tests for the daemon-hosted dashboard: a hand-rolled,
//! read-only, localhost-only HTTP listener serving the status page,
//! `/api/state` (through the same DatabaseHandle read ops the RPC layer
//! uses), and `/events` (SSE fed from the live event broadcast). Drives a
//! real TCP client against a real listener bound to an ephemeral port.

use std::sync::Arc;

use batman_protocol::{
    EventEnvelope, EventSource, ProjectId, Run, RunFlags, RunState, RuntimeEvent, TaskId, TaskRef,
    Timestamp, WorkerId, WorkerProfileRef,
};
use batman_runtime::dashboard::{DashboardDeps, DashboardServer};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::domain::DomainRepository;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

/// Seeds one task + one worker + one run through the real
/// `DomainRepository`, mirroring `tests/recovery.rs`'s helper.
async fn seed_run(db: &DatabaseHandle, project_id: ProjectId) -> batman_protocol::RunId {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = batman_protocol::RunId::new();

    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.upsert_task(
            task_id,
            &TaskRef {
                owner_client_instance_id: "omp-1".into(),
                revision: 1,
            },
        )?;
        let worker = batman_protocol::Worker {
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
    let config = batman_runtime::config::crew::CrewConfig::default();
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
        "GET /api/state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
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
    // Budgets and escalations land with WP19/WP20; the shape is already
    // stable so the page never has to change.
    assert!(state["budgets"].is_array());
    assert!(state["pendingEscalations"].is_array());

    harness.server.stop();
}

#[tokio::test]
async fn index_serves_the_html_page() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
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

#[tokio::test]
async fn unknown_paths_are_404() {
    let harness = start_dashboard().await;
    let response = http_request(
        harness.server.local_addr(),
        "GET /api/does-not-exist HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
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
        .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
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
    let run_id = batman_protocol::RunId::new();
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
            kind: batman_protocol::RuntimeEventKind::RunWorking,
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
