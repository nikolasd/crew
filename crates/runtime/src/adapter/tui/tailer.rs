//! The polling transcript tailer: reads from a vendor transcript file at
//! the cursor's byte offset, hands complete-line batches (and the
//! advanced cursor) to the caller, and leaves partial trailing lines
//! unconsumed. Persistence of the cursor is the *caller's* job -- the
//! tailer itself only tracks its in-memory position.

use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::watch;

use super::{Cursor, TranscriptFormat, TuiEvent, last_emitting_index};

/// Tails one transcript file with one [`TranscriptFormat`].
pub struct TranscriptTailer {
    path: PathBuf,
    format: Arc<dyn TranscriptFormat>,
    cursor: Cursor,
    poll: Duration,
}

impl TranscriptTailer {
    #[must_use]
    pub fn new(
        path: PathBuf,
        format: Arc<dyn TranscriptFormat>,
        cursor: Cursor,
        poll: Duration,
    ) -> Self {
        Self {
            path,
            format,
            cursor,
            poll,
        }
    }

    /// The current in-memory cursor (advanced by each successful poll).
    #[must_use]
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }

    /// Reads everything past the cursor and parses complete lines.
    /// Returns `None` when nothing new was consumed -- the file is
    /// missing (not yet created by the vendor), has no new bytes, or the
    /// new bytes form only a partial (unterminated) line.
    pub async fn poll_once(&mut self) -> Option<(Vec<(TuiEvent, Cursor)>, Cursor)> {
        let raw = self.read_from_cursor().await?;
        if raw.is_empty() {
            return None;
        }
        // Consume through the last complete line so a later poll never
        // re-reads bytes we have already taken ownership of, regardless of
        // whether this batch produced events.
        let consumed = match raw.iter().rposition(|&b| b == b'\n') {
            Some(pos) => pos + 1,
            None => return None,
        };
        let tagged = self.format.parse(&raw, &self.cursor);
        if tagged.is_empty() {
            self.cursor.offset += consumed as u64;
            return None;
        }
        // The batch's durable cursor rides the last *emitting* event, never
        // the unconditional last index: a trailing run of TurnEnded/Raw (the
        // common idle shape) emits nothing, so attaching the cursor there
        // would point before an already-journaled event and a crash would
        // re-tail and re-journal it. Every entry after the emitting index
        // emits nothing, so covering them too is still correct; a no-emit
        // batch persists no cursor at all.
        let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
        let batch_cursor = match last_emitting_index(&events) {
            Some(i) => tagged[i].1.clone(),
            None => self.cursor.clone(),
        };
        self.cursor.offset += consumed as u64;
        self.cursor.last_entry_id = batch_cursor.last_entry_id.clone();
        Some((tagged, batch_cursor))
    }

    async fn read_from_cursor(&self) -> Option<Vec<u8>> {
        let mut file = tokio::fs::File::open(&self.path).await.ok()?;
        file.seek(SeekFrom::Start(self.cursor.offset)).await.ok()?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw).await.ok()?;
        Some(raw)
    }

    /// Moves the tailer onto a background task that polls on the
    /// configured interval, calling `on_batch` for every batch of newly
    /// consumed events with the cursor those events advance to (which the
    /// caller must persist transactionally with the events themselves).
    pub fn spawn(
        mut self,
        mut on_batch: impl FnMut(Vec<(TuiEvent, Cursor)>, Cursor) + Send + 'static,
    ) -> TailerHandle {
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                if let Some((tagged, cursor)) = self.poll_once().await {
                    on_batch(tagged, cursor);
                }
                tokio::select! {
                    _ = stop_rx.changed() => break,
                    () = tokio::time::sleep(self.poll) => {}
                }
            }
        });
        TailerHandle { stop_tx, task }
    }
}

/// Handle to a spawned tailer task; dropping it does *not* stop the
/// tailer (an adapter may stash it), only [`TailerHandle::stop`] does.
pub struct TailerHandle {
    stop_tx: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl TailerHandle {
    /// Stops the tailer promptly: signals the loop and aborts the task
    /// so no further batches are delivered after this returns.
    pub fn stop(&self) {
        let _ = self.stop_tx.send(true);
        self.task.abort();
    }
}
