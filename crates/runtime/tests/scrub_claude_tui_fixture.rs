// Intentionally empty.
//
// This was a one-time migration script used to scrub the raw recorded
// Claude session capture into `fixtures/adapters/claude-tui/session.jsonl`.
// It must not run again: re-running it overwrites that fixture with the
// scrubber's own output only, undoing the further manual redaction pass
// documented in `fixtures/adapters/claude-tui/README.md` (which strips
// environment-specific `attachment` payload bulk the scrubber itself
// does not touch). Left in place, empty, only because this session's
// sandbox permissions would not allow deleting the file outright --
// whoever picks this up should run:
//
//   rm crates/runtime/tests/scrub_claude_tui_fixture.rs
