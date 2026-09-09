// `crew_profile`: registers a reusable worker profile (adapter, model,
// startup options, environment allowlist) that `crew_worker { op: 'create',
// profileId }` and `crew_run` resolve at run time. `register` is tier
// `exec` -- it persists a new profile row the runtime will trust for every
// future worker created against it.

import type { AgentToolResult, ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { homedir } from "node:os";

import { CrewConfigError, persistConfiguredModel, resolveConfiguredModel } from "../crew-config";
import { decideModel, isCataloguedAdapter, readCatalogue } from "../models";
import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_PROFILE_TOOL_NAME = "crew_profile";

/** The four reserved adapter kinds `crates/runtime/src/adapter/profile.rs`'s
 *  `AdapterKind::RESERVED_NAMES` also declares -- kept in sync manually,
 *  since this list isn't part of the generated protocol types. */
const RESERVED_ADAPTER_NAMES: readonly string[] = ["claude", "codex", "copilot", "ompRpc"];

/**
 * Mode injection: adds `mode: "tui"` to `startupOptions[adapter]`
 * for a reserved adapter when the caller omitted a mode entirely --
 * headless is retired, so this is the friendly path to the same
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

/**
 * The stored-model refusal, shaped once because resolution gave it a second caller.
 * `configuredModel` is the RAW text from `.omp/crew.json`: the correction
 * path is to edit that file, so the error has to name what the reader will
 * find in it, not the canonical id it resolves to.
 */
function modelConflictResult(adapter: string, configuredModel: string): AgentToolResult<unknown> {
  return {
    content: [
      {
        type: "text",
        text: `model already configured as ${configuredModel} for adapter ${adapter} -- crew_profile never overwrites a stored model; edit the repository's .omp/crew.json directly to change it (/crew config path locates it).`,
      },
    ],
    details: { code: "model-conflict", adapter, configuredModel },
    isError: true,
  };
}

export function registerProfileTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    adapter: pi.zod.string().describe("The adapter name this profile launches, e.g. claude, codex, copilot, ompRpc, terminalDegraded."),
    model: pi.zod
      .string()
      .optional()
      .describe(
        "The model identifier this profile uses. Optional: if omitted and no model is already configured for this adapter (in .omp/crew.json), registration is refused with a typed 'model-not-configured' error -- ask the user which model to use, then call crew_profile again with it. For claude, codex and copilot the value you pass is resolved before anything else happens, against omp's own model catalogue and the vendor's aliases: a vendor alias ('opus', 'haiku') and a name matching exactly one model ('sol' -> gpt-5.6-sol) both resolve to the canonical id, and the result is reported back to you. A name matching several models is refused with a typed 'model-ambiguous' error listing them -- name one of them exactly. A name omp's catalogue does not know is still used, but is reported as UNVERIFIED and is NOT recorded in .omp/crew.json: pass it again next session, or record it there yourself once a run has proven it. The first time a confirmed model is given explicitly for an adapter with none configured, its canonical id is persisted into the repository's .omp/crew.json for future sessions to reuse silently. crew_profile never overwrites an already-recorded model, and never silently ignores an explicit value that conflicts with one: passing a value naming a *different* model than the one already configured is refused with a typed 'model-conflict' error naming the stored value -- correct it by editing the repository's .omp/crew.json directly (/crew config path locates it; /crew config has no set/edit subcommand), never by passing a new value here. Passing a value naming the same model as the configured one is a no-op success, whichever of the two is the shorthand.",
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
      "Register a reusable worker profile (adapter, model, startup options, environment allowlist) before provisioning workers. Call this once per adapter/model combination, then pass the returned profileId to crew_worker { op: 'create', profileId }. model is optional -- if none is configured yet for this adapter, you'll get a typed error telling you to ask the user which model to use and call this again; that answer is remembered for future sessions. For claude, codex and copilot, model names are resolved against omp's catalogue and the vendor's aliases, so a shorthand or a unique partial name is accepted and echoed back as the canonical id; a name the catalogue does not know still runs, but is flagged UNVERIFIED and is not remembered. mode:'tui' is filled in automatically for reserved adapters when omitted. The profile-first flow (crew_profile → crew_worker → crew_run) replaces the legacy fingerprint/adapter/model pattern. Registration is permanent for the lifetime of the runtime's database; there is no update or delete operation, so register a new profile rather than mutating an existing one.",
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

      // A hallucinating leader inventing a model name is the original
      // symptom -- crew_profile must never let an explicit param silently
      // clobber (nor silently lose to) an already-persisted choice. An
      // explicit model that *conflicts* with the stored one is refused,
      // named, with the correction path spelled out; the *same* explicit
      // value as already stored is a no-op success (nothing to persist,
      // nothing to reject).
      //
      // Resolution goes in front of that comparison, for the
      // adapters omp catalogues. `haiku` and a stored `claude-haiku-4-5` are
      // one model, so comparing the spellings would refuse a correct call
      // with an error telling the user to edit a file that is already right.
      // `decideModel` owns the comparison and the ambiguity rule together --
      // see its doc comment for why each ordering is load-bearing.
      let model: string;
      let note: string | undefined;
      // Whether persistence is allowed to write this model down. Withholding
      // it is only justified where a check was available and came back
      // negative, which is why the non-catalogued path below leaves it true.
      let mayPersist: boolean;

      if (input.model !== undefined && isCataloguedAdapter(input.adapter)) {
        const catalogue = await (ctx.readModelCatalogue ?? readCatalogue)(input.adapter);
        const decision = decideModel(input.adapter, input.model, configuredModel, catalogue);
        if (decision.kind === "conflict") {
          return modelConflictResult(input.adapter, decision.configuredModel);
        }
        if (decision.kind === "ambiguous") {
          return {
            content: [
              {
                type: "text",
                text: `"${decision.from}" matches ${decision.candidates.length} models for adapter ${input.adapter}: ${decision.candidates.join(", ")} -- name one of them exactly.`,
              },
            ],
            details: { code: "model-ambiguous", adapter: input.adapter, requested: decision.from, candidates: decision.candidates },
            isError: true,
          };
        }
        model = decision.model;
        note = decision.note;
        mayPersist = decision.verified;
      } else {
        // No catalogue and no alias source for this adapter (`ompRpc`, or a
        // caller-defined one), or no explicit model to resolve: behaviour
        // here is exactly what it was before resolution existed.
        if (input.model !== undefined && configuredModel !== undefined && input.model !== configuredModel) {
          return modelConflictResult(input.adapter, configuredModel);
        }
        const chosen = input.model ?? configuredModel;
        if (chosen === undefined) {
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
        // A model taken from `.omp/crew.json` is used exactly as recorded. It
        // is already the repository's answer, and re-resolving it could turn
        // a stored value that has always worked into an ambiguity error on a
        // call that passed no model at all.
        model = chosen;
        note = undefined;
        mayPersist = true;
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

      // a resolution is never silent. Whatever the input spelling
      // was, the caller is told what it became -- or that nothing could
      // confirm it.
      if (result.isError !== true && note !== undefined) {
        result.content.push({ type: "text", text: note });
      }

      if (result.isError !== true && input.model !== undefined && configuredModel === undefined) {
        if (!mayPersist) {
          // The failure resolution exists for: an invented dated id became the
          // repository's durable answer in `.omp/crew.json`. A name omp's
          // catalogue does not know is exactly that value, so it may run
          // this once and is not written down.
          result.content.push({
            type: "text",
            text: `Not persisted to .omp/crew.json: nothing could confirm ${model} is a real model for adapter ${input.adapter}. Pass it again next session, or record it in .omp/crew.json yourself once a run has proven it.`,
          });
        } else {
          // Registration is already durable by this point -- a failure to
          // persist the model for next time (e.g. a malformed crew.json a
          // concurrent process left mid-edit) must never surface as a
          // failed crew_profile call; it's a missed convenience, not a
          // failed registration. Warn, don't throw or flip isError.
          //
          // The *resolved* id is what gets persisted, not the spelling the
          // caller typed: claude's config carries an `alias_migration` map,
          // so the vendor anticipates renaming aliases, and a persisted
          // alias is a durable value whose meaning can move underneath the
          // repository.
          try {
            persistConfiguredModel(extCtx.cwd, input.adapter, model);
          } catch (err) {
            const message = err instanceof CrewConfigError ? err.message : err instanceof Error ? err.message : String(err);
            result.content.push({ type: "text", text: `Warning: model was registered but not persisted for future sessions: ${message}` });
          }
        }
      }

      return result;
    },
  });
}
