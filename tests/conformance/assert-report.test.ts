// Tests for the release conformance gate's validation rules.
//
// The gate exists to stop a stub or fabricated report from being published,
// so these tests are written as attacks on it: each mutates one field of an
// otherwise-valid report and asserts the gate refuses it. A gate that
// accepted any of these would be decorative.

import { describe, expect, test } from "bun:test";

import { type AdapterConformanceReport, type CombinedReport, assertReportComplete } from "./assert-report";

/** A capability set with everything the gated scenarios can prove. */
function capabilities(): AdapterConformanceReport["declaredCapabilities"] {
  return {
    protocol: "structured",
    resume: "session",
    steering: "turn",
    approvals: "controllable",
    structuredResult: true,
    usage: "aggregate",
    nested: "none",
    nativeView: "none",
    workspaceControl: "write",
    durability: "vendorResumable",
  };
}

/** The five scenarios that gate a capability, plus one ungated scenario. */
function scenarios(): AdapterConformanceReport["scenarios"] {
  return [
    { name: "approval", outcome: "pass", detail: "ok" },
    { name: "follow_up", outcome: "pass", detail: "ok" },
    { name: "session_resume", outcome: "pass", detail: "ok" },
    { name: "isolated_write", outcome: "pass", detail: "ok" },
    { name: "managed_nesting_rejection", outcome: "pass", detail: "ok" },
    { name: "probe", outcome: "pass", detail: "ok" },
  ];
}

function adapterReport(adapter: string): AdapterConformanceReport {
  return {
    adapter,
    mode: "fixture",
    version: "1.2.3",
    declaredCapabilities: capabilities(),
    effectiveCapabilities: capabilities(),
    scenarios: scenarios(),
    passed: true,
  };
}

// crew-v2 gap-closure WP-C: fixture mode is TUI-sourced now (spec §4.6) --
// the headless control plane and its claude/codex/copilot/ompRpc labels
// are retired. Every key below is hyphenated, so bracket notation (not
// dot access) is required throughout this file.
function validReport(): CombinedReport {
  return {
    timestamp: "2026-08-05T00:00:00Z",
    adapters: {
      "claude-tui": adapterReport("claude-tui"),
      "codex-tui": adapterReport("codex-tui"),
      "copilot-tui": adapterReport("copilot-tui"),
      "omp-tui": adapterReport("omp-tui"),
    },
  };
}

/** Applies `mutate` to a fresh valid report and returns the thrown message. */
function rejectionFor(mutate: (report: CombinedReport) => void): string {
  const report = validReport();
  mutate(report);
  try {
    assertReportComplete(report);
  } catch (err) {
    return err instanceof Error ? err.message : String(err);
  }
  throw new Error("the gate accepted a report it must have refused");
}

describe("conformance gate", () => {
  test("a complete, self-consistent report is accepted", () => {
    expect(() => assertReportComplete(validReport())).not.toThrow();
  });

  test("the four adapters are required under their real TUI labels", () => {
    // "omp-tui", never a mechanical "ompRpc-tui" -- the wrong guess was
    // exactly why the previous (headless-era) gate validated a report the
    // CLI could never produce.
    const message = rejectionFor((r) => {
      const entry = r.adapters["omp-tui"];
      // biome-ignore lint/performance/noDelete: exercising a missing key
      delete (r.adapters as Record<string, unknown>)["omp-tui"];
      (r.adapters as Record<string, unknown>)["ompRpc-tui"] = entry;
    });
    expect(message).toContain("missing adapter: omp-tui");
  });

  test("a zero-scenario report is rejected as a stub", () => {
    const message = rejectionFor((r) => {
      (r.adapters["claude-tui"] as { scenarios: unknown }).scenarios = [];
    });
    expect(message).toContain("zero scenarios");
  });

  test("a report where every scenario failed is rejected", () => {
    const message = rejectionFor((r) => {
      (r.adapters["codex-tui"] as { scenarios: unknown }).scenarios = scenarios().map((s) => ({ ...s, outcome: "fail" }));
    });
    expect(message).toContain("no passing scenarios");
  });

  test("an adapter whose report names a different adapter is rejected", () => {
    const message = rejectionFor((r) => {
      (r.adapters["copilot-tui"] as { adapter: string }).adapter = "claude-tui";
    });
    expect(message).toContain("Adapter mismatch");
  });

  test("capabilities must be an object, not the old string array", () => {
    const message = rejectionFor((r) => {
      (r.adapters["claude-tui"] as { declaredCapabilities: unknown }).declaredCapabilities = [];
    });
    expect(message).toContain("declaredCapabilities");
  });

  // The two assertions that make this a real gate rather than a shape check.

  test("a capability claimed on the strength of a FAILED scenario is rejected", () => {
    const message = rejectionFor((r) => {
      const codex = r.adapters["codex-tui"];
      const resume = codex.scenarios.find((s) => s.name === "session_resume");
      (resume as { outcome: "pass" | "fail" | "skipped"; detail: string }).outcome = "fail";
      (resume as { detail: string }).detail = "vendor refused session/load";
      // `effectiveCapabilities.resume` still claims "session" anyway.
    });
    expect(message).toContain("effectiveCapabilities.resume");
    expect(message).toContain("session_resume");
    expect(message).toContain("vendor refused session/load");
  });

  test("an effective capability the adapter never declared is rejected", () => {
    const message = rejectionFor((r) => {
      (r.adapters["claude-tui"].effectiveCapabilities as { usage: string }).usage = "tokensAndCost";
    });
    expect(message).toContain("effectiveCapabilities.usage");
    expect(message).toContain("carried through unchanged");
  });

  test("a gated capability downgraded to its sentinel is accepted", () => {
    // The legitimate shape: the scenario failed, so the capability was
    // downgraded rather than claimed. This must NOT be rejected.
    const report = validReport();
    const copilot = report.adapters["copilot-tui"];
    const resume = copilot.scenarios.find((s) => s.name === "session_resume");
    (resume as { outcome: "pass" | "fail" | "skipped" }).outcome = "fail";
    (copilot.effectiveCapabilities as { resume: string }).resume = "none";
    expect(() => assertReportComplete(report)).not.toThrow();
  });

  test("a missing capability-gating scenario is rejected", () => {
    const message = rejectionFor((r) => {
      (r.adapters["claude-tui"] as { scenarios: unknown }).scenarios = scenarios().filter((s) => s.name !== "isolated_write");
    });
    expect(message).toContain("isolated_write");
  });
});
