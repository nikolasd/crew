//! Recognizes terminal-protocol reply/report sequences a real terminal
//! emulator generates on a viewer's behalf -- never something a human
//! typed -- so [`crate::display::attach`]'s `serve_viewer` can keep
//! forwarding them to the vendor's PTY (it is the only path that ever
//! answers the vendor's own escape queries) without treating them as
//! out-of-band keyboard input.
//!
//! The echo storm this exists for: a vendor TUI's redraw-driven cursor
//! position (CPR) and device attributes (DA) queries, relayed through
//! `crewd attach`'s mirrored output to whatever real terminal is watching
//! -- a human's, or, confirmed in production, tmux hosting the attach
//! pane in its own window/pane -- come back as replies on the SAME
//! socket ordinary keystrokes arrive on. Left unfiltered, every reply
//! journaled an `OutOfBandInput` event and set the run's
//! `needsReconciliation` flag: 99 in 20 seconds of pure idle, measured
//! against a live tmux pane answering a synthetic query storm.
//!
//! The patterns below match only sequences with a fixed shape a real
//! terminal emits automatically, never something a keyboard driver
//! produces: an arrow key (`ESC[A`) or a function/navigation key
//! (`ESC[3~`) share the `CSI ... final-byte` grammar but neither their
//! parameter shape nor final byte collides with any pattern here.

use std::sync::LazyLock;

use regex::bytes::Regex;

/// Every terminal-reply/report shape this module recognizes, as one
/// alternation so a single scan finds them all:
///
/// * Cursor Position Report reply -- `CSI Pn ; Pn R` (a real terminal's
///   answer to the vendor's `ESC[6n` query).
/// * Primary Device Attributes reply -- `CSI ? Pn (; Pn)* c`.
/// * Secondary Device Attributes reply -- `CSI > Pn ; Pn ; Pn c`.
/// * Device Status Report "terminal OK" reply -- `CSI 0 n`.
/// * Focus in/out reports -- `CSI I` / `CSI O` (unsolicited, sent when
///   the terminal's own focus changes, never a keystroke).
/// * Bracketed-paste start/end markers -- `CSI 200 ~` / `CSI 201 ~`.
///   These wrap a paste's real content rather than replacing it: only
///   the marker bytes match here, so pasted text between them is left
///   alone and still counts as genuine input.
static TERMINAL_REPLY_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x-u)
        \x1b\[ \d+ ; \d+ R                  # CPR reply
      | \x1b\[ \? [\d;]* c                  # primary DA reply
      | \x1b\[ >  [\d;]* c                  # secondary DA reply
      | \x1b\[ 0 n                          # DSR terminal-OK reply
      | \x1b\[ [IO]                         # focus in / focus out report
      | \x1b\[ 20[01] ~                     # bracketed-paste start/end marker
    ",
    )
    .expect("terminal reply pattern must compile")
});

/// Removes every recognized terminal-reply/report sequence from `bytes`,
/// returning only the residue a human could plausibly have typed. A
/// mixed read (real keystrokes interleaved with replies, plausible once
/// a terminal is answering queries mid-session, since both share one
/// duplex stream) keeps its real bytes, untouched and in order, with
/// only the recognized sequences removed.
///
/// Never call this to decide what reaches the vendor process -- the
/// original, unfiltered bytes are still forwarded there unconditionally
/// (a real terminal's reply is the only answer the vendor's own query
/// ever gets); this is for the journaling/reconciliation decision only.
pub fn strip_terminal_replies(bytes: &[u8]) -> Vec<u8> {
    TERMINAL_REPLY_PATTERN
        .replace_all(bytes, &b""[..])
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::strip_terminal_replies;

    /// The exact byte pairs captured live from a real tmux pane hosting
    /// `crewd attach`, answering a fake vendor's `ESC[6n ESC[c` query
    /// every 200ms with nobody typing (CREW-81's reproduction). This is
    /// the measured positive control: a filter that fails to recognize
    /// this exact shape does not fix the bug that was actually observed.
    const CAPTURED_CPR_DA_REPLY: &[u8] = b"\x1b[17;67R\x1b[?1;2;4c";

    #[test]
    fn the_captured_cpr_da_reply_pair_strips_to_nothing() {
        assert_eq!(strip_terminal_replies(CAPTURED_CPR_DA_REPLY), b"");
    }

    /// The captured burst: 146 coalesced CPR+DA reply pairs arriving in
    /// one 2493-byte socket read (measured live -- tmux/the kernel batch
    /// replies under load rather than delivering one read per query).
    /// The filter must treat a whole batch as zero keystrokes, not
    /// (however implausibly) as one.
    #[test]
    fn a_coalesced_burst_of_146_replies_in_one_read_strips_to_nothing() {
        let burst: Vec<u8> = CAPTURED_CPR_DA_REPLY.repeat(146);
        assert_eq!(strip_terminal_replies(&burst), b"");
    }

    #[test]
    fn a_genuine_keystroke_survives_untouched() {
        assert_eq!(strip_terminal_replies(b"hello"), b"hello");
    }

    #[test]
    fn a_reply_interleaved_with_a_real_keystroke_leaves_only_the_keystroke() {
        let mixed = [CAPTURED_CPR_DA_REPLY, b"q", CAPTURED_CPR_DA_REPLY].concat();
        assert_eq!(strip_terminal_replies(&mixed), b"q");
    }

    #[test]
    fn an_arrow_key_is_never_mistaken_for_a_reply() {
        // `ESC[A` (cursor up) shares the CSI introducer but neither the
        // parameter shape nor the final byte of any recognized pattern.
        assert_eq!(strip_terminal_replies(b"\x1b[A"), b"\x1b[A");
    }

    #[test]
    fn a_navigation_key_ending_in_tilde_is_never_mistaken_for_a_paste_marker() {
        // `ESC[3~` (Delete) -- shares the `~` terminator bracketed paste
        // markers use, but not the `200`/`201` parameter.
        assert_eq!(strip_terminal_replies(b"\x1b[3~"), b"\x1b[3~");
    }

    #[test]
    fn bracketed_paste_markers_strip_but_the_pasted_text_between_them_survives() {
        let pasted = b"\x1b[200~pasted text\x1b[201~";
        assert_eq!(strip_terminal_replies(pasted), b"pasted text");
    }

    #[test]
    fn a_focus_report_strips_to_nothing() {
        assert_eq!(strip_terminal_replies(b"\x1b[I"), b"");
        assert_eq!(strip_terminal_replies(b"\x1b[O"), b"");
    }

    #[test]
    fn a_secondary_device_attributes_reply_strips_to_nothing() {
        assert_eq!(strip_terminal_replies(b"\x1b[>1;10;0c"), b"");
    }

    #[test]
    fn a_device_status_report_ready_reply_strips_to_nothing() {
        assert_eq!(strip_terminal_replies(b"\x1b[0n"), b"");
    }

    #[test]
    fn empty_input_strips_to_empty() {
        assert_eq!(strip_terminal_replies(b""), b"");
    }
}
