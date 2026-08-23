import { expect, test } from "bun:test";

import type { ExtensionAPI, ExtensionContext, Theme, ThemeColor } from "@oh-my-pi/pi-coding-agent";

import { assertCompatiblePiCodingAgentVersion, PiCodingAgentVersionError } from "./compat";
import type { MonitorRow, MonitorState } from "./model";
import { MAX_WIDGET_ROWS, renderRowDetails, renderRowLine, renderWidgetBox, stateIcon, stateColor, renderWidgetHeader } from "./render";

function row(overrides: Partial<MonitorRow>): MonitorRow {
  return {
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
    ...overrides,
  };
}

function stateOf(rows: readonly MonitorRow[]): MonitorState {
  const byId: Record<string, MonitorRow> = {};
  for (const r of rows) {
    byId[r.runId] = r;
  }
  return { rows: byId, lastSequence: 1 };
}

function fakeTheme(): Theme {
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
    fg: (color: ThemeColor, text: string) => `[${color}]${text}[/${color}]`,
  } as unknown as Theme;
}

test("a row line includes state, harness/model, flags, and pending approvals", () => {
  const line = renderRowLine(
    row({
      state: "waitingUser",
      adapter: "claude",
      model: "claude-sonnet-4",
      flags: {
        degradedControl: true,
        needsReconciliation: false,
        protocolUnhealthy: false,
        policyQuarantined: false,
        workspaceDirty: false,
        childrenActive: false,
      },
      pendingApprovalCount: 2,
    }),
  );
  expect(line).toContain("waitingUser");
  expect(line).toContain("claude/claude-sonnet-4");
  expect(line).toContain("degraded");
  expect(line).toContain("2 pending approvals");
});

test("stateIcon returns the documented codepoint for every known run state", () => {
  expect(stateIcon("queued")).toBe("\u{F0150}");
  expect(stateIcon("starting")).toBe("\u{F14DF}");
  expect(stateIcon("working")).toBe("\u{F1461}");
  expect(stateIcon("waitingUser")).toBe("\u{F0B5A}");
  expect(stateIcon("waitingPeer")).toBe("\u{F000F}");
  expect(stateIcon("paused")).toBe("\u{F03E6}");
  expect(stateIcon("succeeded")).toBe("\u{F05E1}");
  expect(stateIcon("failed")).toBe("\u{F015A}");
  expect(stateIcon("cancelled")).toBe("\u{F073A}");
  expect(stateIcon("lost")).toBe("\u{F0BA6}");
});

test("stateIcon falls back to a generic icon for an unrecognized state", () => {
  expect(stateIcon("totally-unknown")).toBe("\u{F0625}");
});

test("stateColor returns the documented theme color for every known run state", () => {
  expect(stateColor("queued")).toBe("muted");
  expect(stateColor("starting")).toBe("accent");
  expect(stateColor("working")).toBe("accent");
  expect(stateColor("waitingUser")).toBe("warning");
  expect(stateColor("waitingPeer")).toBe("warning");
  expect(stateColor("paused")).toBe("muted");
  expect(stateColor("succeeded")).toBe("success");
  expect(stateColor("failed")).toBe("error");
  expect(stateColor("cancelled")).toBe("dim");
  expect(stateColor("lost")).toBe("error");
});

test("stateColor falls back to the theme's default text color for an unrecognized state", () => {
  expect(stateColor("totally-unknown")).toBe("text");
});

test("renderWidgetHeader returns the bat icon and the Crew label", () => {
  expect(renderWidgetHeader()).toBe("\u{F0B5F} Crew");
});

test("a row line includes the state icon alongside the state word", () => {
  const line = renderRowLine(row({ state: "succeeded" }));
  expect(line).toContain(`${stateIcon("succeeded")} succeeded`);
});

test("renderRowDetails includes worker, action-relevant fields, and timestamps for /crew status", () => {
  const details = renderRowDetails(row({ workspaceMode: "isolated", latestActivity: "question sent", adapter: "codex", model: "gpt-5" }));
  expect(details).toContain("Run: run-1");
  expect(details).toContain("Task: task-1");
  expect(details).toContain("Worker: worker-1");
  expect(details).toContain("Harness/model: codex/gpt-5");
  expect(details).toContain("Workspace mode: isolated");
  expect(details).toContain("Latest activity: question sent");
  expect(details).toContain("First seen:");
  expect(details).toContain("Last event:");
});

test("renderRowDetails names the decision surface for a run with children active", () => {
  const withChildren = row({
    flags: {
      degradedControl: false,
      needsReconciliation: false,
      protocolUnhealthy: false,
      policyQuarantined: false,
      workspaceDirty: false,
      childrenActive: true,
    },
  });
  const details = renderRowDetails(withChildren);
  // The raw flag name still appears in the flag list; the added line is
  // what tells an operator how to resolve it.
  expect(details).toContain("childrenActive");
  expect(details).toContain("crew_child");

  // A run with no children must not carry the pointer at all.
  expect(renderRowDetails(row({}))).not.toContain("crew_child");
});

test("renderWidgetBox embeds the accent-colored header in the top border", () => {
  const lines = renderWidgetBox({ rows: {}, lastSequence: 0 }, fakeTheme());
  expect(lines[0]).toContain("╭─");
  expect(lines[0]).toContain(`[accent]${renderWidgetHeader()}[/accent]`);
});

test("renderWidgetBox wraps the empty-state line in the border, uncolored", () => {
  const lines = renderWidgetBox({ rows: {}, lastSequence: 0 }, fakeTheme());
  expect(lines).toHaveLength(3); // top border, empty-state line, bottom border
  expect(lines[1]).toContain("[text]No Crew runs yet.[/text]");
  expect(lines[1].startsWith("[border]│[/border]")).toBe(true);
  expect(lines[1].endsWith("[border]│[/border]")).toBe(true);
});

test("renderWidgetBox colors each row by its state and ends with a plain bottom border", () => {
  const succeededRow = row({ runId: "run-1", state: "succeeded" });
  const lines = renderWidgetBox(stateOf([succeededRow]), fakeTheme());

  expect(lines).toHaveLength(3);
  expect(lines[1]).toContain(`[success]${renderRowLine(succeededRow)}[/success]`);

  const bottom = lines[lines.length - 1];
  expect(bottom.startsWith("[border]╰")).toBe(true);
  expect(bottom.endsWith("╯[/border]")).toBe(true);
});

test("renderWidgetBox appends a muted overflow line beyond MAX_WIDGET_ROWS", () => {
  const rows = Array.from({ length: MAX_WIDGET_ROWS + 2 }, (_, i) => row({ runId: `run-${i}`, lastEventAt: `2026-01-01T00:${String(i).padStart(2, "0")}:00Z` }));
  const lines = renderWidgetBox(stateOf(rows), fakeTheme());

  // top border + MAX_WIDGET_ROWS rows + 1 overflow line + bottom border
  expect(lines).toHaveLength(MAX_WIDGET_ROWS + 3);
  const overflowLine = lines[lines.length - 2];
  expect(overflowLine).toContain("[muted]");
  expect(overflowLine).toContain("more; use /crew status <runId> for full details.");
});

test("renderWidgetBox produces a top border, every content line, and the bottom border at equal total width", () => {
  // A `fg` that returns text unchanged, unlike `fakeTheme()`'s tagging `fg` — the
  // color-tag wrapper length would otherwise interfere with measuring raw visual
  // width, which is exactly what this test checks.
  const plainTheme = {
    boxRound: fakeTheme().boxRound,
    fg: (_color: ThemeColor, text: string) => text,
  } as unknown as Theme;

  const rows = [row({ runId: "run-1", state: "succeeded", lastEventAt: "2026-01-01T00:00:00Z" }), row({ runId: "run-2", state: "queued", lastEventAt: "2026-01-01T00:01:00Z" })];
  const lines = renderWidgetBox(stateOf(rows), plainTheme);

  // Width equality must hold in *code points*, not UTF-16 code units. Every
  // content line here carries a `stateIcon(...)` via `renderRowLine`, and the
  // header (`renderWidgetHeader`) carries `BAT_ICON` — both astral-plane
  // characters stored as UTF-16 surrogate pairs, so `.length` overcounts them
  // by 1 each. That overcount is exactly what let the original `.length`-based
  // implementation pass this same width check by coincidence: `.length`
  // equality is tautological given how `assembleBox` derives its padding, so
  // it can never actually detect a code-point-counting bug. Comparing
  // `codePointLength` instead reproduces how many terminal cells each line
  // occupies, which is the property that actually matters, and is exactly
  // what breaks under the surrogate-pair bug this test guards against: the
  // bottom border (no icon) would come out 1 code point wider than the top
  // border and every icon-bearing content row.
  const codePointLength = (text: string): number => Array.from(text).length;
  const widths = new Set(lines.map((line) => codePointLength(line)));
  expect(widths.size).toBe(1);

  // `.length`-based width also happens to be self-consistent by construction
  // (padding is derived from whatever measure is used to build it), so this
  // assertion alone would not have caught the bug — it's here only to
  // document that both metrics agree once the underlying measurement is
  // fixed, i.e. every line's UTF-16 length equals its code point length plus
  // exactly one surrogate pair for the icon-bearing lines (top border + every
  // content row), and plus zero for the plain bottom border.
  const utf16Lengths = lines.map((line) => line.length);
  const codePointLengths = lines.map((line) => codePointLength(line));
  for (let i = 0; i < lines.length; i++) {
    const isBottomBorder = i === lines.length - 1;
    expect(utf16Lengths[i] - codePointLengths[i]).toBe(isBottomBorder ? 0 : 1);
  }
});

test("renderWidgetBox stays equal-width by code points for the empty state, where the header carries an icon but the content line does not", () => {
  // The header (`BAT_ICON`) always carries a surrogate-pair icon; the
  // empty-state line ("No Crew runs yet.") never does. Pre-fix, that
  // asymmetry meant the top border's fill-character count (derived from
  // `header.length`) came out 1 short relative to the body/bottom border
  // (derived from a line with no surrogate pair to overcount) — the exact
  // mismatch the surrogate-pair bug produced, isolated from any body-row
  // icon so it can't be masked by icons appearing on every side of the
  // comparison at once.
  const plainTheme = {
    boxRound: fakeTheme().boxRound,
    fg: (_color: ThemeColor, text: string) => text,
  } as unknown as Theme;

  const lines = renderWidgetBox({ rows: {}, lastSequence: 0 }, plainTheme);

  const codePointLength = (text: string): number => Array.from(text).length;
  const widths = new Set(lines.map((line) => codePointLength(line)));
  expect(widths.size).toBe(1);
});

// ------------------------------------------- version compatibility check

test("the installed @oh-my-pi/pi-coding-agent is within the supported range", () => {
  expect(() => assertCompatiblePiCodingAgentVersion()).not.toThrow();
});

test("a version outside the supported range throws a named PiCodingAgentVersionError", () => {
  expect(() => assertCompatiblePiCodingAgentVersion("16.9.0")).toThrow(PiCodingAgentVersionError);
  expect(() => assertCompatiblePiCodingAgentVersion("18.0.0")).toThrow(PiCodingAgentVersionError);
});

test("a version at the exact lower bound is accepted", () => {
  expect(() => assertCompatiblePiCodingAgentVersion("17.0.7")).not.toThrow();
});

// ---------------------------------- no-model fixture extension compile check

test("a no-model fixture extension compiles and runs pi.appendEntry + ctx.ui.setWidget against the installed OMP surface", () => {
  assertCompatiblePiCodingAgentVersion();

  const appendedEntries: Array<{ customType: string; data: unknown }> = [];
  const widgets: Array<{ key: string; content: unknown; options: unknown }> = [];

  const fakePi = {
    appendEntry: (customType: string, data?: unknown) => {
      appendedEntries.push({ customType, data });
    },
  } as unknown as ExtensionAPI;

  const fakeCtx = {
    ui: {
      setWidget: (key: string, content: unknown, options?: unknown) => {
        widgets.push({ key, content, options });
      },
    },
  } as unknown as ExtensionContext;

  // The exact calls the plan pins to OMP 17.0.7's public surface.
  function fixtureExtension(pi: ExtensionAPI, ctx: ExtensionContext): void {
    pi.appendEntry("crew-monitor", { sequence: 1 });
    ctx.ui.setWidget("crew-monitor", ["fixture"], { placement: "aboveEditor" });
  }

  fixtureExtension(fakePi, fakeCtx);

  expect(appendedEntries).toEqual([{ customType: "crew-monitor", data: { sequence: 1 } }]);
  expect(widgets).toEqual([{ key: "crew-monitor", content: ["fixture"], options: { placement: "aboveEditor" } }]);
});
