// Tests for the monitor controller's session lifecycle: the widget shows
// when the journal has runs and stays hidden when it doesn't (R56,
// revised), and a `session_shutdown` followed by a new session must
// resubscribe rather than early-return into a dead monitor (R39). Both
// drive `registerMonitor` through a fake ExtensionAPI, mirroring
// tools.test.ts's fake-API pattern.

import { expect, spyOn, test } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { EventEnvelope } from "@nikolasd/crew-protocol";
import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import type { CrewClient } from "../client";
import { registerMonitor } from "./controller";

type SessionHandler = (event: unknown, extCtx: ExtensionContext) => Promise<void>;

interface FakeCommand {
  handler: (args: string, ctx: ExtensionContext) => Promise<void>;
  getArgumentCompletions?: (prefix: string) => Array<{ value: string; description?: string; hint?: string }> | null;
}

interface FakeHarness {
  readonly api: ExtensionAPI;
  readonly handlers: Map<string, SessionHandler>;
  readonly commands: Map<string, FakeCommand>;
}

function createFakeApi(): FakeHarness {
  const handlers = new Map<string, SessionHandler>();
  const commands = new Map<string, FakeCommand>();
  const api = {
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    appendEntry() {},
    on(event: string, handler: SessionHandler) {
      handlers.set(event, handler);
    },
    registerCommand(name: string, options: FakeCommand) {
      commands.set(name, options);
    },
  };
  return { api: api as unknown as ExtensionAPI, handlers, commands };
}

interface FakeClient {
  subscribeCalls: number;
  closed: boolean;
  onEvent: ((event: EventEnvelope) => void) | undefined;
  client: CrewClient;
  /** Fires every listener registered via the client's `onClose` and marks it
   *  closed -- simulates the daemon dropping the connection out from under
   *  the monitor (CREW-5), as opposed to `client.close()`, which is the
   *  monitor's own side initiating it. Both route through the same
   *  close-notification path, exactly like the real `CrewClient`. */
  simulateClose(): void;
  /** Marks the client open again, as if a fresh reconnect succeeded --
   *  mirrors `resolveClient`'s real contract (a *resolved* client is never
   *  closed) for a test that drives more than one connect cycle. */
  reopen(): void;
}

function createFakeClient(): FakeClient {
  const closeListeners = new Set<() => void>();
  const fake: FakeClient = {
    subscribeCalls: 0,
    closed: false,
    onEvent: undefined,
    client: undefined as unknown as CrewClient,
    simulateClose() {
      if (fake.closed) {
        return;
      }
      fake.closed = true;
      fake.onEvent = undefined;
      for (const listener of closeListeners) {
        listener();
      }
      closeListeners.clear();
    },
    reopen() {
      fake.closed = false;
    },
  };
  fake.client = {
    get isClosed() {
      return fake.closed;
    },
    close() {
      fake.simulateClose();
    },
    subscribe(_fromSequence: number, onEvent: (event: EventEnvelope) => void) {
      fake.subscribeCalls += 1;
      fake.onEvent = onEvent;
      return () => {
        fake.onEvent = undefined;
      };
    },
    onClose(listener: () => void) {
      if (fake.closed) {
        queueMicrotask(listener);
        return () => {};
      }
      closeListeners.add(listener);
      return () => {
        closeListeners.delete(listener);
      };
    },
  } as unknown as CrewClient;
  return fake;
}

function fakeTheme(): unknown {
  return {
    boxRound: {
      topLeft: "╭",
      topRight: "╮",
      bottomLeft: "╰",
      bottomRight: "╯",
      horizontal: "─",
      vertical: "│",
      cross: "┼",
      teeDown: "┬",
      teeUp: "┴",
      teeRight: "├",
      teeLeft: "┤",
    },
    fg: (_color: unknown, text: string) => text,
  };
}

function fakeExtensionContext(widgetCalls: unknown[][]): ExtensionContext {
  return {
    sessionManager: { getEntries: () => [] },
    ui: {
      theme: fakeTheme(),
      setWidget(...args: unknown[]) {
        widgetCalls.push(args);
      },
      notify() {},
    },
  } as unknown as ExtensionContext;
}

function fakeCommandContext(widgetCalls: unknown[][], hasUI: boolean): { ctx: ExtensionContext; notifications: Array<{ message: string; level: string }> } {
  const notifications: Array<{ message: string; level: string }> = [];
  const ctx = {
    hasUI,
    sessionManager: { getEntries: () => [] },
    ui: {
      theme: fakeTheme(),
      setWidget(...args: unknown[]) {
        widgetCalls.push(args);
      },
      notify(message: string, level: string) {
        notifications.push({ message, level });
      },
    },
  } as unknown as ExtensionContext;
  return { ctx, notifications };
}

test("session_start keeps the widget hidden when the journal has no runs (R56, revised)", async () => {
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);

  // A healthy runtime with no runs: the widget is explicitly removed, not
  // shown with an empty box.
  expect(fake.subscribeCalls).toBe(1);
  expect(widgetCalls.length).toBe(1);
  expect(widgetCalls[0]?.[0]).toBe("crew-monitor");
  expect(widgetCalls[0]?.[1]).toBeUndefined();
  expect(widgetCalls[0]?.[2]).toEqual({ placement: "aboveEditor" });
});

test("the widget appears the moment a run row is created, and stays hidden before it (R56, revised)", async () => {
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);
  expect(widgetCalls.length).toBe(1);
  expect(Array.isArray(widgetCalls[0]?.[1])).toBe(false);

  fake.onEvent?.(runEventEnvelope(1));

  expect(widgetCalls.length).toBe(2);
  expect(Array.isArray(widgetCalls[1]?.[1])).toBe(true);
});

test("/crew renders the box even when empty (explicit command overrides the auto-hide)", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const cmdCtx = fakeExtensionContext(widgetCalls);

  await commands.get("crew")?.handler("", cmdCtx);

  // The user explicitly asked for the monitor, so the (empty) box renders
  // even with no runs — the asymmetry with session_start.
  expect(widgetCalls.length).toBe(1);
  expect(Array.isArray(widgetCalls[0]?.[1])).toBe(true);
});

test("an unknown /crew subcommand is a usage error, never a silent monitor render", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const { ctx, notifications } = fakeCommandContext(widgetCalls, true);

  await commands.get("crew")?.handler("bogus", ctx);

  expect(widgetCalls.length).toBe(0);
  expect(notifications.length).toBe(1);
  expect(notifications[0]?.message).toContain("Unknown subcommand");
  expect(notifications[0]?.message).toContain("Usage: /crew");
  expect(notifications[0]?.level).toBe("error");
});

test("/crew output console.logs (not notify) outside interactive mode", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const { ctx, notifications } = fakeCommandContext(widgetCalls, false);
  const logged: string[] = [];
  const logSpy = spyOn(console, "log").mockImplementation((message: string) => {
    logged.push(message);
  });

  try {
    await commands.get("crew")?.handler("bogus", ctx);
  } finally {
    logSpy.mockRestore();
  }

  expect(notifications.length).toBe(0);
  expect(logged.length).toBe(1);
  expect(logged[0]).toContain("Unknown subcommand");
});

test("bare /crew still renders the monitor box", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const { ctx } = fakeCommandContext(widgetCalls, true);

  await commands.get("crew")?.handler("", ctx);

  expect(widgetCalls.length).toBe(1);
  expect(Array.isArray(widgetCalls[0]?.[1])).toBe(true);
});

/** A working runEvent envelope, shaped like the runtime's (mirrors model.test.ts). */
function runEventEnvelope(sequence: number): EventEnvelope {
  return {
    sequence,
    timestamp: "2026-01-01T00:00:00Z",
    projectId: "018f0000-0000-7000-8000-000000000000",
    taskId: "task-1",
    workerId: null,
    runId: "run-1",
    parentWorkerId: null,
    source: "runtime",
    vendorEventRef: null,
    event: {
      type: "runEvent",
      payload: { kind: "runWorking", runId: "run-1", taskId: "task-1", workerId: "worker-1", state: "working" },
    },
  };
}

test("a session_shutdown followed by a new session_start resubscribes instead of early-returning into a dead monitor (R39)", async () => {
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);
  expect(fake.subscribeCalls).toBe(1);

  await handlers.get("session_shutdown")?.(undefined, extCtx);

  // The old client object is still open (isClosed === false); only the
  // subscription was torn down. A new session must resubscribe anyway.
  await handlers.get("session_start")?.(undefined, extCtx);
  expect(fake.subscribeCalls).toBe(2);
});

test("a closed client is repaired on the next connect even without the shutdown clear (production's index.ts close path)", async () => {
  // Production closes the cached client in its own session_shutdown handler
  // (index.ts), so connect()'s pre-existing repair branch (isClosed check)
  // fires regardless of R39's clear. This pins that path: even if the
  // subscribedClient reference survives, a closed client must be dropped
  // and resubscribed.
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);
  expect(fake.subscribeCalls).toBe(1);

  // No session_shutdown here on purpose: only the client is closed, the
  // subscribedClient reference is still set, so the repair branch is the
  // only thing that can save the monitor.
  fake.client.close();

  await handlers.get("session_start")?.(undefined, extCtx);
  expect(fake.subscribeCalls).toBe(2);
});

test("CREW-5: an unexpected client close reconnects on its own, with no new session_start or /crew", async () => {
  // The gap this closes: before CREW-5, connect() only ever ran from
  // session_start or the /crew command -- a daemon restart mid-session
  // left the widget silently blind until the user happened to type /crew.
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  // A resolved client is never closed (resolveClient's real contract) --
  // simulate that here so a second connect cycle gets a usable client,
  // exactly like a real reconnect would.
  registerMonitor(api, {
    getClient: async () => {
      fake.reopen();
      return fake.client;
    },
  });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);
  expect(fake.subscribeCalls).toBe(1);

  // The daemon restarts: the socket drops out from under the monitor,
  // nothing the user did.
  fake.simulateClose();
  expect(fake.subscribeCalls).toBe(1); // not yet -- the backoff hasn't fired

  // Past the initial backoff delay, with no session_start/`/crew` in between.
  await new Promise<void>((resolve) => setTimeout(resolve, 400));
  expect(fake.subscribeCalls).toBe(2);
});

test("CREW-5: session_shutdown cancels a pending automatic reconnect", async () => {
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, {
    getClient: async () => {
      fake.reopen();
      return fake.client;
    },
  });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);
  expect(fake.subscribeCalls).toBe(1);

  fake.simulateClose();
  await handlers.get("session_shutdown")?.(undefined, extCtx);

  // Past when the (cancelled) backoff would have fired.
  await new Promise<void>((resolve) => setTimeout(resolve, 400));
  expect(fake.subscribeCalls).toBe(1);
});

test("CREW-5 review should-fix: a close firing AFTER session_shutdown does not re-arm a reconnect", async () => {
  // Production's own session_shutdown handler (index.ts) runs *after*
  // registerMonitor's -- it closes the shared cached client, which fires
  // this listener's onClose after the monitor's own cleanup already ran.
  // A bare clearTimeout in the shutdown handler cannot defend against a
  // close arriving afterward and scheduling a brand new timer; only a
  // sticky flag can.
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, {
    getClient: async () => {
      fake.reopen();
      return fake.client;
    },
  });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);
  expect(fake.subscribeCalls).toBe(1);

  await handlers.get("session_shutdown")?.(undefined, extCtx);
  // Simulates index.ts's later session_shutdown handler closing the same
  // shared cached client after registerMonitor's own handler already ran.
  fake.simulateClose();

  await new Promise<void>((resolve) => setTimeout(resolve, 400));
  expect(fake.subscribeCalls).toBe(1);
});

test("CREW-5 review should-fix: automatic reconnect uses a no-spawn client resolver, never the spawning one (ADR-0008)", async () => {
  // Without this, the automatic loop calling the same spawn-on-demand
  // resolver as session_start/`/crew` would silently convert an
  // intentional daemon idle-exit into "never idle": the loop would just
  // spawn a fresh one every time, forever, for a session doing nothing.
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  let spawningCalls = 0;
  let noSpawnCalls = 0;
  registerMonitor(api, {
    getClient: async () => {
      spawningCalls += 1;
      fake.reopen();
      return fake.client;
    },
    getClientWithoutSpawning: async () => {
      noSpawnCalls += 1;
      fake.reopen();
      return fake.client;
    },
  });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);
  expect(spawningCalls).toBe(1);
  expect(noSpawnCalls).toBe(0);

  fake.simulateClose();
  await new Promise<void>((resolve) => setTimeout(resolve, 400));

  expect(fake.subscribeCalls).toBe(2);
  expect(noSpawnCalls).toBe(1);
  expect(spawningCalls).toBe(1); // unchanged -- the automatic path never spawns
});

test("CREW-5 review should-fix: a new session_start re-arms automatic reconnect after a prior session's shutdown", async () => {
  // Guards against a too-blunt fix for the shutdown-ordering hazard: the
  // shuttingDown flag must not stay stuck forever once set -- a fresh
  // session in the same extension instance needs its own working
  // auto-reconnect, not one silently disarmed by a previous session's
  // cleanup.
  const { api, handlers } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, {
    getClient: async () => {
      fake.reopen();
      return fake.client;
    },
  });

  const widgetCalls: unknown[][] = [];
  const extCtx = fakeExtensionContext(widgetCalls);

  await handlers.get("session_start")?.(undefined, extCtx);
  await handlers.get("session_shutdown")?.(undefined, extCtx);

  // A fresh session in the same extension instance.
  await handlers.get("session_start")?.(undefined, extCtx);
  expect(fake.subscribeCalls).toBe(2);

  fake.simulateClose();
  await new Promise<void>((resolve) => setTimeout(resolve, 400));
  expect(fake.subscribeCalls).toBe(3);
});

test("/crew runs, export, clean, and reopen invoke their scoped RPC contracts", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  const requests: Array<{ method: string; params: unknown }> = [];
  (fake.client as unknown as { request(method: string, params?: unknown): Promise<unknown> }).request = async (method, params) => {
    requests.push({ method, params });
    switch (method) {
      case "run/list":
        return { runs: [{ runId: "run-1", state: "working", workerId: "worker-1" }] };
      case "events/replay":
        return [
          { sequence: 1, runId: "run-1" },
          { sequence: 2, runId: "other-run" },
        ];
      case "retention/clean":
        return { deletedEvents: 4, runsPruned: 1 };
      case "pane/reopen":
        return { backend: "tmux", paneRef: "crew:0.1" };
      default:
        throw new Error(`unexpected ${method}`);
    }
  };
  registerMonitor(api, { getClient: async () => fake.client });
  const directory = await mkdtemp(join(tmpdir(), "crew-monitor-command-"));
  const notifications: string[] = [];
  const cmdCtx = {
    ...fakeExtensionContext([]),
    hasUI: true,
    cwd: directory,
    ui: {
      ...fakeExtensionContext([]).ui,
      notify(message: string) {
        notifications.push(message);
      },
    },
  } as unknown as ExtensionContext;
  const command = commands.get("crew");
  await command?.handler("runs", cmdCtx);
  await command?.handler("export run-1", cmdCtx);
  await command?.handler("clean", cmdCtx);
  await command?.handler("reopen run-1", cmdCtx);

  expect(requests.map((request) => request.method)).toEqual(["run/list", "events/replay", "retention/clean", "pane/reopen"]);
  expect(requests[3]?.params).toEqual({ runId: "run-1" });
  const jsonl = await readFile(join(directory, ".omp", "crew", "export-run-1.jsonl"), "utf8");
  expect(jsonl).toContain('"runId":"run-1"');
  expect(jsonl).not.toContain("other-run");
  expect(notifications).toContain("Retention clean removed 4 events across 1 maxRuns-pruned runs.");
  await rm(directory, { recursive: true, force: true });
});

test("/crew dispatches a management subcommand and reports through respond", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  const seen: string[] = [];
  registerMonitor(api, {
    getClient: async () => fake.client,
    management: new Map([
      [
        "health",
        {
          description: "Runtime health",
          run: async (args: string) => {
            seen.push(args);
            return { text: "Crew runtime: running", isError: false };
          },
        },
      ],
    ]),
  });

  const widgetCalls: unknown[][] = [];
  const { ctx, notifications } = fakeCommandContext(widgetCalls, true);

  await commands.get("crew")?.handler("health", ctx);

  expect(seen).toEqual([""]);
  expect(widgetCalls.length).toBe(0);
  expect(notifications[0]?.message).toContain("running");
  expect(notifications[0]?.level).toBe("info");
});

test("a management subcommand receives its remaining arguments verbatim", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  const seen: string[] = [];
  registerMonitor(api, {
    getClient: async () => fake.client,
    management: new Map([
      [
        "config",
        {
          description: "Config",
          run: async (args: string) => {
            seen.push(args);
            return { text: "ok", isError: false };
          },
        },
      ],
    ]),
  });

  const { ctx } = fakeCommandContext([], true);
  await commands.get("crew")?.handler("config print effective", ctx);

  expect(seen).toEqual(["print effective"]);
});

test("/crew run <runId> answers from reduced state without connecting", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const { ctx, notifications } = fakeCommandContext([], true);
  await commands.get("crew")?.handler("run missing-run", ctx);

  expect(fake.subscribeCalls).toBe(0);
  expect(notifications[0]?.message).toContain("No Crew run found for missing-run");
});

test("/crew run with no runId is a usage error at error level", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const widgetCalls: unknown[][] = [];
  const { ctx, notifications } = fakeCommandContext(widgetCalls, true);

  await commands.get("crew")?.handler("run", ctx);

  expect(widgetCalls.length).toBe(0);
  expect(notifications.length).toBe(1);
  expect(notifications[0]?.message).toBe("Usage: /crew run <runId>");
  expect(notifications[0]?.level).toBe("error");
});

test("/crew reopen with no runId is a usage error at error level", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, { getClient: async () => fake.client });

  const { ctx, notifications } = fakeCommandContext([], true);

  await commands.get("crew")?.handler("reopen", ctx);

  expect(notifications.length).toBe(1);
  expect(notifications[0]?.message).toBe("Usage: /crew reopen <runId>");
  expect(notifications[0]?.level).toBe("error");
});

test("/crew clean output console.logs (not notify) outside interactive mode", async () => {
  // Guards the headless path on a *success* output, not just an error: the
  // only thing that would catch a future direct `cmdCtx.ui.notify` creeping
  // back into one of the RPC branches.
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  (fake.client as unknown as { request(method: string, params?: unknown): Promise<unknown> }).request = async () => ({ deletedEvents: 4, runsPruned: 1 });
  registerMonitor(api, { getClient: async () => fake.client });

  const { ctx, notifications } = fakeCommandContext([], false);
  const logged: string[] = [];
  const logSpy = spyOn(console, "log").mockImplementation((message: string) => {
    logged.push(message);
  });

  try {
    await commands.get("crew")?.handler("clean", ctx);
  } finally {
    logSpy.mockRestore();
  }

  expect(notifications.length).toBe(0);
  expect(logged).toEqual(["Retention clean removed 4 events across 1 maxRuns-pruned runs."]);
});

test("/crew completes subcommands from both the monitor set and the management map", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, {
    getClient: async () => fake.client,
    management: new Map([["health", { description: "Runtime health", run: async () => ({ text: "", isError: false }) }]]),
  });

  const complete = commands.get("crew")?.getArgumentCompletions;
  expect(complete).toBeDefined();

  const all = complete!("");
  expect(all?.map((item) => item.value)).toEqual(["health", "run", "runs", "export", "clean", "reopen"]);

  const runOnly = complete!("run");
  expect(runOnly?.map((item) => item.value)).toEqual(["run", "runs"]);

  expect(complete!("zzz")).toBeNull();
});

test("a management subcommand's rejection surfaces through respond, never as an unhandled exception", async () => {
  const { api, commands } = createFakeApi();
  const fake = createFakeClient();
  registerMonitor(api, {
    getClient: async () => fake.client,
    management: new Map([
      [
        "doctor",
        {
          description: "Diagnostics",
          run: async () => {
            throw new Error("no crewd binary installed for this version; run /crew-install to download it, or set OMP_CREW_BINARY to a local build");
          },
        },
      ],
    ]),
  });

  const widgetCalls: unknown[][] = [];
  const { ctx, notifications } = fakeCommandContext(widgetCalls, true);

  await commands.get("crew")?.handler("doctor", ctx);

  expect(widgetCalls.length).toBe(0);
  expect(notifications.length).toBe(1);
  expect(notifications[0]?.message).toContain("no crewd binary installed");
  expect(notifications[0]?.level).toBe("error");
});
