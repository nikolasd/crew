import { describe, expect, test } from "bun:test";
import { RULES } from "./check-markers";
import { markersIn, violationsIn } from "./check-trailers";

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

/**
 * The marker rules, applied to the other surface.
 *
 * The maintainer's ruling is "no ids anywhere, commits included", so the
 * identifiers a file may not carry are identifiers a commit message may not
 * carry either. The rules themselves are imported from `check-markers` rather
 * than restated, which is what stops the two surfaces from drifting into
 * enforcing different things under one name -- and the last test in this block
 * is what makes that guarantee checkable rather than merely intended.
 */
describe("positive control: every marker rule rejects its own commit message", () => {
  const mustReject: ReadonlyArray<readonly [string, string]> = [
    ["ticket id", "fix: close the readiness gate\n\nPart of CREW-79, the escalation work."],
    ["decision label", "refactor: fold the third channel in\n\nPer D28 the channel is redundant."],
    ["review-register marker", "fix: the fabricated disproof\n\nCloses R52."],
    ["review sub-finding", "test: guard the finding\n\nCovers W2."],
    ["work-package label", "chore: retire the shim\n\nRetired by gap-closure WP-C."],
    ["bare pull-request number", "fix: follow up the guard\n\nShipped since #88."],
    ["external spec citation", "feat: drop the headless plane\n\nRetired in crew v2 (spec §4.6)."],
  ];

  for (const [rule, message] of mustReject) {
    test(`${rule} is rejected in a commit message`, () => {
      expect(markersIn("abc1234", message).map((v) => v.rule)).toContain(rule);
    });
  }

  test("every marker rule is exercised by a control above", () => {
    // The same guard `check-markers.test.ts` carries: a rule added to the
    // shared list without a control here would silently apply to commit
    // messages with nothing proving it can fire on one.
    expect(new Set(mustReject.map(([rule]) => rule))).toEqual(new Set(RULES.map((r) => r.name)));
  });

  test("a marker in the subject line is caught, not just in the body", () => {
    // The subject is the half a squash merge keeps, so it is the half most
    // likely to outlive the branch.
    expect(markersIn("abc1234", "fix: the thing CREW-79 asked for").map((v) => v.rule)).toEqual(["ticket id"]);
  });

  test("a violation reports the line the marker sits on", () => {
    const found = markersIn("deadbeef1234", "fix: a thing\n\nCloses R52 after review.");
    expect(found[0]?.commit).toBe("deadbeef1234");
    expect(found[0]?.line).toBe("Closes R52 after review.");
  });
});

describe("what a clean commit message looks like", () => {
  test("reasoning, not pointers, passes both halves", () => {
    // Deliberately the shape CONTRIBUTING.md asks for: name the defect and
    // the mechanism, cite an in-repo path, point at nothing outside.
    const message = ["Scan what git tracks, not what is on disk", "", "The guard walked the filesystem, so it read gitignored generated output", "and failed on a clean checkout. `git ls-files` is the repository's own", "definition of its content. See scripts/check-markers.ts."].join("\n");
    expect(violationsIn("abc1234", message)).toEqual([]);
    expect(markersIn("abc1234", message)).toEqual([]);
  });

  test("a merge subject naming a branch is not a marker", () => {
    // Git Town merges main into a branch routinely; that subject must pass.
    expect(markersIn("abc1234", "Merge remote-tracking branch 'origin/main' into guard-tracked-only")).toEqual([]);
  });
});
