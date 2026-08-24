import { expect, test } from "bun:test";
import type { AgentToolResult, ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import { z as zod } from "zod/v4";

import { CrewClient } from "../client";
import { CREW_PLAN_TOOL_NAME, registerPlanTool } from "./plan";

// A fake runtime client that records every `request` call and returns a
// canned result per method.
function fakeClient(handler?: (method: string, params: unknown) => unknown) {
  const calls: Array<{ method: string; params: unknown }> = [];
  const client = {
    calls,
    request: async (method: string, params: unknown): Promise<unknown> => {
      calls.push({ method, params });
      return handler ? handler(method, params) : { ok: true };
    },
  } as unknown as CrewClient;
  return { client, calls };
}

interface FakeUi {
  notify: (msg: string, level: string) => void;
  select: (prompt: string, options: string[], opts?: { timeout?: number }) => Promise<string | undefined>;
  input: (prompt: string, def: string, opts?: { timeout?: number }) => Promise<string | undefined>;
}

function fakeExtCtx(opts: { cwd: string; hasUI: boolean; ui?: Partial<FakeUi> }): ExtensionContext {
  const ui: FakeUi = {
    notify: opts.ui?.notify ?? (() => {}),
    select: opts.ui?.select ?? (async () => undefined),
    input: opts.ui?.input ?? (async () => undefined),
  };
  return {
    cwd: opts.cwd,
    hasUI: opts.hasUI,
    ui: ui as unknown as ExtensionContext["ui"],
    sessionManager: { getSessionId: () => "test-session-id" },
  } as unknown as ExtensionContext;
}

// Registers `crew_plan` against a fake API, returning the captured execute
// fn, the approval tier, and a `getClient` that hands back `client`.
function setupPlanTool(client: CrewClient) {
  let execute: ((input: unknown, ctx: ExtensionContext) => Promise<AgentToolResult<unknown>>) | undefined;
  let approval: unknown;
  const api = {
    zod,
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    registerTool(tool: { approval?: unknown; execute: (id: string, input: unknown, s: unknown, o: unknown, c: ExtensionContext) => Promise<AgentToolResult<unknown>> }) {
      execute = (input, c) => tool.execute("id", input, undefined, undefined, c);
      approval = tool.approval;
    },
  } as unknown as ExtensionAPI;
  const ctx = { getClient: async () => client } as never;
  registerPlanTool(api, ctx);
  return { tool: execute!, approval };
}

const SUBTASKS_WRITES = [{ id: "s1", description: "write tests", adapter: "claude", writes: true }];

test("crew_plan propose fails closed without UI when a human gate is required", async () => {
  const { client, calls } = fakeClient();
  const { tool } = setupPlanTool(client);
  const result = await tool({ op: "propose", runId: "run-1", taskText: "build", subtasks: SUBTASKS_WRITES }, fakeExtCtx({ cwd: "/tmp/crew-plan-test", hasUI: false }));
  expect(result.isError).toBe(true);
  expect(calls.map((c) => c.method)).toContain("plan/propose");
  expect(calls.map((c) => c.method)).not.toContain("plan/decide");
});

test("crew_plan propose auto-approves (model) when no human gate is required", async () => {
  const { client, calls } = fakeClient();
  const { tool } = setupPlanTool(client);
  const result = await tool({ op: "propose", runId: "run-1", taskText: "build", subtasks: [{ id: "s1", description: "d", adapter: "claude", writes: false }] }, fakeExtCtx({ cwd: "/tmp/crew-plan-test", hasUI: false }));
  expect(result.isError).not.toBe(true);
  const decide = calls.find((c) => c.method === "plan/decide");
  expect(decide).toBeDefined();
  expect((decide!.params as { approved: boolean; decidedBy: string }).approved).toBe(true);
  expect((decide!.params as { decidedBy: string }).decidedBy).toBe("model");
});

test("crew_plan propose runs the human gate and records a human approval", async () => {
  const ui = { notify: () => {}, select: async () => "Approve", input: async () => "looks good" };
  const { client, calls } = fakeClient();
  const { tool } = setupPlanTool(client);
  const result = await tool({ op: "propose", runId: "run-1", taskText: "build", subtasks: SUBTASKS_WRITES }, fakeExtCtx({ cwd: "/tmp/crew-plan-test", hasUI: true, ui }));
  expect(result.isError).not.toBe(true);
  const decide = calls.find((c) => c.method === "plan/decide");
  expect(decide).toBeDefined();
  const p = decide!.params as { approved: boolean; reason: string; decidedBy: string };
  expect(p.approved).toBe(true);
  expect(p.reason).toBe("looks good");
  expect(p.decidedBy).toBe("human");
});

test("crew_plan propose leaves the plan proposed on a dialog timeout", async () => {
  const ui = { notify: () => {}, select: async () => undefined, input: async () => "why" };
  const { client, calls } = fakeClient();
  const { tool } = setupPlanTool(client);
  const result = await tool({ op: "propose", runId: "run-1", taskText: "build", subtasks: SUBTASKS_WRITES }, fakeExtCtx({ cwd: "/tmp/crew-plan-test", hasUI: true, ui }));
  expect(result.isError).not.toBe(true);
  expect(calls.map((c) => c.method)).not.toContain("plan/decide");
});

test("crew_plan get reads via plan/get", async () => {
  const { client, calls } = fakeClient(() => ({ runId: "run-1", plan: { subtasks: [] }, approved: null }));
  const { tool } = setupPlanTool(client);
  const result = await tool({ op: "get", runId: "run-1" }, fakeExtCtx({ cwd: "/tmp/crew-plan-test", hasUI: false }));
  expect(result.isError).not.toBe(true);
  const get = calls.find((c) => c.method === "plan/get");
  expect(get).toBeDefined();
  expect((get!.params as { runId: string }).runId).toBe("run-1");
});

test("crew_plan propose op is exec tier, get is read tier", async () => {
  const { client } = fakeClient();
  const { approval } = setupPlanTool(client);
  const tierOf = (args: unknown) => (typeof approval === "function" ? (approval as (a: unknown) => string)(args) : String(approval));
  expect(tierOf({ op: "propose" })).toBe("exec");
  expect(tierOf({ op: "get" })).toBe("read");
});

test("crew_plan tool name is crew_plan", () => {
  expect(CREW_PLAN_TOOL_NAME).toBe("crew_plan");
});
