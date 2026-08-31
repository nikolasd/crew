// The milestone bridge (spec §7.2) — the single highest-value component
// of the team-leader product layer. The extension already subscribes to
// the runtime journal for the widget; this module injects *milestone
// digests* into the OMP session so the orchestrating model is told when
// something needs its attention instead of having to poll the monitor.
//
// Milestones: terminal run states (succeeded | failed | cancelled |
// lost), workerQuestion, workerTimeout, budgetExceeded, escalationRaised,
// the FIRST transition to `working` per run, and a settled turn (CREW-35 --
// ADR-0027's `waitingUser` + `turnSettled`, the leader's cue that an answer
// is ready without waiting for a terminal state). Everything else (tool
// activity, message chunks, repeated working transitions) is noise and is
// never surfaced.

import type { EventEnvelope, RuntimeEvent } from "@nikolasd/crew-protocol";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { EventDeliveryMeta } from "./client";
import type { MonitorController } from "./monitor/controller";
import type { MonitorRow } from "./monitor/model";

/** A run-state value that ends the run. */
const TERMINAL_STATES: Record<string, true> = {
  succeeded: true,
  failed: true,
  cancelled: true,
  lost: true,
};

/** The instruction appended to a worker-question digest (spec §7.4). */
const QUESTION_TRIAGE = "Answer via crew_send if run context suffices; escalate to the user only for genuinely human decisions.";

/** The rule text appended to a failed-run digest (spec §7.5). */
const TWO_FAILURES_RULE = "Two consecutive failures on the same task require escalation to the user.";

/**
 * The monitor's run rows, keyed by run id. Lets the digest name the run's
 * adapter / task instead of emitting bare ids.
 */
export type RunLookup = Readonly<Record<string, MonitorRow>>;

function capitalize(s: string): string {
  return s.length === 0 ? s : `${s[0].toUpperCase()}${s.slice(1)}`;
}

function lookupKey(e: EventEnvelope): string | undefined {
  // The runtime sets `runId` on the envelope for every run-scoped event,
  // so it is the authoritative key (no union payload narrowing needed).
  return e.runId ?? undefined;
}

/**
 * Tracks which runs have already emitted their once-only `working`
 * milestone, and decides whether a given envelope is a milestone worth
 * digesting.
 */
export class MilestoneTracker {
  readonly #sawWorking = new Set<string>();
  readonly #sawSettled = new Set<string>();

  isMilestone(e: EventEnvelope): boolean {
    const event: RuntimeEvent = e.event;
    switch (event.type) {
      case "runEvent": {
        const state = event.payload.state;
        if (state in TERMINAL_STATES) {
          return true;
        }
        if (state === "working") {
          const runId = event.payload.runId;
          if (this.#sawWorking.has(runId)) {
            return false;
          }
          this.#sawWorking.add(runId);
          return true;
        }
        return false;
      }
      case "runFlagsEvent": {
        // `turnSettled` is the ADR-0027 signal a `runEvent` alone can't
        // give: `waitingUser` is reached both by a settled turn and by a
        // worker question (its own, separately-milestoned event type), and
        // only this flag tells them apart. One-shot per settle episode --
        // `run/finish` (or a follow-up resuming the run) clears the flag,
        // which re-arms this for the run's next settle.
        const runId = event.payload.runId;
        if (event.payload.flags.turnSettled) {
          if (this.#sawSettled.has(runId)) {
            return false;
          }
          this.#sawSettled.add(runId);
          return true;
        }
        this.#sawSettled.delete(runId);
        return false;
      }
      case "workerQuestion":
      case "workerTimeout":
      case "budgetExceeded":
      case "escalationRaised":
      case "paneDowngraded":
        return true;
      default:
        return false;
    }
  }
}

/**
 * Builds the compact prose digest for a milestone envelope. `lookup` names
 * the run's adapter / task from the monitor's rows. Returns undefined when
 * the envelope is not a milestone (callers should only call this after
 * `isMilestone`).
 */
export function formatDigest(e: EventEnvelope, lookup: RunLookup): string | undefined {
  const event: RuntimeEvent = e.event;
  const runId = lookupKey(e);
  const row = runId !== undefined ? lookup[runId] : undefined;
  const who = row !== undefined ? `run ${runId} (${row.adapter || "unknown"} adapter) for task ${row.taskId || "unknown"}` : `run ${runId ?? "unknown"}`;

  switch (event.type) {
    case "runEvent": {
      const state = event.payload.state;
      if (state === "failed") {
        const reason = row?.latestActivity ?? "see runtime";
        return `${capitalize(who)} FAILED: ${reason}. ${TWO_FAILURES_RULE}`;
      }
      if (state === "succeeded") {
        return `${capitalize(who)} succeeded.`;
      }
      if (state === "cancelled") {
        return `${capitalize(who)} was cancelled.`;
      }
      if (state === "lost") {
        return `${capitalize(who)} was lost (worker process died).`;
      }
      if (state === "working") {
        return `${capitalize(who)} started working.`;
      }
      return undefined;
    }
    case "runFlagsEvent": {
      if (!event.payload.flags.turnSettled) {
        return undefined;
      }
      return `${capitalize(who)} settled a turn and is waiting on the leader (not terminal -- ` + `the vendor is still parked, not exited). Read the answer via crew_run { op: "result", runId }, ` + `then either crew_send to follow up or crew_run { op: "finish", runId, outcome } to close it.`;
    }
    case "workerQuestion": {
      const question = event.payload.question ?? "(no question text captured)";
      return `Worker question on ${who}: ${question}. ${QUESTION_TRIAGE}`;
    }
    case "workerTimeout": {
      const kind = event.payload.kind ?? "inactivity";
      return `${capitalize(who)} hit a worker ${kind} timeout. The runtime reports; the leader decides: ` + `give it more time via crew_run { op: "timeoutAck", runId, decision: "extend" }, ` + `redirect it via crew_send (the nudge), or stop it via crew_run { op: "timeoutAck", decision: "abort" }.`;
    }
    case "budgetExceeded":
      return `${capitalize(who)} exceeded its turn budget. Escalate to the user or raise the budget via the plan.`;
    case "escalationRaised": {
      const reason = event.payload.reason;
      return `Escalation raised on ${who}: ${reason}.`;
    }
    case "paneDowngraded": {
      const { requestedBackend, requestedPlacement, actualBackend, reason } = event.payload;
      return `${capitalize(who)}'s pane fell back from ${requestedBackend} (${requestedPlacement}) to ${actualBackend}: ${reason}.`;
    }
    default:
      return undefined;
  }
}

/**
 * Wires the milestone bridge onto the monitor's single live subscription
 * (no second subscription is opened). Every envelope the monitor reduces is
 * also offered to `tracker`; on a milestone it formats a digest and injects
 * it into the OMP session via `pi.sendMessage(..., { deliverAs:
 * "followUp", triggerTurn: true })`, which is the documented oh-my-pi API
 * for extension-originated text delivered to the model (spec §7.2). A
 * thrown digest/injection error must never break the monitor: it is logged
 * and swallowed.
 *
 * CREW-51 (digest currency guard): `tracker.isMilestone(e)` is called for
 * *every* envelope regardless of `meta.replay`, so its one-shot bookkeeping
 * (first `working`, settle episodes) stays correct against the run's full
 * history -- but a digest is only ever formatted and sent for a *live*
 * milestone (`meta.replay === false`). Without this, resuming a session (or
 * any reconnect that replays backlog) would re-tell the leader about
 * whatever terminal/question/escalation states are sitting in the replayed
 * history as if they had just happened -- a run that failed hours ago reads
 * as a fresh failure. `events/replay`'s catch-up array is exactly that
 * backlog (see `EventDeliveryMeta`); only what arrives afterward, live, is
 * current enough to act on.
 *
 * Returns an unsubscribe function that detaches the bridge.
 */
export function attachMilestoneBridge(pi: ExtensionAPI, monitor: MonitorController): () => void {
  const tracker = new MilestoneTracker();

  // The oh-my-pi `ExtensionAPI` surface is version-gated; access
  // `sendMessage` defensively so a renamed/removed method degrades to
  // "no digest" rather than a crash. The documented signature is
  // `sendMessage(message, { deliverAs, triggerTurn })`.
  const send = (
    pi as unknown as {
      sendMessage?: (message: string, options?: { deliverAs?: string; triggerTurn?: boolean }) => unknown;
    }
  ).sendMessage;

  return monitor.subscribeEvents((e: EventEnvelope, meta: EventDeliveryMeta) => {
    const milestone = tracker.isMilestone(e);
    if (!milestone || meta.replay) {
      return;
    }
    try {
      const rows = monitor.getState().rows;
      const digest = formatDigest(e, rows);
      if (digest === undefined) {
        return;
      }
      if (typeof send === "function") {
        void send.call(pi, digest, { deliverAs: "followUp", triggerTurn: true });
      }
    } catch (err) {
      pi.logger.error("crew milestone bridge: digest injection failed", {
        error: err instanceof Error ? err.message : String(err),
      });
    }
  });
}
