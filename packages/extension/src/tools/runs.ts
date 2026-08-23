// `crew_run`: submits, lists, fetches, retries, and cancels runs.
// `submit`, `retry`, and `cancel` are tier `exec` -- they start, restart,
// or stop adapter processes. `retry` creates a distinct run (never mutates the prior one).

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_RUN_TOOL_NAME = "crew_run";

export function registerRunTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["submit", "list", "get", "retry", "cancel", "result"]).describe("Which run operation to perform."),
    prompt: pi.zod.string().optional().describe("Required for submit and retry: the instruction the worker executes. Crew stores no task text, so the task's description must be passed here."),
    taskId: pi.zod.string().optional().describe("Required for submit: the task to execute. Optional filter for list."),
    workerId: pi.zod.string().optional().describe("Required for submit and retry: the worker to execute with."),
    workspaceMode: pi.zod.enum(["shared", "isolated", "copy"]).optional().describe("Optional workspace mode for submit and retry: 'shared' (the repository itself, the default), 'isolated' (a per-run git worktree), or 'copy' (a per-run copy of the repository)."),
    priority: pi.zod.number().int().optional().describe("Optional priority for submit."),
    runId: pi.zod.string().optional().describe("Required for get, cancel, and result: the run id."),
    priorRunId: pi.zod.string().optional().describe("Required for retry: the terminal run id to retry."),
  });

  pi.registerTool({
    name: CREW_RUN_TOOL_NAME,
    label: "Crew Run",
    description:
      "Use to execute, monitor, or manage task execution by external workers. Use op: 'submit' to start execution (requires taskId from crew_task, workerId from crew_worker, and prompt -- the instruction text the worker executes), op: 'get' to check progress/status of a run, op: 'result' to read a finished run's final output text and token usage (requires runId; refused until the run reaches a terminal state -- chain work by passing resultText into the next submit's prompt), op: 'list' to list runs for a task, op: 'retry' to re-execute a terminal run (creates a new runId and starts a fresh worker process; pass prompt again), or op: 'cancel' to stop a running run. After submitting, monitor with op: 'get'. If the run fails, retry with op: 'retry' (new runId). If stuck, cancel with op: 'cancel'.",
    parameters: params,
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && (args.op === "submit" || args.op === "retry" || args.op === "cancel") ? "exec" : "read"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "submit":
          return callOrchestration(client, "run/submit", {
            taskId: input.taskId,
            prompt: input.prompt,
            workerId: input.workerId,
            workspaceMode: input.workspaceMode,
            priority: input.priority,
          });
        case "list":
          return callOrchestration(client, "run/list", { taskId: input.taskId });
        case "get":
          return callOrchestration(client, "run/get", { runId: input.runId });
        case "result":
          return callOrchestration(client, "run/result", { runId: input.runId });
        case "retry":
          return callOrchestration(client, "run/retry", {
            priorRunId: input.priorRunId,
            workerId: input.workerId,
            prompt: input.prompt,
            workspaceMode: input.workspaceMode,
          });
        case "cancel":
          return callOrchestration(client, "run/cancel", { runId: input.runId });
      }
    },
  });
}
