// Resolution of crew.json config layer files (`~/.omp/crew.json`,
// `<repository>/.omp/crew.json`) for `ensureRuntime`'s `--config` args.
//
// The daemon (`crates/runtime/src/config/crew.rs`) is the sole authority on
// a layer file's shape, unknown-key rejection, and merge semantics; this
// module only locates which layer files exist on disk and confirms each one
// parses as JSON, so a malformed file fails the launch early with a clear,
// file-naming error instead of a cryptic daemon-side rejection later.

import { randomBytes } from "node:crypto";
import { closeSync, existsSync, fsyncSync, mkdirSync, openSync, readFileSync, renameSync, writeSync } from "node:fs";
import { join } from "node:path";

/**
 * The shape of a crew.json config layer file, mirroring
 * `crates/runtime/src/config/crew.rs`'s `CrewConfig` (spec §10). Every
 * field is optional: a layer file may set any subset, and the daemon deep-
 * merges layers over its own built-in defaults (`security.patterns` is the
 * one exception -- additive, never replaced). Extension-authored input, not
 * a daemon protocol type: this module never deserializes into it, only
 * documents what a layer file may contain.
 */
export interface CrewConfigFile {
  readonly approval?: "always" | "never" | "auto";
  readonly limits?: {
    readonly maxConcurrentWorkers?: number;
    readonly inactivityTimeoutSec?: number;
    readonly totalTimeoutSec?: number;
    readonly turnBudgetPerSubtask?: number;
  };
  readonly display?: {
    readonly backend?: "auto" | "herdr" | "tmux" | "osWindow" | "hidden";
    readonly closeOnExit?: "never" | "onSuccess" | "always";
  };
  readonly adapters?: Record<
    string,
    {
      readonly enabled?: boolean;
      readonly bin?: string;
      // "headless" is retired (crew-v2 gap-closure WP-C, spec §4.6) -- the
      // daemon still parses it (so an old layer file doesn't fail to load
      // here, ahead of the daemon's own load) but then typed-rejects it
      // before dispatch. Kept in this union for the same reason: this type
      // documents what a layer file may *contain*, not what the daemon
      // will *accept*. See docs/adr/0026-headless-retirement.md.
      readonly mode?: "tui" | "headless";
      readonly permissionMode?: "max" | "default" | "readonly";
      readonly model?: string;
      readonly profile?: string;
      readonly sessionDir?: string;
      readonly extraArgs?: readonly string[];
    }
  >;
  readonly workspace?: {
    readonly defaultMode?: "shared" | "gitWorktree" | "copy";
    readonly copyMaxBytes?: number;
    readonly copyMaxFiles?: number;
  };
  readonly dashboard?: {
    readonly enabled?: boolean;
    readonly port?: number;
  };
  readonly retention?: {
    readonly maxRuns?: number;
    readonly period?: string;
  };
  readonly security?: {
    readonly patterns?: readonly string[];
  };
}

/** Machine-readable reason a {@link CrewConfigError} was thrown. */
export type CrewConfigErrorCode = "invalid-json";

/**
 * Thrown by {@link resolveCrewConfigPaths} when an existing layer file is
 * not valid JSON.
 */
export class CrewConfigError extends Error {
  readonly code: CrewConfigErrorCode;
  readonly path: string;

  constructor(code: CrewConfigErrorCode, path: string, message: string) {
    super(message);
    this.name = "CrewConfigError";
    this.code = code;
    this.path = path;
  }
}

/**
 * Resolves the crew config layer files that exist on disk, in precedence
 * order (lowest first): `~/.omp/crew.json`, then
 * `<repository>/.omp/crew.json` -- so the project file, passed second,
 * wins a later-layer-wins deep merge against the user file. A candidate
 * that does not exist is silently omitted, not an error (both layers are
 * optional). An existing candidate is parsed as JSON purely to fail the
 * launch early with a clear, file-naming error on malformed content; the
 * returned array carries paths, not parsed content.
 *
 * @throws {CrewConfigError} if an existing layer file is not valid JSON.
 */
export function resolveCrewConfigPaths(home: string, repository: string): string[] {
  const candidates = [join(home, ".omp", "crew.json"), join(repository, ".omp", "crew.json")];
  const resolved: string[] = [];
  for (const path of candidates) {
    if (!existsSync(path)) {
      continue;
    }
    try {
      JSON.parse(readFileSync(path, "utf8"));
    } catch (err) {
      throw new CrewConfigError("invalid-json", path, `crew config file ${path} is not valid JSON: ${err instanceof Error ? err.message : String(err)}`);
    }
    resolved.push(path);
  }
  return resolved;
}

/**
 * The `adapters.*` config-section key `crate::config::crew::CrewConfig`
 * actually uses for each reserved adapter -- `RESERVED_ADAPTER_CONFIG_KEYS`
 * in `crates/runtime/src/config/crew.rs`, a *separate* constant from
 * `AdapterKind::RESERVED_NAMES` (the `profile/register` wire adapter
 * name). They agree for `claude`/`codex`/`copilot`, but not for the
 * fourth: the config section is keyed `omp` (matching the actual vendor
 * binary name), while the wire adapter name `crew_profile` receives is
 * `ompRpc`. Not a typo on either side -- `crew.rs:94`'s own doc comment on
 * `RESERVED_ADAPTER_CONFIG_KEYS` names this exact split deliberately;
 * read it there before re-deriving this the hard way from scratch.
 */
const CONFIG_KEY_FOR_ADAPTER: Readonly<Record<string, string>> = { ompRpc: "omp" };

/** The `adapters.*` config-section key for `adapter`'s wire name. */
function configKeyFor(adapter: string): string {
  return CONFIG_KEY_FOR_ADAPTER[adapter] ?? adapter;
}

/**
 * CREW-8: the effective `adapters.<adapter>.model` across the crew.json
 * layers -- user then repo, later (repo) layer wins, matching every other
 * field's own later-layer-wins merge. `null` (`crew.default.json`'s own
 * unset marker for the three reserved adapters with no default) and a
 * missing/absent key both mean "not configured" -- only a non-empty string
 * counts. Reuses {@link resolveCrewConfigPaths} for its exact malformed-
 * layer behavior (throws {@link CrewConfigError} naming the file) rather
 * than degrading silently: a broken layer file must surface here exactly
 * as loudly as it would at daemon launch, not resolve to "no model
 * configured" and mask the real problem behind an ask-the-user prompt.
 */
export function resolveConfiguredModel(home: string, repository: string, adapter: string): string | undefined {
  const configKey = configKeyFor(adapter);
  let model: string | undefined;
  for (const path of resolveCrewConfigPaths(home, repository)) {
    const parsed = JSON.parse(readFileSync(path, "utf8")) as CrewConfigFile;
    const candidate = parsed.adapters?.[configKey]?.model;
    if (typeof candidate === "string" && candidate.length > 0) {
      model = candidate;
    }
  }
  return model;
}

/**
 * CREW-8: records `model` as `adapters.<adapter>.model` in the *repo*
 * layer only (`<repository>/.omp/crew.json`) -- never the user's global
 * layer, and never called by `crew_profile` unless
 * {@link resolveConfiguredModel} already found nothing for this adapter.
 * Read-modify-write, preserving every other existing key (including other
 * adapters' entries): this is a plain JSON merge, not the daemon's own
 * `crew.json` loader, so it never validates unknown keys itself -- it only
 * ever adds one, which the daemon's own strict loader will accept.
 *
 * **Never overwrites an already-configured model for this adapter** --
 * checked again here, defensively, even though the caller is expected to
 * have checked first: the file is authoritative once written; correcting
 * a wrong model means editing `crew.json` directly, not calling
 * `crew_profile` again (see its own tool description).
 *
 * **A malformed existing file aborts the write (throws) rather than
 * silently starting from `{}`** -- doing otherwise would wipe every other
 * key a hand-edit typo left behind. A missing file is not malformed: it
 * starts from `{}`, same as `resolveCrewConfigPaths` treats an absent
 * layer as "no override", not an error.
 *
 * **Atomic, but not on its own crash-safe** -- writes to a sibling
 * `.tmp-<random>` file in the *same* directory (a cross-filesystem temp
 * dir could not be renamed over the target at all), `fsync`s that file
 * descriptor before closing it, then `renameSync`s over the target.
 * `rename` within one directory is atomic for *visibility*: no reader
 * ever observes a half-written file. The `fsync` before it is what makes
 * the *contents* durable too -- without it, `rename`'s atomicity only
 * guarantees a reader never sees a torn write, not that the write
 * survives a crash between the rename and the next fsync of the
 * directory itself.
 *
 * **No file lock, deliberately** -- two sessions racing to register the
 * *same* adapter write the same model, so the lost update is invisible;
 * two racing on *different* adapters touch different subtrees, and the
 * second write's fresh read already contains the first's key. The only
 * genuinely lossy interleaving is two sessions racing to register the
 * *same* adapter with two *different* explicit models -- which means two
 * callers are deliberately reconfiguring the same adapter concurrently,
 * a case a lock could not resolve either (one of the two intents loses
 * regardless of ordering), so refusing on conflict would only turn an
 * invisible non-problem into a user-visible failure on a once-per-adapter
 * operation.
 */
export function persistConfiguredModel(repository: string, adapter: string, model: string): void {
  const dir = join(repository, ".omp");
  const path = join(dir, "crew.json");
  let parsed: Record<string, unknown> = {};
  if (existsSync(path)) {
    try {
      parsed = JSON.parse(readFileSync(path, "utf8")) as Record<string, unknown>;
    } catch (err) {
      throw new CrewConfigError("invalid-json", path, `crew config file ${path} is not valid JSON: ${err instanceof Error ? err.message : String(err)}`);
    }
  }
  const configKey = configKeyFor(adapter);
  const adapters = { ...(parsed.adapters as Record<string, unknown> | undefined) };
  const existingEntry = (adapters[configKey] as Record<string, unknown> | undefined) ?? {};
  if (typeof existingEntry.model === "string" && existingEntry.model.length > 0) {
    return; // Already configured -- an explicit param wins for this call without persisting.
  }
  adapters[configKey] = { ...existingEntry, model };
  parsed.adapters = adapters;

  mkdirSync(dir, { recursive: true });
  const tmpPath = join(dir, `crew.json.tmp-${randomBytes(6).toString("hex")}`);
  const fd = openSync(tmpPath, "w");
  try {
    writeSync(fd, `${JSON.stringify(parsed, null, 2)}\n`);
    fsyncSync(fd);
  } finally {
    closeSync(fd);
  }
  renameSync(tmpPath, path);
}
