// Registers every deterministic Crew orchestration tool: `crew_task`,
// `crew_worker`, `crew_profile`, `crew_run`, `crew_workspace`,
// `crew_artifact`, `crew_child`, `crew_violation`, `crew_message`,
// `crew_approval`, and `crew_reconcile`. Each tool is a thin validated
// adapter over the runtime's JSON-RPC methods -- no worker selection,
// retry, merge, or lifecycle inference happens in TypeScript.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import { registerApprovalTool } from "./approvals";
import { registerArtifactTool } from "./artifacts";
import { registerChildTool } from "./children";
import { registerMessageTool } from "./messages";
import { registerProfileTool } from "./profiles";
import { registerReconcileTool } from "./reconcile";
import { registerRunTool } from "./runs";
import { registerTaskTool } from "./tasks";
import { registerViolationTool } from "./violations";
import { registerWorkerTool } from "./workers";
import { registerWorkspaceTool } from "./workspaces";
import type { OrchestrationToolContext } from "./shared";

export type { OrchestrationToolContext } from "./shared";
export { CREW_TASK_TOOL_NAME } from "./tasks";
export { CREW_WORKER_TOOL_NAME } from "./workers";
export { CREW_RUN_TOOL_NAME } from "./runs";
export { CREW_MESSAGE_TOOL_NAME } from "./messages";
export { CREW_APPROVAL_TOOL_NAME } from "./approvals";
export { CREW_RECONCILE_TOOL_NAME } from "./reconcile";
export { CREW_PROFILE_TOOL_NAME } from "./profiles";
export { CREW_WORKSPACE_TOOL_NAME } from "./workspaces";
export { CREW_ARTIFACT_TOOL_NAME } from "./artifacts";
export { CREW_CHILD_TOOL_NAME } from "./children";
export { CREW_VIOLATION_TOOL_NAME } from "./violations";

/** Registers every orchestration tool against the extension API. */
export function registerOrchestrationTools(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  // Registration order is the order the model sees these tools in, and is
  // asserted verbatim by `tools.test.ts`: identity, then execution, then
  // the evidence and decision surfaces, then messaging.
  registerTaskTool(pi, ctx);
  registerWorkerTool(pi, ctx);
  registerProfileTool(pi, ctx);
  registerRunTool(pi, ctx);
  registerWorkspaceTool(pi, ctx);
  registerArtifactTool(pi, ctx);
  registerChildTool(pi, ctx);
  registerViolationTool(pi, ctx);
  registerMessageTool(pi, ctx);
  registerApprovalTool(pi, ctx);
  registerReconcileTool(pi, ctx);
}
