//! Nonce-based transcript discovery: a TUI adapter injects a unique
//! `[crew:<nonce>]` tag into its first prompt, then finds the vendor's
//! session transcript by polling the vendor's session root for a `.jsonl`
//! file, modified at/after the worker started, that contains the nonce.
//! This avoids guessing vendor file-naming schemes entirely.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How often the session root is rescanned while waiting for the vendor
/// to create/flush its transcript.
const DISCOVERY_POLL: Duration = Duration::from_millis(150);

/// Recursion bound below the session root (vendors nest at most one or
/// two directory levels; a runaway symlink farm must not be walked).
const MAX_DEPTH: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error(
        "no transcript containing nonce {nonce:?} appeared under {root} within {timeout:?} \
         (vendor CLI may not have created its session file)"
    )]
    Timeout {
        nonce: String,
        root: PathBuf,
        timeout: Duration,
    },
}

/// Polls `root_dir` (recursive, depth <= 3) for a `*.jsonl` file whose
/// mtime is at/after `started_at` and whose content contains `nonce`.
///
/// # Errors
/// [`DiscoveryError::Timeout`] when no such file appears within
/// `timeout`. An unreadable root or entry is treated as "no match yet",
/// never as an error -- the vendor may still be creating it.
pub async fn find_transcript_by_nonce(
    root_dir: &Path,
    started_at: SystemTime,
    nonce: &str,
    timeout: Duration,
) -> Result<PathBuf, DiscoveryError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // The scan is synchronous filesystem I/O over a small vendor
        // session directory; at this size, offloading to a blocking
        // thread would cost more than it saves.
        if let Some(found) = scan(root_dir, started_at, nonce, MAX_DEPTH) {
            return Ok(found);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(DiscoveryError::Timeout {
                nonce: nonce.to_string(),
                root: root_dir.to_path_buf(),
                timeout,
            });
        }
        tokio::time::sleep(DISCOVERY_POLL.min(deadline - now)).await;
    }
}

fn scan(dir: &Path, started_at: SystemTime, nonce: &str, depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 1
                && let Some(found) = scan(&path, started_at, nonce, depth - 1)
            {
                return Some(found);
            }
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let modified_recently = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|mtime| mtime >= started_at);
        if !modified_recently {
            continue;
        }
        let Ok(content) = std::fs::read(&path) else {
            continue;
        };
        if content
            .windows(nonce.len())
            .any(|window| window == nonce.as_bytes())
        {
            return Some(path);
        }
    }
    None
}
