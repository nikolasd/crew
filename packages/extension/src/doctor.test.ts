import { describe, expect, test } from "bun:test";

import { formatDoctorOutput, type DoctorResult } from "./doctor";

function result(overrides: Partial<DoctorResult> = {}): DoctorResult {
  return {
    healthy: true,
    passed_checks: ["database_connectivity"],
    failed_checks: [],
    unresolved_gates: [],
    ...overrides,
  };
}

describe("formatDoctorOutput", () => {
  test("renders notes so config observations reach the operator", () => {
    const text = formatDoctorOutput(
      result({
        notes: [{ check_name: "config_present", detail: "no crew.json layer found; running on built-in defaults." }],
      }),
    );

    expect(text).toContain("config_present");
    expect(text).toContain("running on built-in defaults");
  });

  test("keeps a healthy verdict when the only output is a note", () => {
    const text = formatDoctorOutput(
      result({
        notes: [{ check_name: "config_drift", detail: "1 key(s) differ from the current built-in defaults" }],
      }),
    );

    expect(text).toContain("Doctor check: healthy");
  });

  test("omits the notes section entirely when there are none", () => {
    expect(formatDoctorOutput(result())).not.toContain("Notes");
  });

  // A daemon predating the notes field returns no `notes` key at all; the
  // formatter must not throw reading it.
  test("tolerates a result from a daemon that has no notes field", () => {
    const legacy = result();
    delete (legacy as { notes?: unknown }).notes;

    expect(() => formatDoctorOutput(legacy)).not.toThrow();
  });
});
