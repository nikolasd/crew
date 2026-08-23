//! Per-sender sliding-window rate limiting for `coordination/send`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crew_protocol::WorkerId;

/// The sliding window width: one minute.
const WINDOW: Duration = Duration::from_secs(60);

/// Returned when a sender exceeds the allowed rate within the window.
#[derive(Debug, thiserror::Error)]
#[error("rate limited: sender exceeded {limit} messages per minute")]
pub struct RateLimitError {
    pub limit: u32,
}

/// A per-sender sliding-window limiter. One instance per runtime process,
/// shared by the coordination broker.
pub struct RateLimiter {
    limit: u32,
    sent_at: Mutex<HashMap<WorkerId, Vec<Instant>>>,
}

impl RateLimiter {
    #[must_use]
    pub fn new(limit: u32) -> Self {
        Self {
            limit,
            sent_at: Mutex::new(HashMap::new()),
        }
    }

    /// Records one message from `sender` at `now`, evicting timestamps
    /// outside the window first. Returns [`RateLimitError`] if this would
    /// be the `limit + 1`-th message within the trailing minute.
    ///
    /// # Errors
    /// Returns [`RateLimitError`] when the sender has already sent `limit`
    /// messages within the trailing one-minute window.
    pub fn check(&self, sender: WorkerId, now: Instant) -> Result<(), RateLimitError> {
        let mut sent_at = self
            .sent_at
            .lock()
            .expect("rate limiter mutex is never poisoned");
        // Sweep every sender while we hold the lock: evict expired
        // timestamps and drop entries that drained empty, so a retired
        // worker's key does not leak for the life of the process (R65).
        // Self-limiting: after one sweep the map only holds senders
        // active within the trailing window.
        sent_at.retain(|_, timestamps| {
            timestamps.retain(|t| now.duration_since(*t) < WINDOW);
            !timestamps.is_empty()
        });
        let timestamps = sent_at.entry(sender).or_default();

        if timestamps.len() >= self.limit as usize {
            return Err(RateLimitError { limit: self.limit });
        }
        timestamps.push(now);
        Ok(())
    }

    /// The number of senders currently holding a map entry. Test-only:
    /// exists so the retirement sweep is observable.
    #[cfg(test)]
    fn tracked_senders(&self) -> usize {
        self.sent_at
            .lock()
            .expect("rate limiter mutex is never poisoned")
            .len()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(crew_protocol::COORDINATION_RATE_LIMIT_PER_MINUTE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_rejects() {
        let limiter = RateLimiter::new(3);
        let sender = WorkerId::new();
        let now = Instant::now();

        assert!(limiter.check(sender, now).is_ok());
        assert!(limiter.check(sender, now).is_ok());
        assert!(limiter.check(sender, now).is_ok());
        assert!(limiter.check(sender, now).is_err());
    }

    #[test]
    fn window_slides_and_frees_capacity() {
        let limiter = RateLimiter::new(1);
        let sender = WorkerId::new();
        let t0 = Instant::now();

        assert!(limiter.check(sender, t0).is_ok());
        assert!(limiter.check(sender, t0).is_err());

        let later = t0 + Duration::from_secs(61);
        assert!(limiter.check(sender, later).is_ok());
    }

    /// R65: a sender that stops sending must not hold a map entry
    /// forever. One runtime process serves every run of a repository for
    /// as long as it stays resident, so per-retired-worker `Vec<Instant>`
    /// entries are an unbounded leak. Any later check by any other sender
    /// must sweep entries whose whole window has drained.
    #[test]
    fn a_retired_sender_is_forgotten_once_its_window_drains() {
        let limiter = RateLimiter::new(3);
        let retired = WorkerId::new();
        let live = WorkerId::new();
        let t0 = Instant::now();

        assert!(limiter.check(retired, t0).is_ok());
        assert_eq!(limiter.tracked_senders(), 1);

        // The retired sender never sends again; a different sender's
        // check after the window must evict the stale entry.
        let later = t0 + Duration::from_secs(61);
        assert!(limiter.check(live, later).is_ok());
        assert_eq!(
            limiter.tracked_senders(),
            1,
            "the retired sender's drained entry must be swept, not leaked"
        );
    }

    #[test]
    fn tracks_each_sender_independently() {
        let limiter = RateLimiter::new(1);
        let a = WorkerId::new();
        let b = WorkerId::new();
        let now = Instant::now();

        assert!(limiter.check(a, now).is_ok());
        assert!(limiter.check(a, now).is_err());
        assert!(limiter.check(b, now).is_ok());
    }
}
