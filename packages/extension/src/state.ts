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
 * 2. `$XDG_STATE_HOME/omp/batman`, if `XDG_STATE_HOME` is set (must be absolute).
 * 3. `$HOME/${PI_CONFIG_DIR:-.omp}/batman`.
 *
 * The on-disk directory name stays `batman` in both fallback tiers: moving
 * already-provisioned user state is a separate, careful migration this
 * rename does not attempt.
 *
 * Pure and side-effect free: `env` and `home` are taken explicitly (never
 * `process.env`/`os.homedir()` internally) so tests can drive fixtures, and
 * so this never touches the filesystem. Unlike the Rust side, this never
 * creates the directory or checks its permissions -- the Rust runtime is
 * solely responsible for creating and securing state directories; this
 * function only computes the path to pass to `crewd`.
 *
 * @throws {StateRootError} if `CREW_STATE_DIR`/`BATMAN_STATE_DIR` or
 * `XDG_STATE_HOME` is set to a relative path.
 */
export function resolveStateRoot(env: Readonly<Record<string, string | undefined>>, home: string): string {
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
    return join(xdgStateHome, "omp", "batman");
  }

  const piConfigDir = env.PI_CONFIG_DIR ?? ".omp";
  return join(home, piConfigDir, "batman");
}
