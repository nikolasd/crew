// The milestone bridge — the single highest-value component
// of the team-leader product layer. The extension already subscribes to
// the runtime journal for the widget; this module injects *milestone
// digests* into the OMP session so the orchestrating model is told when
// something needs its attention instead of having to poll the monitor.
//
// Milestones: terminal run states (succeeded | failed | cancelled |
// lost), workerQuestion, workerTimeout, budgetExceeded, escalationRaised,
// the FIRST transition to `working` per run, and a settled turn (--
// ADR-0027's `waitingUser` + `turnSettled`, the leader's cue that an answer
// is ready without waiting for a terminal state). Everything else (tool
// activity, message chunks, repeated working transitions) is noise and is
// never surfaced.

import type { EventEnvelope, RuntimeEvent } from "@nikolasd/crew-protocol";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import type { Component } from "@oh-my-pi/pi-tui";

import type { EventDeliveryMeta } from "./client";
import type { MonitorController } from "./monitor/controller";
import type { MonitorRow } from "./monitor/model";

/**
 * The `customType` crew's own injected notices are sent under, on any omp
 * build `supportsCustomMessageRenderer` below confirms can take it. A
 * bare string -- every digest here used to be one, and still is on a
 * build that fails that check -- falls back to oh-my-pi's own
 * `DEFAULT_CUSTOM_MESSAGE_TYPE` ("custom-message") and renders under a
 * generic header naming no extension at all, indistinguishable from any
 * other extension's notice. Sending under this type instead, paired with
 * `registerCrewMessageRenderer` below, is what lets a digest render as
 * crew's own rather than that generic fallback.
 */
const CREW_MESSAGE_TYPE = "crew";

/**
 * Icon and label crew's own notices render under. Provisional pending the
 * maintainer's final wording on the label -- "Crew" is the agreed default
 * to ship with.
 */
const CREW_ICON = "👥";
const CREW_LABEL = "Crew";

/**
 * Whether this omp build can take a custom message renderer at all --
 * `registerMessageRenderer` accessed through a cast, the same defensive
 * lookup convention already used here for `sendMessage`. This single
 * check gates two separate things, deliberately: whether it is safe to
 * *register* a renderer (obviously), and whether it is safe to *send* the
 * `{ customType, content }` object shape `sendMessage` needs for a
 * renderer to ever apply. The two are tied together on purpose -- a build
 * old enough to lack this method is exactly the build most likely to
 * predate `customType` support too, and there is no independent way to
 * probe the second without an installed omp old enough to test against.
 * Tying them to the same signal makes the degradation provable by reading
 * this file rather than by knowing which omp versions accept what: a
 * build without this method gets a bare-string digest, unconditionally,
 * from every call site below.
 */
function supportsCustomMessageRenderer(pi: ExtensionAPI): boolean {
  return (
    typeof (
      pi as unknown as {
        registerMessageRenderer?: (customType: string, renderer: (message: { content: unknown }) => Component | undefined) => void;
      }
    ).registerMessageRenderer === "function"
  );
}

/**
 * Registers a custom renderer for `CREW_MESSAGE_TYPE`: a small box with
 * crew's own icon + label as its header and the digest text as its body,
 * standing in for the generic "custom-message" fallback header every
 * extension-injected message without a registered renderer gets. Only
 * ever called after `supportsCustomMessageRenderer` -- see that function
 * for why its check covers this call being safe to make at all.
 *
 * `@oh-my-pi/pi-tui`'s `Box`/`Text` are imported dynamically, inside this
 * function, rather than statically at module scope: this module (and the
 * `--external` build that ships it) has no control over whether an
 * arbitrary omp build actually exposes that module to extensions, and a
 * static import of an external specifier is evaluated at load time,
 * unconditionally -- a `pi-tui` an omp build doesn't expose would fail
 * the whole extension's load, not just this renderer, on every build,
 * whether or not it ever calls this function. Deferred behind the same
 * capability check `sendMessage`'s own wire shape is gated on, a build
 * without `pi-tui` degrades exactly like a build without the method:
 * no custom renderer, the fallback header applies, nothing else breaks.
 */
async function registerCrewMessageRenderer(pi: ExtensionAPI): Promise<void> {
  const register = (
    pi as unknown as {
      registerMessageRenderer: (customType: string, renderer: (message: { content: unknown }) => Component | undefined) => void;
    }
  ).registerMessageRenderer;
  // One try/catch around both steps, deliberately: neither the dynamic
  // import nor the call into host-provided `register` is code this file
  // controls, and a throw from either must degrade the same way -- no
  // custom renderer, no broken extension load, nothing else affected.
  try {
    const { Box, Text } = await import("@oh-my-pi/pi-tui");
    register.call(pi, CREW_MESSAGE_TYPE, (message: { content: unknown }) => {
      // `content` is `CustomMessageContent = string | (TextContent | ImageContent)[]`
      // in general, but every digest this bridge ever sends is a plain
      // string -- the array shape is defended against rather than assumed
      // absent, since a future digest carrying rich content should still
      // render something instead of throwing.
      const text = typeof message.content === "string" ? message.content : JSON.stringify(message.content);
      const box = new Box(1, 0);
      box.addChild(new Text(`${CREW_ICON} ${CREW_LABEL}`));
      box.addChild(new Text(text));
      return box;
    });
  } catch (err) {
    // The method existing without the module it depends on (or rejecting
    // the renderer it was just handed) is not a combination expected to
    // occur in practice -- the method's own return type is `pi-tui`'s
    // `Component` -- but if it does, this is the same cosmetic
    // degradation as the method being absent: no custom renderer, never a
    // broken extension load.
    pi.logger.debug("crew milestone bridge: could not register the crew message renderer", {
      error: err instanceof Error ? err.message : String(err),
    });
  }
}

/** A run-state value that ends the run. */
const TERMINAL_STATES: Record<string, true> = {
  succeeded: true,
  failed: true,
  cancelled: true,
  lost: true,
};

/** The instruction appended to a worker-question digest. */
const QUESTION_TRIAGE = "Answer via crew_send if run context suffices; escalate to the user only for genuinely human decisions.";

// A "two consecutive failures" rule used to be appended to every plain
// `failed` digest below, unconditionally -- on a task's very first failure
// as much as its second. It read as a count no one had actually taken. The
// runtime now raises its own `EscalationRaised { reason: "repeated_failure"
// }` (with a populated `question`) precisely when, and only when, this
// run's task also failed last time (`previous_run_for_task_also_failed`,
// gated on the run genuinely having transitioned to `failed`, not merely
// computed as if it had). That escalation is a milestone of its own and
// gets its own digest below (the `escalationRaised` case), so the
// two-failures guidance now only ever appears when it is actually true --
// no separate constant or count-tracking needed here.

/**
 * How a leader reads a finished run's report.
 *
 * A terminal digest is a *notification*: it says the run ended, not what it
 * produced. The report itself is only reachable through `run/result`, which
 * nothing pushes -- so a digest that names the state and stops leaves the
 * leader knowing a run succeeded and holding none of its output. The
 * settled-turn digest below has always carried this instruction; the
 * terminal ones did not, which is the asymmetry this closes.
 */
const READ_THE_REPORT = 'Read it via crew_run { op: "result", runId }.';

/**
 * The same pointer for a run that did NOT succeed.
 *
 * `run/result` accepts every terminal state, not just `succeeded`
 * (`service::orchestration::run_result` gates on `is_terminal`), and returns
 * whatever visible text the journal accumulated before the run ended
 * (`query::run_result_events_op`'s `final_text.or(chunk_text)`). For a
 * failed, cancelled or lost run that is usually partial output and is often
 * the most useful diagnostic there is -- but it can also be null, when the
 * worker produced nothing before it died. The wording therefore offers it
 * without promising it: a digest that said "read the output" and returned
 * nothing would be worse than one that never mentioned it.
 */
const READ_ANY_PARTIAL_OUTPUT = 'Any partial output it produced is readable via crew_run { op: "result", runId }, though there may be none.';

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
        return `${capitalize(who)} FAILED: ${reason}. ${READ_ANY_PARTIAL_OUTPUT}`;
      }
      if (state === "succeeded") {
        return `${capitalize(who)} succeeded. ${READ_THE_REPORT}`;
      }
      if (state === "cancelled") {
        return `${capitalize(who)} was cancelled. ${READ_ANY_PARTIAL_OUTPUT}`;
      }
      if (state === "lost") {
        return `${capitalize(who)} was lost (worker process died). ${READ_ANY_PARTIAL_OUTPUT}`;
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
      const { reason, question } = event.payload;
      return question ? `Escalation raised on ${who}: ${reason}. ${question}` : `Escalation raised on ${who}: ${reason}.`;
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
 * for extension-originated text delivered to the model. A
 * thrown digest/injection error must never break the monitor: it is logged
 * and swallowed.
 *
 * Digest currency guard: `tracker.isMilestone(e)` is called for
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

  // Computed once, at attach time, and reused below at both the one call
  // site that registers a renderer and the one that decides what shape to
  // send -- see `supportsCustomMessageRenderer`'s own doc comment for why
  // one check governs both.
  const customRendererSupported = supportsCustomMessageRenderer(pi);
  if (customRendererSupported) {
    void registerCrewMessageRenderer(pi);
  }

  // The oh-my-pi `ExtensionAPI` surface is version-gated; access
  // `sendMessage` defensively so a renamed/removed method degrades to
  // "no digest" rather than a crash. The documented signature is
  // `sendMessage(message, { deliverAs, triggerTurn })`. `message` is the
  // `{ customType, content }` object only when `customRendererSupported`
  // -- a bare string otherwise, since a build old enough to lack
  // `registerMessageRenderer` is not one this file has established
  // accepts anything else.
  const send = (
    pi as unknown as {
      sendMessage?: (message: string | { customType: string; content: string }, options?: { deliverAs?: string; triggerTurn?: boolean }) => unknown;
    }
  ).sendMessage;

  // Warned at most once per session, the first time a digest is actually
  // due. Without this the unavailable-API path is the only silent branch in
  // the bridge: `send` is looked up through a cast so a renamed or removed
  // omp method "degrades to no digest", the `typeof` guard below is then
  // false forever, and every milestone for the whole session is dropped
  // with nothing logged -- the surrounding catch only covers digests that
  // throw. An omp version bump could switch the leader's notifications off
  // entirely and look identical to a quiet run.
  //
  // Logged lazily -- at the first digest actually due -- rather than at
  // attach time, and the reason is where the reader will be, not noise:
  // this warning exists to explain an ABSENCE to someone who has noticed a
  // milestone did not arrive, and that person is reading the log around
  // the moment it should have fired. Attach can be hours earlier, when
  // nobody is troubleshooting anything, so a warning there is filed before
  // the question exists. Lazily also means a session that never reaches a
  // milestone stays quiet, and once-per-session means a long run cannot
  // flood the log with one unchanging fact.
  let warnedMissingSendMessage = false;

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
        const message = customRendererSupported ? { customType: CREW_MESSAGE_TYPE, content: digest } : digest;
        void send.call(pi, message, { deliverAs: "followUp", triggerTurn: true });
      } else if (!warnedMissingSendMessage) {
        warnedMissingSendMessage = true;
        pi.logger.warn("crew milestone bridge: this omp build exposes no sendMessage on ExtensionAPI, so run milestones will not be delivered to the leader for the rest of this session");
      }
    } catch (err) {
      pi.logger.error("crew milestone bridge: digest injection failed", {
        error: err instanceof Error ? err.message : String(err),
      });
    }
  });
}
