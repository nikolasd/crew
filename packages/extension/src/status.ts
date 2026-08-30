// The single status path shared by both the `crew_health` (LLM) tool and the
// `/crew health` (slash) command, plus the shared cached-client resolver used by
// every orchestration tool and the monitor: connect to (or spawn) the
// repository's `crewd` runtime, call `runtime/status`, and shape the

import { BinarySelectionError, connectIfRunning, ensureRuntime, type EnsureRuntimeOptions } from "./runtime";
import { BinaryIntegrityError, UnsupportedPlatformError } from "./platform";
import type { CrewClient } from "./client";
import type { RuntimeStatus } from "@nikolasd/crew-protocol";

/** A text content block, structurally compatible with OMP's `TextContent`. */
export interface StatusTextContent {
  readonly type: "text";
  readonly text: string;
}

/** Sanitized, machine-readable detail returned when the runtime is unreachable. */
export interface RuntimeStatusFailure {
  /** Machine-readable reason, e.g. a {@link BinarySelectionError} code. */
  readonly code: string;
  /** A message safe to display: no stack frames, no environment values. */
  readonly message: string;
  /** A command the operator can run locally to diagnose further. */
  readonly doctorCommand: string;
}

/** Successful result: the runtime answered `runtime/status`. */
export interface RuntimeStatusSuccess {
  content: [StatusTextContent];
  readonly details: RuntimeStatus;
  readonly isError?: false;
}

/** Failure result: the runtime could not be reached or started. */
export interface RuntimeStatusError {
  content: [StatusTextContent];
  readonly details: RuntimeStatusFailure;
  readonly isError: true;
}

export type RuntimeStatusResult = RuntimeStatusSuccess | RuntimeStatusError;

/** Reads and writes the single cached client for the calling extension instance. */
export interface CrewClientCache {
  get(): CrewClient | undefined;
  set(client: CrewClient | undefined): void;
}

/** Context {@link getRuntimeStatus} needs: where to connect, and the client cache. */
export interface GetRuntimeStatusContext {
  readonly ensureRuntimeOptions: EnsureRuntimeOptions;
  readonly cache: CrewClientCache;
}

/**
 * Shared cache-then-connect shape both {@link resolveClient} and
 * {@link resolveClientWithoutSpawning} follow: reuse the cached client while
 * its socket is still open; otherwise close and drop it (to release its
 * listeners) before asking `connector` to produce a fresh one, which gets
 * cached in turn. A daemon idle-exit or socket failure repairs itself on the
 * next call this way, regardless of which `connector` a caller uses.
 */
async function resolveClientVia(ctx: GetRuntimeStatusContext, connector: (options: EnsureRuntimeOptions) => Promise<CrewClient>): Promise<CrewClient> {
  const cached = ctx.cache.get();
  if (cached !== undefined) {
    if (!cached.isClosed) {
      return cached;
    }
    try {
      cached.close();
    } catch {
      // Best-effort: the client is already being discarded.
    }
    ctx.cache.set(undefined);
  }
  const client = await connector(ctx.ensureRuntimeOptions);
  ctx.cache.set(client);
  return client;
}

/**
 * Returns a live client for `ctx`, connecting (or spawning) the repository's
 * runtime on demand. For user-initiated paths only (a tool call, `/crew`) --
 * see {@link resolveClientWithoutSpawning} for the automatic-reconnect
 * counterpart that must never spawn.
 *
 * @throws whatever `ensureRuntime` throws when the runtime cannot be
 * reached or started.
 */
export async function resolveClient(ctx: GetRuntimeStatusContext): Promise<CrewClient> {
  return resolveClientVia(ctx, async (options) => (await ensureRuntime(options)).client);
}

/**
 * Like {@link resolveClient}, but never spawns a new runtime -- only
 * re-attaches to one that is already listening. For the monitor's automatic
 * background reconnect loop after an unexpected close (CREW-5): a spawn
 * belongs to a user-initiated path, not to a timer that would otherwise
 * silently convert an intentional idle-exit (ADR-0008) into "never idle" by
 * respawning the daemon every time it exits.
 *
 * @throws if the cached client is closed or absent and no runtime is
 * currently listening for this repository.
 */
export async function resolveClientWithoutSpawning(ctx: GetRuntimeStatusContext): Promise<CrewClient> {
  return resolveClientVia(ctx, async (options) => {
    const client = await connectIfRunning(options);
    if (client === undefined) {
      throw new Error("no Crew runtime is currently listening for this repository");
    }
    return client;
  });
}
const GENERIC_FAILURE_MESSAGE = "The Crew runtime is not reachable for this repository. Run the doctor command below for details.";

/**
 * Returns the current `runtime/status` for the repository named in
 * `ctx.ensureRuntimeOptions`, connecting to (or spawning) the runtime via the
 * cached client when available. Never throws: connection failures are
 * reported as a sanitized {@link RuntimeStatusError} instead.
 */
export async function getRuntimeStatus(ctx: GetRuntimeStatusContext): Promise<RuntimeStatusResult> {
  let client: CrewClient;
  try {
    client = await resolveClient(ctx);
  } catch (err) {
    return failureResult(ctx.ensureRuntimeOptions, err);
  }

  try {
    const status = (await client.request("runtime/status")) as RuntimeStatus;
    return {
      content: [{ type: "text", text: formatStatus(status) }],
      details: status,
    };
  } catch (err) {
    // The cached client's connection is no longer good; close it before
    // dropping the reference so its socket, listeners, and pending-request
    // map don't leak, then let the next call attempt a fresh `ensureRuntime`.
    try {
      client.close();
    } catch {
      // Best-effort: the client is already being discarded.
    }
    ctx.cache.set(undefined);
    return failureResult(ctx.ensureRuntimeOptions, err);
  }
}

function failureResult(options: EnsureRuntimeOptions, err: unknown): RuntimeStatusError {
  const code = errorCode(err);
  const doctorCommand = `crewd status --repo ${options.repository}`;
  const message = code === "runtime-not-installed" ? "The Crew runtime binary is not installed yet. Run /crew-install to download and verify it." : GENERIC_FAILURE_MESSAGE;
  return {
    isError: true,
    content: [{ type: "text", text: message }],
    details: { code, message, doctorCommand },
  };
}

/**
 * Maps a binary-selection/platform/integrity error to its machine-readable
 * `code`, or `"connection-failed"` for anything else (e.g. a generic
 * connect/spawn failure). Only the `code` is ever surfaced here -- in
 * particular, {@link BinaryIntegrityError}'s `message` embeds filesystem
 * paths and must never be copied into the (generic, sanitized) result
 * returned to the caller.
 */
function errorCode(err: unknown): string {
  if (err instanceof BinarySelectionError || err instanceof BinaryIntegrityError || err instanceof UnsupportedPlatformError) {
    return err.code;
  }
  return "connection-failed";
}

function formatStatus(status: RuntimeStatus): string {
  const lines = [
    `Crew runtime: ${status.running ? "running" : "not running"}`,
    `Protocol: ${status.protocol.major}.${status.protocol.minor} (healthy: ${status.protocolHealthy})`,
    `Project: ${status.projectId}`,
    `Active runs: ${status.activeRuns}`,
    `Schema version: ${status.schemaVersion}`,
    `Uptime: ${status.uptimeSeconds}s`,
    `Binary source: ${status.binarySource}`,
  ];
  // CREW-35: the full URL, including its access token, is included here
  // deliberately -- the maintainer chose one-click discoverability over
  // the narrower alternative (pointing at the daemon log instead). See
  // `RuntimeStatus.dashboardUrl`'s doc comment for the tradeoff. This
  // means the token now enters this session's own context/transcript
  // every time health is checked while the dashboard is enabled.
  if (status.dashboardUrl !== null) {
    lines.push(`Dashboard: ${status.dashboardUrl}`);
  }
  return lines.join("\n");
}
