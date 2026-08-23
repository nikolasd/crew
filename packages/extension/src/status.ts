// The single status path shared by both the `crew_health` tool and the
// `/crew-status` command, plus the shared cached-client resolver used by
// every orchestration tool and the monitor: connect to (or spawn) the
// repository's `crewd` runtime, call `runtime/status`, and shape the

import { BinarySelectionError, ensureRuntime, type EnsureRuntimeOptions } from "./runtime";
import { BinaryIntegrityError, UnsupportedPlatformError } from "./platform";
import type { CrewClient } from "./client";
import type { RuntimeStatus } from "@nikolasd/batman-protocol";

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
 * Returns a live client for `ctx`, reusing the cached one only while its
 * socket is still open. A closed cached client is closed again (to release
 * its listeners) and dropped before reconnecting, so a daemon idle-exit or
 * socket failure repairs itself on the next call instead of breaking every
 * tool for the rest of the session.
 *
 * @throws whatever `ensureRuntime` throws when the runtime cannot be
 * reached or started.
 */
export async function resolveClient(ctx: GetRuntimeStatusContext): Promise<CrewClient> {
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
  const { client } = await ensureRuntime(ctx.ensureRuntimeOptions);
  ctx.cache.set(client);
  return client;
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
  const message = code === "runtime-not-installed" ? "The Crew runtime binary is not installed yet. Run /crew-runtime-install to download and verify it." : GENERIC_FAILURE_MESSAGE;
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
  return [
    `Crew runtime: ${status.running ? "running" : "not running"}`,
    `Protocol: ${status.protocol.major}.${status.protocol.minor} (healthy: ${status.protocolHealthy})`,
    `Project: ${status.projectId}`,
    `Active runs: ${status.activeRuns}`,
    `Schema version: ${status.schemaVersion}`,
    `Uptime: ${status.uptimeSeconds}s`,
    `Binary source: ${status.binarySource}`,
  ].join("\n");
}
