//! A minimal fixed-size terminal grid: models exactly the escape-sequence
//! families the vendor first-run-gate capture set actually contains, nothing
//! speculative. It answers "what text is on screen right now", the question
//! [`super::screen::TuiScreen`] cannot: that primitive only ever accumulates,
//! so a phrase it has seen once still matches after the screen has moved on.
//! See `screen.rs`'s own module docs for the capture that proves the
//! difference (`claude-trust-to-composer.raw`); this module's own tests
//! mirror that capture's test the other way -- see
//! [`tests::a_grid_stops_showing_an_answered_gate_once_the_terminal_clears_it`].
//!
//! Not a general VT100/xterm emulator: no colors/attributes are stored (SGR
//! is consumed and dropped -- this grid answers "what text is on screen",
//! never "how is it styled"), no separate alternate-screen buffer (see the
//! `?1049h` handling in [`TerminalGrid::apply_csi`] and its doc comment for
//! why entering one is a no-op rather than a real buffer swap), no tab
//! stops (none of the captures use HT).
//!
//! Ships with no production caller, same as `screen.rs`'s own first slice:
//! the matching layer that would call this (a later slice's
//! `classify_surface`) is not built yet, so `#[allow(dead_code)]` below is
//! deliberate, not an oversight -- delete it the same slice that adds a
//! real caller.
#![allow(dead_code)]

use super::screen::TuiScreen;

/// Fixed size, matching the probe harness's actual PTY winsize
/// (`struct.pack("HHHH", 40, 120, 0, 0)` -- the size crew's own adapter
/// requests) and comfortably covering the highest row/col any committed
/// capture's `CSI G`/`CSI H` parameters reference (34 / 119). A vendor that
/// used a larger terminal would need these constants revisited, not a
/// dynamic grid -- the fixture set is the sizing authority, not a guess at
/// generality.
pub(crate) const GRID_WIDTH: usize = 120;
pub(crate) const GRID_HEIGHT: usize = 40;

/// A rectangular grid of cells, with a cursor and a scroll region, folded
/// from raw PTY bytes.
#[derive(Debug, Clone)]
pub(crate) struct TerminalGrid {
    cells: Vec<Vec<char>>,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    scroll_top: usize,
    scroll_bottom: usize,
    /// Carries a `push` call's trailing incomplete escape sequence or
    /// scalar into the next one, exactly like `TuiScreen::push`'s own
    /// `pending` buffer -- a PTY read can split either at any byte, and
    /// this must not treat a split sequence's tail as literal text.
    pending: Vec<u8>,
    /// The first CSI final byte, CSI parameter, or single-byte escape this
    /// grid could not apply safely, recorded on the way to either return
    /// (production build) or panic (`#[cfg(test)]` build, so every
    /// existing test still catches this exactly as before) -- see
    /// [`Self::mark_unsupported`]. This is the fact a later slice's
    /// `classify_surface` must consult and map to `Undecided` before this
    /// module gets a real caller: a promise in a doc comment that "the
    /// next slice will handle this" does not fail when someone adds that
    /// caller without meeting it; a field that caller cannot silently
    /// skip does. See [`Self::unsupported`].
    unsupported: Option<String>,
}

impl Default for TerminalGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalGrid {
    pub(crate) fn new() -> Self {
        Self {
            cells: vec![vec![' '; GRID_WIDTH]; GRID_HEIGHT],
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: GRID_HEIGHT - 1,
            pending: Vec::new(),
            unsupported: None,
        }
    }

    /// The first unsupported escape sequence this grid encountered, if
    /// any -- see [`Self::mark_unsupported`] and `apply_csi`'s doc
    /// comment for what "unsupported" means. `None` in a `#[cfg(test)]`
    /// build proves nothing either way: every existing test that reaches
    /// an unsupported sequence panics before this could ever be read, by
    /// design (see [`Self::mark_unsupported`]).
    pub(crate) fn unsupported(&self) -> Option<&str> {
        self.unsupported.as_deref()
    }

    /// A grid built from one complete slice of output.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        let mut grid = Self::new();
        grid.push(bytes);
        grid
    }

    /// Folds one PTY read into the grid, in place.
    ///
    /// Re-derives the same recognition grammar `screen.rs`'s own
    /// `escape_len` already proved correct against this fixture set (the
    /// CSI parameter-byte range, OSC's BEL/ST terminator, the nF
    /// intermediate-byte family, the bare two-byte fallback) rather than
    /// literally calling it: `escape_len` is private to `screen.rs` and,
    /// more fundamentally, only ever returns a length -- enough for an
    /// accumulator that strips every escape and keeps nothing else. This
    /// grid needs to know WHAT each sequence was (its parameters, its
    /// final byte, an OSC's payload) to apply it, so [`classify_escape`]
    /// below returns that too. An earlier revision of this module's design
    /// claimed OSC recognition itself was new here because `screen.rs` had
    /// "no equivalent to reuse" for it; that was wrong -- `escape_len`
    /// already recognizes OSC's own variable-length terminator, correctly,
    /// via the same one function it uses for everything else. What is
    /// actually new here is extracting content, not detecting boundaries.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);

        let mut i = 0usize;
        while i < buf.len() {
            let b = buf[i];
            if b == 0x1b {
                match classify_escape(&buf[i..]) {
                    Escape::Csi {
                        params,
                        final_byte,
                        len,
                    } => {
                        self.apply_csi(params, final_byte);
                        i += len;
                    }
                    Escape::Osc { payload, len } => {
                        self.apply_osc(payload);
                        i += len;
                    }
                    Escape::Charset {
                        designator,
                        final_byte,
                        len,
                    } => {
                        self.apply_charset(designator, final_byte);
                        i += len;
                    }
                    Escape::Single { byte, len } => {
                        self.apply_esc(byte);
                        i += len;
                    }
                    Escape::Incomplete => break,
                }
                continue;
            }
            if b == b'\r' {
                self.carriage_return();
                i += 1;
                continue;
            }
            if b == b'\n' {
                self.line_feed();
                i += 1;
                continue;
            }
            // Every other C0 control (BS, BEL outside an OSC, SI/SO charset
            // shifts -- SI (0x0f) is the only one of these the current
            // fixture set actually contains, 3 times, always as a bare
            // control rather than after ESC) carries no visible text and no
            // cursor motion this grid tracks (SI/SO select a character set
            // register this grid does not model, matching `apply_charset`'s
            // own no-op stance on charset designation): drop it.
            if b < 0x20 || b == 0x7f {
                i += 1;
                continue;
            }
            match next_scalar(&buf[i..]) {
                ScalarScan::Complete(ch, len) => {
                    self.write_char(ch);
                    i += len;
                }
                ScalarScan::Incomplete => break,
                // Not valid UTF-8 and never will be: drop the byte rather
                // than stall the scan on it, matching `TuiScreen::push`.
                ScalarScan::Invalid => {
                    i += 1;
                }
            }
        }

        self.pending = buf.split_off(i);
    }

    /// The grid's current visible content as multi-line text, one row per
    /// line, trailing spaces trimmed per row and wholly-blank trailing rows
    /// dropped (a fixed 40-row grid mostly holding a few lines of dialog
    /// text has no meaning left in the other 30-odd blank rows below it).
    /// A blank row followed by real content further down is kept -- only
    /// the *trailing* run is dropped. Real inter-word spacing is present
    /// (this is a rendered screen, not `TuiScreen`'s whitespace-stripped
    /// accumulator), so [`TuiScreen::shows`]'s own whitespace-insensitive
    /// comparison still works unchanged when run over this output -- the
    /// matching rule stays slice 1's; this module only supplies a truer
    /// "what is on screen now" input to it.
    pub(crate) fn rendered(&self) -> String {
        let lines: Vec<String> = self
            .cells
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect();
        match lines.iter().rposition(|line| !line.is_empty()) {
            Some(last) => lines[..=last].join("\n"),
            None => String::new(),
        }
    }

    /// Convenience: `TuiScreen::shows(phrase)` over [`Self::rendered`].
    /// The predicate layer (a later slice) should call this, never
    /// `rendered()` plus its own matching -- see `screen.rs`'s own warning
    /// that `shows` is "the only supported way to ask".
    pub(crate) fn shows(&self, phrase: &str) -> bool {
        TuiScreen::from_bytes(self.rendered().as_bytes()).shows(phrase)
    }

    // ---------------------------------------------------- write/movement

    fn write_char(&mut self, ch: char) {
        if self.cursor_row < GRID_HEIGHT && self.cursor_col < GRID_WIDTH {
            self.cells[self.cursor_row][self.cursor_col] = ch;
        }
        self.cursor_col = (self.cursor_col + 1).min(GRID_WIDTH - 1);
        // No auto-wrap at the right margin: not exercised by any capture
        // (max column param observed is 119 of 120), so implementing wrap
        // now would be exactly the "speculative" this design refuses to
        // add. If a future capture needs it, add it with that capture as
        // the fixture proving it.
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    /// Line feed: advances the cursor, scrolling the active region up one
    /// line (content moves up, a blank line enters at the bottom) when
    /// already at `scroll_bottom` -- never past it. This is the only
    /// scroll direction any of the current fixture set actually needs from
    /// LF; the opposite direction is [`Self::reverse_index`].
    fn line_feed(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.cells[self.scroll_top..=self.scroll_bottom].rotate_left(1);
            if let Some(row) = self.cells.get_mut(self.scroll_bottom) {
                row.fill(' ');
            }
        } else {
            self.cursor_row = (self.cursor_row + 1).min(GRID_HEIGHT - 1);
        }
    }

    /// Reverse Index (`ESC M`): the mirror of [`Self::line_feed`] -- moves
    /// the cursor up, scrolling the region DOWN (a blank line enters at
    /// the TOP) when already at `scroll_top`. Only exercised by
    /// `codex-directory-trust.raw` (29 occurrences) in the current fixture
    /// set, but load-bearing: without scroll-region-aware handling here,
    /// that capture's later content renders at the wrong rows relative to
    /// the fixed 40-row grid.
    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            self.cells[self.scroll_top..=self.scroll_bottom].rotate_right(1);
            if let Some(row) = self.cells.get_mut(self.scroll_top) {
                row.fill(' ');
            }
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col));
    }

    fn restore_cursor(&mut self) {
        if let Some((row, col)) = self.saved_cursor {
            self.cursor_row = row;
            self.cursor_col = col;
        }
    }

    fn erase_whole_screen(&mut self) {
        for row in &mut self.cells {
            row.fill(' ');
        }
    }

    fn erase_from_cursor_to_end_of_screen(&mut self) {
        if let Some(row) = self.cells.get_mut(self.cursor_row) {
            row[self.cursor_col..].fill(' ');
        }
        for row in self.cells.iter_mut().skip(self.cursor_row + 1) {
            row.fill(' ');
        }
    }

    fn erase_whole_line(&mut self) {
        if let Some(row) = self.cells.get_mut(self.cursor_row) {
            row.fill(' ');
        }
    }

    fn erase_from_cursor_to_end_of_line(&mut self) {
        if let Some(row) = self.cells.get_mut(self.cursor_row) {
            row[self.cursor_col..].fill(' ');
        }
    }

    // ------------------------------------------------------------- CSI

    /// Dispatches one parsed CSI sequence (the bytes between `[` and the
    /// final byte, inclusive of any `?` private-mode prefix; the final
    /// byte itself) to its grid effect.
    ///
    /// **Escape inventory this must cover exactly** (from all seven
    /// committed captures; re-run 2026-09-09 against the current fixture
    /// set, which grew by one, `claude-trust-to-composer.raw` -- the
    /// capture proving the answered-gate limitation -- since this table
    /// was first drawn from six):
    ///
    /// | Final | Params seen | Effect |
    /// |---|---|---|
    /// | `A`/`B`/`C`/`D` | none or a count | cursor up/down/forward/back, clamped to grid bounds |
    /// | `G` | column (1-indexed, default 1) | cursor column absolute |
    /// | `H` | row;col (1-indexed, default 1;1) | cursor position absolute |
    /// | `J` | none/`0` or `2` | erase cursor-to-end-of-screen / whole screen -- **`1`/`3` never appear; a typed failure, not ignored (see below)** |
    /// | `K` | none/`0` or `2` | erase cursor-to-end-of-line / whole line -- **`1` never appears; same treatment as J** |
    /// | `r` | `top;bottom` or none | set/reset the scroll region |
    /// | `m` | any | SGR -- **no-op**, this grid stores no attributes |
    /// | `q` (incl. `CSI >0q`) | any | cursor style / a vendor-specific variant -- **no-op** |
    /// | `c` | none | Device Attributes query -- **no-op** |
    /// | `n` | `6` (DSR cursor-position query) | **no-op**: the vendor is asking a real terminal to report back over stdin, which this grid never does |
    /// | `u` (incl. `CSI ?u`) | any | Kitty keyboard-protocol query/push/pop -- **no-op** |
    /// | `? 1000/1002/1003/1006` + `h` | -- | mouse-tracking mode enables -- **no-op**, no visual effect; only `h` appears in the current fixture set |
    /// | `? 1049` + `h` | -- | enter alternate screen buffer -- **no-op, but contingent, not unconditional**: safe here only because `claude-trust-to-composer.raw`'s own next two sequences are `CSI 2J CSI H` (erase whole screen, home), verified directly against the fixture -- a vendor that entered the alternate screen WITHOUT clearing would show the prior dialog through it, and this grid, which never swaps buffers, would be wrong. `?1049l` (exit/restore) appears nowhere in the current fixture set, so that transition is not modeled |
    /// | `? 1004/2004/2026/2031/25` + `h`/`l` | -- | DEC private modes (focus reporting, bracketed paste, synchronized-update markers, cursor visibility) -- **no-op**, none affect grid content |
    ///
    /// **Every final byte and parameter combination above is drawn from
    /// the actual inventory, not guessed.** The rows marked **no-op** are
    /// content-neutral *families*: no parameter value on `m`/`q`/`c`/`n`/
    /// `u`/a DEC private-mode toggle can move the cursor, erase, or write a
    /// cell, so an unrecognized *parameter* on one of these final bytes is
    /// ignored exactly like the known ones -- a vendor adding a new SGR
    /// attribute or private-mode number must not stop the adapter. Every
    /// other case below -- an unrecognized CSI **final byte**, or an
    /// unrecognized **parameter** on `J`/`K` (which DO affect grid
    /// content) -- goes to [`Self::mark_unsupported`] rather than being
    /// silently ignored: an operation that might move the cursor or erase
    /// would leave the grid wrong while looking fine if ignored, which is
    /// the actual danger here, not merely meeting something new.
    fn apply_csi(&mut self, params: &[u8], final_byte: u8) {
        match final_byte {
            b'A' => self.cursor_row = self.cursor_row.saturating_sub(parse_count(params)),
            b'B' => self.cursor_row = (self.cursor_row + parse_count(params)).min(GRID_HEIGHT - 1),
            b'C' => self.cursor_col = (self.cursor_col + parse_count(params)).min(GRID_WIDTH - 1),
            b'D' => self.cursor_col = self.cursor_col.saturating_sub(parse_count(params)),
            b'G' => self.cursor_col = parse_count(params).saturating_sub(1).min(GRID_WIDTH - 1),
            b'H' => {
                let (row, col) = parse_row_col(params);
                self.cursor_row = row.saturating_sub(1).min(GRID_HEIGHT - 1);
                self.cursor_col = col.saturating_sub(1).min(GRID_WIDTH - 1);
            }
            b'J' => match params {
                b"" | b"0" => self.erase_from_cursor_to_end_of_screen(),
                b"2" => self.erase_whole_screen(),
                other => self.mark_unsupported(unhandled_csi_message(b'J', other)),
            },
            b'K' => match params {
                b"" | b"0" => self.erase_from_cursor_to_end_of_line(),
                b"2" => self.erase_whole_line(),
                other => self.mark_unsupported(unhandled_csi_message(b'K', other)),
            },
            b'r' => {
                if params.is_empty() {
                    self.scroll_top = 0;
                    self.scroll_bottom = GRID_HEIGHT - 1;
                } else {
                    let (top, bottom) = parse_row_col(params);
                    self.scroll_top = top.saturating_sub(1).min(GRID_HEIGHT - 1);
                    self.scroll_bottom = bottom.saturating_sub(1).min(GRID_HEIGHT - 1);
                }
            }
            // Content-neutral families: see the doc comment above.
            b'm' | b'q' | b'c' | b'n' | b'u' => {}
            b'h' | b'l' if params.starts_with(b"?") => {}
            other => self.mark_unsupported(unhandled_csi_message(other, params)),
        }
    }

    /// Records `description` as [`Self::unsupported`] (if nothing has been
    /// recorded yet -- the *first* unsupported sequence is what a caller
    /// needs, not the last), so a novel vendor escape becomes a fact the
    /// daemon can act on rather than a crash. A doc comment saying "the
    /// next slice must handle this before adding a caller" has nothing
    /// that fails when that promise is broken; this field does, because
    /// [`Self::unsupported`] is `pub(crate)` and the slice that adds a
    /// real caller cannot add one without reading it.
    ///
    /// `#[cfg(test)]` builds record exactly the same way, then ALSO
    /// panic, so every existing test that exercises an unsupported
    /// sequence still catches it exactly as before this method existed --
    /// see the `#[cfg(not(test))]` twin below, which is the identical
    /// recording with the panic removed, not a second implementation of
    /// the recording itself. Sharing that one line rather than writing it
    /// twice is what makes the production behavior (record, do not crash)
    /// something this crate's own test suite can actually exercise, even
    /// though `cfg!(test)` being true throughout `cargo test` means no
    /// test here ever reaches a build where the panic itself is absent.
    #[cfg(test)]
    fn mark_unsupported(&mut self, description: String) -> ! {
        if self.unsupported.is_none() {
            self.unsupported = Some(description.clone());
        }
        panic!("{description}")
    }

    #[cfg(not(test))]
    fn mark_unsupported(&mut self, description: String) {
        if self.unsupported.is_none() {
            self.unsupported = Some(description);
        }
    }

    // -------------------------------------------------------- ESC (C1)

    /// The single-extra-byte escapes the captures use: `ESC 7` (DECSC,
    /// save cursor), `ESC 8` (DECRC, restore cursor), `ESC M` (RI, reverse
    /// index -- see [`Self::reverse_index`]). Charset designation and OSC
    /// are classified and dispatched separately (see [`Self::apply_charset`]
    /// and [`Self::apply_osc`]).
    fn apply_esc(&mut self, byte_after_esc: u8) {
        match byte_after_esc {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'M' => self.reverse_index(),
            other => self.mark_unsupported(format!(
                "unrecognized single-byte escape ESC {:?} reached apply_esc; the current \
                 fixture set only contains ESC 7/8/M -- a fixture must exist before a fourth \
                 is implemented",
                other as char
            )),
        }
    }

    /// `ESC` followed by one of `( ) * +` then one more byte: G0-G3
    /// charset designation. No-op here (this grid tracks no character set
    /// state), and consumes all 3 bytes correctly -- `escape_len`'s own nF
    /// handling in `screen.rs` already proved this grammar right against
    /// the fixture that used to trip a fixed-length fallback
    /// (`claude-workspace-trust.raw`'s trailing `ESC ( B`); this method
    /// inherits that already-fixed recognition, it does not re-solve it.
    fn apply_charset(&mut self, _designator: u8, _final_byte: u8) {
        // Deliberately empty: see the doc comment above.
    }

    // -------------------------------------------------------------- OSC

    /// Consumes one OSC sequence's payload and applies nothing: OSC
    /// carries title-bar text, clipboard payloads, and hyperlink URIs,
    /// never cell content, so parse-and-skip is the correct semantics
    /// here, not an approximation of a missing feature. The terminator
    /// itself (BEL or `ESC \`) is found by [`classify_escape`], not here --
    /// this method exists so OSC is an explicit dispatch target, never a
    /// case that silently falls through unclassified.
    fn apply_osc(&mut self, _payload: &[u8]) {
        // Deliberately empty: see the doc comment above.
    }
}

/// One CSI/OSC/charset/single-byte escape, classified and parsed starting
/// at `bytes[0] == 0x1b`, or [`Escape::Incomplete`] if `bytes` ends before
/// the sequence's terminator does.
enum Escape<'a> {
    Csi {
        params: &'a [u8],
        final_byte: u8,
        len: usize,
    },
    Osc {
        payload: &'a [u8],
        len: usize,
    },
    Charset {
        designator: u8,
        final_byte: u8,
        len: usize,
    },
    Single {
        byte: u8,
        len: usize,
    },
    Incomplete,
}

/// Re-derives `screen.rs`'s `escape_len` grammar (CSI parameter bytes
/// `0x20..=0x3f` then a final byte `0x40..=0x7e`; OSC terminated by BEL or
/// `ESC \`; the nF family of one-or-more intermediate bytes `0x20..=0x2f`
/// then a final byte; a bare two-byte fallback), extracting the parsed
/// content each dispatch method needs instead of only a length. See
/// [`TerminalGrid::push`]'s doc comment for why this is not a literal call
/// to `escape_len` itself.
///
/// CSI's parameter bytes and any true intermediate bytes are not split
/// apart: no fixture in the current set uses an actual intermediate on a
/// CSI sequence (the closest, `CSI >0q`, sorts `>` as a parameter byte,
/// matching the inventory table's own "incl. `CSI >0q`" note), so keeping
/// them as one `params` slice is what the evidence supports, not a
/// simplification made for its own sake.
fn classify_escape(bytes: &[u8]) -> Escape<'_> {
    debug_assert_eq!(
        bytes.first(),
        Some(&0x1b),
        "classify_escape called on a non-ESC byte"
    );
    if bytes.len() < 2 {
        return Escape::Incomplete;
    }
    match bytes[1] {
        b'[' => {
            let mut i = 2;
            while i < bytes.len() && (0x20..=0x3f).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() && (0x40..=0x7e).contains(&bytes[i]) {
                Escape::Csi {
                    params: &bytes[2..i],
                    final_byte: bytes[i],
                    len: i + 1,
                }
            } else {
                Escape::Incomplete
            }
        }
        b']' => {
            let mut i = 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return Escape::Osc {
                        payload: &bytes[2..i],
                        len: i + 1,
                    };
                }
                if bytes[i] == 0x1b {
                    return if i + 1 < bytes.len() {
                        Escape::Osc {
                            payload: &bytes[2..i],
                            len: i + 2,
                        }
                    } else {
                        Escape::Incomplete
                    };
                }
                i += 1;
            }
            Escape::Incomplete
        }
        designator @ 0x20..=0x2f => {
            let mut i = 2;
            while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                Escape::Charset {
                    designator,
                    final_byte: bytes[i],
                    len: i + 1,
                }
            } else {
                Escape::Incomplete
            }
        }
        other => Escape::Single {
            byte: other,
            len: 2,
        },
    }
}

/// The message [`TerminalGrid::mark_unsupported`] records (or panics with,
/// under `#[cfg(test)]`) for an unrecognized CSI final byte, or a
/// recognized final byte with a parameter this grid does not know how to
/// apply safely -- see [`TerminalGrid::apply_csi`]'s doc comment for why
/// these are not silently ignored.
fn unhandled_csi_message(final_byte: u8, params: &[u8]) -> String {
    format!(
        "unhandled CSI final byte {:?} with params {:?} reached apply_csi; the current fixture \
         set does not exercise this, so treating it as a no-op would be a guess this design \
         refuses to make -- add the fixture that needs it before adding the handling",
        final_byte as char,
        String::from_utf8_lossy(params)
    )
}

fn parse_count(params: &[u8]) -> usize {
    if params.is_empty() {
        return 1;
    }
    std::str::from_utf8(params)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n != 0)
        .unwrap_or(1)
}

fn parse_row_col(params: &[u8]) -> (usize, usize) {
    let text = std::str::from_utf8(params).unwrap_or_default();
    let mut parts = text.split(';');
    let one = |s: Option<&str>| {
        s.and_then(|s| s.parse::<usize>().ok())
            .filter(|&n| n != 0)
            .unwrap_or(1)
    };
    (one(parts.next()), one(parts.next()))
}

enum ScalarScan {
    Complete(char, usize),
    Incomplete,
    Invalid,
}

/// The scalar starting at `bytes[0]`, or whether it is merely truncated.
/// Mirrors `screen.rs`'s own `next_scalar` exactly (that one is private to
/// `screen.rs`, so this is a second copy of the same content-decoding
/// logic, not a shared call -- see [`classify_escape`]'s doc comment for
/// the analogous choice on the escape side).
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

    const ALL_FIXTURES: &[&str] = &[
        "claude-signin-method.raw",
        "claude-theme-picker.raw",
        "claude-trust-to-composer.raw",
        "claude-workspace-trust.raw",
        "codex-composer-then-trust.raw",
        "codex-directory-trust.raw",
        "codex-signin.raw",
    ];

    // -------------------------------------------------- the point of this module

    /// The mirror of `screen.rs`'s
    /// `an_answered_gate_is_gone_from_the_terminal_but_not_from_the_accumulator`,
    /// over the same fixture. That test asserts three things, in order:
    /// the raw bytes contain the alternate-screen switch, the screen shows
    /// the composer's own banner, and the screen still shows the answered
    /// gate's phrase -- a real screen model must fail that third assertion
    /// (a real grid no longer shows the answered gate's phrase once the
    /// terminal has cleared it) and pass the other two (the switch is
    /// still in the raw bytes; the composer still paints). That test is
    /// not edited -- this is its pair, documenting the two mechanisms'
    /// difference side by side rather than replacing either one.
    #[test]
    fn a_grid_stops_showing_an_answered_gate_once_the_terminal_clears_it() {
        let bytes = fixture("claude-trust-to-composer.raw");
        let grid = TerminalGrid::from_bytes(&bytes);

        assert!(
            bytes.windows(8).any(|w| w == b"\x1b[?1049h"),
            "the capture must contain the alternate-screen switch"
        );
        assert!(
            grid.shows("Claude Code"),
            "the composer must have painted after the gate was answered"
        );

        // The difference from the accumulator: the grid answers "on screen
        // now", and the erase that immediately follows the alt-screen
        // switch (see `apply_csi`'s `?1049h` row) really did clear the
        // gate's text before the composer painted over it.
        assert!(
            !grid.shows("Yes, I trust this folder"),
            "a grid must NOT still show an answered gate's phrase -- that is exactly the \
             accumulator limitation this module exists to remove"
        );
    }

    // ---------------------------------------------------------- cursor movement

    #[test]
    fn cursor_position_is_one_indexed_and_clamped_to_the_grid() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[5;10Hx");
        assert_eq!(
            grid.rendered().lines().nth(4).unwrap().chars().nth(9),
            Some('x')
        );

        // Past the grid: clamp, do not panic or wrap.
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[999;999Hx");
        assert_eq!(
            grid.rendered()
                .lines()
                .nth(GRID_HEIGHT - 1)
                .unwrap()
                .chars()
                .nth(GRID_WIDTH - 1),
            Some('x')
        );
    }

    #[test]
    fn cursor_column_absolute_defaults_to_one_and_is_one_indexed() {
        let mut grid = TerminalGrid::new();
        grid.push(b"ab\x1b[GY"); // CSI G with no param -> column 1
        assert_eq!(
            grid.rendered().lines().next().unwrap().chars().next(),
            Some('Y')
        );
    }

    #[test]
    fn relative_cursor_moves_are_clamped_not_wrapped() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[5Dx"); // 5 back from column 0
        assert_eq!(
            grid.rendered().lines().next().unwrap().chars().next(),
            Some('x')
        );
    }

    // ---------------------------------------------------------------- erase

    #[test]
    fn erase_whole_screen_blanks_every_cell() {
        let mut grid = TerminalGrid::new();
        grid.push(b"hello\x1b[2J");
        assert_eq!(grid.rendered(), "");
    }

    #[test]
    fn erase_to_end_of_screen_from_the_origin_blanks_everything() {
        let mut grid = TerminalGrid::new();
        grid.push(b"one\r\ntwo\x1b[H\x1b[0J");
        assert_eq!(
            grid.rendered(),
            "",
            "cursor was at (0,0): the whole grid is erased too"
        );
    }

    #[test]
    fn erase_to_end_of_screen_leaves_content_before_the_cursor_alone() {
        let mut grid = TerminalGrid::new();
        grid.push(b"keep\r\nabcdef\x1b[2;3H\x1b[0J");
        assert_eq!(
            grid.rendered(),
            "keep\nab",
            "the row before the cursor's row, and the columns before the cursor on its own row, must survive"
        );
    }

    #[test]
    fn erase_to_end_of_line_leaves_earlier_columns_alone() {
        let mut grid = TerminalGrid::new();
        grid.push(b"abcdef\x1b[3D\x1b[K");
        assert_eq!(grid.rendered().lines().next().unwrap(), "abc");
    }

    // ------------------------------------------------------------ scrolling

    #[test]
    fn line_feed_at_the_scroll_bottom_scrolls_up() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[1;2r"); // a 2-row scroll region (rows 1-2, i.e. index 0-1)
        // CR+LF each line, not an absolute reposition: the scroll only
        // triggers when LF is issued from the scroll region's own bottom
        // row, which real cursor advancement (not re-homing) reaches.
        grid.push(b"\x1b[1;1Hone\r\ntwo\r\nthree");
        let rendered = grid.rendered();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0], "two",
            "'one' must have scrolled off the top of the 2-row region"
        );
        assert_eq!(lines[1], "three");
    }

    #[test]
    fn reverse_index_at_the_scroll_top_scrolls_down() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[1;2r\x1b[1;1Hbottom\x1b[2;1Htop");
        grid.push(b"\x1b[1;1H\x1bMnew"); // RI at scroll_top pushes "bottom" down, "top" is lost off the bottom
        let rendered = grid.rendered();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "new");
        assert_eq!(
            lines[1], "bottom",
            "the old top row's content must have moved down one line"
        );
    }

    // ---------------------------------------------------- content-neutral no-ops

    #[test]
    fn sgr_and_device_query_sequences_do_not_move_the_cursor_or_write_a_cell() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[1;31mx\x1b[0m\x1b[6n\x1b[c\x1b[>0q\x1b[?u");
        assert_eq!(grid.rendered(), "x");
    }

    #[test]
    fn dec_private_mode_toggles_including_mouse_tracking_and_alt_screen_are_no_ops() {
        // The exact cluster from claude-trust-to-composer.raw's alt-screen
        // switch: ?1049h ?1000h ?1002h ?1003h ?1006h, none of them a panic,
        // none of them moving the cursor.
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?25hx");
        assert_eq!(grid.rendered(), "x");
    }

    // --------------------------------------------------------- OSC / charset

    #[test]
    fn osc_sequences_are_skipped_with_either_terminator_and_no_effect() {
        let mut grid = TerminalGrid::new();
        grid.push(b"a\x1b]0;a window title\x07b");
        assert_eq!(grid.rendered(), "ab");
        let mut grid = TerminalGrid::new();
        grid.push(b"a\x1b]0;a window title\x1b\\b");
        assert_eq!(grid.rendered(), "ab");
    }

    #[test]
    fn an_osc_terminator_split_across_reads_is_held_incomplete() {
        let mut grid = TerminalGrid::new();
        grid.push(b"a\x1b]0;title");
        grid.push(b"\x07b");
        assert_eq!(grid.rendered(), "ab");
    }

    #[test]
    fn charset_designation_consumes_all_three_bytes() {
        let mut grid = TerminalGrid::new();
        grid.push(b"before\x1b(Bafter");
        assert_eq!(grid.rendered(), "beforeafter");
    }

    #[test]
    fn a_charset_designation_split_across_reads_is_held_incomplete() {
        let mut grid = TerminalGrid::new();
        grid.push(b"before\x1b(");
        grid.push(b"Bafter");
        assert_eq!(grid.rendered(), "beforeafter");
    }

    #[test]
    fn a_bare_c0_shift_in_control_is_dropped_without_effect() {
        // SI (0x0f): present 3 times in the current fixture set, always
        // bare. This grid tracks no charset-selection register, so it is
        // ignored the same way a charset designation is.
        let mut grid = TerminalGrid::new();
        grid.push(b"a\x0fb");
        assert_eq!(grid.rendered(), "ab");
    }

    // ------------------------------------------------------- incremental reads

    #[test]
    fn chunked_pushes_agree_with_one_whole_push_at_every_split() {
        for name in ALL_FIXTURES {
            let bytes = fixture(name);
            let whole = TerminalGrid::from_bytes(&bytes);
            for chunk in [1usize, 2, 3, 5, 7, 64, 512] {
                let mut grid = TerminalGrid::new();
                for slice in bytes.chunks(chunk) {
                    grid.push(slice);
                }
                assert_eq!(
                    grid.rendered(),
                    whole.rendered(),
                    "{name}: a {chunk}-byte read split changed the rendered grid"
                );
            }
        }
    }

    /// Every committed capture must fold into the grid without panicking --
    /// the exhaustive dispatch table in `apply_csi`'s doc comment is
    /// exactly the escape inventory measured against these seven files, so
    /// nothing in them should reach an `unhandled_csi`/`apply_esc` panic.
    #[test]
    fn every_committed_capture_folds_in_without_panicking() {
        for name in ALL_FIXTURES {
            let bytes = fixture(name);
            let grid = TerminalGrid::from_bytes(&bytes);
            assert!(
                !grid.rendered().is_empty(),
                "{name}: rendered grid must not be empty"
            );
        }
    }

    // -------------------------------------------------------- unit rules

    #[test]
    fn a_multibyte_scalar_split_across_reads_is_not_corrupted() {
        let mut grid = TerminalGrid::new();
        let bytes = "trust\u{2014}folder".as_bytes();
        grid.push(&bytes[..6]); // splits the em dash
        grid.push(&bytes[6..]);
        assert_eq!(grid.rendered(), "trust\u{2014}folder");
    }

    #[test]
    fn invalid_utf8_is_dropped_without_stalling_the_scan() {
        let mut grid = TerminalGrid::new();
        grid.push(b"be\xfffore\xfe after");
        // Unlike `TuiScreen`, this grid keeps real spacing -- only the two
        // invalid bytes are dropped, the space between "fore" and "after" is
        // real content and stays.
        assert_eq!(grid.rendered(), "before after");
    }

    // ------------------------------------------------- unhandled sequence policy

    #[test]
    #[should_panic(expected = "unhandled CSI final byte")]
    fn an_unrecognized_csi_final_byte_panics_rather_than_being_ignored() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[5Z"); // CBT, not in the current fixture set
    }

    #[test]
    #[should_panic(expected = "unhandled CSI final byte")]
    fn an_unrecognized_erase_parameter_on_j_panics_rather_than_being_ignored() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[1J"); // erase-to-cursor: never appears, must not be guessed at
    }

    #[test]
    #[should_panic(expected = "unhandled CSI final byte")]
    fn an_unrecognized_erase_parameter_on_k_panics_rather_than_being_ignored() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1b[1K");
    }

    #[test]
    #[should_panic(expected = "unrecognized single-byte escape")]
    fn an_unrecognized_single_byte_escape_panics_rather_than_being_ignored() {
        let mut grid = TerminalGrid::new();
        grid.push(b"\x1bZ"); // not 7/8/M
    }

    /// `mark_unsupported`'s `#[cfg(test)]` build records, THEN panics,
    /// sharing the identical recording line the `#[cfg(not(test))]`
    /// production build uses (see its own doc comment) -- so catching the
    /// panic here and inspecting the grid afterward exercises the exact
    /// same recording behavior a production build would run, not an
    /// approximation of it. What this test cannot prove is that a
    /// production build actually skips the panic (nothing in this suite
    /// compiles as one), only that the fact it would have recorded is the
    /// right one.
    #[test]
    fn an_unsupported_sequence_is_recorded_before_the_test_build_panics() {
        let mut grid = TerminalGrid::new();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            grid.push(b"\x1b[5Z"); // CBT, not in the current fixture set
        }));
        assert!(
            outcome.is_err(),
            "test builds must still panic on a novel CSI final byte"
        );
        let message = grid
            .unsupported()
            .expect("mark_unsupported must record before it panics, not after");
        assert!(
            message.contains("unhandled CSI final byte"),
            "unexpected message: {message:?}"
        );
        assert!(
            message.contains("'Z'"),
            "the final byte itself must be named: {message:?}"
        );
    }

    /// Cross-checks `classify_escape` against `screen.rs`'s own
    /// `escape_len` on every committed capture: both scan the identical
    /// escape grammar, but as two separate implementations (`escape_len`
    /// only ever returns a length; this module needs parsed content too,
    /// so it cannot just call `escape_len` -- see `classify_escape`'s own
    /// doc comment). `escape_len` already had one real bug in this exact
    /// grammar (the nF family's `ESC ( B` truncated to two bytes) found
    /// and fixed during slice 1; had this module existed then, it would
    /// have needed the identical fix independently, and nothing would
    /// have said so. This test is that "something": a length disagreement
    /// between the two fails here, at the point a future edit introduces
    /// it, rather than surfacing months later as a grid that silently
    /// renders wrong.
    #[test]
    fn classify_escape_agrees_with_screen_rs_escape_len_on_every_committed_capture() {
        use super::super::screen::{EscapeScan, escape_len};

        for name in ALL_FIXTURES {
            let bytes = fixture(name);
            let mut i = 0usize;
            while i < bytes.len() {
                if bytes[i] != 0x1b {
                    i += 1;
                    continue;
                }
                let ours = classify_escape(&bytes[i..]);
                let theirs = escape_len(&bytes[i..]);
                let ours_len = match &ours {
                    Escape::Csi { len, .. } => Some(*len),
                    Escape::Osc { len, .. } => Some(*len),
                    Escape::Charset { len, .. } => Some(*len),
                    Escape::Single { len, .. } => Some(*len),
                    Escape::Incomplete => None,
                };
                match (ours_len, theirs) {
                    (Some(ours_len), EscapeScan::Complete(their_len)) => {
                        assert_eq!(
                            ours_len, their_len,
                            "{name} at byte {i}: classify_escape and escape_len disagree on this escape's length"
                        );
                        i += ours_len;
                    }
                    (None, EscapeScan::Incomplete) => break,
                    _ => panic!(
                        "{name} at byte {i}: classify_escape and escape_len disagree on whether this escape is complete"
                    ),
                }
            }
        }
    }
}
