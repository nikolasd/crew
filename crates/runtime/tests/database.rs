//! Integration tests for the SQLite database actor: migration/PRAGMA setup,
//! append + replay across a reopen, and operation-intent recovery across a
//! reopen.

use crew_protocol::{Classified, ContentClass, DiagnosticLevel, OperationId, ProjectId, Timestamp};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::security::redaction::{RawEventKind, RawRuntimeEvent, Redactor};

fn diagnostic_fixture(text: &str) -> RawRuntimeEvent {
    RawRuntimeEvent {
        timestamp: Timestamp::now(),
        project_id: ProjectId::new(),
        run_id: None,
        kind: RawEventKind::Diagnostic {
            level: DiagnosticLevel::Info,
            code: "fixture".to_string(),
            fragments: vec![Classified {
                class: ContentClass::Visible,
                value: text.to_string(),
            }],
        },
    }
}

#[tokio::test]
async fn fresh_database_enables_wal_foreign_keys_and_creates_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("runtime.db");

    let handle = DatabaseHandle::start(db_path.clone()).await.unwrap();
    let diagnostics = handle.diagnostics().await.unwrap();

    assert_eq!(diagnostics.journal_mode.to_lowercase(), "wal");
    assert!(diagnostics.foreign_keys, "foreign_keys must be ON");
    assert_eq!(diagnostics.busy_timeout, 5000);
    assert_eq!(diagnostics.synchronous, 2, "synchronous must be FULL (2)");
    assert!(diagnostics.tables.contains(&"events".to_string()));
    assert!(diagnostics.tables.contains(&"operations".to_string()));

    handle.shutdown().await.unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "database file must be created private (0600)");
    }
}

#[tokio::test]
async fn append_two_events_reopen_and_replay_returns_only_events_after_given_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("runtime.db");
    let redactor = Redactor::new();

    let handle = DatabaseHandle::start(db_path.clone()).await.unwrap();

    let first = redactor.sanitize(diagnostic_fixture("first event"));
    let second = redactor.sanitize(diagnostic_fixture("second event"));

    let seq1 = handle.append_event(first).await.unwrap();
    let seq2 = handle.append_event(second).await.unwrap();
    assert_eq!(seq1, 1);
    assert_eq!(seq2, 2);

    handle.shutdown().await.unwrap();

    let reopened = DatabaseHandle::start(db_path.clone()).await.unwrap();
    let replayed = reopened.replay_events(1).await.unwrap();

    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].sequence, 2);
    assert!(replayed[0].event_json.contains("second event"));

    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn max_sequence_reports_the_tip_without_replaying_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("runtime.db");
    let redactor = Redactor::new();

    let handle = DatabaseHandle::start(db_path.clone()).await.unwrap();

    // An empty journal has no tip.
    assert_eq!(handle.max_sequence().await.unwrap(), None);

    handle
        .append_event(redactor.sanitize(diagnostic_fixture("first event")))
        .await
        .unwrap();
    handle
        .append_event(redactor.sanitize(diagnostic_fixture("second event")))
        .await
        .unwrap();

    // Two events appended: the tip is sequence 2.
    assert_eq!(handle.max_sequence().await.unwrap(), Some(2));

    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn operation_intent_without_acknowledgement_survives_reopen_as_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("runtime.db");

    let handle = DatabaseHandle::start(db_path.clone()).await.unwrap();

    let redactor = Redactor::new();
    let operation_id = OperationId::new();
    handle
        .record_operation_intent(
            operation_id,
            "spawn_worker",
            redactor.sanitize_json(&serde_json::json!({"worker": "example"})),
            Timestamp::now(),
        )
        .await
        .unwrap();

    handle.shutdown().await.unwrap();

    let reopened = DatabaseHandle::start(db_path.clone()).await.unwrap();
    let incomplete = reopened.incomplete_operations().await.unwrap();

    assert_eq!(incomplete.len(), 1);
    assert_eq!(incomplete[0].operation_id, operation_id);
    assert_eq!(incomplete[0].kind, "spawn_worker");

    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn acknowledged_operation_is_excluded_from_incomplete_operations() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("runtime.db");

    let handle = DatabaseHandle::start(db_path.clone()).await.unwrap();

    let redactor = Redactor::new();
    let operation_id = OperationId::new();
    handle
        .record_operation_intent(
            operation_id,
            "spawn_worker",
            redactor.sanitize_json(&serde_json::json!({"worker": "example"})),
            Timestamp::now(),
        )
        .await
        .unwrap();

    handle
        .acknowledge_operation(
            operation_id,
            redactor.sanitize_json(&serde_json::json!({"status": "ok"})),
        )
        .await
        .unwrap();

    let incomplete = handle.incomplete_operations().await.unwrap();
    assert!(incomplete.is_empty());

    handle.shutdown().await.unwrap();
}

/// R66: shutting down after the actor thread died abnormally must still
/// join (reap) the thread and report the actor unavailable -- the old
/// `rx.await.map_err(...)?` short-circuited past the join, leaking the
/// JoinHandle and losing the panic.
#[tokio::test]
async fn shutdown_after_an_actor_panic_still_joins_and_reports_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("runtime.db");
    let handle = DatabaseHandle::start(db_path).await.unwrap();

    // Kill the actor with a panicking domain op.
    let result = handle
        .run_domain_op(Box::new(|_conn| panic!("deliberate test panic")))
        .await;
    assert!(result.is_err(), "a panicking op must not report success");

    // The old code short-circuited out of shutdown() here without joining.
    // The new code must return the error AND have reaped the thread --
    // observable as a clean, non-hanging return.
    let shutdown = tokio::time::timeout(std::time::Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown must not hang while joining a dead actor");
    assert!(
        shutdown.is_err(),
        "shutdown after an actor panic must report the actor unavailable"
    );
}
