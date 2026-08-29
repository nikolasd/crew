//! Post-submit verification that the vendor recorded the whole prompt
//! (CREW-13).
//!
//! CREW-4 fixed the mechanism that corrupted long prompts — they now
//! travel as one bracketed paste in paced chunks, and a vendor that stops
//! reading its stdin fails the write loudly. What neither of those covers
//! is the vendor accepting every byte and then truncating in its own
//! composer: a paste-size cap, an input-length limit, a render bug. From
//! the adapter's side that is indistinguishable from success.
//!
//! Nonce presence does not answer it. The discovery nonce is *appended*
//! to the prompt (`"<prompt> [crew:<nonce>]"`), so a transcript containing
//! it proves only that the tail arrived — which is precisely the half that
//! survived the original CREW-4 corruption. The head is what goes missing,
//! so the check has to compare the recorded text itself.
//!
//! Read once, straight off the discovered transcript, rather than through
//! the event pipeline: a user entry produces no `TuiEvent` and must not
//! start producing one (for a fresh start it would duplicate the prompt
//! the adapter shell already journaled; for a resume it would fabricate
//! prior conversation as new). See [`super::TranscriptFormat::recorded_prompt`].

use serde_json::Value;

use super::TranscriptFormat;

/// Why a prompt's delivery could not be confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptVerdict {
    /// The vendor recorded the prompt exactly as injected.
    Intact,
    /// The vendor recorded a *different* prompt: it accepted the bytes and
    /// then truncated or altered them. The run must fail — this is the
    /// silent fragment CREW-4 exists to prevent, arriving by another road.
    Corrupted {
        expected_len: usize,
        recorded_len: usize,
        detail: String,
    },
    /// No entry carrying this prompt was found, or the vendor's format
    /// does not expose recorded prompts. Not a failure: verification is
    /// best-effort and must never fail a run it cannot judge.
    Unverifiable,
}

/// Compares the prompt this adapter injected against what the vendor
/// actually recorded in `transcript`.
///
/// `injected` is the exact text handed to the PTY, nonce included. The
/// candidate entry is the one whose recorded prompt contains `nonce`,
/// which is what ties it to *this* injection rather than to prior
/// conversation in a resumed session.
pub(crate) fn verify_recorded_prompt(
    transcript: &[u8],
    format: &dyn TranscriptFormat,
    injected: &str,
    nonce: &str,
) -> PromptVerdict {
    let expected = normalize(injected);
    for line in transcript.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(recorded) = format.recorded_prompt(&entry) else {
            continue;
        };
        if !recorded.contains(nonce) {
            continue;
        }
        let recorded = normalize(&recorded);
        if recorded == expected {
            return PromptVerdict::Intact;
        }
        return PromptVerdict::Corrupted {
            expected_len: expected.chars().count(),
            recorded_len: recorded.chars().count(),
            detail: describe(&expected, &recorded),
        };
    }
    PromptVerdict::Unverifiable
}

/// The comparison form. `paste_chunks` folds CR and CRLF to LF on the way
/// out, and vendors differ on trailing whitespace, so neither difference
/// is evidence of corruption.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end()
        .to_string()
}

/// A short, human-readable account of how the two differ — enough for an
/// operator to recognize truncation without dumping either prompt into a
/// log. Deliberately reports only *shape*: prompts are user content, and
/// this string reaches an error message.
fn describe(expected: &str, recorded: &str) -> String {
    if expected.ends_with(recorded) {
        return "the vendor recorded only the END of the prompt: its beginning was lost"
            .to_string();
    }
    if expected.starts_with(recorded) {
        return "the vendor recorded only the START of the prompt: its end was lost".to_string();
    }
    if expected.contains(recorded) {
        return "the vendor recorded only a middle fragment of the prompt".to_string();
    }
    "the vendor's recorded prompt does not match the one that was sent".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::tui::{ClaudeTuiVendor, TuiVendor};
    use std::path::PathBuf;

    fn claude_format() -> std::sync::Arc<dyn TranscriptFormat> {
        ClaudeTuiVendor::new(PathBuf::from("/w"), vec![]).format()
    }

    fn claude_user_line(text: &str) -> Vec<u8> {
        let entry = serde_json::json!({
            "type": "user",
            "sessionId": "sess-1",
            "message": {"role": "user", "content": text},
        });
        let mut bytes = serde_json::to_vec(&entry).expect("serialize");
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn an_exactly_recorded_prompt_is_intact() {
        let injected = "do the thing [crew:n1]";
        let transcript = claude_user_line(injected);
        assert_eq!(
            verify_recorded_prompt(&transcript, claude_format().as_ref(), injected, "crew:n1"),
            PromptVerdict::Intact
        );
    }

    /// The CREW-4 shape, arriving by a different road: the nonce is
    /// appended, so a truncated head still carries it and still passes
    /// discovery. Only comparing the text catches this.
    #[test]
    fn a_prompt_whose_head_was_lost_is_corrupted_not_intact() {
        let injected = "line one\nline two\nline three [crew:n1]";
        let transcript = claude_user_line("line three [crew:n1]");
        match verify_recorded_prompt(&transcript, claude_format().as_ref(), injected, "crew:n1") {
            PromptVerdict::Corrupted { detail, .. } => {
                assert!(
                    detail.contains("beginning was lost"),
                    "detail was: {detail}"
                );
            }
            other => panic!("a lost head must be corruption, got {other:?}"),
        }
    }

    #[test]
    fn a_prompt_whose_tail_was_lost_is_corrupted() {
        // The nonce still has to be present, or nothing ties the entry to
        // this injection -- so the truncation here is of the middle.
        let injected = "alpha beta gamma [crew:n1]";
        let transcript = claude_user_line("alpha [crew:n1]");
        assert!(matches!(
            verify_recorded_prompt(&transcript, claude_format().as_ref(), injected, "crew:n1"),
            PromptVerdict::Corrupted { .. }
        ));
    }

    #[test]
    fn a_carriage_return_difference_is_not_corruption() {
        // `paste_chunks` folds CR to LF on the way out (CREW-4), so a
        // vendor recording LF where the caller wrote CRLF is agreement,
        // not disagreement.
        let injected = "first\r\nsecond [crew:n1]";
        let transcript = claude_user_line("first\nsecond [crew:n1]");
        assert_eq!(
            verify_recorded_prompt(&transcript, claude_format().as_ref(), injected, "crew:n1"),
            PromptVerdict::Intact
        );
    }

    #[test]
    fn a_transcript_with_no_matching_entry_is_unverifiable_not_corrupt() {
        let transcript = claude_user_line("someone else's prompt [crew:other]");
        assert_eq!(
            verify_recorded_prompt(
                &transcript,
                claude_format().as_ref(),
                "mine [crew:n1]",
                "crew:n1"
            ),
            PromptVerdict::Unverifiable,
            "a run must never fail over a prompt this check could not find"
        );
    }

    /// A resumed session's transcript holds prior turns. Only the entry
    /// carrying THIS nonce may be compared, or an earlier, different
    /// prompt would read as corruption.
    #[test]
    fn prior_conversation_in_the_transcript_is_ignored() {
        let mut transcript = claude_user_line("an older, unrelated prompt");
        transcript.extend(claude_user_line("the new one [crew:n1]"));
        assert_eq!(
            verify_recorded_prompt(
                &transcript,
                claude_format().as_ref(),
                "the new one [crew:n1]",
                "crew:n1"
            ),
            PromptVerdict::Intact
        );
    }

    #[test]
    fn a_format_that_exposes_no_recorded_prompt_is_unverifiable() {
        struct Silent;
        impl TranscriptFormat for Silent {
            fn parse(
                &self,
                _raw: &[u8],
                _cursor: &crate::adapter::tui::Cursor,
            ) -> Vec<(crate::adapter::tui::TuiEvent, crate::adapter::tui::Cursor)> {
                Vec::new()
            }
        }
        let transcript = claude_user_line("anything [crew:n1]");
        assert_eq!(
            verify_recorded_prompt(&transcript, &Silent, "anything [crew:n1]", "crew:n1"),
            PromptVerdict::Unverifiable,
            "the default trait impl must disable verification, never fail it"
        );
    }

    #[test]
    fn malformed_lines_are_skipped_rather_than_failing_the_check() {
        let mut transcript = b"{not json at all\n".to_vec();
        transcript.extend(claude_user_line("real [crew:n1]"));
        assert_eq!(
            verify_recorded_prompt(
                &transcript,
                claude_format().as_ref(),
                "real [crew:n1]",
                "crew:n1"
            ),
            PromptVerdict::Intact
        );
    }
}
