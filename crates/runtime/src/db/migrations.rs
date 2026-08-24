//! Opens the runtime's SQLite database privately, configures its PRAGMAs,
//! and applies its schema migrations atomically.

use std::path::Path;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

use super::actor::DbError;

/// Migration 1: the durable event journal and the operation-intent table.
const MIGRATION_1: &str = "
CREATE TABLE events (
  sequence INTEGER PRIMARY KEY,
  timestamp TEXT NOT NULL,
  project_id TEXT NOT NULL,
  run_id TEXT,
  event_json TEXT NOT NULL
);
CREATE TABLE operations (
  operation_id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  intent_json TEXT NOT NULL,
  requested_at TEXT NOT NULL,
  acknowledged_at TEXT,
  acknowledgement_json TEXT
);
";

/// Migration 2: orchestration projections (tasks, workers, worker
/// profiles, runs, messages, approvals). Kept in a normalized shape
/// alongside the append-only `events` journal; every mutation goes
/// through one transaction that appends an event and updates the
/// relevant projection row(s).
const MIGRATION_2: &str = "
CREATE TABLE worker_profiles (
  id TEXT PRIMARY KEY,
  fingerprint TEXT NOT NULL,
  adapter TEXT NOT NULL,
  model TEXT NOT NULL,
  permission_envelope TEXT NOT NULL
);
CREATE TABLE tasks (
  task_id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  owner_client_instance_id TEXT NOT NULL,
  revision INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE workers (
  worker_id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  profile_id TEXT NOT NULL REFERENCES worker_profiles(id),
  parent_worker_id TEXT REFERENCES workers(worker_id),
  created_at TEXT NOT NULL
);
CREATE TABLE runs (
  run_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES tasks(task_id),
  worker_id TEXT NOT NULL REFERENCES workers(worker_id),
  state TEXT NOT NULL,
  flags_degraded_control INTEGER NOT NULL DEFAULT 0,
  flags_needs_reconciliation INTEGER NOT NULL DEFAULT 0,
  flags_protocol_unhealthy INTEGER NOT NULL DEFAULT 0,
  flags_policy_quarantined INTEGER NOT NULL DEFAULT 0,
  flags_workspace_dirty INTEGER NOT NULL DEFAULT 0,
  flags_children_active INTEGER NOT NULL DEFAULT 0,
  vendor_session_id TEXT,
  created_at TEXT NOT NULL,
  started_at TEXT,
  completed_at TEXT
);
CREATE TABLE messages (
  message_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES runs(run_id),
  sender_worker_id TEXT NOT NULL,
  recipient_worker_id TEXT,
  task_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  delivery_state TEXT NOT NULL,
  created_at TEXT NOT NULL,
  sent_at TEXT,
  acknowledged_at TEXT,
  reply_to TEXT
);
CREATE TABLE approvals (
  approval_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES runs(run_id),
  task_id TEXT NOT NULL,
  action TEXT NOT NULL,
  arguments TEXT NOT NULL,
  human_required INTEGER NOT NULL DEFAULT 0,
  policy_reason TEXT NOT NULL,
  created_at TEXT NOT NULL,
  decided_at TEXT,
  decision TEXT
);
";

/// Migration 3: registered adapter worker profiles (Worker Adapters
/// milestone), plus a `workers.resolved_profile_json` column carrying the
/// full resolved `WorkerProfile` snapshot (startup options, environment
/// allowlist, source) for a worker created from a `profileId` -- copied
/// in once at creation time, so it is immune to whatever later happens to
/// the source row in `adapter_profiles`. `adapter_profiles` itself is
/// deliberately outside the append-only `events` journal -- profile
/// registration is configuration, not an orchestration fact, so it is
/// never journaled or broadcast (see
/// `crate::adapter::profile_store::ProfileStore`).
const MIGRATION_3: &str = "
CREATE TABLE adapter_profiles (
  id TEXT PRIMARY KEY,
  adapter TEXT NOT NULL,
  model TEXT NOT NULL,
  permission_envelope TEXT NOT NULL,
  startup_options_json TEXT NOT NULL,
  environment_allowlist_json TEXT NOT NULL,
  source TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  created_at TEXT NOT NULL
);
ALTER TABLE workers ADD COLUMN resolved_profile_json TEXT;
";

/// Migration 4: mid-run nested-worker policy violations (Hardening plan
/// Task 1). Distinct from the pre-authorization `PolicyViolation` runtime
/// event -- this table tracks a worker that was already running and then
/// unexpectedly reported a child, through to its resolution via
/// `policy/violation/decide`. `action` is the `NestedViolationAction`
/// applied at record time (`quarantine`/`cancel`/`quarantineAndCancel`);
/// `resolution` is `release`/`cancel`, set only once `resolved_at` is set.
const MIGRATION_4: &str = "
CREATE TABLE policy_violations (
  violation_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES runs(run_id),
  task_id TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  vendor_child_id TEXT NOT NULL,
  vendor_parent_ref TEXT NOT NULL,
  action TEXT NOT NULL,
  created_at TEXT NOT NULL,
  resolved_at TEXT,
  resolution TEXT,
  resolved_by TEXT
);
";

/// Migration 5: enrich the events journal with envelope convenience columns
/// so that events/replay can reconstruct full EventEnvelopes from disk
/// (previously these fields were only available in-memory during live
/// broadcast). The columns are nullable -- existing rows before migration
/// will have NULL here, and replay() handles NULL by returning None.
const MIGRATION_5: &str = "
ALTER TABLE events ADD COLUMN task_id TEXT;
ALTER TABLE events ADD COLUMN worker_id TEXT;
ALTER TABLE events ADD COLUMN parent_worker_id TEXT;
ALTER TABLE events ADD COLUMN vendor_event_ref TEXT;
";

/// Migration 6: the policy snapshot each run was authorized under, so a
/// violation or audit can be resolved against a specific merged policy.
/// Nullable, because rows written before this migration have no
/// fingerprint -- never backfill a fabricated one.
const MIGRATION_6: &str = "
ALTER TABLE runs ADD COLUMN policy_fingerprint TEXT;
";

/// Migration 7: records who decided each approval, so a `human_required`
/// decision is auditable after the fact. Nullable -- rows written before
/// this migration have no provenance and are never backfilled with a
/// fabricated one.
const MIGRATION_7: &str = "
ALTER TABLE approvals ADD COLUMN decided_by TEXT;
";

/// Migration 8: `policy_violations.vendor_child_id`/`vendor_parent_ref` were
/// declared `NOT NULL` by migration 4, when the only violation kind was a
/// nested worker. A cost-ceiling violation has no vendor child at all and
/// journals both as `None` (see
/// `DomainRepository::record_policy_violation`), so every
/// `cost_ceiling_exceeded` insert failed the constraint and no ceiling could
/// ever be enforced. SQLite cannot drop a column constraint in place, so the
/// table is rebuilt with both columns nullable; every other column, and every
/// existing row, is preserved unchanged.
const MIGRATION_8: &str = "
CREATE TABLE policy_violations_new (
  violation_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES runs(run_id),
  task_id TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  vendor_child_id TEXT,
  vendor_parent_ref TEXT,
  action TEXT NOT NULL,
  created_at TEXT NOT NULL,
  resolved_at TEXT,
  resolution TEXT,
  resolved_by TEXT
);
INSERT INTO policy_violations_new
  SELECT violation_id, run_id, task_id, worker_id, vendor_child_id, vendor_parent_ref,
         action, created_at, resolved_at, resolution, resolved_by
  FROM policy_violations;
DROP TABLE policy_violations;
ALTER TABLE policy_violations_new RENAME TO policy_violations;
";

/// Migration 9: persists the approval decision's rationale (R59) and
/// repairs rows R34 left behind: `decided_by` was written via
/// `serde_json::to_string`, storing the JSON-quoted token (`"human"` with
/// quotes), so equality against the bare token matched nothing. The
/// `UPDATE` strips exactly one leading and trailing quote from affected
/// rows; `reason` stays nullable -- decisions recorded before this
/// migration have no rationale and are never backfilled with a
/// fabricated one.
const MIGRATION_9: &str = "
ALTER TABLE approvals ADD COLUMN reason TEXT;
UPDATE approvals SET decided_by = SUBSTR(decided_by, 2, LENGTH(decided_by) - 2)
  WHERE decided_by LIKE '\"%\"';
";

/// Migration 10: the durable position of a TUI-mode worker's transcript
/// tailer (`crate::adapter::tui::Cursor`, serialized as JSON), persisted
/// transactionally alongside each committed adapter-event batch so a
/// crashed daemon re-tails from its last durable position instead of the
/// start of the vendor transcript. Nullable -- a run with no TUI-mode
/// tailer (or one that has not yet committed a batch) has no cursor and
/// is never backfilled with a fabricated one.
const MIGRATION_10: &str = "
ALTER TABLE runs ADD COLUMN transcript_cursor TEXT;
";
/// Migration 11: the persisted leader plan layer. A plan is proposed for a
/// run (`plan/propose` -> `PlanProposed`) and later approved or rejected
/// (`plan/decide` -> `PlanDecided`). The daemon stores and enforces
/// nothing about *routing* -- OMP owns the task graph; a plan is persisted
/// leader intent plus the `writes`/`turn_budget` metadata a later work
/// package reads when it builds the subtask graph.
///
/// The `PlanProposed`/`PlanDecided` events carry `run_id`/`task_id`/
/// `worker_id`, so those are persisted here too (the brief's table
/// omitted them; they are required to reconstruct the broadcast envelope
/// and to let `plan/get` resolve the run's task/worker). The plan is
/// keyed 1:1 by `run_id`: at most one plan row exists per run, and
/// `plan/propose` refuses to overwrite an existing one.
const MIGRATION_11: &str = "
CREATE TABLE plans (
  plan_id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  task_id TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  owner_client_instance_id TEXT NOT NULL,
  task_text TEXT NOT NULL,
  subtasks_json TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at TEXT NOT NULL,
  decided_at TEXT,
  decided_reason TEXT
);
";

/// Opens `path` as a private (mode `0600`) SQLite database, configures its
/// PRAGMAs (`journal_mode=WAL`, `foreign_keys=ON`, `busy_timeout=5000`,
/// `synchronous=FULL`), and migrates it to the latest schema. Migrations
/// are applied atomically by `rusqlite_migration`.
///
/// # Errors
/// Returns [`DbError`] if the file cannot be created privately, the
/// connection cannot be opened or configured, or migration fails.
pub(super) fn open_and_migrate(path: &Path) -> Result<Connection, DbError> {
    crate::security::ensure_private_file(path)?;

    let mut conn = Connection::open(path)?;

    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "busy_timeout", 5000_i64)?;
    conn.pragma_update(None, "synchronous", "FULL")?;

    migrate(&mut conn)?;

    Ok(conn)
}

/// The one migration list, shared by `migrate` and the migration tests so a
/// test can never assert against a hand-copied schema.
fn migration_list() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(MIGRATION_1),
        M::up(MIGRATION_2),
        M::up(MIGRATION_3),
        M::up(MIGRATION_4),
        M::up(MIGRATION_5),
        M::up(MIGRATION_6),
        M::up(MIGRATION_7),
        M::up(MIGRATION_8),
        M::up(MIGRATION_9),
        M::up(MIGRATION_10),
        M::up(MIGRATION_11),
    ])
}

/// Applies every migration to an already-open connection, atomically.
/// Tests open an in-memory connection and call this rather than
/// hand-copying a schema, so a projection table can never drift from what
/// production runs against.
///
/// # Errors
/// Returns [`DbError`] if migration fails.
pub fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    migration_list().to_latest(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    /// Exercises migration 8's table rebuild: proves the old schema rejects
    /// NULL vendor refs, the rebuild preserves existing rows, and the new
    /// schema accepts them.
    #[test]
    fn migration_8_makes_vendor_refs_nullable_and_preserves_existing_rows() {
        // FK on — this test exercises both column constraints and referential
        // integrity, so we seed parent tables in FK order before inserting
        // violations. This matches production behavior where FKs are enforced.
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign_keys on");

        // Migrate to version 7 (before the rebuild).
        migration_list()
            .to_version(&mut conn, 7)
            .expect("migrate to v7");

        // Seed parent tables in FK order: worker_profiles → tasks/workers → runs
        // Note: project_id is just TEXT (no projects table), worker_profiles has no created_at
        conn.execute(
            "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["wp-1", "sha256:fake", "fake", "test", "{}"],
        )
        .expect("seed worker_profiles");
        conn.execute(
            "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["t-1", "p-1", "omp-1", 1i32, "2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z"],
        )
        .expect("seed tasks");
        conn.execute(
            "INSERT INTO workers (worker_id, project_id, profile_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["w-1", "p-1", "wp-1", "2026-01-01T00:00:00Z"],
        )
        .expect("seed workers");
        conn.execute(
            "INSERT INTO runs (run_id, task_id, worker_id, state, flags_degraded_control, flags_needs_reconciliation,
               flags_protocol_unhealthy, flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
               vendor_session_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                "r-1", "t-1", "w-1", "queued", 0i32, 0i32, 0i32, 0i32, 0i32, 0i32,
                Option::<String>::None, "2026-01-01T00:00:00Z"
            ],
        )
        .expect("seed runs");

        // A nested-worker-shaped row (both vendor refs non-null) succeeds.
        conn.execute(
            "INSERT INTO policy_violations (violation_id, run_id, task_id, worker_id,
               vendor_child_id, vendor_parent_ref, action, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "v-nested",
                "r-1",
                "t-1",
                "w-1",
                "child-1",
                "parent-1",
                "quarantine",
                "2026-01-01T00:00:00Z"
            ],
        )
        .expect("nested-worker row succeeds at v7");

        // A cost-ceiling-shaped row (NULL vendor refs) must fail.
        let err = conn.execute(
            "INSERT INTO policy_violations (violation_id, run_id, task_id, worker_id,
               vendor_child_id, vendor_parent_ref, action, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "v-cost",
                "r-1",
                "t-1",
                "w-1",
                Option::<String>::None,
                Option::<String>::None,
                "quarantine",
                "2026-01-01T00:00:00Z"
            ],
        );
        assert!(err.is_err(), "NULL vendor refs must fail at v7");
        assert!(
            err.unwrap_err().to_string().contains("NOT NULL"),
            "the failure must be the NOT NULL constraint"
        );

        // Apply migration 8 (the table rebuild).
        migration_list()
            .to_version(&mut conn, 8)
            .expect("migrate to v8");

        // The pre-existing row survived with its values intact.
        let (vc, vp, act, created): (String, String, String, String) = conn
            .query_row(
                "SELECT vendor_child_id, vendor_parent_ref, action, created_at
                 FROM policy_violations WHERE violation_id = ?1",
                ["v-nested"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .expect("v-nested row survived the rebuild");
        assert_eq!(vc, "child-1");
        assert_eq!(vp, "parent-1");
        assert_eq!(act, "quarantine");
        assert_eq!(created, "2026-01-01T00:00:00Z");

        // A cost-ceiling-shaped row (NULL vendor refs) now succeeds.
        conn.execute(
            "INSERT INTO policy_violations (violation_id, run_id, task_id, worker_id,
               vendor_child_id, vendor_parent_ref, action, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "v-cost",
                "r-1",
                "t-1",
                "w-1",
                Option::<String>::None,
                Option::<String>::None,
                "quarantine",
                "2026-01-01T00:00:00Z"
            ],
        )
        .expect("cost-ceiling row must succeed at v8");

        // The NULLs are real SQL NULLs, not empty strings.
        let (vc, vp): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT vendor_child_id, vendor_parent_ref
                 FROM policy_violations WHERE violation_id = ?1",
                ["v-cost"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("v-cost row exists");
        assert!(vc.is_none(), "vendor_child_id must be NULL");
        assert!(vp.is_none(), "vendor_parent_ref must be NULL");

        // The action constraint is still enforced (the rebuild did not drop it).
        let err = conn.execute(
            "INSERT INTO policy_violations (violation_id, run_id, task_id, worker_id,
               vendor_child_id, vendor_parent_ref, action, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "v-bad",
                "r-1",
                "t-1",
                "w-1",
                Option::<String>::None,
                Option::<String>::None,
                Option::<String>::None,
                "2026-01-01T00:00:00Z"
            ],
        );
        assert!(err.is_err(), "NULL action must still fail");

        // The resolution columns survived the rebuild.
        conn.execute(
            "UPDATE policy_violations SET resolution = 'release', resolved_by = 'omp-1',
               resolved_at = '2026-01-02T00:00:00Z' WHERE violation_id = 'v-cost'",
            [],
        )
        .expect("resolution columns must work after rebuild");

        let (res, resolved_by, resolved_at): (Option<String>, Option<String>, Option<String>) =
            conn.query_row(
                "SELECT resolution, resolved_by, resolved_at
                 FROM policy_violations WHERE violation_id = ?1",
                ["v-cost"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("v-cost resolution row");
        assert_eq!(res.as_deref(), Some("release"));
        assert_eq!(resolved_by.as_deref(), Some("omp-1"));
        assert_eq!(resolved_at.as_deref(), Some("2026-01-02T00:00:00Z"));

        // Verify referential integrity is intact after the rebuild.
        let fk_violations: Vec<String> = conn
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare")
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        assert!(
            fk_violations.is_empty(),
            "foreign_key_check must report no violations after migration 8: {fk_violations:?}"
        );
    }

    /// Exercises migration 9: a pre-existing approval row whose
    /// `decided_by` was written JSON-quoted by R34's bug is rewritten to
    /// the bare token, and the new `reason` column exists and is NULL for
    /// pre-migration rows.
    #[test]
    fn migration_9_adds_reason_and_repairs_quoted_decided_by() {
        let mut conn = Connection::open_in_memory().expect("in-memory db");

        migration_list()
            .to_version(&mut conn, 8)
            .expect("migrate to v8");

        // Seed parents (FK order) and two approvals: one quoted (the bug),
        // one undecided.
        conn.execute(
            "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
             VALUES ('wp-1', 'sha256:fake', 'fake', 'test', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
             VALUES ('t-1', 'p-1', 'omp-1', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workers (worker_id, project_id, profile_id, created_at)
             VALUES ('w-1', 'p-1', 'wp-1', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (run_id, task_id, worker_id, state, flags_degraded_control, flags_needs_reconciliation,
               flags_protocol_unhealthy, flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
               vendor_session_id, created_at)
             VALUES ('r-1', 't-1', 'w-1', 'queued', 0, 0, 0, 0, 0, 0, NULL, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approvals (approval_id, run_id, task_id, action, arguments, human_required,
               policy_reason, created_at, decided_at, decision, decided_by)
             VALUES ('a-decided', 'r-1', 't-1', 'write', '{}', 1, 'policy',
               '2026-01-01T00:00:00Z', '2026-01-01T01:00:00Z', 'approve', '\"human\"')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO approvals (approval_id, run_id, task_id, action, arguments, human_required,
               policy_reason, created_at)
             VALUES ('a-pending', 'r-1', 't-1', 'write', '{}', 0, 'policy', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        migration_list()
            .to_version(&mut conn, 9)
            .expect("migrate to v9");

        // The quoted token was repaired to the bare form.
        let (decided_by, reason): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT decided_by, reason FROM approvals WHERE approval_id = 'a-decided'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(decided_by.as_deref(), Some("human"));
        assert!(
            reason.is_none(),
            "pre-migration rows are never backfilled with a fabricated reason"
        );

        // The undecided row is untouched (NULL decided_by stays NULL).
        let decided_by: Option<String> = conn
            .query_row(
                "SELECT decided_by FROM approvals WHERE approval_id = 'a-pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(decided_by.is_none());
    }

    /// Exercises migration 10: `runs.transcript_cursor` (the durable TUI
    /// transcript-tailer position, WP12) does not exist before the
    /// migration and exists (nullable) after it.
    #[test]
    fn migration_10_adds_transcript_cursor_column() {
        let mut conn = Connection::open_in_memory().expect("in-memory db");

        migration_list()
            .to_version(&mut conn, 9)
            .expect("migrate to v9");

        let err = conn.query_row("SELECT transcript_cursor FROM runs LIMIT 1", [], |r| {
            r.get::<_, Option<String>>(0)
        });
        assert!(
            err.is_err(),
            "transcript_cursor must not exist before migration 10"
        );

        migration_list()
            .to_version(&mut conn, 10)
            .expect("migrate to v10");

        conn.execute(
            "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
             VALUES ('wp-1', 'sha256:fake', 'fake', 'test', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
             VALUES ('t-1', 'p-1', 'omp-1', 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO workers (worker_id, project_id, profile_id, created_at)
             VALUES ('w-1', 'p-1', 'wp-1', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (run_id, task_id, worker_id, state, flags_degraded_control, flags_needs_reconciliation,
               flags_protocol_unhealthy, flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
               vendor_session_id, created_at)
             VALUES ('r-1', 't-1', 'w-1', 'queued', 0, 0, 0, 0, 0, 0, NULL, '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();

        // A freshly inserted row has no cursor yet (never backfilled with
        // a fabricated one).
        let cursor: Option<String> = conn
            .query_row(
                "SELECT transcript_cursor FROM runs WHERE run_id = 'r-1'",
                [],
                |r| r.get(0),
            )
            .expect("column exists after migration 10");
        assert!(cursor.is_none());

        conn.execute(
            "UPDATE runs SET transcript_cursor = ?1 WHERE run_id = 'r-1'",
            rusqlite::params!["{\"offset\":42,\"lastEntryId\":\"abc\"}"],
        )
        .expect("transcript_cursor is writable after migration 10");

        let cursor: Option<String> = conn
            .query_row(
                "SELECT transcript_cursor FROM runs WHERE run_id = 'r-1'",
                [],
                |r| r.get(0),
            )
            .expect("read back");
        assert_eq!(
            cursor.as_deref(),
            Some("{\"offset\":42,\"lastEntryId\":\"abc\"}")
        );
    }
}
