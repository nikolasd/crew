// Wires the monitor's pure model/render layers into the live extension:
// replay-first startup (resuming from the last persisted sequence),
// continuous widget updates as events arrive, and the `/crew` /
// `/crew run <runId>` commands.

import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { EventEnvelope } from "@nikolasd/crew-protocol";

import type { ExtensionAPI, ExtensionCommandContext, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import type { CrewClient } from "../client";
import { attachMilestoneBridge } from "../milestones";
import { assertCompatiblePiCodingAgentVersion } from "./compat";
import { EMPTY_MONITOR_STATE, enrichWorker, enrichWorkspaceMode, hasVisibleRows, type MonitorState, reduceEvent } from "./model";
import { renderRowDetails, renderWidgetBox } from "./render";

/** The custom session-entry type the last-rendered sequence is persisted under. */
export const MONITOR_ENTRY_TYPE = "crew-monitor";

/** The widget key the monitor renders under. */
const WIDGET_KEY = "crew-monitor";

/** The slash command that opens or refreshes the monitor. */
export const MONITOR_COMMAND_NAME = "crew";

/** One management operation surfaced as a /crew subcommand. The handler
 *  lives in index.ts (it needs the cached-client and doctor-context
 *  closures); the monitor owns only the dispatch. */
export interface ManagementSubcommand {
  readonly description: string;
  /** Ghost-text hint for the completion dropdown, e.g. "[path | print ...]". */
  readonly hint?: string;
  run(args: string, ctx: ExtensionContext): Promise<{ text: string; isError: boolean }>;
}

export interface MonitorControllerContext {
  getClient(extCtx: ExtensionContext): Promise<CrewClient>;
  management?: ReadonlyMap<string, ManagementSubcommand>;
}

/** The subset of `pi.appendEntry`'s session-entry log the controller reads
 *  back on startup to resume from the last rendered sequence. */
export interface SessionEntryLike {
  readonly type: string;
  readonly customType?: string;
  readonly data?: unknown;
}

/**
 * Scans `entries` (oldest to newest, as `getEntries()` returns them) for
 * the most recent `crew-monitor` custom entry and returns its persisted
 * sequence, or `0` if none exists yet.
 */
export function lastPersistedSequence(entries: readonly SessionEntryLike[]): number {
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i];
    if (entry?.type === "custom" && entry.customType === MONITOR_ENTRY_TYPE) {
      const data = entry.data as { sequence?: unknown } | undefined;
      if (typeof data?.sequence === "number") {
        return data.sequence;
      }
    }
  }
  return 0;
}

/**
 * Owns the monitor's replayable state and keeps the embedded widget in
 * sync as events arrive. One instance per OMP session.
 */
export class MonitorController {
  #state: MonitorState = EMPTY_MONITOR_STATE;
  #unsubscribe: (() => void) | undefined;
  #onUpdate: (() => void) | undefined;
  /** Extra per-event listeners (e.g. the milestone bridge), fed by the
   *  single live subscription — never a second one. */
  #eventListeners = new Set<(event: EventEnvelope) => void>();

  /** The current replayable state (read-only view for tests/commands). */
  getState(): MonitorState {
    return this.#state;
  }

  /**
   * Registers an extra per-event listener fed by the monitor's single live
   * subscription. Returns an unsubscribe function. The milestone bridge uses
   * this so the model is told about milestones without a second subscription
   * being opened.
   */
  subscribeEvents(listener: (event: EventEnvelope) => void): () => void {
    this.#eventListeners.add(listener);
    return () => {
      this.#eventListeners.delete(listener);
    };
  }

  /**
   * Subscribes from `fromSequence`, rebuilding state from replay before
   * live notifications arrive (both flow through the same reducer, so
   * there is no separate "replay mode"). Calls `onUpdate` after every
   * applied event so the caller can re-render the widget and persist the
   * new sequence, then fans the event out to any extra listeners (the
   * milestone bridge).
   */
  start(client: CrewClient, fromSequence: number, onUpdate: () => void): void {
    this.#onUpdate = onUpdate;
    this.#unsubscribe = client.subscribe(fromSequence, (event) => {
      this.#state = reduceEvent(this.#state, event);
      this.#onUpdate?.();
      if (event.event.type === "runEvent") {
        void this.enrichRun(client, event.event.payload.runId, event.event.payload.workerId);
      }
      for (const listener of this.#eventListeners) {
        listener(event);
      }
    });
  }

  /** Hydrates a row's worker profile and active workspace mode from the
   * canonical read RPCs after a `runEvent` introduces it. The event remains
   * the source of lifecycle truth; reads only fill display metadata. */
  async enrichRun(client: CrewClient, runId: string, workerId: string): Promise<void> {
    try {
      const [worker, run] = (await Promise.all([client.request("worker/get", { workerId }), client.request("run/get", { runId })])) as [{ profileRef?: { adapter?: string; model?: string } }, { workspace?: { mode?: string } }];
      const adapter = worker.profileRef?.adapter;
      const model = worker.profileRef?.model;
      if (adapter !== undefined && model !== undefined) {
        this.#state = enrichWorker(this.#state, runId, adapter, model);
      }
      const workspaceMode = run.workspace?.mode;
      if (workspaceMode !== undefined) {
        this.#state = enrichWorkspaceMode(this.#state, runId, workspaceMode);
      }
      this.#onUpdate?.();
    } catch {
      // Metadata is best-effort: a run may settle between the event and
      // these reads, but its reduced lifecycle row remains correct.
    }
  }

  /** Unsubscribes from the runtime. Call on `session_shutdown`. */
  stop(): void {
    this.#unsubscribe?.();
    this.#unsubscribe = undefined;
    this.#onUpdate = undefined;
  }

  /** Full detail text for `/crew run <runId>`, or `undefined` if no
   *  row exists for that run. */
  renderStatus(runId: string): string | undefined {
    const row = this.#state.rows[runId];
    return row === undefined ? undefined : renderRowDetails(row);
  }
}

/** Monitor subcommands that talk to the daemon (connect-first). */
const MONITOR_RPC_SUBCOMMANDS: ReadonlySet<string> = new Set(["runs", "export", "clean", "reopen"]);

/**
 * Routes command output to the UI in interactive mode and to stdout
 * otherwise -- `ui.notify` is a no-op outside interactive mode (print/RPC),
 * and a raw console.log inside it would corrupt the TUI. Mirrors the
 * pattern of every flat command in index.ts.
 */
function respond(cmdCtx: ExtensionCommandContext, text: string, level: "info" | "warning" | "error" = "info"): void {
  if (cmdCtx.hasUI !== true) {
    console.log(text);
  } else {
    cmdCtx.ui.notify(text, level);
  }
}

/** Registers the `/crew` command and the replay-first monitor lifecycle.
 *  Wires the milestone bridge (spec §7.2) onto the monitor's single
 *  subscription so the model is injected with digests on milestones
 *  instead of having to poll the monitor. */
export function registerMonitor(pi: ExtensionAPI, ctx: MonitorControllerContext): void {
  const controller = new MonitorController();
  attachMilestoneBridge(pi, controller);
  let subscribedClient: CrewClient | undefined;

  /** The single source of truth for the `/crew` subcommand surface: both the
   *  registration `description` and the handler's `usage` line derive from it,
   *  so a future management entry cannot silently drift between the two. */
  const subcommandList = ["run <runId>", "runs", "export [runId]", "clean", "reopen <runId>", ...(ctx.management?.keys() ?? [])];

  /**
   * Syncs the widget with the current state: renders the box when there are
   * rows to show, removes the widget when there are none. `force` renders
   * the box even when empty — the explicit `/crew` command uses it, so a
   * healthy-but-empty runtime still answers with the "No Crew runs yet."
   * box rather than silence; the session-start and live-event paths stay
   * hidden when there is nothing to show.
   */
  function refresh(extCtx: ExtensionContext, force = false): void {
    const state = controller.getState();
    const content = force || hasVisibleRows(state) ? renderWidgetBox(state, extCtx.ui.theme) : undefined;
    extCtx.ui.setWidget(WIDGET_KEY, content, { placement: "aboveEditor" });
    pi.appendEntry(MONITOR_ENTRY_TYPE, { sequence: Number(state.lastSequence) });
  }

  async function connect(extCtx: ExtensionContext): Promise<void> {
    if (subscribedClient !== undefined && !subscribedClient.isClosed) {
      return;
    }
    if (subscribedClient !== undefined) {
      // The prior subscription is dead with it; drop it before resubscribing.
      controller.stop();
      subscribedClient = undefined;
    }
    // Resume from whichever is further ahead: what was persisted to the
    // session log, or what this controller has already reduced in memory.
    // `reduceEvent` ignores an event at or below a row's applied sequence,
    // so overlapping replay is a no-op rather than a double-count.
    const fromSequence = Math.max(lastPersistedSequence(extCtx.sessionManager.getEntries() as SessionEntryLike[]), Number(controller.getState().lastSequence));
    try {
      try {
        assertCompatiblePiCodingAgentVersion();
      } catch (err) {
        pi.logger.warn("crew monitor: Pi compatibility warning", {
          error: err instanceof Error ? err.message : String(err),
        });
      }
      const client = await ctx.getClient(extCtx);
      controller.start(client, fromSequence, () => refresh(extCtx));
      subscribedClient = client;
    } catch (err) {
      pi.logger.warn("crew monitor: runtime unavailable", {
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }

  pi.on("session_start", async (_event, extCtx) => {
    await connect(extCtx);
    // Render immediately: a healthy runtime with runs shows the widget; one
    // with no runs keeps it hidden until the first run event (R56, revised:
    // only show the box when there is something to show).
    if (subscribedClient !== undefined) {
      refresh(extCtx);
    }
  });

  pi.registerCommand(MONITOR_COMMAND_NAME, {
    description: `Opens the Crew monitor. Subcommands: ${subcommandList.join(", ")}.`,
    handler: async (args, cmdCtx) => {
      const [sub, runId] = args.trim().split(/\s+/, 2);
      const usage = `Usage: /crew [${subcommandList.join(" | ")}]`;

      if (sub === undefined || sub.length === 0) {
        await connect(cmdCtx);
        if (subscribedClient === undefined) {
          respond(cmdCtx, "Crew runtime is unavailable.", "warning");
          return;
        }
        // An explicit user command renders unconditionally, so /crew against an
        // empty runtime still shows the (empty) monitor box rather than nothing.
        refresh(cmdCtx, true);
        return;
      }

      const management = ctx.management?.get(sub);
      if (management !== undefined) {
        const rest = args.trim().slice(sub.length).trim();
        const result = await management.run(rest, cmdCtx);
        respond(cmdCtx, result.text, result.isError ? "error" : "info");
        return;
      }

      if (sub === "run") {
        if (runId === undefined || runId.length === 0) {
          respond(cmdCtx, "Usage: /crew run <runId>", "error");
          return;
        }
        const details = controller.renderStatus(runId);
        respond(cmdCtx, details ?? `No Crew run found for ${runId}.`, details === undefined ? "warning" : "info");
        return;
      }

      if (!MONITOR_RPC_SUBCOMMANDS.has(sub)) {
        respond(cmdCtx, `Unknown subcommand "${sub}". ${usage}`, "error");
        return;
      }

      await connect(cmdCtx);
      const client = subscribedClient;
      if (client === undefined) {
        respond(cmdCtx, "Crew runtime is unavailable.", "warning");
        return;
      }
      if (sub === "runs") {
        const result = (await client.request("run/list", {})) as { runs?: Array<{ runId?: string; state?: string; workerId?: string }> };
        const runs = result.runs ?? [];
        respond(cmdCtx, runs.length === 0 ? "No Crew runs recorded." : runs.map((run) => `${run.runId ?? "(unknown)"}  ${run.state ?? "unknown"}  worker ${run.workerId ?? "unknown"}`).join("\n"), "info");
        return;
      }
      if (sub === "export") {
        // `events/replay` is intentionally sequence-oriented; its daemon
        // params do not include a run filter. Filter the already validated
        // envelopes here so `/crew export <runId>` cannot leak other runs.
        const replay = (await client.request("events/replay", {})) as Array<{ runId?: string | null }>;
        const events = runId === undefined ? replay : replay.filter((event) => event.runId === runId);
        const exportId = runId ?? "all";
        const directory = join(cmdCtx.cwd, ".omp", "crew");
        const output = join(directory, `export-${exportId}.jsonl`);
        await mkdir(directory, { recursive: true });
        await writeFile(output, events.map((event) => JSON.stringify(event)).join("\n") + (events.length > 0 ? "\n" : ""));
        respond(cmdCtx, `Exported ${events.length} Crew events to ${output}.`, "info");
        return;
      }
      if (sub === "clean") {
        const result = (await client.request("retention/clean", {})) as { deletedEvents: number; runsPruned: number };
        respond(cmdCtx, `Retention clean removed ${result.deletedEvents} events across ${result.runsPruned} maxRuns-pruned runs.`, "info");
        return;
      }
      if (sub === "reopen") {
        if (runId === undefined || runId.length === 0) {
          respond(cmdCtx, "Usage: /crew reopen <runId>", "error");
          return;
        }
        const result = (await client.request("pane/reopen", { runId })) as { backend: string; paneRef: string };
        respond(cmdCtx, result.paneRef.length === 0 ? `No visible backend is available for ${runId}.` : `Reopened ${runId} in ${result.backend}: ${result.paneRef}`, "info");
        return;
      }
    },
  });

  pi.on("session_shutdown", async () => {
    controller.stop();
    // Drop the client reference too, exactly as the dead-subscription repair
    // path in connect() does -- otherwise a later connect() early-returns
    // into a monitor whose subscription no longer exists (R39).
    subscribedClient = undefined;
  });
}
