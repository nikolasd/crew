// The embedded monitor's event-reducer: builds one row per run from the
// replayed/live runtime event stream. Pure and total -- the same sequence
// of `reduceEvent` calls always produces the same final state, regardless
// of the order events for *different* runs happen to interleave in (each
// row only reacts to events carrying its own `runId`, and a per-row
// sequence guard makes a stale, out-of-order redelivery a no-op).
//
// Only already-sanitized `RuntimeEvent` fields ever reach a row: there is
// no raw message payload, thinking, or secret-classified content on the
// wire at this layer (see `crates/protocol/src/event.rs`), so hidden
// reasoning and secret-marked fields structurally cannot enter this view
// model.

import type { EventEnvelope, RuntimeEvent } from "@nikolasd/crew-protocol";

/** Independent lifecycle flags, mirrored from the runtime's `RunFlags`. */
export interface MonitorFlags {
  readonly degradedControl: boolean;
  readonly needsReconciliation: boolean;
  readonly protocolUnhealthy: boolean;
  readonly policyQuarantined: boolean;
  readonly workspaceDirty: boolean;
  readonly childrenActive: boolean;
}

/** A run's Crew-owned display pane, as of the latest `displayEvent`. */
export interface MonitorPane {
  readonly backend: string;
  readonly placement: string;
  /** Empty for a `hidden` backend. Never a filesystem path, only a vendor
   *  pane id. */
  readonly paneRef: string;
  readonly attached: boolean;
}

const EMPTY_FLAGS: MonitorFlags = {
  degradedControl: false,
  needsReconciliation: false,
  protocolUnhealthy: false,
  policyQuarantined: false,
  workspaceDirty: false,
  childrenActive: false,
};

/** One monitor row: the replayable view of a single run. */
export interface MonitorRow {
  readonly runId: string;
  readonly taskId: string;
  readonly workerId: string;
  readonly state: string;
  readonly flags: MonitorFlags;
  /** A derived, non-payload activity label (e.g. "question sent") -- never
   *  the raw message text, which this layer never sees. */
  readonly latestActivity?: string;
  readonly pendingApprovalCount: number;
  /** Open (undecided) policy violations on this run, violationId -> code.
   *  An entry here on a quarantined run is the violation holding the
   *  quarantine (R80). */
  readonly openViolations: Readonly<Record<string, string>>;
  /** Set by the controller from `worker/get`; absent until enriched. */
  readonly adapter?: string;
  readonly model?: string;
  /** Set by the controller from `run/get`; absent until enriched. */
  readonly workspaceMode?: string;
  /** The run's Crew-owned display pane, from the latest `displayEvent`;
   *  absent until one arrives. `attached: false` means the most recent
   *  event was a detach -- the row keeps the last-known backend/pane
   *  ref rather than clearing them, so a detail view can still say what
   *  was there. */
  readonly pane?: MonitorPane;
  readonly firstSeenAt: string;
  readonly lastEventAt: string;
  /** The highest event sequence applied to this row; guards against a
   *  stale, out-of-order redelivery regressing the row. */
  readonly lastAppliedSequence: number;
}

/** The monitor's full replayable state. */
export interface MonitorState {
  readonly rows: Readonly<Record<string, MonitorRow>>;
  /** The highest sequence number observed across every row, for resuming
   *  a subscription from the right point after a reconnect. */
  readonly lastSequence: number;
}

/** The initial, empty monitor state. */
export const EMPTY_MONITOR_STATE: MonitorState = { rows: {}, lastSequence: 0 };

/** Whether `state` has at least one run row — the widget's show/hide
 *  decision (the box is only shown when there is something to show). */
export function hasVisibleRows(state: MonitorState): boolean {
  return Object.keys(state.rows).length > 0;
}

/**
 * Applies one durable event envelope to `state`, returning the next state.
 * Never mutates `state`. An event whose `sequence` is not newer than the
 * affected row's `lastAppliedSequence` is a no-op for that row (but still
 * advances `state.lastSequence` for resume purposes).
 */
export function reduceEvent(state: MonitorState, envelope: EventEnvelope): MonitorState {
  const lastSequence = envelope.sequence > state.lastSequence ? envelope.sequence : state.lastSequence;
  const patch = eventPatch(envelope);
  if (patch === undefined) {
    return { rows: state.rows, lastSequence };
  }

  const existing = state.rows[patch.runId];
  if (existing !== undefined && envelope.sequence <= existing.lastAppliedSequence) {
    return { rows: state.rows, lastSequence };
  }

  const base: MonitorRow = existing ?? {
    runId: patch.runId,
    taskId: patch.taskId ?? "",
    workerId: patch.workerId ?? "",
    state: "queued",
    flags: EMPTY_FLAGS,
    pendingApprovalCount: 0,
    openViolations: {},
    firstSeenAt: envelope.timestamp,
    lastEventAt: envelope.timestamp,
    lastAppliedSequence: envelope.sequence,
  };

  const updated: MonitorRow = {
    ...base,
    taskId: patch.taskId ?? base.taskId,
    workerId: patch.workerId ?? base.workerId,
    state: patch.state ?? base.state,
    flags: patch.flags ?? base.flags,
    latestActivity: patch.latestActivity ?? base.latestActivity,
    pendingApprovalCount: patch.pendingApprovalCountDelta !== undefined ? Math.max(0, base.pendingApprovalCount + patch.pendingApprovalCountDelta) : base.pendingApprovalCount,
    openViolations: applyViolationPatch(base.openViolations, patch),
    pane: patch.pane ?? base.pane,
    lastEventAt: envelope.timestamp,
    lastAppliedSequence: envelope.sequence,
  };

  return {
    rows: { ...state.rows, [patch.runId]: updated },
    lastSequence,
  };
}

/** Applies every envelope in `envelopes`, in the order given. */
export function reduceEvents(state: MonitorState, envelopes: readonly EventEnvelope[]): MonitorState {
  let next = state;
  for (const envelope of envelopes) {
    next = reduceEvent(next, envelope);
  }
  return next;
}

/** Sets the `adapter`/`model` fields on a row (from `worker/get`). */
export function enrichWorker(state: MonitorState, runId: string, adapter: string, model: string): MonitorState {
  const row = state.rows[runId];
  if (row === undefined) {
    return state;
  }
  return {
    rows: { ...state.rows, [runId]: { ...row, adapter, model } },
    lastSequence: state.lastSequence,
  };
}

/** Sets the `workspaceMode` field on a row (from `run/get`). */
export function enrichWorkspaceMode(state: MonitorState, runId: string, workspaceMode: string): MonitorState {
  const row = state.rows[runId];
  if (row === undefined) {
    return state;
  }
  return {
    rows: { ...state.rows, [runId]: { ...row, workspaceMode } },
    lastSequence: state.lastSequence,
  };
}

/** The fields one event contributes to its run's row, if any. */
interface EventPatch {
  readonly runId: string;
  readonly taskId?: string;
  readonly workerId?: string;
  readonly state?: string;
  readonly flags?: MonitorFlags;
  readonly latestActivity?: string;
  readonly pendingApprovalCountDelta?: number;
  /** A newly recorded, undecided violation: id and code. */
  readonly addViolation?: { readonly violationId: string; readonly code: string };
  /** A decided violation to remove from the open set. */
  readonly removeViolationId?: string;
  readonly pane?: MonitorPane;
}

function applyViolationPatch(open: Readonly<Record<string, string>>, patch: EventPatch): Readonly<Record<string, string>> {
  if (patch.addViolation !== undefined) {
    return { ...open, [patch.addViolation.violationId]: patch.addViolation.code };
  }
  if (patch.removeViolationId !== undefined && patch.removeViolationId in open) {
    const next = { ...open };
    delete next[patch.removeViolationId];
    return next;
  }
  return open;
}

function eventPatch(envelope: EventEnvelope): EventPatch | undefined {
  const event: RuntimeEvent = envelope.event;
  const runId = envelope.runId ?? undefined;

  switch (event.type) {
    case "runEvent": {
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        workerId: event.payload.workerId,
        state: event.payload.state,
        latestActivity: `run ${event.payload.state}`,
      };
    }
    case "runFlagsEvent": {
      return {
        runId: event.payload.runId,
        flags: {
          degradedControl: event.payload.flags.degradedControl,
          needsReconciliation: event.payload.flags.needsReconciliation,
          protocolUnhealthy: event.payload.flags.protocolUnhealthy,
          policyQuarantined: event.payload.flags.policyQuarantined,
          workspaceDirty: event.payload.flags.workspaceDirty,
          childrenActive: event.payload.flags.childrenActive,
        },
      };
    }
    case "messageEvent": {
      if (runId === null || runId === undefined) {
        return undefined;
      }
      return {
        runId,
        taskId: event.payload.taskId,
        latestActivity: `${event.payload.kind} ${event.payload.deliveryState}`,
      };
    }
    case "approvalEvent": {
      if (runId === null || runId === undefined) {
        return undefined;
      }
      const isRequest = event.payload.kind === "approvalRequested";
      return {
        runId,
        taskId: event.payload.taskId,
        latestActivity: isRequest ? `approval requested: ${event.payload.action}` : `approval decided${event.payload.reason !== undefined && event.payload.reason !== null ? `: ${event.payload.reason}` : ""}`,
        pendingApprovalCountDelta: isRequest ? 1 : -1,
      };
    }
    case "childEvent": {
      if (runId === null || runId === undefined) {
        return undefined;
      }
      const label = event.payload.kind === "childWorkerRequested" ? "child worker requested" : event.payload.kind === "childWorkerAccepted" ? "child worker accepted" : "child worker request denied";
      return { runId, latestActivity: label };
    }
    case "policyViolationRecorded": {
      const kind = event.payload.kind;
      if (typeof kind !== "object" || !("policyViolationRecorded" in kind)) {
        return undefined;
      }
      const recorded = kind.policyViolationRecorded;
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        workerId: event.payload.workerId,
        latestActivity: `policy violation: ${recorded.code}`,
        addViolation: { violationId: recorded.violation_id, code: recorded.code },
      };
    }
    case "policyViolationDecided": {
      const kind = event.payload.kind;
      if (typeof kind !== "object" || !("policyViolationDecided" in kind)) {
        return undefined;
      }
      const decided = kind.policyViolationDecided;
      return {
        runId: event.payload.runId,
        latestActivity: `violation decided: ${decided.resolution}`,
        removeViolationId: decided.violation_id,
      };
    }
    case "adapterUsageEvent": {
      // `inputTokens`/`outputTokens` are plain JSON numbers (R90):
      // never handed to a numeric formatter, which would throw.
      const { inputTokens, outputTokens, costUsd } = event.payload;
      const cost = costUsd === null || costUsd === undefined ? "" : ` ($${costUsd})`;
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        latestActivity: `usage ${inputTokens} in / ${outputTokens} out${cost}`,
      };
    }
    case "adapterProtocolHealthEvent": {
      // R91: R12/R42/R57 invest in a precise detail (the vendor's error
      // subtype, the raw stop reason) -- render it, not a constant label.
      const { healthy, detail } = event.payload;
      const label = healthy ? "protocol healthy" : "protocol unhealthy";
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        workerId: event.payload.workerId,
        latestActivity: detail === null || detail === undefined ? label : `${label}: ${detail}`,
      };
    }
    case "adapterArtifactEvent": {
      return {
        runId: event.payload.runId,
        taskId: event.payload.taskId,
        latestActivity: `artifact ${event.payload.artifactKind} ${event.payload.artifactId}`,
      };
    }
    case "displayEvent": {
      // `kind` is `RuntimeEventKind`'s plain string form here
      // (`"displayPaneAttached"`/`"displayPaneDetached"`) -- unlike
      // `policyViolationRecorded`/`policyViolationDecided`, this event's
      // `kind` never carries the object-shaped violation-detail variants.
      const attached = event.payload.kind === "displayPaneAttached";
      const { backend, placement, paneRef } = event.payload;
      return {
        runId: event.payload.runId,
        latestActivity: attached ? `pane attached: ${backend}${paneRef !== "" ? ` (${paneRef})` : ""}` : `pane detached: ${backend}`,
        pane: { backend, placement, paneRef, attached },
      };
    }
    case "workspaceEvent": {
      // `WorkspaceEvent` is an *adjacently* tagged enum
      // (`#[serde(tag = "type", content = "payload")]`), so the variant
      // name is `kind.type`. Taking the first object key instead would
      // yield the literal string "type".
      const kindLabel = event.payload.kind.type;
      return {
        runId: event.payload.runId,
        latestActivity: `workspace ${kindLabel}`,
      };
    }
    default:
      return undefined;
  }
}
