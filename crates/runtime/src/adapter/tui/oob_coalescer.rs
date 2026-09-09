//! Coalesces rapid out-of-band-input notifications into one journaled
//! row per idle window, rather than one row per read.
//!
//! The other half of the echo-storm fix: `display::terminal_reply`'s filter already
//! keeps a vendor's own terminal-reply storm from reaching
//! `on_user_input` at all, but a human genuinely typing directly into an
//! attached pane -- bypassing the adapter, which is what `OutOfBandInput`
//! exists to notice -- can still fire it once per keystroke read. Each
//! firing used to journal its own row and set `needsReconciliation`
//! immediately; this batches a burst of firings into one row (a count
//! and a span), journaled once the burst goes quiet.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

/// How long a run of out-of-band input must stay quiet before the batch
/// it belongs to is flushed. Short enough that a single, isolated
/// keystroke still journals promptly; long enough to coalesce a redraw
/// storm's rapid successive firings (measured at 50-200ms apart) into
/// one row per second or so of continuous activity, rather than one row
/// per firing.
///
/// Shared with `display::terminal_reply::ReplyFilter`'s own idle flush:
/// a byte that filter holds back (possibly the first byte of a
/// sequence a later read completes) is released as content on the same
/// cadence, rather than inventing a second timer value to keep in sync.
pub(crate) const IDLE_WINDOW: Duration = Duration::from_secs(1);

/// Notifies [`OobCoalescer::spawn`]'s background task of one out-of-band
/// input occurrence. Cloneable so every viewer connection's own callback
/// can hold one without sharing a lock.
#[derive(Clone)]
pub struct OobCoalescer {
    tx: mpsc::UnboundedSender<()>,
}

impl OobCoalescer {
    /// Spawns the coalescing background task and returns a handle to
    /// notify it. `flush(input_count, span_ms)` is called once per
    /// batch, after `IDLE_WINDOW` has passed with no further
    /// notifications -- never more often, however fast `notify` is
    /// called. Dropping every clone of the returned handle ends the
    /// task (its `notify` channel closes, which is also observed as a
    /// final flush of any in-progress batch, matching a caller that
    /// stops without an explicit shutdown call).
    #[must_use]
    pub fn spawn(mut flush: impl FnMut(u64, u64) + Send + 'static) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            loop {
                // Wait for the first notification of a new batch --
                // idling here costs nothing and keeps a run with no
                // out-of-band input at all from ever flushing anything.
                if rx.recv().await.is_none() {
                    return;
                }
                let mut count: u64 = 1;
                let first_at = Instant::now();
                loop {
                    match tokio::time::timeout(IDLE_WINDOW, rx.recv()).await {
                        Ok(Some(())) => count += 1,
                        Ok(None) => {
                            flush(count, ms_since(first_at));
                            return;
                        }
                        Err(_elapsed) => break,
                    }
                }
                flush(count, ms_since(first_at));
            }
        });
        Self { tx }
    }

    /// Records one out-of-band input occurrence, extending the current
    /// batch (or starting a new one if the last batch already flushed).
    /// Never blocks and never fails observably: the channel is
    /// unbounded, and a send after the task has already exited (a race
    /// with shutdown) is silently dropped -- losing one notification
    /// during shutdown is a missed row, not a wrong one.
    pub fn notify(&self) {
        let _ = self.tx.send(());
    }
}

fn ms_since(instant: Instant) -> u64 {
    u64::try_from(instant.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::OobCoalescer;

    #[tokio::test]
    async fn a_single_notification_flushes_as_one_input_with_a_zero_span() {
        let flushes: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&flushes);
        let coalescer = OobCoalescer::spawn(move |count, span_ms| {
            recorded.lock().unwrap().push((count, span_ms));
        });

        coalescer.notify();
        tokio::time::sleep(super::IDLE_WINDOW * 2).await;

        let got = flushes.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "exactly one batch must flush: {got:?}");
        assert_eq!(got[0].0, 1, "a single notification is one input");
    }

    #[tokio::test]
    async fn a_burst_within_the_idle_window_coalesces_into_one_row() {
        let flushes: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&flushes);
        let coalescer = OobCoalescer::spawn(move |count, span_ms| {
            recorded.lock().unwrap().push((count, span_ms));
        });

        for _ in 0..50 {
            coalescer.notify();
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tokio::time::sleep(super::IDLE_WINDOW * 2).await;

        let got = flushes.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "50 rapid notifications inside one idle window must coalesce into one row, not 50: {got:?}"
        );
        assert_eq!(
            got[0].0, 50,
            "the one row must count every notification: {got:?}"
        );
    }

    #[tokio::test]
    async fn two_bursts_separated_by_a_quiet_gap_flush_as_two_rows() {
        let flushes: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&flushes);
        let coalescer = OobCoalescer::spawn(move |count, span_ms| {
            recorded.lock().unwrap().push((count, span_ms));
        });

        coalescer.notify();
        coalescer.notify();
        tokio::time::sleep(super::IDLE_WINDOW * 2).await;
        coalescer.notify();
        tokio::time::sleep(super::IDLE_WINDOW * 2).await;

        let got = flushes.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            2,
            "a quiet gap must start a new batch, not extend the old one: {got:?}"
        );
        assert_eq!(got[0].0, 2);
        assert_eq!(got[1].0, 1);
    }
}
