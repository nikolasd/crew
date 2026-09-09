#!/usr/bin/env bun
/**
 * Fails when a tracked file references a tracker or register that does not
 * live in this repository.
 *
 * A comment exists to tell the next reader why the code is the way it is. An
 * identifier of the form `CREW-<n>`, `D<n>` or `R<n>` cannot do that: the board and the
 * review register are outside the repository, so following one lands nowhere
 * and the reasoning it stood in for is simply gone. In-repo pointers are fine
 * and are the intended replacement -- an ADR, a doc, or a release record,
 * cited by path.
 *
 * Runs first in `bun run check`: it is the cheapest step in the gate and the
 * only one whose failure needs no compilation to understand.
 *
 * ## On the patterns
 *
 * Every one was measured against the repository rather than reasoned about,
 * and two of them are narrower or wider than the obvious guess:
 *
 * - `R` markers allow a single digit. Two of them are real -- one in the
 *   adapter-registry tests, one in the reconnect test -- and a
 *   two-digit-minimum pattern would leave both behind while looking like it
 *   had swept. Allowing one digit costs exactly one collision, in `assets/`,
 *   which is excluded for that reason.
 * - Bare pull-request numbers require a digit immediately after the `#`, so
 *   Rust attributes (`#[derive]`) and shebangs never match. Hex colours do not
 *   match either: `#10a37f` has no word boundary after its leading digits, and
 *   a six-digit colour has none at four.
 *
 * ## On trusting this file
 *
 * A scanner that matches nothing reports success in exactly the same way as a
 * clean tree. That is not hypothetical: while this rule was being designed,
 * four separate checks returned a confident zero from a broken instrument.
 * Two are worth knowing about because they look like nothing is wrong.
 *
 * `git grep -E` with a `\b` in the pattern matches nothing at all -- POSIX
 * extended regular expressions have no word-boundary escape -- so the zero
 * reads exactly like a clean result. And a recursive search that skips hidden
 * directories by default never opens `.github/`, so a marker in a workflow
 * file survives a scan that appears to have covered the repository.
 *
 * Both of those were used to *check this scanner*. The second disagreed with
 * it, and the scanner was right.
 *
 * `check-markers.test.ts` therefore holds a positive control: text every rule
 * must catch. If a pattern is ever edited into something that matches nothing,
 * the control fails before this scan can report a false all-clear.
 */

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";

/** A marker family: what it looks like, and what to say when it is found. */
type Rule = { readonly name: string; readonly pattern: RegExp };

/**
 * Patterns are declared without the global flag and cloned per use --
 * a shared global `RegExp` carries `lastIndex` between calls, which would
 * make a match depend on which file was scanned before it.
 */
export const RULES: readonly Rule[] = [
  { name: "ticket id", pattern: /CREW-[0-9]+/ },
  // The trailing letter is required: lettered labels are real, not typos.
  { name: "decision label", pattern: /\bD[0-9]{1,2}[a-z]?\b/ },
  { name: "review-register marker", pattern: /\bR[0-9]{1,3}\b/ },
  // A sub-finding of a review-register entry. The shortest shape here, and so
  // the most likely to collide with ordinary text one day -- measured at zero
  // collisions when added, but unlike the review marker there is no directory
  // to exclude if that changes, because a collision would be in code.
  { name: "review sub-finding", pattern: /\bW[0-9]\b/ },
  // Work-package labels of a planning document that is not in this repository.
  { name: "work-package label", pattern: /\bWP-?[0-9A-C]+\b/ },
  // A markdown heading anchor is not a pull-request reference. The lookahead
  // is what tells them apart: an anchor carries a slug after its number
  // (an anchor slug follows the digits), a reference does not. Keying on the
  // token's own shape rather than on the surrounding link syntax means a bare
  // anchor in prose is excluded too. Measured identical, repo-wide, to a
  // lookbehind pair excluding `](` and `.md`; this form is the shorter one.
  { name: "bare pull-request number", pattern: /#[0-9]{1,4}(?![-\w])/ },
  // A section of a design document that is not in this repository. The bare
  // section mark is NOT the rule: `§[0-9]` alone matches about a hundred
  // legitimate in-repo cross-references -- a walkthrough citing its own
  // sections, a checklist citing `docs/manual-testing.md` §8 -- and a guard
  // that flags a hundred correct references gets weakened, which is how
  // dangling citations survived this long. The word before it is what makes
  // it dangling, so the word is in the pattern.
  //
  // `[\s\\]*` rather than `\s*` because the real occurrences were split by a
  // Rust string continuation: a space, a BACKSLASH, a newline, indentation.
  // A pattern without the backslash misses exactly the sites a line-oriented
  // hand count already missed.
  { name: "external spec citation", pattern: /spec[\s\\]*§/ },
];

/**
 * Directories never scanned.
 *
 * `fixtures` is the one exception the rule itself grants: those files are
 * byte-exact recordings of real terminal sessions, and two of them contain an
 * identifier because the prompt that was typed at the time contained one.
 * Editing a recording to satisfy a text rule would destroy the only property
 * that makes it evidence.
 *
 * `assets` holds brand files. `BRAND.md` labels the logo's legs with the same
 * letter-and-digit shape this rule matches, which is the sole legitimate
 * collision in the repository.
 */
export const SKIP_DIRS: readonly string[] = [".git", "node_modules", "target", "dist", "fixtures", "assets", ".claude"];

/**
 * Files exempt by name.
 *
 * The guard's own test must contain literal markers -- that is what proves the
 * matcher fires rather than silently matching nothing. It is the one file whose
 * content this scan cannot take at face value.
 */
export const SKIP_FILES: readonly string[] = ["scripts/check-markers.test.ts"];

/**
 * Directories whose files are recorded artifacts rather than authored text.
 *
 * The same reasoning that exempts `fixtures`: `release/live-conformance`'s
 * JSON reports are harness output copied verbatim, and that directory's own
 * README says so -- "no fields altered, provenance preserved". A label inside
 * a report's `detail` field is part of what the harness emitted. Editing it to
 * satisfy a text rule would make the README's claim false, which is the one
 * property that makes the report evidence. The README itself is authored prose
 * and is not exempt.
 */
export const SKIP_GLOB_DIRS: readonly string[] = ["release/live-conformance"];

/** Extensions worth scanning. Everything else is data or binary. */
const SCAN_EXTENSIONS: readonly string[] = [".rs", ".ts", ".tsx", ".js", ".mjs", ".md", ".yml", ".yaml", ".toml", ".json", ".sh"];

export type Finding = {
  readonly file: string;
  readonly line: number;
  readonly token: string;
  readonly rule: string;
};

/**
 * Every marker in `text`, with 1-based line numbers.
 *
 * Scans the **whole file at once**, not line by line, and derives each line
 * number from the match offset. That is not a refactor: a marker can be split
 * across source lines, and a line-by-line scanner cannot see one that is. The
 * case that proved it was a Rust string continuation --
 *
 *     "... retired in crew v2 (spec \\
 *      §4.6) -- the headless control plane ..."
 *
 * -- where a hand count using a line-oriented grep found 12 sites and the
 * real number was 18. The characters between `spec` and the section mark are
 * a space, a backslash, a newline and indentation, so a pattern that means to
 * catch this has to tolerate all four; see the section-citation rule above.
 *
 * The residual limitation, stated rather than left to be discovered: a marker
 * split *mid-token* (`CREW-` ending one line, `79` starting the next) is
 * still invisible, and deliberately so. Catching it would mean allowing
 * whitespace inside every token, which would match far more than it caught.
 */
export function scanText(text: string, file: string): Finding[] {
  const findings: Finding[] = [];
  // Line starts, for turning a match offset into a line number without
  // re-scanning the text once per match.
  const lineStarts: number[] = [0];
  for (let i = 0; i < text.length; i += 1) {
    if (text[i] === "\n") lineStarts.push(i + 1);
  }
  const lineOf = (offset: number): number => {
    let lo = 0;
    let hi = lineStarts.length - 1;
    while (lo < hi) {
      const mid = Math.ceil((lo + hi) / 2);
      if ((lineStarts[mid] ?? 0) <= offset) lo = mid;
      else hi = mid - 1;
    }
    return lo + 1;
  };

  for (const rule of RULES) {
    // A fresh global clone per rule: see RULES' own comment on lastIndex.
    const scanner = new RegExp(rule.pattern.source, "g");
    for (const match of text.matchAll(scanner)) {
      findings.push({
        file,
        line: lineOf(match.index ?? 0),
        token: match[0].replace(/\s+/g, " "),
        rule: rule.name,
      });
    }
  }
  return findings;
}

function* walk(dir: string, root: string): Generator<string> {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.isDirectory()) {
      if (SKIP_DIRS.includes(entry.name)) continue;
      yield* walk(join(dir, entry.name), root);
      continue;
    }
    if (!entry.isFile()) continue;
    const path = join(dir, entry.name);
    if (!SCAN_EXTENSIONS.some((ext) => entry.name.endsWith(ext))) continue;
    // A file large enough to be data rather than source is not worth reading
    // into memory, and nothing that size carries a comment worth checking.
    if (statSync(path).size > 2 * 1024 * 1024) continue;
    const rel = relative(root, path);
    if (SKIP_FILES.includes(rel)) continue;
    // Recorded artifacts are exempt, but only their data: a README beside them
    // is authored text and is scanned like anything else.
    if (rel.endsWith(".json") && SKIP_GLOB_DIRS.some((d) => rel.startsWith(`${d}/`))) continue;
    yield rel;
  }
}

export function scanRepo(root: string): Finding[] {
  const findings: Finding[] = [];
  for (const file of walk(root, root)) {
    findings.push(...scanText(readFileSync(join(root, file), "utf8"), file));
  }
  return findings;
}

function report(findings: Finding[]): string {
  const lines = findings.map(
    (f) => `${f.file}:${f.line}: found "${f.token}"\n` + `  A comment must not point at a tracker or register outside this\n` + `  repository -- a reader cannot follow it. Replace it with the reasoning\n` + `  it stands for. In-repo pointers are fine: an ADR, a doc, or a release\n` + `  record, cited by path.`,
  );
  const files = new Set(findings.map((f) => f.file)).size;
  return `${lines.join("\n\n")}\n\n${findings.length} marker${findings.length === 1 ? "" : "s"} found in ${files} file${files === 1 ? "" : "s"}. See CONTRIBUTING.md.`;
}

if (import.meta.main) {
  const root = process.cwd();
  const findings = scanRepo(root);
  if (findings.length > 0) {
    console.error(report(findings));
    process.exit(1);
  }
  console.log("check-markers: no out-of-repo tracker references found.");
}
