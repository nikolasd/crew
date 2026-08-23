//! Workspace lease arbitration tests.

use crew_protocol::{IsolationKind, LeaseMode, ProjectId, RunId, WorkspaceState};
use crew_runtime::workspace::{ALLOCATING_LEASE_GRACE, LeaseError, LeaseService};

fn test_project_id() -> ProjectId {
    ProjectId::parse("01900000-0000-0000-0000-000000000001").unwrap()
}

fn test_run_id(n: u32) -> RunId {
    RunId::parse(&format!("01900000-0000-0000-0000-00000000000{0}", n)).unwrap()
}

#[test]
fn multiple_shared_readonly_leases_succeed() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let run2 = test_run_id(2);

    let lease1 = service
        .acquire(run1, LeaseMode::ReadOnly, None)
        .expect("first read-only lease");
    assert_eq!(lease1.mode, LeaseMode::ReadOnly);
    assert_eq!(lease1.state, WorkspaceState::Allocating);
    service
        .activate(lease1.lease_id.clone(), "/tmp/ws-1".to_string())
        .unwrap();

    let lease2 = service
        .acquire(run2, LeaseMode::ReadOnly, None)
        .expect("second read-only lease");
    assert_eq!(lease2.mode, LeaseMode::ReadOnly);
    service
        .activate(lease2.lease_id.clone(), "/tmp/ws-2".to_string())
        .unwrap();

    let info1 = service.get(lease1.lease_id.clone()).unwrap();
    assert_eq!(info1.run_id, run1);
    assert_eq!(info1.state, WorkspaceState::Active);
    let info2 = service.get(lease2.lease_id.clone()).unwrap();
    assert_eq!(info2.run_id, run2);
    assert_eq!(info2.state, WorkspaceState::Active);

    service.release(lease1.lease_id).unwrap();
    service.release(lease2.lease_id).unwrap();
}

#[test]
fn write_lease_excludes_all_others() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let run2 = test_run_id(2);

    let lease1 = service
        .acquire(run1, LeaseMode::Write, None)
        .expect("first write lease");
    assert_eq!(lease1.mode, LeaseMode::Write);
    service
        .activate(lease1.lease_id.clone(), "/tmp/ws-1".to_string())
        .unwrap();

    let result = service.acquire(run2, LeaseMode::Write, None);
    assert!(
        result.is_err(),
        "second write lease for same project must fail"
    );

    service.release(lease1.lease_id).unwrap();
    let lease2 = service
        .acquire(run2, LeaseMode::Write, None)
        .expect("write lease after first released");
    assert_eq!(lease2.mode, LeaseMode::Write);
    service
        .activate(lease2.lease_id.clone(), "/tmp/ws-2".to_string())
        .unwrap();
}

#[test]
fn write_lease_blocks_readonly() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let run2 = test_run_id(2);

    let lease1 = service
        .acquire(run1, LeaseMode::Write, None)
        .expect("write lease");
    service
        .activate(lease1.lease_id.clone(), "/tmp/ws-1".to_string())
        .unwrap();

    let result = service.acquire(run2, LeaseMode::ReadOnly, None);
    assert!(
        result.is_err(),
        "read-only lease must fail when write lease exists"
    );

    service.release(lease1.lease_id).unwrap();
}

#[test]
fn readonly_lease_blocks_write() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let run2 = test_run_id(2);

    let lease1 = service.acquire(run1, LeaseMode::ReadOnly, None).unwrap();
    service
        .activate(lease1.lease_id.clone(), "/tmp/ws-1".to_string())
        .unwrap();

    let result = service.acquire(run2, LeaseMode::Write, None);
    assert!(
        result.is_err(),
        "write lease must fail when read-only lease exists"
    );
}

#[test]
fn released_lease_cannot_be_reused() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let run2 = test_run_id(2);

    let lease1 = service
        .acquire(run1, LeaseMode::Write, None)
        .expect("write lease");
    let lease1_id = lease1.lease_id.clone();

    service
        .activate(lease1_id.clone(), "/tmp/ws-1".to_string())
        .unwrap();
    service.release(lease1_id.clone()).unwrap();

    let result = service.acquire(run2, LeaseMode::Write, None);
    assert!(result.is_ok(), "new acquire should succeed");
    let new_lease = result.unwrap();
    assert_ne!(new_lease.lease_id, lease1_id);
}

#[test]
fn active_for_repository_returns_active_count() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let run2 = test_run_id(2);

    assert_eq!(service.active_for_repository().unwrap(), 0);

    let lease1 = service.acquire(run1, LeaseMode::ReadOnly, None).unwrap();
    // allocating state counts toward active_for_repository
    assert_eq!(service.active_for_repository().unwrap(), 1);

    service
        .activate(lease1.lease_id.clone(), "/tmp/ws-1".to_string())
        .unwrap();
    // still 1 after activating (state changed but still in allocating|active set)
    assert_eq!(service.active_for_repository().unwrap(), 1);

    let lease2 = service.acquire(run2, LeaseMode::ReadOnly, None).unwrap();
    // second allocating lease bumps count
    assert_eq!(service.active_for_repository().unwrap(), 2);

    service
        .activate(lease2.lease_id.clone(), "/tmp/ws-2".to_string())
        .unwrap();
    // still 2 after activating
    assert_eq!(service.active_for_repository().unwrap(), 2);

    service.release(lease1.lease_id).unwrap();
    assert_eq!(service.active_for_repository().unwrap(), 1);
}

#[test]
fn activate_transitions_to_active() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);

    let lease = service
        .acquire(run1, LeaseMode::Write, None)
        .expect("write lease");
    assert_eq!(lease.state, WorkspaceState::Allocating);
    assert_eq!(lease.path, "");

    service
        .activate(lease.lease_id.clone(), "/tmp/real-workspace".to_string())
        .unwrap();

    let info = service.get(lease.lease_id).unwrap();
    assert_eq!(info.state, WorkspaceState::Active);
    assert_eq!(info.path, "/tmp/real-workspace");
}

#[test]
fn isolated_workspaces_dont_conflict() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let run2 = test_run_id(2);

    let lease1 = service
        .acquire(run1, LeaseMode::Write, Some(IsolationKind::GitWorktree))
        .expect("first git worktree lease");
    assert_eq!(lease1.isolation_kind, IsolationKind::GitWorktree);
    service
        .activate(lease1.lease_id.clone(), "/tmp/ws-1".to_string())
        .unwrap();

    let lease2 = service
        .acquire(run2, LeaseMode::Write, Some(IsolationKind::GitWorktree))
        .expect("second git worktree lease should not conflict");
    assert_eq!(lease2.isolation_kind, IsolationKind::GitWorktree);
    service
        .activate(lease2.lease_id.clone(), "/tmp/ws-2".to_string())
        .unwrap();

    let info1 = service.get(lease1.lease_id.clone()).unwrap();
    assert_eq!(info1.run_id, run1);
    assert_eq!(info1.path, "/tmp/ws-1");

    let info2 = service.get(lease2.lease_id.clone()).unwrap();
    assert_eq!(info2.run_id, run2);
    assert_eq!(info2.path, "/tmp/ws-2");

    service.release(lease1.lease_id).unwrap();
    service.release(lease2.lease_id).unwrap();
}

#[test]
fn stale_never_flags_an_allocating_lease_within_the_grace_period() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run = test_run_id(1);
    service
        .acquire(run, LeaseMode::Write, Some(IsolationKind::GitWorktree))
        .expect("acquire");

    let stale = service.stale().unwrap();
    assert!(
        stale.is_empty(),
        "a lease acquired moments ago must not be reported as stale: {stale:?}"
    );
}

#[test]
fn stale_flags_an_allocating_lease_that_outlived_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("leases.db");
    let service = LeaseService::open(test_project_id(), &db_path).unwrap();
    let run = test_run_id(1);
    let lease = service
        .acquire(run, LeaseMode::Write, Some(IsolationKind::GitWorktree))
        .expect("acquire");

    // Simulate a caller that crashed between `acquire` and `activate`:
    // back-date `acquired_at` directly. `LeaseService` has no API for this
    // on purpose -- it is exactly what happens to a row nothing else will
    // ever touch again.
    let old =
        (time::OffsetDateTime::now_utc() - ALLOCATING_LEASE_GRACE - time::Duration::minutes(1))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE workspace_leases SET acquired_at = ?1 WHERE lease_id = ?2",
        rusqlite::params![old, lease.lease_id],
    )
    .unwrap();
    drop(conn);

    let stale = service.stale().unwrap();
    assert_eq!(
        stale,
        vec![(lease.lease_id.clone(), "allocating".to_string())],
        "an allocating lease past the grace period must surface even with an empty path"
    );
}

#[test]
fn an_unreleased_cleanup_failed_lease_still_blocks_a_new_shared_writer() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let lease1 = service
        .acquire(run1, LeaseMode::Write, Some(IsolationKind::Shared))
        .expect("first shared write lease");

    // Simulate `release()` itself failing (db error, crash): the row is
    // marked `cleanupFailed` directly without ever going through
    // `release()`, so `released_at` stays NULL -- exactly what a caller
    // observes when the release attempt never succeeded.
    service
        .mark_cleanup_failed(lease1.lease_id.clone())
        .unwrap();

    let run2 = test_run_id(2);
    let result = service.acquire(run2, LeaseMode::Write, Some(IsolationKind::Shared));
    assert!(
        matches!(result, Err(LeaseError::IsolationRequired)),
        "a shared writer whose release() itself failed must still block a new shared writer: {result:?}"
    );
}

#[test]
fn a_released_lease_with_a_failed_teardown_does_not_block_a_new_shared_writer() {
    let service = LeaseService::open_in_memory(test_project_id()).unwrap();
    let run1 = test_run_id(1);
    let lease1 = service
        .acquire(run1, LeaseMode::Write, Some(IsolationKind::Shared))
        .expect("first shared write lease");
    service
        .release(lease1.lease_id.clone())
        .expect("release succeeds");

    // Teardown failed *after* a successful release (e.g. worktree removal
    // failed): `released_at` is already set, so this must not block a new
    // shared writer.
    service
        .mark_cleanup_failed(lease1.lease_id.clone())
        .unwrap();

    let run2 = test_run_id(2);
    let result = service.acquire(run2, LeaseMode::Write, Some(IsolationKind::Shared));
    assert!(
        result.is_ok(),
        "a released lease marked cleanupFailed only for a disk-teardown issue must not block: {result:?}"
    );
}
