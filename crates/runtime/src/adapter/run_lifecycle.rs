//! Applies the durable `RunState` edges the adapter layer has evidence for.
//!
//! Wraps a run's [`AdapterEventSink`] and, *after* the inner sink has
//! journaled each event, commits (and broadcasts) the lifecycle edge that
//! event is evidence of:
//!
//! | evidence | edge |
//! |---|---|
//! | `ProcessStarted` | `queued -> starting` |
//! | `TurnEnded` | up to `waitingUser` (non-terminal -- ADR-0027) |
//! | any other payload except `ProcessExited`/`TurnEnded` | up to `working` |
//! | `ProcessExited` with a signal, or a non-zero code | `-> failed` |
//! | `ProcessExited` with exit 0 and no turn ever settled | `-> failed` |
//! | `ProcessExited` with exit 0 after a settled turn (no `run/finish`) | `-> RunState::unrendered_verdict()` (`cancelled`) |
//! | `ProcessExited` with no code and no signal | `-> lost` |
//!
//! A TUI vendor process exiting cleanly is never itself evidence of
//! success -- only the leader's own `run/finish` call judges that
//! (`OrchestrationService::run_finish`'s doc comment). The ruling's exact
//! condition is load-bearing (`release/live-conformance/
//! 2026-09-08-live-e2e-attempt-3.md:296`): "a cleanly exited run that **did
//! no work** is `failed`". Whether it did work is the run's own
//! `turnSettled` flag (ADR-0027's `observe_turn_ended`), not its current
//! state -- `waitingUser` is also reachable through ADR-0012's approval
//! flow with no turn ever settled, and that case is still `failed`. A run
//! whose turn genuinely settled and then saw a bare exit with no
//! `run/finish` did real work with no verdict rendered on it, which is
//! neither `succeeded` nor `failed` -- see `RunState::unrendered_verdict`'s
//! own doc comment. `lost` stays reserved for an exit the supervisor could
//! not observe at all.
//!
//! Four properties this shape depends on:
//!
//! * **Edges are walked, never jumped.** Codex emits no `ProcessStarted` at
//!   all (its `spawn_client` observes the pid but journals nothing), and
//!   `queued -> working`, `starting -> succeeded`, `waitingUser -> succeeded`
//!   are all illegal in `RunState::can_transition_to`. So a target is reached
//!   by committing each legal hop in turn, which is also what keeps
//!   `runs.started_at` correct: `DomainRepository::transition_run` stamps it
//!   only on the `starting` edge.
//! * **Forward only.** `working` is applied only from `queued`/`starting`, so
//!   vendor output arriving while a run sits in `waitingUser` (an approval) or
//!   `paused` never clobbers that state.
//! * **A terminal state always wins.** A run cancelled through `run/cancel`
//!   (which commits `cancelled` before killing the process) is already
//!   terminal when its exit arrives; every walk stops on a terminal state, and
//!   `transition_run` itself rejects the edge and appends nothing even if a
//!   concurrent commit wins the race.
//! * **No edge without durable evidence.** A failed inner `emit` (the
//!   sanitize/journal/broadcast step never actually committed anything) never
//!   reaches any `observe_*` call: `RunLifecycleSink::emit` gates every
//!   lifecycle observation on the inner sink's `Result` being `Ok`, so a run
//!   never advances on evidence that was never actually journaled.
//!
//! The terminal edge lives here rather than in `registry::watch_settlement`
//! so it is durable *before* `SettlementSink` fires the settlement signal that
//! releases the run's concurrency slot: no other run can be authorized while
//! this one still reads non-terminal. See ADR-0023 for the mapping and for why
//! an unobservable exit is `lost` rather than a guessed `succeeded`/`failed`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crew_protocol::{EventEnvelope, ProjectId, Redacted, RunId, RunState};
use serde_json::json;
use tokio::sync::broadcast;

use crate::db::DatabaseHandle;
use crate::domain::{DomainError, DomainRepository, embed_envelope, take_envelope};
use crate::service::query::run_state_op;

use super::AdapterFuture;
use super::activity::ActivityClock;
use super::event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink};

/// The evidence-driven lifecycle edges for one run. Each run's
/// [`RunLifecycleSink`] owns exactly one of these.
pub(crate) struct RunLifecycle {
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    events_tx: broadcast::Sender<EventEnvelope>,
    run_id: RunId,
}

impl RunLifecycle {
    /// Reads the run's currently-stored state. Returns `None` (with a
    /// warning) when the read fails or the stored value is not a known state --
    /// callers treat `None` as "cannot act on this event", never as a state.
    async fn current(&self) -> Option<RunState> {
        let value = match self.db.run_domain_op(run_state_op(self.run_id)).await {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    run_id = %self.run_id,
                    "failed to read run state for a lifecycle edge"
                );
                return None;
            }
        };
        let Some(stored) = value.get("state").and_then(serde_json::Value::as_str) else {
            tracing::warn!(run_id = %self.run_id, "run-state read returned no state");
            return None;
        };
        match RunState::try_from(stored) {
            Ok(state) => Some(state),
            Err(err) => {
                tracing::warn!(run_id = %self.run_id, %err, "stored run state is not a known state");
                None
            }
        }
    }

    /// Commits one `transition_run` edge, embedding its `EventEnvelope` in the
    /// domain result and broadcasting it to live `events/subscribe` listeners --
    /// the same commit-then-broadcast sequence every `OrchestrationService`
    /// mutation uses (a mutation that appends without broadcasting silently
    /// breaks the monitor).
    async fn commit(&self, to: &RunState) -> Result<(), DomainError> {
        let project_id = self.project_id;
        let run_id = self.run_id;
        let to_owned = to.clone();
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.transition_run(run_id, &to_owned, None)
                    .map(|committed| {
                        embed_envelope(
                            json!({ "sequence": committed.sequence }),
                            &committed.envelope,
                        )
                    })
            }))
            .await?;
        if let Some(envelope) = take_envelope(&mut result) {
            let _ = self.events_tx.send(envelope);
        }
        Ok(())
    }

    /// Whether the run's `turnSettled` flag is currently set. `false` on a
    /// read failure, so a transient database error can never un-park a
    /// wait this sink has no positive evidence about.
    async fn turn_settled(&self) -> bool {
        let project_id = self.project_id;
        let run_id = self.run_id;
        self.db
            .run_domain_op(Box::new(move |conn| {
                DomainRepository::new(conn, project_id)
                    .read_run_flags(run_id)
                    .map(|flags| json!(flags.turn_settled))
            }))
            .await
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    /// Sets or clears the run's `turnSettled` flag, committing and
    /// broadcasting in the same call like every other mutation here
    /// (invariant 7).
    ///
    /// The flag is what lets a snapshot reader tell the two ways a run
    /// reaches `waitingUser` apart -- a finished turn versus a worker's
    /// question -- since the state alone cannot (ADR-0027).
    async fn set_turn_settled(&self, value: bool) {
        let project_id = self.project_id;
        let run_id = self.run_id;
        let outcome = self
            .db
            .run_domain_op(Box::new(move |conn| {
                DomainRepository::new(conn, project_id)
                    .set_run_flag(run_id, crate::domain::RunFlag::TurnSettled, value)
                    .map(|committed| {
                        embed_envelope(
                            json!({ "sequence": committed.sequence }),
                            &committed.envelope,
                        )
                    })
            }))
            .await;
        match outcome {
            Ok(mut result) => {
                if let Some(envelope) = take_envelope(&mut result) {
                    let _ = self.events_tx.send(envelope);
                }
            }
            // The edge itself is already durable; a failed flag write
            // costs the monitor its distinction, not correctness.
            Err(err) => tracing::warn!(
                error = %err,
                run_id = %run_id,
                value,
                "failed to write the turnSettled run flag"
            ),
        }
    }

    /// Commits the legal hops from the run's current state toward `target` --
    /// at most three, since `queued -> starting -> working -> terminal` is the
    /// longest legal path. Stops on a terminal state (a terminal state always
    /// wins) and gives up with a warning when the state cannot be read or no
    /// hop is legal.
    ///
    /// Returns whether **this call** is the one that committed the run into
    /// `target` -- `false` whenever nothing was actually applied here: the
    /// run was already at `target`, already some other terminal state (a
    /// terminal state never gets a second edge, regardless of what a caller
    /// asks for), the read failed, no legal hop existed, or the commit
    /// itself failed. A caller that gates a one-time side effect on reaching
    /// `target` (an escalation, say) must read this return value rather than
    /// assume the walk always lands where it aimed -- the target the caller
    /// computed can already be moot by the time this runs.
    async fn walk_to(&self, target: &RunState) -> bool {
        for _ in 0..3 {
            let Some(current) = self.current().await else {
                return false;
            };
            if current == *target || current.is_terminal() {
                // Already there, or already terminal as something else --
                // either way, not an edge this call is applying.
                return false;
            }
            let Some(next) = next_hop(&current, target) else {
                tracing::warn!(
                    run_id = %self.run_id,
                    from = %current,
                    to = %target,
                    "no legal run-state hop toward the target; giving up"
                );
                return false;
            };
            if let Err(err) = self.commit(&next).await {
                tracing::warn!(
                    error = %err,
                    run_id = %self.run_id,
                    from = %current,
                    to = %next,
                    "failed to commit run-state edge"
                );
                return false;
            }
            if next == *target {
                return true;
            }
        }
        tracing::warn!(
            run_id = %self.run_id,
            to = %target,
            "run-state walk exhausted without reaching the target"
        );
        false
    }

    /// `ProcessStarted` evidence: the vendor process is up. Only a run still
    /// in `queued` moves -- `queued -> starting` is the one legal edge here,
    /// and the one that stamps `runs.started_at`.
    pub(crate) async fn observe_process_started(&self) {
        let Some(current) = self.current().await else {
            return;
        };
        if current == state("queued")
            && let Err(err) = self.commit(&state("starting")).await
        {
            tracing::warn!(
                error = %err,
                run_id = %self.run_id,
                from = %current,
                to = "starting",
                "failed to move a queued run to starting"
            );
        }
    }

    /// Any non-exit payload (a vendor session, message, or tool activity): the
    /// run is doing work, provided it had not already moved past `working`.
    /// Returns `false` only when the current state could not be read, so the
    /// caller re-asks on the next event instead of giving up on a transient
    /// database error.
    ///
    /// This used to also un-park a turn-settled `waitingUser`
    /// on ANY vendor output, on the premise that the vendor producing
    /// anything meant the leader had steered it. That premise was never
    /// true: a vendor transcript's bookkeeping entries (session metadata,
    /// hook summaries, cost/turn-duration records -- none of them a new
    /// user turn) journal as ordinary evidence too, and each one un-parked
    /// the run right back out from under a leader who had not, in fact,
    /// said anything. Resumption is now caused, never inferred here: a
    /// delivered follow-up resumes the run directly at its own call site
    /// (`OrchestrationService::message_send`), and a genuine new
    /// user-authored transcript entry resumes it via
    /// [`Self::observe_real_user_turn`]. This method no longer touches
    /// `waitingUser` at all.
    pub(crate) async fn observe_vendor_activity(&self) -> bool {
        let Some(current) = self.current().await else {
            return false;
        };
        match current.to_string().as_str() {
            "queued" | "starting" => {
                self.walk_to(&state("working")).await;
                true
            }
            // At-or-past `working` (`waitingUser`, `waitingPeer`, `paused`)
            // or terminal: vendor output must never clobber those.
            _ => true,
        }
    }

    /// A genuine new user-authored transcript entry: the
    /// vendor's own transcript recorded a real follow-up, not the
    /// bookkeeping evidence `observe_vendor_activity` used to (wrongly)
    /// treat the same way. Narrower than that method ever was: the caller
    /// is responsible for having already excluded sidechain entries and
    /// tool-result-only content, so by the time this is called the entry
    /// really is a new turn a human or leader typed.
    ///
    /// Gated on the flag exactly like the deleted arm was, for the same
    /// reason: a `waitingUser` meaning "the worker asked a question" (or
    /// an approval wait, or `waitingPeer`/`paused`) must still never be
    /// clobbered by this.
    pub(crate) async fn observe_real_user_turn(&self) {
        if self.turn_settled().await {
            self.set_turn_settled(false).await;
            // Bypasses `walk_to`/`commit` deliberately -- a resume
            // is always exactly one hop (`waitingUser -> working`, legal
            // per `RunState::can_transition_to`), and this needs the
            // transition's own success/failure `Result` to decide whether
            // to journal a resume cause, which `walk_to` (built for a
            // multi-hop walk, shared with `observe_vendor_activity`/
            // `observe_process_started`, and returning `()`) throws away.
            // A future change to `walk_to` should be considered here too,
            // since this path no longer goes through it.
            self.commit_real_user_turn_resume().await;
        }
    }

    /// The single-hop transition + conditional resume-cause journal behind
    /// [`Self::observe_real_user_turn`]. Reads the pre-transition state in
    /// the SAME `run_domain_op` closure as the transition attempt (same
    /// actor turn, so no other closure can interleave between the two) --
    /// only a genuine `waitingUser -> working` edge is a resume caused by a
    /// real user turn; `RunResumed` must never be journaled for
    /// `starting -> working` or any other edge into `working`, nor for the
    /// race this call can lose against a delivered follow-up's own resume
    /// (`OrchestrationService::message_send`) -- see `RunResumed`'s own
    /// doc comment.
    async fn commit_real_user_turn_resume(&self) {
        let project_id = self.project_id;
        let run_id = self.run_id;
        let mut result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                let was_waiting_user = repo
                    .current_run_state(run_id)
                    .is_ok_and(|current| current == state("waitingUser"));
                repo.transition_run(run_id, &state("working"), None)
                    .map(|committed| {
                        embed_envelope(
                            json!({ "sequence": committed.sequence, "wasWaitingUser": was_waiting_user }),
                            &committed.envelope,
                        )
                    })
            }))
            .await;
        let resumed_from_waiting_user = match &mut result {
            Ok(value) => {
                let was_waiting_user = value
                    .get("wasWaitingUser")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if let Some(envelope) = take_envelope(value) {
                    let _ = self.events_tx.send(envelope);
                }
                was_waiting_user
            }
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    run_id = %self.run_id,
                    "a real user turn's own resume transition raced (benign -- the run was \
                     already working)"
                );
                false
            }
        };
        if !resumed_from_waiting_user {
            return;
        }
        // A separate, subsequent commit+broadcast, matching every
        // other lifecycle edge in this file: best-effort, since the
        // transition above is already durable and a failure recording
        // *why* costs the audit trail, never correctness.
        let mut cause_recorded = self
            .db
            .run_domain_op(Box::new(move |conn| {
                DomainRepository::new(conn, project_id)
                    .record_run_resumed(run_id, crew_protocol::ResumeCause::RealUserTurn)
                    .map(|committed| {
                        embed_envelope(
                            json!({ "sequence": committed.sequence }),
                            &committed.envelope,
                        )
                    })
            }))
            .await;
        match &mut cause_recorded {
            Ok(value) => {
                if let Some(envelope) = take_envelope(value) {
                    let _ = self.events_tx.send(envelope);
                }
            }
            Err(err) => tracing::warn!(
                error = %err,
                run_id = %self.run_id,
                "failed to journal a real user turn's resume cause"
            ),
        }
    }

    /// `TurnEnded` evidence: the vendor finished its turn and is holding at
    /// its prompt (ADR-0027).
    ///
    /// Deliberately **non-terminal**. A run is a conversation the leader
    /// closes, and a vendor turn boundary says only that the turn is over
    /// -- not that the task succeeded. `waitingUser` is the existing state
    /// meaning "the runtime is not the blocker", so the boundary hands the
    /// run back without claiming an outcome; settling it is `run/finish`'s
    /// job (or the inactivity backstop's).
    ///
    /// Forward-only and terminal-safe on the same terms as every other
    /// edge here: a run already waiting, paused, or terminal is left
    /// alone, so a boundary arriving after a cancel cannot resurrect it.
    pub(crate) async fn observe_turn_ended(&self) {
        let Some(current) = self.current().await else {
            return;
        };
        match current.to_string().as_str() {
            // A run that never got its `working` edge (codex journals no
            // `ProcessStarted`) still walks there hop-by-hop first.
            "queued" | "starting" | "working" => {
                self.walk_to(&state("waitingUser")).await;
                self.set_turn_settled(true).await;
            }
            // Already waiting on someone, paused, or terminal: a turn
            // boundary must not clobber any of those.
            _ => {}
        }
    }

    /// `ProcessExited` evidence: the vendor process is gone, so the run
    /// terminalizes. The walk keeps the hop-by-hop edges legal without
    /// touching the protocol's transition table (`queued -> lost` and
    /// `waitingUser -> succeeded` are not direct edges; `waitingUser ->
    /// cancelled` and `working -> cancelled` are, so `RunState::
    /// unrendered_verdict()` never needs one). Reads `turnSettled` first:
    /// it is the fact `terminal_state_for` branches on, not the
    /// run's current state, since `waitingUser` is also reachable with no
    /// turn ever settled (ADR-0012's approval flow).
    pub(crate) async fn observe_process_exited(
        &self,
        exit_code: Option<i32>,
        signal: Option<&str>,
    ) {
        let turn_settled = self.turn_settled().await;
        let terminal = terminal_state_for(exit_code, signal, turn_settled);
        // A repeated-failure escalation: a run that just failed for the
        // same task whose previous run also failed raises the leader's
        // attention fact. Committed as its own mutation and broadcast here
        // (invariant 7) -- never folded into the transition commit, since
        // escalation projection is not evidence for lifecycle edges.
        //
        // Gated on `walk_to`'s own return, not on the `terminal` value
        // this function computed: `terminal_state_for` only guesses what
        // *this* exit implies in isolation, and that guess can already be
        // moot by the time it runs -- the leader's own `run/finish` can
        // have settled the run to something else entirely (clearing
        // `turnSettled` as it does) between this exit landing on the wire
        // and this handler reading it. A run that is already terminal
        // never gets a second edge (`walk_to`'s own rule), so evaluating
        // the escalation against the computed guess instead of the edge
        // `walk_to` actually applied would raise `repeated_failure` on a
        // run this call never actually failed.
        //
        // A deliberate narrowing this gate also carries, decided rather
        // than merely inherited: `run/finish { outcome: "failed" }`
        // commits `failed` directly through its own path
        // (`OrchestrationService::run_finish`), never through this walk,
        // and never raises this escalation itself. Before this gate
        // existed, a `ProcessExited` racing in afterward could still
        // trigger it by accident, riding the same computed-guess bug this
        // change closes -- so a leader-adjudicated failure sometimes got
        // the notice and sometimes did not, depending on exit timing. Now
        // it never does, on any timing: a run the leader explicitly
        // failed already carries the leader's own verdict, which is a
        // materially different state of knowledge than the silent,
        // no-verdict-rendered death this escalation exists to catch (see
        // `terminal_state_for`'s own doc comment). Raising it there too
        // would mean threading this same task-history check into
        // `run_finish`, the run/submit start-error backstop, and the boot
        // recovery sweep -- every other place a run can reach `failed` --
        // which is a real feature expansion of this escalation's scope,
        // not a bug fix, and out of scope here.
        if self.walk_to(&terminal).await && terminal == state("failed") {
            self.raise_repeated_failure_if_second().await;
        }
    }

    /// Journals `EscalationRaised{reason: "repeated_failure"}` when this
    /// run's task has already seen another failed run. Best-effort: a
    /// failed detection is logged, never fatal -- the milestone digest
    /// already instructs the leader to ask the human.
    async fn raise_repeated_failure_if_second(&self) {
        let project_id = self.project_id;
        let run_id = self.run_id;
        let result = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                if repo.previous_run_for_task_also_failed(run_id) {
                    repo.record_escalation_raised(
                        run_id,
                        "repeated_failure",
                        Some(repeated_failure_question()),
                    )
                    .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
                } else {
                    Ok(json!(null))
                }
            }))
            .await;
        match result {
            Ok(mut value) => {
                crate::domain::broadcast_committed(&self.events_tx, &mut value);
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    run_id = %self.run_id,
                    "failed to record repeated-failure escalation"
                );
            }
        }
    }
}

/// The runtime-authored text for `EscalationRaised { reason:
/// "repeated_failure" }`'s `question` field -- the second call site that
/// actually populates this field (the first is `first_run_gate_question`
/// in `event_sink.rs`; the field used to be left `None` everywhere).
/// Only called from behind `previous_run_for_task_also_failed`, so by
/// construction this is never sent for a task's first failure.
fn repeated_failure_question() -> Redacted {
    Redacted::assert_runtime_authored(
        "This task's previous run also failed, and this run has now failed too -- two \
         consecutive failures on the same task. Read this run's output (and the one before \
         it) before deciding whether to retry with the same prompt or escalate to the user; \
         crew will not retry it for you.",
    )
}

/// Wraps a run's [`AdapterEventSink`] so the run's journaled evidence also
/// drives its durable [`RunState`].
pub struct RunLifecycleSink {
    inner: Arc<dyn AdapterEventSink>,
    lifecycle: RunLifecycle,
    /// Set once `observe_vendor_activity` has acted (or the run is already
    /// past `working`): a chatty run then pays no state read for the rest of
    /// its lifetime.
    working_observed: AtomicBool,
    /// The shared liveness clock: every journaled non-exit event is
    /// activity, re-arming the inactivity deadline the timeout sweep reads.
    activity: Arc<ActivityClock>,
}

impl RunLifecycleSink {
    /// Wraps `inner` so this run's journaled evidence also drives its
    /// durable `RunState`.
    #[must_use]
    pub fn wrap(
        inner: Arc<dyn AdapterEventSink>,
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
        run_id: RunId,
        activity: Arc<ActivityClock>,
    ) -> Arc<dyn AdapterEventSink> {
        Arc::new(Self {
            inner,
            lifecycle: RunLifecycle {
                db,
                project_id,
                events_tx,
                run_id,
            },
            working_observed: AtomicBool::new(false),
            activity,
        })
    }
}

impl AdapterEventSink for RunLifecycleSink {
    fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
        // Match by reference: `event` is moved into the inner `emit`
        // below, so a by-value match would consume the payload before
        // the inner sink needs it.
        let exit = match &event.payload {
            AdapterEventPayload::ProcessExited { exit_code, signal } => {
                Some((*exit_code, signal.clone()))
            }
            _ => None,
        };
        let process_started = matches!(&event.payload, AdapterEventPayload::ProcessStarted { .. });
        let turn_ended = matches!(&event.payload, AdapterEventPayload::TurnEnded { .. });
        let run_id = self.lifecycle.run_id;
        let activity = Arc::clone(&self.activity);
        Box::pin(async move {
            let result = self.inner.emit(event).await;
            if result.is_ok() {
                if let Some((exit_code, signal)) = exit {
                    // The run is settling: drop its liveness clock so a
                    // later resume starts fresh deadlines.
                    activity.forget(&run_id);
                    self.lifecycle
                        .observe_process_exited(exit_code, signal.as_deref())
                        .await;
                } else {
                    // Any other journaled evidence is vendor activity:
                    // re-arm the inactivity deadline before the lifecycle
                    // edge work so the sweep never sees a stale stamp.
                    activity.touch(&run_id, std::time::Instant::now());
                    if process_started {
                        self.lifecycle.observe_process_started().await;
                    } else if turn_ended {
                        // The boundary's own edge subsumes the `working`
                        // observation: it walks through `working` on its
                        // way to `waitingUser` when the run had not got
                        // there yet.
                        self.lifecycle.observe_turn_ended().await;
                        // Reopen the latch: the run is parked, not working,
                        // so the NEXT vendor event has to be evaluated
                        // again -- that is what un-parks a steered run.
                        // Leaving it set made a follow-up turn's output
                        // skip `observe_vendor_activity` entirely and
                        // stranded the run in `waitingUser`.
                        self.working_observed.store(false, Ordering::Relaxed);
                    } else if !self.working_observed.load(Ordering::Relaxed)
                        && self.lifecycle.observe_vendor_activity().await
                    {
                        self.working_observed.store(true, Ordering::Relaxed);
                    }
                }
            }
            result
        })
    }

    /// The narrow, caused resumption path. Unlike `emit`'s
    /// generic catch-all (deleted from `observe_vendor_activity` for this
    /// exact reason), this only ever fires when the caller has already
    /// confirmed a real user-authored turn -- so it un-parks unconditionally,
    /// with no `working_observed` latch to manage: it is not part of the
    /// ordinary vendor-activity stream this run's other evidence flows
    /// through, and nothing about a turn boundary should reset it.
    fn note_real_user_turn(&self, run_id: RunId) -> AdapterFuture<'_, ()> {
        debug_assert_eq!(run_id, self.lifecycle.run_id);
        Box::pin(async move {
            self.lifecycle.observe_real_user_turn().await;
            Ok(())
        })
    }
}

/// The one legal hop from `from` toward `target`: the target itself when the
/// edge is legal, otherwise the intermediate state the lifecycle table forces
/// (runs always pass through `starting`, then `working`, before a terminal
/// state).
fn next_hop(from: &RunState, target: &RunState) -> Option<RunState> {
    if from.can_transition_to(target) {
        return Some(target.clone());
    }
    match from.to_string().as_str() {
        "queued" => Some(state("starting")),
        "starting" | "waitingUser" | "waitingPeer" | "paused" => Some(state("working")),
        // Terminal (or unknown) states have no outgoing edges.
        _ => None,
    }
}

/// The terminal state an exit status is evidence of, given `turn_settled`
/// -- whether this run's turn had already settled (ADR-0027's
/// `observe_turn_ended`) with no `run/finish` verdict since. The
/// ruling is exact about the condition, not just the exit code
/// (`release/live-conformance/2026-09-08-live-e2e-attempt-3.md:296`, "a
/// cleanly exited run that **did no work** is `failed`"):
///
/// * a signalled death is always `failed` (the code is not trustworthy
///   once a signal is);
/// * a non-zero exit is always `failed`;
/// * a zero exit with no settled turn is `failed` -- the run did no work,
///   whether because it never got past starting (a start failure) or
///   because it parked at `waitingUser` some other way (ADR-0012's
///   approval flow) without ever finishing a turn;
/// * a zero exit AFTER a settled turn is `RunState::unrendered_verdict()`
///   -- the run did real work and produced a result, but nothing (no
///   `run/finish` call) ever rendered a verdict on it before the process
///   went away. Never a guessed `succeeded`: that judgment belongs solely
///   to the leader's own `run/finish` (ADR-0027);
/// * an exit whose status the supervisor could not observe at all is
///   `lost` -- ADR-0023 names that uncertainty rather than guessing, and
///   it is the ONLY case `lost` now covers: a `ProcessExited` with an
///   actual code or signal was, by definition, something the supervisor
///   observed.
fn terminal_state_for(
    exit_code: Option<i32>,
    signal: Option<&str>,
    turn_settled: bool,
) -> RunState {
    if signal.is_some() {
        return state("failed");
    }
    match exit_code {
        Some(0) if turn_settled => RunState::unrendered_verdict(),
        Some(_) => state("failed"),
        None => state("lost"),
    }
}

/// Constructs a [`RunState`] from one of the protocol's own table literals;
/// none of these can fail to parse.
fn state(name: &str) -> RunState {
    RunState::try_from(name).expect("run-state literal from the protocol's own table")
}
#[cfg(test)]
mod tests {
    use crew_protocol::{
        Classified, ContentClass, ProjectId, Run, RunFlags, RunId, RuntimeEvent, RuntimeEventKind,
        TaskId, TaskRef, Timestamp, Worker, WorkerId, WorkerProfileRef,
    };
    use tempfile::TempDir;

    use crate::adapter::AdapterError;
    use crate::adapter::event_sink::SettlementSink;

    use super::*;

    /// The inner sink these tests wrap: accepts every event, journals
    /// nothing, and resolves with the fixed sequence `0` -- so every state
    /// change these tests observe is the work of the lifecycle edge itself.
    struct StubSink;
    impl AdapterEventSink for StubSink {
        fn emit(&self, _event: AdapterEvent) -> AdapterFuture<'_, u64> {
            Box::pin(async { Ok(0) })
        }

        fn note_real_user_turn(&self, _run_id: RunId) -> AdapterFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// The inner sink `a_failed_inner_emit_never_applies_a_lifecycle_edge`
    /// wraps: every event fails as if the sanitize/journal/broadcast step
    /// never actually committed anything, so no evidence was ever durable.
    struct FailingSink;

    impl AdapterEventSink for FailingSink {
        fn emit(&self, _event: AdapterEvent) -> AdapterFuture<'_, u64> {
            Box::pin(async {
                Err(AdapterError::process(
                    "stub",
                    "emit",
                    "journal write failed",
                ))
            })
        }

        fn note_real_user_turn(&self, _run_id: RunId) -> AdapterFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A real, migrated database on a throwaway file: the same pattern
    /// `registry.rs`'s settlement tests use (per-test `TempDir`, explicit
    /// `shutdown` so the actor thread never outlives the test).
    async fn open_db() -> (TempDir, Arc<DatabaseHandle>) {
        let dir = tempfile::Builder::new()
            .prefix("bat-run-lifecycle-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            DatabaseHandle::start(db_path)
                .await
                .expect("start database"),
        );
        (dir, db)
    }

    /// Seeds one task + worker + `queued` run through the real
    /// `DomainRepository` API (copied from `tests/recovery.rs`'s
    /// `seed_run`), returning the identifiers the tests then drive.
    async fn seed_run(db: &DatabaseHandle, project_id: ProjectId) -> (TaskId, WorkerId, RunId) {
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
            repo.upsert_task(
                task_id,
                &TaskRef {
                    owner_client_instance_id: "omp-1".to_string(),
                    revision: 1,
                },
            )?;
            let worker = Worker {
                worker_id,
                profile_ref: WorkerProfileRef {
                    id: worker_id,
                    fingerprint: "sha256:fake".to_string(),
                    adapter: "fake".to_string(),
                    model: "test".to_string(),
                    permission_envelope: serde_json::json!({}),
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
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed run");
        (task_id, worker_id, run_id)
    }

    /// Seeds a second `queued` run under an already-seeded task/worker --
    /// the shape a real retry produces (one task, several runs over time),
    /// without re-registering the task or worker `seed_run` already did.
    async fn seed_second_run(
        db: &DatabaseHandle,
        project_id: ProjectId,
        task_id: TaskId,
        worker_id: WorkerId,
    ) -> RunId {
        let run_id = RunId::new();
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = DomainRepository::new(conn, project_id);
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
            Ok(serde_json::json!({}))
        }))
        .await
        .expect("seed second run");
        run_id
    }

    /// Drives `run_id` through the legal edges from `queued` up to `target`,
    /// directly through `DomainRepository` (bypassing the sink on purpose:
    /// the tests pin what the sink does from a given starting state).
    async fn drive_to_state(
        db: &DatabaseHandle,
        project_id: ProjectId,
        run_id: RunId,
        target: &str,
    ) {
        let path: &[&str] = match target {
            "working" => &["starting", "working"],
            "waitingUser" => &["starting", "working", "waitingUser"],
            "paused" => &["starting", "working", "paused"],
            "cancelled" => &["cancelled"],
            "succeeded" => &["starting", "working", "succeeded"],
            "failed" => &["starting", "working", "failed"],
            other => panic!("no drive path defined for {other}"),
        };
        for state in path {
            let to = RunState::try_from(*state).expect("valid state");
            db.run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.transition_run(run_id, &to, None)
                    .map(|_| serde_json::json!({}))
            }))
            .await
            .unwrap_or_else(|err| panic!("drive to {state} failed: {err}"));
        }
    }

    /// Reads a run's current projected state.
    async fn run_state(db: &DatabaseHandle, run_id: RunId) -> String {
        db.run_domain_op(Box::new(move |conn| {
            let state: String = conn.query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            Ok(serde_json::json!(state))
        }))
        .await
        .expect("read run state")
        .as_str()
        .expect("state is a string")
        .to_string()
    }

    /// Every journaled run-state event for `run_id`, in sequence order: the
    /// `state` each `RunEvent` recorded, so the exact walk the sink committed
    /// is readable back out of the durable journal.
    async fn run_states(db: &DatabaseHandle, run_id: RunId) -> Vec<String> {
        let raw: Vec<String> = db
            .run_domain_op(Box::new(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
                let rows: Vec<String> = stmt
                    .query_map([run_id.to_string()], |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok(serde_json::json!(rows))
            }))
            .await
            .expect("read journaled events")
            .as_array()
            .expect("rows are an array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        raw.into_iter()
            .filter_map(|raw| {
                let event: RuntimeEvent =
                    serde_json::from_str(&raw).expect("parse a journaled event");
                match event {
                    RuntimeEvent::RunEvent { state, .. } => Some(state),
                    _ => None,
                }
            })
            .collect()
    }

    /// Every `EscalationRaised.reason` journaled for `run_id`, in sequence
    /// order -- so a test can assert an escalation this sink might raise
    /// (repeated-failure, say) either did or did not actually fire, rather
    /// than inferring it from the run's state alone.
    async fn escalation_reasons(db: &DatabaseHandle, run_id: RunId) -> Vec<String> {
        let raw: Vec<String> = db
            .run_domain_op(Box::new(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
                let rows: Vec<String> = stmt
                    .query_map([run_id.to_string()], |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok(serde_json::json!(rows))
            }))
            .await
            .expect("read journaled events")
            .as_array()
            .expect("rows are an array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        raw.into_iter()
            .filter_map(|raw| {
                let event: RuntimeEvent =
                    serde_json::from_str(&raw).expect("parse a journaled event");
                match event {
                    RuntimeEvent::EscalationRaised { reason, .. } => Some(reason),
                    _ => None,
                }
            })
            .collect()
    }

    /// Every `EscalationRaised.question` journaled for `run_id`, in
    /// sequence order -- so a test can pin that a call site actually
    /// populates the field, previously left `None` everywhere, rather than
    /// only that an escalation fired at all.
    async fn escalation_questions(db: &DatabaseHandle, run_id: RunId) -> Vec<Option<String>> {
        let raw: Vec<String> = db
            .run_domain_op(Box::new(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
                let rows: Vec<String> = stmt
                    .query_map([run_id.to_string()], |row| row.get(0))?
                    .collect::<Result<_, _>>()?;
                Ok(serde_json::json!(rows))
            }))
            .await
            .expect("read journaled events")
            .as_array()
            .expect("rows are an array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        raw.into_iter()
            .filter_map(|raw| {
                let event: RuntimeEvent =
                    serde_json::from_str(&raw).expect("parse a journaled event");
                match event {
                    RuntimeEvent::EscalationRaised { question, .. } => {
                        Some(question.map(|q| q.as_str().to_string()))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// Whether `runs.started_at` / `runs.completed_at` is set.
    async fn run_timestamp_set(db: &DatabaseHandle, run_id: RunId, column: &'static str) -> bool {
        let value = db
            .run_domain_op(Box::new(move |conn| {
                let timestamp: Option<String> = conn
                    .query_row(
                        &format!("SELECT {column} FROM runs WHERE run_id = ?1"),
                        [run_id.to_string()],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                Ok(serde_json::json!(timestamp.is_some()))
            }))
            .await
            .expect("read run timestamp");
        value.as_bool().unwrap_or(false)
    }

    #[tokio::test]
    async fn process_started_moves_a_queued_run_to_starting() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessStarted { pid: 1234 },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "starting");
        assert_eq!(
            run_states(&db, run_id).await,
            vec!["queued".to_string(), "starting".to_string()]
        );
        assert!(
            run_timestamp_set(&db, run_id, "started_at").await,
            "the starting edge stamps runs.started_at"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn the_first_vendor_event_walks_a_queued_run_through_starting_into_working() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, mut rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::VendorSessionEstablished {
                vendor_session_id: "vs-1".to_string(),
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "working");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string()
            ]
        );

        // The event-broadcast invariant: every edge the walk committed is
        // broadcast, in commit order, before `emit` resolves.
        let first = rx.recv().await.expect("first broadcast envelope");
        match &first.event {
            RuntimeEvent::RunEvent { kind, state, .. } => {
                assert_eq!(*kind, RuntimeEventKind::RunStarting);
                assert_eq!(state.as_str(), "starting");
            }
            other => panic!("expected the starting edge, got {other:?}"),
        }
        let second = rx.recv().await.expect("second broadcast envelope");
        match &second.event {
            RuntimeEvent::RunEvent { kind, state, .. } => {
                assert_eq!(*kind, RuntimeEventKind::RunWorking);
                assert_eq!(state.as_str(), "working");
            }
            other => panic!("expected the working edge, got {other:?}"),
        }
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_turn_end_moves_a_working_run_to_waiting_user_and_broadcasts_it() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, mut rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::TurnEnded {
                outcome: crew_protocol::TurnOutcome::Normal,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(
            run_state(&db, run_id).await,
            "waitingUser",
            "a finished turn hands the run back to the leader (ADR-0027)"
        );
        assert!(
            !run_timestamp_set(&db, run_id, "completed_at").await,
            "a turn boundary is not a terminal state: nothing may stamp completed_at"
        );

        // Invariant 7: the edge is broadcast, not just committed.
        let envelope = rx.recv().await.expect("the edge must be broadcast");
        match &envelope.event {
            RuntimeEvent::RunEvent { kind, state, .. } => {
                assert_eq!(*kind, RuntimeEventKind::RunWaitingUser);
                assert_eq!(state.as_str(), "waitingUser");
            }
            other => panic!("expected the waitingUser edge, got {other:?}"),
        }
        db.shutdown().await.expect("shutdown database");
    }

    async fn run_flags(
        db: &Arc<DatabaseHandle>,
        project_id: ProjectId,
        run_id: RunId,
    ) -> crew_protocol::RunFlags {
        let value = db
            .run_domain_op(Box::new(move |conn| {
                DomainRepository::new(conn, project_id)
                    .read_run_flags(run_id)
                    .map(|flags| serde_json::to_value(flags).expect("flags serialize"))
            }))
            .await
            .expect("read run flags");
        serde_json::from_value(value).expect("flags deserialize")
    }

    /// The distinction a snapshot reader needs: both a finished turn and a
    /// worker's question land a run in `waitingUser`, so the state alone
    /// cannot tell them apart.
    #[tokio::test]
    async fn a_turn_end_marks_the_run_turn_settled() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        assert!(
            !run_flags(&db, project_id, run_id).await.turn_settled,
            "a working run is not turn-settled"
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::TurnEnded {
                outcome: crew_protocol::TurnOutcome::Normal,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "waitingUser");
        assert!(
            run_flags(&db, project_id, run_id).await.turn_settled,
            "the wait must be marked as a finished turn, not a question"
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// A run parked by a finished turn goes back to work when the leader
    /// steers it -- and the flag must not outlive the pause it described,
    /// or a snapshot reader would keep seeing "the answer is ready" for a
    /// run that is busy again.
    #[tokio::test]
    async fn a_trailing_session_meta_entry_never_resumes_a_settled_run() {
        // `observe_vendor_activity`'s `waitingUser` arm used
        // to treat this MessageChunk (or the bookkeeping entries that
        // actually triggered the bug -- a hook summary, a cost record, a
        // hidden `SessionMeta`-only line -- all of which reach this sink
        // the same generic way) as evidence the leader had steered the
        // run. It never had -- resumption is now caused only by a
        // delivered follow-up or a real user-authored transcript entry,
        // neither of which this event is.
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::TurnEnded {
                outcome: crew_protocol::TurnOutcome::Normal,
            },
            cursor: None,
        })
        .await
        .expect("emit");
        assert_eq!(run_state(&db, run_id).await, "waitingUser");
        assert!(run_flags(&db, project_id, run_id).await.turn_settled);

        // Ordinary vendor evidence -- exactly the shape a trailing
        // bookkeeping entry produces, never a real new turn.
        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::MessageChunk {
                role: "assistant".to_string(),
                text: crew_protocol::Classified {
                    class: crew_protocol::ContentClass::Visible,
                    value: "working on it".to_string(),
                },
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(
            run_state(&db, run_id).await,
            "waitingUser",
            "ordinary vendor output must never resume a run on its own"
        );
        assert!(
            run_flags(&db, project_id, run_id).await.turn_settled,
            "the turn-settled marker must survive vendor output that isn't a real new turn"
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// The narrow replacement for the deleted arm above: a real
    /// user-authored turn -- the ONLY vendor-side evidence this sink
    /// still trusts -- resumes a settled run exactly as the old, wrongly
    /// generic arm used to.
    #[tokio::test]
    async fn a_real_user_turn_resumes_a_settled_run() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::TurnEnded {
                outcome: crew_protocol::TurnOutcome::Normal,
            },
            cursor: None,
        })
        .await
        .expect("emit");
        assert_eq!(run_state(&db, run_id).await, "waitingUser");
        assert!(run_flags(&db, project_id, run_id).await.turn_settled);

        sink.note_real_user_turn(run_id).await.expect("note");

        assert_eq!(run_state(&db, run_id).await, "working");
        assert!(!run_flags(&db, project_id, run_id).await.turn_settled);
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_real_user_turn_resume_journals_its_cause() {
        // The resume itself was already caused, not inferred -- this is
        // the evidence a `waitingUser -> working` edge previously carried
        // none of: why it resumed.
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, mut rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::TurnEnded {
                outcome: crew_protocol::TurnOutcome::Normal,
            },
            cursor: None,
        })
        .await
        .expect("emit");
        // Drain the TurnEnded-caused events before the resume, so only the
        // resume's own broadcasts remain to inspect below.
        while rx.try_recv().is_ok() {}

        sink.note_real_user_turn(run_id).await.expect("note");

        let mut saw_resumed_with_cause = false;
        while let Ok(envelope) = rx.try_recv() {
            if let crew_protocol::RuntimeEvent::RunResumed {
                run_id: resumed_run_id,
                cause,
            } = envelope.event
            {
                assert_eq!(resumed_run_id, run_id);
                assert_eq!(cause, crew_protocol::ResumeCause::RealUserTurn);
                saw_resumed_with_cause = true;
            }
        }
        assert!(
            saw_resumed_with_cause,
            "a genuine waitingUser -> working resume must journal its cause"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn starting_to_working_never_journals_a_resume_cause() {
        // A run's very first `working` transition (queued ->
        // starting -> working, via ordinary vendor activity) is legal into
        // `working` exactly like a real resume, but it is not a resume --
        // there was never a settled turn to resume FROM. `RunResumed` must
        // never fire here; journaling it would be a fabricated audit
        // record on the run's very first state transition.
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, mut rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::VendorSessionEstablished {
                vendor_session_id: "vs-1".to_string(),
            },
            cursor: None,
        })
        .await
        .expect("emit");
        assert_eq!(run_state(&db, run_id).await, "working");

        while let Ok(envelope) = rx.try_recv() {
            assert!(
                !matches!(
                    envelope.event,
                    crew_protocol::RuntimeEvent::RunResumed { .. }
                ),
                "queued -> starting -> working must never journal a resume cause: {:?}",
                envelope.event
            );
        }
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_real_user_turn_from_a_non_waiting_user_state_never_journals_a_resume_cause() {
        // The test above only proves `observe_vendor_activity`'s
        // OWN path never journals a resume cause -- it never reaches the
        // new gate at all, so it can't prove the gate itself is
        // load-bearing (confirmed directly: this test failed exactly as
        // expected when the gate was mutated out, while the test above
        // stayed green throughout). This one actually exercises
        // `observe_real_user_turn`'s own from-state check: gating on
        // `transition_run`'s success alone is not enough, because
        // `working -> paused -> working` is ALSO a legal edge into
        // `working`, distinct from a genuine `waitingUser -> working`
        // resume. If `turnSettled` were ever true while the run sat in
        // `paused` (not this file's normal lifecycle, but not ruled out by
        // `observe_real_user_turn`'s own gate either), journaling
        // `RunResumed` for that edge would be a fabricated audit record on
        // a run that was never actually parked waiting on its own settled
        // turn. Drives the run to `paused` directly (a legal edge), forces
        // `turnSettled` true to isolate exactly this case, and asserts the
        // transition succeeds (proving `paused -> working` really is
        // legal) while `RunResumed` still never fires.
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (_task_id, _worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "paused").await;
        db.run_domain_op(Box::new(move |conn| {
            DomainRepository::new(conn, project_id)
                .set_run_flag(run_id, crate::domain::RunFlag::TurnSettled, true)
                .map(|_| serde_json::json!({}))
        }))
        .await
        .expect("force turnSettled for the test");

        let (tx, mut rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.note_real_user_turn(run_id).await.expect("note");

        assert_eq!(
            run_state(&db, run_id).await,
            "working",
            "paused -> working must still be a legal edge"
        );
        while let Ok(envelope) = rx.try_recv() {
            assert!(
                !matches!(
                    envelope.event,
                    crew_protocol::RuntimeEvent::RunResumed { .. }
                ),
                "a paused -> working edge must never journal a resume cause: {:?}",
                envelope.event
            );
        }
        db.shutdown().await.expect("shutdown database");
    }

    /// The test above composes `RunLifecycleSink`
    /// directly, so deleting `SettlementSink`'s forwarding override would
    /// leave CI green -- nothing exercises the wrapping order production
    /// actually uses. This composes exactly that order
    /// (`SettlementSink::wrap(RunLifecycleSink::wrap(..))`, matching
    /// `registry.rs`'s two production call sites) through the trait
    /// object both sinks are hidden behind, so a future stack reorder (or
    /// a dropped override) breaks this test, not the product.
    #[tokio::test]
    async fn a_real_user_turn_resumes_a_settled_run_through_the_production_sink_stack() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let inner = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );
        let (sink, _settled, _slot_free) = SettlementSink::wrap(inner);

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::TurnEnded {
                outcome: crew_protocol::TurnOutcome::Normal,
            },
            cursor: None,
        })
        .await
        .expect("emit");
        assert_eq!(run_state(&db, run_id).await, "waitingUser");
        assert!(run_flags(&db, project_id, run_id).await.turn_settled);

        sink.note_real_user_turn(run_id).await.expect("note");

        assert_eq!(
            run_state(&db, run_id).await,
            "working",
            "the production sink stack must forward note_real_user_turn all the way to \
             RunLifecycleSink, not swallow it at SettlementSink"
        );
        assert!(!run_flags(&db, project_id, run_id).await.turn_settled);
        db.shutdown().await.expect("shutdown database");
    }

    /// The gate that keeps the un-park narrow: a `waitingUser` that is NOT
    /// turn-settled (a worker's question, an approval wait) must still
    /// never be clobbered by vendor output.
    #[tokio::test]
    async fn vendor_activity_never_un_parks_a_wait_that_is_not_turn_settled() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "waitingUser").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::MessageChunk {
                role: "assistant".to_string(),
                text: crew_protocol::Classified {
                    class: crew_protocol::ContentClass::Visible,
                    value: "still thinking out loud".to_string(),
                },
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(
            run_state(&db, run_id).await,
            "waitingUser",
            "a question wait is not a finished turn and must survive vendor output"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_turn_end_never_pulls_a_terminal_run_backwards() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        drive_to_state(&db, project_id, run_id, "cancelled").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::TurnEnded {
                outcome: crew_protocol::TurnOutcome::Normal,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(
            run_state(&db, run_id).await,
            "cancelled",
            "a turn boundary arriving after a cancel must not resurrect the run"
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// A bare zero exit is never itself evidence of success -- a
    /// TUI vendor process exiting is not the leader closing the
    /// conversation via `run/finish` (ADR-0027). This is the "process
    /// exit 0 with no turn on a parked run" shape from the 2026-09-08
    /// attempt-3 conformance run (`01a08253`, minus the `waitingUser`
    /// detour -- see `an_exit_while_waiting_on_a_user_routes_through_working_to_failed`
    /// for that one) exercised straight from `working`.
    #[tokio::test]
    async fn a_zero_exit_settles_the_run_as_failed() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "failed");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "failed".to_string()
            ]
        );
        assert!(
            run_timestamp_set(&db, run_id, "completed_at").await,
            "a terminal edge stamps runs.completed_at"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_nonzero_exit_settles_the_run_as_failed() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(7),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "failed");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "failed".to_string()
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_signalled_exit_settles_the_run_as_failed() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: None,
                signal: Some("SIGKILL".to_string()),
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "failed");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "failed".to_string()
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn an_exit_with_no_observable_status_settles_the_run_as_lost() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: None,
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        // `queued -> lost` is illegal, so the walk proves its shape by
        // committing the forced intermediate hop first.
        assert_eq!(run_state(&db, run_id).await, "lost");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "lost".to_string()
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn an_exit_after_cancellation_leaves_the_cancelled_run_untouched() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "cancelled").await;
        let before = run_states(&db, run_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run_id).await, "cancelled");
        assert_eq!(
            run_states(&db, run_id).await,
            before,
            "a terminal state always wins: no further RunEvent may be appended"
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// The regression `walk_to`'s new return value closes. The leader's own `run/finish` can settle
    /// a run to `succeeded` before the vendor process's own `ProcessExited`
    /// evidence lands -- a real race, not a hypothetical one, since the two
    /// arrive on independent paths. When that exit lands here, `walk_to`
    /// correctly no-ops (the run is already terminal), but the OLD code
    /// raised `repeated_failure` against `terminal_state_for`'s *computed*
    /// guess regardless of what `walk_to` actually applied -- so a run that
    /// never failed at all could still escalate as if it had. This run's
    /// task has a genuinely failed predecessor (`run1`), the exact condition
    /// `previous_run_for_task_also_failed` would say yes to -- so if the old
    /// bug were still here, this is precisely the case that would trip it.
    #[tokio::test]
    async fn an_exit_racing_an_already_finished_run_never_raises_the_computed_failure() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run1) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run1, "failed").await;

        let run2 = seed_second_run(&db, project_id, task_id, worker_id).await;
        drive_to_state(&db, project_id, run2, "succeeded").await;
        let before = run_states(&db, run2).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run2,
            Arc::new(ActivityClock::new()),
        );

        // A non-zero exit is unconditionally `failed` per `terminal_state_for`,
        // independent of `turnSettled` -- the strongest possible computed
        // guess, so this exercises the gate rather than a value that might
        // coincidentally agree with `succeeded` on its own.
        sink.emit(AdapterEvent {
            run_id: run2,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(1),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run2).await, "succeeded");
        assert_eq!(
            run_states(&db, run2).await,
            before,
            "a terminal state always wins: no further RunEvent may be appended"
        );
        assert_eq!(
            escalation_reasons(&db, run2).await,
            Vec::<String>::new(),
            "a run that never actually transitioned to failed must never raise \
             repeated_failure, even though its predecessor genuinely did fail"
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// The narrowing this gate deliberately accepts, pinned rather than
    /// left as an untested side effect: a run the leader already committed
    /// to `failed` through its own path (`run/finish`'s, which
    /// `drive_to_state` stands in for here) never raises `repeated_failure`
    /// off a racing exit either, even on a task whose predecessor
    /// genuinely also failed. Before the fix above existed this case DID
    /// escalate, but only by accident -- riding the same computed-guess bug
    /// that also produced the false positive on `succeeded`. See
    /// `observe_process_exited`'s own doc comment for why this is treated
    /// as acceptable rather than closed: a leader-adjudicated failure
    /// already carries the leader's own verdict.
    #[tokio::test]
    async fn an_exit_racing_a_run_the_leader_already_failed_does_not_escalate_either() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run1) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run1, "failed").await;

        let run2 = seed_second_run(&db, project_id, task_id, worker_id).await;
        drive_to_state(&db, project_id, run2, "failed").await;
        let before = run_states(&db, run2).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run2,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id: run2,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(1),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(run_state(&db, run2).await, "failed");
        assert_eq!(
            run_states(&db, run2).await,
            before,
            "a terminal state always wins: no further RunEvent may be appended"
        );
        assert_eq!(
            escalation_reasons(&db, run2).await,
            Vec::<String>::new(),
            "a run already failed through the leader's own run/finish path must not \
             escalate again off a racing exit, even though its predecessor genuinely failed too"
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// The positive case the regression above needs beside it: gating the
    /// escalation on an applied transition must not also suppress a
    /// genuine one. Two runs on the same task, both actually driven to
    /// `failed` through this sink (never already terminal when their exit
    /// lands), must still raise `repeated_failure` on the second, carrying
    /// the populated question `repeated_failure_question` now supplies.
    #[tokio::test]
    async fn a_second_consecutive_genuine_failure_still_raises_repeated_failure() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run1) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run1, "working").await;
        let (tx1, _rx1) = broadcast::channel(64);
        RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx1,
            run1,
            Arc::new(ActivityClock::new()),
        )
        .emit(AdapterEvent {
            run_id: run1,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(1),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit run1 exit");
        assert_eq!(run_state(&db, run1).await, "failed");
        // run1 is the task's first failure: no predecessor, no escalation.
        assert_eq!(escalation_reasons(&db, run1).await, Vec::<String>::new());

        let run2 = seed_second_run(&db, project_id, task_id, worker_id).await;
        drive_to_state(&db, project_id, run2, "working").await;
        let (tx2, _rx2) = broadcast::channel(64);
        RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx2,
            run2,
            Arc::new(ActivityClock::new()),
        )
        .emit(AdapterEvent {
            run_id: run2,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(1),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit run2 exit");

        assert_eq!(run_state(&db, run2).await, "failed");
        assert_eq!(
            escalation_reasons(&db, run2).await,
            vec!["repeated_failure".to_string()],
            "run2 genuinely failed right after run1 genuinely failed -- the gate must not \
             suppress this"
        );
        let questions = escalation_questions(&db, run2).await;
        assert_eq!(questions.len(), 1);
        let question = questions[0]
            .as_deref()
            .expect("repeated_failure must carry a populated question, not None");
        assert!(
            question.contains("consecutive") && question.contains("failed"),
            "question does not read as a repeated-failure notice: {question:?}"
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// A human closing a parked worker's terminal must not read as
    /// success. This reproduces the 2026-09-08 attempt-3 conformance run's
    /// F4/F17 (`01a08216`/`01a08251`/`01a08253`): the run reached
    /// `waitingUser` with `turnSettled` still `false` (`drive_to_state`
    /// force-writes the state with no real `TurnEnded` ever journaled
    /// through this sink -- exactly ADR-0012's approval-flow arrival at
    /// `waitingUser`, or a start failure's own teardown, neither of which
    /// ever finished a turn), and a bare zero exit arrived. The governing
    /// fact is `turnSettled`, not the current state -- see
    /// `an_exit_after_a_settled_turn_with_no_run_finish_settles_the_run_as_
    /// the_unrendered_verdict_state` below for the settled-turn case this
    /// is NOT. Unlike the old `succeeded` target, `waitingUser -> failed`
    /// is a direct, legal edge (`RunState::can_transition_to`), so no
    /// forced intermediate hop is needed here -- the walk lands on
    /// `failed` in one commit.
    #[tokio::test]
    async fn an_exit_while_waiting_on_a_user_settles_the_run_as_failed() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "waitingUser").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit");

        // `waitingUser -> failed` is a legal direct edge, unlike the old
        // `waitingUser -> succeeded` target -- no forced `working` hop.
        assert_eq!(run_state(&db, run_id).await, "failed");
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "waitingUser".to_string(),
                "failed".to_string()
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    /// This is the false-failure regression caught by staff review of
    /// `terminal_state_for`'s first version: a run whose turn genuinely
    /// settled -- a real `TurnEnded` emitted through this sink, exactly
    /// `a_turn_end_marks_
    /// the_run_turn_settled`'s setup -- did real work. A bare zero exit
    /// with no `run/finish` call in between must not read as `failed`
    /// either: the leader simply never rendered a verdict (a human closed
    /// the parked worker's terminal instead of answering, or leaving it,
    /// which is the same "did work, no verdict" shape an abandoned-leader
    /// trigger would also need to reach through `RunState::
    /// unrendered_verdict()`). `waitingUser -> cancelled` is a legal
    /// direct edge, same as `-> failed`, so no forced hop here either.
    #[tokio::test]
    async fn an_exit_after_a_settled_turn_with_no_run_finish_settles_the_run_as_the_unrendered_verdict_state()
     {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "working").await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        // A REAL turn boundary, not `drive_to_state` -- this is what sets
        // `turnSettled` and is the fact under test.
        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::TurnEnded {
                outcome: crew_protocol::TurnOutcome::Normal,
            },
            cursor: None,
        })
        .await
        .expect("emit turn end");
        assert_eq!(run_state(&db, run_id).await, "waitingUser");
        assert!(
            run_flags(&db, project_id, run_id).await.turn_settled,
            "the turn must genuinely be settled for this test to exercise the right branch"
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::ProcessExited {
                exit_code: Some(0),
                signal: None,
            },
            cursor: None,
        })
        .await
        .expect("emit exit");

        let unrendered = RunState::unrendered_verdict().to_string();
        assert_eq!(
            run_state(&db, run_id).await,
            unrendered,
            "a settled turn with no run/finish call must not read as failed OR succeeded"
        );
        assert_eq!(
            run_states(&db, run_id).await,
            vec![
                "queued".to_string(),
                "starting".to_string(),
                "working".to_string(),
                "waitingUser".to_string(),
                unrendered,
            ]
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn vendor_output_never_reopens_working_on_a_run_that_started_waiting() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        drive_to_state(&db, project_id, run_id, "waitingUser").await;
        let before = run_states(&db, run_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(StubSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        sink.emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::MessageFinal {
                role: "assistant".to_string(),
                text: Classified {
                    class: ContentClass::Visible,
                    value: "all done".to_string(),
                },
            },
            cursor: None,
        })
        .await
        .expect("emit");

        assert_eq!(
            run_state(&db, run_id).await,
            "waitingUser",
            "vendor output must never clobber an approval wait"
        );
        assert_eq!(
            run_states(&db, run_id).await,
            before,
            "no RunEvent may be appended for output that is at-or-past working"
        );
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_failed_inner_emit_never_applies_a_lifecycle_edge() {
        let (_dir, db) = open_db().await;
        let project_id = ProjectId::new();
        let (task_id, worker_id, run_id) = seed_run(&db, project_id).await;
        let (tx, _rx) = broadcast::channel(64);
        let sink = RunLifecycleSink::wrap(
            Arc::new(FailingSink),
            Arc::clone(&db),
            project_id,
            tx,
            run_id,
            Arc::new(ActivityClock::new()),
        );

        let err = sink
            .emit(AdapterEvent {
                run_id,
                task_id,
                worker_id,
                payload: AdapterEventPayload::ProcessStarted { pid: 1234 },
                cursor: None,
            })
            .await
            .expect_err("the inner sink's failure must propagate");
        assert_eq!(
            err.to_string(),
            "adapter stub operation emit failed (process): journal write failed"
        );

        // No edge without durable evidence: a `ProcessStarted` whose journal
        // write never actually committed must never move the run, because
        // nothing durable backs the `starting` edge it would otherwise apply.
        assert_eq!(
            run_state(&db, run_id).await,
            "queued",
            "a run must not advance on evidence its own sink failed to journal"
        );
        assert_eq!(
            run_states(&db, run_id).await,
            vec!["queued".to_string()],
            "no RunEvent may be appended for an emit the inner sink rejected"
        );
        db.shutdown().await.expect("shutdown database");
    }
}
