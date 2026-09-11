//! Exercises the claude-protocol reconciliation reader's REAL (never
//! injected) parse path in a build shaped like the one that ships --
//! not an approximation of it.
//!
//! **What guarantees the seam's absence from a release build is the
//! `#[cfg(test)]` attribute itself, a compile-time property -- this file
//! does not, and structurally cannot, prove that.** If `#[cfg(test)]`
//! were ever removed from `reconcile::arm_drop_for_test`, leaving the
//! seam compiled into release builds but simply never armed, every test
//! in this file would still compile and pass unchanged: an unarmed seam
//! and an absent one are behaviourally identical from here. What this
//! file DOES prove is narrower and still worth having: that
//! `reconcile::find_gaps` works correctly through the real parse path in
//! a compilation where `cfg(test)` is off for this crate's own code --
//! which no test living inside `crates/runtime/src/` can ever do,
//! because `cfg!(test)` is true throughout `cargo test`'s compilation of
//! this crate's OWN unit-test binary. An integration test under
//! `crates/runtime/tests/` is different: it links `crew_runtime` as an
//! ordinary external dependency, and the `test` cfg is not applied to
//! that compilation -- so this file is, structurally, the one place able
//! to exercise the shipping parse path at all.
//!
//! `reconcile::find_gaps`/`TranscriptEntry`/`TranscriptEntryKind` are
//! `pub` for exactly this reason -- see that module's own doc comment.
//! The seam itself (`reconcile::arm_drop_for_test`) stays private to the
//! crate: this file cannot reach it, cannot import it, and cannot even
//! name it -- which is consistent with the attribute doing its job, not
//! a demonstration that it is.

use std::collections::HashSet;

use crew_runtime::adapter::claude_protocol::reconcile::{
    TranscriptEntry, TranscriptEntryKind, find_gaps,
};

fn entry(uuid: &str, kind: &str) -> String {
    format!(r#"{{"uuid":"{uuid}","type":"{kind}","message":{{}}}}"#)
}

/// The real parse path, exercised in a build where `cfg(test)` is off
/// for this crate's own code: every entry in the transcript, present in
/// `journaled_entry_ids` or not, is examined and correctly classified as
/// present or as a gap, with no unexplained loss. This does not by
/// itself prove the seam is absent here (see this file's own module doc
/// comment for why it structurally cannot) -- it proves the thing the
/// seam's absence is supposed to leave behind: the real parse, working.
#[test]
fn the_real_parse_drops_nothing_and_finds_exactly_the_true_gaps() {
    let transcript = [
        entry("u1", "user"),
        entry("a1", "assistant"),
        entry("a2", "assistant"),
        entry("a3", "assistant"),
    ]
    .join("\n");
    let journaled: HashSet<String> = ["u1".to_string(), "a1".to_string()].into_iter().collect();

    let (examined, gaps) = find_gaps(transcript.as_bytes(), &journaled);

    assert_eq!(
        examined, 4,
        "a production build must examine every entry in the transcript, not silently drop any \
         of them"
    );
    let mut gap_ids: Vec<&str> = gaps.iter().map(|g| g.entry_id.as_str()).collect();
    gap_ids.sort_unstable();
    assert_eq!(
        gap_ids,
        vec!["a2", "a3"],
        "a production build must find exactly the entries genuinely missing from the journal -- \
         no more, no fewer"
    );
    assert_eq!(
        gaps,
        vec![
            TranscriptEntry {
                entry_id: "a2".to_string(),
                kind: TranscriptEntryKind::Assistant,
            },
            TranscriptEntry {
                entry_id: "a3".to_string(),
                kind: TranscriptEntryKind::Assistant,
            },
        ]
    );
}

/// The other half: when every transcript entry genuinely was journaled,
/// a production build must find nothing at all -- proving the real
/// parse path is not itself a source of spurious gaps.
#[test]
fn the_real_parse_finds_no_gaps_when_the_journal_is_already_complete() {
    let transcript = [entry("u1", "user"), entry("a1", "assistant")].join("\n");
    let journaled: HashSet<String> = ["u1".to_string(), "a1".to_string()].into_iter().collect();

    let (examined, gaps) = find_gaps(transcript.as_bytes(), &journaled);

    assert_eq!(examined, 2);
    assert!(
        gaps.is_empty(),
        "a production build with a complete journal must find no gaps: {gaps:?}"
    );
}
