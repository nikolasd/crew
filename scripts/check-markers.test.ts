import { describe, expect, test } from "bun:test";
import { RULES, SKIP_DIRS, scanText } from "./check-markers";

/**
 * The positive control.
 *
 * A scanner that matches nothing reports a clean tree in exactly the same way
 * as a clean tree. Three checks during this rule's design returned a confident
 * zero from a broken instrument -- the sharpest being `git grep -E '\bD[0-9]'`,
 * where POSIX extended regular expressions have no `\b`, so the pattern matched
 * nothing and the zero read as a pass.
 *
 * Every rule below is therefore pinned against text it MUST catch. If a pattern
 * is ever edited into something that matches nothing, these fail before the
 * repository scan can report a false all-clear.
 */
describe("positive control: every rule catches its own marker", () => {
  const mustCatch: ReadonlyArray<readonly [string, string, string]> = [
    ["ticket id", "// CREW-79: the readiness gate", "CREW-79"],
    ["decision label", "// D28: the third channel", "D28"],
    ["review-register marker", "// R52: a fabricated disproof", "R52"],
    ["review sub-finding", "// GREEN guard for the W2 finding", "W2"],
    ["work-package label", "// retired by gap-closure WP-C", "WP-C"],
    ["bare pull-request number", "// shipped since #88", "#88"],
  ];

  for (const [rule, text, token] of mustCatch) {
    test(`${rule} is caught in ${JSON.stringify(text)}`, () => {
      const found = scanText(text, "probe.rs");
      expect(found.map((f) => f.token)).toContain(token);
      expect(found.find((f) => f.token === token)?.rule).toBe(rule);
    });
  }

  test("every declared rule is exercised by a control above", () => {
    // Guards the guard's guard: a rule added without a control would
    // otherwise ship unproven.
    expect(new Set(mustCatch.map(([rule]) => rule))).toEqual(new Set(RULES.map((r) => r.name)));
  });
});

describe("the two pattern decisions that were measured, not guessed", () => {
  test("single-digit review markers are caught -- R2 and R6 are real", () => {
    // A `{2,3}` pattern would leave these behind while appearing to sweep.
    expect(scanText("// Defends the R2 fix at the registry level", "a.rs").map((f) => f.token)).toContain("R2");
    expect(scanText("// reconnection (TODO / R6).", "a.ts").map((f) => f.token)).toContain("R6");
  });

  test("a lettered decision label is caught -- D29a is a real label", () => {
    expect(scanText("// D29a's rule: resolve, but never silently", "a.ts").map((f) => f.token)).toContain("D29a");
  });

  test("work-package labels are caught in both spellings", () => {
    expect(scanText("// gap-closure WP29 ruling", "a.rs").map((f) => f.token)).toContain("WP29");
    expect(scanText("// retired by WP-C, spec 4.6", "a.rs").map((f) => f.token)).toContain("WP-C");
  });

  test("a work-package label is not also read as a sub-finding", () => {
    // `WP29` must not match the W rule: the `P` breaks the word boundary.
    // If it ever did, every work-package label would be reported twice under
    // two different rules, and the count would silently double.
    const rules = scanText("// gap-closure WP29", "a.rs").map((f) => f.rule);
    expect(rules).not.toContain("review sub-finding");
  });
});

describe("negative controls: things that must never be flagged", () => {
  test("hex colours are not pull-request numbers", () => {
    const text = 'fill="#0d0d0d" and `#f74fcc` and `#161826` and `#10a37f`';
    expect(scanText(text, "brand.md")).toEqual([]);
  });

  test("rust attributes and shebangs are not pull-request numbers", () => {
    const text = "#[derive(Debug)]\n#![allow(dead_code)]\n#!/usr/bin/env bun";
    expect(scanText(text, "a.rs").filter((f) => f.rule === "bare pull-request number")).toEqual([]);
  });

  test("ordinary prose containing the letters is not flagged", () => {
    const text = "The D register and the R channel both settle. Add 3D rendering.";
    expect(scanText(text, "a.md")).toEqual([]);
  });

  test("markdown heading anchors are not pull-request references", () => {
    // Every remaining `#N` in docs was one of these. An anchor carries a slug
    // after its number; a pull-request reference does not.
    const anchors = "See [Worker adapters](#4-worker-adapters) and [manual-testing.md](manual-testing.md#8-the-dashboard).";
    expect(scanText(anchors, "docs.md").filter((f) => f.rule === "bare pull-request number")).toEqual([]);
  });

  test("a real pull-request reference beside an anchor is still caught", () => {
    // The other half of the pair: excluding anchors must not exclude the thing
    // the rule exists for.
    const mixed = "Landed in #134; see [the section](#6-when-something-breaks).";
    expect(scanText(mixed, "docs.md").map((f) => f.token)).toEqual(["#134"]);
  });

  test("a longer identifier is not a marker", () => {
    // `RUNTIME1` and `DATA12` share a prefix shape but are not markers.
    expect(scanText("const RUNTIME1 = DATA12;", "a.ts")).toEqual([]);
  });
});

describe("scanning mechanics", () => {
  test("the same text scans identically twice -- no shared lastIndex", () => {
    // A module-level global RegExp carries `lastIndex` between calls, which
    // would make a file's result depend on what was scanned before it.
    const text = "// CREW-1 and CREW-2 on one line";
    expect(scanText(text, "a.rs")).toEqual(scanText(text, "a.rs"));
    expect(scanText(text, "a.rs")).toHaveLength(2);
  });

  test("line numbers are 1-based and point at the marker", () => {
    const found = scanText("clean\nalso clean\n// CREW-9 here", "a.rs");
    expect(found).toHaveLength(1);
    expect(found[0]?.line).toBe(3);
  });

  test("clean text yields nothing", () => {
    expect(scanText("// The readiness gate fails closed on an unknown surface.", "a.rs")).toEqual([]);
  });
});

describe("the exceptions the rule itself grants", () => {
  test("fixtures are skipped -- they are byte-exact recordings", () => {
    // Two committed captures contain an identifier because the prompt typed
    // at capture time did. Editing a recording to satisfy a text rule would
    // destroy the property that makes it evidence.
    expect(SKIP_DIRS).toContain("fixtures");
  });

  test("assets are skipped -- the logo's legs are labelled R1-R4", () => {
    // The sole legitimate collision in the repository. Excluding the
    // directory is what lets the R pattern stay wide enough to catch R2/R6.
    expect(SKIP_DIRS).toContain("assets");
  });
});
