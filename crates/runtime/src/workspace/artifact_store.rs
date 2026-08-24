//! Artifact store for persisting and retrieving workspace artifacts.
//!
//! Stores artifacts with full metadata (kind, SHA-256, length, media type,
//! storage path, run_id) and supports bounded base64 chunked fetch.

use crew_protocol::{Artifact, ArtifactFetchResult, ArtifactId, ArtifactKind, ArtifactListResult};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;

/// The maximum number of content bytes one [`ArtifactStore::fetch_chunked`]
/// call may return, regardless of the requested length. A caller reads a
/// larger artifact by following `next_offset`; the cap is what keeps a
/// single RPC response bounded no matter what a caller asks for.
pub const ARTIFACT_FETCH_MAX_BYTES: u64 = 256 * 1024;

/// Default ceiling on the total bytes a persistent store may hold, used
/// when `workspace.artifactMaxBytes` is not configured. Patches and
/// conflict reports are small; this bound exists so a runaway publisher
/// cannot fill the state directory.
pub const DEFAULT_ARTIFACT_STORE_MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum ArtifactStoreError {
    #[error("artifact not found: {0}")]
    NotFound(ArtifactId),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(String),
    /// The stored bytes do not hash to the digest their metadata claims.
    /// A mismatch means the bytes are not the artifact, so they are
    /// refused rather than served with a warning.
    #[error("artifact {artifact_id} digest mismatch: expected {expected}, computed {actual}")]
    DigestMismatch {
        artifact_id: ArtifactId,
        expected: String,
        actual: String,
    },
}

/// Hex-encoded SHA-256 of `content`, in the same form `Artifact::sha256`
/// carries. Shared by every producer so a publisher and the store agree
/// on the encoding.
pub(crate) fn sha256_hex(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    hex::encode(hasher.finalize())
}

/// Artifact store with metadata tracking; optionally persisted on disk so
/// journaled artifact ids survive a daemon restart.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    /// Maps artifact ID to its metadata and content.
    artifacts: Arc<RwLock<HashMap<ArtifactId, StoredArtifact>>>,
    /// Base directory for on-disk storage (optional). Content lives at
    /// `objects/<sha256>` (content-addressed: identical bytes share one
    /// file and a stale filename can never alias a different artifact);
    /// metadata lives at `index/<artifact_id>.json`.
    storage_dir: Option<PathBuf>,
    /// Ceiling on the total stored bytes; a publish that would exceed it
    /// is refused with a typed error.
    max_total_bytes: u64,
}

#[derive(Debug, Clone)]
struct StoredArtifact {
    metadata: Artifact,
    content: Vec<u8>,
}

impl ArtifactStore {
    /// Creates a new in-memory artifact store (no persistence, unbounded --
    /// the pre-persistence behavior, kept for tests and embedded use).
    pub fn new() -> Self {
        ArtifactStore {
            artifacts: Arc::new(RwLock::new(HashMap::new())),
            storage_dir: None,
            max_total_bytes: u64::MAX,
        }
    }

    /// Opens an artifact store persisted under `base_dir`, loading the
    /// on-disk index so artifacts published before a daemon restart stay
    /// fetchable. An index entry whose content file is missing or no
    /// longer hashes to its recorded digest is skipped (with a warning),
    /// never served.
    ///
    /// # Errors
    /// Returns [`ArtifactStoreError::Io`] if the storage directories
    /// cannot be created or the index cannot be scanned.
    pub fn with_storage(
        base_dir: PathBuf,
        max_total_bytes: u64,
    ) -> Result<Self, ArtifactStoreError> {
        std::fs::create_dir_all(base_dir.join("objects"))?;
        std::fs::create_dir_all(base_dir.join("index"))?;
        let loaded = load_index(&base_dir)?;
        Ok(ArtifactStore {
            artifacts: Arc::new(RwLock::new(loaded)),
            storage_dir: Some(base_dir),
            max_total_bytes,
        })
    }

    /// Stores an artifact with full metadata.
    ///
    /// The caller-supplied `artifact.sha256` is verified against the bytes
    /// before anything is written: rejecting at publish keeps a corrupt
    /// artifact out of the store entirely, which is strictly better than
    /// discovering it at fetch time.
    ///
    /// # Errors
    /// Returns [`ArtifactStoreError::DigestMismatch`] if the declared
    /// digest disagrees with `content`, or [`ArtifactStoreError::Io`] if
    /// the on-disk mirror cannot be written.
    pub async fn store(
        &self,
        artifact: Artifact,
        content: Vec<u8>,
    ) -> Result<ArtifactId, ArtifactStoreError> {
        let id = artifact.artifact_id;

        let actual = sha256_hex(&content);
        if actual != artifact.sha256 {
            return Err(ArtifactStoreError::DigestMismatch {
                artifact_id: id,
                expected: artifact.sha256,
                actual,
            });
        }

        // Ceiling check under the write lock, before anything durable
        // happens, so a refused publish leaves no trace on disk.
        let mut artifacts = self.artifacts.write().await;
        let stored_total: u64 = artifacts
            .values()
            .map(|stored| stored.metadata.byte_length)
            .sum();
        if stored_total.saturating_add(content.len() as u64) > self.max_total_bytes {
            return Err(ArtifactStoreError::Storage(format!(
                "artifact store ceiling exceeded: {stored_total} stored + {} new > {} max bytes",
                content.len(),
                self.max_total_bytes
            )));
        }

        if let Some(ref storage_dir) = self.storage_dir {
            // Content first (content-addressed; identical bytes dedupe),
            // index entry second: an index entry always points at content
            // that already exists.
            let object_path = storage_dir.join("objects").join(&artifact.sha256);
            if !object_path.exists() {
                write_atomically(&object_path, &content)?;
            }
            let index_path = storage_dir.join("index").join(format!("{id}.json"));
            let metadata_json = serde_json::to_vec(&artifact)
                .map_err(|e| ArtifactStoreError::Storage(e.to_string()))?;
            write_atomically(&index_path, &metadata_json)?;
        }

        artifacts.insert(
            id,
            StoredArtifact {
                metadata: artifact,
                content,
            },
        );

        Ok(id)
    }

    /// Fetches an artifact's metadata.
    pub async fn fetch(&self, id: &ArtifactId) -> Result<Artifact, ArtifactStoreError> {
        let artifacts = self.artifacts.read().await;
        artifacts
            .get(id)
            .map(|a| a.metadata.clone())
            .ok_or(ArtifactStoreError::NotFound(*id))
    }

    /// Fetches an artifact's content (bytes).
    pub async fn fetch_content(&self, id: &ArtifactId) -> Result<Vec<u8>, ArtifactStoreError> {
        let artifacts = self.artifacts.read().await;
        artifacts
            .get(id)
            .map(|a| a.content.clone())
            .ok_or(ArtifactStoreError::NotFound(*id))
    }

    /// Fetches a bounded chunk of an artifact's content as base64.
    ///
    /// `length` is clamped to [`ARTIFACT_FETCH_MAX_BYTES`] silently:
    /// `next_offset`/`complete` already tell the caller to come back, so a
    /// clamp is correct pagination rather than a failure.
    ///
    /// # Errors
    /// Returns [`ArtifactStoreError::NotFound`] for an unknown id, or
    /// [`ArtifactStoreError::DigestMismatch`] if the stored bytes no longer
    /// hash to the digest recorded at publish time.
    pub async fn fetch_chunked(
        &self,
        id: &ArtifactId,
        offset: u64,
        length: u64,
    ) -> Result<ArtifactFetchResult, ArtifactStoreError> {
        let artifacts = self.artifacts.read().await;
        let stored = artifacts.get(id).ok_or(ArtifactStoreError::NotFound(*id))?;

        let metadata = &stored.metadata;
        let content = &stored.content;

        // Verify the *whole* content: the metadata carries one digest for
        // the artifact, not a per-chunk one, so a chunk alone proves
        // nothing. This is what catches on-disk tampering once the
        // storage mirror is the source of truth.
        let actual = sha256_hex(content);
        if actual != metadata.sha256 {
            return Err(ArtifactStoreError::DigestMismatch {
                artifact_id: *id,
                expected: metadata.sha256.clone(),
                actual,
            });
        }

        // Calculate the chunk, bounded by the per-call ceiling.
        let length = length.min(ARTIFACT_FETCH_MAX_BYTES);
        let end = std::cmp::min(offset + length, content.len() as u64);
        let chunk = if offset >= content.len() as u64 {
            vec![]
        } else {
            content[offset as usize..end as usize].to_vec()
        };

        // Base64 encode the chunk
        let content_base64 = base64_encode(&chunk);

        let next_offset = if end < content.len() as u64 {
            Some(end)
        } else {
            None
        };

        Ok(ArtifactFetchResult {
            artifact: metadata.clone(),
            content_base64,
            next_offset,
            complete: next_offset.is_none(),
        })
    }

    /// Lists all artifacts, optionally filtered by kind.
    pub async fn list(&self, kind: Option<ArtifactKind>) -> ArtifactListResult {
        let artifacts = self.artifacts.read().await;
        let filtered: Vec<Artifact> = if let Some(k) = kind {
            artifacts
                .values()
                .filter(|a| a.metadata.kind == k)
                .map(|a| a.metadata.clone())
                .collect()
        } else {
            artifacts.values().map(|a| a.metadata.clone()).collect()
        };

        ArtifactListResult {
            artifacts: filtered,
        }
    }
}

impl Default for ArtifactStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Writes `bytes` to `path` via a same-directory temp file + rename, so a
/// crash mid-write can never leave a half-written object or index entry
/// under the final name.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> Result<(), ArtifactStoreError> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Loads every `index/<artifact_id>.json` whose content object still
/// exists and still hashes to its recorded digest. Entries failing either
/// check are skipped with a warning -- served-but-wrong is the one
/// unacceptable outcome.
fn load_index(
    base_dir: &std::path::Path,
) -> Result<HashMap<ArtifactId, StoredArtifact>, ArtifactStoreError> {
    let mut loaded = HashMap::new();
    for entry in std::fs::read_dir(base_dir.join("index"))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let metadata: Artifact = match std::fs::read(&path)
            .map_err(ArtifactStoreError::from)
            .and_then(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|e| ArtifactStoreError::Storage(e.to_string()))
            }) {
            Ok(metadata) => metadata,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "artifact_index_entry_unreadable");
                continue;
            }
        };
        let object_path = base_dir.join("objects").join(&metadata.sha256);
        let content = match std::fs::read(&object_path) {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(
                    artifact_id = %metadata.artifact_id,
                    path = %object_path.display(),
                    error = %err,
                    "artifact_content_missing_on_load"
                );
                continue;
            }
        };
        if sha256_hex(&content) != metadata.sha256 {
            tracing::warn!(
                artifact_id = %metadata.artifact_id,
                "artifact_content_digest_mismatch_on_load; refusing to serve it"
            );
            continue;
        }
        loaded.insert(metadata.artifact_id, StoredArtifact { metadata, content });
    }
    Ok(loaded)
}

/// Simple base64 encoding.
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i] as u32;
        let b1 = if i + 1 < data.len() {
            data[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < data.len() {
            data[i + 2] as u32
        } else {
            0
        };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);

        if i + 1 < data.len() {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        if i + 2 < data.len() {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }

        i += 3;
    }
    result
}
