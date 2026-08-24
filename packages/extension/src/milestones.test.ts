import { expect, test } from "bun:test";

import type { EventEnvelope, RuntimeEvent, RuntimeEventKind } from "@nikolasd/crew-protocol";
import { attachMilestoneBridge, formatDigest, MilestoneTracker, type RunLookup } from "./milestones";
import type { MonitorController } from "./monitor/controller";
import type { MonitorRow } from "./monitor/model";

const RUN_KIND_BY_STATE: Record<string, string> = {
  queued: "runQueued",
  starting: "runStarting",
  working: "runWorking",
  waitingUser: "runWaitingUser",
  paused: "runPaused",
  succeeded: "runSucceeded",
  failed: "runFailed",
  cancelled: "runCancelled",
  lost: "runLost",
};

let sequenceCounter = 0;
function envelope(overrides: Partial<EventEnvelope> & { event: RuntimeEvent }): EventEnvelope {
  sequenceCounter += 1;
  return {
    sequence: overrides.sequence ?? sequenceCounter,
    timestamp: overrides.timestamp ?? "2026-01-01T00:00:00Z",
    projectId: "018f0000-0000-7000-8000-000000000000",
    taskId: overrides.taskId ?? null,
    workerId: overrides.workerId ?? null,
    runId: overrides.runId ?? null,
    parentWorkerId: null,
    source: "runtime",
    vendorEventRef: null,
    event: overrides.event,
  };
}

function run(id: string, state: string): EventEnvelope {
  return envelope({
    runId: id,
    event: {
      type: "runEvent",
      payload: {
        kind: (RUN_KIND_BY_STATE[state] ?? "runWorking") as RuntimeEventKind,
        runId: id,
        taskId: "task-1",
        workerId: "worker-1",
        state,
      },
    },
  });
}

function message(): EventEnvelope {
  return envelope({
    runId: "run-1",
    event: {
      type: "messageEvent",
      payload: { kind: "messageSent", messageId: "m1", runId: "run-1", taskId: "task-1", deliveryState: "recorded" },
    },
  });
}

function tracker(): MilestoneTracker {
  return new MilestoneTracker();
}

const ROWS: RunLookup = {
  "run-1": {
    runId: "run-1",
    taskId: "task-1",
    workerId: "worker-1",
    state: "working",
    flags: {
      degradedControl: false,
      needsReconciliation: false,
      protocolUnhealthy: false,
      policyQuarantined: false,
      workspaceDirty: false,
      childrenActive: false,
    },
    pendingApprovalCount: 0,
    openViolations: {},
    firstSeenAt: "2026-01-01T00:00:00Z",
    lastEventAt: "2026-01-01T00:00:00Z",
    lastAppliedSequence: 1,
    adapter: "claude",
  } as MonitorRow,
};

test("terminal run states are milestones", () => {
  const t = tracker();
  for (const state of ["succeeded", "failed", "cancelled", "lost"]) {
    expect(t.isMilestone(run("run-1", state))).toBe(true);
  }
});

test("non-terminal run states are not milestones", () => {
  const t = tracker();
  for (const state of ["queued", "starting", "idle", "waitingUser", "paused"]) {
    expect(t.isMilestone(run("run-1", state))).toBe(false);
  }
});

test("first working is a milestone, later workings are not (per run)", () => {
  const t = tracker();
  expect(t.isMilestone(run("run-1", "working"))).toBe(true);
  expect(t.isMilestone(run("run-1", "working"))).toBe(false);
  expect(t.isMilestone(run("run-1", "working"))).toBe(false);
  expect(t.isMilestone(run("run-2", "working"))).toBe(true);
});

test("worker question / timeout / budget / escalation are milestones", () => {
  const t = tracker();
  expect(
    t.isMilestone(
      envelope({
        runId: "run-1",
        event: { type: "workerQuestion", payload: { runId: "run-1", taskId: "task-1", workerId: "w1", question: "continue?" } },
      }),
    ),
  ).toBe(true);
  expect(
    t.isMilestone(
      envelope({
        runId: "run-1",
        event: { type: "workerTimeout", payload: { runId: "run-1", taskId: "task-1", workerId: "w1", kind: "inactivity", sinceMs: 0 } },
      }),
    ),
  ).toBe(true);
  expect(
    t.isMilestone(
      envelope({
        runId: "run-1",
        event: { type: "budgetExceeded", payload: { runId: "run-1", taskId: "task-1", workerId: "w1", turnsUsed: 10, turnLimit: 10 } },
      }),
    ),
  ).toBe(true);
  expect(
    t.isMilestone(
      envelope({
        runId: "run-1",
        event: { type: "escalationRaised", payload: { runId: "run-1", taskId: "task-1", workerId: "w1", reason: "write_violation", question: null } },
      }),
    ),
  ).toBe(true);
});

test("noise never fires", () => {
  const t = tracker();
  expect(t.isMilestone(message())).toBe(false);
  // The first working is a milestone; a repeat on the same run is not.
  expect(t.isMilestone(run("run-1", "working"))).toBe(true);
  expect(t.isMilestone(run("run-1", "working"))).toBe(false);
});

test("failed digest contains the two-consecutive-failures rule and reason", () => {
  const rows: RunLookup = {
    "run-1": { ...ROWS["run-1"], latestActivity: "process exited 1" } as MonitorRow,
  };
  const digest = formatDigest(run("run-1", "failed"), rows);
  expect(digest).toBeDefined();
  expect(digest).toContain("FAILED");
  expect(digest).toContain("process exited 1");
  expect(digest).toContain("Two consecutive failures");
});

test("question digest contains the question text and triage instruction", () => {
  const digest = formatDigest(
    envelope({
      runId: "run-1",
      event: { type: "workerQuestion", payload: { runId: "run-1", taskId: "task-1", workerId: "w1", question: "should I delete the index?" } },
    }),
    ROWS,
  );
  expect(digest).toBeDefined();
  expect(digest).toContain("should I delete the index?");
  expect(digest).toContain("Answer via crew_send");
});

test("bridge injects a digest for a milestone and stays silent for noise", () => {
  const sent: string[] = [];
  const fakePi = {
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    sendMessage: (message: string) => {
      sent.push(message);
    },
  } as unknown as { logger: { [k: string]: (...a: unknown[]) => void }; sendMessage: (m: string) => void };
  const listeners: Array<(e: EventEnvelope) => void> = [];
  const controller = {
    subscribeEvents(cb: (e: EventEnvelope) => void) {
      listeners.push(cb);
      return () => {
        const i = listeners.indexOf(cb);
        if (i >= 0) {
          listeners.splice(i, 1);
        }
      };
    },
    getState() {
      return { rows: ROWS };
    },
  } as unknown as MonitorController;
  const unsubscribe = attachMilestoneBridge(fakePi as never, controller);
  const dispatch = (e: EventEnvelope): void => {
    for (const l of listeners) {
      l(e);
    }
  };
  expect(listeners.length).toBe(1);

  dispatch(run("run-1", "failed"));
  dispatch(message()); // noise: no digest

  expect(sent.length).toBe(1);
  expect(sent[0]).toContain("FAILED");

  unsubscribe();
  dispatch(run("run-2", "succeeded")); // detached: no further injection
  expect(sent.length).toBe(1);
});
