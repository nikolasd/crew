// Model-name resolution for `crew_profile`.
//
// The problem this exists for: a leader invents a plausible-looking dated
// model id, `crew_profile` accepts it because nothing validates model
// names, and persistence writes it to `.omp/crew.json` where it
// becomes the repository's answer. That happened during a live E2E.
//
// Two separate jobs, and it took a scoping pass to see they are separate:
//
//   VALIDATION comes from omp's own model catalogue (`omp models --json`),
//   which crew can read for free because crew runs INSIDE omp. It lists
//   canonical ids per provider -- 725 models across 7 providers on the
//   machine this was written on, three of which are crew's adapters.
//
//   ALIAS RESOLUTION cannot come from the catalogue, because the catalogue
//   holds canonical ids and display names and never aliases: `openai-codex`
//   has `gpt-5.6-sol` and no bare `sol`; `anthropic` has `claude-haiku-4-5`
//   and no bare `haiku`. Aliases live in the vendor CLIs, so they need a
//   small table -- but every entry's TARGET is checkable against the
//   catalogue, which is what keeps the table from rotting silently.
//
// The rule throughout: resolve, but never silently. A name that resolves
// says what it resolved to; a name that does not is passed through with a
// visible note, never quietly accepted as though it had been verified.

import { execFile } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

/** Crew's reserved adapter kinds, as `profiles.ts` uses them. */
export type Adapter = "claude" | "codex" | "copilot" | "ompRpc";

/**
 * Which omp catalogue provider backs each adapter. Derived by reading the
 * catalogue rather than assumed: `omp models ls --json` groups by provider,
 * and these three names are the ones whose model sets correspond to crew's
 * vendor CLIs.
 *
 * `ompRpc` is deliberately absent. It talks to omp itself rather than to one
 * vendor, so no single provider bounds its valid models and scoping a search
 * to one would give confidently wrong answers.
 */
const PROVIDER_FOR_ADAPTER: Partial<Record<Adapter, string>> = {
  claude: "anthropic",
  codex: "openai-codex",
  copilot: "github-copilot",
};

/**
 * Vendor-defined aliases, per adapter. Small on purpose -- the catalogue
 * does the validation work, so this table only carries shorthands the
 * VENDOR defines, and only where the catalogue cannot resolve them itself.
 *
 * **The vendor's binary is the authority here, not its `--help` text.**
 * `claude --help` offers "`fable`, `opus`, or `sonnet`" as examples, which
 * is an illustration and not the set: the binary's own config object
 * carries a fourth, `haiku`. A table built from the documentation silently
 * omits a working alias. Whoever updates this next should read
 * `latest_per_family` out of the installed binary:
 *
 *   defaults:{}, best:"fable",
 *   latest_per_family:{ fable:"claude-fable-5-1", opus:"claude-opus-5",
 *                       sonnet:"claude-sonnet-5", haiku:"claude-haiku-4-5" },
 *   alias_migration:{}
 *
 * That object also carries an `alias_migration` map, currently empty --
 * so the vendor anticipates renaming aliases. When it becomes non-empty,
 * this table should READ it rather than being hand-patched around a rename.
 *
 * codex's single entry is its own bundled documentation, verbatim: "The
 * alias `gpt-5.6` routes to Sol." Note that bare `sol` is NOT an alias
 * anywhere -- it resolves through the catalogue instead (see
 * `resolveModelName`), which is why this table does not need an entry for
 * the shorthand that motivated the ticket.
 *
 * copilot has no entries because it defines no aliases we can source. Its
 * `--help` documents only `auto`, meaning "let Copilot pick". A crew-invented
 * shorthand for copilot would be a mapping we owned and could not verify,
 * so there is none.
 */
const VENDOR_ALIASES: Partial<Record<Adapter, Readonly<Record<string, string>>>> = {
  claude: {
    fable: "claude-fable-5-1",
    opus: "claude-opus-5",
    sonnet: "claude-sonnet-5",
    haiku: "claude-haiku-4-5",
  },
  codex: {
    "gpt-5.6": "gpt-5.6-sol",
  },
};

/** One provider's canonical model ids, as read from omp's catalogue. */
export type Catalogue = { readonly available: true; readonly ids: readonly string[] } | { readonly available: false; readonly why: string };

export type Resolution =
  /** Already a canonical id for this adapter's provider. */
  | { kind: "exact"; model: string }
  /** A vendor-defined alias. */
  | { kind: "alias"; model: string; from: string }
  /** The only model in this provider whose id contains the input. */
  | { kind: "match"; model: string; from: string }
  /** More than one candidate -- refuse rather than guess. */
  | { kind: "ambiguous"; from: string; candidates: readonly string[] }
  /** Passed through unverified, with the reason it could not be checked. */
  | { kind: "unverified"; model: string; why: "not-in-catalogue" | "catalogue-unavailable"; detail?: string };

/**
 * Reads one provider's model ids from omp's catalogue.
 *
 * Shell-out rather than an API call, and NOT for want of an API. omp does
 * expose a model facade to extensions, on the very context `execute`
 * receives -- `extCtx.models: ExtensionModelQuery`, with `list()`,
 * `current()`, `resolve(spec)` and `family(model)`, plus
 * `extCtx.modelRegistry`. (Read it from the published typings:
 * `npm view @oh-my-pi/pi-coding-agent dist.tarball`, then
 * `dist/types/extensibility/extensions/types.d.ts`. It is a peerDependency
 * omp provides at runtime, so it is not in `node_modules` here -- which is
 * not the same as unreadable, and an earlier version of this comment made
 * exactly that mistake.)
 *
 * It is the wrong tool for this particular question, for three reasons
 * taken from those typings:
 *
 *   1. `resolve()` breaks ambiguity by PREFERENCE. It is backed by
 *      `resolveModelFromString(value, available, matchPreferences)`, and
 *      `ModelMatchPreferences` carries `usageOrder` ("most-recently-used
 *      model keys to prefer when ambiguous"), `providerOrder` and
 *      `deprioritizeProviders`. So an ambiguous shorthand resolves to
 *      whatever the user last used -- a plausible answer, chosen silently.
 *      That is the failure this whole module exists to stop, so ambiguity
 *      has to come back as a refusal naming the candidates instead.
 *   2. `list()` is scoped to models OMP is authenticated for. Crew is not
 *      asking "can omp call this model", it is asking "will the `codex`
 *      binary accept this name" -- and that binary has its own auth and its
 *      own catalogue. `omp models ls` answers the second: it lists models
 *      this machine plainly cannot call right now (`lm-studio`), so it is a
 *      catalogue, not an entitlement list.
 *   3. `resolve()`'s aliases are omp's configured `modelRoles` (`@slow`,
 *      `advisor`, `task`), not the vendor's. Nothing in omp knows claude's
 *      `latest_per_family` or codex's `gpt-5.6` -> Sol, so
 *      `VENDOR_ALIASES` is needed either way.
 *
 * Related trap, since it touches the persistence rule in `decideModel`:
 * `family()`'s own doc says "compare it; do not persist it (the vocabulary
 * tracks new releases)". What gets persisted here is a catalogue id, not a
 * family token. Do not reach for `family()` to canonicalise for storage.
 *
 * This is still the one function to replace if that reasoning stops
 * holding -- e.g. if omp grows an unfiltered catalogue query, reason 2 goes
 * away.
 *
 * `ls` reads a local cached catalogue; only `refresh` is networked, and this
 * never refreshes. So a stale catalogue is possible, which is exactly why an
 * unrecognised name is passed through rather than refused.
 */
export async function readCatalogue(adapter: Adapter, run: (cmd: string, args: string[]) => Promise<string> = defaultRun): Promise<Catalogue> {
  const provider = PROVIDER_FOR_ADAPTER[adapter];
  if (provider === undefined) {
    return { available: false, why: `no single catalogue provider bounds the ${adapter} adapter` };
  }
  let raw: string;
  try {
    raw = await run("omp", ["models", "ls", "--json"]);
  } catch (err) {
    return { available: false, why: `could not run \`omp models\`: ${err instanceof Error ? err.message : String(err)}` };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { available: false, why: "`omp models --json` did not return JSON" };
  }
  const models = (parsed as { models?: unknown })?.models;
  if (!Array.isArray(models)) {
    return { available: false, why: "`omp models --json` has no `models` array -- its shape has changed" };
  }
  const ids = models
    .filter((m): m is { provider: string; id: string } => typeof (m as { provider?: unknown })?.provider === "string" && typeof (m as { id?: unknown })?.id === "string")
    .filter((m) => m.provider === provider)
    .map((m) => m.id);

  // An EMPTY provider list is the instrument failing, not the world: "no
  // models exist for codex" is never true. Treating it as a validation
  // result would turn a broken read into a confident answer about every
  // model name -- which is the empty-result-as-finding trap, and the reason
  // this branch is explicit rather than falling out of the code.
  if (ids.length === 0) {
    return { available: false, why: `omp's catalogue lists no models for provider \`${provider}\`` };
  }
  return { available: true, ids };
}

async function defaultRun(cmd: string, args: string[]): Promise<string> {
  const { stdout } = await execFileAsync(cmd, args, { maxBuffer: 8 * 1024 * 1024 });
  return stdout;
}

/**
 * Resolves `input` to a canonical model id for `adapter`.
 *
 * Order matters, and one step is load-bearing: **alias before substring.**
 * `haiku` scoped to `anthropic` matches three catalogue ids
 * (`claude-3-haiku-20240307`, `claude-haiku-4-5`, `claude-haiku-4-5-20251001`),
 * so a substring search alone would call it ambiguous and refuse a shorthand
 * the vendor defines unambiguously. The vendor's own answer has to win.
 *
 * Conversely `sol` needs no table entry: scoped to `openai-codex` it matches
 * exactly one id, which is how the shorthand that motivated this ticket
 * resolves without crew owning a mapping for it.
 */
export function resolveModelName(adapter: Adapter, input: string, catalogue: Catalogue): Resolution {
  const aliases = VENDOR_ALIASES[adapter] ?? {};

  if (catalogue.available && catalogue.ids.includes(input)) {
    return { kind: "exact", model: input };
  }

  const aliased = aliases[input];
  if (aliased !== undefined) {
    return { kind: "alias", model: aliased, from: input };
  }

  if (!catalogue.available) {
    return { kind: "unverified", model: input, why: "catalogue-unavailable", detail: catalogue.why };
  }

  const matches = catalogue.ids.filter((id) => id.includes(input));
  if (matches.length === 1) {
    return { kind: "match", model: matches[0]!, from: input };
  }
  if (matches.length > 1) {
    return { kind: "ambiguous", from: input, candidates: matches };
  }
  return { kind: "unverified", model: input, why: "not-in-catalogue" };
}

/**
 * The one-line note a user sees. A resolution is never silent, and an
 * unverified name never looks like an accepted one.
 *
 * `ambiguous` has no note because it is not a resolution -- the caller turns
 * it into a typed refusal naming the candidates.
 */
export function resolutionNote(adapter: Adapter, r: Resolution): string | undefined {
  switch (r.kind) {
    case "exact":
      return undefined;
    case "alias":
      return `model: ${r.model} (resolved "${r.from}" via ${adapter}'s own alias table)`;
    case "match":
      return `model: ${r.model} (resolved "${r.from}" -- the only ${PROVIDER_FOR_ADAPTER[adapter]} model matching it)`;
    case "ambiguous":
      return undefined;
    case "unverified":
      return r.why === "not-in-catalogue" ? `model: ${r.model} (not in omp's catalogue for ${PROVIDER_FOR_ADAPTER[adapter]}; passing through UNVERIFIED -- the vendor will reject it if it is wrong)` : `model: ${r.model} (could not be verified: ${r.detail ?? "catalogue unavailable"}; passing through UNVERIFIED)`;
  }
}

/**
 * Whether omp's catalogue bounds this adapter's models, i.e. whether
 * resolution can say anything true about a name given for it.
 *
 * `ompRpc` and any caller-defined adapter are excluded: they have no
 * provider and no alias source, so resolving them could only annotate a
 * name nothing checked. Excluding them keeps `crew_profile`'s behaviour for
 * those adapters identical to what it was before resolution existed.
 */
export function isCataloguedAdapter(adapter: string): adapter is Adapter {
  return Object.hasOwn(PROVIDER_FOR_ADAPTER, adapter);
}

/** Which source answered a `currentModels()` call. */
export type ModelListSource = "catalogue" | "vendorFamilyTable";

/** The list `crew_profile`'s model-ask dialog offers, and which source built it. */
export type CurrentModels = { readonly available: true; readonly models: readonly string[]; readonly source: ModelListSource } | { readonly available: false };

/**
 * The "current" models to offer for `adapter`, in the order the maintainer's
 * ruling defines: omp's own catalogue first (it needs no interpretation --
 * its ids are current by construction), the vendor's own family table when
 * the catalogue has nothing for this provider, and neither when both come up
 * empty.
 *
 * The family-table fallback exists because omp's catalogue is scoped to
 * providers it holds credentials for, which is a different question from
 * "does the vendor CLI accept this model" -- on a machine with no anthropic
 * credentials in omp, `readCatalogue("claude")` is unavailable even though
 * the claude CLI itself works fine and has its own opinion of what's
 * current. `VENDOR_ALIASES`' values are exactly that opinion for the
 * adapters it lists; deduplicated since more than one alias can name the
 * same canonical id (not true today, but the table makes no promise
 * against it).
 *
 * This is a fallback of last resort, not a second catalogue: `ompRpc` and
 * any caller-defined adapter have no entry in `VENDOR_ALIASES` at all, and
 * `copilot`'s is empty (no aliases it can source), so both correctly report
 * unavailable here when the catalogue is also down.
 */
export function currentModels(adapter: Adapter, catalogue: Catalogue): CurrentModels {
  if (catalogue.available) {
    return { available: true, models: catalogue.ids, source: "catalogue" };
  }
  const aliases = VENDOR_ALIASES[adapter];
  const familyModels = aliases === undefined ? [] : Array.from(new Set(Object.values(aliases)));
  return familyModels.length > 0 ? { available: true, models: familyModels, source: "vendorFamilyTable" } : { available: false };
}

/** What `crew_profile` should do with the model it was given. */
export type ModelOutcome =
  /**
   * Register with `model`; surface `note` when the input was not already
   * canonical. `verified` says whether anything actually confirmed the
   * model exists -- an exact catalogue id, or crew's own reviewed alias
   * table. It gates PERSISTENCE, not registration: the symptom was an
   * invented dated id becoming the repository's durable answer, so a name
   * nothing could confirm may run but must not be written to
   * `.omp/crew.json`.
   */
  | { kind: "use"; model: string; verified: boolean; note?: string }
  /** The refusal: the request names a different model than the stored one. */
  | { kind: "conflict"; configuredModel: string }
  /** The input matches several models -- refuse and name them. */
  | { kind: "ambiguous"; from: string; candidates: readonly string[] };

/**
 * Resolves `requested`, then applies the stored-model rules to the
 * result.
 *
 * Two orderings here are load-bearing and neither is obvious from the
 * ticket:
 *
 * **Resolution precedes the conflict check.** A stored `claude-opus-5` and
 * an explicit `opus` name one model; comparing spellings would refuse a
 * correct call with an error telling the user to go edit a file that is
 * already right.
 *
 * **Both sides are resolved, not just the request.** `.omp/crew.json` files
 * written before resolution existed hold shorthands, so the stored value is as likely
 * to be the un-canonical side as the request is.
 *
 * **Ambiguity is decided before the comparison.** An input matching several
 * models has no canonical form to compare, and letting it fall through to a
 * string compare against the stored value reports a conflict -- the wrong
 * thing, about the wrong thing.
 *
 * The `model` returned is always canonical where one is known, so the vendor
 * receives an unambiguous id. `configuredModel` in a conflict is the RAW
 * stored text, because the correction path is "edit `.omp/crew.json`" and the
 * error has to name what the reader will find there. Nothing here rewrites
 * the stored value: `crew_profile` promises never to overwrite a recorded
 * model, and a file saying `opus` while runs pass `claude-opus-5` is
 * consistent -- they are the same model.
 */
export function decideModel(adapter: Adapter, requested: string, configured: string | undefined, catalogue: Catalogue): ModelOutcome {
  const resolved = resolveModelName(adapter, requested, catalogue);
  if (resolved.kind === "ambiguous") {
    return { kind: "ambiguous", from: resolved.from, candidates: resolved.candidates };
  }

  if (configured !== undefined) {
    const storedResolution = resolveModelName(adapter, configured, catalogue);
    // An ambiguous *stored* value keeps its raw text: it is what the file
    // says, and a comparison against it is the honest one to make.
    const storedCanonical = storedResolution.kind === "ambiguous" ? configured : storedResolution.model;
    if (storedCanonical !== resolved.model) {
      return { kind: "conflict", configuredModel: configured };
    }
  }

  const note = resolutionNote(adapter, resolved);
  const verified = resolved.kind !== "unverified";
  return note === undefined ? { kind: "use", model: resolved.model, verified } : { kind: "use", model: resolved.model, verified, note };
}
