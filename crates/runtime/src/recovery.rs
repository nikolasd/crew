//! Crash recovery for the Crew runtime.
//!
//! After an unclean shutdown (crash, OOM kill, SIGKILL), runs may be left in
//! non-terminal states (`queued`, `starting`, `working`, `waitingUser`,
//! `waitingPeer`, `paused`). The [`RecoveryCoordinator`] finds these stuck
//! runs and transitions each to an appropriate terminal state:
//! `queued`/`starting`/`working` to `failed` (no evidence the work ever
//! completed), and `waitingUser`/`waitingPeer`/`paused` to `cancelled` when
//! the corresponding [`RecoveryConfig`] flag opts in (these runs are
//! intentionally waiting on a human/peer, so recovering them by default
//! would cancel valid work).
//!
//! [`crate::lifecycle::serve`] runs the sweep once, synchronously, after
//! opening the database but before the socket accepts any connection. The
//! sweep decides by **ownership, not age**: `serve` holds the single-instance
//! lock and the adapter registry starts with an empty running map, so every
//! non-terminal run visible at that moment provably has no live supervisor
//! behind it -- however recent its last event. An age threshold here (the
//! pre-fix `stuck_threshold`) could only hide the most common real crash, in
//! which a supervisor restarts the daemon seconds after the death (R51).
//!
//! There is deliberately no periodic re-sweep: no adapter emits a heartbeat,
//! so while a daemon is alive a run can be silent for minutes without being
//! dead, and a time-based sweep would fail runs that are merely quiet.
//! [`DEFAULT_STALE_RUN_THRESHOLD`] exists for the doctor's read-only
//! `stale_runs` report -- the live-daemon counterpart -- and for nothing
//! else.

use std::sync::Arc;
use std::time::Duration;

use batman_protocol::{ProjectId, RunId, RunState, WorkerId};
use thiserror::Error;

use crate::db::{DatabaseHandle, DomainClosure};
use crate::domain::DomainRepository;

/// Errors that can occur during crash recovery.
#[derive(Debug, Error)]
pub enum RecoveryError {
    /// The database handle is invalid or closed.
    #[error("database handle is invalid: {0}")]
    InvalidDatabase(String),

    /// A run could not be transitioned to a terminal state.
    #[error("failed to transition run {run_id} from {from_state} to {to_state}: {reason}")]
    TransitionFailed {
        run_id: String,
        from_state: String,
        to_state: String,
        reason: String,
    },

    /// No runs were found that needed recovery.
    #[error("no runs found that needed recovery")]
    NoRunsToRecover,
}

/// How long a run must be silent before the doctor's read-only `stale_runs`
/// check names it. Never used to recover anything: the startup sweep decides
/// by ownership (nothing can be live at boot), and no periodic sweep exists,
/// because no adapter emits a heartbeat -- a time-based sweep against a live
/// daemon would fail runs that are merely quiet.
pub const DEFAULT_STALE_RUN_THRESHOLD: Duration = Duration::from_secs(300);

/// Which non-terminal runs a stuck-run query returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepScope {
    /// Every non-terminal run in the project, however recent its last event.
    /// Sound only at startup: no run visible then can have a live supervisor.
    EveryNonTerminal,
    /// Only runs whose last activity predates the given silence threshold --
    /// the live-daemon reading, used by the doctor's passive report.
    StaleBeyond(Duration),
}

/// Configuration for crash recovery.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecoveryConfig {
    /// Whether to recover runs in `paused` state. Paused runs are intentionally
    /// waiting for user input, so recovering them would cancel valid work.
    pub recover_paused: bool,

    /// Whether to recover runs in `waitingUser` or `waitingPeer` state. These
    /// runs are waiting for approval, so recovering them would cancel valid work.
    pub recover_waiting: bool,
}

/// Result of a recovery operation.
#[derive(Debug, Clone)]
pub struct RecoveryResult {
    /// The number of runs that were recovered.
    pub recovered_count: usize,

    /// The runs that were recovered, with their previous and new states.
    pub recovered_runs: Vec<RecoveredRun>,
}

/// A single run that was recovered.
#[derive(Debug, Clone)]
pub struct RecoveredRun {
    /// The run's unique identifier.
    pub run_id: RunId,

    /// The run's worker identifier.
    pub worker_id: WorkerId,

    /// The run's previous state before recovery.
    pub previous_state: RunState,

    /// The run's new state after recovery.
    pub new_state: RunState,

    /// The RFC 3339 timestamp of the run's last activity before recovery.
    pub last_activity: String,

    /// Whether the recovery was successful.
    pub success: bool,

    /// An optional error message if recovery failed.
    pub error: Option<String>,
}

/// Coordinates crash recovery for the Crew runtime.
///
/// The [`RecoveryCoordinator`] finds runs that are stuck in non-terminal states
/// after an unclean shutdown and transitions them to appropriate terminal states.
pub struct RecoveryCoordinator {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    config: RecoveryConfig,
}

impl RecoveryCoordinator {
    /// Creates a new [`RecoveryCoordinator`] with the given database handle and
    /// configuration.
    #[must_use]
    pub fn new(db: Arc<DatabaseHandle>, project_id: ProjectId, config: RecoveryConfig) -> Self {
        Self {
            db,
            project_id,
            config,
        }
    }

    /// Creates a [`RecoveryCoordinator`] with default configuration.
    #[must_use]
    pub fn with_defaults(db: Arc<DatabaseHandle>, project_id: ProjectId) -> Self {
        Self::new(db, project_id, RecoveryConfig::default())
    }

    /// Performs crash recovery on all runs in the database.
    ///
    /// This method:
    /// 1. Finds every run in a non-terminal state, with no age filter -- see
    ///    the module header for why ownership, not age, is the sound test at
    ///    startup
    /// 2. Transitions each to an appropriate terminal state based on its
    ///    current state and the recovery configuration
    ///
    /// Each stuck run is recovered independently -- one run's transition
    /// failure never aborts the sweep for the others; it is recorded on
    /// that run's own [`RecoveredRun::success`]/[`RecoveredRun::error`].
    ///
    /// # Errors
    ///
    /// Returns a [`RecoveryError`] only if finding stuck runs itself fails
    /// (the database handle is invalid, or a stored row is corrupt).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use batman_protocol::ProjectId;
    /// # use batman_runtime::db::DatabaseHandle;
    /// # use batman_runtime::recovery::RecoveryCoordinator;
    /// # async fn example(db: Arc<DatabaseHandle>, project_id: ProjectId) -> Result<(), Box<dyn std::error::Error>> {
    /// let coordinator = RecoveryCoordinator::with_defaults(db, project_id);
    /// let result = coordinator.recover().await?;
    /// println!("Recovered {} runs", result.recovered_count);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn recover(&self) -> Result<RecoveryResult, RecoveryError> {
        let stuck_runs = self.find_stuck_runs(SweepScope::EveryNonTerminal).await?;
        let mut recovered_runs = Vec::with_capacity(stuck_runs.len());
        for stuck in &stuck_runs {
            match self.recover_run(stuck).await {
                Ok(new_state) => recovered_runs.push(RecoveredRun {
                    run_id: stuck.run_id,
                    worker_id: stuck.worker_id,
                    previous_state: stuck.current_state.clone(),
                    new_state,
                    last_activity: stuck.last_activity.clone(),
                    success: true,
                    error: None,
                }),
                Err(err) => recovered_runs.push(RecoveredRun {
                    run_id: stuck.run_id,
                    worker_id: stuck.worker_id,
                    previous_state: stuck.current_state.clone(),
                    new_state: stuck.current_state.clone(),
                    last_activity: stuck.last_activity.clone(),
                    success: false,
                    error: Some(err.to_string()),
                }),
            }
        }
        let recovered_count = recovered_runs.iter().filter(|r| r.success).count();
        Ok(RecoveryResult {
            recovered_count,
            recovered_runs,
        })
    }

    /// Finds all runs that are stuck in non-terminal states.
    ///
    /// A run is considered stuck if:
    /// - It's in a non-terminal state (`queued`, `starting`, `working`,
    ///   `waitingUser`, `waitingPeer`, `paused`)
    /// - It is included per `scope`: every one at startup, or -- for the
    ///   doctor's passive report -- only those whose last activity (its most
    ///   recent journaled event, or its `created_at` if it has none) predates
    ///   the bound threshold
    /// - If [`RecoveryConfig::recover_paused`] is `false`, runs in `paused`
    ///   state are excluded
    /// - If [`RecoveryConfig::recover_waiting`] is `false`, runs in
    ///   `waitingUser` or `waitingPeer` state are excluded
    pub(crate) async fn find_stuck_runs(
        &self,
        scope: SweepScope,
    ) -> Result<Vec<StuckRun>, RecoveryError> {
        let cutoff: Option<String> = match scope {
            SweepScope::EveryNonTerminal => None,
            SweepScope::StaleBeyond(threshold) => {
                let cutoff = time::OffsetDateTime::now_utc()
                    .checked_sub(time::Duration::seconds(
                        i64::try_from(threshold.as_secs()).unwrap_or(i64::MAX),
                    ))
                    .ok_or_else(|| {
                        RecoveryError::InvalidDatabase(
                            "stale threshold exceeds representable time".to_string(),
                        )
                    })?
                    .format(&time::format_description::well_known::Rfc3339)
                    .map_err(|e| {
                        RecoveryError::InvalidDatabase(format!(
                            "failed to format cutoff timestamp: {e}"
                        ))
                    })?;
                Some(cutoff)
            }
        };

        let project_id = self.project_id;
        let cutoff_param = cutoff.clone();
        let closure: DomainClosure = Box::new(move |conn| {
            let rows: Vec<(String, String, String, String)> = match &cutoff_param {
                Some(cutoff) => {
                    let mut stmt =
                        conn.prepare(&format!("{STUCK_RUN_SELECT}{STALE_ONLY_PREDICATE}"))?;
                    stmt.query_map(
                        rusqlite::params![project_id.to_string(), cutoff],
                        map_stuck_row,
                    )?
                    .collect::<Result<Vec<_>, _>>()?
                }
                None => {
                    let mut stmt = conn.prepare(STUCK_RUN_SELECT)?;
                    stmt.query_map(rusqlite::params![project_id.to_string()], map_stuck_row)?
                        .collect::<Result<Vec<_>, _>>()?
                }
            };
            Ok(serde_json::to_value(rows)?)
        });

        let value = self
            .db
            .run_domain_op(closure)
            .await
            .map_err(|e| RecoveryError::InvalidDatabase(e.to_string()))?;
        let rows: Vec<(String, String, String, String)> =
            serde_json::from_value(value).map_err(|e| {
                RecoveryError::InvalidDatabase(format!("malformed stuck-run rows: {e}"))
            })?;

        let mut stuck = Vec::new();
        for (run_id_str, state_str, worker_id_str, last_activity) in rows {
            let run_id = RunId::parse(&run_id_str).map_err(|e| {
                RecoveryError::InvalidDatabase(format!("invalid run id {run_id_str}: {e}"))
            })?;
            let worker_id = WorkerId::parse(&worker_id_str).map_err(|e| {
                RecoveryError::InvalidDatabase(format!("invalid worker id {worker_id_str}: {e}"))
            })?;
            let current_state = RunState::try_from(state_str.as_str()).map_err(|e| {
                RecoveryError::InvalidDatabase(format!("invalid run state {state_str}: {e}"))
            })?;
            if current_state.is_terminal() {
                // Excluded here (rather than in SQL) so the single source of
                // truth for "which states are terminal" stays
                // `RunState::is_terminal()` -- never a second, driftable copy.
                continue;
            }

            let eligible = match current_state.to_string().as_str() {
                "paused" => self.config.recover_paused,
                "waitingUser" | "waitingPeer" => self.config.recover_waiting,
                _ => true,
            };
            if !eligible {
                continue;
            }

            stuck.push(StuckRun {
                run_id,
                current_state,
                worker_id,
                last_activity,
            });
        }
        Ok(stuck)
    }

    /// Recovers a single stuck run by transitioning it to an appropriate
    /// terminal state.
    ///
    /// The target state is determined by the run's current state:
    /// - `queued` → `failed`
    /// - `starting` → `failed`
    /// - `working` → `failed`
    /// - `waitingUser` → `cancelled` (if [`RecoveryConfig::recover_waiting`] is `true`)
    /// - `waitingPeer` → `cancelled` (if [`RecoveryConfig::recover_waiting`] is `true`)
    /// - `paused` → `cancelled` (if [`RecoveryConfig::recover_paused`] is `true`)
    ///
    /// `find_stuck_runs` already applies the `recover_paused`/`recover_waiting`
    /// gate, so every [`StuckRun`] reaching this method has a defined target.
    async fn recover_run(&self, stuck_run: &StuckRun) -> Result<RunState, RecoveryError> {
        let target = target_state_for(&stuck_run.current_state).ok_or_else(|| {
            RecoveryError::TransitionFailed {
                run_id: stuck_run.run_id.to_string(),
                from_state: stuck_run.current_state.to_string(),
                to_state: "<none>".to_string(),
                reason: "no recovery target is defined for this state".to_string(),
            }
        })?;

        let run_id = stuck_run.run_id;
        let project_id = self.project_id;
        let target_for_closure = target.clone();
        let closure: DomainClosure = Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.transition_run(run_id, &target_for_closure, None)
                .map(|c| serde_json::json!({ "sequence": c.sequence }))
        });

        self.db
            .run_domain_op(closure)
            .await
            .map_err(|err| RecoveryError::TransitionFailed {
                run_id: run_id.to_string(),
                from_state: stuck_run.current_state.to_string(),
                to_state: target.to_string(),
                reason: err.to_string(),
            })?;

        Ok(target)
    }
}

/// The terminal state a stuck run in `current` should recover to, or `None`
/// if `current` (already terminal, or an unrecognized state) has no
/// recovery target.
fn target_state_for(current: &RunState) -> Option<RunState> {
    match current.to_string().as_str() {
        "queued" | "starting" | "working" => RunState::try_from("failed").ok(),
        "waitingUser" | "waitingPeer" | "paused" => RunState::try_from("cancelled").ok(),
        _ => None,
    }
}

/// The projection both sweep scopes share: one row per run in this project,
/// with its last activity (most recent journaled event, falling back to
/// `created_at`). Terminal states are filtered in Rust against
/// `RunState::is_terminal()`, never in SQL.
const STUCK_RUN_SELECT: &str = "SELECT r.run_id, r.state, r.worker_id,
                COALESCE((SELECT MAX(e.timestamp) FROM events e WHERE e.run_id = r.run_id), r.created_at)
         FROM runs r
         JOIN tasks t ON r.task_id = t.task_id
         WHERE t.project_id = ?1";

/// Appended for `SweepScope::StaleBeyond`: restricts the projection to runs
/// whose last activity predates the bound cutoff (`?2`).
const STALE_ONLY_PREDICATE: &str = "
           AND COALESCE((SELECT MAX(e2.timestamp) FROM events e2 WHERE e2.run_id = r.run_id), r.created_at) < ?2";

fn map_stuck_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, String)> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, String>(3)?,
    ))
}

/// A run that is stuck in a non-terminal state.
pub(crate) struct StuckRun {
    /// The run's unique identifier.
    pub(crate) run_id: RunId,

    /// The run's current state.
    pub(crate) current_state: RunState,

    /// The run's worker identifier.
    pub(crate) worker_id: WorkerId,

    /// The RFC 3339 timestamp of the run's last activity (its most recent
    /// journaled event, or its creation time if it has none).
    pub(crate) last_activity: String,
}
