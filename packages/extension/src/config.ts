// The `crewd config` CLI wrapper behind `/crew-config` and the
// `crew_config` tool: scaffold a crew.json layer, print a configuration
// document, or report which layers apply.
//
// Modelled on `doctor.ts`'s split -- `buildConfigArgs` is the pure,
// hermetically testable core; `runConfigCommand` spawns the binary and
// shapes the tool/command result. The daemon owns every decision about
// layer precedence, merge semantics, and file contents; this module only
// resolves an operation into an argument vector.

import { spawn } from "node:child_process";

/** Which document `config print` emits. */
export type ConfigDocument = "defaults" | "schema" | "effective";

/** A `crewd config` invocation, before it becomes an argument vector. */
export type ConfigRequest =
  | { readonly op: "init"; readonly repository: string; readonly global?: boolean; readonly force?: boolean }
  | { readonly op: "print"; readonly repository: string; readonly document?: ConfigDocument }
  | { readonly op: "path"; readonly repository: string };

/** A text content block, structurally compatible with OMP's `TextContent`. */
export type ConfigTextContent = { type: "text"; text: string };

/** The result of running a `crewd config` operation. */
export interface ConfigCommandResult {
  readonly isError?: boolean;
  readonly content: [ConfigTextContent];
  readonly details: Record<string, unknown>;
}

/** Everything `runConfigCommand` needs to spawn the binary. */
export interface ConfigContext {
  readonly crewdPath: string;
  readonly repository: string;
}

/** How long a config invocation may run before it is abandoned. */
const CONFIG_TIMEOUT_MS = 30_000;

/**
 * The exact argument vector for a `crewd config` invocation.
 *
 * `--global` deliberately omits `--repo`: the daemon resolves `~/.omp`
 * from its own environment, and passing both would invite the two sides to
 * disagree about which file is being written.
 */
export function buildConfigArgs(request: ConfigRequest): string[] {
  switch (request.op) {
    case "init": {
      const args = ["config", "init"];
      if (request.global === true) {
        args.push("--global");
      } else {
        args.push("--repo", request.repository);
      }
      if (request.force === true) {
        args.push("--force");
      }
      return args;
    }
    case "print":
      return ["config", "print", `--${request.document ?? "effective"}`, "--repo", request.repository];
    case "path":
      return ["config", "path", "--repo", request.repository];
  }
}

/**
 * Runs `crewd config` and returns its output. Never throws: a non-zero
 * exit is reported as an error result carrying the daemon's own stderr,
 * which is the actionable content (an existing crew.json refusing to be
 * clobbered, an unknown key naming its JSON path).
 */
export async function runConfigCommand(ctx: ConfigContext, request: ConfigRequest): Promise<ConfigCommandResult> {
  const args = buildConfigArgs(request);

  return new Promise((resolve) => {
    const proc = spawn(ctx.crewdPath, args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => proc.kill("SIGKILL"), CONFIG_TIMEOUT_MS);

    proc.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    proc.stderr.on("data", (chunk) => {
      stderr += chunk;
    });

    proc.on("error", (err) => {
      clearTimeout(timer);
      resolve({
        isError: true,
        content: [{ type: "text", text: `crew config failed: ${err.message}` }],
        details: { code: "spawn-failed", message: err.message, op: request.op },
      });
    });

    proc.on("close", (code) => {
      clearTimeout(timer);
      if (code === 0) {
        resolve({
          content: [{ type: "text", text: stdout.trimEnd() }],
          details: { op: request.op, output: stdout },
        });
        return;
      }
      const message = stderr.trim() || `crewd config exited with code ${code}`;
      resolve({
        isError: true,
        content: [{ type: "text", text: `crew config failed: ${message}` }],
        details: { code: "config-failed", message, op: request.op },
      });
    });
  });
}
