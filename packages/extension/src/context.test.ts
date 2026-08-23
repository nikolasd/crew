import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { buildStatusContext } from "./context";
import { CrewConfigError } from "./crew-config";

function tempHomeAndRepo(): { home: string; repository: string } {
  return {
    home: mkdtempSync("/tmp/bat-ctx-home-"),
    repository: mkdtempSync("/tmp/bat-ctx-repo-"),
  };
}

function writeConfig(root: string, contents: string): string {
  const dir = join(root, ".omp");
  mkdirSync(dir, { recursive: true });
  const path = join(dir, "crew.json");
  writeFileSync(path, contents);
  return path;
}

test("buildStatusContext threads no --config layers when neither file exists", () => {
  const { home, repository } = tempHomeAndRepo();
  const { ensureRuntimeOptions } = buildStatusContext({ cwd: repository, env: {}, home });
  expect(ensureRuntimeOptions.configPaths).toEqual([]);
});

test("buildStatusContext threads existing layer files, user before project", () => {
  const { home, repository } = tempHomeAndRepo();
  const userPath = writeConfig(home, "{}");
  const projectPath = writeConfig(repository, "{}");
  const { ensureRuntimeOptions } = buildStatusContext({ cwd: repository, env: {}, home });
  expect(ensureRuntimeOptions.configPaths).toEqual([userPath, projectPath]);
});

test("buildStatusContext throws CrewConfigError for a malformed layer file", () => {
  const { home, repository } = tempHomeAndRepo();
  writeConfig(home, "{ not json");
  expect(() => buildStatusContext({ cwd: repository, env: {}, home })).toThrow(CrewConfigError);
});
