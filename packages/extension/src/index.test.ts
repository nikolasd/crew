import { afterAll, afterEach, beforeAll, expect, spyOn, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync } from "node:fs";
import { join } from "node:path";

import type { ExtensionAPI, ExtensionCommandContext, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

import { z as zod } from "zod/v4";

import { validateRuntimeStatus } from "@nikolasd/batman-protocol/validate";

import extension from "./index";
import { getRuntimeStatus, type RuntimeStatusResult } from "./status";
import { BinaryIntegrityError } from "./platform";

import statusResultFixture from "../../../fixtures/omp/status-result.json" with { type: "json" };

const REPO_ROOT = join(import.meta.dir, "..", "..", "..");
const CREWD = join(REPO_ROOT, "target", "debug", "crewd");

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

test("exports an OMP extension factory", () => {
  expect(typeof extension).toBe("function");
});

// A minimal fake mirroring the subset of the real `ExtensionAPI` surface
// (node_modules/@oh-my-pi/pi-coding-agent/dist/types/extensibility/extensions/types.d.ts)
// that `index.ts` calls: `registerTool`, `registerCommand`, `zod.object`, and `on`.
interface FakeToolDefinition {
  readonly name: string;
  readonly execute: (toolCallId: string, params: unknown, signal: AbortSignal | undefined, onUpdate: undefined, ctx: ExtensionContext) => Promise<RuntimeStatusResult>;
}

interface FakeRegisteredCommand {
  readonly description?: string;
  readonly handler: (args: string, ctx: ExtensionCommandContext) => Promise<void>;
}

function createFakeApi(): {
  api: ExtensionAPI;
  tools: Map<string, FakeToolDefinition>;
  commands: Map<string, FakeRegisteredCommand>;
} {
  const tools = new Map<string, FakeToolDefinition>();
  const commands = new Map<string, FakeRegisteredCommand>();
  const api = {
    zod,
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    registerTool(tool: FakeToolDefinition) {
      tools.set(tool.name, tool);
    },
    registerCommand(name: string, options: FakeRegisteredCommand) {
      commands.set(name, options);
    },
    on() {
      // Not exercised by these tests.
    },
  };

  return { api: api as unknown as ExtensionAPI, tools, commands };
}
function fakeExtensionContext(cwd: string): ExtensionContext {
  const sessionManager = {
    getSessionId: () => "test-session-id-12345",
  };
  return {
    cwd,
    sessionManager: sessionManager as unknown as ExtensionContext["sessionManager"],
  } as unknown as ExtensionContext;
}
function fakeCommandContext(cwd: string, hasUI: boolean): { ctx: ExtensionCommandContext; notifications: string[] } {
  const notifications: string[] = [];
  const sessionManager = {
    getSessionId: () => "test-session-id-12345",
  };
  return {
    notifications,
    ctx: {
      cwd,
      hasUI,
      ui: { notify: (message: string) => notifications.push(message) },
      sessionManager: sessionManager as unknown as ExtensionCommandContext["sessionManager"],
    } as unknown as ExtensionCommandContext,
  };
}

test("registers crew_health plus every orchestration tool, and every slash command", () => {
  const { api, tools, commands } = createFakeApi();
  extension(api);
  expect([...tools.keys()]).toEqual(["crew_health", "crew_task", "crew_worker", "crew_profile", "crew_run", "crew_workspace", "crew_artifact", "crew_child", "crew_violation", "crew_message", "crew_approval", "crew_reconcile", "crew_doctor", "crew_runtime_install"]);
  expect([...commands.keys()]).toEqual(["crew-status", "crew", "crew-doctor", "crew-runtime-install"]);
});

// ---- Live-daemon path: a real foreground `crewd` the tool must reach. ----

let daemon: ReturnType<typeof Bun.spawn> | undefined;
let stateDir: string;
let repoDir: string;
const savedEnv: Record<string, string | undefined> = {};

function setEnv(key: string, value: string): void {
  if (!(key in savedEnv)) {
    savedEnv[key] = process.env[key];
  }
  process.env[key] = value;
}

function restoreEnv(): void {
  for (const [key, value] of Object.entries(savedEnv)) {
    if (value === undefined) {
      delete process.env[key];
    } else {
      process.env[key] = value;
    }
  }
  for (const key of Object.keys(savedEnv)) {
    delete savedEnv[key];
  }
}

function findSocket(state: string): string | undefined {
  const reposDir = join(state, "repos");
  if (!existsSync(reposDir)) {
    return undefined;
  }
  for (const entry of readdirSync(reposDir)) {
    const candidate = join(reposDir, entry, "runtime.sock");
    if (existsSync(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

async function waitForSocket(state: string): Promise<void> {
  for (let attempt = 0; attempt < 200; attempt++) {
    if (findSocket(state) !== undefined) {
      return;
    }
    await sleep(25);
  }
  throw new Error("runtime socket did not appear");
}

beforeAll(async () => {
  const build = Bun.spawnSync(["cargo", "build", "-p", "batman-runtime"], { cwd: REPO_ROOT });
  if (build.exitCode !== 0) {
    throw new Error(`cargo build failed: ${build.stderr.toString()}`);
  }

  stateDir = mkdtempSync("/tmp/bat-omp-s-");
  repoDir = mkdtempSync("/tmp/bat-omp-r-");
  mkdirSync(join(repoDir, ".git"));

  daemon = Bun.spawn([CREWD, "serve", "--foreground", "--state-dir", stateDir, "--repo", repoDir], { stdout: "ignore", stderr: "pipe" });

  await waitForSocket(stateDir);
}, 180_000);

afterAll(async () => {
  daemon?.kill("SIGTERM");
  await daemon?.exited;
});

afterEach(() => {
  restoreEnv();
});

test("crew_health tool reports a healthy runtime for a real foreground daemon", async () => {
  setEnv("CREW_STATE_DIR", stateDir);

  const { api, tools } = createFakeApi();
  extension(api);
  const tool = tools.get("crew_health")!;

  const ctx = fakeExtensionContext(repoDir);
  const result = await tool.execute("call-1", {}, undefined, undefined, ctx);

  expect(result.isError).toBeUndefined();
  expect(result.details).toMatchObject({
    running: true,
    protocol: { major: 1, minor: 0 },
    activeRuns: 0,
    protocolHealthy: true,
  });
});
test("crew_health tool reuses the cached client across a second call", async () => {
  setEnv("CREW_STATE_DIR", stateDir);

  const { api, tools } = createFakeApi();
  extension(api);
  const tool = tools.get("crew_health")!;
  const ctx = fakeExtensionContext(repoDir);

  const first = await tool.execute("call-1", {}, undefined, undefined, ctx);
  const second = await tool.execute("call-2", {}, undefined, undefined, ctx);

  expect(first.isError).toBeUndefined();
  expect(second.isError).toBeUndefined();
  expect((second.details as { projectId: string }).projectId).toBe((first.details as { projectId: string }).projectId);
});

test("crew_health tool returns a sanitized error when the runtime cannot be reached", async () => {
  const emptyState = mkdtempSync("/tmp/bat-omp-empty-");
  const brokenRepo = mkdtempSync("/tmp/bat-omp-broken-");
  mkdirSync(join(brokenRepo, ".git"));

  const invalidBinary = "/nonexistent/crewd-does-not-exist";
  setEnv("CREW_STATE_DIR", emptyState);
  setEnv("OMP_CREW_BINARY", invalidBinary);

  const { api, tools } = createFakeApi();
  extension(api);
  const tool = tools.get("crew_health")!;
  const ctx = fakeExtensionContext(brokenRepo);

  const result = await tool.execute("call-1", {}, undefined, undefined, ctx);

  expect(result.isError).toBe(true);
  const details = result.details as { code: string; message: string; doctorCommand: string };
  expect(typeof details.code).toBe("string");
  expect(details.code.length).toBeGreaterThan(0);
  expect(details.doctorCommand).toContain("crewd status --repo");
  expect(details.doctorCommand).toContain(brokenRepo);

  // Sanitized: no stack frames, no leaked environment values (e.g. the
  // invalid binary override path) anywhere in the returned payload.
  const serialized = JSON.stringify(result);
  expect(serialized).not.toContain(invalidBinary);
  expect(serialized).not.toMatch(/\n\s*at .+:\d+:\d+/);
  expect(details.message).not.toContain(invalidBinary);
});

test("crew_health surfaces a typed BinaryIntegrityError code without leaking its path", async () => {
  const emptyState = mkdtempSync("/tmp/bat-omp-empty-");
  const brokenRepo = mkdtempSync("/tmp/bat-omp-broken-");
  mkdirSync(join(brokenRepo, ".git"));

  const sensitivePath = "/leaf/package/dir/bin/crewd";

  const result = await getRuntimeStatus({
    ensureRuntimeOptions: {
      stateDir: emptyState,
      repository: brokenRepo,
      idleSeconds: 60,
      env: {},
      packagedBinaryResolver: () => {
        throw new BinaryIntegrityError("checksum-mismatch", `checksum mismatch for ${sensitivePath}: manifest ${sensitivePath}.json declares ` + "aaa, computed bbb");
      },
    },
    cache: { get: () => undefined, set: () => {} },
  });

  expect(result.isError).toBe(true);
  const details = result.details as { code: string; message: string; doctorCommand: string };
  // The specific typed code survives, not the generic "connection-failed".
  expect(details.code).toBe("checksum-mismatch");
  // But the error's message -- which embeds a filesystem path -- must never
  // be copied into the sanitized, user-facing result.
  expect(details.message).not.toContain(sensitivePath);
  expect(details.message).not.toMatch(/\n\s*at .+:\d+:\d+/);
  const serialized = JSON.stringify(result);
  expect(serialized).not.toContain(sensitivePath);
});

test("crew-status command notifies (not console.logs) in interactive mode", async () => {
  setEnv("CREW_STATE_DIR", stateDir);

  const { api, commands } = createFakeApi();
  extension(api);
  const command = commands.get("crew-status")!;

  const { ctx, notifications } = fakeCommandContext(repoDir, true);
  const logSpy = spyOn(console, "log");

  try {
    await command.handler("", ctx);
  } finally {
    logSpy.mockRestore();
  }

  expect(notifications.length).toBe(1);
  expect(notifications[0]).toContain("running");
  // A raw console.log in interactive mode would corrupt the TUI.
  expect(logSpy).not.toHaveBeenCalled();
});

test("crew-status command console.logs (not notify) outside interactive mode", async () => {
  setEnv("CREW_STATE_DIR", stateDir);

  const { api, commands } = createFakeApi();
  extension(api);
  const command = commands.get("crew-status")!;

  const { ctx, notifications } = fakeCommandContext(repoDir, false);
  const logged: string[] = [];
  const logSpy = spyOn(console, "log").mockImplementation((message: string) => {
    logged.push(message);
  });

  try {
    await command.handler("", ctx);
  } finally {
    logSpy.mockRestore();
  }

  expect(logged.length).toBe(1);
  expect(logged[0]).toContain("running");
  // ctx.ui.notify must not be called when hasUI is false; print/RPC relies on console.log only
  expect(notifications.length).toBe(0);
});

test("the golden runtime/status fixture validates against the canonical schema", () => {
  // Guards fixtures/omp/status-result.json against drift from the real
  // `RuntimeStatus` schema: nothing else in the suite loads this fixture.
  expect(validateRuntimeStatus(statusResultFixture)).toBe(true);
});
