import { existsSync } from "node:fs";
import { isAbsolute, join } from "node:path";

import { envFlag } from "./env-flag";

/**
 * Machine-readable reason a {@link resolveStateRoot} call was rejected.
 */
export type StateRootErrorCode = "relative-override";

/**
 * Thrown by {@link resolveStateRoot} when an override environment variable
 * is set but is not an absolute path.
 */
export class StateRootError extends Error {
  readonly code: StateRootErrorCode;

  constructor(code: StateRootErrorCode, message: string) {
    super(message);
    this.name = "StateRootError";
    this.code = code;
  }
}

/**
 * Resolves the Crew state root directory.
 *
 * Precedence, identical to Rust's `StateRoot::resolve`:
 * 1. `CREW_STATE_DIR` (or its pre-rename name, `BATMAN_STATE_DIR`), if set (must be absolute).
 * 2. `$XDG_STATE_HOME/omp/crew`, if `XDG_STATE_HOME` is set (must be absolute) -- unless that
 *    directory does not exist and the legacy `$XDG_STATE_HOME/omp/batman` does, in which case the
 *    legacy directory is used.
 * 3. `$HOME/${PI_CONFIG_DIR:-.omp}/crew` -- with the same legacy fallback to
 *    `$HOME/${PI_CONFIG_DIR:-.omp}/batman` when only the legacy directory exists.
 *
 * A fresh install therefore lands under the new `crew`-named directory, while an existing install
 * keeps working against its `batman`-named directory -- this never moves data itself.
 *
 * `env` and `home` are taken explicitly (never `process.env`/`os.homedir()` internally) so tests
 * can drive fixtures deterministically. `exists` is the directory-existence probe used only in the
 * two fallback tiers above; it defaults to a real `existsSync` check but can be injected (e.g. by
 * tests, via the shared fixture's `existingDirs` field) to stay deterministic without touching the
 * real filesystem -- mirroring Rust's `StateRoot::resolve_with`. Unlike the Rust side, this never
 * creates the directory or checks its permissions -- the Rust runtime is solely responsible for
 * creating and securing state directories; this function only computes the path to pass to
 * `crewd`.
 *
 * @throws {StateRootError} if `CREW_STATE_DIR`/`BATMAN_STATE_DIR` or
 * `XDG_STATE_HOME` is set to a relative path.
 */
export function resolveStateRoot(env: Readonly<Record<string, string | undefined>>, home: string, exists: (path: string) => boolean = existsSync): string {
  const crewStateDir = envFlag(env, "CREW_STATE_DIR", "BATMAN_STATE_DIR");
  if (crewStateDir !== undefined) {
    if (!isAbsolute(crewStateDir)) {
      throw new StateRootError("relative-override", `CREW_STATE_DIR must be an absolute path, got ${JSON.stringify(crewStateDir)}`);
    }
    return crewStateDir;
  }

  const xdgStateHome = env.XDG_STATE_HOME;
  if (xdgStateHome !== undefined) {
    if (!isAbsolute(xdgStateHome)) {
      throw new StateRootError("relative-override", `XDG_STATE_HOME must be an absolute path, got ${JSON.stringify(xdgStateHome)}`);
    }
    return preferringLegacyIfOnlyItExists(join(xdgStateHome, "omp"), exists);
  }

  const piConfigDir = env.PI_CONFIG_DIR ?? ".omp";
  return preferringLegacyIfOnlyItExists(join(home, piConfigDir), exists);
}

/**
 * Given a parent directory (`$XDG_STATE_HOME/omp` or `$HOME/${PI_CONFIG_DIR:-.omp}`), returns
 * `parent/crew` unless `parent/batman` exists and `parent/crew` does not, in which case it returns
 * `parent/batman`. Mirrors Rust's `StateRoot::preferring_legacy_if_only_it_exists`.
 */
function preferringLegacyIfOnlyItExists(parent: string, exists: (path: string) => boolean): string {
  const crewDir = join(parent, "crew");
  const legacyDir = join(parent, "batman");
  if (!exists(crewDir) && exists(legacyDir)) {
    return legacyDir;
  }
  return crewDir;
}
