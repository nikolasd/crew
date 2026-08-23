// `crew_child`: the accept/deny half of nested-worker spawning. A worker
// that wants a child records the intent via its own coordination tool; OMP
// decides here. `list` is tier `read`; `decide` is tier `exec` -- accepting
// provisions a real child run that executes on its own.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_CHILD_TOOL_NAME = "crew_child";

export function registerChildTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["list", "decide"]).describe("Which child-request operation to perform."),
    runId: pi.zod.string().optional().describe("Optional filter for list: only return child requests recorded by this run."),
    parentRunId: pi.zod.string().optional().describe("Required for decide: the run whose child request is being decided."),
    decision: pi.zod.enum(["accept", "deny"]).optional().describe("Required for decide: accept provisions the child run, deny refuses it."),
    childTaskId: pi.zod.string().optional().describe("Required when decision is accept: the task the child run executes."),
    childWorkerId: pi.zod.string().optional().describe("Required when decision is accept: the worker the child run executes as."),
    childRunId: pi.zod.string().optional().describe("Required when decision is accept: the run id to provision for the child."),
    reason: pi.zod.string().optional().describe("Required when decision is deny: why the child was refused."),
  });

  pi.registerTool({
    name: CREW_CHILD_TOOL_NAME,
    label: "Crew Child",
    description:
      "Use to see and decide nested-worker requests: a worker that wants to spawn a child records the intent, and nothing happens until you decide. Use op: 'list' to see pending requests (optionally filtered by runId), then op: 'decide' with parentRunId and decision. Accepting requires childTaskId, childWorkerId, and childRunId; denying requires reason. A request is only an intent -- accepting is what creates the child run.",
    parameters: params,
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && args.op === "decide" ? "exec" : "read"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "list":
          return callOrchestration(client, "coordination/child/list", { runId: input.runId });
        // Field-level requirements (childTaskId/childWorkerId/childRunId for
        // accept, reason for deny) are enforced by the runtime, which
        // returns invalid_params. Duplicating that here would be a second
        // copy of the rule, free to drift from the one that matters.
        case "decide":
          return callOrchestration(client, "coordination/child/decide", {
            parentRunId: input.parentRunId,
            decision: input.decision,
            childTaskId: input.childTaskId,
            childWorkerId: input.childWorkerId,
            childRunId: input.childRunId,
            reason: input.reason,
          });
      }
    },
  });
}
