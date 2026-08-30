// Tests for the shared cache-then-connect resolver both `resolveClient`
// (spawn-on-demand, for user-initiated paths) and `resolveClientWithoutSpawning`
// (never spawns, for the monitor's automatic reconnect loop -- CREW-5's
// review should-fix against silently defeating ADR-0008's idle self-shutdown)
// go through.

import { afterEach, beforeAll, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import type { CrewClient } from "./client";
import { ensureRuntime, type EnsureRuntimeOptions } from "./runtime";
import { type CrewClientCache, type GetRuntimeStatusContext, getRuntimeStatus, resolveClient, resolveClientWithoutSpawning } from "./status";

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

/** A `crew.json` config layer enabling the dashboard on an OS-assigned
 *  (port 0) ephemeral port, so parallel test runs never collide. */
function dashboardEnabledConfigPath(): string {
  const dir = mkdtempSync("/tmp/bat-st-cfg-");
  const path = join(dir, "crew.json");
  writeFileSync(path, JSON.stringify({ dashboard: { enabled: true, port: 0 } }));
  return path;
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

// ------------------------------------------------- CREW-35: dashboard URL

test("runtime/status reports no dashboard URL when the dashboard is disabled (the default)", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  const { ctx } = contextFor(options(stateDir, repository));

  const result = await getRuntimeStatus(ctx);
  expect(result.isError).toBeUndefined();
  const details = result.details as { dashboardUrl: string | null };
  expect(details.dashboardUrl).toBeNull();
  expect(result.content[0].text).not.toContain("Dashboard:");
});

test("runtime/status reports the dashboard's live URL (token included) when enabled, and getRuntimeStatus's text surfaces it verbatim", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  const configPath = dashboardEnabledConfigPath();
  const { ctx } = contextFor({ ...options(stateDir, repository), configPaths: [configPath] });

  const result = await getRuntimeStatus(ctx);
  expect(result.isError).toBeUndefined();
  const details = result.details as { dashboardUrl: string | null };
  expect(details.dashboardUrl).not.toBeNull();
  // 127.0.0.1 only (never a routable interface), an OS-assigned port, and
  // the 32-hex-char token `dashboard::generate_token` mints.
  expect(details.dashboardUrl).toMatch(/^http:\/\/127\.0\.0\.1:\d+\/\?token=[0-9a-f]{32}$/);
  // CREW-35: the maintainer's explicit choice -- the full URL, token
  // included, must appear in the text a leader model actually reads.
  expect(result.content[0].text).toContain(`Dashboard: ${details.dashboardUrl}`);
});
