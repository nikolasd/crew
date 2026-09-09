import { describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { RULES, SKIP_DIRS, SKIP_FILES, scanRepo, scanText } from "./check-markers";

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
    ["external spec citation", "// retired in crew v2 (spec §4.6)", "spec §"],
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

describe("the multi-line case a line-oriented scan cannot see", () => {
  test("a citation split by a Rust string continuation is still caught", () => {
    // The real occurrence, verbatim: a hand count using a line-oriented grep
    // reported 12 sites where there were 18, because the characters between
    // `spec` and the section mark are a space, a backslash, a newline and
    // indentation -- and `\\s*` alone does not cover a backslash.
    const source = '"... which is retired in crew v2 (spec \\\n         §4.6) -- the headless ..."';
    const found = scanText(source, "registry.rs");
    expect(found.map((f) => f.rule)).toContain("external spec citation");
  });

  test("the line number reported is where the marker starts", () => {
    const found = scanText("clean\nalso clean\n// spec §4.6 here", "a.rs");
    expect(found).toHaveLength(1);
    expect(found[0]?.line).toBe(3);
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

  test("an in-repo section reference is not an external spec citation", () => {
    // ~100 of these exist and every one is correct: the section mark is never
    // the defect, the document being sectioned is.
    const text = "See `docs/manual-testing.md` §8 and §3 of the walkthrough.";
    expect(scanText(text, "a.md")).toEqual([]);
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

  test("both guards' own tests are exempt -- they must contain literal markers", () => {
    // A positive control has to spell out the thing it catches. These two
    // files pin the same rules against the two surfaces (files, and commit
    // messages), so each contains one instance of every marker family by
    // necessity. Nothing else in the repository may.
    expect(SKIP_FILES).toEqual(["scripts/check-markers.test.ts", "scripts/check-trailers.test.ts"]);
  });

  test("assets are skipped -- the logo's legs are labelled R1-R4", () => {
    // The sole legitimate collision in the repository. Excluding the
    // directory is what lets the R pattern stay wide enough to catch R2/R6.
    expect(SKIP_DIRS).toContain("assets");
  });
});

/**
 * What the scan considers to be the repository.
 *
 * These build a real git repository in a temporary directory rather than
 * mocking the enumeration, because the property under test is precisely the
 * one a mock would assume: that `git ls-files` and the filesystem disagree,
 * and that the scan follows git.
 *
 * The defect these pin was live on main. The scanner walked the working tree,
 * so it read `crates/protocol/bindings/` -- gitignored output that `ts-rs`
 * writes during a Rust test run, carrying markers copied from the doc comments
 * it was generated from. `bun run check` failed at its first step on a clean
 * checkout, reporting 385 markers in files that are not repository content and
 * do not exist in CI. The rule is about what the repository says; a generated
 * artifact does not say anything.
 */
describe("the scan follows git, not the filesystem", () => {
  /** A throwaway git repository. Files are staged, never committed -- `git ls-files` reads the index. */
  const repoWith = (files: Record<string, string>): string => {
    const root = mkdtempSync(join(tmpdir(), "check-markers-"));
    execFileSync("git", ["init", "--quiet"], { cwd: root });
    for (const [rel, contents] of Object.entries(files)) {
      mkdirSync(dirname(join(root, rel)), { recursive: true });
      writeFileSync(join(root, rel), contents);
    }
    execFileSync("git", ["add", "--all"], { cwd: root });
    return root;
  };

  test("a gitignored file carrying a marker is not flagged", () => {
    const root = repoWith({
      ".gitignore": "generated/\n",
      "src/clean.rs": "// The readiness gate fails closed on an unknown surface.\n",
      "generated/bindings.ts": "// CREW-79: copied from the doc comment this was generated from\n",
    });
    // The ignored file is on disk -- this is a test of what the scan reads,
    // not of what exists. A filesystem walk finds it; the scan must not.
    expect(existsSync(join(root, "generated/bindings.ts"))).toBe(true);
    expect(scanRepo(root).findings).toEqual([]);
  });

  test("a tracked file carrying the same marker IS flagged", () => {
    // The positive control for the pair above. Without it, "not flagged"
    // proves nothing: a scan that reads no files at all passes the first
    // test, and that is the exact bug class this whole file guards against.
    const root = repoWith({
      ".gitignore": "generated/\n",
      "src/tracked.rs": "// CREW-79: copied from the doc comment this was generated from\n",
      "generated/bindings.ts": "// CREW-79: the same marker, in ignored output\n",
    });
    const { findings } = scanRepo(root);
    expect(findings.map((f) => f.file)).toEqual(["src/tracked.rs"]);
    expect(findings[0]?.token).toBe("CREW-79");
  });

  test("an untracked file is not flagged even when nothing ignores it", () => {
    // `git ls-files` reads the index, so a file that is merely present --
    // a scratch note, a half-written draft -- is not repository content yet
    // and cannot fail anyone's build.
    const root = repoWith({ "src/clean.rs": "// nothing to see\n" });
    writeFileSync(join(root, "scratch.md"), "// CREW-79 in an unstaged note\n");
    expect(scanRepo(root).findings).toEqual([]);
  });

  test("a skipped directory is skipped at any depth, not only at the root", () => {
    // `packages/extension/dist/index.js` is a tracked, committed bundle,
    // generated from `packages/extension/src/`. A leading-prefix test would
    // exempt only a top-level `dist/` and scan the bundle as authored source,
    // reporting every marker twice -- the second time against a file that
    // cannot be fixed by editing it.
    const root = repoWith({
      "packages/extension/dist/index.js": "// CREW-79: inlined from the source comment\n",
      "packages/extension/src/index.ts": "// clean\n",
    });
    expect(scanRepo(root).findings).toEqual([]);
    expect(scanRepo(root).scanned).toBe(1);
  });

  test("a scan that enumerates nothing throws instead of reporting clean", () => {
    // The enumerator's own control. Zero files is what a missing `git`, a
    // non-repository directory, or an extension filter edited into
    // uselessness all produce -- and every one of them would otherwise print
    // the same all-clear as a clean tree.
    const empty = mkdtempSync(join(tmpdir(), "check-markers-empty-"));
    execFileSync("git", ["init", "--quiet"], { cwd: empty });
    expect(() => scanRepo(empty)).toThrow(/no scannable files/);
  });

  test("the count reported is the number of files actually read", () => {
    // The success line carries this number so a scan that read almost
    // nothing is distinguishable from a clean repository in a CI log.
    const root = repoWith({
      ".gitignore": "generated/\n",
      "src/a.rs": "// clean\n",
      "src/b.ts": "// clean\n",
      "generated/c.ts": "// ignored\n",
      "notes.bin": "not a scanned extension\n",
    });
    // `src/a.rs` and `src/b.ts` only: the ignored file is not repository
    // content, and neither `notes.bin` nor `.gitignore` itself carries an
    // extension this scan reads.
    expect(scanRepo(root).scanned).toBe(2);
  });
});
