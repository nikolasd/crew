// `crew_approval`: lists pending approvals and records a human decision.
// The whole tool is gated at tier `exec` with `override: true` -- an
// approval decision is a user-facing safety action that must never
// auto-approve, even for the `list` op.
//
// `decide` checks the approval's `humanRequired` flag before trusting the
// caller-provided decision: when true and interactive UI is available, it
// shows the human approval dialog (see `../approval-ui`) and decides with
// the human's actual answer. When humanRequired is true but no UI is
// available, it returns an error without calling the server (fail-closed).
// A dialog timeout leaves the request pending rather than falling back to
// the model-provided decision.

import type { AgentToolResult, ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import { showApprovalDialog, type PendingApproval } from "../approval-ui";
import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_APPROVAL_TOOL_NAME = "crew_approval";

/** Fetches the pending approval matching `approvalId`, if still pending. */
async function findPendingApproval(client: { request(method: string, params?: unknown): Promise<unknown> }, approvalId: string): Promise<PendingApproval | undefined> {
  const result = await client.request("approval/list", {});
  if (typeof result !== "object" || result === null || !("approvals" in result)) {
    return undefined;
  }
  const approvals = (result as { approvals: unknown }).approvals;
  if (!Array.isArray(approvals)) {
    return undefined;
  }
  const match = approvals.find((entry): entry is Record<string, unknown> => typeof entry === "object" && entry !== null && (entry as Record<string, unknown>).approvalId === approvalId);
  if (match === undefined) {
    return undefined;
  }
  return {
    approvalId,
    action: typeof match.action === "string" ? match.action : "",
    arguments: match.arguments,
    policyReason: typeof match.policyReason === "string" ? match.policyReason : "",
    humanRequired: match.humanRequired === true,
  };
}

export function registerApprovalTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["list", "decide"]).describe("Which approval operation to perform."),
    runId: pi.zod.string().optional().describe("Optional run id filter for list."),
    approvalId: pi.zod.string().optional().describe("Required for decide: the approval request id."),
    decision: pi.zod.enum(["approve", "deny"]).optional().describe("Required for decide: approve or deny."),
    reason: pi.zod.string().optional().describe("Required for decide: the reason for this decision."),
  });

  pi.registerTool({
    name: CREW_APPROVAL_TOOL_NAME,
    label: "Crew Approval",
    description:
      "Use when a worker escalates a decision to human (e.g., for risky operations). The runtime shows a dialog; call this to list pending approvals (with human-in-the-loop flag) or decide with the human's approve/deny decision. The runtime enforces humanRequired flags -- never auto-approve, even for list. Use when a worker pauses execution waiting for human input.",
    parameters: params,
    approval: { tier: "exec", override: true, reason: "Approval decisions are a user-facing safety action." },
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx): Promise<AgentToolResult<unknown>> {
      const client = await ctx.getClient(extCtx);
      if (input.op !== "decide") {
        return callOrchestration(client, "approval/list", { runId: input.runId });
      }
      if (input.approvalId === undefined) {
        return callOrchestration(client, "approval/decide", {
          approvalId: input.approvalId,
          decision: input.decision,
          reason: input.reason,
          decidedBy: "model",
        });
      }

      const pending = await findPendingApproval(client, input.approvalId);
      if (pending?.humanRequired === true) {
        if (!extCtx.hasUI) {
          return {
            content: [{ type: "text", text: `Approval ${input.approvalId} requires a human decision and no interactive UI is available; it remains pending.` }],
            details: { approvalId: input.approvalId, outcome: "pending", reason: "humanRequiredWithoutUi" },
            isError: true,
          };
        }
        const human = await showApprovalDialog(extCtx.ui, pending);
        if (human === undefined) {
          return {
            content: [{ type: "text", text: `Approval dialog timed out; ${input.approvalId} remains pending.` }],
            details: { approvalId: input.approvalId, outcome: "pending" },
          };
        }
        return callOrchestration(client, "approval/decide", {
          approvalId: input.approvalId,
          decision: human.decision,
          reason: human.reason,
          decidedBy: "human",
        });
      }

      return callOrchestration(client, "approval/decide", {
        approvalId: input.approvalId,
        decision: input.decision,
        reason: input.reason,
        decidedBy: "model",
      });
    },
  });
}
