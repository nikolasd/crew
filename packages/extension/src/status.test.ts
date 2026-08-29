// Tests for the shared cache-then-connect resolver both `resolveClient`
// (spawn-on-demand, for user-initiated paths) and `resolveClientWithoutSpawning`
// (never spawns, for the monitor's automatic reconnect loop -- CREW-5's
// review should-fix against silently defeating ADR-0008's idle self-shutdown)
// go through.

import { afterEach, beforeAll, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

import type { CrewClient } from "./client";
import { ensureRuntime, type EnsureRuntimeOptions } from "./runtime";
import { type CrewClientCache, type GetRuntimeStatusContext, resolveClient, resolveClientWithoutSpawning } from "./status";

const REPO_ROOT = join(import.meta.dir, "..", "..", "..");
const CREWD = join(REPO_ROOT, "target", "debug", "crewd");

const openClients: CrewClient[] = [];
const stateDirs: string[] = [];

function newRepo(): string {
  const repo = mkdtempSync("/tmp/bat-st-r-");
  mkdirSync(join(repo, ".git"));
  return repo;
}

function newStateDir(): string {
  const state = mkdtempSync("/tmp/bat-st-s-");
  stateDirs.push(state);
  return state;
}

/** Sends SIGTERM to every daemon under a state dir via its lock-file pid
 *  (mirrors runtime.test.ts's helper of the same name). */
function stopDaemons(stateDir: string): void {
  const repos = join(stateDir, "repos");
  if (!existsSync(repos)) {
    return;
  }
  for (const entry of readdirSync(repos)) {
    const lock = join(repos, entry, "runtime.lock");
    if (!existsSync(lock)) {
      continue;
    }
    try {
      const { pid } = JSON.parse(readFileSync(lock, "utf8")) as { pid: number };
      process.kill(pid, "SIGTERM");
    } catch {
      // Already gone.
    }
  }
}

function options(stateDir: string, repository: string): EnsureRuntimeOptions {
  return {
    stateDir,
    repository,
    idleSeconds: 30,
    env: { OMP_CREW_BINARY: CREWD },
  };
}

function contextFor(ensureRuntimeOptions: EnsureRuntimeOptions): { ctx: GetRuntimeStatusContext; cache: CrewClientCache } {
  let cached: CrewClient | undefined;
  const cache: CrewClientCache = {
    get: () => cached,
    set: (client) => {
      cached = client;
    },
  };
  return { ctx: { ensureRuntimeOptions, cache }, cache };
}

beforeAll(() => {
  const build = Bun.spawnSync(["cargo", "build", "-p", "crew-runtime"], { cwd: REPO_ROOT });
  if (build.exitCode !== 0) {
    throw new Error(`cargo build failed: ${build.stderr.toString()}`);
  }
}, 180_000);

afterEach(async () => {
  for (const client of openClients.splice(0)) {
    client.close();
  }
  for (const stateDir of stateDirs.splice(0)) {
    stopDaemons(stateDir);
  }
  await new Promise<void>((resolve) => setTimeout(resolve, 200));
});

test("resolveClientWithoutSpawning throws and spawns nothing when no runtime is listening", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  const { ctx } = contextFor(options(stateDir, repository));

  await expect(resolveClientWithoutSpawning(ctx)).rejects.toThrow();

  // Proof nothing was spawned, not just that the rejection didn't keep a
  // reference to it.
  expect(existsSync(join(stateDir, "repos"))).toBe(false);
});

test("resolveClientWithoutSpawning re-attaches to and caches an already-running daemon", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  const { client: seeded } = await ensureRuntime(options(stateDir, repository));
  openClients.push(seeded);

  const { ctx, cache } = contextFor(options(stateDir, repository));
  const client = await resolveClientWithoutSpawning(ctx);
  openClients.push(client);

  const status = (await client.request("runtime/status")) as { running: boolean };
  expect(status.running).toBe(true);
  expect(cache.get()).toBe(client);

  // Still exactly one runtime -- re-attached, not spawned a second one.
  const repos = join(stateDir, "repos");
  expect(readdirSync(repos).length).toBe(1);
});

test("resolveClientWithoutSpawning reuses an open cached client without reconnecting at all", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  const { ctx, cache } = contextFor(options(stateDir, repository));

  // Seed the cache with an object that would throw if anything tried to
  // treat it as a real CrewClient beyond `isClosed` -- proving the cached
  // branch never calls the no-spawn connector at all.
  const fakeCached = { isClosed: false } as unknown as CrewClient;
  cache.set(fakeCached);

  const client = await resolveClientWithoutSpawning(ctx);
  expect(client).toBe(fakeCached);

  // No runtime ever got a chance to spawn (there was no listener to skip
  // even trying to reach), and no repo directory appeared.
  expect(existsSync(join(stateDir, "repos"))).toBe(false);
});

test("resolveClient (spawning) still spawns when nothing is listening", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  const { ctx } = contextFor(options(stateDir, repository));

  const client = await resolveClient(ctx);
  openClients.push(client);

  const status = (await client.request("runtime/status")) as { running: boolean };
  expect(status.running).toBe(true);
});
