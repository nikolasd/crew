//! TUI-mode worker observation: TUI workers are never observed by parsing
//! their terminal output -- observation happens by tailing the vendor
//! CLI's own transcript file (session JSONL) with a durable byte-offset
//! cursor, so a crashed daemon re-tails from its stored cursor with zero
//! duplicated events.
//!
//! This module owns the vendor-agnostic pieces: the [`Cursor`], the
//! [`TuiEvent`] normalization target, the [`TranscriptFormat`] trait each
//! vendor implements, the shared JSONL cursor math
//! ([`parse_jsonl_chunk`]), the polling [`TranscriptTailer`], and
//! nonce-based transcript discovery ([`find_transcript_by_nonce`]).

mod adapter;
mod discovery;
mod tailer;

use crew_protocol::Classified;
use serde::{Deserialize, Serialize};

pub use adapter::{LaunchSpec, TuiAdapter, TuiTimings, TuiVendor, VersionVerdict};
pub use discovery::{DiscoveryError, find_transcript_by_nonce};
pub use tailer::{TailerHandle, TranscriptTailer};

/// A durable position in a vendor transcript: the byte offset of the
/// first unconsumed byte, plus the vendor id of the last consumed entry
/// (when the format carries one). Persisted transactionally with each
/// committed event batch, so replay after a crash is idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cursor {
    pub offset: u64,
    pub last_entry_id: Option<String>,
}

impl Cursor {
    /// The cursor before anything has been consumed.
    #[must_use]
    pub fn start() -> Self {
        Self {
            offset: 0,
            last_entry_id: None,
        }
    }
}

/// One parsed vendor transcript entry, pre-normalization. Free text is
/// already [`Classified`] here -- classification happens at the parse
/// boundary, before anything can travel toward the durable journal.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    AssistantText {
        text: Classified<String>,
        is_question: bool,
        ts: Option<String>,
    },
    ToolActivity {
        tool: String,
        detail: Classified<String>,
        ts: Option<String>,
    },
    SessionMeta {
        vendor_session_id: String,
    },
    TurnEnded,
    /// An entry the format does not understand. Unknown entries degrade
    /// to `Raw` (carrying only the vendor's own type tag) rather than
    /// failing the tail -- vendor formats drift.
    Raw {
        entry_type: String,
    },
}

/// A vendor transcript format: given a raw chunk that starts at
/// `cursor.offset`, parse only *complete* lines and return the advanced
/// cursor. A partial trailing line is left unconsumed, so re-parsing
/// from any returned cursor is idempotent at arbitrary byte splits.
pub trait TranscriptFormat: Send + Sync {
    fn parse(&self, raw: &[u8], cursor: &Cursor) -> (Vec<TuiEvent>, Cursor);
}

/// Shared JSONL cursor math for [`TranscriptFormat`] implementations:
/// walks complete newline-terminated lines in `raw` (which starts at
/// `cursor.offset`), advancing the offset by the exact byte length of
/// each consumed line plus its newline. Blank lines are consumed without
/// producing events; a line that is not valid JSON degrades to
/// [`TuiEvent::Raw`] with `entry_type: "parse_error"`.
///
/// `map_line` turns one parsed JSON entry into its events plus the
/// vendor entry id (if any) recorded as `last_entry_id`.
pub fn parse_jsonl_chunk<F>(raw: &[u8], cursor: &Cursor, map_line: F) -> (Vec<TuiEvent>, Cursor)
where
    F: Fn(&serde_json::Value) -> (Vec<TuiEvent>, Option<String>),
{
    let mut events = Vec::new();
    let mut consumed: usize = 0;
    let mut last_entry_id = cursor.last_entry_id.clone();

    let mut rest = raw;
    while let Some(newline_pos) = rest.iter().position(|&b| b == b'\n') {
        let line = &rest[..newline_pos];
        consumed += newline_pos + 1;
        rest = &rest[newline_pos + 1..];

        let trimmed = line
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .map(|start| {
                let end = line
                    .iter()
                    .rposition(|b| !b.is_ascii_whitespace())
                    .expect("a non-whitespace byte exists");
                &line[start..=end]
            })
            .unwrap_or(&[]);
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_slice::<serde_json::Value>(trimmed) {
            Ok(value) => {
                let (line_events, entry_id) = map_line(&value);
                events.extend(line_events);
                if let Some(entry_id) = entry_id {
                    last_entry_id = Some(entry_id);
                }
            }
            Err(_) => events.push(TuiEvent::Raw {
                entry_type: "parse_error".to_string(),
            }),
        }
    }

    (
        events,
        Cursor {
            offset: cursor.offset + consumed as u64,
            last_entry_id,
        },
    )
}
