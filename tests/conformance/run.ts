// Aggregate conformance runner.
//
// Spawns the real `crewd conformance` CLI once for every adapter and
// writes a combined report. This is the release gate's data source, so it
// never fabricates a report: a non-zero exit throws and no file is written.

import { readFileSync, writeFileSync } from "node:fs";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { AdapterConformanceReport, CombinedReport } from "./assert-report";

/** Resolves the `crewd` binary the same way the extension's tests do,
 * accepting the pre-rename `OMP_BATMAN_BINARY` name as a fallback. */
function crewdPath(): string {
  return process.env.OMP_CREW_BINARY ?? process.env.OMP_BATMAN_BINARY ?? "target/debug/crewd";
}

/**
 * Runs every adapter's **fixture** conformance suite and writes
 * `{ timestamp, adapters }` to `outputPath`, keyed by each report's own
 * `adapter` field.
 *
 * `--adapter all` already loops over all four adapters, so this spawns the
 * CLI exactly once rather than once per adapter.
 *
 * Fixture mode only: `--live` invokes real vendor CLIs and would make a
 * release gate spend billed tokens.
 *
 * @throws if the CLI exits non-zero, or emits output that is not an array
 *   of reports. Either way `outputPath` is left untouched.
 */
export async function runAllFixtures(outputPath: string): Promise<void> {
  const scratch = mkdtempSync(join(tmpdir(), "crew-conformance-"));
  const reportPath = join(scratch, "conformance.json");

  const proc = Bun.spawn([crewdPath(), "conformance", "--adapter", "all", "--fixture", "--output", reportPath], { stdout: "pipe", stderr: "pipe" });
  const [exitCode, stderr] = await Promise.all([proc.exited, new Response(proc.stderr).text()]);

  if (exitCode !== 0) {
    throw new Error(`crewd conformance exited ${exitCode}: ${stderr.trim() || "(no stderr)"}`);
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(readFileSync(reportPath, "utf-8"));
  } catch (err) {
    throw new Error(`crewd conformance wrote an unreadable report: ${err}`);
  }
  if (!Array.isArray(parsed)) {
    throw new Error("crewd conformance must write a JSON array of reports");
  }

  const adapters: Record<string, AdapterConformanceReport> = {};
  for (const entry of parsed) {
    if (entry === null || typeof entry !== "object" || !("adapter" in entry)) {
      throw new Error("every conformance report element must carry an 'adapter' field");
    }
    const { adapter } = entry as { adapter: unknown };
    if (typeof adapter !== "string") {
      throw new Error("a conformance report's 'adapter' field must be a string");
    }
    adapters[adapter] = entry as AdapterConformanceReport;
  }

  const combined: CombinedReport = {
    timestamp: new Date().toISOString(),
    adapters,
  };
  writeFileSync(outputPath, `${JSON.stringify(combined, null, 2)}\n`);
}
