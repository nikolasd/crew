//! The workspace-lease database must live under the same invariants as
//! the main journal (tests/database.rs style): WAL journal mode, foreign
//! keys ON, busy_timeout, synchronous=FULL, and versioned migrations --
//! plus one held connection instead of an open-per-call pattern.

use batman_protocol::{IsolationKind, LeaseMode, ProjectId, RunId};
use batman_runtime::workspace::LeaseService;

#[test]
fn lease_db_enables_wal_foreign_keys_busy_timeout_and_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("workspace-leases.db");
    let service = LeaseService::open(ProjectId::new(), &db_path).unwrap();

    let diagnostics = service.diagnostics().unwrap();
    assert_eq!(diagnostics.journal_mode.to_lowercase(), "wal");
    assert!(diagnostics.foreign_keys, "foreign_keys must be ON");
    assert_eq!(diagnostics.busy_timeout, 5000);
    assert_eq!(diagnostics.synchronous, 2, "synchronous must be FULL (2)");
    assert!(
        diagnostics.user_version >= 1,
        "the schema must be versioned by rusqlite_migration, got {}",
        diagnostics.user_version
    );
}

#[test]
fn a_pre_versioned_database_is_adopted_without_data_loss() {
    // Simulate a lease DB created by the old unversioned code: bare
    // CREATE TABLE, user_version 0, one live lease row.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("workspace-leases.db");
    let legacy_run_id = RunId::new();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspace_leases (
                lease_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, mode TEXT NOT NULL,
                isolation_kind TEXT NOT NULL DEFAULT 'shared', path TEXT NOT NULL,
                base_revision TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'active',
                acquired_at TEXT NOT NULL, acquisition_sequence INTEGER NOT NULL DEFAULT 0,
                released_at TEXT
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workspace_leases
                (lease_id, run_id, mode, isolation_kind, path, base_revision, state,
                 acquired_at, acquisition_sequence)
             VALUES ('legacy-lease', ?1, 'write', 'gitWorktree', '/tmp/w',
                     'HEAD', 'active', '2026-01-01T00:00:00Z', 1)",
            [legacy_run_id.to_string()],
        )
        .unwrap();
    }

    let service = LeaseService::open(ProjectId::new(), &db_path).unwrap();
    let diagnostics = service.diagnostics().unwrap();
    assert!(
        diagnostics.user_version >= 1,
        "adopted DB must be versioned"
    );
    let info = service
        .get("legacy-lease".to_string())
        .expect("the legacy row must survive adoption");
    assert_eq!(info.lease_id, "legacy-lease");
}

#[test]
fn lease_lifecycle_still_works_through_the_held_connection() {
    let service = LeaseService::open_in_memory(ProjectId::new()).unwrap();
    let run_id = RunId::new();

    let created = service
        .acquire(run_id, LeaseMode::Write, Some(IsolationKind::GitWorktree))
        .unwrap();
    service
        .activate(created.lease_id.clone(), "/tmp/materialized".to_string())
        .unwrap();
    let info = service
        .active_for_run(run_id)
        .unwrap()
        .expect("an activated lease is active for its run");
    assert_eq!(info.path, "/tmp/materialized");

    service.release(created.lease_id.clone()).unwrap();
    assert!(
        service.active_for_run(run_id).unwrap().is_none(),
        "a released lease is no longer active"
    );

    // Concurrent use through the same held connection: two isolated
    // leases from different callers may interleave freely.
    let a = service
        .acquire(RunId::new(), LeaseMode::Write, Some(IsolationKind::Copy))
        .unwrap();
    let b = service
        .acquire(
            RunId::new(),
            LeaseMode::Write,
            Some(IsolationKind::GitWorktree),
        )
        .unwrap();
    assert_ne!(a.lease_id, b.lease_id);
}
