//! Crash recovery for the Crew runtime.
//!
//! After an unclean shutdown (crash, OOM kill, SIGKILL), runs may be left in
//! non-terminal states (`queued`, `starting`, `working`, `waitingUser`,
//! `waitingPeer`, `paused`). Since WP15 the startup sweep is *resume first*:
//! before any terminal fallback it tries to continue each stuck run on the
//! vendor session its previous incarnation already established, through
//! [`ResumeSeam`]'s [`crate::adapter::AdapterRegistry::resume_run`]. A run
//! that resumes keeps its prior state and its same identity -- a resume is a
//! continuation of the SAME run, never a retry that would fabricate a new
//! one -- while a run that cannot resume falls back to this module's
//! original terminalize path: `queued`/`starting`/`working` to `failed`, and
//! `waitingUser`/`waitingPeer`/`paused` to `cancelled` when the
//! corresponding [`RecoveryConfig`] flag opts in. The attempt itself is
//! always journaled: `resume_attempted` before anything is decided, then
//! `resume_succeeded` on a continuation or `resume_failed` -- journaled
//! BEFORE the failed terminal edge -- with the reason.
//!
//! Eligibility is decided per run before anything spawns: the run's resolved
//! worker profile names the adapter kind and mode; a missing
//! `vendor_session_id`, an unavailable adapter, a headless kind whose
//! declared capabilities claim no resumption, or a TUI-mode run whose
//! deterministic transcript path (`transcript_root/<session-id>.jsonl`) does
//! not exist all make the run ineligible, and ineligible means the exact
//! same fallback as a failed resume.
//!
//! `waitingUser`/`waitingPeer`/`paused` runs are resume candidates too --
//! they hold live vendor sessions more often than crashed ones -- but the
//! conservative default survives on the fallback side only: such a run whose
//! resume fails is left untouched (never terminalized), exactly as before.
//!
//! Resume does NOT violate the ownership-not-age reasoning below: the sweep
//! still runs at the one moment every visible non-terminal run provably has
//! no live supervisor, and the spawn a resume performs is owned by the
//! daemon doing the resuming -- `resume_run` builds the fresh adapter inside
//! this process's own registry, reserving a slot in its running map, so the
//! continued vendor session is supervised by the new daemon from its first
//! resumed byte. Nothing reaches back into the dead incarnation.
//!
//! [`crate::lifecycle::serve`] runs the sweep once, synchronously, after
//! opening the database and supplying both post-construction registry
//! supports (`set_tui_support` before IPC bind, `set_resume_support` after
//! it -- `resume_run` fails closed without the latter) but still before the
//! socket accepts any connection, so no live run can race it. The sweep
//! decides by **ownership, not age**: `serve` holds the single-instance
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

use crew_protocol::{DiagnosticLevel, EventEnvelope, ProjectId, RunId, RunState, WorkerId};
use thiserror::Error;

use crate::adapter::registry::requested_mode;
use crate::adapter::tui::Cursor;
use crate::adapter::{AdapterMode, AdapterRegistry, VendorSessionRef, WorkerProfile};
use crate::db::{DatabaseHandle, DomainClosure};
use crate::domain::{DomainRepository, broadcast_committed, embed_envelope};

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

/// The resume-first seam (WP15): an [`AdapterRegistry`] whose
/// `set_tui_support`/`set_resume_support` have both been supplied, plus the
/// live event broadcast every journaled sweep mutation fans out on.
/// Constructing a coordinator with one (via
/// [`RecoveryCoordinator::with_resume`]) turns `recover` into the
/// resume-first sweep; without one, `recover` keeps the pre-WP15
/// terminalize-only behavior exactly.
pub struct ResumeSeam {
    pub(crate) registry: Arc<AdapterRegistry>,
    events_tx: tokio::sync::broadcast::Sender<EventEnvelope>,
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

    /// How this sweep's involvement with the run ended -- a continuation on
    /// its prior vendor session, a terminal transition, or the conservative
    /// leave-untouched skip.
    pub outcome: RecoveredOutcome,
}

/// What the sweep ultimately did with one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredOutcome {
    /// The run was resumed on its prior vendor session; it continues in its
    /// prior (non-terminal) state under this daemon's own adapter.
    Resumed,
    /// The run could not be resumed and was transitioned to a terminal
    /// state (the pre-WP15 fallback).
    Terminalized,
    /// The run could not be resumed but was left untouched -- the
    /// conservative default for `waitingUser`/`waitingPeer`/`paused`.
    LeftUntouched,
}

/// Coordinates crash recovery for the Crew runtime.
///
/// The [`RecoveryCoordinator`] finds runs that are stuck in non-terminal states
/// after an unclean shutdown and transitions them to appropriate terminal states.
pub struct RecoveryCoordinator {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    config: RecoveryConfig,
    /// The resume-first seam. `None` keeps the pre-WP15 behavior exactly;
    /// `Some` makes every non-terminal run a resume candidate first.
    resume: Option<ResumeSeam>,
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
            resume: None,
        }
    }

    /// Creates a resume-first [`RecoveryCoordinator`]: every non-terminal
    /// run becomes a resume candidate before the terminalize fallback.
    ///
    /// The registry must already have had BOTH post-construction supports
    /// supplied -- `set_tui_support` and `set_resume_support` -- because
    /// `resume_run` fails closed (`RegistryError::ResumeUnsupported`)
    /// without the latter, and a `mode: "tui"` run cannot even have its
    /// transcript eligibility checked without the former. This ordering is
    /// why [`crate::lifecycle::serve`] runs the sweep after IPC bind (which
    /// is what makes the resume support's server-owned pieces exist) but
    /// still before it accepts any connection.
    #[must_use]
    pub fn with_resume(
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        config: RecoveryConfig,
        registry: Arc<AdapterRegistry>,
        events_tx: tokio::sync::broadcast::Sender<EventEnvelope>,
    ) -> Self {
        Self {
            db,
            project_id,
            config,
            resume: Some(ResumeSeam {
                registry,
                events_tx,
            }),
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
    /// 2. With a [`ResumeSeam`] wired (the production boot path since WP15),
    ///    attempts to resume each run on its prior vendor session FIRST; a
    ///    success leaves the run non-terminal in its prior state, and only a
    ///    failed or ineligible resume falls through to --
    /// 3. -- the original terminalize fallback, keyed on the run's current
    ///    state and the recovery configuration
    ///
    /// Each stuck run is recovered independently -- one run's transition
    /// failure never aborts the sweep for the others; it is recorded on
    /// that run's own [`RecoveredRun::success`]/[`RecoveredRun::error`].
    ///
    /// The sweep is idempotent across boots: a run this process already
    /// drives an adapter for is by definition not stuck, and a run whose most
    /// recent journaled resume outcome is a failure was already decided by a
    /// previous sweep (its fallback decision -- possibly the conservative
    /// leave-non-terminal skip -- stands), so neither is ever re-journaled.
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
    /// # use crew_protocol::ProjectId;
    /// # use crew_runtime::db::DatabaseHandle;
    /// # use crew_runtime::recovery::RecoveryCoordinator;
    /// # async fn example(db: Arc<DatabaseHandle>, project_id: ProjectId) -> Result<(), Box<dyn std::error::Error>> {
    /// let coordinator = RecoveryCoordinator::with_defaults(db, project_id);
    /// let result = coordinator.recover().await?;
    /// println!("Recovered {} runs", result.recovered_count);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn recover(&self) -> Result<RecoveryResult, RecoveryError> {
        let stuck_runs = match &self.resume {
            // Resume-first: `waitingUser`/`waitingPeer`/`paused` runs are
            // candidates too, so the `recover_paused`/`recover_waiting`
            // flags -- which gate only the terminalize fallback below --
            // must not filter candidates out up front.
            Some(_) => {
                self.query_stuck_runs(SweepScope::EveryNonTerminal, false)
                    .await?
            }
            None => {
                self.query_stuck_runs(SweepScope::EveryNonTerminal, true)
                    .await?
            }
        };
        let mut recovered_runs = Vec::with_capacity(stuck_runs.len());
        for stuck in &stuck_runs {
            if let Some(seam) = &self.resume {
                // A run this process already holds an adapter for is not
                // stuck -- an earlier sweep in this same boot resumed it.
                if seam.registry.running_adapter(stuck.run_id).is_some() {
                    continue;
                }
                // And a run whose latest journaled resume outcome is a
                // failure was already handled by a previous sweep; deciding
                // again could only double-journal.
                if self.previous_sweep_failed_resume(stuck.run_id).await? {
                    continue;
                }
            }
            recovered_runs.push(match &self.resume {
                Some(_) => self.recover_run_resume_first(stuck).await,
                None => self.recover_run_without_seam(stuck).await,
            });
        }
        let recovered_count = recovered_runs.iter().filter(|r| r.success).count();
        Ok(RecoveryResult {
            recovered_count,
            recovered_runs,
        })
    }
    /// Finds all runs that are stuck in non-terminal states, applying the
    /// [`RecoveryConfig`] `paused`/`waiting` gate.
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
        self.query_stuck_runs(scope, true).await
    }

    /// The shared projection behind both sweep scopes. `apply_config_gate`
    /// controls whether the `recover_paused`/`recover_waiting` flags exclude
    /// waiting/paused runs from the result: `true` for the pre-WP15
    /// terminalize-only sweep and the doctor's report, `false` for the
    /// resume-first sweep -- a waiting run is a resume *candidate* even when
    /// its fallback would be skipped, so it must not be filtered here.
    async fn query_stuck_runs(
        &self,
        scope: SweepScope,
        apply_config_gate: bool,
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
            let rows: Vec<(String, String, String, String, String)> = match &cutoff_param {
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
        let rows: Vec<(String, String, String, String, String)> = serde_json::from_value(value)
            .map_err(|e| {
                RecoveryError::InvalidDatabase(format!("malformed stuck-run rows: {e}"))
            })?;

        let mut stuck = Vec::new();
        for (run_id_str, task_id_str, state_str, worker_id_str, last_activity) in rows {
            let run_id = RunId::parse(&run_id_str).map_err(|e| {
                RecoveryError::InvalidDatabase(format!("invalid run id {run_id_str}: {e}"))
            })?;
            let task_id = crew_protocol::TaskId::parse(&task_id_str).map_err(|e| {
                RecoveryError::InvalidDatabase(format!("invalid task id {task_id_str}: {e}"))
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

            if apply_config_gate {
                let eligible = match current_state.to_string().as_str() {
                    "paused" => self.config.recover_paused,
                    "waitingUser" | "waitingPeer" => self.config.recover_waiting,
                    _ => true,
                };
                if !eligible {
                    continue;
                }
            }

            stuck.push(StuckRun {
                run_id,
                task_id,
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
    /// Callers apply the `recover_paused`/`recover_waiting` gate themselves
    /// (up front in the seam-less sweep, or only after a failed resume in
    /// the resume-first one), so every [`StuckRun`] reaching this method has
    /// a defined target.
    async fn terminalize(&self, stuck_run: &StuckRun) -> Result<RunState, RecoveryError> {
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
                .map(|c| embed_envelope(serde_json::json!({ "sequence": c.sequence }), &c.envelope))
        });

        let mut value = match self.db.run_domain_op(closure).await {
            Ok(value) => value,
            Err(err) => {
                // The resume attempt's own failure path (an adapter
                // fail_start) may already have transitioned the run to the
                // terminal target while it unwound. The recovery goal --
                // the run is terminal -- is then already achieved; treat
                // the re-transition as satisfied instead of failing on the
                // illegal working->failed-after-failed edge.
                let run_id_string = run_id.to_string();
                let current = self
                    .db
                    .run_domain_op(Box::new(move |conn| {
                        let state: String = conn.query_row(
                            "SELECT state FROM runs WHERE run_id = ?1",
                            [&run_id_string],
                            |row| row.get(0),
                        )?;
                        Ok(serde_json::json!({ "state": state }))
                    }))
                    .await
                    .map_err(|read_err| RecoveryError::TransitionFailed {
                        run_id: run_id.to_string(),
                        from_state: stuck_run.current_state.to_string(),
                        to_state: target.to_string(),
                        reason: format!("{err}; state re-read also failed: {read_err}"),
                    })?;
                let current = current["state"].as_str().unwrap_or_default();
                if current == target.to_string() {
                    serde_json::json!({ "sequence": null, "alreadyTerminal": true })
                } else {
                    return Err(RecoveryError::TransitionFailed {
                        run_id: run_id.to_string(),
                        from_state: stuck_run.current_state.to_string(),
                        to_state: target.to_string(),
                        reason: format!("{err}; run is now {current}, not the expected {target}"),
                    });
                }
            }
        };
        // A domain mutation commits its event AND broadcasts the same
        // envelope. The seam-less path has no broadcast to fan out on and
        // keeps its historical no-broadcast behavior exactly.
        if let Some(seam) = &self.resume {
            broadcast_committed(&seam.events_tx, &mut value);
        }

        Ok(target)
    }

    /// One stuck run under the pre-WP15 (seam-less) behavior: straight to
    /// the terminalize fallback, unchanged.
    async fn recover_run_without_seam(&self, stuck: &StuckRun) -> RecoveredRun {
        match self.terminalize(stuck).await {
            Ok(new_state) => RecoveredRun {
                run_id: stuck.run_id,
                worker_id: stuck.worker_id,
                previous_state: stuck.current_state.clone(),
                new_state,
                last_activity: stuck.last_activity.clone(),
                success: true,
                error: None,
                outcome: RecoveredOutcome::Terminalized,
            },
            Err(err) => failed_entry(stuck, err.to_string(), RecoveredOutcome::LeftUntouched),
        }
    }

    /// WP15's resume-first handling of one stuck run: announce the attempt,
    /// decide eligibility, resume through the registry -- and only on a
    /// failed or ineligible resume fall back to the terminalize path.
    async fn recover_run_resume_first(&self, stuck: &StuckRun) -> RecoveredRun {
        // The attempt is journaled BEFORE anything is decided; every later
        // step's own outcome event follows it.
        if let Err(err) = self
            .journal_resume_event(
                stuck.run_id,
                "resume_attempted",
                format!(
                    "startup recovery is attempting to resume this {} run on its prior vendor session",
                    stuck.current_state
                ),
            )
            .await
        {
            return failed_entry(
                stuck,
                format!("could not journal resume_attempted: {err}"),
                RecoveredOutcome::LeftUntouched,
            );
        }

        let candidate = match self.evaluate_resume_eligibility(stuck).await {
            Ok(candidate) => candidate,
            Err(reason) => return self.resume_failed_fallback(stuck, reason).await,
        };

        let seam = self
            .resume
            .as_ref()
            .expect("resume-first requires a ResumeSeam");
        match seam
            .registry
            .resume_run(stuck.run_id, candidate.session, candidate.cursor)
            .await
        {
            Ok(()) => {
                let error = match self.journal_resume_event(stuck.run_id, "resume_succeeded",
                    "startup recovery resumed this run on its prior vendor session; it continues in its prior state".to_string(),
                ).await {
                    Ok(()) => None,
                    Err(err) => Some(format!("resumed, but journaling resume_succeeded failed: {err}")),
                };
                RecoveredRun {
                    run_id: stuck.run_id,
                    worker_id: stuck.worker_id,
                    previous_state: stuck.current_state.clone(),
                    new_state: stuck.current_state.clone(),
                    last_activity: stuck.last_activity.clone(),
                    success: true,
                    error,
                    outcome: RecoveredOutcome::Resumed,
                }
            }
            Err(err) => self.resume_failed_fallback(stuck, err).await,
        }
    }

    /// Journals `resume_failed` (always BEFORE any failed terminal edge),
    /// then applies the original fallback: the existing terminalize path --
    /// except that a `waitingUser`/`waitingPeer`/`paused` run keeps today's
    /// conservative default and is left untouched rather than terminalized.
    async fn resume_failed_fallback(&self, stuck: &StuckRun, reason: String) -> RecoveredRun {
        if let Err(err) = self
            .journal_resume_event(stuck.run_id, "resume_failed", reason.clone())
            .await
        {
            return failed_entry(
                stuck,
                format!("could not journal resume_failed: {err}"),
                RecoveredOutcome::LeftUntouched,
            );
        }

        let gated_out = match stuck.current_state.to_string().as_str() {
            "paused" => !self.config.recover_paused,
            "waitingUser" | "waitingPeer" => !self.config.recover_waiting,
            _ => false,
        };
        if gated_out || target_state_for(&stuck.current_state).is_none() {
            return failed_entry(stuck, reason, RecoveredOutcome::LeftUntouched);
        }

        match self.terminalize(stuck).await {
            Ok(new_state) => RecoveredRun {
                run_id: stuck.run_id,
                worker_id: stuck.worker_id,
                previous_state: stuck.current_state.clone(),
                new_state,
                last_activity: stuck.last_activity.clone(),
                success: true,
                // The run is safely terminal, but the resume attempt did
                // not succeed -- surface WHY so operators (and CI) see the
                // root cause, not just the fallback outcome.
                error: Some(reason),
                outcome: RecoveredOutcome::Terminalized,
            },
            Err(err) => failed_entry(
                stuck,
                format!("resume failed ({reason}); then terminalization also failed: {err}"),
                RecoveredOutcome::LeftUntouched,
            ),
        }
    }

    /// Decides whether `stuck` may be resumed at all, before anything can
    /// spawn. Every `Err` is an eligibility verdict with its reason -- it
    /// becomes the journaled `resume_failed` message, never a silent skip.
    #[allow(clippy::type_complexity)]
    async fn evaluate_resume_eligibility(
        &self,
        stuck: &StuckRun,
    ) -> Result<ResumeCandidate, String> {
        let (vendor_session_id, transcript_cursor_json, resolved_profile_json) = self
            .read_resume_facts(stuck.run_id)
            .await
            .map_err(|e| format!("this daemon cannot read this run's resume state: {e}"))?;

        let Some(session) = vendor_session_id.filter(|s| !s.trim().is_empty()) else {
            return Err("no vendor session was ever established for this run".to_string());
        };
        // The cursor column holds opaque JSON of a TUI `Cursor` (WP12); an
        // unreadable one fails closed into the fallback rather than risking a
        // fresh-tail duplicate replay.
        let cursor = transcript_cursor_json
            .as_deref()
            .map(|json| {
                serde_json::from_str::<Cursor>(json)
                    .map_err(|e| format!("the stored transcript cursor is unreadable: {e}"))
            })
            .transpose()?;

        let profile_json = resolved_profile_json.ok_or_else(|| {
            "this daemon cannot build this run's adapter: its worker has no resolved profile"
                .to_string()
        })?;
        let profile: WorkerProfile = serde_json::from_str(&profile_json)
            .map_err(|e| format!("the resolved worker profile is unreadable: {e}"))?;
        // No kind at all means StartupOptions::TerminalDegraded.
        let kind = profile.adapter_kind().ok_or_else(|| {
            "terminal-degraded runs declare no resumable vendor session".to_string()
        })?;

        match requested_mode(&profile.startup_options) {
            Some(AdapterMode::Tui) => {
                // Adapter availability for a TUI run is concrete here: the
                // registry must be able to derive the deterministic
                // transcript path (`transcript_root/<session-id>.jsonl`) --
                // which needs both a supplied TuiSupport and this kind's
                // adapter entry -- and the file must actually exist.
                let session_ref = VendorSessionRef(session.clone());
                let path = self.resume.as_ref().and_then(|seam| {
                    seam.registry.tui_transcript_path_for_session(
                        kind,
                        stuck.run_id,
                        stuck.task_id,
                        stuck.worker_id,
                        &session_ref,
                    )
                });
                let Some(path) = path else {
                    return Err(format!("adapter {kind} has no TUI support in this daemon"));
                };
                if !path.exists() {
                    return Err(format!("transcript {} does not exist", path.display()));
                }
            }
            Some(AdapterMode::Headless) => {
                // crew-v2 gap-closure WP-C: the headless control plane is
                // retired (spec §4.6) -- there is no adapter implementation
                // left to even ask "does it declare session resumption".
                // Reject here, at the FIRST point recovery inspects this
                // run's mode, with the same honest reason `gate_profile`
                // gives a live submit -- never the confusing downstream
                // symptom a pre-WP-C build would have produced instead (a
                // "profile unreadable" from a since-deleted capability
                // lookup, or a Claude-shaped transcript-path failure from
                // treating a headless-mode run as if it were the TUI
                // continuation it never claimed to be).
                return Err(format!(
                    "adapter {kind} was requested with mode: \"headless\", which is retired in \
                     crew v2 (spec §4.6) -- the headless control plane has no adapter \
                     implementation to dispatch to; use mode: \"tui\""
                ));
            }
            None => {
                return Err(
                    "terminal-degraded runs declare no resumable vendor session".to_string()
                );
            }
        }

        Ok(ResumeCandidate {
            session: VendorSessionRef(session),
            cursor,
        })
    }

    /// Reads one run's stored resume seam: its `vendor_session_id`, its
    /// `transcript_cursor`, and its worker's resolved profile JSON (which
    /// names the adapter kind and mode).
    async fn read_resume_facts(
        &self,
        run_id: RunId,
    ) -> Result<(Option<String>, Option<String>, Option<String>), RecoveryError> {
        let run_id_string = run_id.to_string();
        let value = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let row: (Option<String>, Option<String>, Option<String>) = conn.query_row(
                    "SELECT r.vendor_session_id, r.transcript_cursor, w.resolved_profile_json \
                     FROM runs r JOIN workers w ON r.worker_id = w.worker_id \
                     WHERE r.run_id = ?1",
                    [&run_id_string],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                Ok(serde_json::json!({
                    "vendorSessionId": row.0,
                    "transcriptCursor": row.1,
                    "resolvedProfileJson": row.2,
                }))
            }))
            .await
            .map_err(|e| RecoveryError::InvalidDatabase(e.to_string()))?;
        let field = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        };
        Ok((
            field("vendorSessionId"),
            field("transcriptCursor"),
            field("resolvedProfileJson"),
        ))
    }

    /// Journals one sweep diagnostic (`resume_attempted`, `resume_succeeded`,
    /// or `resume_failed`) through the domain repository and broadcasts the
    /// very envelope it committed -- the same commit-and-broadcast-equal rule
    /// every other domain mutation follows.
    async fn journal_resume_event(
        &self,
        run_id: RunId,
        code: &str,
        message: String,
    ) -> Result<(), RecoveryError> {
        let Some(seam) = &self.resume else {
            return Ok(());
        };
        let project_id = self.project_id;
        let code = code.to_string();
        let mut value = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_diagnostic(run_id, DiagnosticLevel::Info, code, message)
                    .map(|c| {
                        embed_envelope(serde_json::json!({ "sequence": c.sequence }), &c.envelope)
                    })
            }))
            .await
            .map_err(|e| RecoveryError::InvalidDatabase(e.to_string()))?;
        broadcast_committed(&seam.events_tx, &mut value);
        Ok(())
    }

    /// Whether this run's most recent journaled resume outcome is a
    /// `resume_failed` -- i.e. some previous sweep already decided this
    /// run's fallback (possibly leaving it non-terminal via the conservative
    /// skip), so deciding again could only double-journal. An attempt whose
    /// decision was never journaled (a crash between the two events) still
    /// counts as undecided and may retry.
    async fn previous_sweep_failed_resume(&self, run_id: RunId) -> Result<bool, RecoveryError> {
        let run_id_string = run_id.to_string();
        let value = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let last_of_code = |code: &str| -> rusqlite::Result<Option<i64>> {
                    conn.query_row(
                        "SELECT MAX(sequence) FROM events \
                         WHERE run_id = ?1 AND event_json LIKE ?2",
                        rusqlite::params![run_id_string, format!("%\"code\":\"{code}\"%")],
                        |row| row.get(0),
                    )
                };
                let attempted = last_of_code("resume_attempted")?;
                let succeeded = last_of_code("resume_succeeded")?;
                let failed = last_of_code("resume_failed")?;
                Ok(serde_json::json!({
                    "attempted": attempted,
                    "succeeded": succeeded,
                    "failed": failed,
                }))
            }))
            .await
            .map_err(|e| RecoveryError::InvalidDatabase(e.to_string()))?;
        let seq = |key: &str| value.get(key).and_then(serde_json::Value::as_i64);
        let attempted = seq("attempted");
        let succeeded = seq("succeeded");
        Ok(match seq("failed") {
            Some(failed) => failed >= attempted.unwrap_or(0) && failed >= succeeded.unwrap_or(0),
            None => false,
        })
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
const STUCK_RUN_SELECT: &str = "SELECT r.run_id, r.task_id, r.state, r.worker_id,
                COALESCE((SELECT MAX(e.timestamp) FROM events e WHERE e.run_id = r.run_id), r.created_at)
         FROM runs r
         JOIN tasks t ON r.task_id = t.task_id
         WHERE t.project_id = ?1";

/// Appended for `SweepScope::StaleBeyond`: restricts the projection to runs
/// whose last activity predates the bound cutoff (`?2`).
const STALE_ONLY_PREDICATE: &str = "
           AND COALESCE((SELECT MAX(e2.timestamp) FROM events e2 WHERE e2.run_id = r.run_id), r.created_at) < ?2";

fn map_stuck_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, String, String, String, String)> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, String>(3)?,
        row.get::<_, String>(4)?,
    ))
}

/// A run that is stuck in a non-terminal state.
pub(crate) struct StuckRun {
    /// The run's unique identifier.
    pub(crate) run_id: RunId,

    /// The run's task identifier (needed to derive a TUI run's transcript
    /// path the same way the resumed adapter itself will).
    pub(crate) task_id: crew_protocol::TaskId,

    /// The run's current state.
    pub(crate) current_state: RunState,

    /// The run's worker identifier.
    pub(crate) worker_id: WorkerId,

    /// The RFC 3339 timestamp of the run's last activity (its most recent
    /// journaled event, or its creation time if it has none).
    pub(crate) last_activity: String,
}

/// One run's resume decision inputs, once eligibility has been established:
/// the vendor session to continue and the transcript position to re-tail
/// from (`None` when nothing was ever durably consumed).
struct ResumeCandidate {
    session: VendorSessionRef,
    cursor: Option<Cursor>,
}

/// A [`RecoveredRun`] recording that this sweep left the run exactly as it
/// found it, with the reason.
fn failed_entry(stuck: &StuckRun, error: String, outcome: RecoveredOutcome) -> RecoveredRun {
    RecoveredRun {
        run_id: stuck.run_id,
        worker_id: stuck.worker_id,
        previous_state: stuck.current_state.clone(),
        new_state: stuck.current_state.clone(),
        last_activity: stuck.last_activity.clone(),
        success: false,
        error: Some(error),
        outcome,
    }
}
