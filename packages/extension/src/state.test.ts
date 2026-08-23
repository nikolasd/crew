import { expect, test } from "bun:test";
import { StateRootError, resolveStateRoot } from "./state";

interface StateRootCase {
  name: string;
  env: Record<string, string>;
  home: string;
  existingDirs?: string[];
  expected?: string;
  error?: string;
}

const cases = (await Bun.file("fixtures/state/state-root-cases.json").json()) as StateRootCase[];

test("shared fixture has at least one case", () => {
  expect(cases.length).toBeGreaterThan(0);
});

for (const testCase of cases) {
  test(`state root precedence: ${testCase.name}`, () => {
    const existingDirs = testCase.existingDirs ?? [];
    const exists = (path: string) => existingDirs.includes(path);
    if (testCase.expected !== undefined) {
      expect(resolveStateRoot(testCase.env, testCase.home, exists)).toBe(testCase.expected);
    } else if (testCase.error !== undefined) {
      expect(() => resolveStateRoot(testCase.env, testCase.home, exists)).toThrow(StateRootError);
    } else {
      throw new Error(`case ${testCase.name} must set exactly one of expected/error`);
    }
  });
}

test("rejects a relative CREW_STATE_DIR override", () => {
  expect(() => resolveStateRoot({ CREW_STATE_DIR: "relative/state" }, "/home/alice")).toThrow(StateRootError);
});

test("rejects a relative legacy BATMAN_STATE_DIR override", () => {
  expect(() => resolveStateRoot({ BATMAN_STATE_DIR: "relative/state" }, "/home/alice")).toThrow(StateRootError);
});

test("rejects a relative XDG_STATE_HOME override", () => {
  expect(() => resolveStateRoot({ XDG_STATE_HOME: "relative/state" }, "/home/alice")).toThrow(StateRootError);
});

test("CREW_STATE_DIR wins over XDG_STATE_HOME and the default", () => {
  const root = resolveStateRoot(
    {
      CREW_STATE_DIR: "/var/lib/crew",
      XDG_STATE_HOME: "/home/alice/.local/state",
    },
    "/home/alice",
  );
  expect(root).toBe("/var/lib/crew");
});

test("the legacy BATMAN_STATE_DIR name still works when CREW_STATE_DIR is unset", () => {
  expect(resolveStateRoot({ BATMAN_STATE_DIR: "/var/lib/batman" }, "/home/alice")).toBe("/var/lib/batman");
});

test("CREW_STATE_DIR wins over the legacy BATMAN_STATE_DIR when both are set", () => {
  const root = resolveStateRoot({ CREW_STATE_DIR: "/var/lib/crew", BATMAN_STATE_DIR: "/var/lib/batman" }, "/home/alice");
  expect(root).toBe("/var/lib/crew");
});

test("falls back to $HOME/.omp/crew when nothing is set and nothing exists", () => {
  expect(resolveStateRoot({}, "/home/alice", () => false)).toBe("/home/alice/.omp/crew");
});

test("falls back to the legacy $HOME/.omp/batman when only it exists", () => {
  const exists = (path: string) => path === "/home/alice/.omp/batman";
  expect(resolveStateRoot({}, "/home/alice", exists)).toBe("/home/alice/.omp/batman");
});

test("PI_CONFIG_DIR overrides the default .omp directory name", () => {
  expect(resolveStateRoot({ PI_CONFIG_DIR: ".config-omp" }, "/home/alice", () => false)).toBe("/home/alice/.config-omp/crew");
});

test("does not read process-global env or home", () => {
  const originalStateDir = process.env.CREW_STATE_DIR;
  process.env.CREW_STATE_DIR = "/should/not/be/read";
  try {
    expect(resolveStateRoot({}, "/home/alice", () => false)).toBe("/home/alice/.omp/crew");
  } finally {
    if (originalStateDir === undefined) {
      delete process.env.CREW_STATE_DIR;
    } else {
      process.env.CREW_STATE_DIR = originalStateDir;
    }
  }
});
