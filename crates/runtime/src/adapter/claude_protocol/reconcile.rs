//! Reconciles a run's journal against claude's own durable transcript
//! (`~/.claude/projects/<slug>/<session>.jsonl`) -- the audit path this
//! adapter relies on precisely because the live control-channel stream
//! carries no sequence number a dropped message would be missed
//! against, so "the stream went quiet" and "nothing happened" are
//! indistinguishable from the stream alone. The vendor's own transcript
//! is written to disk continuously during the turn and is read here as
//! a plain file, never through the same in-process channel this
//! adapter already consumed events from -- reading the same live
//! stream twice would make this reconciliation agree with itself by
//! construction, which is the exact failure this mechanism exists to
//! stop repeating (screen recognition's own "the accumulator still says
//! the gate is on screen" shape, one layer up).
//!
//! **Checked against a live capture** (three real turns against
//! `claude_code_version` `2.1.268`): the entry shape below -- `uuid` as
//! the correlation id, `type` as the kind discriminant -- is exactly
//! what a real transcript carries for `user`/`assistant` entries, which
//! is all this module's own purpose needs regardless of the vendor's
//! full JSON shape. One thing the same capture found that this module
//! did not originally account for: a real transcript also carries
//! non-conversational bookkeeping entries (a `SessionStart` hook's own
//! error, observed as `type: "attachment"`) that DO carry a `uuid` but
//! never appear on the live control-channel stream at all -- see
//! [`TranscriptEntryKind::Attachment`]'s own doc comment for why that
//! matters to [`find_gaps`] specifically.

use std::collections::HashSet;

/// The coarse kind of one transcript entry -- closed, and never
/// widened to carry the entry's own content: this is the field a
/// journaled `ReconciliationCompleted` count could eventually be broken
/// down by, and the same "closed-set, no interpolated content" rule
/// `TerminalGrid::mark_unsupported`'s own `class` field follows applies
/// here for the identical reason (a caller-visible field, not a debug
/// aid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptEntryKind {
    /// A user-authored turn -- the prompt this adapter itself injected,
    /// or (for a resumed session) prior conversation.
    User,
    /// An assistant-authored turn: text, tool use, or both.
    Assistant,
    /// A vendor-authored bookkeeping/diagnostic record -- observed as
    /// `type: "attachment"` (a `SessionStart` hook's own error output,
    /// in the live capture that found this) -- written to the durable
    /// transcript but confirmed, across every capture taken so far,
    /// never to appear on the live control-channel stream at all.
    /// [`find_gaps`] excludes this kind from its own gap set for
    /// exactly that reason: an entry that never travels the stream by
    /// design is not a gap when it is absent from what the stream
    /// delivered. Still counted in `examined`, same as every other
    /// entry this module can parse -- only the gap set narrows, not the
    /// count of what was looked at.
    Attachment,
    /// Anything this module does not yet classify -- session metadata,
    /// a summary entry, or a future entry type the vendor adds. Grouped
    /// under one variant rather than one per unknown shape so a vendor
    /// addition never requires touching this enum's own callers; the
    /// PARSED `kind` this module reads directly (a small string) is
    /// enough to distinguish them without one Rust variant each.
    ///
    /// Deliberately NOT excluded from [`find_gaps`]'s own gap set, on
    /// the same basis [`Attachment`](Self::Attachment) IS excluded: this
    /// module only ever denylists a kind it has positively confirmed
    /// never streams. A kind it has not yet seen is `Other`, not
    /// `Attachment`, and stays gappable -- noisy (a false-positive gap
    /// the first time a genuinely new non-streaming kind appears) is
    /// the correct failure direction for a mechanism whose whole
    /// purpose is catching silence; silently excluding every unknown
    /// kind (an allowlist instead of a denylist) would make a real
    /// future drop of an unrecognized kind invisible by construction,
    /// which is the failure this module exists to prevent.
    Other,
}

/// One entry from claude's own transcript, reduced to what
/// reconciliation needs: an identity to correlate against what this run
/// already journaled, and a coarse kind to describe a gap by. Never
/// carries the entry's own text -- reconciliation counts and repairs,
/// it does not re-surface vendor content a second time outside the
/// path that already redacts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    /// The vendor's own per-entry identifier (`uuid` in every entry
    /// shape observed so far) -- the correlation key against what this
    /// run already journaled. Provisional field name pending a live
    /// capture, per this module's own doc comment.
    pub entry_id: String,
    pub kind: TranscriptEntryKind,
}

/// Parses claude's transcript into entries this module can reconcile,
/// tolerant of one malformed line the same way
/// `adapter::tui::verify::verify_recorded_prompt` is: a line that is
/// not valid JSON, or valid JSON this module does not recognize the
/// shape of, is skipped rather than failing the whole pass. A vendor
/// transcript is append-only and machine-written; a single torn write
/// at the tail (the file caught mid-flush) must not make an otherwise
/// intact transcript unreadable.
fn parse_transcript(bytes: &[u8]) -> Vec<TranscriptEntry> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(parse_transcript_line)
        .collect()
}

#[cfg(not(test))]
fn parse_transcript_line(line: &[u8]) -> Option<TranscriptEntry> {
    parse_transcript_line_uninjected(line)
}

/// The real parse, shared by both cfg bodies below -- the test build's
/// only addition is the drop seam wrapped around this, never a second
/// implementation of the parse itself. Mirrors
/// `TerminalGrid::mark_unsupported`'s own split for the identical
/// reason: the two builds must do the same real work and differ only in
/// the one property under test.
fn parse_transcript_line_uninjected(line: &[u8]) -> Option<TranscriptEntry> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let entry_id = value.get("uuid")?.as_str()?.to_string();
    let kind = match value.get("type").and_then(serde_json::Value::as_str) {
        Some("user") => TranscriptEntryKind::User,
        Some("assistant") => TranscriptEntryKind::Assistant,
        Some("attachment") => TranscriptEntryKind::Attachment,
        _ => TranscriptEntryKind::Other,
    };
    Some(TranscriptEntry { entry_id, kind })
}

#[cfg(test)]
thread_local! {
    /// Set by a test via [`arm_drop_for_test`], consumed by the very
    /// next parsed entry whose kind matches -- exactly once, so a test
    /// proves reconciliation catches ONE dropped event without having
    /// to also prove it does not swallow every subsequent one of the
    /// same kind. Thread-local, not a shared global: `cargo test` runs
    /// this crate's tests on a shared process, and a global flag would
    /// make one test's injection visible to another running
    /// concurrently on a different thread.
    static DROP_NEXT: std::cell::Cell<Option<TranscriptEntryKind>> = const { std::cell::Cell::new(None) };
}

/// Arms the test-only drop seam: the next transcript line whose parsed
/// kind equals `kind`, on THIS thread, is dropped as though it had
/// never been written -- simulating exactly the failure this
/// reconciliation mechanism exists to catch (a live event the vendor
/// recorded that this run's own event stream never delivered). Consumed
/// on first match; call again to drop a second one.
///
/// `#[cfg(test)]` only -- there is no non-test version of this function
/// at all, not a no-op body. A release build has no symbol, no thread
/// local, and no branch checking one: nothing here for a build flag or
/// config value to ever re-arm. See
/// `crates/runtime/tests/claude_protocol_reconcile.rs` for the proof
/// that this is true of the actual shipping binary, not merely of this
/// crate's own test compilation (`cfg!(test)` is true throughout that
/// compilation regardless, so a test living in THIS file could never
/// tell the difference on its own -- the same structural gap
/// `TerminalGrid::mark_unsupported`'s own doc comment names, closed the
/// same way: an external integration test linking this crate as an
/// ordinary dependency, where the `test` cfg is not applied).
#[cfg(test)]
pub(crate) fn arm_drop_for_test(kind: TranscriptEntryKind) {
    DROP_NEXT.with(|cell| cell.set(Some(kind)));
}

#[cfg(test)]
fn parse_transcript_line(line: &[u8]) -> Option<TranscriptEntry> {
    let entry = parse_transcript_line_uninjected(line)?;
    let armed = DROP_NEXT.with(std::cell::Cell::get);
    if armed == Some(entry.kind) {
        DROP_NEXT.with(|cell| cell.set(None));
        return None;
    }
    Some(entry)
}

/// Compares `transcript_bytes` against `journaled_entry_ids` (every
/// vendor entry id this run has already journaled, from whichever
/// events this adapter already emitted while the turn was live) and
/// reports every transcript entry NOT in that set as a gap -- present
/// in the vendor's own durable record, missing from what this run
/// actually journaled. Pure and side-effect-free: the caller owns
/// deciding how to repair each gap (journaling a
/// [`super::super::event_sink::AdapterEventPayload`] for it) and
/// counting how many of the attempts actually succeeded, since only the
/// caller holds the sink and the run/task/worker identity a repair
/// needs.
///
/// `examined` is the number of entries this pass actually parsed --
/// deliberately not the number of lines in the file (a malformed line
/// is not "examined", it could not be) -- so a transcript that parsed
/// as entirely empty (a truncated read, a wrong path, a vendor format
/// change) is visible as `examined == 0` rather than looking identical
/// to a transcript with nothing to reconcile.
///
/// **Not yet enforced, and it is the caller's obligation once it
/// exists, not this function's:** an `examined == 0` result must never
/// be journaled as an ordinary, successful `ReconciliationCompleted` --
/// refuse to journal it, or journal it distinguishably as a failure. A
/// pass that looked at nothing and a pass that found nothing wrong must
/// never read the same in the durable record. This function only
/// SURFACES the count; nothing yet acts on it, and it must before this
/// module has a real caller.
pub fn find_gaps(
    transcript_bytes: &[u8],
    journaled_entry_ids: &HashSet<String>,
) -> (u64, Vec<TranscriptEntry>) {
    let entries = parse_transcript(transcript_bytes);
    let examined = entries.len() as u64;
    let gaps: Vec<TranscriptEntry> = entries
        .into_iter()
        // `examined` above already counted every entry this module
        // could parse, `Attachment` included -- only the gap set
        // narrows here. A denylist (name the kinds confirmed never to
        // stream), not an allowlist (name the kinds allowed to be
        // gaps): see `TranscriptEntryKind::Other`'s own doc comment for
        // why an unrecognized future kind must stay gappable rather
        // than silently excluded.
        .filter(|entry| entry.kind != TranscriptEntryKind::Attachment)
        .filter(|entry| !journaled_entry_ids.contains(&entry.entry_id))
        .collect();
    (examined, gaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(uuid: &str, kind: &str) -> String {
        format!(r#"{{"uuid":"{uuid}","type":"{kind}","message":{{}}}}"#)
    }

    /// The floor every other test here depends on: a transcript with
    /// nothing this run already journaled must report every entry as a
    /// gap, and `examined` must equal the entry count, not the gap
    /// count -- the two are the same only by coincidence in this one
    /// test, never by construction.
    #[test]
    fn every_entry_is_a_gap_against_an_empty_journaled_set() {
        let transcript = [entry("u1", "user"), entry("a1", "assistant")].join("\n");
        let (examined, gaps) = find_gaps(transcript.as_bytes(), &HashSet::new());
        assert_eq!(examined, 2);
        assert_eq!(gaps.len(), 2);
    }

    /// The denylist, proven directly: an `attachment`-kind entry never
    /// journaled live is counted in `examined` (it was successfully
    /// parsed) but never reported as a gap (it is a confirmed
    /// never-streams kind) -- the exact case a live capture found real
    /// transcripts carry (a `SessionStart` hook's own error record).
    #[test]
    fn an_attachment_entry_is_examined_but_never_reported_as_a_gap() {
        let transcript = [entry("u1", "user"), entry("hook1", "attachment")].join("\n");
        let (examined, gaps) = find_gaps(transcript.as_bytes(), &HashSet::new());
        assert_eq!(examined, 2, "attachment entries are parsed, so they count");
        assert_eq!(
            gaps,
            vec![TranscriptEntry {
                entry_id: "u1".to_string(),
                kind: TranscriptEntryKind::User,
            }],
            "only the user entry may be a gap; the attachment entry must not be"
        );
    }

    /// The denylist's other half: a kind this module does not recognize
    /// at all (never `Other`'s own doc comment) still stays gappable --
    /// proving the choice is a denylist of confirmed non-streaming
    /// kinds, not an allowlist of `User`/`Assistant` alone.
    #[test]
    fn an_unrecognized_kind_still_counts_as_a_gap() {
        let transcript = entry("mystery1", "some-future-kind-this-module-has-never-seen");
        let (examined, gaps) = find_gaps(transcript.as_bytes(), &HashSet::new());
        assert_eq!(examined, 1);
        assert_eq!(gaps.len(), 1, "an unrecognized kind must remain gappable");
        assert_eq!(gaps[0].kind, TranscriptEntryKind::Other);
    }

    /// The ordinary case: everything the transcript recorded was also
    /// journaled live, so reconciliation finds nothing to repair --
    /// still `examined > 0`, distinguishing "checked and agreed" from
    /// "never checked".
    #[test]
    fn nothing_is_a_gap_when_the_journal_already_has_every_entry() {
        let transcript = [entry("u1", "user"), entry("a1", "assistant")].join("\n");
        let journaled: HashSet<String> = ["u1".to_string(), "a1".to_string()].into_iter().collect();
        let (examined, gaps) = find_gaps(transcript.as_bytes(), &journaled);
        assert_eq!(examined, 2);
        assert!(gaps.is_empty(), "must find no gaps: {gaps:?}");
    }

    /// A malformed line (a torn write at the tail, the shape this
    /// module does not recognize) is skipped, not fatal to the pass --
    /// and, the part worth pinning explicitly, it does NOT count toward
    /// `examined`: a line that could not be parsed was not examined.
    #[test]
    fn a_malformed_line_is_skipped_and_never_counted_as_examined() {
        let transcript = format!(
            "{}\nnot json at all\n{}",
            entry("u1", "user"),
            entry("a1", "assistant")
        );
        let (examined, gaps) = find_gaps(transcript.as_bytes(), &HashSet::new());
        assert_eq!(
            examined, 2,
            "the malformed line must not inflate the examined count"
        );
        assert_eq!(gaps.len(), 2);
    }

    /// An empty transcript must report `examined == 0`, never silently
    /// look identical to "reconciled and found nothing wrong" -- the
    /// distinction `ReconciliationCompleted`'s own doc comment exists
    /// for, proven here at the level this module owns.
    #[test]
    fn an_empty_transcript_examines_nothing() {
        let (examined, gaps) = find_gaps(b"", &HashSet::new());
        assert_eq!(examined, 0);
        assert!(gaps.is_empty());
    }

    /// The drop seam, exercised directly: arm it for one kind, and the
    /// very next entry of that kind is absent from the parsed result --
    /// not merely absent from `journaled_entry_ids`, absent from
    /// PARSING, exactly simulating a live event the adapter's own
    /// stream never delivered. A second entry of the SAME kind
    /// afterward must survive untouched, proving the seam consumes
    /// itself rather than dropping every match.
    #[test]
    fn the_drop_seam_removes_exactly_the_next_matching_entry() {
        arm_drop_for_test(TranscriptEntryKind::Assistant);
        let transcript = [
            entry("u1", "user"),
            entry("a1", "assistant"),
            entry("a2", "assistant"),
        ]
        .join("\n");
        let (examined, gaps) = find_gaps(transcript.as_bytes(), &HashSet::new());
        // The dropped entry was never parsed at all, so it does not
        // count as examined either -- from this pass's own point of
        // view, a dropped live event and a line the vendor never wrote
        // are indistinguishable, which is the whole point: this proves
        // the SAME reconciliation logic that would need to catch a real
        // gap actually does.
        assert_eq!(
            examined, 2,
            "the dropped entry must not be counted as examined"
        );
        let gap_ids: Vec<&str> = gaps.iter().map(|g| g.entry_id.as_str()).collect();
        assert_eq!(
            gap_ids,
            vec!["u1", "a2"],
            "a1 must be the one missing entry"
        );
    }

    /// The realistic end-to-end shape this module exists for: an entry
    /// IS in the vendor's transcript, the live stream dropped it (so it
    /// is NOT in `journaled_entry_ids`), and reconciliation must report
    /// it as a gap -- not merely absent from the raw parse (the
    /// previous test) but absent from what this run believed it had
    /// journaled, which is the actual failure mode this whole module
    /// exists to catch.
    #[test]
    fn a_transcript_entry_missing_from_the_journal_is_reported_as_a_gap() {
        let transcript = [
            entry("u1", "user"),
            entry("a1", "assistant"),
            entry("a2", "assistant"),
        ]
        .join("\n");
        // The live stream delivered u1 and a1, but a2 (say, a second
        // assistant chunk arriving right as the process exited) never
        // reached this run's own event pipeline.
        let journaled: HashSet<String> = ["u1".to_string(), "a1".to_string()].into_iter().collect();
        let (examined, gaps) = find_gaps(transcript.as_bytes(), &journaled);
        assert_eq!(examined, 3);
        assert_eq!(
            gaps,
            vec![TranscriptEntry {
                entry_id: "a2".to_string(),
                kind: TranscriptEntryKind::Assistant,
            }]
        );
    }
}
