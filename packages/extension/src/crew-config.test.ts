import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { CrewConfigError, resolveCrewConfigPaths } from "./crew-config";

function tempHomeAndRepo(): { home: string; repository: string } {
  return {
    home: mkdtempSync("/tmp/bat-cc-home-"),
    repository: mkdtempSync("/tmp/bat-cc-repo-"),
  };
}

function writeConfig(root: string, contents: string): string {
  const dir = join(root, ".omp");
  mkdirSync(dir, { recursive: true });
  const path = join(dir, "crew.json");
  writeFileSync(path, contents);
  return path;
}

test("neither layer file existing resolves to an empty array", () => {
  const { home, repository } = tempHomeAndRepo();
  expect(resolveCrewConfigPaths(home, repository)).toEqual([]);
});

test("only the user layer existing resolves to just that path", () => {
  const { home, repository } = tempHomeAndRepo();
  const userPath = writeConfig(home, "{}");
  expect(resolveCrewConfigPaths(home, repository)).toEqual([userPath]);
});

test("only the project layer existing resolves to just that path", () => {
  const { home, repository } = tempHomeAndRepo();
  const projectPath = writeConfig(repository, "{}");
  expect(resolveCrewConfigPaths(home, repository)).toEqual([projectPath]);
});

test("both layers existing resolve user-first, then project (project wins the later merge)", () => {
  const { home, repository } = tempHomeAndRepo();
  const userPath = writeConfig(home, '{"approval":"never"}');
  const projectPath = writeConfig(repository, '{"approval":"auto"}');
  expect(resolveCrewConfigPaths(home, repository)).toEqual([userPath, projectPath]);
});

test("an existing but malformed user layer file throws CrewConfigError naming that file", () => {
  const { home, repository } = tempHomeAndRepo();
  const userPath = writeConfig(home, "{ not valid json");
  let thrown: unknown;
  try {
    resolveCrewConfigPaths(home, repository);
  } catch (err) {
    thrown = err;
  }
  expect(thrown).toBeInstanceOf(CrewConfigError);
  expect((thrown as CrewConfigError).code).toBe("invalid-json");
  expect((thrown as CrewConfigError).path).toBe(userPath);
  expect((thrown as CrewConfigError).message).toContain(userPath);
});

test("an existing but malformed project layer file throws CrewConfigError naming that file", () => {
  const { home, repository } = tempHomeAndRepo();
  writeConfig(home, "{}");
  const projectPath = writeConfig(repository, "[unterminated");
  let thrown: unknown;
  try {
    resolveCrewConfigPaths(home, repository);
  } catch (err) {
    thrown = err;
  }
  expect(thrown).toBeInstanceOf(CrewConfigError);
  expect((thrown as CrewConfigError).path).toBe(projectPath);
});
