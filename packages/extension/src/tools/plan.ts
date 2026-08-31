// `crew_plan`: proposes a leader's decomposition of a run into subtasks and
// reads a previously proposed plan. `propose` persists intent via
// `plan/propose`, then runs the approval gate (config-driven human gate,
// fail-closed without UI) before `plan/decide`. `get` is a pure read of
// `plan/get`.
//
// The daemon stores and enforces nothing about *routing* -- OMP owns the task
// graph. A plan is persisted leader intent plus `writes`/`turn_budget` metadata
// for budget tracking and approval gates.

import type { AgentToolResult, ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { existsSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_PLAN_TOOL_NAME = "crew_plan";

type ApprovalMode = "always" | "never" | "auto";

interface SubtaskProposal {
  id: string;
  description: string;
  adapter: string;
  writes?: boolean;
  turnBudget?: number;
}

const APPROVAL_DIALOG_TIMEOUT_MS = 5 * 60 * 1000;

/**
 * Resolves the effective approval mode from the crew config layers
 * (`~/.omp/crew.json` then `<repo>/.omp/crew.json`, repo wins). Defaults to
 * `"auto"` when no layer sets it. A malformed layer degrades to the default
 * rather than inventing a rejection -- the daemon fails launch on it anyway.
 */
export function resolveApprovalMode(repoCwd: string): ApprovalMode {
  const candidates = [join(homedir(), ".omp", "crew.json"), join(repoCwd, ".omp", "crew.json")];
  let mode: ApprovalMode | undefined;
  for (const path of candidates) {
    if (!existsSync(path)) continue;
    try {
      const parsed = JSON.parse(readFileSync(path, "utf8")) as { approval?: ApprovalMode };
      if (parsed.approval === "always" || parsed.approval === "never" || parsed.approval === "auto") {
        mode = parsed.approval; // later candidate (repo) wins a later-layer-wins merge
      }
    } catch {
      // Ignore unreadable/malformed layer; fall through to the default.
    }
  }
  return mode ?? "auto";
}

/** Whether the plan needs a human decision before it is approved. */
function needsHumanGate(mode: ApprovalMode, subtasks: SubtaskProposal[]): boolean {
  if (mode === "always") return true;
  if (mode === "auto") return subtasks.some((s) => s.writes === true);
  return false; // "never"
}

export function registerPlanTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["propose", "get"]).describe("Which plan operation to perform: propose | get."),
    runId: pi.zod.string().describe("The run this plan is proposed for or read from (required for both ops)."),
    taskText: pi.zod.string().optional().describe("Required for propose: the leader's task description."),
    subtasks: pi.zod
      .array(
        pi.zod.object({
          id: pi.zod.string().describe("Stable subtask id, referenced by crew_spawn."),
          description: pi.zod.string().describe("The instruction this subtask executes."),
          adapter: pi.zod.string().describe("The adapter (claude, codex, copilot, ompNative) that executes this subtask."),
          writes: pi.zod.boolean().optional().describe("Whether this subtask may write to the repository (drives the approval gate)."),
          turnBudget: pi.zod.number().int().optional().describe("Optional per-subtask turn budget, snapshotted into the run's budget row."),
        }),
      )
      .optional()
      .describe("Required for propose: the decomposition."),
  });

  pi.registerTool({
    name: CREW_PLAN_TOOL_NAME,
    label: "Crew Plan",
    description:
      "Use to decompose a run into subtasks the leader will spawn with crew_spawn. Use op 'propose' to persist a plan (ownerClientInstanceId is the connected instance) and run the approval gate (human decision required when config approval=always, or approval=auto with any writes:true subtask); op 'get' to read a previously proposed plan and its decision. A plan is leader intent, not a task graph -- OMP owns routing.",
    parameters: params,
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && (args as { op: string }).op === "propose" ? "exec" : "read"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx): Promise<AgentToolResult<unknown>> {
      const client = await ctx.getClient(extCtx);
      if (input.op === "get") {
        return callOrchestration(client, "plan/get", { runId: input.runId });
      }

      const subtasks = (input.subtasks ?? []) as SubtaskProposal[];
      const propose = await callOrchestration(client, "plan/propose", {
        runId: input.runId,
        ownerClientInstanceId: extCtx.sessionManager.getSessionId(),
        taskText: input.taskText ?? "",
        plan: { subtasks },
      });
      if (propose.isError === true) {
        return propose;
      }

      const mode = resolveApprovalMode(extCtx.cwd);
      if (!needsHumanGate(mode, subtasks)) {
        return callOrchestration(client, "plan/decide", {
          runId: input.runId,
          approved: true,
          decidedBy: "model",
        });
      }

      if (!extCtx.hasUI) {
        return {
          content: [
            {
              type: "text",
              text: `Plan ${input.runId} requires a human approval decision and no interactive UI is available; it remains proposed.`,
            },
          ],
          details: { runId: input.runId, outcome: "proposed", reason: "humanRequiredWithoutUi" },
          isError: true,
        };
      }

      const rendered = `Plan for run ${input.runId}:\n` + subtasks.map((s) => `- [${s.id}] ${s.description} (${s.adapter}${s.writes ? ", writes" : ""})`).join("\n");
      extCtx.ui.notify(rendered, "info");
      const selection = await extCtx.ui.select(`Approve plan ${input.runId}?`, ["Approve", "Reject"], {
        timeout: APPROVAL_DIALOG_TIMEOUT_MS,
      });
      if (selection === undefined) {
        return {
          content: [{ type: "text", text: `Plan ${input.runId} approval dialog timed out; it remains proposed.` }],
          details: { runId: input.runId, outcome: "proposed" },
        };
      }
      const approved = selection === "Approve";
      const reason = await extCtx.ui.input(approved ? "Reason for approving" : "Reason for rejecting", "", {
        timeout: APPROVAL_DIALOG_TIMEOUT_MS,
      });
      if (reason === undefined) {
        return {
          content: [{ type: "text", text: `Plan ${input.runId} approval dialog timed out; it remains proposed.` }],
          details: { runId: input.runId, outcome: "proposed" },
        };
      }
      return callOrchestration(client, "plan/decide", {
        runId: input.runId,
        approved,
        reason,
        decidedBy: "human",
      });
    },
  });
}
