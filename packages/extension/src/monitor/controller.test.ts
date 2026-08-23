// Tests for the monitor controller's session lifecycle: the widget shows
// when the journal has runs and stays hidden when it doesn't (R56,
// revised), and a `session_shutdown` followed by a new session must
// resubscribe rather than early-return into a dead monitor (R39). Both
// drive `registerMonitor` through a fake ExtensionAPI, mirroring
// tools.test.ts's fake-API pattern.

import { expect, test } from "bun:test";

import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";

import type { CrewClient } from "../client";
import type { EventEnvelope } from "@nikolasd/batman-protocol";
import { registerMonitor } from "./controller";

type SessionHandler = (event: unknown, extCtx: ExtensionContext) => Promise<void>;

interface FakeHarness {
  readonly api: ExtensionAPI;
  readonly handlers: Map<string, SessionHandler>;
  readonly commands: Map<string, { handler: (args: string, ctx: ExtensionContext) => Promise<void> }>;
}

function createFakeApi(): FakeHarness {
  const handlers = new Map<string, SessionHandler>();
  const commands = new Map<string, { handler: (args: string, ctx: ExtensionContext) => Promise<void> }>();
  const api = {
    logger: { debug() {}, info() {}, warn() {}, error() {} },
    appendEntry() {},
    on(event: string, handler: SessionHandler) {
      handlers.set(event, handler);
    },
    registerCommand(name: string, options: { handler: (args: string, ctx: ExtensionContext) => Promise<void> }) {
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
}

function createFakeClient(): FakeClient {
  const fake: FakeClient = {
    subscribeCalls: 0,
    closed: false,
    onEvent: undefined,
    client: undefined as unknown as CrewClient,
  };
  fake.client = {
    get isClosed() {
      return fake.closed;
    },
    close() {
      fake.closed = true;
      fake.onEvent = undefined;
    },
    subscribe(_fromSequence: number, onEvent: (event: EventEnvelope) => void) {
      fake.subscribeCalls += 1;
      fake.onEvent = onEvent;
      return () => {
        fake.onEvent = undefined;
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
