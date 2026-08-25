//! Integration tests for the audit module: retention pruning and JSONL
//! export, both against a real `DatabaseHandle`.

use crew_protocol::{Classified, ContentClass, DiagnosticLevel, ProjectId, Timestamp};
use crew_runtime::audit::{Export, Retention};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::security::redaction::{RawEventKind, RawRuntimeEvent, Redactor};
use serde_json::Value;

/// A diagnostic event dated `timestamp`, carrying `text` as visible
/// content. `secret`, when supplied, is classified and must never survive
/// the redaction boundary.
fn event_at(timestamp: &str, text: &str, secret: Option<&str>) -> RawRuntimeEvent {
    let mut fragments = vec![Classified {
        class: ContentClass::Visible,
        value: text.to_string(),
    }];
    if let Some(secret) = secret {
        fragments.push(Classified {
            class: ContentClass::Secret,
            value: secret.to_string(),
        });
    }
    RawRuntimeEvent {
        timestamp: Timestamp::parse(timestamp).expect("fixture timestamp is RFC 3339"),
        project_id: ProjectId::new(),
        run_id: None,
        kind: RawEventKind::Diagnostic {
            level: DiagnosticLevel::Info,
            code: "fixture".to_string(),
            fragments,
        },
    }
}

/// A database seeded with `events`, plus the temp dir keeping it alive.
async fn seeded_db(events: Vec<RawRuntimeEvent>) -> (DatabaseHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = DatabaseHandle::start(dir.path().join("runtime.db"))
        .await
        .unwrap();
    let redactor = Redactor::new();
    for event in events {
        db.append_event(redactor.sanitize(event)).await.unwrap();
    }
    (db, dir)
}

fn exported_lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("export writes a file even when empty")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is a JSON object"))
        .collect()
}

#[tokio::test]
async fn retention_prunes_old_events() {
    // The prune protects events belonging to non-terminal runs; these
    // have no run at all, so age is the only thing deciding their fate.
    let old = Timestamp::now();
    let (db, _dir) = seeded_db(vec![
        event_at("2020-01-01T00:00:00Z", "ancient", None),
        event_at(old.as_str(), "recent", None),
    ])
    .await;

    Retention::new("30d", 20).prune(&db).await.unwrap();

    let surviving: Vec<String> = db
        .replay_events(0)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event_json)
        .collect();
    assert_eq!(surviving.len(), 1, "only the recent event survives");
    assert!(
        surviving[0].contains("recent"),
        "the wrong event was kept: {surviving:?}"
    );
}

#[tokio::test]
async fn export_creates_jsonl_file() {
    let (db, dir) = seeded_db(vec![
        event_at(
            "2024-01-01T00:00:00Z",
            "first",
            Some("sk-live-abcdef123456"),
        ),
        event_at("2024-06-01T00:00:00Z", "second", None),
    ])
    .await;
    let output = dir.path().join("audit.jsonl");

    let count = Export::new(
        "repo",
        dir.path().to_string_lossy(),
        &*output.to_string_lossy(),
    )
    .export(&db)
    .await
    .unwrap();

    assert_eq!(count, 2);
    let lines = exported_lines(&output);
    assert_eq!(lines.len(), 2);
    // Sequence order, and `event` is parsed rather than a nested string.
    assert_eq!(lines[0]["sequence"], 1);
    assert_eq!(lines[1]["sequence"], 2);
    assert!(lines[0]["event"].is_object(), "event must be parsed JSON");

    // The journal is already sanitized, so export does not re-redact --
    // this asserts the secret never reached the journal in the first
    // place, which is what makes re-redaction unnecessary.
    let raw = std::fs::read_to_string(&output).unwrap();
    assert!(
        !raw.contains("sk-live-abcdef123456"),
        "a classified fragment must never appear in an export"
    );
}

#[tokio::test]
async fn export_handles_empty_range() {
    let (db, dir) = seeded_db(vec![event_at("2024-01-01T00:00:00Z", "only", None)]).await;
    let output = dir.path().join("empty.jsonl");

    let mut export = Export::new(
        "repo",
        dir.path().to_string_lossy(),
        &*output.to_string_lossy(),
    );
    export.from = Some("2030-01-01T00:00:00Z".to_string());
    export.to = Some("2030-12-31T00:00:00Z".to_string());

    let count = export.export(&db).await.unwrap();

    assert_eq!(count, 0);
    // An empty file, not a missing one: a consumer must be able to tell
    // "nothing in range" from "the export never ran".
    assert_eq!(
        std::fs::read_to_string(&output).unwrap(),
        "",
        "an empty range writes an empty file"
    );
}

#[tokio::test]
async fn export_filters_by_timestamp() {
    let (db, dir) = seeded_db(vec![
        event_at("2024-01-01T00:00:00Z", "before", None),
        event_at("2024-06-15T00:00:00Z", "inside", None),
        event_at("2024-12-31T00:00:00Z", "after", None),
    ])
    .await;
    let output = dir.path().join("range.jsonl");

    let mut export = Export::new(
        "repo",
        dir.path().to_string_lossy(),
        &*output.to_string_lossy(),
    );
    export.from = Some("2024-06-01T00:00:00Z".to_string());
    export.to = Some("2024-07-01T00:00:00Z".to_string());

    let count = export.export(&db).await.unwrap();

    assert_eq!(count, 1);
    let lines = exported_lines(&output);
    assert!(
        serde_json::to_string(&lines[0]).unwrap().contains("inside"),
        "only the in-range event is exported: {lines:?}"
    );
}

#[tokio::test]
async fn retention_prunes_terminal_events_beyond_max_runs_but_keeps_recent_history() {
    let (db, _dir) = seeded_db(Vec::new()).await;
    db.run_domain_op(Box::new(|conn| {
        conn.execute(
            "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope) VALUES ('p', 'fp', 'claude', 'test', '{}')",
            [],
        )?;
        conn.execute(
            "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at) VALUES ('t', '018f0000-0000-7000-8000-0000000000aa', 'owner', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )?;
        conn.execute(
            "INSERT INTO workers (worker_id, project_id, profile_id, created_at) VALUES ('w', '018f0000-0000-7000-8000-0000000000aa', 'p', '2026-01-01T00:00:00Z')",
            [],
        )?;
        for run_id in [
            "018f0000-0000-7000-8000-000000000001",
            "018f0000-0000-7000-8000-000000000002",
            "018f0000-0000-7000-8000-000000000003",
        ] {
            conn.execute(
                "INSERT INTO runs (run_id, task_id, worker_id, state, created_at) VALUES (?1, 't', 'w', 'succeeded', '2026-01-01T00:00:00Z')",
                [run_id],
            )?;
            conn.execute(
                "INSERT INTO events (timestamp, project_id, run_id, event_json) VALUES ('2026-01-01T00:00:00Z', '018f0000-0000-7000-8000-0000000000aa', ?1, '{}')",
                [run_id],
            )?;
        }
        Ok(Value::Null)
    }))
    .await
    .unwrap();

    // A century cutoff keeps every event by age; only maxRuns may prune.
    let report = Retention::new("100y", 1).prune(&db).await.unwrap();
    assert_eq!(report.deleted_events, 2);
    assert_eq!(report.runs_pruned, 2);
    let survivors = db.replay_events(0).await.unwrap();
    assert_eq!(survivors.len(), 1);
    assert_eq!(
        survivors[0]
            .run_id
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("018f0000-0000-7000-8000-000000000003")
    );
}
