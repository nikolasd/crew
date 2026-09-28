#!/usr/bin/env bun
/**
 * Fails when the ADR index in `docs/adr/README.md` and an ADR's own
 * `Status:` line disagree about where that decision stands.
 *
 * The index is the table a reader scans to learn which decisions are live. The
 * record is the authority on its own state. When they drift, the index is what
 * gets read and believed, and a record that still says a decision is
 * `Proposed` sits unread beside a table row that says `Accepted` -- so the
 * defect is invisible in the one direction that matters, and nobody notices
 * until a reader cites a decision that was never actually made.
 *
 * That is not hypothetical. ADR-0038 was listed as `Accepted` in the index
 * while its own header still read `* Status: Proposed`, which is the exact
 * disagreement this script exists to fail on.
 *
 * ## What is compared, and why only that
 *
 * The leading lifecycle word -- `Accepted`, `Superseded`, `Proposed` -- and
 * nothing after it. Both sides carry free-form trailing clauses that
 * legitimately differ: the index writes `Accepted, amended by [0027](...)`
 * where the record writes `* Status: Accepted`, and 0026's index row reads
 * `Accepted, superseded by [0037](...)` where the record reads
 * `Accepted; superseded by [0037](...)`. Demanding the whole cell match would
 * fail on every amended and superseded record, and a check that fails on
 * correct content gets deleted rather than fixed.
 *
 * ## The 0033 gap
 *
 * There is no ADR-0033: the number was reserved and deliberately left unused,
 * which `docs/adr/README.md` says in prose. This check runs from the index
 * outward, so a number appearing in no row is never visited -- the gap is
 * invisible to it, and is not a missing record. Nothing here infers that a
 * record exists from its number, because in this series a number is not
 * evidence of that.
 *
 * ## On trusting this file
 *
 * A check that reads nothing reports success in exactly the same way as a
 * clean repository. `check-adr-status.ts` therefore throws when the index
 * yields no rows at all -- a relocated file, a rewritten table, or a moved
 * `docs/adr/` each produce zero, and each would otherwise print the same
 * all-clear as a healthy index. `check-adr-status.test.ts` pins that.
 */

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

/** Where the index lives, and therefore what this check is about. */
const INDEX = "docs/adr/README.md";

/** A rule: what the failure is called, and what to say about it. */
type Rule = { readonly name: string; readonly guidance: readonly string[] };

const RULES = {
  disagreement: {
    name: "index status disagrees with the record's own status",
    guidance: [
      "  The index row in docs/adr/README.md and the record's own `Status:`",
      "  line must lead with the same lifecycle status. A decision that has",
      "  been made is Accepted, and a record is the authority on its own",
      "  state, so change the `Status:` line and let the index row follow.",
      "  (`Proposed` to `Accepted` is the ordinary step for a decision just",
      "  made, not an exception to the write-once rule; only a status that",
      "  reflects a later reversal of a settled decision is worth weighing.)",
    ],
  },
  "missing record": {
    name: "index row cites a record that does not exist",
    guidance: ["  Every row in docs/adr/README.md must point at a file in docs/adr/.", "  Either the record was never written or the row is wrong. The number", "  0033 is not this case: it was reserved and deliberately left unused,", "  and it has no row."],
  },
  "missing status": {
    name: "record has no Status: line",
    guidance: ["  Every record in docs/adr/ opens with a MADR header carrying a", "  `* Status:` line. Without one there is nothing for the index to", "  agree with, and the decision's state is unrecorded, not merely", "  unlisted."],
  },
  "empty status": {
    name: "index row has an empty status",
    guidance: ["  Every row in docs/adr/README.md must state a lifecycle status.", "  An empty cell is a hole, not a state."],
  },
  "unparsed row": {
    name: "index row could not be parsed",
    guidance: [
      "  A table row that opens like the others but does not parse is",
      "  reported, not skipped. A row this check cannot read is a row it",
      "  cannot verify, and one that stops being read looks exactly like a",
      "  clean index. Keep the shape the other rows have: the ID cell is a",
      "  link, `[NNNN](NNNN-slug.md)`.",
    ],
  },
} as const satisfies Readonly<Record<string, Rule>>;

type RuleName = keyof typeof RULES;

export type Finding = {
  /** The index row as `file:line`: the disagreement is the index's to fix. */
  readonly file: string;
  /** 1-based line of that row in the index. */
  readonly line: number;
  /** Both statuses as they actually read, so the message can be checked by eye. */
  readonly token: string;
  readonly rule: RuleName;
};

/** One row of the index table. */
type Row = {
  /** 1-based line in the index, for reporting. */
  readonly line: number;
  /** The four-digit id, as written. */
  readonly id: string;
  /** The cited record's path relative to the repository root. */
  readonly target: string;
  /** The status cell, exactly as written. */
  readonly status: string;
  /** The ID cell exactly as written, so an unparseable row can quote it. */
  readonly raw: string;
};

/**
 * The leading lifecycle word of a status -- the only part compared here.
 *
 * `null` for text that does not begin with a word, which is how an empty cell
 * is told apart from a written one rather than by a length test that would
 * accept punctuation as a status.
 */
export function statusToken(text: string): string | null {
  const word = /^[A-Za-z][A-Za-z-]*/.exec(text.trim());
  return word === null ? null : word[0].toLowerCase();
}

/** The cells of a markdown table line, or `[]` for a line that is not one. */
function cells(line: string): string[] {
  const trimmed = line.trim();
  if (!trimmed.startsWith("|")) return [];
  return trimmed
    .split("|")
    .slice(1, -1)
    .map((cell) => cell.trim());
}

/**
 * Every row of the index table, with the line each was read from.
 *
 * A line carrying the table's header, its separator, or no bracketed ID is
 * not a row. A row whose ID cell opens a bracket but is not a link still is
 * one -- it comes back with an empty `target` and is reported by `checkRow`,
 * because a row that quietly stops being checked is what this file exists to
 * prevent. A row whose status cell is empty parses the same way: the cell
 * comes back empty and is reported, never dropped.
 */
export function parseIndex(text: string): Row[] {
  const rows: Row[] = [];
  text.split("\n").forEach((line, i) => {
    const parsed = cells(line);
    const idCell = parsed[0] ?? "";
    if (parsed.length < 3 || !idCell.startsWith("[")) return;
    const link = /^\[(\d{4})\]\(([^)]+)\)$/.exec(idCell);
    rows.push({
      line: i + 1,
      id: link?.[1] ?? /^\[?(\d{4})/.exec(idCell)?.[1] ?? "",
      raw: idCell,
      target: link === null ? "" : `docs/adr/${link[2]}`,
      status: parsed[2] ?? "",
    });
  });
  return rows;
}

/**
 * The record's own `Status:` line, or `null` when it has none.
 *
 * MADR puts it in the header, which is the `* Key: value` block under the
 * title, so the search stops at the first heading after that title. Reading
 * the whole file would let a section that quotes a status line -- an
 * explanation of how a decision was amended, say -- supply the state of a
 * record that never declared one.
 */
export function parseRecordStatus(text: string): { readonly line: number; readonly value: string } | null {
  const lines = text.split("\n");
  const firstSection = lines.findIndex((line, i) => i > 0 && /^#{1,6}\s/.test(line));
  for (let i = 1; i < (firstSection === -1 ? lines.length : firstSection); i++) {
    const match = /^\*\s*Status:\s*(.*)$/.exec(lines[i] ?? "");
    if (match !== null) return { line: i + 1, value: (match[1] ?? "").trim() };
  }
  return null;
}

/** The line the message points at, so the reader need not go looking for it. */
const where = (file: string, line: number): string => `${file}:${line}`;

/** Compare one index row against the record it cites. */
function checkRow(root: string, row: Row): Finding[] {
  const at = (token: string, rule: RuleName): Finding[] => [{ file: INDEX, line: row.line, token, rule }];
  if (row.target === "") return at(row.raw, "unparsed row");
  const path = join(root, row.target);
  const cited = row.raw;

  if (!existsSync(path)) return at(cited, "missing record");

  const own = parseRecordStatus(readFileSync(path, "utf8"));
  if (own === null) return at(cited, "missing status");

  const indexed = statusToken(row.status);
  if (indexed === null) return at(`${cited} status ${JSON.stringify(row.status)}`, "empty status");

  if (statusToken(own.value) !== indexed) {
    return at(`index ${JSON.stringify(row.status)} vs record ${where(row.target, own.line)} ${JSON.stringify(own.value)}`, "disagreement");
  }
  return [];
}

/**
 * The whole check: every index row against the record it cites.
 *
 * Throws on an index that yields no rows, for the reason in the header.
 */
export function checkStatus(root: string): { readonly findings: Finding[]; readonly rows: number } {
  const rows = parseIndex(readFileSync(join(root, INDEX), "utf8"));
  if (rows.length === 0) throw new Error(`${INDEX} yielded no index rows to check -- is the table still a table?`);
  return { findings: rows.flatMap((row) => checkRow(root, row)), rows: rows.length };
}

function report(findings: Finding[]): string {
  const blocks = findings.map((f) => [`${f.file}:${f.line}: found "${f.token}"`, `  rule: ${RULES[f.rule].name}`, ...RULES[f.rule].guidance].join("\n"));
  const files = new Set(findings.map((f) => f.file)).size;
  return `${blocks.join("\n\n")}\n\n${findings.length} conflict${findings.length === 1 ? "" : "s"} found in ${files} file${files === 1 ? "" : "s"}. See ${INDEX}.`;
}

if (import.meta.main) {
  const { findings, rows } = checkStatus(process.cwd());
  if (findings.length > 0) {
    console.error(report(findings));
    process.exit(1);
  }
  // The count is in the success line for the reason `check-markers` puts its
  // scan count there: it is the only thing that distinguishes a clean index
  // from a parser that read almost nothing, and it costs one line in a log.
  console.log(`check-adr-status: ${rows} index rows agree with the records they cite; no status conflicts found.`);
}
