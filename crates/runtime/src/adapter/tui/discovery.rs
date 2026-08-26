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

/// Deepest directory nesting scanned below the transcript root. Codex is
/// the deepest built-in layout: `~/.codex/sessions/YYYY/MM/DD/*.jsonl` --
/// three date directories plus the rollout file -- so 4 covers it with the
/// other vendors' flat/slug layouts to spare.
const MAX_DEPTH: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("nonce must be non-empty")]
    InvalidNonce,
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
/// [`DiscoveryError::InvalidNonce`] when `nonce` is empty.
/// [`DiscoveryError::Timeout`] when no such file appears within
/// `timeout`. An unreadable root or entry is treated as "no match yet",
/// never as an error -- the vendor may still be creating it.
pub async fn find_transcript_by_nonce(
    root_dir: &Path,
    started_at: SystemTime,
    nonce: &str,
    timeout: Duration,
) -> Result<PathBuf, DiscoveryError> {
    if nonce.is_empty() {
        return Err(DiscoveryError::InvalidNonce);
    }

    let root_dir = root_dir.to_path_buf();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // Filesystem I/O is moved to a blocking thread pool to avoid
        // blocking the tokio runtime. The scan itself is synchronous
        // std::fs operations, invoked via spawn_blocking and awaited.
        let root_for_scan = root_dir.clone();
        let nonce_for_scan = nonce.to_string();
        let scan_result = tokio::task::spawn_blocking(move || {
            scan(&root_for_scan, started_at, &nonce_for_scan, MAX_DEPTH)
        })
        .await;

        if let Ok(Some(found)) = scan_result {
            return Ok(found);
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(DiscoveryError::Timeout {
                nonce: nonce.to_string(),
                root: root_dir,
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
