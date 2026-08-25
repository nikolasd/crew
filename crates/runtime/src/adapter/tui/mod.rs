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
pub mod claude;
pub mod claude_conformance;
pub mod codex;
pub mod codex_conformance;
pub use codex::CodexTuiVendor;
mod discovery;
mod tailer;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crew_protocol::Classified;
use serde::{Deserialize, Serialize};

pub use adapter::{LaunchSpec, ResumeContext, TuiAdapter, TuiTimings, TuiVendor, VersionVerdict};
pub use claude::ClaudeTuiVendor;
pub use discovery::{DiscoveryError, find_transcript_by_nonce};
pub use tailer::{TailerHandle, TranscriptTailer};

use crate::config::crew::{AdapterConfig, CloseOnExit};
use crate::display::DisplayRegistry;

/// Static, daemon-lifetime inputs a real [`TuiVendor`] impl needs beyond
/// what its own trait methods compute -- threaded into
/// [`crate::adapter::registry::AdapterRegistry`] once, post-construction
/// (via `AdapterRegistry::set_tui_support`), mirroring exactly why
/// `AdapterMcpConfig`/`CoordinationBroker` are threaded the way they are:
/// the per-run pieces this bundles into a [`crate::display::PaneCoordinator`]
/// (`db`, `project_id`, `events_tx`) are only available from a run's own
/// [`crate::service::RunDriverContext`], never at registry-construction
/// time, so this struct carries everything that *is* available then --
/// the resolved `crewd` binary path, the state root, and the display
/// registry/config -- and `AdapterRegistry` builds a fresh
/// `PaneCoordinator` per run from this plus that run's own context.
#[derive(Clone)]
pub struct TuiSupport {
    pub display_registry: Arc<DisplayRegistry>,
    pub panes_dir: PathBuf,
    pub crewd_path: PathBuf,
    pub state_dir: PathBuf,
    pub close_on_exit: CloseOnExit,
    /// The config-forced display backend (`display.backend`, mapped via
    /// `crate::config::protocol_display_backend`; `None` for `Auto`,
    /// meaning "try the default chain").
    pub forced_backend: Option<crew_protocol::DisplayBackend>,
    /// `CrewConfig.adapters`, keyed by vendor name (`"claude"`, ...) --
    /// each vendor's own `TuiVendor` impl reads its own entry for `bin`/
    /// `permissionMode`/`model`/`sessionDir`/`extraArgs`.
    pub adapters: BTreeMap<String, AdapterConfig>,
    pub timings: TuiTimings,
}

/// A durable position in a vendor transcript: the byte offset of the
/// first unconsumed byte, plus the vendor id of the last consumed entry
/// (when the format carries one). Persisted transactionally with each
/// committed event batch, so replay after a crash is idempotent.
///
/// Deliberately *not* `deny_unknown_fields`: unlike a wire type exchanged
/// over IPC, this is a runtime-internal shape persisted to
/// `runs.transcript_cursor`. A future field added here must stay
/// readable by a rolled-back daemon reading its own previously stored
/// cursor -- `deny_unknown_fields` would instead fail that
/// deserialization outright, and a caller that cannot parse a stored
/// cursor at all has no better fallback than re-tailing the whole
/// transcript from the start (a resume-path decision left to a later
/// work package; this type just stays tolerant so that decision has
/// something to work with).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

impl TuiEvent {
    /// Whether mapping this event (`crate::adapter::tui::adapter::emit_tui_event`)
    /// produces at least one `crate::adapter::event_sink::AdapterEventPayload`.
    /// An exhaustive positive/negative match, deliberately never a
    /// wildcard arm: a future `TuiEvent` variant is a compile error here
    /// until it declares which side it's on, rather than silently
    /// inheriting whichever default a wildcard would have picked.
    #[must_use]
    pub fn emits_a_payload(&self) -> bool {
        match self {
            TuiEvent::AssistantText { .. }
            | TuiEvent::ToolActivity { .. }
            | TuiEvent::SessionMeta { .. } => true,
            TuiEvent::TurnEnded | TuiEvent::Raw { .. } => false,
        }
    }
}

/// The index within `events` of the last one that emits at least one
/// adapter event, or `None` if none of them do.
///
/// A tailed batch's advanced `Cursor` must be attached to exactly this
/// index, never to the batch's last index unconditionally: a trailing
/// run of `TurnEnded`/`Raw` entries (which is the common shape after a
/// worker finishes a turn and goes idle) emits nothing, so attaching the
/// cursor there would either lose it (if a later non-emitting entry
/// exists in the same channel message) or -- the bug this function
/// fixes -- leave the stored cursor pointing *before* an event that was
/// in fact already journaled, so a crash-restart re-tails and
/// re-journals that already-observed event. Both safety directions still
/// hold with this rule: every entry *after* the returned index emits
/// nothing, so the stored cursor covering them too is still exactly
/// correct; a batch where nothing emits (this returns `None`) persists
/// no cursor at all, safe because nothing durable happened to duplicate.
#[must_use]
pub fn last_emitting_index(events: &[TuiEvent]) -> Option<usize> {
    events.iter().rposition(TuiEvent::emits_a_payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Cursor` is persisted (`runs.transcript_cursor`), never exchanged
    /// over the wire, so it must tolerate an unrecognized field a *newer*
    /// version of this daemon wrote -- a rolled-back daemon reading its
    /// own previously stored cursor must not fail closed into treating it
    /// as absent (which would force a full re-tail). Regression guard for
    /// the field once carrying `deny_unknown_fields`.
    #[test]
    fn cursor_deserializes_tolerantly_past_an_unknown_field() {
        let json = r#"{"offset":42,"lastEntryId":"abc","futureField":"ignored"}"#;
        let cursor: Cursor = serde_json::from_str(json).expect("unknown fields are tolerated");
        assert_eq!(
            cursor,
            Cursor {
                offset: 42,
                last_entry_id: Some("abc".to_string()),
            }
        );
    }

    fn text_event() -> TuiEvent {
        TuiEvent::AssistantText {
            text: Classified {
                class: crew_protocol::ContentClass::Visible,
                value: "hi".to_string(),
            },
            is_question: false,
            ts: None,
        }
    }

    #[test]
    fn last_emitting_index_skips_a_trailing_turn_ended() {
        let events = vec![text_event(), TuiEvent::TurnEnded];
        assert_eq!(last_emitting_index(&events), Some(0));
    }

    #[test]
    fn last_emitting_index_skips_a_trailing_raw() {
        let events = vec![
            text_event(),
            TuiEvent::Raw {
                entry_type: "unknown".to_string(),
            },
        ];
        assert_eq!(last_emitting_index(&events), Some(0));
    }

    #[test]
    fn last_emitting_index_finds_the_last_of_several_emitting_events() {
        let events = vec![text_event(), text_event(), TuiEvent::TurnEnded];
        assert_eq!(last_emitting_index(&events), Some(1));
    }

    #[test]
    fn last_emitting_index_is_none_when_nothing_in_the_batch_emits() {
        let events = vec![
            TuiEvent::TurnEnded,
            TuiEvent::Raw {
                entry_type: "unknown".to_string(),
            },
        ];
        assert_eq!(last_emitting_index(&events), None);
    }

    #[test]
    fn last_emitting_index_is_none_for_an_empty_batch() {
        assert_eq!(last_emitting_index(&[]), None);
    }
}
