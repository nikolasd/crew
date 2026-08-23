// Release-gate validation for a combined conformance report.
//
// Models the real `ConformanceReport` shape emitted by
// `crates/runtime/src/conformance/report.rs` (serialized camelCase). The
// point of this module is that a *stub* or *fabricated* report cannot pass:
// beyond field presence it re-derives the one invariant the Rust side
// promises, namely that `effectiveCapabilities` is the subset of
// `declaredCapabilities` that this run's own scenarios actually proved.

import { readFileSync } from "node:fs";

/** One scenario outcome. `name` is a `conformance::scenario` constant. */
export interface ScenarioResult {
  readonly name: string;
  /** `"pass"` proved, `"fail"` disproved, `"skipped"` never attempted. */
  readonly outcome: "pass" | "fail" | "skipped";
  readonly detail: string;
}

/**
 * `AdapterCapabilities`, an object of ten fields — not a string array.
 * Values are enum wire names (e.g. `"session"`, `"none"`) except
 * `structuredResult`, which is a boolean.
 */
export interface AdapterCapabilities {
  readonly protocol: string;
  readonly resume: string;
  readonly steering: string;
  readonly approvals: string;
  readonly structuredResult: boolean;
  readonly usage: string;
  readonly nested: string;
  readonly nativeView: string;
  readonly workspaceControl: string;
  readonly durability: string;
}

export interface AdapterConformanceReport {
  readonly adapter: string;
  readonly mode: "fixture" | "live";
  readonly version: string | null;
  readonly declaredCapabilities: AdapterCapabilities;
  readonly effectiveCapabilities: AdapterCapabilities;
  readonly scenarios: readonly ScenarioResult[];
  readonly passed: boolean;
}

export interface CombinedReport {
  readonly timestamp: string;
  readonly adapters: Record<string, AdapterConformanceReport>;
}

/** Adapter wire names. There is no `omp-rpc`; the wire name is `ompRpc`. */
const EXPECTED_ADAPTERS = ["claude", "codex", "copilot", "ompRpc"] as const;

/**
 * The capability fields whose value is downgraded when a specific scenario
 * fails, mirroring `report.rs`'s `downgrade_on_scenario_failure` exactly.
 * `sentinel` is the value the field is forced to on failure.
 *
 * These five are the only capabilities a scenario can prove, so they are
 * the only ones this module can hold to account. A capability appearing
 * here with a non-sentinel value MUST have its scenario passing.
 */
const CAPABILITY_GATES = [
  { field: "approvals", scenario: "approval", sentinel: "none" },
  { field: "steering", scenario: "follow_up", sentinel: "none" },
  { field: "resume", scenario: "session_resume", sentinel: "none" },
  { field: "workspaceControl", scenario: "isolated_write", sentinel: "readOnly" },
  { field: "nested", scenario: "managed_nesting_rejection", sentinel: "none" },
] as const satisfies readonly { field: keyof AdapterCapabilities; scenario: string; sentinel: string }[];

/**
 * Loads and validates a conformance report file.
 *
 * @throws Error if the report is unreadable, malformed, or fails validation.
 */
export function loadAndValidateReport(filePath: string): void {
  let content: string;
  try {
    content = readFileSync(filePath, "utf-8");
  } catch (err) {
    throw new Error(`Failed to read report file ${filePath}: ${err}`);
  }

  let report: unknown;
  try {
    report = JSON.parse(content);
  } catch (err) {
    throw new Error(`Failed to parse report JSON: ${err}`);
  }

  assertReportComplete(report);
}

/**
 * Asserts a combined report covers every adapter and that each adapter's
 * report is internally consistent.
 *
 * @throws Error on the first violation, naming the adapter and the reason.
 */
export function assertReportComplete(report: unknown): void {
  if (report === null || typeof report !== "object") {
    throw new Error("Report must be an object");
  }
  if (!("adapters" in report) || report.adapters === null || typeof report.adapters !== "object") {
    throw new Error("Report missing 'adapters' field");
  }
  const adapters = report.adapters as Record<string, unknown>;

  for (const adapter of EXPECTED_ADAPTERS) {
    const entry = adapters[adapter];
    if (entry === undefined) {
      throw new Error(`Report missing adapter: ${adapter}`);
    }
    assertAdapterReportValid(entry, adapter);
  }
}

/** Asserts one adapter's report is well-formed and self-consistent. */
function assertAdapterReportValid(value: unknown, adapterName: string): void {
  if (value === null || typeof value !== "object") {
    throw new Error(`Adapter ${adapterName} report must be an object`);
  }
  const report = value as Partial<AdapterConformanceReport>;

  if (report.adapter !== adapterName) {
    throw new Error(`Adapter mismatch: report says '${report.adapter}', expected '${adapterName}'`);
  }
  if (report.mode !== "fixture" && report.mode !== "live") {
    throw new Error(`Adapter ${adapterName} has invalid mode: '${report.mode}'`);
  }
  if (!Array.isArray(report.scenarios)) {
    throw new Error(`Adapter ${adapterName} missing 'scenarios' array`);
  }
  // A zero-scenario report is the stub signature this gate exists to catch.
  if (report.scenarios.length === 0) {
    throw new Error(`Adapter ${adapterName} has zero scenarios — the conformance report appears to be a stub. ` + `A real run spawns 'crewd conformance' and records every canonical scenario.`);
  }
  const passing = report.scenarios.filter((s) => s.outcome === "pass");
  if (passing.length === 0) {
    throw new Error(`Adapter ${adapterName} has no passing scenarios — ${report.scenarios.length} total, all failing.`);
  }

  const declared = assertCapabilities(report.declaredCapabilities, adapterName, "declaredCapabilities");
  const effective = assertCapabilities(report.effectiveCapabilities, adapterName, "effectiveCapabilities");

  assertEffectiveIsProven(declared, effective, report.scenarios, adapterName);
}

/** Asserts a capability object carries all ten fields with the right types. */
function assertCapabilities(value: unknown, adapterName: string, which: string): AdapterCapabilities {
  if (value === null || typeof value !== "object") {
    throw new Error(`Adapter ${adapterName} missing '${which}' object`);
  }
  const caps = value as Record<string, unknown>;
  const stringFields = ["protocol", "resume", "steering", "approvals", "usage", "nested", "nativeView", "workspaceControl", "durability"];
  for (const field of stringFields) {
    if (typeof caps[field] !== "string") {
      throw new Error(`Adapter ${adapterName} '${which}.${field}' must be a string, got ${typeof caps[field]}`);
    }
  }
  if (typeof caps.structuredResult !== "boolean") {
    throw new Error(`Adapter ${adapterName} '${which}.structuredResult' must be a boolean`);
  }
  return caps as unknown as AdapterCapabilities;
}

/**
 * The real assertion: `effectiveCapabilities` must be exactly the subset of
 * `declaredCapabilities` this run proved.
 *
 * 1. No effective value may differ from its declared value except by being
 *    downgraded to that capability's sentinel — an effective capability the
 *    adapter never declared would be a fabricated claim.
 * 2. Every gated capability still holding a non-sentinel value must have its
 *    backing scenario passing or, if never attempted (skipped), must carry
 *    the declared value through. A capability asserted on the strength of a
 *    disproved scenario is exactly what this gate refuses to publish.
 * 3. Every gated capability downgraded to its sentinel must be backed by a
 *    *disproved* scenario, never a skip: a skip is the absence of evidence,
 *    and absence of evidence never removes a declared capability.
 */
function assertEffectiveIsProven(declared: AdapterCapabilities, effective: AdapterCapabilities, scenarios: readonly ScenarioResult[], adapterName: string): void {
  const gatedFields = new Set<string>(CAPABILITY_GATES.map((g) => g.field));

  // (1a) Ungated capabilities are carried through untouched.
  for (const [field, declaredValue] of Object.entries(declared)) {
    if (gatedFields.has(field)) continue;
    const effectiveValue = effective[field as keyof AdapterCapabilities];
    if (effectiveValue !== declaredValue) {
      throw new Error(`Adapter ${adapterName} effectiveCapabilities.${field} is '${effectiveValue}' but declared ` + `'${declaredValue}'; no scenario gates this capability, so it must be carried through unchanged.`);
    }
  }

  for (const gate of CAPABILITY_GATES) {
    const declaredValue = declared[gate.field];
    const effectiveValue = effective[gate.field];
    const scenario = scenarios.find((s) => s.name === gate.scenario);
    if (scenario === undefined) {
      throw new Error(`Adapter ${adapterName} is missing the '${gate.scenario}' scenario, which gates ` + `capability '${gate.field}' — a report cannot claim that capability without it.`);
    }

    // (1b) A gated field may only equal its declared value or the sentinel.
    if (effectiveValue !== declaredValue && effectiveValue !== gate.sentinel) {
      throw new Error(`Adapter ${adapterName} effectiveCapabilities.${gate.field} is '${effectiveValue}', which is ` + `neither the declared '${declaredValue}' nor the '${gate.sentinel}' downgrade.`);
    }

    // (2) A surviving capability must be backed by a passing scenario.
    if (effectiveValue !== gate.sentinel && scenario.outcome === "fail") {
      throw new Error(`Adapter ${adapterName} claims effectiveCapabilities.${gate.field} = '${effectiveValue}' but its ` + `backing scenario '${gate.scenario}' DISPROVED it: ${scenario.detail}`);
    }

    // (3) A capability downgraded to its sentinel must be backed by a
    // genuine disproof. A skip means "never attempted" and must leave the
    // capability declared -- a regression that downgrades on a skip would
    // otherwise pass this gate silently.
    if (effectiveValue === gate.sentinel && declaredValue !== gate.sentinel && scenario.outcome !== "fail") {
      throw new Error(`Adapter ${adapterName} downgraded effectiveCapabilities.${gate.field} from '${declaredValue}' to the '${gate.sentinel}' sentinel, but its ` + `backing scenario '${gate.scenario}' was NOT disproved (outcome: '${scenario.outcome}'): ${scenario.detail}`);
    }
  }
}
