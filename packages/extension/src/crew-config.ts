// Resolution of crew.json config layer files (`~/.omp/crew.json`,
// `<repository>/.omp/crew.json`) for `ensureRuntime`'s `--config` args.
//
// The daemon (`crates/runtime/src/config/crew.rs`) is the sole authority on
// a layer file's shape, unknown-key rejection, and merge semantics; this
// module only locates which layer files exist on disk and confirms each one
// parses as JSON, so a malformed file fails the launch early with a clear,
// file-naming error instead of a cryptic daemon-side rejection later.

import { existsSync, readFileSync } from "node:fs";
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
