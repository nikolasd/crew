import { expect, test } from "bun:test";

import type { EventEnvelope } from "@nikolasd/crew-protocol";

import { EMPTY_MONITOR_STATE, hasVisibleRows, reduceEvent, reduceEvents } from "./model";

let sequenceCounter = 0;
function envelope(overrides: Partial<EventEnvelope> & { event: EventEnvelope["event"] }): EventEnvelope {
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
    ...overrides,
  };
}

function runEvent(runId: string, taskId: string, workerId: string, state: string, sequence?: number): EventEnvelope {
  return envelope({
    runId,
    taskId,
    workerId,
    sequence,
    event: {
      type: "runEvent",
      payload: { kind: "runWorking", runId, taskId, workerId, state },
    },
  });
}

test("starts from empty state", () => {
  expect(EMPTY_MONITOR_STATE.rows).toEqual({});
  expect(EMPTY_MONITOR_STATE.lastSequence).toBe(0);
});
test("hasVisibleRows is false for empty state and true once a row exists", () => {
  expect(hasVisibleRows(EMPTY_MONITOR_STATE)).toBe(false);
  const withRow = reduceEvent(EMPTY_MONITOR_STATE, runEvent("run-1", "task-1", "worker-1", "working"));
  expect(hasVisibleRows(withRow)).toBe(true);
});

test("a runEvent creates a row with task/worker/state", () => {
  const next = reduceEvent(EMPTY_MONITOR_STATE, runEvent("run-1", "task-1", "worker-1", "working"));
  const row = next.rows["run-1"];
  expect(row).toBeDefined();
  expect(row?.taskId).toBe("task-1");
  expect(row?.workerId).toBe("worker-1");
  expect(row?.state).toBe("working");
});

// ---- Evidence events: usage, artifacts, workspace activity. ----
//
// All three carry `runId` in their own payload (unlike message/approval/
// child events, which fall back to `envelope.runId`), and none of them
// touches the pending-approval count.

test("an adapterUsageEvent reports token counts, and they interpolate as plain numbers", () => {
  const state = reduceEvent(
    EMPTY_MONITOR_STATE,
    envelope({
      event: {
        type: "adapterUsageEvent",
        payload: {
          runId: "run-1",
          taskId: "task-1",
          workerId: "worker-1",
          inputTokens: 1234,
          outputTokens: 56,
          costUsd: null,
        },
      },
    }),
  );
  const row = state.rows["run-1"];
  expect(row?.latestActivity).toBe("usage 1234 in / 56 out");
  expect(row?.taskId).toBe("task-1");
  expect(row?.pendingApprovalCount).toBe(0);
});

test("an adapterUsageEvent appends cost only when the vendor reported one", () => {
  const state = reduceEvent(
    EMPTY_MONITOR_STATE,
    envelope({
      event: {
        type: "adapterUsageEvent",
        payload: {
          runId: "run-1",
          taskId: "task-1",
          workerId: "worker-1",
          inputTokens: 10,
          outputTokens: 2,
          costUsd: 0.0042,
        },
      },
    }),
  );
  expect(state.rows["run-1"]?.latestActivity).toBe("usage 10 in / 2 out ($0.0042)");
});

test("an adapterArtifactEvent reports the artifact's kind and id", () => {
  const state = reduceEvent(
    EMPTY_MONITOR_STATE,
    envelope({
      event: {
        type: "adapterArtifactEvent",
        payload: {
          runId: "run-1",
          taskId: "task-1",
          workerId: "worker-1",
          artifactId: "018f0000-0000-7000-8000-0000000000ab",
          artifactKind: "fileChange",
        },
      },
    }),
  );
  expect(state.rows["run-1"]?.latestActivity).toBe("artifact fileChange 018f0000-0000-7000-8000-0000000000ab");
});

test("a displayPaneAttached event sets the pane field and reports the backend and pane ref", () => {
  const state = reduceEvent(
    EMPTY_MONITOR_STATE,
    envelope({
      event: {
        type: "displayEvent",
        payload: {
          kind: "displayPaneAttached",
          runId: "run-1",
          backend: "tmux",
          placement: "splitRight",
          paneRef: "%7",
        },
      },
    }),
  );
  const row = state.rows["run-1"];
  expect(row?.pane).toEqual({ backend: "tmux", placement: "splitRight", paneRef: "%7", attached: true });
  expect(row?.latestActivity).toBe("pane attached: tmux (%7)");
});

test("a displayPaneAttached event with an empty pane ref (hidden fallback) omits the parenthetical", () => {
  const state = reduceEvent(
    EMPTY_MONITOR_STATE,
    envelope({
      event: {
        type: "displayEvent",
        payload: {
          kind: "displayPaneAttached",
          runId: "run-1",
          backend: "hidden",
          placement: "embedded",
          paneRef: "",
        },
      },
    }),
  );
  const row = state.rows["run-1"];
  expect(row?.pane).toEqual({ backend: "hidden", placement: "embedded", paneRef: "", attached: true });
  expect(row?.latestActivity).toBe("pane attached: hidden");
});

test("a displayPaneDetached event marks the pane detached but keeps its last-known backend and ref", () => {
  const attached = reduceEvent(
    EMPTY_MONITOR_STATE,
    envelope({
      event: {
        type: "displayEvent",
        payload: {
          kind: "displayPaneAttached",
          runId: "run-1",
          backend: "herdr",
          placement: "tab",
          paneRef: "w1:p2",
        },
      },
    }),
  );
  const detached = reduceEvent(
    attached,
    envelope({
      event: {
        type: "displayEvent",
        payload: {
          kind: "displayPaneDetached",
          runId: "run-1",
          backend: "herdr",
          placement: "tab",
          paneRef: "w1:p2",
        },
      },
    }),
  );
  const row = detached.rows["run-1"];
  expect(row?.pane).toEqual({ backend: "herdr", placement: "tab", paneRef: "w1:p2", attached: false });
  expect(row?.latestActivity).toBe("pane detached: herdr");
});

test("a workspaceEvent renders its adjacently-tagged variant name", () => {
  const state = reduceEvent(
    EMPTY_MONITOR_STATE,
    envelope({
      event: {
        type: "workspaceEvent",
        payload: {
          // The real wire shape: `#[serde(tag = "type", content =
          // "payload")]`. Reading the first object key instead of `.type`
          // would render "workspace type".
          kind: {
            type: "leaseAcquired",
            payload: {
              leaseId: "lease-1",
              runId: "run-1",
              path: "/tmp/wt",
              isolationKind: "gitWorktree",
              baseRevision: "abc123",
            },
          },
          runId: "run-1",
          leaseId: "lease-1",
        },
      },
    }),
  );
  expect(state.rows["run-1"]?.latestActivity).toBe("workspace leaseAcquired");
});

test("replaying out-of-order cross-run events with ordered per-run sequences produces stable rows regardless of interleaving order", () => {
  // Run A: queued(1) -> starting(2) -> working(3). Run B: queued(4) -> working(5).
  const a1 = runEvent("run-a", "task-a", "worker-a", "queued", 1);
  const a2 = runEvent("run-a", "task-a", "worker-a", "starting", 2);
  const a3 = runEvent("run-a", "task-a", "worker-a", "working", 3);
  const b1 = runEvent("run-b", "task-b", "worker-b", "queued", 4);
  const b2 = runEvent("run-b", "task-b", "worker-b", "working", 5);

  // Feed order 1: strictly ascending global sequence.
  const orderedResult = reduceEvents(EMPTY_MONITOR_STATE, [a1, a2, a3, b1, b2]);

  // Feed order 2: cross-run interleaving that is NOT globally ascending
  // (b1 arrives before a1, b2 arrives between a2 and a3), but each run's
  // OWN events still appear in their correct per-run order.
  const interleavedResult = reduceEvents(EMPTY_MONITOR_STATE, [b1, a1, a2, b2, a3]);

  expect(interleavedResult.rows["run-a"]?.state).toBe(orderedResult.rows["run-a"]?.state);
  expect(interleavedResult.rows["run-b"]?.state).toBe(orderedResult.rows["run-b"]?.state);
  expect(interleavedResult.rows["run-a"]?.state).toBe("working");
  expect(interleavedResult.rows["run-b"]?.state).toBe("working");
});

test("a stale, out-of-order event for a run is a no-op and does not regress its row", () => {
  const working = runEvent("run-1", "task-1", "worker-1", "working", 5);
  const staleQueued = runEvent("run-1", "task-1", "worker-1", "queued", 2);

  const state = reduceEvents(EMPTY_MONITOR_STATE, [working, staleQueued]);

  expect(state.rows["run-1"]?.state).toBe("working");
});

test("lastSequence advances even for events that produce no row patch", () => {
  const diagnostic = envelope({
    sequence: 7,
    event: { type: "diagnostic", payload: { level: "info", code: "x", message: "hello" } },
  });
  const state = reduceEvent(EMPTY_MONITOR_STATE, diagnostic);
  expect(state.lastSequence).toBe(7);
  expect(state.rows).toEqual({});
});

// -------------------------------------------------- lifecycle fixtures

test("renders a working fixture", () => {
  const state = reduceEvent(EMPTY_MONITOR_STATE, runEvent("run-1", "task-1", "worker-1", "working"));
  expect(state.rows["run-1"]?.state).toBe("working");
});

test("renders a waitingUser fixture", () => {
  const state = reduceEvent(EMPTY_MONITOR_STATE, runEvent("run-1", "task-1", "worker-1", "waitingUser"));
  expect(state.rows["run-1"]?.state).toBe("waitingUser");
});

test("renders a failed fixture", () => {
  const state = reduceEvent(EMPTY_MONITOR_STATE, runEvent("run-1", "task-1", "worker-1", "failed"));
  expect(state.rows["run-1"]?.state).toBe("failed");
});

test("renders a degraded fixture via runFlagsEvent", () => {
  const flagsEvent = envelope({
    runId: "run-1",
    event: {
      type: "runFlagsEvent",
      payload: {
        runId: "run-1",
        flags: {
          degradedControl: true,
          needsReconciliation: false,
          protocolUnhealthy: false,
          policyQuarantined: false,
          workspaceDirty: false,
          childrenActive: false,
        },
      },
    },
  });
  const state = reduceEvents(EMPTY_MONITOR_STATE, [runEvent("run-1", "task-1", "worker-1", "working"), flagsEvent]);
  expect(state.rows["run-1"]?.flags.degradedControl).toBe(true);
});

test("renders a lost fixture", () => {
  const state = reduceEvent(EMPTY_MONITOR_STATE, runEvent("run-1", "task-1", "worker-1", "lost"));
  expect(state.rows["run-1"]?.state).toBe("lost");
});

// --------------------------------------------- approval count tracking

test("an approvalRequested event increments pendingApprovalCount and a decided one decrements it", () => {
  const requested = envelope({
    runId: "run-1",
    event: {
      type: "approvalEvent",
      payload: { kind: "approvalRequested", approvalId: "approval-1", runId: "run-1", taskId: "task-1", action: "write file", decidedBy: null },
    },
  });
  const decided = envelope({
    runId: "run-1",
    event: {
      type: "approvalEvent",
      payload: { kind: "approvalDecided", approvalId: "approval-1", runId: "run-1", taskId: "task-1", action: "write file", decidedBy: "human" },
    },
  });

  const afterRequest = reduceEvents(EMPTY_MONITOR_STATE, [runEvent("run-1", "task-1", "worker-1", "waitingUser"), requested]);
  expect(afterRequest.rows["run-1"]?.pendingApprovalCount).toBe(1);

  const afterDecision = reduceEvent(afterRequest, decided);
  expect(afterDecision.rows["run-1"]?.pendingApprovalCount).toBe(0);
});

// ----------------------------------------- secret/thinking content never enters the model

test("a protocol-health event renders its detail, not a constant label (R91)", () => {
  const state = reduceEvent(
    EMPTY_MONITOR_STATE,
    envelope({
      runId: "run-1",
      event: {
        type: "adapterProtocolHealthEvent",
        payload: {
          runId: "run-1",
          taskId: "task-1",
          workerId: "worker-1",
          healthy: false,
          detail: "error result: rate_limited",
        },
      },
    } as Partial<EventEnvelope> & { event: EventEnvelope["event"] }),
  );
  expect(state.rows["run-1"]?.latestActivity).toBe("protocol unhealthy: error result: rate_limited");

  const healthyAgain = reduceEvent(
    state,
    envelope({
      runId: "run-1",
      event: {
        type: "adapterProtocolHealthEvent",
        payload: { runId: "run-1", taskId: "task-1", workerId: "worker-1", healthy: true, detail: null },
      },
    } as Partial<EventEnvelope> & { event: EventEnvelope["event"] }),
  );
  expect(healthyAgain.rows["run-1"]?.latestActivity).toBe("protocol healthy");
});

test("only the sanitized fields the RuntimeEvent union carries ever reach a row -- no raw message payload, thinking, or secret content", () => {
  const messageEvent = envelope({
    runId: "run-1",
    event: {
      type: "messageEvent",
      payload: { kind: "messageSent", messageId: "message-1", runId: "run-1", taskId: "task-1", deliveryState: "sent" },
    },
  });
  const state = reduceEvents(EMPTY_MONITOR_STATE, [runEvent("run-1", "task-1", "worker-1", "working"), messageEvent]);
  const row = state.rows["run-1"];
  expect(row).toBeDefined();
  // The row's only content-shaped field is a derived, kind-based label --
  // never the message's raw payload text (which this reducer never sees:
  // RuntimeEvent::MessageEvent carries no payload field on the wire).
  expect(row?.latestActivity).toBe("messageSent sent");
  expect(JSON.stringify(row, (_key, value) => (typeof value === "bigint" ? value.toString() : value))).not.toContain("payload");
  expect(Object.keys(row ?? {}).sort()).toEqual(["runId", "taskId", "workerId", "state", "flags", "latestActivity", "pendingApprovalCount", "openViolations", "pane", "firstSeenAt", "lastEventAt", "lastAppliedSequence"].sort());
});

// ------------------------------------------------- open violation tracking

test("a policyViolationRecorded event appears in openViolations and a decided one removes it (R80)", () => {
  const recorded = envelope({
    runId: "run-1",
    event: {
      type: "policyViolationRecorded",
      payload: {
        kind: {
          policyViolationRecorded: {
            violation_id: "violation-1",
            code: "nested_worker_denied",
            observed_event_sequence: 5,
            policy_fingerprint: "sha256:abc",
            vendor_child_id: "child-1",
            vendor_parent_ref: "parent-1",
            action: "quarantine",
          },
        },
        runId: "run-1",
        taskId: "task-1",
        workerId: "worker-1",
      },
    },
  });
  const afterRecorded = reduceEvents(EMPTY_MONITOR_STATE, [runEvent("run-1", "task-1", "worker-1", "working"), recorded]);
  expect(afterRecorded.rows["run-1"]?.openViolations).toEqual({ "violation-1": "nested_worker_denied" });
  expect(afterRecorded.rows["run-1"]?.latestActivity).toBe("policy violation: nested_worker_denied");

  const decided = envelope({
    runId: "run-1",
    event: {
      type: "policyViolationDecided",
      payload: {
        kind: {
          policyViolationDecided: {
            violation_id: "violation-1",
            resolution: "release",
            resolved_by: "omp-1",
          },
        },
        runId: "run-1",
        taskId: "task-1",
        workerId: "worker-1",
      },
    },
  });
  const afterDecided = reduceEvent(afterRecorded, decided);
  expect(afterDecided.rows["run-1"]?.openViolations).toEqual({});
  expect(afterDecided.rows["run-1"]?.latestActivity).toBe("violation decided: release");
});
