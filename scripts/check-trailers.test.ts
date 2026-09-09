import { describe, expect, test } from "bun:test";
import { violationsIn } from "./check-trailers";

/**
 * The positive control.
 *
 * A checker that matches nothing passes every commit in exactly the same way
 * as a clean branch. The same reasoning as `check-markers.test.ts`: each rule
 * is pinned against a message it MUST reject, so a pattern edited into
 * something unsatisfiable fails here rather than quietly approving the next
 * trailer that lands.
 */
describe("positive control: every rule rejects its own trailer", () => {
  const mustReject: ReadonlyArray<readonly [string, string]> = [
    ["session link", "feat: a change\n\nClaude-Session: https://example.invalid/session_abc"],
    ["co-author attribution", "feat: a change\n\nCo-authored-by: Some Assistant <bot@example.invalid>"],
    ["tool attribution", "feat: a change\n\nGenerated with: some tool"],
  ];

  for (const [rule, message] of mustReject) {
    test(`${rule} is rejected`, () => {
      const found = violationsIn("abc1234", message);
      expect(found.map((v) => v.rule)).toContain(rule);
    });
  }

  test("matching is case-insensitive -- a trailer is a trailer in any casing", () => {
    expect(violationsIn("abc1234", "x\n\nclaude-session: y")).toHaveLength(1);
    expect(violationsIn("abc1234", "x\n\nCLAUDE-SESSION: y")).toHaveLength(1);
  });
});

describe("the one allowed trailer", () => {
  test("the dist-refresh bot's co-author line is permitted", () => {
    // A real account made a real commit; that is honest provenance, not
    // attribution noise, and refusing it would break the bundle refresh.
    const message = "chore(dist): refresh bundle\n\nCo-authored-by: crew-bot[bot] <crew-bot@users.noreply.github.com>";
    expect(violationsIn("abc1234", message)).toEqual([]);
  });

  test("but a different co-author is still refused", () => {
    // The allowance is for that one account, not for the trailer key.
    const message = "chore: x\n\nCo-authored-by: Someone Else <someone@example.invalid>";
    expect(violationsIn("abc1234", message).map((v) => v.rule)).toEqual(["co-author attribution"]);
  });
});

describe("negative controls: things that must never be flagged", () => {
  test("an ordinary technical message is clean", () => {
    const message = "fix(runtime): a cleanly exited run that did no work is failed\n\n" + "A zero exit means the process closed, nothing more; only the leader's\n" + "own run/finish call judges success.";
    expect(violationsIn("abc1234", message)).toEqual([]);
  });

  test("a message that mentions a trailer mid-sentence is not a trailer", () => {
    // Trailers are line-leading keys. Prose discussing the rule -- a commit
    // that adds this very check, for instance -- must not trip it.
    const message = "build: refuse a Claude-Session: line in commit messages\n\nThe rule is enforced by scripts/check-trailers.ts.";
    expect(violationsIn("abc1234", message)).toEqual([]);
  });

  test("indentation does not hide a trailer", () => {
    // The line is trimmed before matching, so leading whitespace is not an
    // escape hatch.
    expect(violationsIn("abc1234", "x\n\n    Claude-Session: y")).toHaveLength(1);
  });
});

describe("reporting", () => {
  test("every violation names its commit and the offending line", () => {
    const found = violationsIn("deadbeef1234", "x\n\nClaude-Session: https://example.invalid/s");
    expect(found[0]?.commit).toBe("deadbeef1234");
    expect(found[0]?.line).toBe("Claude-Session: https://example.invalid/s");
  });

  test("a message with two trailers reports both", () => {
    const message = "x\n\nClaude-Session: a\nCo-authored-by: Someone <s@example.invalid>";
    expect(violationsIn("abc1234", message)).toHaveLength(2);
  });
});
