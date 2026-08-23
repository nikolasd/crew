// End-to-end test for cached-client reconnection (TODO #73 / R6).
//
// Proves that when the daemon exits (idle timeout or SIGTERM), the next
// tool call automatically reconnects instead of failing with a stale,
// closed client. Exercises the full extension path (not just the status
// command, which already had its own repair logic).
//
// Uses a live foreground daemon with a restartable lifecycle.

import { afterAll, beforeAll, expect, test } from "bun:test";
import { existsSync, mkdtempSync, mkdirSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { z as zod } from "zod/v4";

import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import extension from "./index";

const REPO_ROOT = join(import.meta.dir, "..", "..", "..");
const CREWD = join(REPO_ROOT, "target", "debug", "crewd");

// ---- Daemon lifecycle (self-contained, restartable) ----

let daemonProcess: { kill: (s: "SIGTERM") => void; exited: Promise<unknown> } = { kill: () => {}, exited: Promise.resolve(undefined) };
let stateDir = "";
let repoDir = "";

function startDaemon(): Promise<void> {
  const reposDir = join(stateDir, "repos");

  daemonProcess = Bun.spawn([CREWD, "serve", "--foreground", "--state-dir", stateDir, "--repo", repoDir], {
    stdout: "ignore",
    stderr: "pipe",
  });

  // Wait for the runtime socket to appear.
  // eslint-disable-next-line no-setTimeout
  const { promise: ready, resolve: onReady } = Promise.withResolvers<void>();
  const check = () => {
    if (!existsSync(reposDir)) {
      setTimeout(check, 50);
      return;
    }
    for (const entry of readdirSync(reposDir)) {
      if (existsSync(join(reposDir, entry, "runtime.sock"))) {
        onReady();
        return;
      }
    }
    setTimeout(check, 50);
  };
  check();
  return ready;
}

function stopDaemon(): Promise<void> {
  daemonProcess.kill("SIGTERM");
  return daemonProcess.exited as unknown as Promise<void>;
}

async function restartDaemon(): Promise<void> {
  await stopDaemon();
  await startDaemon();
}

beforeAll(async () => {
  const build = Bun.spawnSync(["cargo", "build", "-p", "batman-runtime"], { cwd: REPO_ROOT });
  if (build.exitCode !== 0) {
    throw new Error(`cargo build failed: ${build.stderr.toString()}`);
  }

  stateDir = mkdtempSync("/tmp/bat-rec-s-");
  repoDir = mkdtempSync("/tmp/bat-rec-r-");
  mkdirSync(join(repoDir, ".git"));

  process.env.CREW_STATE_DIR = stateDir;

  await startDaemon();
}, 180_000);

afterAll(async () => {
  daemonProcess.kill("SIGTERM");
  await daemonProcess.exited;
  delete process.env.CREW_STATE_DIR;
});

// ---- Fake ExtensionAPI ----

interface FakeToolDefinition {
  readonly name: string;
  readonly execute: (toolCallId: string, params: unknown, signal: AbortSignal | undefined, onUpdate: undefined, ctx: ExtensionContext) => Promise<{ isError?: boolean; details?: unknown }>;
}

function createFakeApi(): {
  api: ExtensionAPI;
  tools: Map<string, FakeToolDefinition>;
} {
  const tools = new Map<string, FakeToolDefinition>();
  const api = {
    zod,
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    registerTool(tool: FakeToolDefinition) {
      tools.set(tool.name, tool);
    },
    registerCommand() {},
    on() {},
    appendEntry() {},
  };

  return { api: api as unknown as ExtensionAPI, tools };
}

function makeContext(sessionId: string): ExtensionContext {
  return {
    cwd: repoDir,
    sessionManager: { getSessionId: () => sessionId } as ExtensionContext["sessionManager"],
  } as unknown as ExtensionContext;
}

// ---- Tests ----

test("a tool reconnects after the daemon exits", async () => {
  const { api, tools } = createFakeApi();
  extension(api);

  const ctx = makeContext("reconnect-session");

  // First call succeeds and populates the cache.
  const taskTool = tools.get("crew_task")!;
  const result1 = await taskTool.execute("call-1", { op: "upsert" }, undefined, undefined, ctx);
  expect(result1.isError).toBeFalsy();

  // Stop the daemon, then start it again with the same state dir and repo.
  await restartDaemon();

  // The second call must succeed despite the cached client being closed.
  // Against pre-fix code this fails with "connection closed by runtime".
  const result2 = await taskTool.execute("call-2", { op: "upsert" }, undefined, undefined, ctx);
  expect(result2.isError).toBeFalsy();
}, 60_000);
