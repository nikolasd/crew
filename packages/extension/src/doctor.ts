// The `crewd doctor` CLI command wrapper: spawn the binary with `--json`
// and parse the structured output. Used by both the `crew_doctor` tool and
// the `/crew-doctor` command.
//
// Unlike `status.ts`, this does not connect to a running runtime — it invokes
// the CLI directly, so it works even when no runtime is serving the repo.

import { spawn } from "node:child_process";
import { homedir } from "node:os";
import { detectLibc, resolveCrewd } from "./platform";
import { resolveStateRoot } from "./state";

/** A single failed check from the doctor output. */
export interface FailedCheck {
  /** The name of the check. */
  readonly check_name: string;
  /** The error message. */
  readonly error: string;
}

/** Successful doctor output. */
export interface DoctorResult {
  /** Whether the runtime is healthy. */
  readonly healthy: boolean;
  /** The set of checks that passed. */
  readonly passed_checks: string[];
  /** The set of checks that failed, with error messages. */
  readonly failed_checks: FailedCheck[];
  /** The set of unresolved rollout gates. */
  readonly unresolved_gates: string[];
}

/** Sanitized, machine-readable detail when the doctor command fails. */
export interface DoctorFailure {
  /** Machine-readable error code. */
  readonly code: string;
  /** Human-readable error message. */
  readonly message: string;
  /** The command the operator can run to diagnose further. */
  readonly doctorCommand: string;
}

/** Successful result from the doctor command. */
export interface DoctorSuccess {
  /** Always absent for success, so `isError` discriminates the union
   * without every caller having to narrow with `in` first. */
  readonly isError?: false;
  /** Content blocks for display. */
  readonly content: [DoctorTextContent];
  /** Parsed doctor result. */
  readonly details: DoctorResult;
}

/** Failure result from the doctor command. */
export interface DoctorErrorResult {
  /** Always true for errors. */
  readonly isError: true;
  /** Content blocks for display. */
  readonly content: [DoctorTextContent];
  /** Machine-readable failure details. */
  readonly details: DoctorFailure;
}

export type DoctorTextContent = {
  type: "text";
  text: string;
};

export type DoctorCommandResult = DoctorSuccess | DoctorErrorResult;

/** Context needed to run the doctor command. */
export interface DoctorContext {
  /** Absolute Crew state root. */
  readonly stateDir: string;
  /** Absolute repository path. */
  readonly repository: string;
  /** Path to the `crewd` binary. */
  readonly crewdPath: string;
}

/**
 * Builds the doctor context for the given working directory.
 *
 * Resolves the state root through {@link resolveStateRoot} -- the same
 * function the launcher uses to spawn the daemon -- so the doctor always
 * diagnoses the directory a daemon actually writes. A second resolution
 * path here was the defect: it produced `<cwd>/.crew`, which no daemon
 * ever used.
 */
export function buildDoctorContext(cwd: string, env: NodeJS.ProcessEnv = process.env): DoctorContext {
  const stateDir = resolveStateRoot(env, homedir());
  const binary = resolveCrewd(process.platform, process.arch, detectLibc(), env, stateDir);
  return {
    stateDir,
    repository: cwd,
    crewdPath: binary.path,
  };
}

/**
 * Runs `crewd doctor --json` and parses the structured output.
 *
 * This is a synchronous spawn (no network, no runtime connection), so it
 * works even when no runtime is serving the repository.
 */
export async function runDoctorCommand(ctx: DoctorContext): Promise<DoctorCommandResult> {
  return new Promise<DoctorCommandResult>((resolve) => {
    const proc = spawn(ctx.crewdPath, ["doctor", "--json", "--state-dir", ctx.stateDir, "--repo", ctx.repository], {
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";

    proc.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString();
    });

    proc.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString();
    });

    proc.on("close", (code) => {
      const exitCode = code ?? 1;
      const doctorCommand = `${ctx.crewdPath} doctor --state-dir ${ctx.stateDir} --repo ${ctx.repository}`;

      if (exitCode !== 0) {
        // `crewd doctor --json` reports two distinct shapes on a
        // non-zero exit: a full result whose checks failed, and an abort
        // envelope `{ healthy: false, error }` for a condition that
        // stopped the catalog from running at all. Both carry the real
        // diagnostic; replacing either with a generic message is what
        // made this tool useless to debug.
        let parsed: unknown;
        try {
          parsed = JSON.parse(stdout);
        } catch {
          resolve(failureResult(ctx, "doctor-failed", stderr.trim() || `Doctor command exited with code ${exitCode}`, doctorCommand));
          return;
        }

        if (isDoctorResult(parsed)) {
          resolve({
            isError: true,
            content: [{ type: "text", text: formatDoctorOutput(parsed) }],
            details: {
              code: "doctor-failed",
              message: stderr.trim() || `Doctor reported ${parsed.failed_checks.length} failed check(s)`,
              doctorCommand,
            },
          });
          return;
        }

        const aborted = abortReason(parsed);
        resolve(failureResult(ctx, "doctor-failed", aborted || stderr.trim() || `Doctor command exited with code ${exitCode}`, doctorCommand));
      } else {
        // A zero exit is always a full result; a malformed body here is a
        // protocol break, not a check failure.
        let parsed: unknown;
        try {
          parsed = JSON.parse(stdout);
        } catch (err) {
          const message = err instanceof Error ? err.message : "Failed to parse doctor output";
          resolve(failureResult(ctx, "parse-error", message, doctorCommand));
          return;
        }
        if (!isDoctorResult(parsed)) {
          resolve(failureResult(ctx, "parse-error", "Doctor output is missing its check lists", doctorCommand));
          return;
        }
        resolve({
          content: [{ type: "text", text: formatDoctorOutput(parsed) }],
          details: parsed,
        });
      }
    });

    proc.on("error", (err) => {
      const doctorCommand = `${ctx.crewdPath} doctor --state-dir ${ctx.stateDir} --repo ${ctx.repository}`;
      resolve(failureResult(ctx, "spawn-error", err.message, doctorCommand));
    });
  });
}

function failureResult(ctx: DoctorContext, code: string, message: string, doctorCommand?: string): DoctorErrorResult {
  return {
    isError: true,
    content: [{ type: "text", text: `Doctor command failed: ${message}` }],
    details: {
      code,
      message,
      doctorCommand: doctorCommand ?? `${ctx.crewdPath} doctor --state-dir ${ctx.stateDir} --repo ${ctx.repository}`,
    },
  };
}

/**
 * Narrows to a full doctor result, as opposed to the abort envelope the
 * CLI emits when a condition stops the catalog from running. The check is
 * on the two list fields the formatter dereferences, so a value that
 * passes here cannot make the formatter throw.
 */
function isDoctorResult(value: unknown): value is DoctorResult {
  if (value === null || typeof value !== "object") return false;
  if (!("passed_checks" in value) || !("failed_checks" in value)) return false;
  return Array.isArray(value.passed_checks) && Array.isArray(value.failed_checks);
}

/**
 * The `error` text from an abort envelope, or `undefined` if the value is
 * not one. This is the underlying diagnostic the tool must surface rather
 * than replace with a generic message.
 */
function abortReason(value: unknown): string | undefined {
  if (value === null || typeof value !== "object" || !("error" in value)) return undefined;
  return typeof value.error === "string" ? value.error : undefined;
}

function formatDoctorOutput(result: DoctorResult): string {
  const lines: string[] = [];
  lines.push(`Doctor check: ${result.healthy ? "healthy" : "failed"}`);

  if (result.passed_checks.length > 0) {
    lines.push(`Passed checks: ${result.passed_checks.join(", ")}`);
  }

  if (result.failed_checks.length > 0) {
    lines.push("Failed checks:");
    for (const check of result.failed_checks) {
      lines.push(`  - ${check.check_name}: ${check.error}`);
    }
  }

  if (result.unresolved_gates.length > 0) {
    lines.push(`Unresolved gates: ${result.unresolved_gates.join(", ")}`);
  }

  return lines.join("\n");
}
