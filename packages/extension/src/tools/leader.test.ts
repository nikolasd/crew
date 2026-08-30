import { expect, test } from "bun:test";
import type { AgentToolResult, ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import { z as zod } from "zod/v4";

import { CrewClient, JsonRpcRemoteError } from "../client";
import { CREW_FINISH_TOOL_NAME, CREW_SEND_TOOL_NAME, CREW_SPAWN_TOOL_NAME, CREW_STATUS_TOOL_NAME, CREW_STOP_TOOL_NAME, CREW_TRANSCRIPT_TOOL_NAME, registerLeaderTools } from "./leader";
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

function fakeExtCtx(opts: { cwd?: string; hasUI?: boolean } = {}): ExtensionContext {
  return {
    cwd: opts.cwd ?? "/tmp/crew-leader-test",
    hasUI: opts.hasUI ?? false,
    ui: { notify: () => {}, select: async () => undefined, input: async () => undefined } as unknown as ExtensionContext["ui"],
    sessionManager: { getSessionId: () => "test-session-id" },
  } as unknown as ExtensionContext;
}

// Registers all leader tools, returning a map from tool name to its execute
// fn plus the captured approval tiers.
function setupLeaderTools(client: CrewClient) {
  const tools = new Map<string, (input: unknown, ctx: ExtensionContext) => Promise<AgentToolResult<unknown>>>();
  const approvals = new Map<string, unknown>();
  const api = {
    zod,
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    registerTool(tool: { name: string; approval?: unknown; execute: (id: string, input: unknown, s: unknown, o: unknown, c: ExtensionContext) => Promise<AgentToolResult<unknown>> }) {
      tools.set(tool.name, (input, c) => tool.execute("id", input, undefined, undefined, c));
      approvals.set(tool.name, tool.approval);
    },
  } as unknown as ExtensionAPI;
  const ctx = { getClient: async () => client } as never;
  registerLeaderTools(api, ctx);
  return { tools, approvals };
}

const defaultHandler = (method: string): unknown => {
  switch (method) {
    case "plan/get":
      return { runId: "plan-1", plan: { subtasks: [{ id: "s1", description: "do X", adapter: "claude" }] }, approved: true };
    case "worker/list":
      return { workers: [{ workerId: "w-existing", adapter: "claude" }] };
    case "worker/create":
      return { workerId: "w-new" };
    case "run/submit":
      return { runId: "run-new" };
    case "run/list":
      return { runs: [] };
    case "events/replay":
      return [
        { sequence: 1, runId: "plan-1", event: { type: "runEvent" } },
        { sequence: 2, runId: "other", event: { type: "messageEvent" } },
        { sequence: 3, runId: "plan-1", event: { type: "taskEvent" } },
      ];
    default:
      return { ok: true };
  }
};

test("crew_spawn reuses an existing worker and submits with plan linkage", async () => {
  const { client, calls } = fakeClient(defaultHandler);
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_SPAWN_TOOL_NAME)!({ op: "spawn", taskId: "task-1", planId: "plan-1", subtaskId: "s1" }, fakeExtCtx());
  expect(result.isError).not.toBe(true);
  expect(calls.map((c) => c.method)).toContain("worker/list");
  expect(calls.map((c) => c.method)).not.toContain("worker/create");
  const submit = calls.find((c) => c.method === "run/submit");
  expect(submit).toBeDefined();
  const p = submit!.params as { taskId: string; workerId: string; prompt: string; planId: string; subtaskId: string };
  expect(p.taskId).toBe("task-1");
  expect(p.workerId).toBe("w-existing");
  expect(p.prompt).toBe("do X");
  expect(p.planId).toBe("plan-1");
  expect(p.subtaskId).toBe("s1");
});

test("crew_spawn creates a worker when none matches the adapter", async () => {
  const { client, calls } = fakeClient((method) => {
    if (method === "worker/list") return { workers: [{ workerId: "w-other", adapter: "codex" }] };
    return defaultHandler(method);
  });
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_SPAWN_TOOL_NAME)!({ op: "spawn", taskId: "task-1", planId: "plan-1", subtaskId: "s1" }, fakeExtCtx());
  expect(result.isError).not.toBe(true);
  const create = calls.find((c) => c.method === "worker/create");
  expect(create).toBeDefined();
  expect((create!.params as { adapter: string }).adapter).toBe("claude");
  const submit = calls.find((c) => c.method === "run/submit");
  expect((submit!.params as { workerId: string }).workerId).toBe("w-new");
});

test("crew_spawn errors when the subtask is absent from the plan", async () => {
  const { client } = fakeClient((method) => (method === "plan/get" ? { runId: "plan-1", plan: { subtasks: [] }, approved: true } : defaultHandler(method)));
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_SPAWN_TOOL_NAME)!({ op: "spawn", taskId: "task-1", planId: "plan-1", subtaskId: "missing" }, fakeExtCtx());
  expect(result.isError).toBe(true);
});

test("crew_send forwards to message/send with default followUp kind", async () => {
  const { client, calls } = fakeClient();
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_SEND_TOOL_NAME)!({ op: "send", runId: "run-1", payload: "next step" }, fakeExtCtx());
  expect(result.isError).not.toBe(true);
  const send = calls.find((c) => c.method === "message/send");
  expect(send).toBeDefined();
  expect((send!.params as { kind: string; payload: string }).kind).toBe("followUp");
  expect((send!.params as { payload: string }).payload).toBe("next step");
});

test("crew_status snapshots via run/list", async () => {
  const { client, calls } = fakeClient(defaultHandler);
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_STATUS_TOOL_NAME)!({ op: "snapshot", taskId: "task-1" }, fakeExtCtx());
  expect(result.isError).not.toBe(true);
  const list = calls.find((c) => c.method === "run/list");
  expect(list).toBeDefined();
  expect((list!.params as { taskId: string }).taskId).toBe("task-1");
});

test("crew_transcript filters events to the run and normalizes", async () => {
  const { client } = fakeClient(defaultHandler);
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_TRANSCRIPT_TOOL_NAME)!({ op: "replay", runId: "plan-1" }, fakeExtCtx());
  expect(result.isError).not.toBe(true);
  const events = (result.details as { events: Array<{ sequence: number; type: string }> }).events;
  expect(events.map((e) => e.sequence).sort()).toEqual([1, 3]);
  expect(events.every((e) => e.type === "runEvent" || e.type === "taskEvent")).toBe(true);
});

test("crew_stop done sends a wrap-up then cancels immediately -- there is no soft/graceful distinction (CREW-35)", async () => {
  const { client, calls } = fakeClient();
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_STOP_TOOL_NAME)!({ op: "stop", runId: "run-1", outcome: "done" }, fakeExtCtx());
  expect(result.isError).not.toBe(true);
  expect(calls.map((c) => c.method)).toContain("message/send");
  const cancel = calls.find((c) => c.method === "run/cancel");
  expect(cancel).toBeDefined();
  // No `mode` param: the daemon never reads one, so sending it claimed a
  // distinction ("soft" cancel) that didn't exist. `done`'s only real
  // difference from `abort` is the wrap-up message sent above.
  expect(cancel!.params).toEqual({ runId: "run-1" });
});

test("crew_stop abort cancels immediately without a wrap-up message", async () => {
  const { client, calls } = fakeClient();
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_STOP_TOOL_NAME)!({ op: "stop", runId: "run-1", outcome: "abort" }, fakeExtCtx());
  expect(result.isError).not.toBe(true);
  expect(calls.map((c) => c.method)).not.toContain("message/send");
  const cancel = calls.find((c) => c.method === "run/cancel");
  expect(cancel).toBeDefined();
  expect((cancel!.params as { runId: string }).runId).toBe("run-1");
});

test("crew_finish cancels each run id and reports failures", async () => {
  const { client } = fakeClient((method, params) => {
    if (method === "run/cancel") {
      const p = params as { runId: string };
      if (p.runId === "r2") throw new JsonRpcRemoteError(-32000, "gone", undefined);
    }
    return { ok: true };
  });
  const { tools } = setupLeaderTools(client);
  const result = await tools.get(CREW_FINISH_TOOL_NAME)!({ op: "finish", runIds: ["r1", "r2"] }, fakeExtCtx());
  expect(result.isError).not.toBe(true);
  const d = result.details as { cancelled: string[]; failed: Array<{ runId: string }> };
  expect(d.cancelled).toEqual(["r1"]);
  expect(d.failed.map((f) => f.runId)).toEqual(["r2"]);
});

test("leader tool tiers: spawn/stop/finish exec, send write, status/transcript read", async () => {
  const { client } = fakeClient();
  const { approvals } = setupLeaderTools(client);
  const tierOf = (name: string, args: unknown) => {
    const a = approvals.get(name);
    return typeof a === "function" ? (a as (x: unknown) => string)(args) : String(a);
  };
  expect(tierOf(CREW_SPAWN_TOOL_NAME, {})).toBe("exec");
  expect(tierOf(CREW_STOP_TOOL_NAME, {})).toBe("exec");
  expect(tierOf(CREW_FINISH_TOOL_NAME, {})).toBe("exec");
  expect(tierOf(CREW_SEND_TOOL_NAME, {})).toBe("write");
  expect(tierOf(CREW_STATUS_TOOL_NAME, {})).toBe("read");
  expect(tierOf(CREW_TRANSCRIPT_TOOL_NAME, {})).toBe("read");
});
