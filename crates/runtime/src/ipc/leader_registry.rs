//! Tracks how many live connections currently belong to each leader
//! instance id, so a run's owning leader disconnecting ENTIRELY (every
//! connection for its instance id gone, not just one of several) can be
//! told apart from an ordinary reconnect or a second, unrelated
//! connection using the same id.
//!
//! Refcounted, not a set or a single flag: two live connections can
//! legitimately share one instance id (the `crew-extension` fallback --
//! see [`super::connection::validate_instance_id`]'s own doc comment --
//! is used by every leader session that has no real session id of its
//! own, so more than one physical leader process can be registered under
//! it at once), and both must drop before the id is considered gone.
//! Counting also makes both connect/disconnect orderings safe by
//! construction rather than by care: a reconnect that lands before the
//! old connection's own teardown runs nets `1 -> 2 -> 1`, never touching
//! zero; a reconnect racing the other way (a fresh `0 -> 1` racing a
//! stale decrement already in flight) still resolves correctly, because
//! plain addition does not care which side's `+1`/`-1` is observed
//! first.
//!
//! A bare `count == 0` is not enough to decide "gone for the grace
//! window", though -- see [`Entry::zero_since`]'s own doc comment for
//! why, and [`LeaderRegistry::gone_for_at_least`] for the check that
//! actually answers it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a run's owning leader may hold no live connection at all
/// before the run is settled as unrendered (`RunState::unrendered_verdict`).
///
/// The maintainer's ruling: a slot is held for at most a minute after a
/// leader genuinely leaves, which is short enough that an abandoned run
/// does not tie up a concurrency slot indefinitely, and long enough to
/// cover a quick OMP restart or reconnect -- the ordinary case this must
/// never fire for. A daemon restart is charged the same way (see
/// `LeaderRegistry::seed_disconnected_since`'s own doc comment for why
/// the clock starts at the daemon's own startup instant, never at the
/// run's last-seen time).
pub(crate) const LEADER_DISCONNECT_GRACE_WINDOW: Duration = Duration::from_secs(60);

/// One instance id's live-connection bookkeeping.
#[derive(Default)]
struct Entry {
    count: usize,
    /// The instant `count` last dropped to zero, cleared the moment it
    /// rises above zero again. A grace-window timer wakes long after it
    /// was spawned and cannot tell, from `count == 0` alone, whether that
    /// zero is the SAME one it was spawned to watch or a fresher one from
    /// an intervening reconnect-then-disconnect cycle that happened to
    /// also land on zero. Recording the instant of the transition (not
    /// just the fact of it) is what lets the timer ask the actual
    /// question: has the CURRENT absence lasted the full window, not
    /// merely "is the count zero right now".
    zero_since: Option<Instant>,
}

/// Refcounted live-connection registry, keyed by leader instance id.
/// Cloning shares the same underlying map (cheap, `Arc`-backed) -- every
/// clone observes and mutates the same state.
#[derive(Clone, Default)]
pub(crate) struct LeaderRegistry {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

impl LeaderRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers one live connection for `instance_id`, returning a guard
    /// that deregisters it on drop -- including on an early `return` out
    /// of the connection's dispatch loop or a panic unwind, never only on
    /// the loop's "normal" exit. See [`LeaderConnectionGuard`]'s own doc
    /// comment for why that has to be structural.
    ///
    /// `on_gone` fires exactly when THIS drop is the one that brings the
    /// count to zero (never on a decrement that leaves it above zero, and
    /// never more than once per 1->0 transition) -- the caller's hook to
    /// arm a grace-window timer. Fired from inside `Drop::drop` itself,
    /// so arming is exactly as structural as the decrement it rides on:
    /// a future early `return` that would otherwise skip an explicit
    /// "now arm a timer" statement cannot skip this one, because there
    /// is no separate statement to skip. This module knows nothing about
    /// what the timer does or what "settling" means -- that stays in
    /// `ipc::connection`/`service::orchestration`, which is what `on_gone`
    /// is for.
    pub(crate) fn register(
        &self,
        instance_id: String,
        on_gone: impl Fn(String) + Send + Sync + 'static,
    ) -> LeaderConnectionGuard {
        let mut entries = self
            .entries
            .lock()
            .expect("leader registry mutex never poisoned");
        let entry = entries.entry(instance_id.clone()).or_default();
        entry.count += 1;
        entry.zero_since = None;
        drop(entries);
        LeaderConnectionGuard {
            registry: self.clone(),
            instance_id,
            on_gone: Arc::new(on_gone),
        }
    }

    /// Decrements `instance_id`'s live count, returning `true` exactly
    /// when this decrement is the one that brought it to zero (the
    /// signal [`LeaderConnectionGuard::drop`] uses to fire `on_gone`).
    fn deregister_reporting_zero_transition(&self, instance_id: &str) -> bool {
        let mut entries = self
            .entries
            .lock()
            .expect("leader registry mutex never poisoned");
        let Some(entry) = entries.get_mut(instance_id) else {
            return false;
        };
        entry.count = entry.count.saturating_sub(1);
        if entry.count == 0 {
            entry.zero_since = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// Seeds `instance_id` as having been at zero live connections since
    /// `since`, WITHOUT creating a live connection -- the daemon-restart
    /// case: at startup there are no connections at all yet, but a
    /// parked run's owning leader was disconnected (from THIS daemon's
    /// perspective) at the moment this process started, not whenever it
    /// last happened to be observed. A leader cannot reconnect to a
    /// daemon that is not running, so the clock for "how long has the
    /// leader been gone" starts at startup, never at the run's own
    /// last-seen time -- charging the daemon's own downtime against the
    /// leader would settle live runs on every restart after a long stop,
    /// which is exactly the destructive direction this feature exists to
    /// avoid.
    ///
    /// A no-op if `instance_id` already has a live connection (count > 0)
    /// by the time this runs -- seeding never clobbers a real connection
    /// that beat it to registering.
    pub(crate) fn seed_disconnected_since(&self, instance_id: String, since: Instant) {
        let mut entries = self
            .entries
            .lock()
            .expect("leader registry mutex never poisoned");
        let entry = entries.entry(instance_id).or_default();
        if entry.count == 0 {
            entry.zero_since.get_or_insert(since);
        }
    }

    /// Whether `instance_id` has had zero live connections continuously
    /// for at least `grace`, since the transition [`Self::gone_for_at_least`]'s
    /// caller is actually asking about -- not merely whether the count
    /// happens to read zero at the moment of the call. `zero_since` is
    /// cleared by any intervening reconnect (see [`Self::register`]), so
    /// a stale timer waking against a fresher zero correctly sees that
    /// fresher zero hasn't aged enough yet, rather than settling early
    /// against a zero that isn't the one it was spawned for.
    pub(crate) fn gone_for_at_least(&self, instance_id: &str, grace: Duration) -> bool {
        let entries = self
            .entries
            .lock()
            .expect("leader registry mutex never poisoned");
        match entries.get(instance_id) {
            Some(entry) => {
                entry.count == 0
                    && entry
                        .zero_since
                        .is_some_and(|since| since.elapsed() >= grace)
            }
            // No entry at all is the same fact as "gone": nothing has
            // ever registered a live connection for this id.
            None => true,
        }
    }
}

/// Deregisters its connection's slot from a [`LeaderRegistry`] on drop.
///
/// The existing `active_connections` counter (`server.rs`) gets this
/// "always decremented, however the connection ends" property for free,
/// because its own decrement lives in the SPAWN WRAPPER around the whole
/// connection future (`Server::admit`), which runs regardless of how
/// that future resolves. This bookkeeping instead has to live INSIDE the
/// connection's own future (`connection::handle`), because it needs the
/// negotiated instance id, which the spawn wrapper never sees -- the
/// `initialize` handshake that produces it happens deep inside `handle`,
/// not in `admit`. Living inside the future means every `break`/`return`
/// out of `handle`'s dispatch loop has to reach the decrement, and that
/// is a property of today's control flow, not of the type -- the next
/// early `return` added to that function would otherwise leak a
/// registration silently. A leaked registration means the count never
/// reaches zero, which means this leader is never considered gone: the
/// feature fails CLOSED, silently, and no test would notice unless it
/// happened to exercise that exact exit path. A drop guard makes the
/// invariant structural instead of conventional.
pub(crate) struct LeaderConnectionGuard {
    registry: LeaderRegistry,
    instance_id: String,
    on_gone: Arc<dyn Fn(String) + Send + Sync>,
}

impl Drop for LeaderConnectionGuard {
    fn drop(&mut self) {
        if self
            .registry
            .deregister_reporting_zero_transition(&self.instance_id)
        {
            (self.on_gone)(self.instance_id.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{LeaderConnectionGuard, LeaderRegistry};

    const GRACE: Duration = Duration::from_millis(20);

    /// Registers with a no-op `on_gone`, for tests that only care about
    /// the count/`zero_since` bookkeeping, not the callback.
    fn reg(registry: &LeaderRegistry, id: &str) -> LeaderConnectionGuard {
        registry.register(id.to_string(), |_| {})
    }

    #[test]
    fn a_single_connection_is_not_gone_while_live() {
        let registry = LeaderRegistry::new();
        let _guard = reg(&registry, "leader-1");
        assert!(!registry.gone_for_at_least("leader-1", GRACE));
    }

    #[test]
    fn dropping_the_only_connection_eventually_reads_gone() {
        let registry = LeaderRegistry::new();
        let guard = reg(&registry, "leader-1");
        drop(guard);
        assert!(
            !registry.gone_for_at_least("leader-1", GRACE),
            "not yet -- the window hasn't elapsed"
        );
        std::thread::sleep(GRACE * 2);
        assert!(registry.gone_for_at_least("leader-1", GRACE));
    }

    /// A reconnect landing before the old connection's own teardown runs:
    /// count goes 1 -> 2 -> 1, never touching zero. Never gone.
    #[test]
    fn a_reconnect_racing_the_previous_teardown_does_not_strand_the_leader() {
        let registry = LeaderRegistry::new();
        let old = reg(&registry, "leader-1");
        let new = reg(&registry, "leader-1"); // reconnect arrives first
        drop(old); // old connection's teardown lands second
        assert!(
            !registry.gone_for_at_least("leader-1", GRACE),
            "one live connection (`new`) remains"
        );
        std::thread::sleep(GRACE * 2);
        assert!(
            !registry.gone_for_at_least("leader-1", GRACE),
            "still live after the window elapses"
        );
        drop(new);
    }

    /// The race the other direction: the OLD connection's decrement is
    /// the one to land second even though the reconnect happened first in
    /// wall-clock terms -- still resolves correctly under commutative
    /// arithmetic.
    #[test]
    fn a_reconnect_before_a_delayed_old_teardown_still_leaves_the_leader_live() {
        let registry = LeaderRegistry::new();
        let old = reg(&registry, "leader-1");
        let new = reg(&registry, "leader-1");
        // old's teardown is delayed (simulated by ordering the drop last)
        drop(old);
        assert!(!registry.gone_for_at_least("leader-1", GRACE));
        drop(new);
    }

    #[test]
    fn a_zero_that_reconnects_and_disconnects_again_within_the_window_resets_the_clock() {
        let registry = LeaderRegistry::new();
        let first = reg(&registry, "leader-1");
        drop(first); // 1 -> 0, zero_since = now
        std::thread::sleep(GRACE); // the original zero is now old enough...
        let second = reg(&registry, "leader-1"); // ...but a reconnect arrives
        drop(second); // 1 -> 0 again -- a FRESH zero_since
        // The fresh zero has not aged the full window yet, even though
        // enough wall-clock time has passed since the ORIGINAL zero that
        // a stale timer checking only `count == 0` would have settled.
        assert!(!registry.gone_for_at_least("leader-1", GRACE));
    }

    #[test]
    fn seeding_a_never_connected_instance_treats_it_as_gone_since_the_given_instant() {
        let registry = LeaderRegistry::new();
        let since = std::time::Instant::now() - GRACE * 2;
        registry.seed_disconnected_since("leader-1".to_string(), since);
        assert!(registry.gone_for_at_least("leader-1", GRACE));
    }

    #[test]
    fn seeding_never_clobbers_a_connection_that_already_registered() {
        let registry = LeaderRegistry::new();
        let _guard = reg(&registry, "leader-1");
        let since = std::time::Instant::now() - GRACE * 2;
        registry.seed_disconnected_since("leader-1".to_string(), since);
        assert!(
            !registry.gone_for_at_least("leader-1", GRACE),
            "a live connection must never be treated as gone"
        );
    }

    #[test]
    fn an_unregistered_instance_id_reads_as_gone() {
        let registry = LeaderRegistry::new();
        assert!(registry.gone_for_at_least("never-seen", GRACE));
    }

    #[test]
    fn on_gone_fires_exactly_once_on_the_true_1_to_0_transition() {
        let registry = LeaderRegistry::new();
        let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&fired);
        let a = registry.register("leader-1".to_string(), move |id| {
            recorded.lock().unwrap().push(id)
        });
        let recorded2 = Arc::clone(&fired);
        let b = registry.register("leader-1".to_string(), move |id| {
            recorded2.lock().unwrap().push(id)
        });

        drop(a); // 2 -> 1: not zero, on_gone must not fire
        assert!(
            fired.lock().unwrap().is_empty(),
            "a decrement that leaves the count above zero must never fire on_gone"
        );

        drop(b); // 1 -> 0: the true transition
        assert_eq!(*fired.lock().unwrap(), vec!["leader-1".to_string()]);
    }
}
