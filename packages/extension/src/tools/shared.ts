// The single execution path shared by every orchestration tool: call the
// runtime's JSON-RPC method through the cached `CrewClient` and return its
// result verbatim as tool `details`, or map a JSON-RPC error to a stable
// tool error shape. Tools never select a worker, retry, mutate OMP todos,
// approve, merge, or infer lifecycle state here -- that authority stays with
// OMP and the runtime.

import type { AgentToolResult, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

import { CrewClient, JsonRpcRemoteError } from "../client";

/** Resolves the cached (or newly connected) runtime client for `cwd`. */
export interface OrchestrationToolContext {
  getClient(extCtx: ExtensionContext): Promise<CrewClient>;
  /** Optional callback to report a run/submit failure to the monitor. */
  reportSubmitFailure?: (message: string) => void;
}

/** The stable, structured shape of a mapped JSON-RPC tool error. */
export interface OrchestrationToolError {
  code: number;
  message: string;
  data?: unknown;
}

/**
 * Calls `method` on the runtime and shapes the result as an
 * {@link AgentToolResult}. On a JSON-RPC error, returns a non-throwing
 * `isError` result carrying `{ code, message, data }` (correlated IDs, when
 * the runtime includes them, travel in `data`) rather than throwing --
 * callers see a stable tool error instead of an unhandled rejection.
 */
export async function callOrchestration(client: CrewClient, method: string, params: unknown): Promise<AgentToolResult<unknown>> {
  try {
    const result = await client.request(method, params);
    return {
      content: [{ type: "text", text: renderSummary(method, result) }],
      details: result,
    };
  } catch (err) {
    if (err instanceof JsonRpcRemoteError) {
      const details: OrchestrationToolError = {
        code: err.code,
        message: err.message,
        data: err.data,
      };
      return {
        content: [{ type: "text", text: `${method} failed: ${err.message}` }],
        details,
        isError: true,
      };
    }
    throw err;
  }
}

function renderSummary(method: string, result: unknown): string {
  return `${method}: ${JSON.stringify(result)}`;
}

/** Wire values `run/submit`/`run/retry`'s `displayPreference.launchProgram`
 *  accepts (CREW-9) -- must match `crates/protocol/src/display.rs`'s
 *  `HostProgramHint` exactly. A closed set, never a raw string: this value
 *  ends up selecting and parameterizing an `osascript` invocation on the
 *  daemon side, so `$TERM_PROGRAM` content must never reach it unmapped --
 *  see `HostProgramHint`'s own doc comment. */
export type LaunchProgramHint = "appleTerminal" | "iTerm2" | "ghostty";

/**
 * Maps `$TERM_PROGRAM` to one of the closed set of programs
 * `OsWindowDisplay` knows how to target directly, or `undefined` for
 * anything else this build doesn't recognize.
 *
 * `$TERM_PROGRAM` names whatever process launched *this one* -- which may
 * be a multiplexer (`tmux`) or something else entirely, not necessarily
 * the terminal emulator the user is sitting in. An unrecognized value
 * (including a multiplexer's) is harmless here: tmux/herdr already win
 * backend resolution before `OsWindowDisplay` is ever reached, so this
 * hint only matters once resolution has already fallen through to a bare
 * terminal window, and `undefined` degrades to today's default there.
 */
export function launchProgramHint(env: Readonly<Record<string, string | undefined>> = process.env): LaunchProgramHint | undefined {
  switch (env.TERM_PROGRAM) {
    case "Apple_Terminal":
      return "appleTerminal";
    case "iTerm.app":
      return "iTerm2";
    case "ghostty":
      return "ghostty";
    default:
      return undefined;
  }
}

/**
 * The `displayPreference` fragment to spread into a `run/submit`/
 * `run/retry` params object, or `{}` when there is no recognized hint --
 * this is deliberately the *whole* key, not just `launchProgram` alone,
 * because `DisplayPreference.ordered` is a required field on the wire:
 * sending a bare `{launchProgram}` without it would be rejected as
 * malformed. `ordered: []` matches the daemon's own default (any
 * available backend) when `displayPreference` is absent entirely.
 *
 * CREW-52: `placement` is deliberately NOT included here at all, never a
 * hardcoded value. It used to be `"embedded"`, independently hardcoded on
 * both sides of the wire and asserted (by a comment, not a test) to match
 * the daemon's own default -- exactly the class of doc-claim defect this
 * wave has been closing all week. `placement` is now optional on the
 * wire (`#[serde(default)]`): omitting it here means the resolved
 * backend picks its own natural placement (`DisplayRegistry::resolve`),
 * single-sourced on the protocol side where the extension cannot
 * contradict it, rather than two copies that only agreed by comment.
 */
export function displayPreferenceFragment(env: Readonly<Record<string, string | undefined>> = process.env): { displayPreference?: { ordered: []; launchProgram: LaunchProgramHint } } {
  const hint = launchProgramHint(env);
  return hint === undefined ? {} : { displayPreference: { ordered: [], launchProgram: hint } };
}
