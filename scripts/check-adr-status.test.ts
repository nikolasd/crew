import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { checkStatus, parseIndex, parseRecordStatus, statusToken } from "./check-adr-status";

/**
 * The controls for this check.
 *
 * A check that reads nothing reports a clean index in exactly the same way as
 * a clean index. Every rule below is therefore pinned against text it must
 * catch and against the near-misses it must leave alone -- the second half
 * matters more here than in a marker scan, because what this check compares is
 * free-form prose on both sides, and a rule that matched too eagerly would
 * fail on every amended and superseded record in the table.
 */

/** An index holding `rows`, given as `[id, filename, status]` triples. */
const indexWith = (rows: ReadonlyArray<readonly [string, string, string]>, preamble = ""): string => ["# Architecture Decision Records", "", preamble, "| ID | Title | Status |", "|---|---|---|", ...rows.map(([id, file, status]) => `| [${id}](${file}) | Some title | ${status} |`), ""].join("\n");

/** A throwaway repository holding an index and the records it cites. */
const repoWith = (rows: ReadonlyArray<readonly [string, string, string]>, bodies: Readonly<Record<string, string>>): string => {
  const root = mkdtempSync(join(tmpdir(), "check-adr-status-"));
  mkdirSync(join(root, "docs/adr"), { recursive: true });
  writeFileSync(join(root, "docs/adr/README.md"), indexWith(rows));
  for (const [name, body] of Object.entries(bodies)) writeFileSync(join(root, "docs/adr", name), body);
  return root;
};

/** A record whose header carries `status`, or no status line at all. */
const record = (status: string | null): string => ["# A decision", "", ...(status === null ? [] : [`* Status: ${status}`, "* Date: 2026-09-22"]), "", "## Context", "", "The reasoning the status line is attached to.", ""].join("\n");

describe("positive control: every disagreement is caught", () => {
  test("an index that says Accepted beside a record that says Proposed fails", () => {
    // The live defect this check was written for. Each side was correct in
    // isolation and only the pair was wrong, so nothing that reads one side
    // alone would ever have reported it.
    const root = repoWith([["0038", "0038-one-turn.md", "Accepted"]], { "0038-one-turn.md": record("Proposed") });
    const { findings } = checkStatus(root);
    expect(findings).toHaveLength(1);
    expect(findings[0]?.file).toBe("docs/adr/README.md");
    expect(findings[0]?.line).toBe(6);
    expect(findings[0]?.rule).toBe("disagreement");
    // The token carries both statuses and the line the record's own value is
    // on, so the reader is told which of the two to change rather than being
    // sent to look.
    expect(findings[0]?.token).toBe('index "Accepted" vs record docs/adr/0038-one-turn.md:3 "Proposed"');
  });

  test("every disagreeing row is reported, not only the first", () => {
    const root = repoWith(
      [
        ["0001", "0001-a.md", "Accepted"],
        ["0002", "0002-b.md", "Superseded"],
        ["0003", "0003-c.md", "Accepted"],
      ],
      { "0001-a.md": record("Proposed"), "0002-b.md": record("Accepted"), "0003-c.md": record("Accepted") },
    );
    const { findings } = checkStatus(root);
    expect(findings.map((f) => f.rule)).toEqual(["disagreement", "disagreement"]);
    expect(findings.map((f) => f.line)).toEqual([6, 7]);
  });

  test("a row citing a record that is not there fails", () => {
    const root = repoWith(
      [
        ["0001", "0001-present.md", "Accepted"],
        ["0002", "0002-absent.md", "Accepted"],
      ],
      { "0001-present.md": record("Accepted") },
    );
    const { findings } = checkStatus(root);
    expect(findings.map((f) => f.rule)).toEqual(["missing record"]);
    expect(findings[0]?.line).toBe(7);
  });

  test("a record with no Status: line fails", () => {
    const root = repoWith([["0001", "0001-headless.md", "Accepted"]], { "0001-headless.md": record(null) });
    expect(checkStatus(root).findings.map((f) => f.rule)).toEqual(["missing status"]);
  });

  test("a row with an empty status cell fails", () => {
    const root = repoWith([["0001", "0001-open.md", "Accepted"]], { "0001-open.md": record("Accepted") });
    writeFileSync(join(root, "docs/adr/README.md"), "| ID | Title | Status |\n|---|---|---|\n| [0001](0001-open.md) | Some title |  |\n");
    expect(checkStatus(root).findings.map((f) => f.rule)).toEqual(["empty status"]);
  });
});

describe("negative controls: the shapes that must never be flagged", () => {
  test("trailing clauses that differ in wording are not a disagreement", () => {
    // Three of the rows that carry one. If the comparison were on the whole
    // cell rather than its leading word, every amended and superseded record
    // in the table would fail, and a check that fails on correct content gets
    // switched off rather than fixed.
    const root = repoWith(
      [
        ["0023", "0023-a.md", "Accepted, amended by [0027](0027-b.md)"],
        ["0026", "0026-b.md", "Accepted, superseded by [0037](0037-c.md)"],
        ["0027", "0027-b.md", "Accepted; supersedes [0026](0026-b.md), amends [0025](0025-d.md)"],
      ],
      {
        "0023-a.md": record("Accepted, amended by [0027](0027-b.md)"),
        "0026-b.md": record("Accepted; superseded by [0037](0037-c.md)"),
        "0027-b.md": record("Accepted; Decision point 3 partially superseded by [0036](0036-e.md) -- its other half stands, unamended."),
      },
    );
    expect(checkStatus(root).findings).toEqual([]);
  });

  test("a superseded record is not read as a missing one", () => {
    // The one row whose leading word is not `Accepted`, and the reason the
    // comparison is on the leading word rather than the literal string.
    const root = repoWith([["0010", "0010-npm.md", "Superseded by [0022](0022-release.md)"]], { "0010-npm.md": record("Superseded by [ADR-0022](0022-release.md)") });
    expect(checkStatus(root).findings).toEqual([]);
  });

  test("the reserved-and-unused 0033 is not a missing record", () => {
    // The number was reserved and left unused, and `docs/adr/README.md` says
    // so in prose. The check runs from the index outward, so the gap between
    // these two rows is never visited and never reported: what the rule asks
    // is that the index and the record agree, not that the numbering be
    // contiguous.
    const root = repoWith(
      [
        ["0032", "0032-a.md", "Accepted"],
        ["0034", "0034-b.md", "Accepted"],
      ],
      { "0032-a.md": record("Accepted"), "0034-b.md": record("Accepted") },
    );
    expect(checkStatus(root).findings).toEqual([]);
  });

  test("the prose documenting the 0033 gap is not read as a row", () => {
    const gap = "There is no ADR-0033: the number was reserved and left unused, and no\nrow below cites ADR-0033.";
    const root = repoWith([["0034", "0034-b.md", "Accepted"]], { "0034-b.md": record("Accepted") });
    writeFileSync(join(root, "docs/adr/README.md"), indexWith([["0034", "0034-b.md", "Accepted"]], gap));
    const { findings, rows } = checkStatus(root);
    expect(findings).toEqual([]);
    expect(rows).toBe(1);
  });

  test("the header, the separator, and the table's neighbours are not rows", () => {
    const root = repoWith([["0001", "0001-a.md", "Accepted"]], { "0001-a.md": record("Accepted") });
    writeFileSync(join(root, "docs/adr/README.md"), ["# Architecture Decision Records", "", "| ID | Title | Status |", "|---|---|---|", "| [0001](0001-a.md) | Some title | Accepted |", "", "Row 0026 is superseded by 0037 in 0037's own text, not by editing its own.", ""].join("\n"));
    expect(checkStatus(root).rows).toBe(1);
  });
});

describe("the two ways a parser reads too little", () => {
  test("an index that yields no rows throws instead of reporting clean", () => {
    // A relocated README, a rewritten table, or a moved `docs/adr/` each read
    // as zero rows, and all three would otherwise print the same all-clear as
    // a healthy index.
    const root = repoWith([["0001", "0001-a.md", "Accepted"]], { "0001-a.md": record("Accepted") });
    writeFileSync(join(root, "docs/adr/README.md"), "# Architecture Decision Records\n\nNothing tabular here.\n");
    expect(() => checkStatus(root)).toThrow(/no index rows/);
  });

  test("a row whose link does not parse is reported, not skipped", () => {
    // The same failure class one row deep. The record this row names is
    // present and its status agrees with the cell, so the only thing wrong is
    // that the row is not readable -- and a row that quietly stops being
    // checked is indistinguishable from a clean index.
    const root = repoWith([["0002", "0002-b.md", "Accepted"]], { "0002-b.md": record("Accepted") });
    writeFileSync(join(root, "docs/adr/README.md"), "| ID | Title | Status |\n|---|---|---|\n| [0002] 0002-b.md | Some title | Accepted |\n");
    const { findings, rows } = checkStatus(root);
    expect(rows).toBe(1);
    expect(findings).toHaveLength(1);
    expect(findings[0]?.rule).toBe("unparsed row");
    expect(findings[0]?.line).toBe(3);
    expect(findings[0]?.token).toBe("[0002] 0002-b.md");
  });
});

describe("the units", () => {
  test("the leading word is the status, whatever follows it", () => {
    expect(statusToken("Accepted")).toBe("accepted");
    expect(statusToken("Accepted, amended by [0027](0027-x.md)")).toBe("accepted");
    expect(statusToken("Accepted; supersedes [0026](0026-y.md)")).toBe("accepted");
    expect(statusToken("Superseded by [ADR-0022](0022-z.md)")).toBe("superseded");
    expect(statusToken("  Accepted  ")).toBe("accepted");
  });

  test("text that is not a status has no token rather than an empty one", () => {
    // Telling "no status written" apart from "the word accepted" by a null
    // check is what lets the empty-cell case be reported at all.
    expect(statusToken("")).toBeNull();
    expect(statusToken("   ")).toBeNull();
    expect(statusToken("-- not decided --")).toBeNull();
  });

  test("the record's own header line is read, and a quoted one is not", () => {
    expect(parseRecordStatus(record("Accepted, amended by [0027](0027-x.md)"))?.line).toBe(3);
    // A section that quotes a status line must not supply the record's own,
    // or a record could claim a state it never declared.
    expect(parseRecordStatus("# A decision\n\n## Context\n\n* Status: Proposed\n")).toBeNull();
  });

  test("the header is read whatever order and spacing it uses", () => {
    expect(parseRecordStatus("# A decision\n\n* Status: Proposed\n* Date: 2026-09-22\n")?.value).toBe("Proposed");
    expect(parseRecordStatus("# A decision\n\n* Deciders: someone\n*Status: Accepted\n")?.value).toBe("Accepted");
  });
});

/**
 * What this check considers to be the repository.
 *
 * These read the real `docs/adr/`, because the property under test is the one
 * a fixture would only assume: that the live index and the live records
 * actually agree, and that the index and the directory enumerate the same
 * records. A fixture can only ever prove the checker works on the fixture.
 */
describe("this repository's own index", () => {
  const root = join(import.meta.dir, "..");
  const index = readFileSync(join(root, "docs/adr/README.md"), "utf8");

  test("no index row disagrees with the record it cites", () => {
    expect(checkStatus(root).findings).toEqual([]);
  });

  test("every record on disk is cited by exactly one row", () => {
    const records = readdirSync(join(root, "docs/adr")).filter((name) => /^0\d{3}-.+\.md$/.test(name));
    // Not a hard-coded count, which would rot on the next ADR: the invariant
    // is that the index and the directory enumerate the same records, so a
    // record written without a row shows up here as a difference.
    expect(checkStatus(root).rows).toBe(records.length);
    expect(records.length).toBeGreaterThan(0);
  });

  test("the gap the index documents is a gap in it, not a hole", () => {
    const ids = parseIndex(index).map((row) => row.id);
    expect(ids).toContain("0032");
    expect(ids).toContain("0034");
    expect(ids).not.toContain("0033");
  });
});
