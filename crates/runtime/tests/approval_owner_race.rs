//! Regression tests for a stale-ownership race: `ApprovalService::decide` used to check task
//! ownership as a caller-side pre-check against a snapshot loaded before
//! the guarded write, so a `reconcile/omp` ownership rebind landing in the
//! window between that snapshot and the write left a stale owner's
//! decision with nothing left to refuse it.
//!
//! See `approval_decide_race.rs`'s header for the full actor-FIFO argument;
//! summarized here only as far as this file's construction needs it:
//! `DatabaseHandle::run_domain_op` sends whole boxed closures to a
//! single-owner thread over a FIFO channel, one `oneshot` reply per
//! command. The actor never interleaves the *inside* of two closures, only
//! whole closures with each other, in the order their commands were
//! enqueued.
//!
//! Before the fix, `ApprovalService::decide` was *two* round trips:
//! `load_snapshot` (which read `tasks.owner_client_instance_id` for the
//! caller-side pre-check), then, once the pre-check and `humanRequired`
//! check passed synchronously, `decide_approval` (the guarded write, which
//! never re-checked ownership). `DomainRepository::reconcile_ownership` --
//! the same method `reconcile/omp` calls -- is *one* round trip: an
//! unguarded `UPDATE tasks SET owner_client_instance_id = ...`.
//!
//! The first test below drives `svc.decide` (as the *original* owner) and
//! a direct call to `reconcile_ownership` (rebinding to a *new* owner)
//! through `tokio::join!(biased; ...)`, `decide` declared first, to pin a
//! reproducible interleaving: `biased` polls `decide` before the rebind on
//! every poll, so `decide`'s `load_snapshot` command was always enqueued --
//! and thus processed -- before the rebind's `UPDATE`, and the rebind
//! always committed before `decide`'s second command (`decide_approval`)
//! could possibly be sent (that second `send()` cannot occur before
//! `load_snapshot`'s reply wakes `decide` for another poll, which cannot
//! happen before the rebind's synchronous first-poll `send()`). Against
//! the pre-fix code, that pinned interleaving is exactly what made this
//! test fail RED: the stale owner's caller-side pre-check read the
//! original owner and passed, and the unguarded write had no ownership
//! check left to refuse it with, so the decision was accepted instead of
//! refused. That committed RED failure is what pinned the interleaving
//! above as real and reproducible, not hypothetical.
//!
//! Ownership is now arbitrated exclusively inside `decide_approval`'s
//! guarded transaction, alongside the conflict, idempotent-replay,
//! and settled-run checks that an earlier fix already moved there: the write itself
//! re-reads `tasks.owner_client_instance_id` and refuses a caller that no
//! longer owns the task. That makes the `biased` enqueue ordering this
//! file still uses no longer load-bearing for correctness -- it now exists
//! only to keep the interleaving deterministic and easy to reason about.
//! Ownership is arbitrated at write time, so the assertion below holds
//! under every ordering the rebind and the decide can occur in, not only
//! the one `biased` pins.
//!
//! The second test proves the fix does not over-reject: a rebind followed,
//! sequentially, by a decide from the *new* owner must still succeed, with
//! exactly one `ApprovalDecided` event.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crew_protocol::{
    ApprovalId, ApprovalRequest, DecidedBy, ProjectId, Run, RunFlags, RunId, RunState, TaskId,
    TaskRef, Timestamp, Worker, WorkerId, WorkerProfileRef,
};
use crew_runtime::approval::{
    ApprovalCallback, ApprovalError, ApprovalService, CallbackFuture, DecideOutcome,
};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::domain::DomainRepository;
use rusqlite::OptionalExtension;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::broadcast;

async fn open_db() -> (TempDir, DatabaseHandle) {
    let state_dir = TempDir::new().unwrap();
    let db_path = state_dir.path().join("runtime.db");
    let db = DatabaseHandle::start(db_path).await.unwrap();
    (state_dir, db)
}

/// An [`ApprovalCallback`] that counts invocations and always succeeds, for
/// asserting that a refused decision never reaches the adapter.
struct CountingCallback {
    calls: Arc<AtomicU32>,
}

impl ApprovalCallback for CountingCallback {
    fn acknowledge(&self, _approval_id: ApprovalId, _decision: &str) -> CallbackFuture<'static> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// Seeds one task (owned by `"omp-1"`)/worker/run, drives the run to
/// `working`, and creates one pending, human-required approval against it.
/// Returns the ids a test needs to decide, rebind, and probe.
async fn seed_pending_approval(
    db: &DatabaseHandle,
    project_id: ProjectId,
) -> (ApprovalId, RunId, TaskId) {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = RunId::new();
    let approval_id = ApprovalId::new();

    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.upsert_task(
            task_id,
            &TaskRef {
                owner_client_instance_id: "omp-1".into(),
                revision: 1,
            },
        )?;
        let worker = Worker {
            worker_id,
            profile_ref: WorkerProfileRef {
                id: worker_id,
                fingerprint: "sha256:fake".into(),
                adapter: "fake".into(),
                model: "test".into(),
                permission_envelope: json!({}),
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
        Ok(json!({}))
    }))
    .await
    .expect("seed task/worker/run");

    for state in ["starting", "working"] {
        let to = RunState::try_from(state).expect("valid state");
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.transition_run(run_id, &to, None).map(|_| json!({}))
        }))
        .await
        .unwrap_or_else(|e| panic!("drive to {state} failed: {e}"));
    }

    let request = ApprovalRequest {
        approval_id,
        run_id,
        task_id,
        action: "write file".into(),
        arguments: json!({ "path": "/tmp/x" }),
        human_required: true,
        policy_reason: "write requires human approval".into(),
        created_at: Timestamp::now(),
        decided_at: None,
        decision: None,
        decided_by: None,
        reason: None,
    };
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.create_approval(&request).map(|_| json!({}))
    }))
    .await
    .expect("create the approval");

    (approval_id, run_id, task_id)
}

/// An [`ApprovalService`] wired to the given callback; the broadcast sender
/// has no subscribers, which is the production shape for an unattached
/// console.
fn service(
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    callback: Arc<dyn ApprovalCallback>,
) -> ApprovalService {
    ApprovalService::new(db, project_id, callback, broadcast::channel(64).0)
}

/// A single `run_domain_op` round trip calling
/// [`DomainRepository::reconcile_ownership`] -- the same repo method
/// `reconcile/omp` calls (`service/orchestration.rs::reconcile_omp`) --
/// directly, presenting the task's current stored revision so its guarded
/// revision match admits the rebind.
async fn rebind_owner(
    db: &DatabaseHandle,
    project_id: ProjectId,
    task_id: TaskId,
    new_owner: &str,
    revision: u64,
) {
    let new_owner = new_owner.to_string();
    db.run_domain_op(Box::new(move |conn| {
        let mut repo = DomainRepository::new(conn, project_id);
        repo.reconcile_ownership(task_id, &new_owner, revision)
            .map(|_| json!({}))
    }))
    .await
    .expect("rebind task ownership");
}

async fn approval_decision(db: &DatabaseHandle, approval_id: ApprovalId) -> Option<String> {
    db.run_domain_op(Box::new(move |conn| {
        let decision: Option<String> = conn.query_row(
            "SELECT decision FROM approvals WHERE approval_id = ?1",
            [approval_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(json!(decision))
    }))
    .await
    .expect("read approval decision")
    .as_str()
    .map(str::to_string)
}

async fn run_state(db: &DatabaseHandle, run_id: RunId) -> String {
    db.run_domain_op(Box::new(move |conn| {
        let state: String = conn.query_row(
            "SELECT state FROM runs WHERE run_id = ?1",
            [run_id.to_string()],
            |r| r.get(0),
        )?;
        Ok(json!(state))
    }))
    .await
    .expect("read run state")
    .as_str()
    .expect("state is a string")
    .to_string()
}

async fn decided_event_count(db: &DatabaseHandle) -> i64 {
    db.run_domain_op(Box::new(|conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_json LIKE '%approvalDecided%'",
            [],
            |r| r.get(0),
        )?;
        Ok(json!(count))
    }))
    .await
    .expect("count decided events")
    .as_i64()
    .expect("count is an integer")
}

/// The journal sequence of the `reconcileEvent` row rebinding `task_id`, if
/// one has been committed. Diagnostic only, for
/// [`a_stale_owner_is_refused_by_the_guarded_write_after_a_rebind`]'s
/// failure message -- this event always commits unconditionally
/// (`reconcile_ownership` has no ownership guard of its own), so its
/// absence would itself be informative.
async fn reconcile_event_sequence(db: &DatabaseHandle, task_id: TaskId) -> Option<i64> {
    db.run_domain_op(Box::new(move |conn| {
        let sequence: Option<i64> = conn
            .query_row(
                "SELECT sequence FROM events WHERE task_id = ?1 AND event_json LIKE '%reconcileEvent%' \
                 ORDER BY sequence ASC LIMIT 1",
                [task_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json!(sequence))
    }))
    .await
    .expect("query reconcile event sequence")
    .as_i64()
}

/// The journal sequence of the `approvalDecided` row for `approval_id` on
/// `task_id`, if the guarded write actually committed one. Diagnostic
/// only, for the same failure message as [`reconcile_event_sequence`] --
/// comparing the two sequences (when both exist) tells a reader which
/// write the actor actually processed first, which is the fact the
/// enqueue-order argument in this file's header claims is fixed and this
/// diagnostic exists to confirm or refute on an actual recurrence rather
/// than a rerun of the same argument.
///
/// **The `LIKE '%approvalDecided%<id>%'` match is correct only because
/// `RuntimeEvent` is adjacently tagged
/// (`#[serde(tag = "type", content = "payload")]`), so the type string
/// always precedes the id in the serialized JSON.** If that tagging ever
/// changes, this returns `None` for an event that actually exists, and
/// the failure message would then read as "the guarded write refused"
/// when it did not -- the opposite of the truth.
/// `a_former_owner_replaying_its_identical_decision_is_refused` below
/// asserts this helper actually finds a real, known-committed event,
/// specifically so a serde change like that fails there, at the point of
/// change, instead of producing a confidently wrong diagnosis on a rare
/// CI recurrence of this test.
async fn decided_event_sequence(
    db: &DatabaseHandle,
    task_id: TaskId,
    approval_id: ApprovalId,
) -> Option<i64> {
    db.run_domain_op(Box::new(move |conn| {
        let sequence: Option<i64> = conn
            .query_row(
                "SELECT sequence FROM events WHERE task_id = ?1 AND event_json LIKE ?2 \
                 ORDER BY sequence ASC LIMIT 1",
                rusqlite::params![
                    task_id.to_string(),
                    format!("%approvalDecided%{approval_id}%")
                ],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json!(sequence))
    }))
    .await
    .expect("query decided event sequence")
    .as_i64()
}

/// The task's currently stored owner. Diagnostic only: since
/// `decide_approval`'s guarded write never writes this column (only reads
/// and compares it), whatever this reads back after the race has settled
/// is exactly the value the last committed `reconcile_ownership` call
/// wrote -- useful alongside the two sequences above to tell whether the
/// guarded write's own re-read (not observable directly from the test)
/// plausibly saw the same value.
async fn current_task_owner(db: &DatabaseHandle, task_id: TaskId) -> Option<String> {
    db.run_domain_op(Box::new(move |conn| {
        let owner: Option<String> = conn
            .query_row(
                "SELECT owner_client_instance_id FROM tasks WHERE task_id = ?1",
                [task_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json!(owner))
    }))
    .await
    .expect("query current task owner")
    .as_str()
    .map(str::to_string)
}

/// The sibling assertion in `violation_owner_race.rs` was observed to fail
/// once on CI (macOS), unreproduced locally across 1,030 runs spanning
/// three contention profiles; this test shares the identical race shape
/// and gets the same diagnostic treatment on the same suspicion, though it
/// has not itself failed. On recurrence (either file), read the sequence
/// numbers, the guarded-write outcome, and the current owner printed below
/// rather than re-running the enqueue-order argument in this file's header.
/// Reading the three: `decided sequence < reconcile sequence` means the
/// guarded write really was processed first (the enqueue-order argument
/// itself is wrong); `decided sequence > reconcile sequence` with the
/// current owner already `"omp-2"` means the write ran after the rebind
/// but its ownership re-read still saw the stale value -- a real bug in
/// `decide_approval`, though printing these three facts alone cannot yet
/// distinguish a stale read from no re-read happening at all; `reconcile
/// sequence: None` means the rebind itself never committed, pointing at
/// `rebind_owner`/`reconcile_ownership` instead.
#[tokio::test]
async fn a_stale_owner_is_refused_by_the_guarded_write_after_a_rebind() {
    let ok_reason = crew_protocol::Redacted::assert_runtime_authored("ok");
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, task_id) = seed_pending_approval(&db, project_id).await;
    let calls = Arc::new(AtomicU32::new(0));
    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        }),
    );

    // `decide` (as the original owner "omp-1") is declared first, so its
    // `load_snapshot` round trip is enqueued -- and thus processed -- before
    // the rebind's `UPDATE`; the rebind then commits before `decide`'s
    // second round trip (`decide_approval`, the guarded write that now
    // arbitrates ownership) can possibly be sent, per the enqueue-order
    // argument in this file's header. Deterministic, no dependence on real
    // timing -- though, per that same header, no longer load-bearing for
    // correctness, only for reproducibility.
    let (decide_result, _rebind) = tokio::join!(
        biased;
        svc.decide(approval_id, "omp-1", "approve", &ok_reason, DecidedBy::Human),
        rebind_owner(&db, project_id, task_id, "omp-2", 1),
    );

    // Gathered unconditionally, not only on failure -- `assert!`'s message
    // is a synchronous expression and cannot itself await a query. The
    // extra reads cost nothing the test doesn't already pay for and give a
    // future failure exactly what a debugger would ask for first: which
    // write the actor actually processed first, not another restatement
    // of which order it is supposed to.
    let reconcile_seq = reconcile_event_sequence(&db, task_id).await;
    let decided_seq = decided_event_sequence(&db, task_id, approval_id).await;
    let owner_now = current_task_owner(&db, task_id).await;

    assert!(
        matches!(decide_result, Err(ApprovalError::Forbidden { .. })),
        "a stale owner must be refused once the guarded write observes the rebind, not accepted: \
         {decide_result:?} -- reconcileEvent sequence: {reconcile_seq:?}, approvalDecided sequence: \
         {decided_seq:?} (present means the guarded write committed instead of refusing; a lower \
         number than the reconcile sequence means it was processed first), current stored owner: \
         {owner_now:?} (the guarded write never writes this column, only reads it, so this is \
         exactly what `reconcile_ownership` last committed)"
    );
    assert_eq!(
        approval_decision(&db, approval_id).await,
        None,
        "a refused decision must never be recorded"
    );
    assert_eq!(
        decided_event_count(&db).await,
        0,
        "no approvalDecided event may survive a refused decision"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a refused decision must never reach the adapter callback"
    );
    assert_eq!(
        run_state(&db, run_id).await,
        "waitingUser",
        "a refused decision must not move the run out of waitingUser"
    );
}

#[tokio::test]
async fn the_new_owner_can_decide_after_a_rebind() {
    let ok_reason = crew_protocol::Redacted::assert_runtime_authored("ok");
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, task_id) = seed_pending_approval(&db, project_id).await;
    let calls = Arc::new(AtomicU32::new(0));
    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        }),
    );

    // Rebind sequentially, fully committed before decide is even called --
    // zero timing dependency -- so this test guards the eventual fix
    // against over-rejection: a legitimate new owner must still be able to
    // decide after a rebind.
    rebind_owner(&db, project_id, task_id, "omp-2", 1).await;

    let outcome = svc
        .decide(
            approval_id,
            "omp-2",
            "approve",
            &ok_reason,
            DecidedBy::Human,
        )
        .await;

    assert!(
        matches!(outcome, Ok(DecideOutcome::Decided)),
        "the rebound owner must be able to decide: {outcome:?}"
    );
    assert_eq!(
        approval_decision(&db, approval_id).await,
        Some("approve".to_string())
    );
    assert_eq!(
        decided_event_count(&db).await,
        1,
        "exactly one approvalDecided event must be journaled"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the deciding call must reach the adapter callback"
    );
    assert_eq!(run_state(&db, run_id).await, "working");
}

#[tokio::test]
async fn a_former_owner_replaying_its_identical_decision_is_refused() {
    let ok_reason = crew_protocol::Redacted::assert_runtime_authored("ok");
    let (_state_dir, db) = open_db().await;
    let db = Arc::new(db);
    let project_id = ProjectId::new();
    let (approval_id, run_id, task_id) = seed_pending_approval(&db, project_id).await;
    let calls = Arc::new(AtomicU32::new(0));
    let svc = service(
        Arc::clone(&db),
        project_id,
        Arc::new(CountingCallback {
            calls: Arc::clone(&calls),
        }),
    );

    let outcome = svc
        .decide(
            approval_id,
            "omp-1",
            "approve",
            &ok_reason,
            DecidedBy::Human,
        )
        .await;
    assert!(
        matches!(outcome, Ok(DecideOutcome::Decided)),
        "the original owner must be able to decide: {outcome:?}"
    );
    // Positive control for `decided_event_sequence`'s own doc comment: a
    // real, known-committed event must actually be found by the same LIKE
    // pattern the failure-diagnostic helper above depends on, so a future
    // change to RuntimeEvent's serde tagging shape fails loudly here
    // instead of silently returning None from that helper on a rare CI
    // recurrence of the stale-owner test.
    assert!(
        decided_event_sequence(&db, task_id, approval_id)
            .await
            .is_some(),
        "a just-committed approvalDecided event must be found by decided_event_sequence's own \
         match pattern -- if this fails, RuntimeEvent's serde tagging changed and the diagnostic \
         in a_stale_owner_is_refused_by_the_guarded_write_after_a_rebind is no longer trustworthy"
    );

    rebind_owner(&db, project_id, task_id, "omp-2", 1).await;

    // The guarded write checks `tasks.owner_client_instance_id` before it
    // checks whether a decision is already on record (repository.rs's
    // `decide_approval`), so a former owner replaying its own,
    // now-recorded decision is refused with `Forbidden` -- ownership
    // outranks idempotent replay, it is not treated as a no-op repeat of
    // an identical decision.
    let replay = svc
        .decide(
            approval_id,
            "omp-1",
            "approve",
            &ok_reason,
            DecidedBy::Human,
        )
        .await;

    assert!(
        matches!(replay, Err(ApprovalError::Forbidden { .. })),
        "a former owner replaying its own identical decision must be refused by ownership, not accepted as an idempotent replay: {replay:?}"
    );
    assert_eq!(
        approval_decision(&db, approval_id).await,
        Some("approve".to_string()),
        "the original decision must remain on record"
    );
    assert_eq!(
        decided_event_count(&db).await,
        1,
        "the refused replay must not journal a second event"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the refused replay must not reach the adapter callback a second time"
    );
    assert_eq!(run_state(&db, run_id).await, "working");
}
