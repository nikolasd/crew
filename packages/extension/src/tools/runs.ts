// `crew_run`: submits, lists, fetches, retries, cancels, and settles runs.
// `submit`, `retry`, `cancel`, `timeoutAck`, and `finish` are tier `exec`
// -- they start, restart, stop, or settle adapter processes. `retry`
// creates a distinct run (never mutates the prior one). `finish` is the
// leader's own ADR-0027 settle decision (run/finish), distinct from
// `cancel`: it states an outcome and works on any non-terminal run, not
// only one that has stopped producing output.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration, displayPreferenceFragment } from "./shared";

export const CREW_RUN_TOOL_NAME = "crew_run";

export function registerRunTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["submit", "list", "get", "retry", "cancel", "result", "timeoutAck", "finish"]).describe("Which run operation to perform: submit | list | get | retry | cancel | result | timeoutAck | finish."),
    prompt: pi.zod.string().optional().describe("Required for submit and retry: the instruction the worker executes. Crew stores no task text, so the task's description must be passed here."),
    taskId: pi.zod.string().optional().describe("Required for submit: the task to execute. Optional filter for list."),
    workerId: pi.zod.string().optional().describe("Required for submit and retry: the worker to execute with."),
    workspaceMode: pi.zod.enum(["shared", "isolated", "copy"]).optional().describe("Optional workspace mode for submit and retry: shared | isolated | copy — shared (repository itself, default), isolated (per-run git worktree), copy (per-run copy)."),
    priority: pi.zod.number().int().optional().describe("Optional priority for submit."),
    runId: pi.zod.string().optional().describe("Required for get, cancel, result, timeoutAck, and finish: the run id."),
    priorRunId: pi.zod.string().optional().describe("Required for retry: the terminal run id to retry."),
    decision: pi.zod
      .enum(["extend", "nudge", "abort"])
      .optional()
      .describe(
        "Required for timeoutAck: how to respond to a WorkerTimeout fact. 'extend' re-arms both liveness deadlines with a fresh window. 'nudge' is a server-side no-op -- follow up with crew_send (op: 'send') to actually nudge the worker. 'abort' cancels the run (same effect as op: 'cancel'). 'extend' can be refused (code -32602, 'no tracked timeout to extend') if the run settled between the timeout being journaled and this call arriving -- an expected, benign race (you acted correctly, just slightly late), not a fault: do not retry or escalate it, just move on (read the run's state with op: 'get' if unsure).",
      ),
    outcome: pi.zod.enum(["succeeded", "failed"]).optional().describe("Optional for finish (default 'succeeded'): succeeded | failed — the leader's judgment of how the run went. Never inferred from the vendor's own turn markers -- only the leader can judge whether the task actually succeeded."),
  });

  pi.registerTool({
    name: CREW_RUN_TOOL_NAME,
    label: "Crew Run",
    description:
      "Use to execute, monitor, or manage task execution by external workers. Use op: 'submit' to start execution (requires taskId from crew_task, workerId from crew_worker, and prompt -- the instruction text the worker executes), op: 'get' to check progress/status of a run, op: 'result' to read a run's output text and token usage (requires runId; readable once the run is terminal OR has settled a turn without exiting -- a TUI worker persists between turns and a protocol worker exits when its turn ends, but in both cases a settled turn is when the answer is readable, so 'waitingUser' with a journaled turn-end already qualifies, per ADR-0027 -- chain work by passing resultText into the next submit's prompt), op: 'list' to list runs for a task, op: 'retry' to re-execute a terminal run (creates a new runId and starts a fresh worker process; pass prompt again), op: 'cancel' to stop a running run immediately, op: 'timeoutAck' to decide a WorkerTimeout fact (requires runId and decision: 'extend' | 'nudge' | 'abort'), or op: 'finish' to end a run as the leader's own settle decision (requires runId; optional outcome: 'succeeded' | 'failed', default 'succeeded' -- states the outcome rather than inferring it, and works on any non-terminal run, not only one that has settled a turn). After submitting, monitor with op: 'get'. A run's NORMAL end is a rendered verdict, not a retry or a cancel -- but which of 'result'/'finish' applies depends on whether the run is still parked or already terminal, and the two must not be joined: if op: 'get' (or a milestone) shows the run settled but still non-terminal ('waitingUser'), read the answer with op: 'result' and close it out with op: 'finish' -- a protocol worker runs one turn and exits, so that park is transient by construction: by the time you act the run commonly already reads terminal instead of parked, which is expected, not a race you lost. If op: 'get' already shows the run terminal (including 'cancelled' after a settled turn -- the same run, no verdict rendered, but nothing left to render one on), op: 'result' is the whole of it: read it, and do not call op: 'finish' -- a terminal run refuses it ('invalid_params', already finished). Only retry with op: 'retry' if op: 'get' actually reports the run failed. Only cancel with op: 'cancel' if op: 'get' shows it genuinely stuck (no activity), never merely because it looks quiet while settled and awaiting your own read.",
    parameters: params,
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && (args.op === "submit" || args.op === "retry" || args.op === "cancel" || args.op === "timeoutAck" || args.op === "finish") ? "exec" : "read"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "submit": {
          const result = await callOrchestration(client, "run/submit", {
            taskId: input.taskId,
            prompt: input.prompt,
            workerId: input.workerId,
            workspaceMode: input.workspaceMode,
            priority: input.priority,
            ...displayPreferenceFragment(),
          });
          if (result.isError && ctx.reportSubmitFailure !== undefined) {
            const msg = (result.details as { message?: string })?.message ?? "run/submit failed";
            ctx.reportSubmitFailure(`run/submit failed: ${msg}`);
          }
          return result;
        }
        case "list":
          return callOrchestration(client, "run/list", { taskId: input.taskId });
        case "get":
          return callOrchestration(client, "run/get", { runId: input.runId });
        case "result":
          return callOrchestration(client, "run/result", { runId: input.runId });
        case "retry": {
          const result = await callOrchestration(client, "run/retry", {
            priorRunId: input.priorRunId,
            workerId: input.workerId,
            prompt: input.prompt,
            workspaceMode: input.workspaceMode,
            ...displayPreferenceFragment(),
          });
          if (result.isError && ctx.reportSubmitFailure !== undefined) {
            const msg = (result.details as { message?: string })?.message ?? "run/retry failed";
            ctx.reportSubmitFailure(`run/retry failed: ${msg}`);
          }
          return result;
        }
        case "cancel":
          return callOrchestration(client, "run/cancel", { runId: input.runId });
        case "timeoutAck":
          return callOrchestration(client, "run/timeoutAck", { runId: input.runId, decision: input.decision });
        case "finish":
          return callOrchestration(client, "run/finish", { runId: input.runId, outcome: input.outcome });
      }
    },
  });
}
