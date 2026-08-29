// Tests for the shared `launchProgramHint`/`displayPreferenceFragment`
// helpers (CREW-9): mapping `$TERM_PROGRAM` to the closed set of wire
// values `run/submit`/`run/retry`'s `displayPreference.launchProgram`
// accepts, and never a raw string -- see shared.ts's own doc comments for
// why.

import { expect, test } from "bun:test";
import { displayPreferenceFragment, launchProgramHint } from "./shared";

test("launchProgramHint maps Apple_Terminal to appleTerminal", () => {
  expect(launchProgramHint({ TERM_PROGRAM: "Apple_Terminal" })).toBe("appleTerminal");
});

test("launchProgramHint maps iTerm.app to iTerm2", () => {
  expect(launchProgramHint({ TERM_PROGRAM: "iTerm.app" })).toBe("iTerm2");
});

test("launchProgramHint maps ghostty to ghostty", () => {
  expect(launchProgramHint({ TERM_PROGRAM: "ghostty" })).toBe("ghostty");
});

test("launchProgramHint returns undefined for an absent TERM_PROGRAM", () => {
  expect(launchProgramHint({})).toBeUndefined();
});

test("launchProgramHint returns undefined for a multiplexer, not a raw passthrough", () => {
  // Inside tmux, $TERM_PROGRAM is commonly "tmux" -- this must not be
  // treated as (or passed through as) a terminal-emulator hint. Harmless
  // either way since tmux wins backend resolution first, but the mapping
  // itself must not pretend "tmux" is a recognized terminal.
  expect(launchProgramHint({ TERM_PROGRAM: "tmux" })).toBeUndefined();
});

test("launchProgramHint returns undefined for an unrecognized value", () => {
  expect(launchProgramHint({ TERM_PROGRAM: "some-future-terminal" })).toBeUndefined();
});

test("displayPreferenceFragment is empty when there is no recognized hint", () => {
  expect(displayPreferenceFragment({})).toEqual({});
});

test("displayPreferenceFragment carries the hint alongside the daemon's own no-preference defaults", () => {
  expect(displayPreferenceFragment({ TERM_PROGRAM: "iTerm.app" })).toEqual({
    displayPreference: { ordered: [], placement: "embedded", launchProgram: "iTerm2" },
  });
});
