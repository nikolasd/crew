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

const ALL_FALSE_FLAGS = {
  degradedControl: false,
  needsReconciliation: false,
  protocolUnhealthy: false,
  policyQuarantined: false,
  workspaceDirty: false,
  turnSettled: false,
  childrenActive: false,
};

/** A `runFlagsEvent` for `id` carrying every flag false except `turnSettled`. */
function flags(id: string, turnSettled: boolean): EventEnvelope {
  return envelope({
    runId: id,
    event: {
      type: "runFlagsEvent",
      payload: { runId: id, flags: { ...ALL_FALSE_FLAGS, turnSettled } },
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
    flags: ALL_FALSE_FLAGS,
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

test("CREW-60: a paneDowngraded event is always a milestone", () => {
  const t = tracker();
  expect(
    t.isMilestone(
      envelope({
        runId: "run-1",
        event: { type: "paneDowngraded", payload: { runId: "run-1", requestedBackend: "tmux", requestedPlacement: "splitDown", actualBackend: "hidden", reason: "tmux exploded" } },
      }),
    ),
  ).toBe(true);
});

test("CREW-60: paneDowngraded digest names the requested/actual backends and the reason", () => {
  const digest = formatDigest(
    envelope({
      runId: "run-1",
      event: { type: "paneDowngraded", payload: { runId: "run-1", requestedBackend: "tmux", requestedPlacement: "splitDown", actualBackend: "hidden", reason: "tmux exploded" } },
    }),
    ROWS,
  );
  expect(digest).toBeDefined();
  expect(digest).toContain("tmux");
  expect(digest).toContain("hidden");
  expect(digest).toContain("tmux exploded");
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

test("a settled turn (runFlagsEvent turnSettled:true) is a milestone, once per settle episode", () => {
  const t = tracker();
  // First settle: milestone.
  expect(t.isMilestone(flags("run-1", true))).toBe(true);
  // A repeat while still settled (e.g. an unrelated flag also changed):
  // not a milestone again -- the leader was already told.
  expect(t.isMilestone(flags("run-1", true))).toBe(false);
  // Un-settling (run/finish clears the flag, or a follow-up resumes the
  // run) re-arms it for the run's next settle.
  expect(t.isMilestone(flags("run-1", false))).toBe(false);
  expect(t.isMilestone(flags("run-1", true))).toBe(true);
  // Independent per run.
  expect(t.isMilestone(flags("run-2", true))).toBe(true);
});

test("a runFlagsEvent with turnSettled false is never a milestone on its own", () => {
  const t = tracker();
  expect(t.isMilestone(flags("run-1", false))).toBe(false);
});

test("settled-turn digest points the leader at crew_run result and finish", () => {
  const digest = formatDigest(flags("run-1", true), ROWS);
  expect(digest).toBeDefined();
  expect(digest).toContain("settled a turn");
  expect(digest).toContain('crew_run { op: "result"');
  expect(digest).toContain('crew_run { op: "finish"');
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

function fakeBridge(): {
  sent: string[];
  dispatch: (e: EventEnvelope, meta?: { replay: boolean }) => void;
  unsubscribe: () => void;
} {
  const sent: string[] = [];
  const fakePi = {
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    sendMessage: (message: string) => {
      sent.push(message);
    },
  } as unknown as { logger: { [k: string]: (...a: unknown[]) => void }; sendMessage: (m: string) => void };
  const listeners: Array<(e: EventEnvelope, meta: { replay: boolean }) => void> = [];
  const controller = {
    subscribeEvents(cb: (e: EventEnvelope, meta: { replay: boolean }) => void) {
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
  const dispatch = (e: EventEnvelope, meta: { replay: boolean } = { replay: false }): void => {
    for (const l of listeners) {
      l(e, meta);
    }
  };
  expect(listeners.length).toBe(1);
  return { sent, dispatch, unsubscribe };
}

test("bridge injects a digest for a milestone and stays silent for noise", () => {
  const { sent, dispatch, unsubscribe } = fakeBridge();

  dispatch(run("run-1", "failed"));
  dispatch(message()); // noise: no digest

  expect(sent.length).toBe(1);
  expect(sent[0]).toContain("FAILED");

  unsubscribe();
  dispatch(run("run-2", "succeeded")); // detached: no further injection
  expect(sent.length).toBe(1);
});

test("CREW-51: a replayed milestone never injects a digest (stale-failure guard)", () => {
  const { sent, dispatch } = fakeBridge();

  // A run that failed long ago, delivered as replay catch-up: no digest --
  // it would tell the leader about a stale failure as if it just happened.
  dispatch(run("run-1", "failed"), { replay: true });
  expect(sent.length).toBe(0);

  // The same shape of event, but genuinely live: digest fires.
  dispatch(run("run-2", "failed"), { replay: false });
  expect(sent.length).toBe(1);
  expect(sent[0]).toContain("FAILED");
});

test("CREW-60/CREW-51: a replayed paneDowngraded never injects a digest either", () => {
  // The generic replay-guard test above only exercises `runEvent`. This
  // pins it for the newest milestone type specifically: CREW-51's guard
  // in the bridge is generic over every milestone (it gates on
  // `meta.replay` after `isMilestone`, not on the event's own type), but
  // that genericity is exactly what a future "simplify the guard" change
  // could break for one type without the others' own tests catching it.
  const { sent, dispatch } = fakeBridge();

  const downgraded = (): EventEnvelope =>
    envelope({
      runId: "run-1",
      event: {
        type: "paneDowngraded",
        payload: { runId: "run-1", requestedBackend: "tmux", requestedPlacement: "splitDown", actualBackend: "hidden", reason: "tmux exploded" },
      },
    });

  dispatch(downgraded(), { replay: true });
  expect(sent.length).toBe(0);

  dispatch(downgraded(), { replay: false });
  expect(sent.length).toBe(1);
  expect(sent[0]).toContain("tmux");
});

test("CREW-51: a replayed milestone still updates the tracker's one-shot bookkeeping", () => {
  const { sent, dispatch } = fakeBridge();

  // Replayed: run-1's first `working` is real history, so it must count
  // against the one-shot "first working per run" bookkeeping even though it
  // produces no digest -- otherwise a later, genuinely live `working` for
  // the same run would be wrongly treated as "first" and digested again.
  dispatch(run("run-1", "working"), { replay: true });
  expect(sent.length).toBe(0);

  dispatch(run("run-1", "working"), { replay: false });
  expect(sent.length).toBe(0);

  // A different run's first `working`, live, is still a real milestone.
  dispatch(run("run-2", "working"), { replay: false });
  expect(sent.length).toBe(1);
});
