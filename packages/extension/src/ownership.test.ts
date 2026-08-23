// End-to-end test for extension identity and task ownership (TODO #68).
//
// Proves the sessionId → instanceId → ownerClientInstanceId chain holds:
// when the extension connects with a sessionId, creates a task owned by
// that sessionId, and then decides an approval or violation, the runtime
// accepts the decision because the connection principal matches the task owner.
//
// Uses a live foreground daemon with database-seeded test data to create the
// approval/violation preconditions that a real adapter would produce during
// execution. This test is self-contained (own daemon lifecycle) to avoid
// ordering dependencies with index.test.ts.

import { afterAll, beforeAll, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync } from "node:fs";
import { join } from "node:path";

import type { ExtensionAPI, ExtensionCommandContext, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

import { z as zod } from "zod/v4";

import extension from "./index";
import { repositoryId } from "./runtime";

const REPO_ROOT = join(import.meta.dir, "..", "..", "..");
const CREWD = join(REPO_ROOT, "target", "debug", "crewd");

// ---- Daemon lifecycle (self-contained) ----

let daemonProcess: { kill: (s: "SIGTERM") => void; exited: Promise<unknown> } = { kill: () => {}, exited: Promise.resolve(undefined) };
let stateDir = "";
let repoDir = "";
const savedEnv: Record<string, string | undefined> = {};

beforeAll(async () => {
  const build = Bun.spawnSync(["cargo", "build", "-p", "batman-runtime"], { cwd: REPO_ROOT });
  if (build.exitCode !== 0) {
    throw new Error(`cargo build failed: ${build.stderr.toString()}`);
  }

  stateDir = mkdtempSync("/tmp/bat-own-s-");
  repoDir = mkdtempSync("/tmp/bat-own-r-");
  mkdirSync(join(repoDir, ".git"));

  // Real spawn wait required: the daemon must bind its socket before tests can connect.
  // eslint-disable-next-line no-setTimeout
  daemonProcess = Bun.spawn([CREWD, "serve", "--foreground", "--state-dir", stateDir, "--repo", repoDir], {
    stdout: "ignore",
    stderr: "pipe",
  });

  // Wait for the runtime socket to appear (real wall-clock wait for external process).
  // eslint-disable-next-line no-setTimeout
  const { promise: ready, resolve: onReady } = Promise.withResolvers<void>();
  const check = () => {
    const reposDir = join(stateDir, "repos");
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
  await ready;
}, 180_000);

afterAll(async () => {
  daemonProcess.kill("SIGTERM");
  await daemonProcess.exited;
});

// ---- Test helpers ----

function databasePath(): string {
  return join(stateDir, "repos", repositoryId(repoDir), "runtime.db");
}

/** Derive the project_id (UUID format) from the repository hex hash. */
function projectIdFromRepo(repo: string): string {
  const hex = repositoryId(repo);
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`;
}

function seedTestData(ownerInstanceId: string): { taskId: string; runId: string; approvalId: string; violationId: string } {
  const db = new (require("bun:sqlite").Database)(databasePath());
  const now = new Date().toISOString();
  const pid = projectIdFromRepo(repoDir);

  const taskId = crypto.randomUUID();
  const workerId = crypto.randomUUID();
  const runId = crypto.randomUUID();
  const approvalId = crypto.randomUUID();
  const violationId = crypto.randomUUID();

  // task: task_id, project_id, owner_client_instance_id, revision, created_at, updated_at
  db.run(
    `INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?5)`,
    taskId,
    pid,
    ownerInstanceId,
    0,
    now,
  );

  // worker_profiles: id, fingerprint, adapter, model, permission_envelope (no created_at)
  const profileId = `profile-${crypto.randomUUID()}`;
  db.run(
    `INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
     VALUES (?1, ?2, ?3, ?4, ?5)`,
    profileId,
    "sha256:test",
    "claude",
    "claude-sonnet-4-20250514",
    "{}",
  );

  // workers: worker_id, project_id, profile_id, parent_worker_id, created_at, resolved_profile_json
  db.run(
    `INSERT INTO workers (worker_id, project_id, profile_id, parent_worker_id, created_at)
     VALUES (?1, ?2, ?3, NULL, ?4)`,
    workerId,
    pid,
    profileId,
    now,
  );

  // runs: run in 'waitingUser' state (the state after approval request is created)
  db.run(
    `INSERT INTO runs (run_id, task_id, worker_id, state,
                       flags_degraded_control, flags_needs_reconciliation, flags_protocol_unhealthy,
                       flags_policy_quarantined, flags_workspace_dirty, flags_children_active,
                       created_at, started_at)
     VALUES (?1, ?2, ?3, 'waitingUser', 0, 0, 0, 0, 0, 0, ?4, ?4)`,
    runId,
    taskId,
    workerId,
    now,
  );

  // approvals: approval_id, run_id, task_id, action, arguments, human_required, policy_reason, created_at
  db.run(
    `INSERT INTO approvals (approval_id, run_id, task_id, action, arguments,
                            human_required, policy_reason, created_at)
     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)`,
    approvalId,
    runId,
    taskId,
    "write file",
    '{"path":"/tmp/test"}',
    "file write requires approval",
    now,
  );

  // policy_violations: violation_id, run_id, task_id, worker_id, vendor_child_id, vendor_parent_ref, action, created_at
  db.run(
    `INSERT INTO policy_violations (violation_id, run_id, task_id, worker_id,
                                    vendor_child_id, vendor_parent_ref, action, created_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)`,
    violationId,
    runId,
    taskId,
    workerId,
    "child-1",
    "parent-1",
    "quarantine",
    now,
  );

  db.close();
  return { taskId, runId, approvalId, violationId };
}

function saveEnv(key: string): void {
  if (!(key in savedEnv)) savedEnv[key] = process.env[key];
}

function setEnvVar(key: string, value: string): void {
  saveEnv(key);
  process.env[key] = value;
}

function restoreEnvVars(): void {
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

// ---- Fake ExtensionAPI (mirrors index.test.ts pattern) ----

interface FakeToolDefinition {
  readonly name: string;
  readonly execute: (toolCallId: string, params: unknown, signal: AbortSignal | undefined, onUpdate: undefined, ctx: ExtensionContext) => Promise<{ isError?: boolean; details?: unknown }>;
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
    on() {},
    appendEntry() {},
  };

  return { api: api as unknown as ExtensionAPI, tools, commands };
}

function makeContext(sessionId: string): ExtensionContext {
  return {
    cwd: repoDir,
    sessionManager: { getSessionId: () => sessionId } as ExtensionContext["sessionManager"],
  } as unknown as ExtensionContext;
}

// ---- Tests ----

test("matching sessionId allows task owner to decide approval and violation", async () => {
  setEnvVar("CREW_STATE_DIR", stateDir);

  const sessionId = "own-test-session-1";
  const { approvalId, violationId } = seedTestData(sessionId);

  const { api, tools } = createFakeApi();
  extension(api);

  const ctx = makeContext(sessionId);
  const approvalTool = tools.get("crew_approval")!;
  const violationTool = tools.get("crew_violation")!;

  // Decide the approval — should succeed because instanceId (from sessionId)
  // matches the task's ownerClientInstanceId.
  const approvalResult = await approvalTool.execute(
    "call-1",
    {
      op: "decide",
      approvalId,
      decision: "approve",
      reason: "test approval",
    },
    undefined,
    undefined,
    ctx,
  );

  expect(approvalResult.isError).toBeUndefined();
  if (typeof approvalResult === "object" && approvalResult !== null && "details" in approvalResult) {
    expect((approvalResult as Record<string, unknown>).details).toMatchObject({ outcome: "decided" });
  } else {
    throw new Error("approval result missing details");
  }

  // Decide the violation — should succeed for the same reason.
  const violationResult = await violationTool.execute(
    "call-2",
    {
      op: "decide",
      violationId,
      resolution: "release",
    },
    undefined,
    undefined,
    ctx,
  );

  expect(violationResult.isError).toBeUndefined();
  if (typeof violationResult === "object" && violationResult !== null && "details" in violationResult) {
    expect((violationResult as Record<string, unknown>).details).toMatchObject({ outcome: "decided" });
  } else {
    throw new Error("violation result missing details");
  }

  restoreEnvVars();
});

test("mismatched sessionId forbids approval and violation decisions", async () => {
  setEnvVar("CREW_STATE_DIR", stateDir);

  // Seed data owned by one session.
  const ownerSession = "own-test-session-owner";
  const { approvalId, violationId } = seedTestData(ownerSession);

  // Connect with a different session — instanceId will not match the task owner.
  const { api, tools } = createFakeApi();
  extension(api);

  const ctx = makeContext("own-test-session-imposter");
  const approvalTool = tools.get("crew_approval")!;
  const violationTool = tools.get("crew_violation")!;

  // Approval decide should fail with Forbidden.
  const approvalResult = await approvalTool.execute(
    "call-3",
    {
      op: "decide",
      approvalId,
      decision: "approve",
      reason: "should be rejected",
    },
    undefined,
    undefined,
    ctx,
  );

  expect(approvalResult.isError).toBe(true);
  if (typeof approvalResult === "object" && approvalResult !== null && "details" in approvalResult) {
    const details = (approvalResult as Record<string, unknown>).details;
    if (typeof details === "object" && details !== null && "message" in details) {
      expect((details as Record<string, unknown>).message).toContain("does not own");
    } else {
      throw new Error("approval error missing message");
    }
  } else {
    throw new Error("approval result missing details");
  }

  // Violation decide should also fail with Forbidden.
  const violationResult = await violationTool.execute(
    "call-4",
    {
      op: "decide",
      violationId,
      resolution: "release",
    },
    undefined,
    undefined,
    ctx,
  );

  expect(violationResult.isError).toBe(true);
  if (typeof violationResult === "object" && violationResult !== null && "details" in violationResult) {
    const details = (violationResult as Record<string, unknown>).details;
    if (typeof details === "object" && details !== null && "message" in details) {
      expect((details as Record<string, unknown>).message).toContain("does not own");
    } else {
      throw new Error("violation error missing message");
    }
  } else {
    throw new Error("violation result missing details");
  }

  restoreEnvVars();
});
