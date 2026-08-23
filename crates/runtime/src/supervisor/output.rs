//! Bounded stdio for supervised vendor processes: a line reader that can
//! never buffer an unbounded frame, and a rotating capture that can never
//! retain more than a fixed number of stderr bytes. A flooding or
//! malfunctioning vendor process can never force unbounded runtime memory
//! growth through either stream.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::io::AsyncReadExt;
use tokio::process::{ChildStderr, ChildStdout};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LinesCodec};

/// The hard ceiling on a single stdout frame (line). An adapter's own
/// protocol may declare a tighter limit via `SpawnSpec`, but this is the
/// supervisor's own default and absolute ceiling.
pub const MAX_STDOUT_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// The hard ceiling on total retained stderr bytes. Older bytes are
/// discarded first once the cap is reached.
pub const MAX_STDERR_CAPTURE_BYTES: usize = 25 * 1024 * 1024;

/// Receives a clone of every raw stdout frame decoded by any
/// `spawn_stdout_reader` task in this process. Installed once, by
/// `crewd conformance capture` only -- production never installs one,
/// and the cost when absent is a single `OnceLock::get` returning `None`.
static FRAME_TAP: std::sync::OnceLock<mpsc::UnboundedSender<Vec<u8>>> = std::sync::OnceLock::new();

/// Installs the process-wide raw-frame tap.
///
/// # Errors
/// Returns `Err` if a tap was already installed -- one capture session
/// per process, never two competing recorders.
pub(crate) fn install_frame_tap(tx: mpsc::UnboundedSender<Vec<u8>>) -> Result<(), &'static str> {
    FRAME_TAP
        .set(tx)
        .map_err(|_| "a frame tap is already installed")
}

fn tap_frame(bytes: &[u8]) {
    if let Some(tap) = FRAME_TAP.get() {
        // A closed receiver means the capture session ended; frames are
        // dropped rather than failing the supervised process.
        let _ = tap.send(bytes.to_vec());
    }
}
/// Spawns a task that decodes bounded newline-delimited frames from
/// `stdout` and forwards each to the returned channel. A frame exceeding
/// `max_frame_bytes` (or any I/O error) ends the task -- the codec never
/// buffers past the bound before detecting the overflow, so a flooding
/// process can never force unbounded memory growth here.
pub fn spawn_stdout_reader(stdout: ChildStdout, max_frame_bytes: usize) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let codec = LinesCodec::new_with_max_length(max_frame_bytes);
        let mut framed = FramedRead::new(stdout, codec);
        while let Some(item) = framed.next().await {
            match item {
                Ok(line) => {
                    tap_frame(line.as_bytes());
                    if tx.send(line.into_bytes()).await.is_err() {
                        break;
                    }
                }
                // An oversized frame or I/O error: stop forwarding rather
                // than attempt to resynchronize on a stream a flooding
                // process controls.
                Err(_) => break,
            }
        }
    });
    rx
}

/// A fixed-capacity byte buffer that discards its oldest bytes once full.
/// Backed by a ring buffer (`VecDeque`) rather than a plain `Vec`: async
/// reads typically arrive in small chunks, and evicting from the front of
/// a `Vec` is O(remaining length) per push (it must shift everything
/// after the removed prefix), which would turn a sustained flood into
/// repeated multi-megabyte memmoves. `VecDeque::drain` at the front is
/// O(evicted length) instead, independent of how much remains.
#[derive(Debug, Clone)]
pub struct RotatingCapture {
    buf: std::collections::VecDeque<u8>,
    cap: usize,
}

impl RotatingCapture {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            buf: std::collections::VecDeque::new(),
            cap,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes.iter().copied());
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.drain(0..excess);
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Spawns a task that reads raw bytes from `stderr` into a
/// [`RotatingCapture`] capped at `cap` bytes, returning a handle to read
/// the current snapshot at any time.
pub fn spawn_stderr_capture(mut stderr: ChildStderr, cap: usize) -> Arc<Mutex<RotatingCapture>> {
    let capture = Arc::new(Mutex::new(RotatingCapture::new(cap)));
    let capture_for_task = capture.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut guard = capture_for_task
                        .lock()
                        .expect("stderr capture mutex is never poisoned");
                    guard.push(&buf[..n]);
                }
            }
        }
    });
    capture
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotating_capture_discards_oldest_bytes_first() {
        let mut capture = RotatingCapture::new(10);
        capture.push(b"0123456789");
        capture.push(b"ABCDE");
        assert_eq!(capture.len(), 10);
        assert_eq!(capture.snapshot(), b"56789ABCDE");
    }
}
