//! Regression tests for R74: `task/upsert` and `reconcile/omp` used to
//! split their revision check from their write into two separate
//! `run_domain_op` round trips -- a caller-side pre-check that read the
//! stored revision, then a write whose statement carried no revision
//! predicate of its own:
//!
//! - `OrchestrationService::task_upsert` read `tasks.revision` and
//!   rejected a lower revision in memory, then called
//!   `DomainRepository::upsert_task`, whose
//!   `INSERT ... ON CONFLICT(task_id) DO UPDATE` unconditionally
//!   overwrote `owner_client_instance_id`/`revision`.
//! - `OrchestrationService::reconcile_omp` read `tasks.revision` and
//!   rejected a mismatched revision in memory, then called
//!   `DomainRepository::reconcile_ownership`, whose `UPDATE` carried no
//!   `AND revision = ?` predicate either.
//!
//! `DatabaseHandle::run_domain_op` sends whole boxed closures to a
//! single-owner actor thread over a FIFO channel (see
//! `approval_decide_race.rs`'s header for the full actor-FIFO argument):
//! the actor never interleaves the *inside* of two closures, only whole
//! closures with each other, in enqueue order. Because each pre-check and
//! its write were two separate closures, two concurrent callers could
//! both enqueue their pre-check read before either enqueued its write, so
//! both reads observed the same stale stored revision and both pre-checks
//! passed -- and then both writes landed, unconditionally. A lower
//! revision landing after a higher one moved the stored revision
//! backwards and silently rebound the owner to a stale client, and a
//! reconcile whose revision was stale at write time rebound (and moved
//! the revision back) regardless. The first test below observed exactly
//! that RED against that shape.
//!
//! The R74 fix moved both guards into the writes themselves:
//! `upsert_task`'s `ON CONFLICT` arm only applies when the presented
//! revision is not lower than the stored one, and
//! `reconcile_ownership`'s `UPDATE` carries `AND revision = ?`, each
//! refusal classified inside the same transaction
//! ([`DomainError::RevisionTooLow`] / [`DomainError::RevisionMismatch`]).
//! The stored revision is deliberately NOT consumed by a rebind: reclaim
//! stays idempotent across retries and restarts (last reconciler wins),
//! and a usurped owner is refused at decision time by the R71/R72 in-tx
//! ownership arbitration instead. The caller-side pre-checks were
//! deleted, so the contract holds under every ordering, not only the one
//! `join!(biased; ...)` pins for reproducibility.
//!
//! (`service::query` is `pub(crate)` and `task_upsert`/`reconcile_omp`
//! are private methods on `OrchestrationService`, so this file drives the
//! repo/db layers directly, exactly as the production service methods now
//! do -- one guarded write round trip each.)

use crew_protocol::{ProjectId, TaskId, TaskRef};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::DomainRepository;
use serde_json::{Value, json};
use tempfile::TempDir;

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

/// The task's current `(revision, owner_client_instance_id)`, or `None`
/// if it has never been upserted. A standalone read, not
/// `service::query::task_get_op` (`pub(crate)`, unreachable here).
async fn stored_task(db: &DatabaseHandle, task_id: TaskId) -> Option<(u64, String)> {
    db.run_domain_op(Box::new(move |conn| {
        let result: Result<(i64, String), rusqlite::Error> = conn.query_row(
            "SELECT revision, owner_client_instance_id FROM tasks WHERE task_id = ?1",
            [task_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        match result {
            Ok((revision, owner)) => Ok(json!({ "revision": revision, "owner": owner })),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Value::Null),
            Err(err) => Err(err.into()),
        }
    }))
    .await
    .expect("read stored task")
    .as_object()
    .map(|obj| {
        (
            obj["revision"].as_u64().expect("revision is a u64"),
            obj["owner"]
                .as_str()
                .expect("owner is a string")
                .to_string(),
        )
    })
}

/// Seeds a task at `revision`, owned by `owner`, via the direct repo
/// write -- bypassing every pre-check, exactly like
/// `approval_owner_race.rs`'s `seed_pending_approval`.
async fn seed_task(
    db: &DatabaseHandle,
    project_id: ProjectId,
    task_id: TaskId,
    owner: &str,
    revision: u64,
) {
    let owner = owner.to_string();
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.upsert_task(
            task_id,
            &TaskRef {
                owner_client_instance_id: owner,
                revision,
            },
        )
        .map(|_| json!({}))
    }))
    .await
    .expect("seed task");
}

/// Mirrors `OrchestrationService::task_upsert`'s post-R74/R76 shape: one
/// guarded write round trip via [`DomainRepository::upsert_task`], whose
/// `ON CONFLICT` arm refuses a lower revision (R74) or a mismatched
/// owner (R76) inside its own transaction. No caller-side pre-check
/// remains.
async fn task_upsert_round_trips(
    db: &DatabaseHandle,
    project_id: ProjectId,
    task_id: TaskId,
    owner: &str,
    revision: u64,
) -> Result<(), String> {
    let task_ref = TaskRef {
        owner_client_instance_id: owner.to_string(),
        revision,
    };
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.upsert_task(task_id, &task_ref).map(|_| json!({}))
    }))
    .await
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// Mirrors `OrchestrationService::reconcile_omp`'s post-R74 shape: one
/// guarded write round trip via [`DomainRepository::reconcile_ownership`],
/// whose `AND revision = ?` predicate arbitrates the match inside its own
/// transaction; the stored revision is not consumed. No caller-side
/// pre-check remains.
async fn reconcile_omp_round_trips(
    db: &DatabaseHandle,
    project_id: ProjectId,
    task_id: TaskId,
    new_owner: &str,
    revision: u64,
) -> Result<(), String> {
    let new_owner = new_owner.to_string();
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.reconcile_ownership(task_id, &new_owner, revision)
            .map(|_| json!({}))
    }))
    .await
    .map_err(|err| err.to_string())?;
    Ok(())
}

/// The number of journaled `ReconcileEvent`s across the whole events
/// table: `RuntimeEvent`'s `#[serde(tag = "type", rename_all =
/// "camelCase")]` renders the variant as `"type":"reconcileEvent"`, and
/// this file uses exactly one task per test, so a substring match is
/// unambiguous (mirrors `approval_owner_race.rs`'s
/// `decided_event_count`).
async fn reconcile_event_count(db: &DatabaseHandle) -> i64 {
    db.run_domain_op(Box::new(|conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_json LIKE '%reconcileEvent%'",
            [],
            |row| row.get(0),
        )?;
        Ok(json!(count))
    }))
    .await
    .expect("count reconcile events")
    .as_i64()
    .expect("count is an integer")
}

/// A lower revision must never land after -- and thus overwrite -- a
/// higher one, even when both writes were enqueued while revision 3 was
/// still stored. Seeds revision 3 (owner `omp-1`); two concurrent
/// `task/upsert`-shaped calls from the same owner present revision 5
/// (declared first) and revision 4 (declared second) -- same owner on
/// both sides so this file's revision-monotonicity subject is isolated
/// from R76's ownership guard, which would otherwise refuse both
/// contenders outright. Written RED against the pre-R74 shape, where
/// both callers' pre-checks read stored revision 3 and both unconditional
/// writes landed, revision 4 last -- final state was `(4, "omp-1")`.
/// Post-fix the guard inside the write refuses the lower revision once
/// revision 5 is stored, under every ordering.
#[tokio::test]
async fn concurrent_upserts_cannot_move_a_revision_backwards() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let task_id = TaskId::new();
    seed_task(&db, project_id, task_id, "omp-1", 3).await;

    let (higher, lower) = tokio::join!(
        biased;
        task_upsert_round_trips(&db, project_id, task_id, "omp-1", 5),
        task_upsert_round_trips(&db, project_id, task_id, "omp-1", 4),
    );

    assert!(
        higher.is_ok(),
        "revision 5 is above the stored revision 3 and must land: {higher:?}"
    );
    let lower_err = lower
        .expect_err("revision 4's guarded write must be refused once revision 5 is already stored");
    assert!(
        lower_err.contains("is lower than stored revision 5"),
        "the refusal must classify the actual stored revision in the same transaction: {lower_err}"
    );

    let (final_revision, final_owner) = stored_task(&db, task_id).await.expect("task exists");
    assert_eq!(
        (final_revision, final_owner.as_str()),
        (5, "omp-1"),
        "the higher revision must win regardless of write order; a guarded write must refuse \
         the lower revision's write once it observes revision 5 is already stored"
    );
}

/// A reconcile whose presented revision is stale *at write time* must be
/// refused. Seeds revision 3 (owner `omp-1`); an upsert from the same
/// owner advances the task to revision 5 -- same owner as the seed so
/// this setup step is unaffected by R76's ownership guard, which is out
/// of scope for this reconcile-revision test; a reconcile still
/// presenting revision 3 must then be refused by the `AND revision = ?`
/// predicate, classified in-transaction with the actual stored revision
/// -- pre-R74 the unguarded `UPDATE` would have rebound (and moved the
/// revision back to 3) regardless. A reconcile presenting the current
/// revision 5 then succeeds, and the stored revision is NOT consumed by
/// the rebind: reclaim stays idempotent -- a repeat reconcile at 5 also
/// succeeds (last reconciler wins; a usurped owner is refused at decision
/// time by the R71/R72 in-tx ownership arbitration instead).
#[tokio::test]
async fn a_reconcile_presenting_a_stale_revision_is_refused() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let task_id = TaskId::new();
    seed_task(&db, project_id, task_id, "omp-1", 3).await;
    task_upsert_round_trips(&db, project_id, task_id, "omp-1", 5)
        .await
        .expect("revision 5 must land");

    let stale = reconcile_omp_round_trips(&db, project_id, task_id, "omp-3", 3)
        .await
        .expect_err("a reconcile presenting revision 3 must be refused once 5 is stored");
    assert!(
        stale.contains("does not match stored revision 5"),
        "the refusal must classify the actual stored revision in the same transaction: {stale}"
    );
    let (revision, owner) = stored_task(&db, task_id).await.expect("task exists");
    assert_eq!(
        (revision, owner.as_str()),
        (5, "omp-1"),
        "a refused reconcile must change nothing"
    );
    assert_eq!(
        reconcile_event_count(&db).await,
        0,
        "a refused reconcile must journal nothing"
    );

    reconcile_omp_round_trips(&db, project_id, task_id, "omp-3", 5)
        .await
        .expect("a reconcile presenting the stored revision must rebind");
    reconcile_omp_round_trips(&db, project_id, task_id, "omp-4", 5)
        .await
        .expect("reclaim is idempotent: the rebind does not consume the stored revision");

    let (revision, owner) = stored_task(&db, task_id).await.expect("task exists");
    assert_eq!(
        (revision, owner.as_str()),
        (5, "omp-4"),
        "the last reconciler wins and the stored revision is unchanged"
    );
    assert_eq!(
        reconcile_event_count(&db).await,
        2,
        "each admitted rebind journals one event"
    );
}

/// A stale revision arriving strictly *after* a newer one, with no
/// concurrency at all: pre-R74 the repo-layer write this file drives
/// accepted it unconditionally (only the since-deleted service-layer
/// pre-check refused it), so this was RED here too. Post-fix the guarded
/// write alone must refuse it. Both upserts present the same owner as
/// the seed -- same owner on both sides so this revision-sequencing
/// subject is isolated from R76's ownership guard.
#[tokio::test]
async fn a_stale_upsert_arriving_after_a_newer_one_is_refused_sequentially() {
    let (_state_dir, db) = open_db().await;
    let project_id = ProjectId::new();
    let task_id = TaskId::new();
    seed_task(&db, project_id, task_id, "omp-1", 1).await;

    task_upsert_round_trips(&db, project_id, task_id, "omp-1", 5)
        .await
        .expect("revision 5 must be accepted");

    let stale = task_upsert_round_trips(&db, project_id, task_id, "omp-1", 4).await;

    assert!(
        stale.is_err(),
        "a strictly sequential stale revision must stay refused: {stale:?}"
    );
    let (final_revision, final_owner) = stored_task(&db, task_id).await.expect("task exists");
    assert_eq!((final_revision, final_owner.as_str()), (5, "omp-1"));
}
