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

import { execFileSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import { join } from "node:path";

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

/**
 * Whether any *directory* on this path is one of `SKIP_DIRS`.
 *
 * Matched per path segment, at any depth, rather than as a leading prefix.
 * A prefix test would exempt only a top-level `dist/` and let
 * `packages/extension/dist/index.js` -- the committed bundle, which git does
 * track -- be scanned as though it were authored source. It is generated from
 * `packages/extension/src/`, so a marker there would be reported twice, and
 * the second report would name a file that cannot be fixed by editing it.
 *
 * The last segment is the filename and is excluded: these name directories,
 * and a file is exempted by `SKIP_FILES` instead.
 */
function inSkippedDir(rel: string): boolean {
  const dirs = rel.split("/").slice(0, -1);
  return dirs.some((segment) => SKIP_DIRS.includes(segment));
}

/**
 * The files to scan: **what git tracks**, nothing else.
 *
 * This deliberately does not walk the filesystem. A walk scans whatever
 * happens to be on disk, and what is on disk includes generated output that
 * is gitignored precisely because it is not the repository's content --
 * `crates/protocol/bindings/`, which `ts-rs` writes during
 * `cargo test -p crew-protocol`, carries markers copied from the Rust doc
 * comments it was generated from. Those are stale artifacts of an earlier
 * commit on one developer's machine, absent from CI, and flagging them made
 * `bun run check` fail at its first step on an otherwise clean checkout.
 *
 * `git ls-files` is the exact definition the rule wants -- "content in this
 * repository" -- and it honours `.gitignore` and `.git/info/exclude` by
 * construction rather than by a list this file has to keep in sync. It also
 * reaches dot-directories like `.github/`, which a naive recursive scan of
 * the working tree tends to skip.
 */
export function trackedFiles(root: string): string[] {
  const out = execFileSync("git", ["ls-files", "-z"], {
    cwd: root,
    encoding: "buffer",
    maxBuffer: 64 * 1024 * 1024,
  });
  const files = out
    .toString("utf8")
    .split("\0")
    .filter((rel) => rel.length > 0)
    .filter((rel) => SCAN_EXTENSIONS.some((ext) => rel.endsWith(ext)))
    .filter((rel) => !SKIP_FILES.includes(rel))
    .filter((rel) => !inSkippedDir(rel))
    // Recorded artifacts are exempt, but only their data: a README beside
    // them is authored text and is scanned like anything else.
    .filter((rel) => !(rel.endsWith(".json") && SKIP_GLOB_DIRS.some((d) => rel.startsWith(`${d}/`))))
    .filter((rel) => {
      // A file large enough to be data rather than source is not worth
      // reading into memory, and nothing that size carries a comment.
      try {
        return statSync(join(root, rel)).size <= 2 * 1024 * 1024;
      } catch {
        // Tracked but absent from the working tree (a sparse checkout, a
        // partial clone): nothing to read, nothing to judge.
        return false;
      }
    });

  // The enumerator's own positive control, and the reason it throws rather
  // than returning an empty list. Everything above this line can fail
  // silently: `git` missing from PATH, a checkout that is not a repository,
  // an extension filter edited until it matches nothing. Every one of those
  // produces zero files, zero findings, and the same cheerful all-clear as a
  // genuinely clean repository -- the precise failure this scanner was
  // written to make impossible, reappearing one level out in the code that
  // chooses what to scan. No checkout of this repository has zero scannable
  // tracked files, so zero is a broken instrument, never a result.
  if (files.length === 0) {
    throw new Error("check-markers: `git ls-files` produced no scannable files -- this is a broken scan, not a clean repository. Run from inside a checkout with `git` on PATH.");
  }
  return files;
}

export function scanRepo(root: string): { readonly findings: Finding[]; readonly scanned: number } {
  const files = trackedFiles(root);
  const findings: Finding[] = [];
  for (const file of files) {
    findings.push(...scanText(readFileSync(join(root, file), "utf8"), file));
  }
  return { findings, scanned: files.length };
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
  const { findings, scanned } = scanRepo(root);
  if (findings.length > 0) {
    console.error(report(findings));
    process.exit(1);
  }
  // The count is in the success line on purpose: it is the only thing that
  // distinguishes a clean repository from a scan that read almost nothing,
  // and it costs one number in a CI log to tell them apart at a glance.
  console.log(`check-markers: ${scanned} tracked files scanned, no out-of-repo tracker references found.`);
}
