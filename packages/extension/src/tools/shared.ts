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
