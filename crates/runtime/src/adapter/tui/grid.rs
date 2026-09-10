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
//! `wait_for_readiness` (`adapter.rs`) is this module's real production
//! caller, via `TuiVendor::classify_surface`: it builds one of these per
//! run, fed from the PTY's own output as it arrives, and polls
//! `classify_surface` on it instead of treating any output as ready.

use super::screen::TuiScreen;

/// The size every committed fixture under `fixtures/adapters/tui-screens/`
/// was captured at (see that directory's own README for the per-fixture
/// size column). Fixture-replay tests build a grid at this size, matching
/// the capture; production instead builds one at the PTY's own real size
/// (`crate::supervisor::pty::{DEFAULT_COLS, DEFAULT_ROWS}`, currently
/// 120x32) -- the two can legitimately differ, which is exactly why size
/// is a constructor parameter now rather than a fixed constant. This
/// constant used to claim the fixed 120x40 size matched "the size crew's
/// own adapter requests"; it did not -- 40 rows came from the probe
/// harness that took the captures, never from anything production spawns
/// a PTY at. That mismatch (real PTY rows scrolling content off a grid
/// modeling more rows than exist) is a live working hypothesis for how a
/// real first-run gate went unrecognized: a screen that scrolled past row
/// 32 on the real terminal could still sit within this grid's own
/// (larger, wrong) row range, rendering at a position the real terminal
/// never actually held it at. `#[cfg(test)]`: nothing in production
/// builds a grid at this size.
#[cfg(test)]
pub(crate) const FIXTURE_GRID_WIDTH: usize = 120;
#[cfg(test)]
pub(crate) const FIXTURE_GRID_HEIGHT: usize = 40;

/// A rectangular grid of cells, with a cursor and a scroll region, folded
/// from raw PTY bytes.
///
/// `pub`, not `pub(crate)`: `TuiVendor::classify_surface`, this type's
/// real caller, is itself `pub`, and a `pub` trait method cannot
/// reference a less-visible type in its signature.
/// The `tui` module's own privacy still bounds this to the crate in
/// practice -- nothing outside it can reach `TuiVendor` at all -- this
/// widening only satisfies that per-item consistency check, not an
/// intent to expose the grid beyond this crate.
#[derive(Debug, Clone)]
pub struct TerminalGrid {
    width: usize,
    height: usize,
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
    /// The current grid's trustworthiness: `Some(class)` when the most
    /// recent escape sequence this grid could not apply safely has not
    /// been superseded by a full repaint since -- see
    /// [`Self::mark_unsupported`] and [`Self::erase_whole_screen`] (which
    /// clears this) for the two halves of that lifecycle. `class` is a
    /// closed-set name (e.g. `"CSI 'b' (REP)"`), never the raw sequence:
    /// this field's value can reach a `RuntimeEvent` string (the
    /// Enter-precondition failure detail), which crosses the redaction
    /// boundary -- see [`Self::mark_unsupported`]'s own doc comment.
    unsupported: Option<String>,
    /// How many times this grid has ever recorded an unsupported
    /// sequence, across the grid's whole lifetime -- never cleared,
    /// unlike [`Self::unsupported`] itself. A grid that clears and
    /// re-latches repeatedly is a chronically-misparsing vendor hiding
    /// behind a well-behaved repaint loop; nothing else here would make
    /// that visible once the flag itself can recover.
    unsupported_count: usize,
}

impl TerminalGrid {
    /// Builds an empty grid of `width` columns by `height` rows.
    /// Production builds one at the PTY's own real size
    /// (`crate::supervisor::pty::{DEFAULT_COLS, DEFAULT_ROWS}`); a
    /// fixture-replay test builds one at [`FIXTURE_GRID_WIDTH`]/
    /// [`FIXTURE_GRID_HEIGHT`], the size every committed capture was
    /// actually recorded at. The two are not required to agree, and
    /// historically have not (see [`FIXTURE_GRID_WIDTH`]'s own doc
    /// comment) -- that mismatch is precisely why this takes a size
    /// rather than assuming one.
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![vec![' '; width]; height],
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: height - 1,
            pending: Vec::new(),
            unsupported: None,
            unsupported_count: 0,
        }
    }

    /// A grid at [`FIXTURE_GRID_WIDTH`]x[`FIXTURE_GRID_HEIGHT`] -- every
    /// committed fixture's own recorded size. The convenience fixture-test
    /// callers reach for instead of naming that size at every call site.
    #[cfg(test)]
    pub(crate) fn new_at_fixture_size() -> Self {
        Self::new(FIXTURE_GRID_WIDTH, FIXTURE_GRID_HEIGHT)
    }

    /// The current grid's trustworthiness: `Some(class)` (a closed-set
    /// name, never the raw sequence -- see [`Self::unsupported`]'s own
    /// doc comment) when an unsupported escape sequence has been recorded
    /// and no full repaint has cleared it since. `pub`, not `pub(crate)`:
    /// a production-build integration test (`crates/runtime/tests/`,
    /// which compiles this crate WITHOUT the `test` cfg, so it is the
    /// only place that ever exercises `mark_unsupported`'s
    /// `#[cfg(not(test))]` body) needs to read this to prove the flag
    /// records and clears rather than panicking -- see
    /// [`Self::mark_unsupported`]'s own doc comment for why no test
    /// inside this crate can do that.
    pub fn unsupported(&self) -> Option<&str> {
        self.unsupported.as_deref()
    }

    /// How many times this grid has ever recorded an unsupported
    /// sequence -- see [`Self::unsupported_count`]'s field doc comment.
    /// `pub` for the same reason as [`Self::unsupported`].
    pub fn unsupported_count(&self) -> usize {
        self.unsupported_count
    }

    /// A grid built from one complete slice of output.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        let mut grid = Self::new_at_fixture_size();
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
    pub fn push(&mut self, bytes: &[u8]) {
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
    /// dropped (a grid this size mostly holding a few lines of dialog text
    /// has no meaning left in the many blank rows below it).
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
        if self.cursor_row < self.height && self.cursor_col < self.width {
            self.cells[self.cursor_row][self.cursor_col] = ch;
        }
        self.cursor_col = (self.cursor_col + 1).min(self.width - 1);
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
            self.cursor_row = (self.cursor_row + 1).min(self.height - 1);
        }
    }

    /// Reverse Index (`ESC M`): the mirror of [`Self::line_feed`] -- moves
    /// the cursor up, scrolling the region DOWN (a blank line enters at
    /// the TOP) when already at `scroll_top`. Only exercised by
    /// `codex-directory-trust.raw` (29 occurrences) in the current fixture
    /// set, but load-bearing: without scroll-region-aware handling here,
    /// that capture's later content renders at the wrong rows relative to
    /// the grid.
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

    /// Blanks every cell -- a vendor sending `CSI 2 J` is declaring the
    /// prior screen void, which is also why this is where
    /// [`Self::unsupported`] clears: every cell this grid's own content-
    /// matching predicates (`shows`/`rendered`) can read is, from this
    /// point, freshly and uniformly overwritten, so whatever an earlier
    /// unsupported sequence may have distorted no longer survives in cell
    /// content for those predicates to trust or distrust. A per-write
    /// clear would be wrong for the reason a per-write clear of anything
    /// here is wrong: a write following the corrupting read can itself
    /// land at a cursor position that read left distorted, so only an
    /// operation that overwrites the WHOLE grid unconditionally, not one
    /// cell at a time, actually re-establishes trust.
    fn erase_whole_screen(&mut self) {
        for row in &mut self.cells {
            row.fill(' ');
        }
        self.unsupported = None;
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

    /// `CSI 1 K` -- erase from the start of the line through the cursor
    /// **inclusive**, which is why the slice is `..=cursor_col` and not
    /// `..cursor_col`. The off-by-one matters: the cursor cell is the one
    /// a vendor is most likely to be rewriting when it emits this, so
    /// excluding it leaves exactly the character the sequence was sent to
    /// clear.
    fn erase_from_start_of_line_to_cursor(&mut self) {
        if let Some(row) = self.cells.get_mut(self.cursor_row) {
            let last = self.cursor_col.min(row.len().saturating_sub(1));
            row[..=last].fill(' ');
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
            b'B' => self.cursor_row = (self.cursor_row + parse_count(params)).min(self.height - 1),
            b'C' => self.cursor_col = (self.cursor_col + parse_count(params)).min(self.width - 1),
            b'D' => self.cursor_col = self.cursor_col.saturating_sub(parse_count(params)),
            b'G' => self.cursor_col = parse_count(params).saturating_sub(1).min(self.width - 1),
            b'H' => {
                let (row, col) = parse_row_col(params);
                self.cursor_row = row.saturating_sub(1).min(self.height - 1);
                self.cursor_col = col.saturating_sub(1).min(self.width - 1);
            }
            b'J' => match params {
                b"" | b"0" => self.erase_from_cursor_to_end_of_screen(),
                b"2" => self.erase_whole_screen(),
                // `CSI 3 J` erases the terminal's *saved lines* -- the
                // scrollback -- and leaves the visible screen alone. This
                // grid is a fixed-height window with no scrollback to
                // erase, so there is nothing here for it to do. That is a
                // property of the model, not an assumption about the
                // sequence: were a scrollback ever added, this arm would
                // have to clear it. Reached first by the omp composer
                // capture, which emits it alongside `2 J` on entry.
                b"3" => {}
                other => self.mark_unsupported("CSI 'J' (unrecognised parameter)", || {
                    unhandled_csi_message(b'J', other)
                }),
            },
            b'K' => match params {
                b"" | b"0" => self.erase_from_cursor_to_end_of_line(),
                // Reached first by the copilot capture. Unlike the query
                // sequences above this one genuinely clears cells, so it
                // is implemented rather than ignored -- treating an erase
                // as a no-op would leave text on the model that the real
                // terminal has removed, which is the failure this grid
                // exists to avoid.
                b"1" => self.erase_from_start_of_line_to_cursor(),
                b"2" => self.erase_whole_line(),
                other => self.mark_unsupported("CSI 'K' (unrecognised parameter)", || {
                    unhandled_csi_message(b'K', other)
                }),
            },
            b'r' => {
                if params.is_empty() {
                    self.scroll_top = 0;
                    self.scroll_bottom = self.height - 1;
                } else {
                    let (top, bottom) = parse_row_col(params);
                    self.scroll_top = top.saturating_sub(1).min(self.height - 1);
                    self.scroll_bottom = bottom.saturating_sub(1).min(self.height - 1);
                }
            }
            // Content-neutral families: see the doc comment above.
            // Presentation, queries and window operations: none of them
            // change a cell, so a model of cell content ignores them.
            //
            // `t` and `p` joined this list with the copilot capture, which
            // is the first committed fixture to emit either. `CSI 22;0 t`
            // pushes the window title onto the terminal's own title stack;
            // `CSI ? Ps $ p` is DECRQM, asking the terminal to report
            // whether a mode is set (copilot asks about 12, cursor blink,
            // and 2026, synchronized output). A query is answered by the
            // terminal, not by the screen -- and the replies are what the
            // viewer-socket filter in `display::terminal_reply` exists to
            // discard, so the two halves of that exchange are both
            // accounted for and neither reaches a grid cell.
            b'm' | b'q' | b'c' | b'n' | b'u' | b't' | b'p' => {}
            b'h' | b'l' if params.starts_with(b"?") => {}
            other => self.mark_unsupported(csi_unsupported_class(other), || {
                unhandled_csi_message(other, params)
            }),
        }
    }

    /// Records `class` as [`Self::unsupported`] (if nothing has been
    /// recorded yet -- the *first* unsupported sequence is what a caller
    /// needs, not the last) and always increments
    /// [`Self::unsupported_count`], so a novel vendor escape becomes a
    /// fact the daemon can act on rather than a crash. A doc comment
    /// saying "the next slice must handle this before adding a caller"
    /// has nothing that fails when that promise is broken; this field
    /// does, because [`Self::unsupported`] is `pub` and the caller that
    /// surfaces it (the Enter-precondition failure detail) cannot add one
    /// without reading it.
    ///
    /// `class` must be a closed-set, redaction-safe name -- one of the
    /// small number of fixed strings a caller builds it from (see
    /// [`csi_unsupported_class`]), never a value built from the escape
    /// sequence's own parameters. This is what ends up in
    /// [`Self::unsupported`], which can reach a `RuntimeEvent` string
    /// (the Enter-precondition failure detail naming the unsupported
    /// class); a description built from raw vendor bytes would cross the
    /// redaction boundary carrying whatever text the vendor happened to
    /// echo back through an unimplemented sequence's parameters.
    ///
    /// `diagnostic` is the fuller, raw-bytes-included description used
    /// ONLY by the `#[cfg(test)]` panic below -- never journaled, so the
    /// redaction constraint does not apply to it -- and is lazy
    /// (`impl FnOnce`) so building it costs nothing in the production
    /// build that never calls it.
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
    /// test here ever reaches a build where the panic itself is absent --
    /// see [`Self::unsupported`]'s own doc comment for how a
    /// production-build integration test proves the non-panicking body
    /// anyway.
    #[cfg(test)]
    fn mark_unsupported(
        &mut self,
        class: impl Into<String>,
        diagnostic: impl FnOnce() -> String,
    ) -> ! {
        if self.unsupported.is_none() {
            self.unsupported = Some(class.into());
        }
        self.unsupported_count += 1;
        panic!("{}", diagnostic())
    }

    #[cfg(not(test))]
    fn mark_unsupported(&mut self, class: impl Into<String>, _diagnostic: impl FnOnce() -> String) {
        if self.unsupported.is_none() {
            self.unsupported = Some(class.into());
        }
        self.unsupported_count += 1;
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
            other => self.mark_unsupported("unrecognised single-byte escape", || {
                format!(
                    "unrecognized single-byte escape ESC {:?} reached apply_esc; the current \
                     fixture set only contains ESC 7/8/M -- a fixture must exist before a \
                     fourth is implemented",
                    other as char
                )
            }),
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

/// A closed-set, redaction-safe class name for an unrecognized CSI final
/// byte -- what [`TerminalGrid::mark_unsupported`] actually stores (see
/// its own doc comment for why). Every arm is a fixed string literal:
/// even the fallback for a final byte outside this small, curated table
/// names no byte at all, deliberately -- a single interpolated byte is
/// this table's only concession, restricted to the identified,
/// maintainer-reviewed mnemonics below, never a hole a caller could widen
/// by adding a match arm that formats the byte in. Candidates come from
/// staff's own review of `apply_csi`'s implemented set against a plausible
/// vendor splash screen (box-drawing/ASCII art uses REP; window-resize and
/// scroll-related redraws plausibly use IL/DL/ICH/DCH/ECH/SU/SD); this
/// table is not exhaustive of the CSI grammar, only of what a real capture
/// has been observed or is plausible to need -- widening it on a guess
/// rather than a fixture is exactly the discipline this module's own
/// module doc warns against.
fn csi_unsupported_class(final_byte: u8) -> &'static str {
    match final_byte {
        b'b' => "CSI 'b' (REP)",
        b'L' => "CSI 'L' (IL)",
        b'M' => "CSI 'M' (DL)",
        b'P' => "CSI 'P' (DCH)",
        b'@' => "CSI '@' (ICH)",
        b'X' => "CSI 'X' (ECH)",
        b'S' => "CSI 'S' (SU)",
        b'T' => "CSI 'T' (SD)",
        b'd' => "CSI 'd' (VPA)",
        b'E' => "CSI 'E' (CNL)",
        b'F' => "CSI 'F' (CPL)",
        b'Z' => "CSI 'Z' (CBT)",
        b's' => "CSI 's' (SCP)",
        _ => "unrecognised CSI final byte",
    }
}

/// The message [`TerminalGrid::mark_unsupported`] records (or panics with,
/// under `#[cfg(test)]`) for an unrecognized CSI final byte, or a
/// recognized final byte with a parameter this grid does not know how to
/// apply safely -- see [`TerminalGrid::apply_csi`]'s doc comment for why
/// these are not silently ignored. Only ever reaches the `#[cfg(test)]`
/// panic (see [`TerminalGrid::mark_unsupported`]'s own doc comment for
/// why the raw parameter bytes it embeds must never reach further than
/// that).
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

/// The seven committed captures under `fixtures/adapters/tui-screens/`,
/// the single definition both this file's own tests and `classify.rs`'s
/// tests use. A hand-maintained list is a check that certifies only what
/// it happens to list: a fixture added to the directory tomorrow and
/// never added here would be scanned by nothing that uses this constant,
/// silently, forever. `tests::all_fixtures_matches_the_committed_directory`
/// is what makes that impossible instead of merely unlikely -- keeping
/// this to one definition, rather than one per user, means that one check
/// covers every user instead of certifying only whichever copy it reads.
#[cfg(test)]
pub(super) const ALL_FIXTURES: &[&str] = &[
    "claude-signin-method.raw",
    "claude-theme-picker.raw",
    "claude-trust-to-composer.raw",
    "claude-workspace-trust.raw",
    "codex-composer-empty.raw",
    "codex-composer-holding.raw",
    "codex-composer-then-trust.raw",
    "codex-directory-trust.raw",
    "codex-signin.raw",
    "copilot-composer.raw",
    "copilot-folder-trust.raw",
    "omp-composer.raw",
    "omp-setup-step1.raw",
];

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

    /// The floor under every other test in this file and in
    /// `classify.rs`'s own `ever_shows`-based checks: `ALL_FIXTURES` must
    /// name exactly the `.raw` files actually committed under
    /// `fixtures/adapters/tui-screens/`, in both directions. A file added
    /// to the directory and not added here would be compared by nothing
    /// (the escape-agreement floors, the exhaustive classify table, the
    /// exclusivity table -- none of them would ever see it, and all of
    /// them would keep passing, green, having looked at one fixture set
    /// short). A name removed or renamed on disk without updating this
    /// list would make every other test here fail with a "file not
    /// found" that could be mistaken for something else; this test names
    /// the actual mismatch instead.
    #[test]
    fn all_fixtures_matches_the_committed_directory() {
        use std::collections::BTreeSet;
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/adapters/tui-screens");
        let on_disk: BTreeSet<String> = std::fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
            .map(|entry| {
                entry.unwrap_or_else(|err| panic!("reading an entry of {}: {err}", dir.display()))
            })
            .filter_map(|entry| {
                let name = entry
                    .file_name()
                    .into_string()
                    .unwrap_or_else(|raw| panic!("non-UTF-8 file name: {raw:?}"));
                name.ends_with(".raw").then_some(name)
            })
            .collect();
        let listed: BTreeSet<String> = ALL_FIXTURES.iter().map(|s| s.to_string()).collect();

        let missing_from_list: Vec<&String> = on_disk.difference(&listed).collect();
        let missing_from_disk: Vec<&String> = listed.difference(&on_disk).collect();
        assert!(
            missing_from_list.is_empty() && missing_from_disk.is_empty(),
            "ALL_FIXTURES has drifted from {}: on disk but not listed: {missing_from_list:?}; \
             listed but not on disk (renamed or removed?): {missing_from_disk:?}",
            dir.display()
        );
    }

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
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"\x1b[5;10Hx");
        assert_eq!(
            grid.rendered().lines().nth(4).unwrap().chars().nth(9),
            Some('x')
        );

        // Past the grid: clamp, do not panic or wrap.
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"\x1b[999;999Hx");
        assert_eq!(
            grid.rendered()
                .lines()
                .nth(FIXTURE_GRID_HEIGHT - 1)
                .unwrap()
                .chars()
                .nth(FIXTURE_GRID_WIDTH - 1),
            Some('x')
        );
    }

    #[test]
    fn cursor_column_absolute_defaults_to_one_and_is_one_indexed() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"ab\x1b[GY"); // CSI G with no param -> column 1
        assert_eq!(
            grid.rendered().lines().next().unwrap().chars().next(),
            Some('Y')
        );
    }

    #[test]
    fn relative_cursor_moves_are_clamped_not_wrapped() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"\x1b[5Dx"); // 5 back from column 0
        assert_eq!(
            grid.rendered().lines().next().unwrap().chars().next(),
            Some('x')
        );
    }

    // ---------------------------------------------------------------- erase

    #[test]
    fn erase_whole_screen_blanks_every_cell() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"hello\x1b[2J");
        assert_eq!(grid.rendered(), "");
    }

    #[test]
    fn erase_to_end_of_screen_from_the_origin_blanks_everything() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"one\r\ntwo\x1b[H\x1b[0J");
        assert_eq!(
            grid.rendered(),
            "",
            "cursor was at (0,0): the whole grid is erased too"
        );
    }

    #[test]
    fn erase_to_end_of_screen_leaves_content_before_the_cursor_alone() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"keep\r\nabcdef\x1b[2;3H\x1b[0J");
        assert_eq!(
            grid.rendered(),
            "keep\nab",
            "the row before the cursor's row, and the columns before the cursor on its own row, must survive"
        );
    }

    #[test]
    fn erase_to_end_of_line_leaves_earlier_columns_alone() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"abcdef\x1b[3D\x1b[K");
        assert_eq!(grid.rendered().lines().next().unwrap(), "abc");
    }

    // ------------------------------------------------------------ scrolling

    #[test]
    fn line_feed_at_the_scroll_bottom_scrolls_up() {
        let mut grid = TerminalGrid::new_at_fixture_size();
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
        let mut grid = TerminalGrid::new_at_fixture_size();
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
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"\x1b[1;31mx\x1b[0m\x1b[6n\x1b[c\x1b[>0q\x1b[?u");
        assert_eq!(grid.rendered(), "x");
    }

    #[test]
    fn dec_private_mode_toggles_including_mouse_tracking_and_alt_screen_are_no_ops() {
        // The exact cluster from claude-trust-to-composer.raw's alt-screen
        // switch: ?1049h ?1000h ?1002h ?1003h ?1006h, none of them a panic,
        // none of them moving the cursor.
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?25hx");
        assert_eq!(grid.rendered(), "x");
    }

    // --------------------------------------------------------- OSC / charset

    #[test]
    fn osc_sequences_are_skipped_with_either_terminator_and_no_effect() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"a\x1b]0;a window title\x07b");
        assert_eq!(grid.rendered(), "ab");
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"a\x1b]0;a window title\x1b\\b");
        assert_eq!(grid.rendered(), "ab");
    }

    #[test]
    fn an_osc_terminator_split_across_reads_is_held_incomplete() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"a\x1b]0;title");
        grid.push(b"\x07b");
        assert_eq!(grid.rendered(), "ab");
    }

    #[test]
    fn charset_designation_consumes_all_three_bytes() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"before\x1b(Bafter");
        assert_eq!(grid.rendered(), "beforeafter");
    }

    #[test]
    fn a_charset_designation_split_across_reads_is_held_incomplete() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"before\x1b(");
        grid.push(b"Bafter");
        assert_eq!(grid.rendered(), "beforeafter");
    }

    #[test]
    fn a_bare_c0_shift_in_control_is_dropped_without_effect() {
        // SI (0x0f): present 3 times in the current fixture set, always
        // bare. This grid tracks no charset-selection register, so it is
        // ignored the same way a charset designation is.
        let mut grid = TerminalGrid::new_at_fixture_size();
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
                let mut grid = TerminalGrid::new_at_fixture_size();
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
        let mut grid = TerminalGrid::new_at_fixture_size();
        let bytes = "trust\u{2014}folder".as_bytes();
        grid.push(&bytes[..6]); // splits the em dash
        grid.push(&bytes[6..]);
        assert_eq!(grid.rendered(), "trust\u{2014}folder");
    }

    #[test]
    fn invalid_utf8_is_dropped_without_stalling_the_scan() {
        let mut grid = TerminalGrid::new_at_fixture_size();
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
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"\x1b[5Z"); // CBT, not in the current fixture set
    }

    #[test]
    #[should_panic(expected = "unhandled CSI final byte")]
    fn an_unrecognized_erase_parameter_on_j_panics_rather_than_being_ignored() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"\x1b[1J"); // erase-to-cursor: never appears, must not be guessed at
    }

    #[test]
    #[should_panic(expected = "unhandled CSI final byte")]
    fn an_unrecognized_erase_parameter_on_k_panics_rather_than_being_ignored() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        // `3` rather than `1`: this test used `1K` until the copilot
        // capture arrived emitting it for real, at which point it stopped
        // being unrecognized and had to be implemented. The test's subject
        // is the refusal to guess at an unknown parameter, not that
        // parameter in particular, so it moves to one nothing emits.
        grid.push(b"\x1b[3K");
    }

    /// `CSI 3 J` clears the scrollback and must leave the visible screen
    /// standing. The distinction is the entire reason it is a no-op here
    /// rather than an alias for `2 J`: a model that cleared the screen on
    /// it would lose a surface the terminal still shows, and a predicate
    /// reading that model would report an empty screen for a populated
    /// one.
    #[test]
    fn erase_scrollback_on_j_leaves_the_visible_screen_alone() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"a visible line");
        grid.push(b"\x1b[3J");
        assert!(
            grid.shows("a visible line"),
            "3J erases saved lines, not the screen the grid models"
        );
        assert!(
            grid.unsupported().is_none(),
            "3J is handled, not marked unsupported"
        );
    }

    /// The parameter the test above used to carry, now that a committed
    /// capture emits it: erase from the start of the line through the
    /// cursor, inclusive of the cursor cell.
    #[test]
    fn erase_to_cursor_on_k_clears_the_line_start_including_the_cursor_cell() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        grid.push(b"abcdef");
        grid.push(b"\x1b[1;4H"); // cursor onto the 'd'
        grid.push(b"\x1b[1K");
        let first_row = grid
            .rendered()
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            first_row.trim_end(),
            "    ef",
            "columns 1..=4 must be cleared and 'e','f' left standing"
        );
        // The negative half: the cursor cell itself is gone, not merely
        // the cells before it -- the `..=` in the helper is the point.
        assert!(!grid.shows("d"), "the cursor cell 'd' must be cleared too");
    }

    #[test]
    #[should_panic(expected = "unrecognized single-byte escape")]
    fn an_unrecognized_single_byte_escape_panics_rather_than_being_ignored() {
        let mut grid = TerminalGrid::new_at_fixture_size();
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
        let mut grid = TerminalGrid::new_at_fixture_size();
        assert_eq!(grid.unsupported_count(), 0);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            grid.push(b"\x1b[5Z"); // CBT, not in the current fixture set
        }));
        assert!(
            outcome.is_err(),
            "test builds must still panic on a novel CSI final byte"
        );
        // What `unsupported()` stores is the closed-set class, not the
        // raw diagnostic -- this is what a `RuntimeEvent` failure detail
        // is allowed to surface (see `mark_unsupported`'s own doc
        // comment for why raw parameter bytes must never reach here).
        assert_eq!(
            grid.unsupported(),
            Some("CSI 'Z' (CBT)"),
            "the stored class must be the closed-set mnemonic, not the raw diagnostic"
        );
        assert_eq!(
            grid.unsupported_count(),
            1,
            "the count must reflect this one recorded sequence"
        );
        // The fuller, raw-bytes-including diagnostic still exists -- it
        // is just confined to the panic message, never journaled. Reading
        // it back proves this test's own instrument (the class name
        // above) is not merely a weaker check on a message that was
        // secretly identical all along.
        let panic_message = outcome
            .unwrap_err()
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            panic_message.contains("unhandled CSI final byte"),
            "unexpected panic message: {panic_message:?}"
        );
        assert!(
            panic_message.contains("'Z'"),
            "the final byte itself must still be named in the panic diagnostic: \
             {panic_message:?}"
        );
    }

    /// The redaction-safety property `mark_unsupported`'s doc comment
    /// promises: whatever a vendor's own escape-sequence parameters
    /// happen to carry must never appear in what `unsupported()` stores.
    /// CSI's own grammar restricts parameter bytes to `0x20..=0x3f`
    /// (digits and punctuation, never letters -- a "phrase" cannot
    /// literally occupy this position at all), so this uses parameters
    /// distinctive enough to notice a leak (an unusual digit run) rather
    /// than an implausible letter-carrying payload the grammar itself
    /// could never deliver here.
    #[test]
    fn an_unsupported_sequences_params_never_reach_the_stored_class() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Z is the unrecognized final byte; 90210;12345 are its
            // (equally unrecognized, and irrelevant to the class) params.
            grid.push(b"\x1b[90210;12345Z");
        }));
        assert!(outcome.is_err());
        let class = grid
            .unsupported()
            .expect("mark_unsupported must record before it panics, not after");
        assert_eq!(
            class, "CSI 'Z' (CBT)",
            "the stored class must be exactly the closed-set mnemonic, independent of params"
        );
        assert!(
            !class.contains("90210") && !class.contains("12345"),
            "parameter bytes must never appear in the stored class: {class:?}"
        );
    }

    /// The clear half of the latch's lifecycle: a full repaint (`CSI 2
    /// J`) restores trust, and the count -- unlike the flag -- never
    /// resets, so a grid that clears and re-latches repeatedly is still
    /// visible as a chronically-misparsing vendor.
    #[test]
    fn a_full_erase_clears_unsupported_but_never_the_count() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            grid.push(b"\x1b[5Z");
        }));
        assert!(first.is_err());
        assert!(grid.unsupported().is_some());
        assert_eq!(grid.unsupported_count(), 1);

        grid.push(b"\x1b[2J");
        assert_eq!(
            grid.unsupported(),
            None,
            "a full repaint must clear the latch"
        );
        assert_eq!(
            grid.unsupported_count(),
            1,
            "the count must survive the clear -- it is a lifetime count, not a current-state flag"
        );

        let second = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            grid.push(b"\x1b[6Z");
        }));
        assert!(second.is_err());
        assert_eq!(
            grid.unsupported_count(),
            2,
            "a second, later unsupported sequence must still be counted after a clear"
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

        // A per-fixture floor, not just a total: this also catches one
        // capture silently losing its escapes (a bad rewrite, an
        // accidental re-capture) while the other six still carry plenty --
        // a total-only floor would let that pass unnoticed. Both numbers
        // sit far below the real per-fixture counts (the smallest fixture
        // alone has well over a hundred), so neither is brittle against a
        // future capture shrinking slightly; they exist to catch zero, not
        // to pin an exact count.
        const MIN_PER_FIXTURE: usize = 10;
        const MIN_TOTAL: usize = 500;

        let mut total_compared = 0usize;
        for name in ALL_FIXTURES {
            let bytes = fixture(name);
            let mut i = 0usize;
            let mut compared_in_fixture = 0usize;
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
                        compared_in_fixture += 1;
                        i += ours_len;
                    }
                    (None, EscapeScan::Incomplete) => break,
                    _ => panic!(
                        "{name} at byte {i}: classify_escape and escape_len disagree on whether this escape is complete"
                    ),
                }
            }
            assert!(
                compared_in_fixture >= MIN_PER_FIXTURE,
                "{name}: only {compared_in_fixture} escapes compared -- either this fixture lost \
                 its escapes, or this assertion would be passing vacuously"
            );
            total_compared += compared_in_fixture;
        }
        assert!(
            total_compared >= MIN_TOTAL,
            "only {total_compared} escapes compared across all seven fixtures -- otherwise the \
             agreement this test exists to check would be passing vacuously"
        );
    }
}
