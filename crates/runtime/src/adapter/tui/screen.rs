//! What a vendor TUI is showing, derived from the bytes it wrote to the PTY.
//!
//! CREW-79 slice 1. This module carries only the normalization primitive and
//! its fixtures; it classifies nothing. Vendor predicates (`classify_surface`)
//! land on top of it in a later slice, so that the matching rule and the
//! vendor-specific phrases are reviewable separately.
//!
//! # The trap this module exists to close
//!
//! A vendor TUI does not write its screen as text. It positions **each word**
//! with its own cursor-column escape, so the trust dialog that renders as
//!
//! ```text
//! Quick safety check: Is this a project you created or one you trust?
//! ```
//!
//! reaches the PTY as `\x1b[2GQuick\x1b[8Gsafety\x1b[15Gcheck:\x1b[22GIs...`.
//! Strip the escapes and the visible text contains **no spaces at all**:
//! `Quicksafetycheck:Isthis...`.
//!
//! So the obvious predicate -- strip escapes, then search for
//! `"Yes, I trust this folder"` -- finds nothing on a screen that is
//! displaying exactly that phrase. This is not a hypothetical: it produced a
//! false negative during the live investigation that produced these fixtures,
//! reporting "no dialog on screen" against a screen showing the dialog, and
//! only the raw capture revealed why.
//!
//! [`TuiScreen::shows`] is therefore the *only* supported way to ask whether a
//! phrase is on screen. It compares with all whitespace removed from both
//! sides. No caller should match against the raw or the escape-stripped text
//! directly.
//!
//! # Known limitation, stated rather than papered over
//!
//! This is a normalization primitive, not a terminal emulator. It answers
//! "has this phrase appeared in the output so far", never "is it on screen
//! now" -- it does not model the cursor, erasure, scrolling, or the alternate
//! screen buffer, so a phrase that was drawn and then cleared still matches.
//!
//! That is sufficient for detecting a gate that is blocking startup, which is
//! what CREW-79 needs first. It is **not** sufficient for a predicate of the
//! form "the composer is up *and* no gate is up", because the gate half of
//! that test would stay true forever once the gate had appeared. A vendor
//! needing that shape needs a real screen model; `codex-composer-then-trust`
//! in the fixture set is the capture that proves the case is real.
//!
//! Memory is bounded in practice by how long a caller accumulates output --
//! the readiness cap for a startup poll. It is worth knowing that this is not
//! a small number for every vendor: `codex` emits ~780 KB of animated banner
//! within ten seconds of launch, of which the sign-in prompt occupies the
//! first ~1.6 KB. A future byte cap must not be a plain tail window, which
//! would discard exactly the gate it is meant to find.

// This slice deliberately ships the primitive with no production caller, so
// that the matching rule and its fixtures are reviewable on their own, before
// any vendor phrase depends on them. The tests below are the only callers
// today. DELETE THIS ATTRIBUTE in the slice that adds `classify_surface`: at
// that point every item here has a real caller, and leaving it would hide a
// genuinely unused item from the next reader.
#![allow(dead_code)]

/// The escape-stripped, whitespace-stripped view of a vendor TUI's output.
///
/// Built incrementally: [`Self::push`] normalizes only the new bytes, so a
/// caller polling a long-lived process does not re-scan everything it has
/// already seen.
#[derive(Debug, Default, Clone)]
pub(crate) struct TuiScreen {
    /// Output with escape sequences and all whitespace removed.
    normalized: String,
    /// Bytes handed to [`Self::push`] that did not form a complete escape
    /// sequence or UTF-8 scalar, carried into the next call. A PTY read can
    /// split either one at any byte.
    pending: Vec<u8>,
}

impl TuiScreen {
    /// A screen built from one complete slice of output.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        let mut screen = Self::default();
        screen.push(bytes);
        screen
    }

    /// Folds one PTY read into the screen.
    ///
    /// Safe to call with reads split at arbitrary byte boundaries: an escape
    /// sequence or multi-byte scalar cut in half is held in `pending` and
    /// completed by the next call, never emitted as stray text.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);

        let mut i = 0usize;
        while i < buf.len() {
            if buf[i] == 0x1b {
                match escape_len(&buf[i..]) {
                    EscapeScan::Complete(len) => {
                        i += len;
                    }
                    // An escape sequence split across reads: keep the tail.
                    EscapeScan::Incomplete => break,
                }
                continue;
            }
            // A control byte that is not an escape introducer carries no
            // visible text (BEL, backspace, the carriage returns the TUI
            // uses for positioning); whitespace is dropped by definition.
            if buf[i] < 0x20 || buf[i] == 0x7f {
                i += 1;
                continue;
            }
            match next_scalar(&buf[i..]) {
                ScalarScan::Complete(ch, len) => {
                    if !ch.is_whitespace() {
                        self.normalized.push(ch);
                    }
                    i += len;
                }
                // A multi-byte scalar split across reads: keep the tail.
                ScalarScan::Incomplete => break,
                // Not valid UTF-8 and never will be: drop the byte rather
                // than stall the scan on it.
                ScalarScan::Invalid => {
                    i += 1;
                }
            }
        }

        self.pending = buf.split_off(i);
    }

    /// Whether `phrase` is on screen, compared with all whitespace removed
    /// from both sides.
    ///
    /// **The only supported way to ask.** Callers pass the phrase exactly as
    /// it reads to a human -- `"Yes, I trust this folder"` -- and this
    /// handles the fact that the vendor never wrote those spaces. See the
    /// module docs for why matching the stripped text directly does not work.
    pub(crate) fn shows(&self, phrase: &str) -> bool {
        let needle: String = phrase.chars().filter(|c| !c.is_whitespace()).collect();
        if needle.is_empty() {
            return false;
        }
        self.normalized.contains(&needle)
    }

    /// The normalized text itself, for diagnostics and tests. Not a matching
    /// surface: use [`Self::shows`].
    #[cfg(test)]
    pub(crate) fn normalized(&self) -> &str {
        &self.normalized
    }
}

enum EscapeScan {
    Complete(usize),
    Incomplete,
}

/// The length of the escape sequence starting at `bytes[0] == 0x1b`.
///
/// Covers the three forms a vendor TUI actually emits: CSI (`ESC [` … final
/// byte in `@`–`~`), OSC (`ESC ]` … `BEL` or `ESC \`), and the two-byte
/// escapes used for charset and keypad mode. Anything else is treated as a
/// two-byte escape, which drops the introducer and one byte rather than
/// letting a stray `ESC` leak into the text.
fn escape_len(bytes: &[u8]) -> EscapeScan {
    if bytes.len() < 2 {
        return EscapeScan::Incomplete;
    }
    match bytes[1] {
        b'[' => {
            // CSI: parameter and intermediate bytes, then one final byte.
            let mut i = 2;
            while i < bytes.len() && (0x20..=0x3f).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() && (0x40..=0x7e).contains(&bytes[i]) {
                EscapeScan::Complete(i + 1)
            } else {
                EscapeScan::Incomplete
            }
        }
        b']' => {
            // OSC: terminated by BEL or by ST (`ESC \`).
            let mut i = 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return EscapeScan::Complete(i + 1);
                }
                if bytes[i] == 0x1b {
                    return if i + 1 < bytes.len() {
                        EscapeScan::Complete(i + 2)
                    } else {
                        EscapeScan::Incomplete
                    };
                }
                i += 1;
            }
            EscapeScan::Incomplete
        }
        _ => EscapeScan::Complete(2),
    }
}

enum ScalarScan {
    Complete(char, usize),
    Incomplete,
    Invalid,
}

/// The scalar starting at `bytes[0]`, or whether it is merely truncated.
///
/// The distinction matters: a truncated scalar must be carried to the next
/// read, while an invalid one must be dropped, or the scan stalls forever on
/// a byte that can never complete.
fn next_scalar(bytes: &[u8]) -> ScalarScan {
    let width = match bytes[0] {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return ScalarScan::Invalid,
    };
    if bytes.len() < width {
        return ScalarScan::Incomplete;
    }
    match std::str::from_utf8(&bytes[..width]) {
        Ok(s) => match s.chars().next() {
            Some(ch) => ScalarScan::Complete(ch, width),
            None => ScalarScan::Invalid,
        },
        Err(_) => ScalarScan::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/adapters/tui-screens")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
    }

    // ---------------------------------------------------------- the trap

    /// The reason this module exists, pinned against real captured bytes:
    /// the phrase is on screen, and the escape-stripped text does not
    /// contain it, because the vendor never wrote the spaces.
    #[test]
    fn a_phrase_on_screen_is_absent_from_the_escape_stripped_text() {
        let screen = TuiScreen::from_bytes(&fixture("claude-workspace-trust.raw"));
        assert!(
            !screen.normalized().contains("Yes, I trust this folder"),
            "the spaced phrase must NOT be findable -- if this ever passes, the \
             vendor changed how it paints and `shows` may no longer be needed"
        );
        assert!(
            screen.shows("Yes, I trust this folder"),
            "...but the same phrase must be found through `shows`"
        );
    }

    #[test]
    fn shows_finds_every_phrase_of_the_claude_trust_dialog() {
        let screen = TuiScreen::from_bytes(&fixture("claude-workspace-trust.raw"));
        for phrase in [
            "Accessing workspace:",
            "Quick safety check:",
            "Claude Code'll be able to read, edit, and execute files here.",
            "Yes, I trust this folder",
            "No, exit",
            "Enter to confirm",
        ] {
            assert!(screen.shows(phrase), "not found on screen: {phrase:?}");
        }
    }

    #[test]
    fn shows_finds_every_phrase_of_the_codex_trust_dialog() {
        let screen = TuiScreen::from_bytes(&fixture("codex-directory-trust.raw"));
        for phrase in [
            "Do you trust the contents of this directory?",
            "Yes, continue",
            "No, quit",
            "Press enter to continue",
        ] {
            assert!(screen.shows(phrase), "not found on screen: {phrase:?}");
        }
    }

    /// The other three first-run gates found live, so a later slice's
    /// predicates have a capture to be written against rather than a guess.
    #[test]
    fn shows_finds_the_remaining_first_run_gates() {
        let theme = TuiScreen::from_bytes(&fixture("claude-theme-picker.raw"));
        assert!(theme.shows("Choose the text style that looks best with your terminal"));

        let signin = TuiScreen::from_bytes(&fixture("claude-signin-method.raw"));
        assert!(signin.shows("Select login method:"));
        assert!(signin.shows("Claude account with subscription"));

        let codex = TuiScreen::from_bytes(&fixture("codex-signin.raw"));
        assert!(codex.shows("Sign in with ChatGPT"));
        assert!(codex.shows("Provide your own API key"));
    }

    /// A screen showing one vendor's gate must not match another's phrases --
    /// otherwise `shows` would be satisfied by anything and the tests above
    /// would be passing vacuously.
    #[test]
    fn shows_does_not_match_a_phrase_that_is_not_on_screen() {
        let screen = TuiScreen::from_bytes(&fixture("claude-workspace-trust.raw"));
        assert!(!screen.shows("Do you trust the contents of this directory?"));
        assert!(!screen.shows("Sign in with ChatGPT"));
        assert!(!screen.shows("this phrase appears on no vendor screen"));
    }

    /// The capture behind the module's stated limitation: codex paints its
    /// composer, accepts a pasted prompt into it, and only *then* raises its
    /// trust gate. Both are present in one capture, which is why a
    /// "composer up and no gate up" predicate cannot be built on this
    /// primitive alone.
    #[test]
    fn the_codex_capture_holds_the_composer_and_the_gate_at_once() {
        let screen = TuiScreen::from_bytes(&fixture("codex-composer-then-trust.raw"));
        assert!(screen.shows("Ask Codex to do anything"), "composer painted");
        assert!(
            screen.shows("Do you trust the contents of this directory?"),
            "and the gate arrived after it"
        );
    }

    // ------------------------------------------------- incremental reads

    /// A PTY read can split an escape sequence or a scalar at any byte, so
    /// the result must not depend on where the boundaries fall.
    #[test]
    fn chunked_pushes_agree_with_one_whole_push_at_every_split() {
        let bytes = fixture("claude-workspace-trust.raw");
        let whole = TuiScreen::from_bytes(&bytes);
        for chunk in [1usize, 2, 3, 5, 7, 64, 512] {
            let mut screen = TuiScreen::default();
            for slice in bytes.chunks(chunk) {
                screen.push(slice);
            }
            assert_eq!(
                screen.normalized(),
                whole.normalized(),
                "a {chunk}-byte read split changed the normalized screen"
            );
            assert!(screen.shows("Yes, I trust this folder"));
        }
    }

    #[test]
    fn a_multibyte_scalar_split_across_reads_is_not_corrupted() {
        let mut screen = TuiScreen::default();
        let bytes = "trust\u{2014}folder".as_bytes();
        screen.push(&bytes[..6]); // splits the em dash
        screen.push(&bytes[6..]);
        assert_eq!(screen.normalized(), "trust\u{2014}folder");
    }

    #[test]
    fn an_escape_sequence_split_across_reads_never_leaks_as_text() {
        let mut screen = TuiScreen::default();
        screen.push(b"ab\x1b[3");
        screen.push(b"8;5;2mcd");
        assert_eq!(screen.normalized(), "abcd");
    }

    // -------------------------------------------------------- unit rules

    #[test]
    fn every_kind_of_whitespace_is_removed_from_both_sides() {
        let screen = TuiScreen::from_bytes(b"one\ttwo\r\n three\x0bfour");
        assert_eq!(screen.normalized(), "onetwothreefour");
        assert!(screen.shows("one two"));
        assert!(screen.shows("  three\tfour  "));
    }

    #[test]
    fn osc_sequences_are_dropped_with_either_terminator() {
        let bel = TuiScreen::from_bytes(b"a\x1b]0;a window title\x07b");
        assert_eq!(bel.normalized(), "ab");
        let st = TuiScreen::from_bytes(b"a\x1b]0;a window title\x1b\\b");
        assert_eq!(st.normalized(), "ab");
    }

    #[test]
    fn an_empty_phrase_never_matches() {
        let screen = TuiScreen::from_bytes(&fixture("claude-workspace-trust.raw"));
        assert!(
            !screen.shows(""),
            "an empty phrase must not match everything"
        );
        assert!(!screen.shows("   "));
    }

    #[test]
    fn invalid_utf8_is_dropped_without_stalling_the_scan() {
        let screen = TuiScreen::from_bytes(b"be\xfffore\xfe after");
        assert_eq!(screen.normalized(), "beforeafter");
    }
}
