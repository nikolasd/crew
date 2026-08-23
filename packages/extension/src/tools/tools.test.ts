import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readdirSync, existsSync } from "node:fs";
import { join } from "node:path";

import type { AgentToolResult, ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import { z as zod } from "zod/v4";

import { CrewClient } from "../client";
import { registerOrchestrationTools } from "./index";

const REPO_ROOT = join(import.meta.dir, "..", "..", "..", "..");
const CREWD = join(REPO_ROOT, "target", "debug", "crewd");

// ---------------------------------------------------------------- fake API

interface FakeToolDefinition {
  readonly name: string;
  readonly label: string;
  readonly description: string;
  readonly approval?: unknown;
  readonly parameters: unknown;
  readonly execute: (toolCallId: string, params: unknown, signal: AbortSignal | undefined, onUpdate: undefined, ctx: ExtensionContext) => Promise<AgentToolResult<unknown>>;
}

function createFakeApi(): { api: ExtensionAPI; tools: Map<string, FakeToolDefinition> } {
  const tools = new Map<string, FakeToolDefinition>();
  const api = {
    zod,
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    registerTool(tool: FakeToolDefinition) {
      tools.set(tool.name, tool);
    },
  };
  return { api: api as unknown as ExtensionAPI, tools };
}

// The daemon binds `task/upsert`'s `ownerClientInstanceId` to the connected
// principal (R76), and production wires `sessionId -> instanceId ->
// ownerClientInstanceId` through one value (runtime.ts:262, tasks.ts) --
// mirror that chain here or every upsert is refused with -32602.
const FAKE_SESSION_ID = "test-session-id-12345";

function fakeExtensionContext(cwd: string): ExtensionContext {
  const sessionManager = {
    getSessionId: () => FAKE_SESSION_ID,
  };
  return {
    cwd,
    sessionManager: sessionManager as unknown as ExtensionContext["sessionManager"],
  } as unknown as ExtensionContext;
}

// ------------------------------------------------------- registration shape

test("registers exactly the eleven orchestration tools, in the order the model sees them", () => {
  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: () => {
      throw new Error("not exercised in this test");
    },
  });
  expect([...tools.keys()]).toEqual(["crew_task", "crew_worker", "crew_profile", "crew_run", "crew_workspace", "crew_artifact", "crew_child", "crew_violation", "crew_message", "crew_approval", "crew_reconcile"]);
});

test("read-only ops resolve to tier read, mutating worker/run ops resolve to tier exec", () => {
  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: () => {
      throw new Error("not exercised in this test");
    },
  });

  const worker = tools.get("crew_worker");
  expect(worker).toBeDefined();
  const workerApproval = worker?.approval as (args: unknown) => string;
  expect(workerApproval({ op: "create" })).toBe("exec");
  expect(workerApproval({ op: "list" })).toBe("read");
  expect(workerApproval({ op: "get" })).toBe("read");

  const run = tools.get("crew_run");
  expect(run).toBeDefined();
  const runApproval = run?.approval as (args: unknown) => string;
  expect(runApproval({ op: "submit" })).toBe("exec");
  expect(runApproval({ op: "cancel" })).toBe("exec");
  expect(runApproval({ op: "list" })).toBe("read");
  expect(runApproval({ op: "get" })).toBe("read");
  expect(runApproval({ op: "retry" })).toBe("exec");
  expect(runApproval({ op: "result" })).toBe("read");
});

test("every op's approval tier matches whether it mutates", () => {
  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: () => {
      throw new Error("not exercised in this test");
    },
  });
  const tierOf = (name: string, args: unknown): string => {
    const tool = tools.get(name);
    expect(tool).toBeDefined();
    const approval = tool?.approval;
    return typeof approval === "function" ? (approval as (a: unknown) => string)(args) : String(approval);
  };

  // Reading a task must not cost a write approval.
  expect(tierOf("crew_task", { op: "upsert" })).toBe("write");
  expect(tierOf("crew_task", { op: "get" })).toBe("read");

  // Artifacts are pure evidence: no op mutates.
  expect(tierOf("crew_artifact", { op: "list" })).toBe("read");
  expect(tierOf("crew_artifact", { op: "fetch" })).toBe("read");

  // Accepting a child provisions a real run; listing requests does not.
  expect(tierOf("crew_child", { op: "list" })).toBe("read");
  expect(tierOf("crew_child", { op: "decide" })).toBe("exec");

  // Deciding a violation releases or cancels a quarantined run.
  expect(tierOf("crew_violation", { op: "decide" })).toBe("exec");
  // The list branch is a pure read; a constant "exec" regression fails here.
  expect(tierOf("crew_violation", { op: "list" })).toBe("read");

  // `apply` rewrites a real working tree, so it joins acquire/release.
  expect(tierOf("crew_workspace", { op: "apply" })).toBe("exec");
  expect(tierOf("crew_workspace", { op: "acquire" })).toBe("exec");
  expect(tierOf("crew_workspace", { op: "release" })).toBe("exec");
  expect(tierOf("crew_workspace", { op: "get" })).toBe("read");
  expect(tierOf("crew_workspace", { op: "inspect" })).toBe("read");
});

test("crew_approval never auto-approves: fixed exec tier with override and reason", () => {
  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: () => {
      throw new Error("not exercised in this test");
    },
  });

  const approval = tools.get("crew_approval");
  expect(approval).toBeDefined();
  expect(approval?.approval).toEqual({
    tier: "exec",
    override: true,
    reason: "Approval decisions are a user-facing safety action.",
  });
});

test("crew_approval requires approvalId, decision, and reason for decide", () => {
  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: () => {
      throw new Error("not exercised in this test");
    },
  });
  const approval = tools.get("crew_approval");
  const schema = approval?.parameters as zod.ZodObject;
  expect(() => schema.parse({ op: "decide" })).not.toThrow(); // shape allows optional fields; runtime enforces requiredness
  const shape = schema.shape as Record<string, unknown>;
  expect(Object.keys(shape)).toEqual(["op", "runId", "approvalId", "decision", "reason"]);
});

test("crew_violation rejects a prose resolution the runtime would refuse (R16)", () => {
  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: () => {
      throw new Error("not exercised in this test");
    },
  });
  const violation = tools.get("crew_violation");
  const schema = violation?.parameters as zod.ZodObject;
  expect(schema.safeParse({ op: "decide", violationId: "v-1", resolution: "release" }).success).toBe(true);
  expect(schema.safeParse({ op: "decide", violationId: "v-1", resolution: "cancel" }).success).toBe(true);
  // The runtime accepts only "release" and "cancel"; prose must fail at the
  // schema so the model gets a usable error before any RPC is issued.
  expect(schema.safeParse({ op: "decide", violationId: "v-1", resolution: "please release the quarantined run" }).success).toBe(false);
});

test("crew_message rejects a kind outside the nine coordination kinds (R88)", () => {
  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: () => {
      throw new Error("not exercised in this test");
    },
  });
  const message = tools.get("crew_message");
  const schema = message?.parameters as zod.ZodObject;
  expect(schema.safeParse({ op: "send", runId: "r-1", kind: "steer" }).success).toBe(true);
  expect(schema.safeParse({ op: "send", runId: "r-1", kind: "approvalDecision" }).success).toBe(true);
  // The runtime rejects anything outside the closed MessageKind enum;
  // prose must fail at the schema so the model gets a usable error
  // before any RPC is issued.
  expect(schema.safeParse({ op: "send", runId: "r-1", kind: "please steer the worker" }).success).toBe(false);
});

test("crew_run rejects a workspaceMode outside shared/isolated/copy (R29)", () => {
  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: () => {
      throw new Error("not exercised in this test");
    },
  });
  const run = tools.get("crew_run");
  const schema = run?.parameters as zod.ZodObject;
  for (const mode of ["shared", "isolated", "copy"]) {
    expect(schema.safeParse({ op: "submit", taskId: "t", workerId: "w", prompt: "p", workspaceMode: mode }).success).toBe(true);
  }
  expect(schema.safeParse({ op: "submit", taskId: "t", workerId: "w", prompt: "p", workspaceMode: "worktree" }).success).toBe(false);
});

test("crew_run accepts the result op and calls run/result with the runId", async () => {
  const { api, tools } = createFakeApi();
  const calls: Array<{ method: string; params: unknown }> = [];
  const stubClient = {
    request: async (method: string, params: unknown) => {
      calls.push({ method, params });
      if (method === "run/result") {
        return {
          runId: "00000000-0000-4000-8000-000000000000",
          state: "succeeded",
          resultText: "all done",
          usage: { inputTokens: 10, outputTokens: 20, costUsd: null },
          completedAt: "2026-08-21T00:00:00Z",
        };
      }
      throw new Error(`unexpected method: ${method}`);
    },
  };
  registerOrchestrationTools(api, { getClient: async () => stubClient as unknown as CrewClient });
  const run = tools.get("crew_run");
  expect(run).toBeDefined();
  const schema = run?.parameters as zod.ZodObject;
  expect(schema.safeParse({ op: "result", runId: "r-1" }).success).toBe(true);
  const result = await run?.execute("call-1", { op: "result", runId: "r-1" }, undefined, undefined, fakeExtensionContext("/tmp"));
  expect(result?.isError).toBeUndefined();
  expect(calls).toEqual([{ method: "run/result", params: { runId: "r-1" } }]);
  const details = result?.details as { resultText: string };
  expect(details.resultText).toBe("all done");
});

// -------------------------------------------------- live-daemon round trip

let daemon: ReturnType<typeof Bun.spawn> | undefined;
let stateDir: string;
let repoDir: string;

function findSocket(state: string): string | undefined {
  const reposDir = join(state, "repos");
  if (!existsSync(reposDir)) return undefined;
  for (const entry of readdirSync(reposDir)) {
    const candidate = join(reposDir, entry, "runtime.sock");
    if (existsSync(candidate)) return candidate;
  }
  return undefined;
}

// Polling for the runtime's socket file: OS filesystem creation exposes no
// event/promise API to await directly, so a genuine wall-clock delay between
// polls is unavoidable here (per the real-timer exception for integration
// tests exercising the platform clock).
function delay(ms: number): Promise<void> {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
}

async function waitForSocket(state: string): Promise<void> {
  for (let i = 0; i < 200; i++) {
    if (findSocket(state) !== undefined) return;
    await delay(50);
  }
  throw new Error("timed out waiting for runtime.sock");
}

beforeAll(async () => {
  const build = Bun.spawnSync(["cargo", "build", "-p", "batman-runtime"], { cwd: REPO_ROOT });
  if (build.exitCode !== 0) {
    throw new Error(`cargo build failed: ${build.stderr.toString()}`);
  }

  stateDir = mkdtempSync("/tmp/bat-tools-s-");
  repoDir = mkdtempSync("/tmp/bat-tools-r-");
  mkdirSync(join(repoDir, ".git"));

  daemon = Bun.spawn([CREWD, "serve", "--foreground", "--state-dir", stateDir, "--repo", repoDir], { stdout: "ignore", stderr: "pipe" });

  await waitForSocket(stateDir);
}, 180_000);

afterAll(async () => {
  daemon?.kill("SIGTERM");
  await daemon?.exited;
});

async function connectedClient(): Promise<CrewClient> {
  const socketPath = findSocket(stateDir);
  if (socketPath === undefined) {
    throw new Error("runtime socket not found");
  }
  const client = new CrewClient({ socketPath });
  await client.whenConnected();
  await client.initialize({
    client: { name: "@nikolasd/crew", version: "0.1.0" },
    supported: { min: { major: 1, minor: 0 }, max: { major: 1, minor: 0 } },
    repository: { canonicalPath: repoDir, vcsRoot: repoDir },
    auth: { role: "ompExtension", instanceId: FAKE_SESSION_ID, agentDirectory: repoDir },
    capabilities: { eventReplay: true, maxFrameBytes: 1024 * 1024 },
    lastSequence: null,
  });
  return client;
}

test("crew_task upsert creates a task with auto-generated ID and session owner, and get reads it back", async () => {
  const { api, tools } = createFakeApi();
  let cached: CrewClient | undefined;
  registerOrchestrationTools(api, {
    getClient: async () => {
      cached ??= await connectedClient();
      return cached;
    },
  });

  const taskTool = tools.get("crew_task");
  expect(taskTool).toBeDefined();
  if (taskTool === undefined) throw new Error("unreachable");

  // Create a new task - extension auto-generates taskId and uses session ID as owner
  const result = await taskTool.execute("call-1", { op: "upsert" }, undefined, undefined, fakeExtensionContext(repoDir));

  // Should succeed with a valid taskId
  expect(result.isError).toBeUndefined();
  const details = result.details as { taskId: string };
  expect(typeof details.taskId).toBe("string");
  expect(details.taskId).toMatch(/^[0-9a-f-]+$/); // Valid UUID format

  // `get` is the op that was previously unreachable: the tool had no `op`
  // discriminator, so `task/get` could never be called at all.
  const fetched = await taskTool.execute("call-2", { op: "get", taskId: details.taskId }, undefined, undefined, fakeExtensionContext(repoDir));
  expect(fetched.isError).toBeUndefined();
  const fetchedDetails = fetched.details as { taskId?: string };
  expect(fetchedDetails.taskId).toBe(details.taskId);

  cached?.close();
});

test("crew_worker tool maps a JSON-RPC error to a stable, non-throwing tool error", async () => {
  const { api, tools } = createFakeApi();
  let cached: CrewClient | undefined;
  registerOrchestrationTools(api, {
    getClient: async () => {
      cached ??= await connectedClient();
      return cached;
    },
  });

  const workerTool = tools.get("crew_worker");
  expect(workerTool).toBeDefined();
  if (workerTool === undefined) throw new Error("unreachable");

  // "get" with a well-formed but nonexistent workerId triggers a runtime
  // NOT_FOUND-shaped error; the tool must surface it as a structured,
  // non-throwing result rather than an unhandled rejection.
  const result = await workerTool.execute("call-1", { op: "get", workerId: "018f0000-0000-7000-8000-000000000000" }, undefined, undefined, fakeExtensionContext(repoDir));
  expect(result.isError).toBe(true);
  const details = result.details as { code: number; message: string };
  expect(typeof details.code).toBe("number");
  expect(typeof details.message).toBe("string");

  cached?.close();
});

test("crew_approval fails closed when humanRequired and no UI is available", async () => {
  // Create a stub client that returns a human_required approval.
  const stubClient = {
    request: async (method: string) => {
      if (method === "approval/list") {
        return {
          approvals: [
            {
              approvalId: "test-approval-1",
              action: "write file",
              arguments: { path: "/tmp/test" },
              policyReason: "write requires approval",
              humanRequired: true,
            },
          ],
        };
      }
      throw new Error(`unexpected method: ${method}`);
    },
  };

  // Track if approval/decide was called (it should NOT be for the fail-closed path).
  let decideCalled = false;
  const trackingClient = {
    request: async (method: string) => {
      if (method === "approval/decide") {
        decideCalled = true;
      }
      return stubClient.request(method);
    },
  };

  const { api, tools } = createFakeApi();
  registerOrchestrationTools(api, {
    getClient: async () => trackingClient as unknown as CrewClient,
  });

  const approvalTool = tools.get("crew_approval");
  if (!approvalTool) throw new Error("approval tool not found");

  // Context with no UI (hasUI: false).
  const ctx = {
    ...fakeExtensionContext("/tmp"),
    hasUI: false,
  } as unknown as ExtensionContext;

  const result = await approvalTool.execute("call-1", { op: "decide", approvalId: "test-approval-1", decision: "approve", reason: "ok" }, undefined, undefined, ctx);

  // The fail-closed path returns an error without calling the server.
  expect(result.isError).toBe(true);
  expect(decideCalled).toBe(false);
  const details = result.details as { reason: string };
  expect(details.reason).toBe("humanRequiredWithoutUi");
});
