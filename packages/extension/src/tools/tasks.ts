// `crew_task`: the extension-side front for OMP-owner `task/upsert` and
// `task/get`. A worker-scoped MCP tool of the same display name runs in a
// different process/tool registry and exposes read-only task context; this
// tool is the ompExtension-authorized counterpart.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import { OMP_NATIVE_CORRELATION_ENTRY_TYPE } from "../omp-native/persistence";
import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_TASK_TOOL_NAME = "crew_task";

/**
 * The revision every task this tool creates is stored with. `task/upsert`
 * persists exactly the revision it is sent and returns only
 * `{ taskId, sequence }`, so this constant is also the revision a later
 * `reconcile/omp` must present -- the two must never be written separately.
 */
const INITIAL_TASK_REVISION = 0;

export function registerTaskTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["upsert", "get"]).describe("Which task operation to perform."),
    taskId: pi.zod.string().optional().describe("Optional for upsert: reuse an existing task ID (for resume); auto-generated if omitted. Required for get."),
  });

  pi.registerTool({
    name: CREW_TASK_TOOL_NAME,
    label: "Crew Task",
    description:
      "Use when you need to create a persistent, cross-session unit of work that will be executed by an external AI harness (Claude, Codex, Copilot, or OMP-RPC) -- not OMP's native in-process task subagent. Use op: 'upsert' to create or update a task, or op: 'get' to read one back. Crew stores no task text: the task graph and its descriptions live in OMP, and the instruction a worker executes is passed to crew_run as prompt. Persists across session disconnects (stored in SQLite journal), executes via external harness processes, and can be retried, cancelled, or reconciled after failure. Auto-generates a task ID and uses your OMP session as owner. After creating, select a worker with crew_worker { op: 'list' } and submit execution with crew_run { op: 'submit', taskId, workerId, prompt }.",
    parameters: params,
    // `get` is a read: charging it a write approval made reading a task
    // cost the same as mutating one.
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && args.op === "get" ? "read" : "write"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "upsert": {
          const taskId = input.taskId ?? crypto.randomUUID();
          const result = await callOrchestration(client, "task/upsert", {
            taskId,
            ownerClientInstanceId: extCtx.sessionManager.getSessionId(),
            revision: INITIAL_TASK_REVISION,
          });
          // Remember the correlation so a later OMP process can rebind
          // this task's ownership via `reconcile/omp`. The runtime exposes
          // no way to enumerate owned tasks, so an unrecorded task can
          // never be reclaimed after a restart.
          if (result.isError !== true) {
            try {
              pi.appendEntry(OMP_NATIVE_CORRELATION_ENTRY_TYPE, {
                taskId,
                revision: INITIAL_TASK_REVISION,
              });
            } catch (err) {
              pi.logger.warn("crew task: failed to persist task correlation", {
                taskId,
                error: err instanceof Error ? err.message : String(err),
              });
            }
          }
          return result;
        }
        case "get":
          return callOrchestration(client, "task/get", { taskId: input.taskId });
      }
    },
  });
}
