// `crew_profile`: registers a reusable worker profile (adapter, model,
// startup options, environment allowlist) that `crew_worker { op: 'create',
// profileId }` and `crew_run` resolve at run time. `register` is tier
// `exec` -- it persists a new profile row the runtime will trust for every
// future worker created against it.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_PROFILE_TOOL_NAME = "crew_profile";

export function registerProfileTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    adapter: pi.zod.string().describe("The adapter name this profile launches, e.g. claude, codex, copilot, ompRpc, terminalDegraded."),
    model: pi.zod.string().describe("The model identifier this profile uses."),
    startupOptions: pi.zod.record(pi.zod.string(), pi.zod.unknown()).describe("Adapter-specific startup options, tagged by adapter kind, e.g. { claude: { ... } } or { codex: { ... } }."),
    environmentAllowlist: pi.zod.array(pi.zod.string()).optional().describe("Environment variable names this profile's process is allowed to read."),
    permissionEnvelope: pi.zod.record(pi.zod.string(), pi.zod.unknown()).optional(),
  });

  pi.registerTool({
    name: CREW_PROFILE_TOOL_NAME,
    label: "Crew Profile",
    description:
      "Use to register a reusable worker profile (adapter, model, startup options, environment allowlist) before provisioning workers against it. Call this once per adapter/model combination, then pass the returned profileId to crew_worker { op: 'create', profileId } instead of repeating fingerprint/adapter/model/permissionEnvelope on every worker. Registration is permanent for the lifetime of the runtime's database; there is no update or delete operation, so register a new profile rather than mutating an existing one.",
    parameters: params,
    approval: () => "exec",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      return callOrchestration(client, "profile/register", {
        adapter: input.adapter,
        model: input.model,
        startupOptions: input.startupOptions,
        environmentAllowlist: input.environmentAllowlist ?? [],
        permissionEnvelope: input.permissionEnvelope ?? {},
        source: "omp",
      });
    },
  });
}
