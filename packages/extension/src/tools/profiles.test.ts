import { expect, test } from "bun:test";
import type { AgentToolResult, ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { z as zod } from "zod/v4";

import { CrewClient, JsonRpcRemoteError } from "../client";
import { injectTuiMode, registerProfileTool } from "./profiles";

function fakeClient(handler?: (method: string, params: unknown) => unknown) {
  const calls: Array<{ method: string; params: unknown }> = [];
  const client = {
    calls,
    request: async (method: string, params: unknown): Promise<unknown> => {
      calls.push({ method, params });
      return handler ? handler(method, params) : { profileId: "profile-1", sequence: 1 };
    },
  } as unknown as CrewClient;
  return { client, calls };
}

function fakeExtCtx(cwd: string): ExtensionContext {
  return { cwd } as unknown as ExtensionContext;
}

function setupProfileTool(client: CrewClient) {
  let execute: ((input: unknown, ctx: ExtensionContext) => Promise<AgentToolResult<unknown>>) | undefined;
  const api = {
    zod,
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    registerTool(tool: { execute: (id: string, input: unknown, s: unknown, o: unknown, c: ExtensionContext) => Promise<AgentToolResult<unknown>> }) {
      execute = (input, c) => tool.execute("id", input, undefined, undefined, c);
    },
  } as unknown as ExtensionAPI;
  const ctx = { getClient: async () => client } as never;
  registerProfileTool(api, ctx);
  return { tool: execute! };
}

function tempRepo(): string {
  return mkdtempSync("/tmp/bat-profiles-test-");
}

function writeRepoConfig(repository: string, contents: string): string {
  const dir = join(repository, ".omp");
  mkdirSync(dir, { recursive: true });
  const path = join(dir, "crew.json");
  writeFileSync(path, contents);
  return path;
}

// ------------------------------------------------------- resolution order

test("crew_profile uses the explicit model when given and nothing is configured yet", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  const register = calls.find((c) => c.method === "profile/register");
  expect((register!.params as { model: string }).model).toBe("sonnet");
});

test("crew_profile treats an explicit model matching the configured one as a no-op success", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "opus", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  const register = calls.find((c) => c.method === "profile/register");
  expect((register!.params as { model: string }).model).toBe("opus");
});

// CREW-8's original symptom: a hallucinating leader invents a model name.
// An explicit param that conflicts with an already-stored model must be
// refused, not silently applied and not silently dropped -- either would
// hide the disagreement from whoever is supposed to resolve it.
test("crew_profile refuses with a typed conflict error when an explicit model differs from the configured one, and never calls profile/register", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).toBe(true);
  expect((result.details as { code: string; configuredModel: string }).code).toBe("model-conflict");
  expect((result.details as { code: string; configuredModel: string }).configuredModel).toBe("opus");
  expect(calls.map((c) => c.method)).not.toContain("profile/register");
});

test("crew_profile falls back to the configured model when none is given explicitly", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  const register = calls.find((c) => c.method === "profile/register");
  expect((register!.params as { model: string }).model).toBe("opus");
});

test("crew_profile returns a typed model-not-configured error, and never calls profile/register, when neither is available", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  const result = await tool({ adapter: "claude", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).toBe(true);
  expect((result.details as { code: string }).code).toBe("model-not-configured");
  expect(calls.map((c) => c.method)).not.toContain("profile/register");
});

// ------------------------------------------------------------- persistence

test("crew_profile persists an explicit model into the repo layer when none was configured", async () => {
  const repository = tempRepo();
  const { client } = fakeClient();
  const { tool } = setupProfileTool(client);

  await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  const written = JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8"));
  expect(written.adapters.claude.model).toBe("sonnet");
});

test("crew_profile preserves existing keys when persisting", async () => {
  const repository = tempRepo();
  writeRepoConfig(repository, '{"approval":"auto","adapters":{"codex":{"model":"gpt-5"}}}');
  const { client } = fakeClient();
  const { tool } = setupProfileTool(client);

  await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  const written = JSON.parse(readFileSync(join(repository, ".omp", "crew.json"), "utf8"));
  expect(written.approval).toBe("auto");
  expect(written.adapters.codex.model).toBe("gpt-5");
  expect(written.adapters.claude.model).toBe("sonnet");
});

test("crew_profile never (re-)persists when the explicit model just matches what's already configured", async () => {
  const repository = tempRepo();
  const path = writeRepoConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  const before = readFileSync(path, "utf8");
  const { client } = fakeClient();
  const { tool } = setupProfileTool(client);

  await tool({ adapter: "claude", model: "opus", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(readFileSync(path, "utf8")).toBe(before);
});

test("crew_profile never persists when profile/register itself failed", async () => {
  const repository = tempRepo();
  // `callOrchestration` shapes a thrown JsonRpcRemoteError into an
  // isError result -- a rejecting client is the real error path.
  const rejecting = {
    request: async () => {
      throw new JsonRpcRemoteError(-32602, "bad params", undefined);
    },
  } as unknown as CrewClient;
  const { tool } = setupProfileTool(rejecting);

  const result = await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).toBe(true);
  expect(() => readFileSync(join(repository, ".omp", "crew.json"), "utf8")).toThrow();
});

test("crew_profile warns rather than throwing when persistence fails after a successful registration", async () => {
  const repository = tempRepo();
  // Registration is already durable by the time persistConfiguredModel
  // runs -- simulate a concurrent process corrupting the repo layer file
  // in the window between the read (resolveConfiguredModel, at the top
  // of execute) and the write, by mutating it as a side effect of the
  // mocked profile/register call itself.
  const racy = {
    request: async () => {
      mkdirSync(join(repository, ".omp"), { recursive: true });
      writeFileSync(join(repository, ".omp", "crew.json"), "{ not valid json");
      return { profileId: "profile-1", sequence: 1 };
    },
  } as unknown as CrewClient;
  const { tool } = setupProfileTool(racy);

  const result = await tool({ adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }, fakeExtCtx(repository));

  expect(result.isError).not.toBe(true);
  expect(result.content.some((c) => "text" in c && c.text.includes("Warning"))).toBe(true);
});

// -------------------------------------------------------------- injection

test("crew_profile fills in mode: tui for a reserved adapter when the caller omits it", async () => {
  const repository = tempRepo();
  const { client, calls } = fakeClient();
  const { tool } = setupProfileTool(client);

  await tool({ adapter: "claude", model: "sonnet" }, fakeExtCtx(repository));

  const register = calls.find((c) => c.method === "profile/register");
  const startupOptions = (register!.params as { startupOptions: Record<string, unknown> }).startupOptions;
  expect((startupOptions.claude as { mode: string }).mode).toBe("tui");
});

test("injectTuiMode never overrides an explicit mode, including an explicit headless", () => {
  expect(injectTuiMode("claude", { claude: { mode: "headless" } })).toEqual({ claude: { mode: "headless" } });
});

test("injectTuiMode leaves a non-reserved adapter's startup options untouched", () => {
  expect(injectTuiMode("terminalDegraded", { terminalDegraded: { backend: "tmux" } })).toEqual({ terminalDegraded: { backend: "tmux" } });
});

test("injectTuiMode preserves other keys already present on the reserved adapter's own options", () => {
  expect(injectTuiMode("claude", { claude: { permissionMode: "max" } })).toEqual({ claude: { permissionMode: "max", mode: "tui" } });
});
