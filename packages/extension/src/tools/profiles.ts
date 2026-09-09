// `crew_profile`: registers a reusable worker profile (adapter, model,
// startup options, environment allowlist) that `crew_worker { op: 'create',
// profileId }` and `crew_run` resolve at run time. `register` is tier
// `exec` -- it persists a new profile row the runtime will trust for every
// future worker created against it.

import type { AgentToolResult, ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import { homedir } from "node:os";

import { CrewConfigError, persistConfiguredModel, resolveConfiguredModel } from "../crew-config";
import { currentModels, decideModel, isCataloguedAdapter, readCatalogue, resolveModelName } from "../models";
import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_PROFILE_TOOL_NAME = "crew_profile";

/** How long the model-ask dialog waits for a pick before timing out, same
 *  window `plan.ts`'s and `approval-ui.ts`'s dialogs use. */
const MODEL_DIALOG_TIMEOUT_MS = 5 * 60 * 1000;

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

/**
 * The refusal for every "no answer exists, and none can be obtained" path.
 * One `code` throughout (`model-not-configured`) since a caller's retry
 * logic only needs to know registration didn't happen; `reason` in
 * `details` and the message text say WHY, since that differs by adapter
 * and by whether crew could even try to ask.
 */
function modelNotConfiguredResult(adapter: string, reason: "no-model-given" | "no-ui" | "no-current-models" | "dialog-timeout", text: string): AgentToolResult<unknown> {
  return {
    content: [{ type: "text", text }],
    details: { code: "model-not-configured", adapter, reason },
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
        "A suggested model identifier, not a decision. Once a model is already configured for this adapter (in .omp/crew.json), it is authoritative: this parameter is only compared to it (a genuine mismatch is refused with a typed 'model-conflict' error naming the stored value; the same model either way is a no-op success), so pass the previously-registered value or omit it. Before any model is configured, for claude, codex and copilot, crew itself asks the user which model to use via an interactive dialog -- it does not accept this value directly, however it is spelled. Passing a value here still helps: if it resolves against omp's catalogue or the vendor's own aliases ('opus', 'haiku', a name matching exactly one catalogue entry), it is preselected in the dialog, but the user's own pick is what gets used and persisted. If no interactive UI is attached, or the dialog times out with no answer, or there is nothing to offer (no catalogue entry and no vendor alias table for this adapter), registration is refused with a typed 'model-not-configured' error -- register a profile from an interactive session once, or write the model into the repository's .omp/crew.json directly (/crew config path locates it). ompRpc and other adapters omp does not catalogue have no dialog to offer either way: pass the model explicitly, or the same 'model-not-configured' error applies.",
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
      "Register a reusable worker profile (adapter, model, startup options, environment allowlist) before provisioning workers. Call this once per adapter/model combination, then pass the returned profileId to crew_worker { op: 'create', profileId }. model is optional and, for claude, codex and copilot, is never accepted as a decision on the first call for an adapter: with no model configured yet, crew opens an interactive dialog and asks the user itself, offering the current models (omp's catalogue, or the vendor's own model family when omp has no catalogue entry for it) with your suggestion (if any) preselected -- the user's pick is what gets used and remembered, not your suggestion. A typed 'model-not-configured' error means there was no way to ask (no interactive UI, the dialog timed out, or nothing to offer) or, for ompRpc and adapters omp does not catalogue, that no model was given at all. Once a model IS configured for an adapter, it is reused silently forever; a value you pass is only compared to it (mismatch: typed 'model-conflict', naming the stored value; match: no-op success). mode:'tui' is filled in automatically for reserved adapters when omitted. The profile-first flow (crew_profile → crew_worker → crew_run) replaces the legacy fingerprint/adapter/model pattern. Registration is permanent for the lifetime of the runtime's database; there is no update or delete operation, so register a new profile rather than mutating an existing one.",
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

      // The ask is driven by whether a user answer already exists
      // (`configuredModel`), never by whether the leader supplied one. A
      // hallucinating leader inventing a model name was the original
      // symptom, and a leader-supplied model silently bypassing the ask
      // when one is due was a second instance of the same shape of bug --
      // both are closed by making the STORED answer, not the caller's
      // input, the only thing that skips asking.
      let model: string;
      let note: string | undefined;
      // Whether persistence is allowed to write this model down. Withholding
      // it is only justified where a check was available and came back
      // negative, which is why the ompRpc/dialog paths below leave it true:
      // an ompRpc model can't be checked at all, and a dialog pick is by
      // construction a member of a list something already verified.
      let mayPersist: boolean;

      if (configuredModel !== undefined) {
        // A user answer already exists. Unchanged from before the ask
        // existed: an explicit, catalogued input is resolved and compared
        // to the stored value (`decideModel` owns the comparison and the
        // ambiguity rule together -- see its doc comment for why each
        // ordering is load-bearing); anything else reuses the stored text
        // exactly as recorded.
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
          if (input.model !== undefined && input.model !== configuredModel) {
            return modelConflictResult(input.adapter, configuredModel);
          }
          // A model taken from `.omp/crew.json` is used exactly as recorded. It
          // is already the repository's answer, and re-resolving it could turn
          // a stored value that has always worked into an ambiguity error on a
          // call that passed no model at all.
          model = configuredModel;
          note = undefined;
          mayPersist = true;
        }
      } else if (!isCataloguedAdapter(input.adapter)) {
        // `ompRpc` and any caller-defined adapter have no catalogue and no
        // alias source, so there is no bounded list crew could offer a
        // dialog against -- resolution, and the ask, could only annotate a
        // name nothing checked. Unchanged from before the ask existed: use
        // whatever was given, or fail closed if nothing was.
        if (input.model === undefined) {
          return modelNotConfiguredResult(input.adapter, "no-model-given", `no model configured for adapter ${input.adapter} -- ask the user which model to use, then call crew_profile again with it; the answer will be persisted for future sessions.`);
        }
        model = input.model;
        note = undefined;
        mayPersist = true;
      } else if (!extCtx.hasUI) {
        // A catalogued adapter with no configured model needs a user
        // answer, and no interactive UI is attached to collect one. Fails
        // closed rather than accepting the leader's suggestion as though it
        // were that answer -- that silent accept was the bug.
        return modelNotConfiguredResult(input.adapter, "no-ui", `no model configured for adapter ${input.adapter}, and no interactive UI is attached to ask which one to use -- register a profile from an interactive session once, or set the model in the repository's .omp/crew.json directly.`);
      } else {
        // The ask: crew opens the dialog itself rather than instructing the
        // leader to ask and call back. A leader-supplied `input.model` never
        // gets accepted on its own say-so here -- it only preselects a row
        // when it resolves to one of the offered models, so the human's own
        // pick is what is actually used.
        const catalogue = await (ctx.readModelCatalogue ?? readCatalogue)(input.adapter);
        const options = currentModels(input.adapter, catalogue);
        if (!options.available) {
          return modelNotConfiguredResult(input.adapter, "no-current-models", `no model configured for adapter ${input.adapter}, and there is nothing to offer -- omp's catalogue has no entry for it and it has no reviewed vendor model-family table either.`);
        }
        const sourceLabel = options.source === "catalogue" ? "omp's model catalogue" : "the vendor's own model family table (omp's catalogue has no entry for this adapter)";
        let initialIndex: number | undefined;
        if (input.model !== undefined) {
          const suggested = resolveModelName(input.adapter, input.model, catalogue);
          const suggestedModel = suggested.kind === "ambiguous" || suggested.kind === "unverified" ? undefined : suggested.model;
          const idx = suggestedModel === undefined ? -1 : options.models.indexOf(suggestedModel);
          if (idx >= 0) {
            initialIndex = idx;
          }
        }
        const pick = await extCtx.ui.select(`Model for the ${input.adapter} worker (from ${sourceLabel})`, [...options.models], {
          timeout: MODEL_DIALOG_TIMEOUT_MS,
          initialIndex,
        });
        if (pick === undefined) {
          return modelNotConfiguredResult(input.adapter, "dialog-timeout", `no model configured for adapter ${input.adapter}, and the model-selection dialog timed out with no answer.`);
        }
        model = pick;
        note = `model: ${model} (chosen from ${sourceLabel})`;
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

      // `configuredModel === undefined` alone is the right gate now: every
      // path that reaches here with it undefined either resolved an
      // explicit `input.model` (the ompRpc/custom-adapter fallback) or
      // collected a dialog pick, and both are exactly the "first time this
      // adapter gets an answer" cases persistence exists for.
      if (result.isError !== true && configuredModel === undefined) {
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
