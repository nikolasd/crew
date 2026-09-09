#!/usr/bin/env bun
/**
 * Fails when a pull request's own commit messages carry attribution trailers
 * or point at a tracker outside this repository.
 *
 * Commits here are technical content only: no tool attribution, no agent
 * names, no session links, and -- since the rule was extended to cover commit
 * messages -- no ticket ids, decision labels or bare pull-request numbers
 * either. A trailer of the first kind says nothing about the change and
 * outlives every context in which it meant anything; an identifier of the
 * second kind sends a reader somewhere they cannot go.
 *
 * ## Why this is separate from the marker guard
 *
 * `check-markers.ts` scans *files*. A commit message is not a file, so no
 * amount of scanning the tree can see one, and the two checks cannot be
 * merged. The marker *rules* are shared rather than restated: this imports
 * `RULES` and `scanText` from that module, so the two surfaces can never
 * drift into enforcing different things under one name.
 *
 * The scope difference matters more. A file can be edited, so the marker guard
 * runs over every tracked file and demands zero. **History cannot be edited**,
 * and commits already on `main` carry both trailers and identifiers that
 * predate these rules -- rewriting them would be a far worse act than they are
 * a problem. So this check looks only at the commits a pull request is
 * *adding*, and never at history.
 *
 * Three surfaces, three rules:
 *
 * | what            | checked by                    | scope                   |
 * |-----------------|-------------------------------|-------------------------|
 * | files           | `check-markers.ts`            | every tracked file, 0   |
 * | commit messages | this script (trailers + markers) | the PR's own commits |
 * | history         | nothing                       | immutable, out of scope |
 *
 * ## Usage
 *
 *     bun scripts/check-trailers.ts <base-sha>..HEAD [<already-landed-ref>]
 *
 * ## What "the PR's own commits" has to mean, and why the range is not enough
 *
 * `<base-sha>..HEAD` alone is wrong, and the marker rules are what exposed it.
 * The repository's Git Town workflow merges `main` *into* a branch rather than
 * rebasing it (a pushed branch must not be rebased), so as soon as `main`
 * advances and the branch syncs, every commit `main` gained since the base is
 * inside `<base-sha>..HEAD`. Those are squash-merge commits, and GitHub appends
 * `(#NNN)` to each of their subjects.
 *
 * Measured, not predicted: the range `7a339c2..b7d9215` -- exactly the one this
 * check ran on for the pull request that made the vendor-CLI kill switch
 * structural -- contains the squash-merge subject of the marker-guard change
 * that landed before it, GitHub's appended reference and all. Applying the
 * marker rules to that range would have failed a pull request for the subject
 * of a commit already on `main`, written by nobody on that branch, and
 * amendable by no one.
 *
 * So the commits examined are those in the range that are **not already
 * reachable from the landed branch** (`origin/main` by default, overridable as
 * the second argument). That is the honest definition of "what this pull
 * request adds", it still cannot be slipped past -- anything not on `main` is
 * read -- and it makes the `(#NNN)` GitHub appends at merge a non-issue by
 * construction rather than by an exception carved into the patterns.
 */

import { execFileSync } from "node:child_process";
import { RULES, scanText } from "./check-markers";

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

/**
 * Out-of-repo identifiers in one commit message.
 *
 * Delegates to the marker guard's own `scanText`, over the whole message at
 * once rather than line by line -- a marker split across a subject and a body
 * line is still a marker, and the same reasoning that made the file scan
 * whole-file applies here.
 */
export function markersIn(commit: string, message: string): TrailerViolation[] {
  return scanText(message, commit).map((f) => ({
    commit,
    // The matched token, with the line it sits on for context. A commit
    // message has no path to print, so the token has to carry its own.
    line: (message.split("\n")[f.line - 1] ?? "").trim(),
    rule: f.rule,
  }));
}

/**
 * Brings the exclusion ref up to date, and refuses if it cannot.
 *
 * The exclusion is only as good as the ref. A stale `origin/main` leaves
 * every commit the target branch has gained since the last fetch inside
 * `<base>..HEAD`, where they read as the pull request's own -- and since
 * those are squash-merge commits carrying an appended reference, they read
 * specifically as bare pull-request numbers the author never wrote.
 *
 * That is not hypothetical. On the day this check landed it reported six
 * such findings against a contributor's branch, all of them commits already
 * on the target branch, and the report was convincing enough that the
 * contributor rewrote history to satisfy it. Nothing was wrong with their
 * commits. A guard that produces a confident, specific, wrong list of
 * violations is worse than one that produces none, because someone acts on
 * it -- and the person most likely to act on it is the one who trusts the
 * tooling.
 *
 * So this fetches rather than merely detecting: a check that can repair the
 * condition it would otherwise report is obliged to. Fetching also removes
 * the failure mode a bare comparison would introduce, where the target
 * branch advancing mid-run makes a correct checkout look stale.
 *
 * A ref that is not a remote-tracking branch (a raw sha, a local branch) is
 * the caller being deliberate and is left alone. A fetch that fails --
 * offline, no such remote, no credentials -- is reported loudly and the scan
 * continues on what is already there: refusing outright would make the check
 * unusable without a network, and the exclusion may well still be current.
 * What it must never do is fail silently, which is the whole subject of this
 * file.
 */
function refreshExclusionRef(landed: string, cwd?: string): void {
  const remoteTracking = /^([A-Za-z0-9._-]+)\/(.+)$/.exec(landed);
  if (remoteTracking === null) return;
  const [, remote, branch] = remoteTracking;
  try {
    execFileSync("git", ["fetch", "--quiet", remote, branch], { stdio: "pipe", cwd });
  } catch {
    console.error(
      `check-trailers: WARNING -- could not fetch ${remote}/${branch}, so "${landed}" may be behind.\n` + "  If it is, commits already on the target branch will be read as this branch's own and\n" + "  reported as violations their author never wrote. Re-run after `git fetch` before acting\n" + "  on anything below.",
    );
  }
}

/**
 * The commits a pull request actually adds.
 *
 * `range` is the pull request's own `<base>..HEAD`. `landed` is a ref whose
 * history is already on the target branch -- anything reachable from it is
 * excluded, because it is not this pull request's to answer for. See this
 * file's header for the measured case that makes the exclusion necessary
 * rather than defensive.
 */
export function commitsAdded(range: string, landed: string, cwd?: string): { sha: string; message: string }[] {
  // A missing exclusion ref must be an error, never a silent widening: without
  // it every commit `main` gained since the base would be read as this pull
  // request's own, and the check would fail on subjects nobody on the branch
  // wrote.
  try {
    execFileSync("git", ["rev-parse", "--verify", "--quiet", `${landed}^{commit}`], { stdio: "pipe", cwd });
  } catch {
    console.error(`check-trailers: cannot resolve "${landed}", so the commits already on the target branch\n` + "  cannot be excluded and this check would read commits the pull request did not add.\n" + "  Fetch the branch (CI uses fetch-depth: 0) or pass a different ref as the second argument.");
    process.exit(2);
  }

  refreshExclusionRef(landed, cwd);

  // `%x00` separates records and `%x01` separates fields: a commit message can
  // contain any printable text, including whatever delimiter looked safe.
  const raw = execFileSync("git", ["log", "--format=%H%x01%B%x00", range, "--not", landed], {
    encoding: "utf8",
    cwd,
  });
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
  const landed = process.argv[3] ?? "origin/main";
  if (range === undefined) {
    console.error("usage: bun scripts/check-trailers.ts <base-sha>..HEAD [<already-landed-ref>]");
    process.exit(2);
  }
  const commits = commitsAdded(range, landed);
  const violations = commits.flatMap((c) => [...violationsIn(c.sha, c.message), ...markersIn(c.sha, c.message)]);
  if (violations.length > 0) {
    for (const v of violations) {
      console.error(
        `${v.commit.slice(0, 12)}: ${v.rule}\n` +
          `  ${v.line}\n` +
          `  A commit message carries technical content only -- no tool attribution,\n` +
          `  agent names or session links, and no identifier that points outside this\n` +
          `  repository. Write the reasoning instead. Amend the message and force-push\n` +
          `  the branch; this reads only the commits this pull request adds, never\n` +
          `  history. See CONTRIBUTING.md.`,
      );
    }
    console.error(`\n${violations.length} violation(s) in ${commits.length} commit(s).`);
    process.exit(1);
  }
  // The count is in the success line for the same reason `check-markers`
  // prints its file count: zero commits examined and zero violations found
  // are different results that would otherwise read identically.
  console.log(`check-trailers: ${commits.length} commit(s) clean (excluding what is already on ${landed}).`);
}
