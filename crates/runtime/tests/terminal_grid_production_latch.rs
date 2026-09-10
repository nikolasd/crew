//! Proves `TerminalGrid::mark_unsupported`'s PRODUCTION body (record,
//! never panic) against the code that actually ships -- not an
//! approximation of it.
//!
//! `mark_unsupported` has two bodies: under `#[cfg(test)]` it records
//! then panics, so every unit test inside `crates/runtime/src/` that
//! reaches an unsupported sequence catches it exactly as before this
//! mechanism existed; under `#[cfg(not(test))]` it records and returns.
//! `cfg!(test)` is true throughout `cargo test`'s compilation of this
//! crate's OWN unit-test binary, so no test living inside
//! `crates/runtime/src/` can ever compile a build where the panic itself
//! is absent -- it would need the crate compiled without the `test` cfg,
//! which a unit test cannot arrange for itself. An integration test under
//! `crates/runtime/tests/` is different: it links `crew_runtime` as an
//! ordinary external dependency, and the `test` cfg is not applied to
//! that compilation (it is scoped to the crate's own test-binary build,
//! not to every crate that merely depends on it as a library) -- so this
//! file is, structurally, the one place able to see the shipping body at
//! all.
//!
//! `TerminalGrid` (`crew_runtime::adapter::tui::grid`) is `pub` for
//! exactly this reason -- see its module's own doc comment.

use crew_runtime::adapter::tui::grid::TerminalGrid;

/// `#[cfg(not(test))]`'s whole promise: a novel escape sequence in this
/// binary's build of `crew_runtime` is recorded, and `push` returns
/// normally rather than unwinding. Before this mechanism existed, the
/// production consequence of an unsupported sequence was a `String`
/// stored once and never read; the two `#[cfg]` bodies split apart when a
/// real caller (`classify_surface`) started reading it, and this test is
/// what makes that split's non-panicking half provable at all.
#[test]
fn a_novel_escape_sequence_is_recorded_without_panicking() {
    let mut grid = TerminalGrid::new(120, 32);
    assert_eq!(grid.unsupported(), None);
    assert_eq!(grid.unsupported_count(), 0);

    // CBT (`CSI n Z`), not implemented by `apply_csi` -- see grid.rs's
    // own doc comment on its implemented CSI set.
    grid.push(b"\x1b[5Z");

    assert_eq!(
        grid.unsupported(),
        Some("CSI 'Z' (CBT)"),
        "a production build must record the closed-set class rather than panicking"
    );
    assert_eq!(grid.unsupported_count(), 1);

    // The grid must still be usable afterward -- a production build
    // that recorded the fact must not be otherwise broken by it. `push`
    // returning normally (this line executing at all, past the sequence
    // above, in a build with no panic in it) is that proof; `shows`/
    // `rendered` are `pub(crate)` and unreachable from here, which is
    // itself consistent with this file's own narrow purpose (proving
    // `mark_unsupported`'s production body, not general grid content).
    grid.push(b"still usable");
}

/// The latch's other half, proven on the same shipping build: a full
/// repaint clears it, and a run recovers classification through
/// `push`/`shows` alone -- this file has no access to `classify_surface`
/// itself (`pub(crate)`), so it proves recovery at the level it can
/// actually reach, the grid's own content-trust state.
#[test]
fn the_latch_clears_on_a_full_repaint_in_the_production_build() {
    let mut grid = TerminalGrid::new(120, 32);
    grid.push(b"\x1b[5Z");
    assert!(grid.unsupported().is_some());

    grid.push(b"\x1b[2J"); // CSI 2 J: erase whole screen
    assert_eq!(
        grid.unsupported(),
        None,
        "a full repaint must clear the latch in the production build too"
    );
    assert_eq!(
        grid.unsupported_count(),
        1,
        "the lifetime count must survive the clear"
    );
}
