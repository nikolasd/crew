// Assembles the context `status.ts` needs to reach the per-repository
// `crewd` runtime: the resolved Crew state root, the repository the
// caller is operating in, and the options `ensureRuntime` (see `runtime.ts`)
// needs to connect to (or spawn) that runtime. Kept separate from
// `status.ts` so the RPC/formatting logic there stays free of environment
// and filesystem concerns.

import { homedir } from "node:os";

import { detectLibc, resolveCrewd } from "./platform";
import type { EnsureRuntimeOptions } from "./runtime";
import { resolveStateRoot } from "./state";

/**
 * Idle-shutdown budget, in seconds, for a `crewd` daemon this extension
 * spawns on demand. Foundation scope: a fixed default. A later task may make
 * this configurable.
 */
export const DEFAULT_IDLE_SECONDS = 30 * 60;

/** Inputs to {@link buildStatusContext}. */
export interface BuildStatusContextOptions {
  /**
   * The repository this status request concerns. Callers pass the OMP
   * extension context's `cwd` (the workspace/repository root OMP is running
   * against); falls back to `process.cwd()` when omitted.
   */
  readonly cwd?: string;
  /** Environment to resolve the state root and binary override from. Defaults to `process.env`. */
  readonly env?: Readonly<Record<string, string | undefined>>;
  /** Home directory used to resolve the default state root. Defaults to `os.homedir()`. */
  readonly home?: string;
  /**
   * Resolves the packaged `crewd` binary when no `OMP_CREW_BINARY`
   * (or legacy `OMP_BATMAN_BINARY`) override is set. Defaults to {@link resolveCrewd} against the current
   * process's platform/arch/libc; tests inject a stand-in here to stay
   * hermetic.
   */
  readonly packagedBinaryResolver?: () => string;
  /** OMP session ID for connection instanceId (for task ownership consistency). */
  readonly sessionId?: string;
}

/** The assembled context a `runtime/status` request needs. */
export interface StatusRuntimeContext {
  /** Options for {@link ensureRuntime}, fully resolved from the environment. */
  readonly ensureRuntimeOptions: EnsureRuntimeOptions;
}

/**
 * Builds the {@link StatusRuntimeContext} for a status request. Pure aside
 * from the `process.env`/`os.homedir()` defaults, which callers can override.
 */
export function buildStatusContext(options: BuildStatusContextOptions = {}): StatusRuntimeContext {
  const env = options.env ?? process.env;
  const home = options.home ?? homedir();
  const repository = options.cwd ?? process.cwd();
  const stateDir = resolveStateRoot(env, home);

  return {
    ensureRuntimeOptions: {
      stateDir,
      repository,
      idleSeconds: DEFAULT_IDLE_SECONDS,
      env,
      packagedBinaryResolver: options.packagedBinaryResolver ?? (() => resolveCrewd(process.platform, process.arch, detectLibc(), env, stateDir).path),
      sessionId: options.sessionId,
    },
  };
}
