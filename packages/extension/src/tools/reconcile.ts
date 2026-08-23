// `crew_reconcile`: rebinds a task's owning OMP client instance after a
// disconnect/reconnect. The runtime only accepts the rebind when task id
// and monotonic OMP revision match; it journals the old/new owner ids.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_RECONCILE_TOOL_NAME = "crew_reconcile";

export function registerReconcileTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    taskId: pi.zod.string().describe("The task id to rebind to this OMP client instance."),
    revision: pi.zod.number().int().nonnegative().describe("The monotonic OMP revision that must match the stored task."),
  });

  pi.registerTool({
    name: CREW_RECONCILE_TOOL_NAME,
    label: "Crew Reconcile",
    description:
      "Use after a session drop or reconnect when your OMP session was interrupted and you had active tasks. Rebinds task ownership from the prior session to the current one. Requires matching taskId and monotonic revision (the runtime rejects rebinds on revision mismatch to prevent race conditions). Call when your session was interrupted and restarted, you have active tasks from a prior session that need to be reattached, or the runtime reports ownership conflicts.",
    parameters: params,
    approval: "write",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      return callOrchestration(client, "reconcile/omp", {
        taskId: input.taskId,
        revision: input.revision,
      });
    },
  });
}
