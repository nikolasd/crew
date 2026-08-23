// On-demand launcher for the per-repository `crewd` runtime daemon.
//
// `ensureRuntime` connects to an existing runtime if one is already serving
// the repository; otherwise it selects a `crewd` binary (a validated
// `OMP_CREW_BINARY` development override -- or its pre-rename name,
// `OMP_BATMAN_BINARY` -- or an injected packaged-binary resolver), spawns it
// detached, and retries connecting with bounded exponential backoff.
// Concurrent callers converge on a single runtime: the daemon's own
// `O_EXCL` lock guarantees exactly one wins the race, and every caller ends
// up connected to that winner.

import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, lstatSync, realpathSync, statSync } from "node:fs";
import { dirname, isAbsolute, join } from "node:path";

import { CrewClient } from "./client";
import { envFlag } from "./env-flag";
import type { InitializeParams } from "@nikolasd/batman-protocol";

import pkg from "../package.json" with { type: "json" };
/** The bootstrap frame size the launcher's own connections negotiate. */
const CONNECT_MAX_FRAME_BYTES = 1024 * 1024;

/** Total budget for connecting to a freshly spawned daemon. */
const CONNECT_DEADLINE_MS = 5000;

/** Options for {@link ensureRuntime} and {@link buildServeArgs}. */
export interface EnsureRuntimeOptions {
  /** Absolute Crew state root, passed verbatim to `crewd`. */
  readonly stateDir: string;
  /** Absolute repository path the runtime serves. */
  readonly repository: string;
  /** Idle interval, in seconds, the detached daemon is launched with. */
  readonly idleSeconds: number;
  /** Environment to read `OMP_CREW_BINARY` (or the legacy `OMP_BATMAN_BINARY`) from. Defaults to `process.env`. */
  readonly env?: Readonly<Record<string, string | undefined>>;
  /**
   * Resolves the packaged binary path when no override is set. Task 9 supplies
   * the real implementation; without it and without an override, selection
   * fails.
   */
  readonly packagedBinaryResolver?: () => string;
  /**
   * The OMP session ID, used as the connection `instanceId` so that task
   * ownership, approval decisions, and violation decisions all share the
   * same principal. When omitted, falls back to a constant identity.
   */
  readonly sessionId?: string;
}

/** The result of {@link ensureRuntime}. */
export interface EnsureRuntimeResult {
  /** A connected, initialized client for the runtime. */
  readonly client: CrewClient;
  /** Whether this call spawned the runtime it connected to. */
  readonly childStarted: boolean;
}

/** Machine-readable reason a binary could not be selected. */
export type BinarySelectionCode = "not-absolute" | "not-found" | "not-regular" | "not-executable" | "no-binary" | "runtime-not-installed";

/** Thrown before any spawn when a `crewd` binary cannot be selected. */
export class BinarySelectionError extends Error {
  readonly code: BinarySelectionCode;

  constructor(code: BinarySelectionCode, message: string) {
    super(message);
    this.name = "BinarySelectionError";
    this.code = code;
  }
}

/** The origin of the selected binary, mirrored by `runtime/status`. */
export type BinarySource = "override" | "package";

/** The result of a binary-selection step: a path and where it came from. */
export interface SelectedBinary {
  readonly path: string;
  readonly source: BinarySource;
}

/**
 * The exact argument vector for a detached `crewd serve`. Detached launches
 * deliberately omit `--foreground`, so the daemon owns `runtime.log` before
 * its inherited stdio is discarded.
 */
export function buildServeArgs(options: EnsureRuntimeOptions): string[] {
  return ["serve", "--state-dir", options.stateDir, "--repo", options.repository, "--idle-seconds", String(options.idleSeconds)];
}

/**
 * Connects to the runtime for `options.repository`, spawning it detached if it
 * is not already serving.
 */
export async function ensureRuntime(options: EnsureRuntimeOptions): Promise<EnsureRuntimeResult> {
  const socketPath = socketPathFor(options.stateDir, options.repository);

  // 1. If a runtime is already serving, connect to it without spawning.
  const existing = await tryConnect(socketPath, options.repository, options.sessionId);
  if (existing !== undefined) {
    return { client: existing, childStarted: false };
  }

  // 2. Select and validate the binary BEFORE spawning. Every violation throws
  //    a typed error here, before any process is created.
  const binary = selectBinary(options.env ?? process.env, options.packagedBinaryResolver);

  // 3. Spawn detached; the child owns its own lifetime and logs to runtime.log.
  const child = spawn(binary.path, buildServeArgs(options), {
    detached: true,
    stdio: "ignore",
    env: { ...process.env, CREW_BINARY_SOURCE: binary.source },
  });
  // An async spawn failure (EAGAIN, EMFILE, a binary deleted between
  // validation and exec) surfaces as an `error` event, not a throw. Without
  // a listener it becomes an unhandled-error crash; with one it is logged
  // and the connect backoff below reports the runtime as unreachable. This
  // file has no logger seam, so console.error is deliberate. Attached
  // before unref() so the event cannot race the listener registration.
  child.on("error", (err) => {
    console.error(`crew runtime: failed to spawn ${binary.path}: ${err instanceof Error ? err.message : String(err)}`);
  });
  child.unref();

  // 4. Retry connecting with bounded exponential backoff. If a different
  //    caller won the lock, we still connect to the winner here.
  const client = await connectWithBackoff(socketPath, options.repository, options.sessionId);
  return { client, childStarted: true };
}

/**
 * The deterministic socket path for a repository, derived exactly as the Rust
 * runtime derives it: SHA-256 of the canonical VCS root, first 16 bytes hex.
 */
function socketPathFor(stateDir: string, repository: string): string {
  return join(stateDir, "repos", repositoryId(repository), "runtime.sock");
}

/**
 * The stable `repository-id` for a repository path on disk: canonicalizes it,
 * discovers its VCS root (see {@link discoverVcsRoot}), and hashes that root
 * exactly as the Rust runtime does. Exported so the reconciled `.git`
 * discovery semantics are directly testable.
 */
export function repositoryId(repository: string): string {
  const canonical = realpathSync(repository);
  const vcsRoot = discoverVcsRoot(canonical) ?? canonical;
  return repositoryIdFromRoot(vcsRoot);
}

/**
 * The stable `repository-id` for an already-canonical VCS root: the lowercase
 * hex of the first 16 bytes of the SHA-256 of its UTF-8 bytes (32 hex
 * characters). A pure function of the path string -- no filesystem access --
 * and the exact hash the Rust runtime's `repository_id_from_canonical_root`
 * produces. Both sides are guarded by `fixtures/repo-id/repo-id-cases.json`.
 */
export function repositoryIdFromRoot(canonicalRoot: string): string {
  const digest = createHash("sha256").update(canonicalRoot, "utf8").digest();
  return digest.subarray(0, 16).toString("hex");
}

/**
 * Walks up from `canonical` (inclusive) for a `.git` entry, mirroring the Rust
 * runtime's `discover_vcs_root` exactly: it treats presence alone as the
 * marker via `lstat` (so a `.git` file, directory, or even a *broken* symlink
 * counts -- the entry's contents are never read), and walks the *lexical*
 * parents of the canonical path (never re-resolving symlinks mid-walk). Returns
 * `undefined` if none is found before the filesystem root.
 */
function discoverVcsRoot(canonical: string): string | undefined {
  let current = canonical;
  for (;;) {
    if (pathExists(join(current, ".git"))) {
      return current;
    }
    const parent = dirname(current);
    if (parent === current) {
      return undefined;
    }
    current = parent;
  }
}

/**
 * Whether `path` exists as any kind of entry, matched by `lstat` so a broken
 * symlink still counts (it is the link node's presence that matters, never its
 * target). This mirrors Rust's `symlink_metadata` check.
 */
function pathExists(path: string): boolean {
  try {
    lstatSync(path);
    return true;
  } catch {
    return false;
  }
}

/**
 * Validates and returns the `OMP_CREW_BINARY` development override (or its
 * pre-rename name, `OMP_BATMAN_BINARY`) from `env`, or `undefined` if
 * neither is set (or set to an empty string). The override must be
 * absolute, exist, be a regular file, and be executable; each violation
 * throws a {@link BinarySelectionError} before any spawn.
 *
 * Shared by {@link ensureRuntime}'s binary selection and by
 * `platform.ts`'s `resolveCrewd`, so override precedence and validation
 * behave identically wherever a `crewd` binary is selected.
 */
export function resolveOverride(env: Readonly<Record<string, string | undefined>>): SelectedBinary | undefined {
  const override = envFlag(env, "OMP_CREW_BINARY", "OMP_BATMAN_BINARY");
  if (override === undefined || override === "") {
    return undefined;
  }

  if (!isAbsolute(override)) {
    throw new BinarySelectionError("not-absolute", `OMP_CREW_BINARY must be an absolute path, got ${JSON.stringify(override)}`);
  }

  // Canonicalize to prove existence (and follow symlinks for the file-type
  // and executability checks, so the override cannot point at a directory).
  let canonical: string;
  try {
    canonical = realpathSync(override);
  } catch {
    throw new BinarySelectionError("not-found", `OMP_CREW_BINARY does not exist: ${override}`);
  }

  const stat = statSync(canonical);
  if (!stat.isFile()) {
    throw new BinarySelectionError("not-regular", `OMP_CREW_BINARY is not a regular file: ${override}`);
  }
  // Owner/group/other execute bit set?
  if ((stat.mode & 0o111) === 0) {
    throw new BinarySelectionError("not-executable", `OMP_CREW_BINARY is not executable: ${override}`);
  }

  // Selected verbatim: the override path is used as given.
  return { path: override, source: "override" };
}

/**
 * Selects the `crewd` binary. A set `OMP_CREW_BINARY` (or legacy
 * `OMP_BATMAN_BINARY`) wins as a development override (see
 * {@link resolveOverride}); otherwise the packaged resolver is used if
 * provided.
 */
function selectBinary(env: Readonly<Record<string, string | undefined>>, packagedBinaryResolver?: () => string): SelectedBinary {
  const override = resolveOverride(env);
  if (override !== undefined) {
    return override;
  }

  if (packagedBinaryResolver !== undefined) {
    return { path: packagedBinaryResolver(), source: "package" };
  }

  throw new BinarySelectionError("no-binary", "no OMP_CREW_BINARY override is set and no packaged-binary resolver was provided");
}

/**
 * Builds ompExtension-role initialize params. This is the extension's own
 * connection to its repository-scoped daemon -- the same client is shared
 * by every orchestration tool and the embedded monitor, so it must carry
 * the write-capable `ompExtension` role rather than the read-only
 * `display` role a separate, external viewer would use.
 */
function initParams(repository: string, sessionId?: string): InitializeParams {
  const canonical = realpathSync(repository);
  return {
    client: { name: "@nikolasd/crew", version: pkg.version },
    supported: { min: { major: 1, minor: 0 }, max: { major: 1, minor: 0 } },
    repository: { canonicalPath: canonical, vcsRoot: canonical },
    auth: { role: "ompExtension", instanceId: sessionId ?? "crew-extension", agentDirectory: canonical },
    capabilities: { eventReplay: false, maxFrameBytes: CONNECT_MAX_FRAME_BYTES },
    lastSequence: null,
  } as InitializeParams;
}

/**
 * Attempts one connect + initialize. Resolves to the client on success, or
 * `undefined` if the runtime is absent or the handshake fails.
 */
async function tryConnect(socketPath: string, repository: string, sessionId?: string): Promise<CrewClient | undefined> {
  if (!existsSync(socketPath)) {
    return undefined;
  }
  const client = new CrewClient({ socketPath });
  try {
    await client.whenConnected();
    await client.initialize(initParams(repository, sessionId));
    return client;
  } catch {
    client.close();
    return undefined;
  }
}

/**
 * Retries {@link tryConnect} with exponential backoff, up to
 * {@link CONNECT_DEADLINE_MS} total.
 */
async function connectWithBackoff(socketPath: string, repository: string, sessionId?: string): Promise<CrewClient> {
  const deadline = Date.now() + CONNECT_DEADLINE_MS;
  let delay = 25;
  for (;;) {
    const client = await tryConnect(socketPath, repository, sessionId);
    if (client !== undefined) {
      return client;
    }
    if (Date.now() >= deadline) {
      throw new Error(`runtime did not become reachable at ${socketPath} within ${CONNECT_DEADLINE_MS}ms`);
    }
    await sleep(Math.min(delay, deadline - Date.now()));
    delay = Math.min(delay * 2, 500);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}
