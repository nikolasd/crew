import { describe, expect, test } from "bun:test";

import { buildConfigArgs } from "./config";

describe("buildConfigArgs", () => {
  test("init targets the repository layer by default", () => {
    expect(buildConfigArgs({ op: "init", repository: "/r" })).toEqual(["config", "init", "--repo", "/r"]);
  });

  test("init --global omits --repo so the daemon resolves ~/.omp itself", () => {
    const args = buildConfigArgs({ op: "init", repository: "/r", global: true });

    expect(args).toEqual(["config", "init", "--global"]);
    expect(args).not.toContain("--repo");
  });

  test("init passes force through only when asked", () => {
    expect(buildConfigArgs({ op: "init", repository: "/r", force: true })).toEqual(["config", "init", "--repo", "/r", "--force"]);
    expect(buildConfigArgs({ op: "init", repository: "/r" })).not.toContain("--force");
  });

  test("print defaults to the effective document", () => {
    expect(buildConfigArgs({ op: "print", repository: "/r" })).toEqual(["config", "print", "--effective", "--repo", "/r"]);
  });

  test("print selects exactly one document flag", () => {
    expect(buildConfigArgs({ op: "print", repository: "/r", document: "defaults" })).toEqual(["config", "print", "--defaults", "--repo", "/r"]);
    expect(buildConfigArgs({ op: "print", repository: "/r", document: "schema" })).toEqual(["config", "print", "--schema", "--repo", "/r"]);
  });

  test("path reports the layers for the given repository", () => {
    expect(buildConfigArgs({ op: "path", repository: "/r" })).toEqual(["config", "path", "--repo", "/r"]);
  });
});
