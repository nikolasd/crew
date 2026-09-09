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

const ESC: u8 = 0x1b;

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
///
/// Operates on one already-complete buffer. A socket read can split a
/// reply sequence across two reads (measured live: a lone trailing `c`
/// or a truncated `ESC[?1;2;4` with the closing `c` in the next read),
/// which this function alone cannot see across -- use
/// [`ReplyFilter`] to filter a stream of reads instead.
pub fn strip_terminal_replies(bytes: &[u8]) -> Vec<u8> {
    TERMINAL_REPLY_PATTERN
        .replace_all(bytes, &b""[..])
        .into_owned()
}

/// The longest sequence this module recognizes, generously bounded (the
/// longest in practice, a multi-parameter DA reply, is well under this).
/// Bounds how far back [`ReplyFilter`] looks for a trailing, possibly
/// incomplete sequence to hold back across reads.
const MAX_SEQUENCE_LEN: usize = 32;

/// Filters a STREAM of reads (unlike the bare [`strip_terminal_replies`],
/// which only sees one buffer at a time and so cannot detect a reply
/// sequence a socket read split in two). Carries at most
/// [`MAX_SEQUENCE_LEN`] unresolved trailing bytes forward from one call
/// to the next: if a read ends with an `ESC` that has no recognized
/// sequence completed after it yet, those bytes are held rather than
/// released as residue, and reconsidered once the next read's bytes are
/// appended. A held-back byte still reaches the vendor's PTY on the
/// caller's own unfiltered path -- this only delays the
/// journaling/reconciliation decision for it, never delivery.
///
/// Holding back is driven by volume (more bytes arriving), not time --
/// on its own, a lone `ESC` with nothing after it (a human pressing
/// Escape once, alone, and then doing nothing else -- in a claude TUI,
/// Escape interrupts the turn, about as consequential an out-of-band
/// action as exists) would be held forever, since nothing ever arrives
/// to age it out. The caller MUST also poll [`ReplyFilter::has_pending`]
/// on an idle tick (`serve_viewer` uses the same window
/// `adapter::tui::oob_coalescer::IDLE_WINDOW` coalescing already ticks
/// on) and release whatever [`ReplyFilter::take_pending`] returns as
/// content once that tick fires with no intervening read -- a real
/// terminal answers within milliseconds, so anything still held after a
/// full idle window was never going to complete.
///
/// One instance per viewer connection: `serve_viewer` owns one, for the
/// lifetime of that connection's read loop -- never shared across
/// connections, or one viewer's partial sequence could swallow another
/// viewer's keystroke.
#[derive(Default)]
pub struct ReplyFilter {
    pending: Vec<u8>,
}

impl ReplyFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `chunk` to whatever was held back from the previous call,
    /// strips every recognized sequence found in the now-safe prefix,
    /// and returns the residue -- holding back a new trailing prefix in
    /// turn if this chunk itself ends mid-sequence.
    pub fn filter(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let split_at = Self::safe_prefix_len(&self.pending);
        let residue = strip_terminal_replies(&self.pending[..split_at]);
        self.pending.drain(..split_at);
        residue
    }

    /// Whether this filter is currently holding back any bytes. The
    /// caller checks this on its idle tick to decide whether
    /// [`Self::take_pending`] has anything worth releasing.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Drains and returns whatever this filter is currently holding
    /// back, unconditionally treating it as content -- called only after
    /// an idle window has passed with no further read, per this
    /// struct's own doc comment on why that must happen at all.
    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// The length of `buf`'s prefix that is safe to filter and release
    /// now: everything up to (but not including) a trailing `ESC` --
    /// within the last `MAX_SEQUENCE_LEN` bytes of whatever remains
    /// after every already-complete recognized sequence -- that has no
    /// recognized sequence completed after it within `buf` itself. That
    /// `ESC` might be the first byte of a sequence the next read
    /// completes, so it and everything after it stay held back.
    fn safe_prefix_len(buf: &[u8]) -> usize {
        let after_last_match = TERMINAL_REPLY_PATTERN
            .find_iter(buf)
            .last()
            .map_or(0, |m| m.end());
        let trailing = &buf[after_last_match..];
        let scan_from = trailing.len().saturating_sub(MAX_SEQUENCE_LEN);
        for i in (scan_from..trailing.len()).rev() {
            if trailing[i] == ESC {
                return after_last_match + i;
            }
        }
        buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{ReplyFilter, strip_terminal_replies};

    /// The exact byte pairs captured live from a real tmux pane hosting
    /// `crewd attach`, answering a fake vendor's `ESC[6n ESC[c` query
    /// every 200ms with nobody typing (the echo-storm investigation's own
    /// reproduction). This is the measured positive control: a filter
    /// that fails to recognize this exact shape does not fix the bug
    /// that was actually observed.
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

    // ------------------------------------------------------- ReplyFilter

    // Regression: measured live, a socket read split a reply sequence in
    // two -- a lone trailing `c` in one read, a truncated `ESC[?1;2;4`
    // missing its closing `c` in the next -- and the bare
    // `strip_terminal_replies` (seeing only one buffer at a time) leaked
    // both halves as residue, journaling a spurious `outOfBandInput` for
    // a storm the filter should have erased entirely.

    #[test]
    fn a_reply_sequence_split_across_two_reads_still_strips_to_nothing() {
        for split in 1..CAPTURED_CPR_DA_REPLY.len() {
            let mut filter = ReplyFilter::new();
            let mut residue = filter.filter(&CAPTURED_CPR_DA_REPLY[..split]);
            residue.extend(filter.filter(&CAPTURED_CPR_DA_REPLY[split..]));
            assert_eq!(
                residue, b"",
                "splitting the reply at byte {split} must not leak anything: got {residue:?}"
            );
        }
    }

    #[test]
    fn a_burst_of_replies_split_at_arbitrary_read_boundaries_strips_to_nothing() {
        // The exact shape measured live: 146 coalesced replies, chunked
        // at a boundary that does not align with any single reply's
        // 17-byte length.
        let burst: Vec<u8> = CAPTURED_CPR_DA_REPLY.repeat(146);
        let mut filter = ReplyFilter::new();
        let mut residue = Vec::new();
        for chunk in burst.chunks(7) {
            residue.extend(filter.filter(chunk));
        }
        assert_eq!(
            residue, b"",
            "a burst chunked at misaligned boundaries must not leak anything: {residue:?}"
        );
    }

    #[test]
    fn a_genuine_keystroke_still_survives_the_stream_filter() {
        let mut filter = ReplyFilter::new();
        assert_eq!(filter.filter(b"hello"), b"hello");
    }

    #[test]
    fn a_keystroke_immediately_after_a_split_reply_is_not_swallowed_with_it() {
        let mut filter = ReplyFilter::new();
        let mut residue = filter.filter(&CAPTURED_CPR_DA_REPLY[..10]);
        residue.extend(filter.filter(&[&CAPTURED_CPR_DA_REPLY[10..], b"q" as &[u8]].concat()));
        assert_eq!(residue, b"q");
    }

    #[test]
    fn a_lone_trailing_esc_is_held_back_then_released_once_enough_more_data_arrives() {
        let mut filter = ReplyFilter::new();
        // A bare ESC with nothing after it yet: held back, not leaked as
        // a spurious single-byte "keystroke".
        assert_eq!(filter.filter(b"\x1b"), b"");
        // Enough further content arrives without ever completing a
        // recognized sequence that the held-back ESC falls out of the
        // lookback window -- released as ordinary content, never
        // silently lost. This is volume-driven, not time-driven: see
        // the next test for the case where no further bytes ever come.
        let filler = vec![b'x'; super::MAX_SEQUENCE_LEN];
        let released = filter.filter(&filler);
        assert!(
            released.starts_with(b"\x1b"),
            "the held-back ESC must be released once enough data follows it, not dropped: {released:?}"
        );
    }

    // A lone Escape keypress with NO further bytes ever arriving (a
    // human presses Escape once -- in a claude TUI, this interrupts the
    // current turn -- and then does nothing else) has no volume to age
    // it out by the mechanism above. `has_pending`/`take_pending` are
    // what a caller uses to release it anyway, on its own idle tick, and
    // this is the case that matters: every other test here releases a
    // held-back byte by supplying more data, which this one deliberately
    // never does.
    #[test]
    fn a_lone_esc_with_no_further_bytes_is_still_pending_until_the_caller_releases_it_on_its_own_idle_tick()
     {
        let mut filter = ReplyFilter::new();
        assert_eq!(filter.filter(b"\x1b"), b"");
        assert!(
            filter.has_pending(),
            "a lone ESC with nothing after it must stay held, not be silently dropped"
        );
        assert_eq!(
            filter.take_pending(),
            b"\x1b",
            "the caller's own idle-tick release must recover exactly the held-back byte, not drop it"
        );
        assert!(
            !filter.has_pending(),
            "take_pending must actually drain, not just peek"
        );
    }
}
