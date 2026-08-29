// Leader-facing orchestration tools built on the existing `crew_*` primitives.
// These are the team-leader product surface: decomposed execution
// (`crew_spawn`), steering (`crew_send`), situation awareness
// (`crew_status`, `crew_transcript`), and lifecycle control (`crew_stop`,
// `crew_finish`). Each is a thin, validated composition -- no worker
// selection heuristics beyond "reuse an idle worker of the right adapter",
// no retry/merge/lifecycle inference.

import type { AgentToolResult, ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_SPAWN_TOOL_NAME = "crew_spawn";
export const CREW_SEND_TOOL_NAME = "crew_send";
export const CREW_STATUS_TOOL_NAME = "crew_status";
export const CREW_TRANSCRIPT_TOOL_NAME = "crew_transcript";
export const CREW_STOP_TOOL_NAME = "crew_stop";
export const CREW_FINISH_TOOL_NAME = "crew_finish";

interface SubtaskProposal {
  id: string;
  description: string;
  adapter: string;
  writes?: boolean;
  turnBudget?: number;
}

// --------------------------------------------------------------- crew_spawn

export function registerSpawnTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["spawn"]).describe("Spawn a single approved subtask as its own run."),
    taskId: pi.zod.string().describe("The leader task this subtask executes under."),
    planId: pi.zod.string().describe("The plan's run id (plan_id = run_id); the subtask must belong to it."),
    subtaskId: pi.zod.string().describe("The subtask id from the approved plan to spawn."),
    workspaceMode: pi.zod.enum(["shared", "isolated", "copy"]).optional().describe("Optional workspace mode for the spawned run."),
    priority: pi.zod.number().int().optional().describe("Optional priority for the spawned run."),
  });

  pi.registerTool({
    name: CREW_SPAWN_TOOL_NAME,
    label: "Crew Spawn",
    description: "Use after crew_plan is approved to execute one subtask. Resolves the subtask from the plan, reuses an idle worker of its adapter (or creates one), and submits a run with the subtask description as the prompt, tagging it with the plan and subtask ids for budget tracking and supervision.",
    parameters: params,
    approval: "exec",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx): Promise<AgentToolResult<unknown>> {
      const client = await ctx.getClient(extCtx);

      const planRes = await callOrchestration(client, "plan/get", { runId: input.planId });
      if (planRes.isError === true) return planRes;
      const plan = planRes.details as { plan?: { subtasks?: SubtaskProposal[] } };
      const subtasks = plan.plan?.subtasks ?? [];
      const subtask = subtasks.find((s) => s.id === input.subtaskId);
      if (subtask === undefined) {
        return {
          content: [{ type: "text", text: `Subtask ${input.subtaskId} not found in plan ${input.planId}.` }],
          details: { planId: input.planId, subtaskId: input.subtaskId, outcome: "notFound" },
          isError: true,
        };
      }

      const workersRes = await callOrchestration(client, "worker/list", {});
      const workers = (workersRes.details as { workers?: Array<{ workerId: string; adapter: string }> } | undefined)?.workers ?? [];
      const existing = workers.find((w) => w.adapter === subtask.adapter);
      let workerId: string | undefined;
      if (existing !== undefined) {
        workerId = existing.workerId;
      } else {
        const created = await callOrchestration(client, "worker/create", { adapter: subtask.adapter });
        if (created.isError === true) return created;
        workerId = (created.details as { workerId?: string } | undefined)?.workerId;
      }
      if (workerId === undefined) {
        return {
          content: [{ type: "text", text: `Could not resolve a worker for adapter ${subtask.adapter}.` }],
          details: { adapter: subtask.adapter, outcome: "noWorker" },
          isError: true,
        };
      }

      const result = await callOrchestration(client, "run/submit", {
        taskId: input.taskId,
        workerId,
        prompt: subtask.description,
        planId: input.planId,
        subtaskId: input.subtaskId,
        ...(input.workspaceMode !== undefined ? { workspaceMode: input.workspaceMode } : {}),
        ...(input.priority !== undefined ? { priority: input.priority } : {}),
      });
      if (result.isError && ctx.reportSubmitFailure !== undefined) {
        const msg = (result.details as { message?: string })?.message ?? "run/submit failed";
        ctx.reportSubmitFailure(`crew_spawn: run/submit failed: ${msg}`);
      }
      return result;
    },
  });
}

// ---------------------------------------------------------------- crew_send

export function registerSendTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["send"]).describe("Send a steering message to a run."),
    runId: pi.zod.string().describe("The run to steer."),
    senderWorkerId: pi.zod.string().optional().describe("The sending worker id."),
    taskId: pi.zod.string().optional().describe("The task this message relates to."),
    kind: pi.zod.string().optional().describe("Coordination kind; defaults to 'followUp'."),
    payload: pi.zod.string().describe("The message payload."),
    recipientWorkerId: pi.zod.string().optional().describe("Optional recipient worker id."),
    replyTo: pi.zod.string().optional().describe("Optional id of a prior message this replies to."),
  });

  pi.registerTool({
    name: CREW_SEND_TOOL_NAME,
    label: "Crew Send",
    description: "Use to steer or follow up with a running worker (e.g. after a milestone digest names a next step, or a budget refusal instructs escalation). Mirrors crew_message send; budget refusals surface verbatim as tool errors.",
    parameters: params,
    approval: "write",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx): Promise<AgentToolResult<unknown>> {
      const client = await ctx.getClient(extCtx);
      return callOrchestration(client, "message/send", {
        runId: input.runId,
        senderWorkerId: input.senderWorkerId,
        taskId: input.taskId,
        kind: input.kind ?? "followUp",
        payload: input.payload,
        recipientWorkerId: input.recipientWorkerId,
        replyTo: input.replyTo,
      });
    },
  });
}

// -------------------------------------------------------------- crew_status

export function registerStatusTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["snapshot"]).describe("Snapshot the orchestration state for a task."),
    taskId: pi.zod.string().optional().describe("Optional task filter; omit for all runs."),
  });

  pi.registerTool({
    name: CREW_STATUS_TOOL_NAME,
    label: "Crew Status",
    description: "Use to read the current run list for a task (or all tasks) as a situation snapshot (the base run/list projection).",
    parameters: params,
    approval: "read",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx): Promise<AgentToolResult<unknown>> {
      const client = await ctx.getClient(extCtx);
      return callOrchestration(client, "run/list", { taskId: input.taskId });
    },
  });
}

// ------------------------------------------------------------ crew_transcript

export function registerTranscriptTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["replay"]).describe("Replay a run's events as normalized digests."),
    runId: pi.zod.string().describe("The run whose events to replay."),
    fromSequence: pi.zod.number().int().optional().describe("Optional starting sequence (default 0)."),
    limit: pi.zod.number().int().optional().describe("Optional max number of events to return."),
  });

  pi.registerTool({
    name: CREW_TRANSCRIPT_TOOL_NAME,
    label: "Crew Transcript",
    description: "Use to review a run's event history as normalized digests (not raw payloads), filtered to a single run and paged by sequence. Surfaces the same timeline the monitor reduces from events/replay.",
    parameters: params,
    approval: "read",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx): Promise<AgentToolResult<unknown>> {
      const client = await ctx.getClient(extCtx);
      const replay = await callOrchestration(client, "events/replay", { afterSequence: input.fromSequence ?? 0 });
      if (replay.isError === true) return replay;
      const events = (replay.details as Array<{ sequence?: number; runId?: string; event?: { type?: string } }> | undefined) ?? [];
      const forRun = events
        .filter((e) => e.runId === input.runId)
        .map((e) => ({ sequence: e.sequence, type: e.event?.type ?? "unknown" }))
        .slice(0, input.limit ?? events.length);
      return {
        content: [{ type: "text", text: `crew_transcript: ${forRun.length} event(s) for run ${input.runId}.` }],
        details: { runId: input.runId, events: forRun },
      };
    },
  });
}

// ----------------------------------------------------------------- crew_stop

export function registerStopTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["stop"]).describe("Stop a running worker."),
    runId: pi.zod.string().describe("The run to stop."),
    outcome: pi.zod.enum(["done", "abort"]).describe("'done' = graceful wrap-up then soft cancel; 'abort' = immediate cancel."),
  });

  pi.registerTool({
    name: CREW_STOP_TOOL_NAME,
    label: "Crew Stop",
    description: "Use to stop a worker. outcome 'done' sends a wrap-up follow-up then cancels softly (the worker finishes its current turn); outcome 'abort' cancels the run immediately. Cleanup (workspace release) is never automatic -- the leader does that.",
    parameters: params,
    approval: "exec",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx): Promise<AgentToolResult<unknown>> {
      const client = await ctx.getClient(extCtx);
      if (input.outcome === "abort") {
        return callOrchestration(client, "run/cancel", { runId: input.runId });
      }
      await callOrchestration(client, "message/send", {
        runId: input.runId,
        kind: "followUp",
        payload: "Wrap-up: stopping this run per leader instruction.",
      });
      return callOrchestration(client, "run/cancel", { runId: input.runId, mode: "soft" });
    },
  });
}

// --------------------------------------------------------------- crew_finish

export function registerFinishTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["finish"]).describe("Cancel the remaining live runs of a plan."),
    runIds: pi.zod.array(pi.zod.string()).describe("The run ids to cancel (the subtask runs you spawned from the plan)."),
  });

  pi.registerTool({
    name: CREW_FINISH_TOOL_NAME,
    label: "Crew Finish",
    description: "Use to cancel the remaining live runs of a plan once the leader is done. Takes the explicit run ids you spawned. Workspace release is left to the leader -- never automatic.",
    parameters: params,
    approval: "exec",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx): Promise<AgentToolResult<unknown>> {
      const client = await ctx.getClient(extCtx);
      const cancelled: string[] = [];
      const failed: Array<{ runId: string; error: unknown }> = [];
      for (const runId of input.runIds) {
        const result = await callOrchestration(client, "run/cancel", { runId });
        if (result.isError === true) {
          failed.push({ runId, error: result.details });
        } else {
          cancelled.push(runId);
        }
      }
      return {
        content: [{ type: "text", text: `crew_finish: cancelled ${cancelled.length}, failed ${failed.length}.` }],
        details: { cancelled, failed },
      };
    },
  });
}

/** Registers every team-leader tool against the extension API. */
export function registerLeaderTools(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  registerSpawnTool(pi, ctx);
  registerSendTool(pi, ctx);
  registerStatusTool(pi, ctx);
  registerTranscriptTool(pi, ctx);
  registerStopTool(pi, ctx);
  registerFinishTool(pi, ctx);
}
