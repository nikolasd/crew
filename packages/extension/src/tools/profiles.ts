// `crew_profile`: registers a reusable worker profile (adapter, model,
// startup options, environment allowlist) that `crew_worker { op: 'create',
// profileId }` and `crew_run` resolve at run time. `register` is tier
// `exec` -- it persists a new profile row the runtime will trust for every
// future worker created against it.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { homedir } from "node:os";

import { CrewConfigError, persistConfiguredModel, resolveConfiguredModel } from "../crew-config";
import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_PROFILE_TOOL_NAME = "crew_profile";

/** The four reserved adapter kinds `crates/runtime/src/adapter/profile.rs`'s
 *  `AdapterKind::RESERVED_NAMES` also declares -- kept in sync manually,
 *  since this list isn't part of the generated protocol types. */
const RESERVED_ADAPTER_NAMES: readonly string[] = ["claude", "codex", "copilot", "ompRpc"];

/**
 * CREW-8 mode injection: adds `mode: "tui"` to `startupOptions[adapter]`
 * for a reserved adapter when the caller omitted a mode entirely --
 * headless is retired (CREW-7), so this is the friendly path to the same
 * `mode: "tui"` a caller would otherwise have to remember to spell out
 * every time. Never overrides an *explicit* mode (including an explicit
 * `"headless"`): the daemon's own typed rejection for that stays exactly
 * as loud as it is today, this only fills in an omission.
 */
export function injectTuiMode(adapter: string, startupOptions: Record<string, unknown>): Record<string, unknown> {
  if (!RESERVED_ADAPTER_NAMES.includes(adapter)) {
    return startupOptions;
  }
  const existing = (startupOptions[adapter] as Record<string, unknown> | undefined) ?? {};
  if ("mode" in existing) {
    return startupOptions;
  }
  return { ...startupOptions, [adapter]: { ...existing, mode: "tui" } };
}

export function registerProfileTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    adapter: pi.zod.string().describe("The adapter name this profile launches, e.g. claude, codex, copilot, ompRpc, terminalDegraded."),
    model: pi.zod
      .string()
      .optional()
      .describe(
        "The model identifier this profile uses. Optional: if omitted and no model is already configured for this adapter (in .omp/crew.json), registration is refused with a typed 'model-not-configured' error -- ask the user which model to use, then call crew_profile again with it. The first time a model is given explicitly for an adapter with none configured, it is persisted into the repository's .omp/crew.json for future sessions to reuse silently. crew_profile never overwrites an already-recorded model, and never silently ignores an explicit value that conflicts with one: passing a *different* model than the one already configured is refused with a typed 'model-conflict' error naming the stored value -- correct it by editing the repository's .omp/crew.json directly (/crew config path locates it; /crew config has no set/edit subcommand), never by passing a new value here. Passing the same value as already configured is a no-op success.",
      ),
    startupOptions: pi.zod
      .record(pi.zod.string(), pi.zod.unknown())
      .optional()
      .describe("Adapter-specific startup options, tagged by adapter kind, e.g. { claude: { mode: 'tui' } }. For a reserved adapter (claude, codex, copilot, ompRpc), an omitted mode is filled in as 'tui' automatically -- headless is retired. Other options depend on the adapter (see crew-orchestration skill)."),
    environmentAllowlist: pi.zod.array(pi.zod.string()).optional().describe("Environment variable names this profile's process is allowed to read."),
    permissionEnvelope: pi.zod.record(pi.zod.string(), pi.zod.unknown()).optional(),
  });

  pi.registerTool({
    name: CREW_PROFILE_TOOL_NAME,
    label: "Crew Profile",
    description:
      "Register a reusable worker profile (adapter, model, startup options, environment allowlist) before provisioning workers. Call this once per adapter/model combination, then pass the returned profileId to crew_worker { op: 'create', profileId }. model is optional -- if none is configured yet for this adapter, you'll get a typed error telling you to ask the user which model to use and call this again; that answer is remembered for future sessions. mode:'tui' is filled in automatically for reserved adapters when omitted. The profile-first flow (crew_profile → crew_worker → crew_run) replaces the legacy fingerprint/adapter/model pattern. Registration is permanent for the lifetime of the runtime's database; there is no update or delete operation, so register a new profile rather than mutating an existing one.",
    parameters: params,
    approval: () => "exec",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const home = homedir();
      let configuredModel: string | undefined;
      try {
        configuredModel = resolveConfiguredModel(home, extCtx.cwd, input.adapter);
      } catch (err) {
        if (err instanceof CrewConfigError) {
          return {
            content: [{ type: "text", text: `crew_profile: ${err.message}` }],
            details: { code: "config-invalid", path: err.path, message: err.message },
            isError: true,
          };
        }
        throw err;
      }

      // A hallucinating leader inventing a model name is CREW-8's original
      // symptom -- crew_profile must never let an explicit param silently
      // clobber (nor silently lose to) an already-persisted choice. An
      // explicit model that *conflicts* with the stored one is refused,
      // named, with the correction path spelled out; the *same* explicit
      // value as already stored is a no-op success (nothing to persist,
      // nothing to reject).
      if (input.model !== undefined && configuredModel !== undefined && input.model !== configuredModel) {
        return {
          content: [
            {
              type: "text",
              text: `model already configured as ${configuredModel} for adapter ${input.adapter} -- crew_profile never overwrites a stored model; edit the repository's .omp/crew.json directly to change it (/crew config path locates it).`,
            },
          ],
          details: { code: "model-conflict", adapter: input.adapter, configuredModel },
          isError: true,
        };
      }

      const model = input.model ?? configuredModel;
      if (model === undefined) {
        return {
          content: [
            {
              type: "text",
              text: `no model configured for adapter ${input.adapter} -- ask the user which model to use, then call crew_profile again with it; the answer will be persisted for future sessions.`,
            },
          ],
          details: { code: "model-not-configured", adapter: input.adapter },
          isError: true,
        };
      }

      const client = await ctx.getClient(extCtx);
      const startupOptions = injectTuiMode(input.adapter, input.startupOptions ?? {});
      const result = await callOrchestration(client, "profile/register", {
        adapter: input.adapter,
        model,
        startupOptions,
        environmentAllowlist: input.environmentAllowlist ?? [],
        permissionEnvelope: input.permissionEnvelope ?? {},
        source: "omp",
      });

      if (result.isError !== true && input.model !== undefined && configuredModel === undefined) {
        // Registration is already durable by this point -- a failure to
        // persist the model for next time (e.g. a malformed crew.json a
        // concurrent process left mid-edit) must never surface as a
        // failed crew_profile call; it's a missed convenience, not a
        // failed registration. Warn, don't throw or flip isError.
        try {
          persistConfiguredModel(extCtx.cwd, input.adapter, input.model);
        } catch (err) {
          const message = err instanceof CrewConfigError ? err.message : err instanceof Error ? err.message : String(err);
          result.content.push({ type: "text", text: `Warning: model was registered but not persisted for future sessions: ${message}` });
        }
      }

      return result;
    },
  });
}
