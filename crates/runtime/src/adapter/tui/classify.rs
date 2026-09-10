//! What a vendor's terminal surface currently shows, classified from a
//! [`TerminalGrid`]'s rendered content -- the `classify_surface` `screen.rs`
//! and `grid.rs` both anticipated and deferred to "a later slice, so that
//! the matching rule and the vendor-specific phrases are reviewable
//! separately". This is that slice.
//!
//! Built on [`TerminalGrid`], not [`super::screen::TuiScreen`]: the whole
//! reason the grid exists is that `TuiScreen` cannot represent "was on
//! screen, then cleared" (see `screen.rs`'s own module docs and
//! `an_answered_gate_is_gone_from_the_terminal_but_not_from_the_accumulator`),
//! and a classifier built on the accumulator would inherit that limitation
//! -- a first-run gate answered and cleared would still classify as
//! blocking, forever, on the accumulator's read.
//!
//! `ClaudeTuiVendor`/`CodexTuiVendor`/`CopilotTuiVendor`/`OmpTuiVendor`'s
//! own `TuiVendor::classify_surface` overrides are this module's real
//! production callers, wiring
//! `classify_claude_surface`/`classify_codex_surface`/`classify_copilot_surface`/`classify_omp_surface`
//! into `wait_for_readiness`'s poll (`adapter.rs`) and the Enter-time
//! re-check both depend on.

use super::grid::TerminalGrid;

/// A named first-run gate, vendor-specific. Each variant corresponds to
/// exactly one committed capture that proves it (see the tests below) --
/// no variant here is speculative.
///
/// `pub`, not `pub(crate)`, for the same reason as [`super::grid::TerminalGrid`]:
/// it flows through `TuiVendor::classify_surface`, a `pub` trait method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    ClaudeWorkspaceTrust,
    ClaudeThemePicker,
    ClaudeSignIn,
    CodexDirectoryTrust,
    CodexSignIn,
    CopilotFolderTrust,
}

/// What a vendor's terminal surface currently shows. `pub` for the same
/// reason as [`GateKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Surface {
    /// A recognized first-run gate is blocking the run; see [`GateKind`].
    /// Matched BEFORE the grid-trust check below, deliberately -- see
    /// [`classify_surface`]'s own doc comment for why a positive gate
    /// match is trusted even against an [`Surface::Unreadable`] grid.
    Gate(GateKind),
    /// The vendor's main composer is up and no known gate is blocking it.
    PromptReady,
    /// Neither a known gate nor prompt-readiness is recognizable on the
    /// current surface, and the grid itself is trustworthy (no
    /// unsupported escape sequence is currently latched). This is not
    /// "probably fine" -- it is the state a caller must park on and
    /// escalate from, never treat as ready: a truly novel gate this
    /// module does not yet know about renders exactly like this.
    Undecided,
    /// An escape sequence this grid could not apply safely is currently
    /// latched (see [`TerminalGrid::unsupported`]), so nothing below a
    /// matched gate phrase can be trusted -- a positive gate match still
    /// wins over this (see [`classify_surface`]), but the absence of one
    /// proves nothing here. Carries the same closed-set class name
    /// [`TerminalGrid::unsupported`] returns, for a caller's failure
    /// detail to name -- never the raw sequence; see that method's own
    /// doc comment for why.
    Unreadable(String),
}

// --------------------------------------------------------------- claude

const CLAUDE_WORKSPACE_TRUST_TITLE: &str = "Accessing workspace:";
const CLAUDE_THEME_PICKER_TITLE: &str = "Choose the text style that looks best with your terminal";
const CLAUDE_SIGNIN_TITLE: &str = "Select login method:";
/// The composer's own banner. **Weaker than the other six phrase
/// constants here, by design, and accepted as such (maintainer's
/// ruling):** every other phrase in this file names a screen no OTHER
/// screen also shows, verified below. This one does not -- claude's
/// sign-in gate (`claude-signin-method.raw`) also carries this exact
/// banner text (checked directly against the fixture, not assumed), so on
/// its own this phrase cannot distinguish "ready" from "showing a gate
/// that happens to share a banner". It is safe here ONLY because
/// [`classify_claude_surface`] checks every known gate first and returns
/// on the first match -- if a future edit reorders those checks to run
/// after this one, or adds this phrase to a decision made before the gate
/// checks, that ordering is what silently breaks, not this constant.
const CLAUDE_PROMPT_READY_BANNER: &str = "Claude Code";

/// Classifies claude's current terminal surface using the shared
/// precedence (see [`classify_surface`]); gate checks run in the order
/// above and return on the first match -- see
/// [`CLAUDE_PROMPT_READY_BANNER`]'s doc comment for why that order is
/// load-bearing, not incidental.
pub(crate) fn classify_claude_surface(grid: &TerminalGrid) -> Surface {
    classify_surface(
        grid,
        &[
            (CLAUDE_WORKSPACE_TRUST_TITLE, GateKind::ClaudeWorkspaceTrust),
            (CLAUDE_THEME_PICKER_TITLE, GateKind::ClaudeThemePicker),
            (CLAUDE_SIGNIN_TITLE, GateKind::ClaudeSignIn),
        ],
        CLAUDE_PROMPT_READY_BANNER,
    )
}

// ---------------------------------------------------------------- codex

const CODEX_DIRECTORY_TRUST_TITLE: &str = "Do you trust the contents of this directory?";
const CODEX_SIGNIN_TITLE: &str = "Sign in with ChatGPT";
/// The composer's own banner. Genuinely exclusive to the composer being
/// up -- it is shown in `codex-composer-then-trust.raw` (before that
/// fixture's own gate is drawn) and transiently in `codex-directory-
/// trust.raw` (after trust is granted, on the way to that fixture's own
/// final startup output), never on a codex screen where the composer
/// itself is not genuinely on screen (verified below). Still, gate checks
/// run first below, for symmetry with claude's classifier and because a
/// novel codex gate sharing this banner is exactly as unproven either way.
const CODEX_PROMPT_READY: &str = "Ask Codex to do anything";

/// Classifies codex's current terminal surface. See
/// [`classify_claude_surface`] for the shared structure both use.
pub(crate) fn classify_codex_surface(grid: &TerminalGrid) -> Surface {
    classify_surface(
        grid,
        &[
            (CODEX_DIRECTORY_TRUST_TITLE, GateKind::CodexDirectoryTrust),
            (CODEX_SIGNIN_TITLE, GateKind::CodexSignIn),
        ],
        CODEX_PROMPT_READY,
    )
}

// ------------------------------------------------------------- copilot

const COPILOT_FOLDER_TRUST_TITLE: &str = "Confirm folder trust";
/// The composer's own footer chrome, present whenever the composer is up
/// regardless of sign-in state -- `copilot-composer.raw` (the fixture
/// this constant is keyed on) was captured signed OUT (its status line
/// reads "Please use /login to sign in to use Copilot"), so a phrase
/// naming that state would fail closed on every normal, signed-in
/// machine instead. This is deliberately the footer's own help text, not
/// anything from the status line above it: verified directly (see the
/// tests below) that it is absent from `copilot-folder-trust.raw`'s own
/// footer, which reads "Tip: /theme" while the dialog is up.
///
/// **That replacement is what makes keying on the footer safe at all, not
/// an incidental fact.** Copilot's folder-trust dialog REPLACES the
/// footer rather than drawing over the composer's own -- if a gate
/// instead overlaid the composer chrome while leaving it on screen, this
/// phrase would still be visible on a blocking surface and this predicate
/// would classify it `PromptReady`, pasting into it. Copilot's sign-in
/// gate has never been observed (isolation covered `HOME` and the XDG
/// variables but not the macOS Keychain, so an operator's own credentials
/// may have satisfied it) -- if it turns out to overlay rather than
/// replace, this predicate needs a check for that gate too, not just a
/// new fixture. A signed-in composer's exact rendering is also unmeasured
/// (see `release/live-conformance/2026-09-10-copilot-omp-first-run.md`'s
/// own Limits section); this predicate never reads the status line, so it
/// carries no dependency on that particular gap.
const COPILOT_PROMPT_READY_FOOTER: &str = "open sidebar";

/// Classifies copilot's current terminal surface. See
/// [`classify_claude_surface`] for the shared structure both use.
pub(crate) fn classify_copilot_surface(grid: &TerminalGrid) -> Surface {
    classify_surface(
        grid,
        &[(COPILOT_FOLDER_TRUST_TITLE, GateKind::CopilotFolderTrust)],
        COPILOT_PROMPT_READY_FOOTER,
    )
}

// -------------------------------------------------------------- omp

/// omp raises no per-directory trust dialog anywhere in its source
/// (verified by reading it, not by absence of a capture -- `trust` occurs
/// zero times across every observed screen). What it raises instead is a
/// global, five-step first-run setup wizard that blocks the composer
/// until it completes or is skipped; the maintainer's ruling is that
/// crew never launches omp under a fresh home, so a machine showing this
/// wizard is one where omp was never configured -- an operator error,
/// not a decision waiting for a human to make through crew. There is
/// therefore no `GateKind` for omp: the wizard, and everything else this
/// module does not positively recognize, falls through to `Undecided`
/// and fails the start closed with a typed error, exactly like a
/// genuinely novel screen would.
///
/// This phrase is drawn from omp's own "Tips" panel (`# for prompt
/// actions`), not the welcome banner, the LSP-servers list, or the
/// recent-sessions list beside it -- those two are conditional on empty
/// state (`if (this.lspServers.length === 0)` /
/// `if (this.recentSessions.length === 0)`, oh-my-pi
/// `packages/coding-agent/src/modes/components/welcome.ts:340-341` and
/// `:314-315` at tag `v18.1.16`) and would read differently once
/// providers or sessions exist. The Tips lines are hardcoded into the
/// same render pass with no such condition
/// (`welcome.ts:363-367`). That the whole welcome panel is itself a
/// startup-only element -- constructed only when `!preferences.quiet`
/// and retired once the transcript fills the screen
/// (`packages/coding-agent/src/modes/composer.ts:242`, `:405-426`) -- is
/// true and does not weaken this: `classify_omp_surface` is reached from
/// exactly two call sites in this codebase, `wait_for_readiness` and
/// `enter_precondition`, both scoped to the startup window before the
/// first prompt is delivered, which is precisely the window in which
/// this panel is what is actually on screen.
///
/// The one config key that defeats this: `startup.quiet` (omp's own
/// `config.yml`, schema documented as suppressing "all startup chrome
/// including the splash") reaches the same `preferences.quiet` check
/// above end to end (`packages/coding-agent/src/main.ts:1602-1603` and
/// `interactive-mode.ts:1228-1229`, both feeding
/// `Composer.setPreferences`) -- a user who has set it gets no welcome
/// panel at all, and this predicate never sees `PromptReady`. Crew's own
/// launch never requests quiet mode (`OmpTuiVendor::launch` passes no
/// such flag), but an operator's own config can still set it; see
/// [`TuiVendor::readiness_failure_hint`]'s override on
/// [`super::omp::OmpTuiVendor`] for the resulting failure's remedy, and
/// `docs/compatibility.md`'s "TUI First-Run Gate Detection" section for
/// the same fact stated as a prerequisite.
///
/// Not keyed on the persistent status line instead, even though the line
/// survives `startup.quiet` and the welcome panel does not: counted as
/// literal bytes in `omp-composer.raw`, the status line's words are
/// escape-interleaved and never appear as a contiguous run, only the
/// welcome panel's do. A phrase has to match the same way every other
/// vendor's does here -- as bytes actually adjacent in the capture, not
/// words that merely render adjacent on screen (see
/// `fixtures/adapters/tui-screens/README.md`'s "Why these are captures
/// and not hand-written strings", the claude/codex per-word escape trap
/// this same rule already guards against) -- so the panel is not a
/// compromise beside a spared status line; it is the only phrase in this
/// capture that was ever eligible.
const OMP_PROMPT_READY_TIP: &str = "for prompt actions";

/// Classifies omp's current terminal surface. Unlike every other
/// classifier in this module, there is no gate list at all -- see
/// [`OMP_PROMPT_READY_TIP`]'s own doc comment for why.
pub(crate) fn classify_omp_surface(grid: &TerminalGrid) -> Surface {
    classify_surface(grid, &[], OMP_PROMPT_READY_TIP)
}

/// The precedence every vendor's classifier above shares, parameterized
/// on its own gate list and ready phrase:
///
/// 1. a gate phrase match, checked first, regardless of the grid's
///    trustworthiness;
/// 2. the grid being untrustworthy (an unsupported escape sequence is
///    currently latched);
/// 3. the ready phrase;
/// 4. otherwise, nothing matched.
///
/// Gate detection runs before the trust check -- the reverse of this
/// module's original order -- because the two kinds of evidence are not
/// symmetric. An unimplemented escape sequence can only distort layout:
/// it moves things, erases things, leaves stale content standing: it
/// never invents text that was not already somewhere in the vendor's own
/// output. So a positive phrase match (`grid.shows(phrase)` returning
/// true) is real evidence even when the grid is otherwise untrustworthy,
/// while the ABSENCE of one proves nothing about a grid that might be
/// hiding a phrase behind exactly the distortion that made it
/// untrustworthy. Checking gates only after ruling the grid untrustworthy
/// (this module's original order) let one unimplemented sequence
/// permanently blind every later check to a gate that was genuinely,
/// visibly on screen -- the live regression a real first-run sign-in gate
/// went unrecognized for.
fn classify_surface(grid: &TerminalGrid, gates: &[(&str, GateKind)], ready: &str) -> Surface {
    for (phrase, kind) in gates {
        if grid.shows(phrase) {
            return Surface::Gate(*kind);
        }
    }
    if let Some(class) = grid.unsupported() {
        return Surface::Unreadable(class.to_string());
    }
    if grid.shows(ready) {
        return Surface::PromptReady;
    }
    Surface::Undecided
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // The one definition lives in `grid.rs`, guarded there by
    // `grid::tests::all_fixtures_matches_the_committed_directory` -- see
    // its own doc comment for why a second copy here would be a check
    // that certifies only whichever copy it reads.
    use crate::adapter::tui::grid::ALL_FIXTURES;

    fn fixture(name: &str) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/adapters/tui-screens")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
    }

    fn grid_from(name: &str) -> TerminalGrid {
        TerminalGrid::from_bytes(&fixture(name))
    }

    /// Pushes a fixture in real-PTY-read-sized chunks, reporting whether
    /// `predicate` was ever true for the grid's state after some chunk
    /// (checked after every chunk, including the last) -- not just its
    /// final state. Two of the seven fixtures (see
    /// `claude_signin_is_shown_before_it_moves_on_to_a_later_screen` and
    /// `codex_directory_trust_is_shown_before_it_moves_on_to_startup`)
    /// answer their own gate and continue recording well past it, so
    /// "does this fixture's FINAL state show the gate" is the wrong
    /// question for them -- "was the gate ever genuinely on screen during
    /// this capture" is what actually proves the fixture demonstrates
    /// what its name claims, and it is also the strictly stronger check
    /// for the fixtures whose final state IS the gate.
    fn ever_shows(bytes: &[u8], phrase: &str) -> bool {
        let mut grid = TerminalGrid::new_at_fixture_size();
        let mut ever = false;
        for chunk in bytes.chunks(64) {
            grid.push(chunk);
            if grid.shows(phrase) {
                ever = true;
            }
        }
        ever
    }

    // ------------------------------------------------- the negative half first

    /// Every phrase this module classifies on, checked against every
    /// fixture with `ever_shows` -- a phrase that ever appeared on some
    /// fixture outside its owner list, at any point during that capture,
    /// would make every positive test below pass vacuously (see
    /// `screen.rs`'s own `shows_does_not_match_a_phrase_that_is_not_on_screen`
    /// for the same discipline applied to the accumulator).
    /// Three phrases legitimately have more than one owner, each for the
    /// same reason: a second (or third) fixture exists specifically to
    /// show that same gate or screen reached from a different path, so it
    /// necessarily shows the phrase too, checked directly, not assumed.
    /// `CLAUDE_WORKSPACE_TRUST_TITLE` -- `claude-trust-to-composer.raw`
    /// shows this gate being ANSWERED, briefly, before moving on to the
    /// composer. `CODEX_DIRECTORY_TRUST_TITLE` -- `codex-composer-then-
    /// trust.raw` shows the composer painting FIRST and this gate arriving
    /// after it, which is also that fixture's final state (see
    /// `every_committed_capture_classifies_as_expected`). `CODEX_PROMPT_
    /// READY` -- `codex-directory-trust.raw` shows the composer transiently
    /// once trust is granted, on its way to the fuller startup output that
    /// is its own final state (see
    /// `codex_directory_trust_is_shown_before_it_moves_on_to_startup`).
    /// `CLAUDE_PROMPT_READY_BANNER` is excluded from this table entirely
    /// on purpose -- its doc comment already states, and a dedicated test
    /// below proves, that it is NOT exclusive to the ready screen;
    /// asserting exclusivity for it here would contradict the very
    /// limitation this module documents.
    #[test]
    fn each_exclusive_phrase_matches_only_its_own_fixtures() {
        const CASES: &[(&str, &[&str])] = &[
            (
                CLAUDE_WORKSPACE_TRUST_TITLE,
                &["claude-workspace-trust.raw", "claude-trust-to-composer.raw"],
            ),
            (CLAUDE_THEME_PICKER_TITLE, &["claude-theme-picker.raw"]),
            (CLAUDE_SIGNIN_TITLE, &["claude-signin-method.raw"]),
            (
                CODEX_DIRECTORY_TRUST_TITLE,
                &["codex-directory-trust.raw", "codex-composer-then-trust.raw"],
            ),
            (CODEX_SIGNIN_TITLE, &["codex-signin.raw"]),
            (
                CODEX_PROMPT_READY,
                &["codex-composer-then-trust.raw", "codex-directory-trust.raw"],
            ),
            (COPILOT_FOLDER_TRUST_TITLE, &["copilot-folder-trust.raw"]),
            (COPILOT_PROMPT_READY_FOOTER, &["copilot-composer.raw"]),
            (OMP_PROMPT_READY_TIP, &["omp-composer.raw"]),
        ];

        for (phrase, owners) in CASES {
            for candidate in ALL_FIXTURES {
                let expected = owners.contains(candidate);
                assert_eq!(
                    ever_shows(&fixture(candidate), phrase),
                    expected,
                    "phrase {phrase:?} (owned by {owners:?}) ever shown on {candidate}: expected {expected}"
                );
            }
        }
    }

    /// The limitation stated in `CLAUDE_PROMPT_READY_BANNER`'s own doc
    /// comment, proven rather than asserted: claude's sign-in gate really
    /// does carry the composer's banner text too, at some point during
    /// that capture (the same window in which the gate itself is shown --
    /// see `claude_signin_is_shown_before_it_moves_on_to_a_later_screen`).
    #[test]
    fn the_claude_prompt_ready_banner_also_appears_on_the_signin_gate() {
        assert!(
            ever_shows(
                &fixture("claude-signin-method.raw"),
                CLAUDE_PROMPT_READY_BANNER
            ),
            "if this ever fails, the limitation documented on CLAUDE_PROMPT_READY_BANNER no longer \
             holds and the ordering caveat in its doc comment can be revisited"
        );
    }

    // ------------------------------------------------------- claude, positive

    #[test]
    fn classifies_claude_workspace_trust() {
        assert_eq!(
            classify_claude_surface(&grid_from("claude-workspace-trust.raw")),
            Surface::Gate(GateKind::ClaudeWorkspaceTrust)
        );
    }

    #[test]
    fn classifies_claude_theme_picker() {
        assert_eq!(
            classify_claude_surface(&grid_from("claude-theme-picker.raw")),
            Surface::Gate(GateKind::ClaudeThemePicker)
        );
    }

    /// Unlike the other two claude gates, this capture does not end on the
    /// gate: the user selects a login method and the vendor moves on to
    /// "Opening browser to sign in..." (verified directly -- the gate
    /// phrase is shown, then stops being shown, well before the capture
    /// ends). Proving this gate positively means finding the real window
    /// where it was genuinely on screen, not assuming the capture freezes
    /// there the way `claude-workspace-trust.raw`/`claude-theme-picker.raw`
    /// do.
    #[test]
    fn claude_signin_is_shown_before_it_moves_on_to_a_later_screen() {
        let bytes = fixture("claude-signin-method.raw");
        let mut grid = TerminalGrid::new_at_fixture_size();
        let mut saw_signin = false;
        for chunk in bytes.chunks(64) {
            grid.push(chunk);
            if classify_claude_surface(&grid) == Surface::Gate(GateKind::ClaudeSignIn) {
                saw_signin = true;
            }
        }
        assert!(
            saw_signin,
            "claude-signin-method.raw must show the SignIn gate at some point"
        );
        assert_ne!(
            classify_claude_surface(&grid),
            Surface::Gate(GateKind::ClaudeSignIn),
            "this capture continues past a login method being selected -- its final state is NOT the gate"
        );
    }

    /// The capture that exists specifically to prove a grid can return to
    /// `PromptReady` after a gate is answered, where `TuiScreen` cannot
    /// (see `screen.rs`'s own module docs). This is the whole reason
    /// `classify_surface` is built on the grid, not the accumulator.
    #[test]
    fn classifies_claude_prompt_ready_after_the_gate_is_answered() {
        assert_eq!(
            classify_claude_surface(&grid_from("claude-trust-to-composer.raw")),
            Surface::PromptReady
        );
    }

    // -------------------------------------------------------- codex, positive

    /// Like `claude-signin-method.raw`, this capture does not end on its
    /// own gate: the user grants trust and codex proceeds to its normal
    /// startup output (an MCP connection error, in this particular
    /// capture), which the recording continues past. Same technique as
    /// claude's equivalent case: find the real window, don't assume one.
    #[test]
    fn codex_directory_trust_is_shown_before_it_moves_on_to_startup() {
        let bytes = fixture("codex-directory-trust.raw");
        let mut grid = TerminalGrid::new_at_fixture_size();
        let mut saw_gate = false;
        for chunk in bytes.chunks(64) {
            grid.push(chunk);
            if classify_codex_surface(&grid) == Surface::Gate(GateKind::CodexDirectoryTrust) {
                saw_gate = true;
            }
        }
        assert!(
            saw_gate,
            "codex-directory-trust.raw must show the DirectoryTrust gate at some point"
        );
        assert_ne!(
            classify_codex_surface(&grid),
            Surface::Gate(GateKind::CodexDirectoryTrust),
            "this capture continues past trust being granted -- its final state is NOT the gate"
        );
    }

    #[test]
    fn classifies_codex_signin() {
        assert_eq!(
            classify_codex_surface(&grid_from("codex-signin.raw")),
            Surface::Gate(GateKind::CodexSignIn)
        );
    }

    /// `codex-composer-then-trust.raw` never ENDS on `PromptReady` -- the
    /// gate arrives partway through and is the fixture's final state (see
    /// `every_committed_capture_classifies_as_expected` below), so proving
    /// codex's `PromptReady` positively means finding the real prefix
    /// where the composer is up and the gate has not yet been drawn, not
    /// assuming a byte offset. Pushed incrementally in real PTY-read-sized
    /// chunks (matching how the vendor's own output actually arrives),
    /// recording the classify result after every chunk: this discovers
    /// the transition from the data itself rather than a precomputed
    /// number that could go stale the moment the fixture is re-captured.
    #[test]
    fn classifies_codex_prompt_ready_before_its_gate_is_drawn() {
        let bytes = fixture("codex-composer-then-trust.raw");
        let mut grid = TerminalGrid::new_at_fixture_size();
        let mut saw_prompt_ready = false;
        for chunk in bytes.chunks(64) {
            grid.push(chunk);
            if classify_codex_surface(&grid) == Surface::PromptReady {
                saw_prompt_ready = true;
            }
        }
        assert!(
            saw_prompt_ready,
            "codex-composer-then-trust.raw must show PromptReady at some point before its gate is drawn"
        );
        assert_eq!(
            classify_codex_surface(&grid),
            Surface::Gate(GateKind::CodexDirectoryTrust),
            "the fixture's FINAL state is the gate, drawn after the composer -- PromptReady is a \
             real but transient state in this capture, not its ending"
        );
    }

    // ------------------------------------------------------- copilot, positive

    #[test]
    fn classifies_copilot_folder_trust() {
        assert_eq!(
            classify_copilot_surface(&grid_from("copilot-folder-trust.raw")),
            Surface::Gate(GateKind::CopilotFolderTrust)
        );
    }

    /// `copilot-composer.raw` is captured signed OUT of Copilot (see
    /// `COPILOT_PROMPT_READY_FOOTER`'s own doc comment); this test pins
    /// exactly which bytes the predicate keys on and confirms the status
    /// line naming that state plays no part in the classification --
    /// removing the status line from a hypothetical future fixture must
    /// not change this result.
    #[test]
    fn classifies_copilot_prompt_ready_from_footer_chrome_not_the_status_line() {
        let bytes = fixture("copilot-composer.raw");
        assert!(
            bytes
                .windows(b"Please use /login to sign in to use Copilot".len())
                .any(|w| w == b"Please use /login to sign in to use Copilot"),
            "this fixture must still be the not-signed-in capture the predicate is deliberately \
             indifferent to -- if this fails, the fixture changed and the indifference this test \
             proves is no longer being tested"
        );
        assert_eq!(
            classify_copilot_surface(&grid_from("copilot-composer.raw")),
            Surface::PromptReady,
            "must classify PromptReady from footer chrome ({COPILOT_PROMPT_READY_FOOTER:?}) alone, \
             regardless of the not-signed-in status line also present on this screen"
        );
    }

    // ----------------------------------------------------------- omp, positive

    #[test]
    fn classifies_omp_prompt_ready_from_the_tips_panel() {
        assert_eq!(
            classify_omp_surface(&grid_from("omp-composer.raw")),
            Surface::PromptReady
        );
    }

    /// The negative half of requirement (b) for omp: its five-step setup
    /// wizard has no `GateKind` at all (see [`OMP_PROMPT_READY_TIP`]'s own
    /// doc comment), so a capture of it must classify `Undecided` --
    /// recognized as "not ready", never paste-worthy, and never a novel
    /// gate this module invents a variant for.
    #[test]
    fn classifies_omp_setup_wizard_as_undecided_not_a_gate() {
        assert_eq!(
            classify_omp_surface(&grid_from("omp-setup-step1.raw")),
            Surface::Undecided
        );
    }

    // --------------------------------------------------------- exhaustive

    /// Every committed capture's classify result under its own vendor's
    /// classifier, at its FINAL state -- exhaustively for the fixtures
    /// whose final state is well-defined and vendor-meaningful.
    /// `claude-signin-method.raw` and `codex-directory-trust.raw` are
    /// deliberately absent from this table: their own dedicated tests
    /// above already establish (and explain) what their final state
    /// actually is, which is not their own gate.
    #[test]
    fn every_committed_capture_classifies_as_expected() {
        for (name, expected) in [
            (
                "claude-workspace-trust.raw",
                Surface::Gate(GateKind::ClaudeWorkspaceTrust),
            ),
            (
                "claude-theme-picker.raw",
                Surface::Gate(GateKind::ClaudeThemePicker),
            ),
            ("claude-trust-to-composer.raw", Surface::PromptReady),
        ] {
            assert_eq!(
                classify_claude_surface(&grid_from(name)),
                expected,
                "{name} under classify_claude_surface"
            );
        }
        for (name, expected) in [
            ("codex-signin.raw", Surface::Gate(GateKind::CodexSignIn)),
            (
                "codex-composer-then-trust.raw",
                Surface::Gate(GateKind::CodexDirectoryTrust),
            ),
        ] {
            assert_eq!(
                classify_codex_surface(&grid_from(name)),
                expected,
                "{name} under classify_codex_surface"
            );
        }
        for (name, expected) in [
            (
                "copilot-folder-trust.raw",
                Surface::Gate(GateKind::CopilotFolderTrust),
            ),
            ("copilot-composer.raw", Surface::PromptReady),
        ] {
            assert_eq!(
                classify_copilot_surface(&grid_from(name)),
                expected,
                "{name} under classify_copilot_surface"
            );
        }
        for (name, expected) in [
            ("omp-composer.raw", Surface::PromptReady),
            ("omp-setup-step1.raw", Surface::Undecided),
        ] {
            assert_eq!(
                classify_omp_surface(&grid_from(name)),
                expected,
                "{name} under classify_omp_surface"
            );
        }
    }

    /// A surface with no recognizable gate and no prompt-ready banner is
    /// `Undecided`, not a guess -- a fresh grid with nothing pushed into
    /// it is the simplest such surface.
    #[test]
    fn an_empty_surface_is_undecided_for_both_vendors() {
        let grid = TerminalGrid::new_at_fixture_size();
        assert_eq!(classify_claude_surface(&grid), Surface::Undecided);
        assert_eq!(classify_codex_surface(&grid), Surface::Undecided);
        assert_eq!(classify_copilot_surface(&grid), Surface::Undecided);
        assert_eq!(classify_omp_surface(&grid), Surface::Undecided);
    }

    /// The single most important test in this module: a real, on-screen
    /// gate must never be masked by an unrelated unsupported escape
    /// sequence appearing anywhere else in the same output. This is the
    /// live regression (a genuine codex sign-in gate went unrecognized
    /// because an unimplemented escape sequence had already latched
    /// `TerminalGrid::unsupported`, and the classifier used to check that
    /// flag BEFORE any gate phrase) -- see [`classify_surface`]'s own doc
    /// comment for why gate phrases are checked first now. Deliberately
    /// asserted through `catch_unwind` around the actual `push` that
    /// trips `mark_unsupported`, not by calling a lower-level setter,
    /// so this exercises the exact same recording path production hits.
    #[test]
    fn a_gate_phrase_survives_an_unsupported_sequence_and_still_returns_gate() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        // Inspecting `grid` after the panic is only meaningful because
        // `mark_unsupported` records before it panics, not after (see its
        // own doc comment in grid.rs) -- that ordering is exactly what
        // this test depends on to say anything at all.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            grid.push(format!("{CLAUDE_WORKSPACE_TRUST_TITLE}\x1b[5Z").as_bytes());
        }));
        assert!(
            outcome.is_err(),
            "test builds still panic on a novel CSI final byte, unchanged by this module"
        );
        assert!(
            grid.unsupported().is_some(),
            "the unsupported sequence must actually have latched for this test to mean anything"
        );
        assert!(
            grid.shows(CLAUDE_WORKSPACE_TRUST_TITLE),
            "the gate phrase must genuinely be on screen for this test to mean anything"
        );
        assert_eq!(
            classify_claude_surface(&grid),
            Surface::Gate(GateKind::ClaudeWorkspaceTrust),
            "a gate phrase actually on screen must be recognized regardless of an unrelated \
             unsupported sequence elsewhere in the same output -- an unimplemented escape can \
             only distort layout, never fabricate the phrase this checks for"
        );
    }

    /// The other side of the same asymmetry: a READY phrase, unlike a
    /// gate phrase, does not get this exemption -- `classify_surface`
    /// checks gates first, then grid trust, then readiness, so an
    /// unsupported sequence still masks `PromptReady` even with the
    /// phrase genuinely on screen. Readiness is intentionally held to the
    /// stricter standard: proceeding into an untrustworthy grid (typing
    /// into it) is the dangerous direction, while recognizing a gate and
    /// parking is always safe.
    #[test]
    fn an_unsupported_sequence_still_masks_a_ready_phrase() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            grid.push(format!("{CLAUDE_PROMPT_READY_BANNER}\x1b[5Z").as_bytes());
        }));
        assert!(outcome.is_err());
        assert!(grid.shows(CLAUDE_PROMPT_READY_BANNER));
        assert!(matches!(
            classify_claude_surface(&grid),
            Surface::Unreadable(_)
        ));
    }

    /// A run recovers classification after the latch clears -- proven at
    /// this module's own level (not just the grid's), since it is
    /// `classify_claude_surface`'s reading of `unsupported()` that a
    /// caller (`wait_for_readiness`) actually depends on to decide
    /// whether polling should continue making progress.
    #[test]
    fn classification_recovers_once_a_full_repaint_clears_the_latch() {
        let mut grid = TerminalGrid::new_at_fixture_size();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            grid.push(b"\x1b[5Z");
        }));
        assert!(outcome.is_err());
        assert!(matches!(
            classify_claude_surface(&grid),
            Surface::Unreadable(_)
        ));

        grid.push(format!("\x1b[2J{CLAUDE_PROMPT_READY_BANNER}").as_bytes());
        assert_eq!(
            classify_claude_surface(&grid),
            Surface::PromptReady,
            "a full repaint clearing the latch, followed by the composer's own banner, must let \
             classification recover -- not stay stuck on the last thing that ever went wrong"
        );
    }
}
