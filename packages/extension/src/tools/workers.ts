// `crew_worker`: creates, lists, and fetches logical worker identities.
// `create` is tier `exec` -- it provisions a harness/profile identity that
// later runs execute against.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_WORKER_TOOL_NAME = "crew_worker";

export function registerWorkerTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["create", "list", "get"]).describe("Which worker operation to perform."),
    fingerprint: pi.zod.string().optional().describe("Legacy create field. Rejected (PROFILE_REQUIRED) for reserved adapters (claude, codex, copilot, ompRpc) -- use profileId instead."),
    adapter: pi.zod.string().optional().describe("Legacy create field. Rejected (PROFILE_REQUIRED) for reserved adapters -- use profileId instead."),
    model: pi.zod.string().optional().describe("Legacy create field. Rejected (PROFILE_REQUIRED) for reserved adapters -- use profileId instead."),
    profileId: pi.zod.string().optional().describe("Required for create with reserved adapters (claude, codex, copilot, ompRpc). Register a profile with crew_profile first, then pass its id here."),
    permissionEnvelope: pi.zod.record(pi.zod.string(), pi.zod.unknown()).optional(),
    parentWorkerId: pi.zod.string().optional().describe("Parent worker id, if spawned as a child."),
    workerId: pi.zod.string().optional().describe("Required for get: the worker id to fetch."),
  });

  pi.registerTool({
    name: CREW_WORKER_TOOL_NAME,
    label: "Crew Worker",
    description:
      "Use to find or provision external AI harness workers (Claude, Codex, Copilot, OMP-RPC) that execute tasks. Required flow: (1) crew_profile { adapter, model, startupOptions } to register a profile with mode:'tui', (2) crew_worker { op: 'create', profileId } to provision the worker, (3) crew_run { op: 'submit', workerId, prompt } to execute. Use op: 'list' to see available workers for a repository (call before submitting a run), op: 'get' to fetch details of a specific worker.",
    parameters: params,
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && args.op === "create" ? "exec" : "read"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "create":
          return callOrchestration(client, "worker/create", {
            fingerprint: input.fingerprint,
            adapter: input.adapter,
            model: input.model,
            profileId: input.profileId,
            permissionEnvelope: input.permissionEnvelope,
            parentWorkerId: input.parentWorkerId,
          });
        case "list":
          return callOrchestration(client, "worker/list", {});
        case "get":
          return callOrchestration(client, "worker/get", { workerId: input.workerId });
      }
    },
  });
}
