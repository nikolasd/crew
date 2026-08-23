//! Workspace lease arbitration.

use batman_protocol::{IsolationKind, LeaseMode, ProjectId, RunId, WorkspaceInfo, WorkspaceState};
use rusqlite::params;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

/// How long a lease may legitimately sit in `allocating` before it is
/// treated as abandoned. Materialization is a `git worktree add` or a
/// bounded tree copy, so a row still `allocating` long after this belongs
/// to a call that died between [`LeaseService::acquire`] and
/// [`LeaseService::activate`] (crash, `SIGKILL`) rather than one still
/// working -- [`LeaseService::stale`] surfaces it to `crewd doctor` even
/// though its `path` is still empty and therefore invisible to the
/// missing-path check below it.
pub const ALLOCATING_LEASE_GRACE: Duration = Duration::minutes(10);

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("database error: {0}")]
    Db(String),
    #[error("lease not found: {lease_id}")]
    NotFound { lease_id: String },
    /// A read-only shared request was refused because a shared **write**
    /// lease is blocking (any lease still `allocating`, `active`, or
    /// `cleanupFailed` without a `released_at`). This is the only arm that
    /// raises `Conflict`; a contending shared *write* request gets
    /// [`LeaseError::IsolationRequired`] instead. There is deliberately no
    /// same-run guard: a single run may hold multiple concurrent leases,
    /// e.g. a read-only view alongside an isolated write worktree, by
    /// design.
    #[error("conflict: another lease exists for this project")]
    Conflict,
    /// A *shared* write lease was refused because another shared lease is
    /// already active. Unlike [`LeaseError::Conflict`] this is caller-
    /// correctable: requesting `gitWorktree` or `copy` isolation creates an
    /// independent workspace that never contends for the repository root.
    #[error(
        "isolation required: a shared write lease is already active; request gitWorktree or copy isolation"
    )]
    IsolationRequired,
    #[error("lease already released: {lease_id}")]
    AlreadyReleased { lease_id: String },
}

#[derive(Debug, Clone)]
pub struct CreatedLease {
    pub lease_id: String,
    pub run_id: RunId,
    pub mode: LeaseMode,
    pub path: String,
    pub isolation_kind: IsolationKind,
    pub base_revision: String,
    pub state: WorkspaceState,
    pub acquisition_sequence: u64,
}

pub struct LeaseService {
    db_path: std::path::PathBuf,
}

impl LeaseService {
    pub fn open_in_memory(_project_id: ProjectId) -> Result<Self, LeaseError> {
        let db_path = std::env::temp_dir().join(format!("crew-lease-{}.db", Uuid::now_v7()));
        Self::open(_project_id, &db_path)
    }

    pub fn open(_project_id: ProjectId, db_path: &std::path::Path) -> Result<Self, LeaseError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| LeaseError::Db(e.to_string()))?;
        }

        let conn =
            rusqlite::Connection::open(db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspace_leases (
            lease_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, mode TEXT NOT NULL,
            isolation_kind TEXT NOT NULL DEFAULT 'shared', path TEXT NOT NULL,
            base_revision TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'active',
            acquired_at TEXT NOT NULL, acquisition_sequence INTEGER NOT NULL DEFAULT 0,
            released_at TEXT
        )",
        )
        .map_err(|e| LeaseError::Db(e.to_string()))?;

        let _ = conn.close();

        Ok(LeaseService {
            db_path: db_path.to_path_buf(),
        })
    }

    pub fn acquire(
        &self,
        run_id: RunId,
        mode: LeaseMode,
        requested_isolation: Option<IsolationKind>,
    ) -> Result<CreatedLease, LeaseError> {
        let conn =
            rusqlite::Connection::open(&self.db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        conn.execute("BEGIN IMMEDIATE", params![])
            .map_err(|e| LeaseError::Db(e.to_string()))?;

        let isolation_kind = requested_isolation.unwrap_or(IsolationKind::Shared);

        // Conflict rules: only Shared isolation is exclusive within a project.
        // GitWorktree and Copy create independent workspaces that never conflict.
        //
        // A row that reached `cleanupFailed` because `release()` itself
        // failed never had `released_at` set (`release()` only reaches its
        // `UPDATE ... released_at` statement on success, and a row that was
        // already `released` before failing gets there via the
        // `AlreadyReleased` guard, which does set it) -- the underlying
        // claim was never actually relinquished, so `released_at IS NULL`
        // must still count as blocking here. A `cleanupFailed` row whose
        // release succeeded (only its disk teardown failed) has
        // `released_at` set and is correctly excluded.
        if isolation_kind == IsolationKind::Shared {
            // Write mode is exclusive within shared: blocks any other shared lease
            if mode == LeaseMode::Write {
                let shared_active: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM workspace_leases
                         WHERE isolation_kind = 'shared'
                           AND (state IN ('allocating', 'active')
                                OR (state = 'cleanupFailed' AND released_at IS NULL))",
                        params![],
                        |row| row.get(0),
                    )
                    .map_err(|e| LeaseError::Db(e.to_string()))?;
                if shared_active > 0 {
                    let _ = conn.execute("ROLLBACK", params![]);
                    return Err(LeaseError::IsolationRequired);
                }
            }

            // ReadOnly mode is blocked by an active shared Write lease
            if mode == LeaseMode::ReadOnly {
                let shared_write: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM workspace_leases
                         WHERE isolation_kind = 'shared' AND mode = 'write'
                           AND (state IN ('allocating', 'active')
                                OR (state = 'cleanupFailed' AND released_at IS NULL))",
                        params![],
                        |row| row.get(0),
                    )
                    .map_err(|e| LeaseError::Db(e.to_string()))?;
                if shared_write > 0 {
                    let _ = conn.execute("ROLLBACK", params![]);
                    return Err(LeaseError::Conflict);
                }
            }
        }

        let lease_id = Uuid::now_v7().to_string();
        let now: OffsetDateTime = OffsetDateTime::now_utc();
        let now_str = now
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| LeaseError::Db(format!("time format: {}", e)))?;

        // Two-phase: INSERT in 'allocating' with empty path.
        // Caller materializes the workspace, then calls activate() with the real path.
        conn.execute(
            "INSERT INTO workspace_leases (lease_id, run_id, mode, isolation_kind, path, base_revision, state, acquired_at, acquisition_sequence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &lease_id,
                &run_id.to_string(),
                match mode { LeaseMode::ReadOnly => "readOnly", LeaseMode::Write => "write" },
                match isolation_kind {
                    IsolationKind::Shared => "shared",
                    IsolationKind::GitWorktree => "gitWorktree",
                    IsolationKind::Copy => "copy",
                },
                "",
                "HEAD",
                "allocating",
                now_str,
                1u64,
            ],
        ).map_err(|e| LeaseError::Db(e.to_string()))?;

        conn.execute("COMMIT", params![])
            .map_err(|e| LeaseError::Db(e.to_string()))?;

        Ok(CreatedLease {
            lease_id,
            run_id,
            mode,
            path: String::new(),
            isolation_kind,
            base_revision: "HEAD".to_string(),
            state: WorkspaceState::Allocating,
            acquisition_sequence: 1,
        })
    }

    /// Transitions a lease from `allocating` to `active` with the real
    /// workspace path, after the caller has materialized the workspace.
    pub fn activate(&self, lease_id: String, path: String) -> Result<(), LeaseError> {
        let conn =
            rusqlite::Connection::open(&self.db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        let rows_affected = conn
            .execute(
                "UPDATE workspace_leases SET state = 'active', path = ?1
                 WHERE lease_id = ?2 AND state = 'allocating'",
                params![path, lease_id],
            )
            .map_err(|e| LeaseError::Db(e.to_string()))?;

        let _ = conn.close();

        if rows_affected == 0 {
            return Err(LeaseError::NotFound { lease_id });
        }
        Ok(())
    }

    /// Returns workspace info for an active lease bound to `run_id`.
    /// Used by OMP to discover a peer agent's workspace path.
    pub fn active_for_run(&self, run_id: RunId) -> Result<Option<WorkspaceInfo>, LeaseError> {
        let conn =
            rusqlite::Connection::open(&self.db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        // "No rows" is a real, expected outcome (`None`); every other
        // rusqlite error (locked/corrupted DB, schema mismatch) must
        // surface instead of masquerading as "no lease" (R62).
        use rusqlite::OptionalExtension;
        let result: Option<(String, String, String, String, String, String)> = conn
            .query_row(
                "SELECT run_id, mode, isolation_kind, path, state, base_revision
                 FROM workspace_leases WHERE run_id = ?1 AND state = 'active'",
                params![run_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| LeaseError::Db(e.to_string()))?;

        let _ = conn.close();

        Ok(result.map(
            |(run_id_str, mode_str, isol_kind, path, _state, base_rev)| {
                let mode = match mode_str.as_str() {
                    "readOnly" => LeaseMode::ReadOnly,
                    "write" => LeaseMode::Write,
                    _ => LeaseMode::ReadOnly,
                };
                WorkspaceInfo {
                    lease_id: String::new(),
                    run_id: run_id_from_str(&run_id_str).unwrap_or(run_id),
                    mode,
                    isolation_kind: match isol_kind.as_str() {
                        "shared" => IsolationKind::Shared,
                        "gitWorktree" => IsolationKind::GitWorktree,
                        "copy" => IsolationKind::Copy,
                        _ => IsolationKind::Shared,
                    },
                    path,
                    state: WorkspaceState::Active,
                    base_revision: base_rev,
                }
            },
        ))
    }

    /// Returns `(lease_id, state)` for every lease that a healthy runtime
    /// should not have: `allocating`/`active` leases whose materialized
    /// path no longer exists on disk, every `cleanupFailed` lease, and
    /// every `allocating` lease that has sat past [`ALLOCATING_LEASE_GRACE`]
    /// without being activated or released.
    ///
    /// `allocating` leases legitimately have an empty path until
    /// [`Self::activate`] runs, so an empty path alone is never counted as
    /// missing -- only a non-empty path pointing at nothing is. Without the
    /// grace-period check, a row stuck in `allocating` by a caller that
    /// crashed between [`Self::acquire`] and [`Self::activate`] would have
    /// an empty path forever and never surface here.
    ///
    /// # Errors
    /// Returns [`LeaseError::Db`] if the lease database cannot be read or
    /// the grace-period cutoff cannot be computed.
    pub fn stale(&self) -> Result<Vec<(String, String)>, LeaseError> {
        let conn =
            rusqlite::Connection::open(&self.db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        let cutoff = OffsetDateTime::now_utc()
            .checked_sub(ALLOCATING_LEASE_GRACE)
            .ok_or_else(|| {
                LeaseError::Db("allocating-lease grace exceeds representable time".to_string())
            })?
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| LeaseError::Db(format!("time format: {}", e)))?;

        let mut stmt = conn
            .prepare(
                "SELECT lease_id, state, path, acquired_at FROM workspace_leases
                 WHERE state IN ('allocating', 'active', 'cleanupFailed')",
            )
            .map_err(|e| LeaseError::Db(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| LeaseError::Db(e.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| LeaseError::Db(e.to_string()))?;

        Ok(rows
            .into_iter()
            .filter(|(_, state, path, acquired_at)| {
                state == "cleanupFailed"
                    || (!path.is_empty() && !std::path::Path::new(path).exists())
                    || (state == "allocating" && *acquired_at < cutoff)
            })
            .map(|(lease_id, state, _, _)| (lease_id, state))
            .collect())
    }

    pub fn get(&self, lease_id: String) -> Result<WorkspaceInfo, LeaseError> {
        let conn =
            rusqlite::Connection::open(&self.db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        let (run_id_str, mode_str, isol_kind, path, state, base_rev): (String, String, String, String, String, String) = conn.query_row(
            "SELECT run_id, mode, isolation_kind, path, state, base_revision FROM workspace_leases WHERE lease_id = ?1",
            params![&lease_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).map_err(|e| match e {
            // An unknown lease id is the typed NotFound, never a generic
            // Db error -- callers classify NotFound as a caller error (R84).
            rusqlite::Error::QueryReturnedNoRows => LeaseError::NotFound {
                lease_id: lease_id.clone(),
            },
            other => LeaseError::Db(other.to_string()),
        })?;

        let mode = match mode_str.as_str() {
            "readOnly" => LeaseMode::ReadOnly,
            "write" => LeaseMode::Write,
            _ => {
                return Err(LeaseError::NotFound {
                    lease_id: lease_id.clone(),
                });
            }
        };

        let state = match state.as_str() {
            "allocating" => WorkspaceState::Allocating,
            "active" => WorkspaceState::Active,
            "dirty" => WorkspaceState::Dirty,
            "released" => WorkspaceState::Released,
            "cleanupFailed" => WorkspaceState::CleanupFailed,
            _ => {
                return Err(LeaseError::NotFound {
                    lease_id: lease_id.clone(),
                });
            }
        };

        let _ = conn.close();

        Ok(WorkspaceInfo {
            lease_id,
            run_id: run_id_from_str(&run_id_str)?,
            mode,
            isolation_kind: match isol_kind.as_str() {
                "shared" => IsolationKind::Shared,
                "gitWorktree" => IsolationKind::GitWorktree,
                "copy" => IsolationKind::Copy,
                _ => IsolationKind::Shared,
            },
            path,
            state,
            base_revision: base_rev,
        })
    }

    pub fn release(&self, lease_id: String) -> Result<(), LeaseError> {
        let conn =
            rusqlite::Connection::open(&self.db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        let state: String = conn
            .query_row(
                "SELECT state FROM workspace_leases WHERE lease_id = ?1",
                params![&lease_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => LeaseError::NotFound {
                    lease_id: lease_id.clone(),
                },
                other => LeaseError::Db(other.to_string()),
            })?;

        if state == "released" {
            return Err(LeaseError::AlreadyReleased { lease_id });
        }

        let now: OffsetDateTime = OffsetDateTime::now_utc();
        let now_str = now
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|e| LeaseError::Db(format!("time format: {}", e)))?;

        conn.execute(
            "UPDATE workspace_leases SET state = 'released', released_at = ?1 WHERE lease_id = ?2",
            params![now_str, lease_id],
        )
        .map_err(|e| LeaseError::Db(e.to_string()))?;

        let _ = conn.close();

        Ok(())
    }

    /// Marks a released lease's materialized directory as un-removable.
    ///
    /// A teardown failure never invalidates the release itself -- the lease
    /// is genuinely gone and the repository is free -- so this records the
    /// leaked directory for the doctor's stale-workspace check instead of
    /// failing the caller's RPC.
    ///
    /// # Errors
    /// Returns [`LeaseError::Db`] if the update fails.
    pub fn mark_cleanup_failed(&self, lease_id: String) -> Result<(), LeaseError> {
        let conn =
            rusqlite::Connection::open(&self.db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        conn.execute(
            "UPDATE workspace_leases SET state = 'cleanupFailed' WHERE lease_id = ?1",
            params![lease_id],
        )
        .map_err(|e| LeaseError::Db(e.to_string()))?;

        let _ = conn.close();

        Ok(())
    }

    /// Counts leases that still hold a claim on the repository: `allocating`
    /// or `active` rows, plus a `cleanupFailed` row whose `release()` call
    /// itself never succeeded (`released_at IS NULL`) -- see the comment in
    /// [`Self::acquire`] for why that case must still count as held.
    pub fn active_for_repository(&self) -> Result<u64, LeaseError> {
        let conn =
            rusqlite::Connection::open(&self.db_path).map_err(|e| LeaseError::Db(e.to_string()))?;

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM workspace_leases
                 WHERE state IN ('allocating', 'active')
                    OR (state = 'cleanupFailed' AND released_at IS NULL)",
                [],
                |row| row.get(0),
            )
            .map_err(|e| LeaseError::Db(e.to_string()))?;

        let _ = conn.close();

        Ok(count as u64)
    }
}

fn run_id_from_str(s: &str) -> Result<RunId, LeaseError> {
    RunId::parse(s).map_err(|_| LeaseError::NotFound {
        lease_id: s.to_string(),
    })
}
