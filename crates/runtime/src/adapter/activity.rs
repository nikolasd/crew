//! Per-run liveness clocks (WP19): the activity evidence the timeout sweep
//! reduces into `WorkerTimeout{Inactivity|Total}` journal decisions.
//!
//! Two independent deadlines per run, per spec §7.5:
//!
//! * **Inactivity** — measured from the last vendor event that flowed
//!   through [`crate::adapter::run_lifecycle::RunLifecycleSink`]. Every
//!   journaled event re-arms it (clearing a already-journaled expiry), so a
//!   chatty worker is never timed out and a stalled one is reported once
//!   per stall.
//! * **Total** — measured from the run's first observed activity (its
//!   clock start). Independent of activity: no amount of chatter disarms
//!   it, and it fires at most once.
//!
//! The runtime never kills on timeout — it journals the fact and lets the
//! leader decide (WP21 adds `run/timeoutAck`). The clocks are in-memory:
//! they describe *this process's* observation window, and a daemon restart
//! legitimately starts a fresh one. The journaled `WorkerTimeout` events
//! are the durable record.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crew_protocol::{RunId, TimeoutKind};

/// One run's liveness state.
#[derive(Debug, Clone, Copy)]
pub struct RunActivity {
    /// When this run's total-duration clock started (first observation).
    pub started_at: Instant,
    /// The last vendor activity observed.
    pub last_activity: Instant,
    /// Whether `WorkerTimeout::Inactivity` was already journaled for the
    /// current quiet stretch. Cleared by [`ActivityClock::touch`].
    pub inactivity_journaled: bool,
    /// Whether `WorkerTimeout::Total` was already journaled. Never cleared
    /// by activity — only a fresh clock entry (a resumed run) resets it.
    pub total_journaled: bool,
}

impl RunActivity {
    fn starting(now: Instant) -> Self {
        Self {
            started_at: now,
            last_activity: now,
            inactivity_journaled: false,
            total_journaled: false,
        }
    }
}

/// The process-wide set of live runs' liveness clocks.
///
/// Shared between every [`RunLifecycleSink`] (which touches it) and the
/// timeout sweep (which reads and marks it). Lock-held regions are O(1)
/// map operations; the sweep works on a snapshot so it never holds the
/// lock across an await.
#[derive(Default)]
pub struct ActivityClock {
    runs: Mutex<HashMap<RunId, RunActivity>>,
}

impl ActivityClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records vendor activity for `run_id`, creating its clock if absent
    /// (the first event both starts the total clock and counts as
    /// activity). Re-arms the inactivity deadline.
    pub fn touch(&self, run_id: &RunId, now: Instant) {
        let mut runs = self.runs.lock().expect("activity clock mutex");
        let entry = runs
            .entry(*run_id)
            .or_insert_with(|| RunActivity::starting(now));
        entry.last_activity = now;
        entry.inactivity_journaled = false;
    }

    /// Starts (or preserves) a run's total-duration clock without counting
    /// activity — used when a run is submitted/resumed so the total
    /// deadline reflects wall-clock time even before the first vendor
    /// event arrives. An existing entry's start is never moved.
    pub fn start(&self, run_id: &RunId, now: Instant) {
        let mut runs = self.runs.lock().expect("activity clock mutex");
        runs.entry(*run_id)
            .or_insert_with(|| RunActivity::starting(now));
    }

    /// Point-in-time copy of every tracked run's clock.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(RunId, RunActivity)> {
        self.runs
            .lock()
            .expect("activity clock mutex")
            .clone()
            .into_iter()
            .collect()
    }

    /// Marks a timeout kind as journaled for `run_id`. A no-op when the
    /// run's clock is gone (it settled between snapshot and mark).
    pub fn mark_journaled(&self, run_id: &RunId, kind: TimeoutKind) {
        let mut runs = self.runs.lock().expect("activity clock mutex");
        if let Some(entry) = runs.get_mut(run_id) {
            match kind {
                TimeoutKind::Inactivity => entry.inactivity_journaled = true,
                TimeoutKind::Total => entry.total_journaled = true,
            }
        }
    }

    /// Drops a settled run's clock: a later resume creates a fresh one via
    /// [`Self::start`]/[`Self::touch`], restarting both deadlines.
    pub fn forget(&self, run_id: &RunId) {
        self.runs
            .lock()
            .expect("activity clock mutex")
            .remove(run_id);
    }
}

/// The timeout kinds whose deadline has elapsed for `activity` at `now`,
/// in report order (inactivity first). Pure so the sweep's decision table
/// is unit-testable without time mocking.
#[must_use]
pub fn due_timeouts(
    activity: &RunActivity,
    inactivity: Duration,
    total: Duration,
    now: Instant,
) -> Vec<TimeoutKind> {
    let mut due = Vec::new();
    if !activity.inactivity_journaled && now.duration_since(activity.last_activity) >= inactivity {
        due.push(TimeoutKind::Inactivity);
    }
    if !activity.total_journaled && now.duration_since(activity.started_at) >= total {
        due.push(TimeoutKind::Total);
    }
    due
}

/// Milliseconds since `since`, saturating — an `Instant` can never be
/// "before" another on the same clock, but a stale snapshot paired with a
/// fresh mark must not panic on a platform quirk either.
#[must_use]
pub fn millis_since(since: Instant, now: Instant) -> u64 {
    now.duration_since(since)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activity_at(start_ms: u64, last_ms: u64) -> RunActivity {
        let start = Instant::now() - Duration::from_millis(start_ms);
        RunActivity {
            started_at: start,
            last_activity: Instant::now() - Duration::from_millis(last_ms),
            inactivity_journaled: false,
            total_journaled: false,
        }
    }

    #[test]
    fn fresh_run_has_no_due_timeouts() {
        let a = activity_at(1_000, 1_000);
        assert!(
            due_timeouts(
                &a,
                Duration::from_secs(300),
                Duration::from_secs(1800),
                Instant::now()
            )
            .is_empty()
        );
    }

    #[test]
    fn idle_past_inactivity_is_due_once_then_rearmed_by_touch() {
        let clock = ActivityClock::new();
        let run_id = RunId::new();
        let start = Instant::now() - Duration::from_secs(400);
        clock.start(&run_id, start);
        clock.touch(&run_id, start);
        // Simulate the quiet stretch: rewind last_activity by mutating via
        // touch at an earlier instant than the threshold.
        let mut a = activity_at(400_000, 400_000);
        assert_eq!(
            due_timeouts(
                &a,
                Duration::from_secs(300),
                Duration::from_secs(1800),
                Instant::now()
            ),
            vec![TimeoutKind::Inactivity]
        );
        clock.mark_journaled(&run_id, TimeoutKind::Inactivity);
        a.inactivity_journaled = true;
        assert!(
            due_timeouts(
                &a,
                Duration::from_secs(300),
                Duration::from_secs(1800),
                Instant::now()
            )
            .is_empty()
        );

        // New activity re-arms: touch clears the flag.
        clock.touch(&run_id, Instant::now());
        let snap = clock.snapshot();
        let (_, entry) = snap.iter().find(|(id, _)| id == &run_id).expect("entry");
        assert!(!entry.inactivity_journaled);
    }

    #[test]
    fn total_deadline_fires_once_regardless_of_activity() {
        let mut a = activity_at(2_000_000, 400_000);
        assert_eq!(
            due_timeouts(
                &a,
                Duration::from_secs(300),
                Duration::from_secs(1800),
                Instant::now()
            ),
            vec![TimeoutKind::Inactivity, TimeoutKind::Total]
        );
        a.inactivity_journaled = true;
        assert_eq!(
            due_timeouts(
                &a,
                Duration::from_secs(300),
                Duration::from_secs(1800),
                Instant::now()
            ),
            vec![TimeoutKind::Total]
        );
        a.total_journaled = true;
        assert!(
            due_timeouts(
                &a,
                Duration::from_secs(300),
                Duration::from_secs(1800),
                Instant::now()
            )
            .is_empty()
        );
    }

    #[test]
    fn start_never_moves_an_existing_total_clock_and_touch_rearms() {
        let clock = ActivityClock::new();
        let run_id = RunId::new();
        let early = Instant::now() - Duration::from_secs(60);
        clock.touch(&run_id, early);
        // A later start() must not reset started_at (total keeps running).
        clock.start(&run_id, Instant::now());
        let snap = clock.snapshot();
        let (_, entry) = snap.iter().find(|(id, _)| id == &run_id).expect("entry");
        assert!(entry.started_at <= early + Duration::from_secs(1));
    }

    #[test]
    fn forget_drops_the_clock() {
        let clock = ActivityClock::new();
        let run_id = RunId::new();
        clock.touch(&run_id, Instant::now());
        clock.forget(&run_id);
        assert!(clock.snapshot().is_empty());
        // mark after forget is a no-op, not a panic.
        clock.mark_journaled(&run_id, TimeoutKind::Total);
    }
}
