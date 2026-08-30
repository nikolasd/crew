import { expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { CrewConfigError, persistConfiguredModel, resolveConfiguredModel, resolveCrewConfigPaths } from "./crew-config";

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

// ------------------------------------------------------- resolveConfiguredModel

test("resolveConfiguredModel returns undefined when neither layer configures the adapter", () => {
  const { home, repository } = tempHomeAndRepo();
  expect(resolveConfiguredModel(home, repository, "claude")).toBeUndefined();
});

test("resolveConfiguredModel returns the user layer's model when only it sets one", () => {
  const { home, repository } = tempHomeAndRepo();
  writeConfig(home, '{"adapters":{"claude":{"model":"opus"}}}');
  expect(resolveConfiguredModel(home, repository, "claude")).toBe("opus");
});

test("resolveConfiguredModel prefers the repo layer over the user layer (later layer wins)", () => {
  const { home, repository } = tempHomeAndRepo();
  writeConfig(home, '{"adapters":{"claude":{"model":"opus"}}}');
  writeConfig(repository, '{"adapters":{"claude":{"model":"sonnet"}}}');
  expect(resolveConfiguredModel(home, repository, "claude")).toBe("sonnet");
});

test("resolveConfiguredModel ignores a null model (crew.default.json's own unset marker)", () => {
  const { home, repository } = tempHomeAndRepo();
  writeConfig(repository, '{"adapters":{"claude":{"model":null}}}');
  expect(resolveConfiguredModel(home, repository, "claude")).toBeUndefined();
});

// `crates/runtime/src/config/crew.rs`'s `RESERVED_ADAPTER_CONFIG_KEYS` keys
// the fourth reserved adapter's config section "omp" (the vendor binary
// name), distinct from `AdapterKind::RESERVED_NAMES`'s wire name "ompRpc"
// -- two independent, both-correct conventions, not a mismatch to paper
// over by renaming the config file.
test("resolveConfiguredModel reads ompRpc's model from the config's own 'omp' section key", () => {
  const { home, repository } = tempHomeAndRepo();
  writeConfig(repository, '{"adapters":{"omp":{"model":"qwen"}}}');
  expect(resolveConfiguredModel(home, repository, "ompRpc")).toBe("qwen");
});

test("persistConfiguredModel writes ompRpc's model under the config's own 'omp' section key", () => {
  const { repository } = tempHomeAndRepo();
  persistConfiguredModel(repository, "ompRpc", "qwen");
  const path = join(repository, ".omp", "crew.json");
  const written = JSON.parse(readFileSync(path, "utf8"));
  expect(written.adapters.omp.model).toBe("qwen");
  expect(written.adapters.ompRpc).toBeUndefined();
});

test("resolveConfiguredModel is scoped to the requested adapter", () => {
  const { home, repository } = tempHomeAndRepo();
  writeConfig(repository, '{"adapters":{"codex":{"model":"gpt-5"}}}');
  expect(resolveConfiguredModel(home, repository, "claude")).toBeUndefined();
});

test("resolveConfiguredModel throws CrewConfigError on a malformed layer, same as resolveCrewConfigPaths", () => {
  const { home, repository } = tempHomeAndRepo();
  writeConfig(repository, "{ not valid json");
  expect(() => resolveConfiguredModel(home, repository, "claude")).toThrow(CrewConfigError);
});

// ------------------------------------------------------- persistConfiguredModel

test("persistConfiguredModel creates the repo layer file when none exists", () => {
  const { repository } = tempHomeAndRepo();
  persistConfiguredModel(repository, "claude", "opus");
  const path = join(repository, ".omp", "crew.json");
  const written = JSON.parse(readFileSync(path, "utf8"));
  expect(written.adapters.claude.model).toBe("opus");
});

test("persistConfiguredModel preserves every other existing key", () => {
  const { repository } = tempHomeAndRepo();
  writeConfig(repository, '{"approval":"auto","adapters":{"codex":{"model":"gpt-5","profile":"reviews"}}}');
  persistConfiguredModel(repository, "claude", "opus");
  const path = join(repository, ".omp", "crew.json");
  const written = JSON.parse(readFileSync(path, "utf8"));
  expect(written.approval).toBe("auto");
  expect(written.adapters.codex).toEqual({ model: "gpt-5", profile: "reviews" });
  expect(written.adapters.claude.model).toBe("opus");
});

test("persistConfiguredModel refuses to clobber an already-configured model for that adapter", () => {
  const { repository } = tempHomeAndRepo();
  writeConfig(repository, '{"adapters":{"claude":{"model":"opus"}}}');
  persistConfiguredModel(repository, "claude", "sonnet");
  const path = join(repository, ".omp", "crew.json");
  const written = JSON.parse(readFileSync(path, "utf8"));
  expect(written.adapters.claude.model).toBe("opus");
});

test("persistConfiguredModel throws rather than silently discarding a malformed existing file", () => {
  const { repository } = tempHomeAndRepo();
  const path = writeConfig(repository, "{ not valid json");
  expect(() => persistConfiguredModel(repository, "claude", "opus")).toThrow();
  expect(readFileSync(path, "utf8")).toBe("{ not valid json");
});

test("persistConfiguredModel leaves no temp file behind and writes into the same directory as the target", () => {
  const { repository } = tempHomeAndRepo();
  persistConfiguredModel(repository, "claude", "opus");
  const dir = join(repository, ".omp");
  const entries = readdirSync(dir);
  expect(entries).toEqual(["crew.json"]);
});

test("persistConfiguredModel never touches the user's global layer", () => {
  const { home, repository } = tempHomeAndRepo();
  persistConfiguredModel(repository, "claude", "opus");
  expect(existsSync(join(home, ".omp", "crew.json"))).toBe(false);
});
