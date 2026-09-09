#!/usr/bin/env bun
/**
 * Fails when a pull request's own commit messages carry attribution trailers.
 *
 * Commits in this repository are technical content only: no tool attribution,
 * no agent names, no session links. A trailer of that kind says nothing about
 * the change and outlives every context in which it meant anything.
 *
 * ## Why this is separate from the marker guard
 *
 * `check-markers.ts` scans *files*. A commit message is not a file, so no
 * amount of scanning the tree can see one, and the two checks cannot be
 * merged.
 *
 * The scope difference matters more. A file can be edited, so the marker guard
 * runs over the whole tree and demands zero. **History cannot be edited**, and
 * commits already on `main` carry trailers that predate this rule -- rewriting
 * them would be a far worse act than the trailers are a problem. So this check
 * looks only at the commits a pull request is *adding*, and never at history.
 *
 * Three surfaces, three rules:
 *
 * | what          | checked by         | scope                  |
 * |---------------|--------------------|------------------------|
 * | files         | `check-markers.ts` | whole tree, must be 0  |
 * | commit messages | this script      | the PR's own commits   |
 * | history       | nothing            | immutable, out of scope|
 *
 * ## Usage
 *
 *     bun scripts/check-trailers.ts <base-sha>..HEAD
 *
 * In CI the range is the pull request's own base, so a rebase or a
 * force-push cannot smuggle a commit past it: whatever the PR proposes to add
 * to `main` is exactly what gets read.
 */

import { execFileSync } from "node:child_process";

/**
 * The one trailer that is allowed.
 *
 * The dist-refresh bot commits the rebuilt extension bundle on the author's
 * behalf, and its `Co-authored-by:` line is the honest record of that: a real
 * account made a real commit. Everything else in the attribution family --
 * session links, tool names, agent identities -- says nothing about the
 * change and is refused.
 */
const ALLOWED_COAUTHOR = "crew-bot[bot]";

export type TrailerViolation = { readonly commit: string; readonly line: string; readonly rule: string };

/**
 * Attribution trailers in one commit message.
 *
 * Matching is anchored to the start of a line and case-insensitive, because a
 * trailer is a line-leading key. A message *quoting* a trailer mid-sentence
 * (this file's own doc comment does exactly that) is not a trailer and is not
 * flagged.
 */
export function violationsIn(commit: string, message: string): TrailerViolation[] {
  const found: TrailerViolation[] = [];
  for (const raw of message.split("\n")) {
    const line = raw.trim();
    if (/^claude-session\s*:/i.test(line)) {
      found.push({ commit, line, rule: "session link" });
      continue;
    }
    if (/^co-authored-by\s*:/i.test(line) && !line.includes(ALLOWED_COAUTHOR)) {
      found.push({ commit, line, rule: "co-author attribution" });
      continue;
    }
    if (/^(generated with|authored by|assisted by)\s*:/i.test(line)) {
      found.push({ commit, line, rule: "tool attribution" });
    }
  }
  return found;
}

function commitsIn(range: string): { sha: string; message: string }[] {
  // `%x00` separates records and `%x01` separates fields: a commit message can
  // contain any printable text, including whatever delimiter looked safe.
  const raw = execFileSync("git", ["log", "--format=%H%x01%B%x00", range], { encoding: "utf8" });
  return raw
    .split("\0")
    .map((r) => r.trim())
    .filter((r) => r.length > 0)
    .map((record) => {
      const [sha = "", message = ""] = record.split("\x01");
      return { sha, message };
    });
}

if (import.meta.main) {
  const range = process.argv[2];
  if (range === undefined) {
    console.error("usage: bun scripts/check-trailers.ts <base-sha>..HEAD");
    process.exit(2);
  }
  const commits = commitsIn(range);
  const violations = commits.flatMap((c) => violationsIn(c.sha, c.message));
  if (violations.length > 0) {
    for (const v of violations) {
      console.error(
        `${v.commit.slice(0, 12)}: ${v.rule}\n` +
          `  ${v.line}\n` +
          `  Commit messages carry technical content only -- no tool attribution,\n` +
          `  agent names or session links. Amend the message and force-push the\n` +
          `  branch; this checks only the commits this pull request adds, never\n` +
          `  history. See CONTRIBUTING.md.`,
      );
    }
    console.error(`\n${violations.length} attribution trailer(s) in ${commits.length} commit(s).`);
    process.exit(1);
  }
  console.log(`check-trailers: ${commits.length} commit(s) clean.`);
}
