import { afterEach, beforeAll, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, realpathSync, symlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import type { CrewClient } from "./client";
import { BinarySelectionError, buildServeArgs, ensureRuntime, type EnsureRuntimeOptions, repositoryId, repositoryIdFromRoot } from "./runtime";

interface RepoIdCase {
  name: string;
  canonicalRoot: string;
  repositoryId: string;
}

const repoIdCases = (await Bun.file("fixtures/repo-id/repo-id-cases.json").json()) as RepoIdCase[];

test("shared repo-id fixture has at least one case", () => {
  expect(repoIdCases.length).toBeGreaterThan(0);
});

for (const testCase of repoIdCases) {
  test(`repository id matches shared cross-language fixture: ${testCase.name}`, () => {
    expect(repositoryIdFromRoot(testCase.canonicalRoot)).toBe(testCase.repositoryId);
  });
}

test("a .git FILE (worktree) is a VCS marker and a nested dir resolves to the same repo", () => {
  const worktree = mkdtempSync("/tmp/bat-rt-wt-");
  writeFileSync(join(worktree, ".git"), "gitdir: /elsewhere/.git/worktrees/example\n");
  const nested = join(worktree, "src");
  mkdirSync(nested);

  const canonicalRoot = realpathSync(worktree);
  const expected = repositoryIdFromRoot(canonicalRoot);
  // The worktree root and a subdirectory both discover the same .git FILE.
  expect(repositoryId(worktree)).toBe(expected);
  expect(repositoryId(nested)).toBe(expected);
});

test("a broken .git symlink still counts as a VCS marker (matches Rust lstat semantics)", () => {
  const repo = mkdtempSync("/tmp/bat-rt-sym-");
  // A dangling symlink: its target does not exist, so existsSync would miss it
  // but lstat (and Rust's symlink_metadata) treat the link node as present.
  symlinkSync("/does/not/exist", join(repo, ".git"));

  const canonicalRoot = realpathSync(repo);
  expect(repositoryId(repo)).toBe(repositoryIdFromRoot(canonicalRoot));
});

const REPO_ROOT = join(import.meta.dir, "..", "..", "..");
const CREWD = join(REPO_ROOT, "target", "debug", "crewd");

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/** Clients/daemons started by each test, torn down in afterEach. */
const openClients: CrewClient[] = [];
const stateDirs: string[] = [];

function newRepo(): string {
  const repo = mkdtempSync("/tmp/bat-rt-r-");
  mkdirSync(join(repo, ".git"));
  return repo;
}

function newStateDir(): string {
  const state = mkdtempSync("/tmp/bat-rt-s-");
  stateDirs.push(state);
  return state;
}

function baseOptions(stateDir: string, repository: string): EnsureRuntimeOptions {
  return {
    stateDir,
    repository,
    idleSeconds: 30,
    env: { OMP_CREW_BINARY: CREWD },
  };
}

/** Sends SIGTERM to every daemon under a state dir via its lock-file pid. */
function stopDaemons(stateDir: string): void {
  const repos = join(stateDir, "repos");
  if (!existsSync(repos)) {
    return;
  }
  for (const entry of readdirSync(repos)) {
    const lock = join(repos, entry, "runtime.lock");
    if (!existsSync(lock)) {
      continue;
    }
    try {
      const { pid } = JSON.parse(readFileSync(lock, "utf8")) as { pid: number };
      process.kill(pid, "SIGTERM");
    } catch {
      // Already gone.
    }
  }
}

beforeAll(() => {
  const build = Bun.spawnSync(["cargo", "build", "-p", "batman-runtime"], { cwd: REPO_ROOT });
  if (build.exitCode !== 0) {
    throw new Error(`cargo build failed: ${build.stderr.toString()}`);
  }
}, 180_000);

afterEach(async () => {
  for (const client of openClients.splice(0)) {
    client.close();
  }
  for (const stateDir of stateDirs.splice(0)) {
    stopDaemons(stateDir);
  }
  // Give SIGTERM'd daemons a moment to release their sockets.
  await sleep(200);
});

test("buildServeArgs returns the exact detached serve argument vector", () => {
  const args = buildServeArgs({
    stateDir: "/s",
    repository: "/r",
    idleSeconds: 42,
    env: {},
  });
  expect(args).toEqual(["serve", "--state-dir", "/s", "--repo", "/r", "--idle-seconds", "42"]);
  // Detached launches never pass --foreground.
  expect(args).not.toContain("--foreground");
});

test("ensureRuntime spawns a detached daemon that logs runtime_started to runtime.log", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();

  const { client, childStarted } = await ensureRuntime(baseOptions(stateDir, repository));
  openClients.push(client);

  expect(childStarted).toBe(true);

  const status = (await client.request("runtime/status")) as { running: boolean };
  expect(status.running).toBe(true);

  // The detached daemon owns runtime.log; a structured runtime_started record
  // must be present there (not on any inherited stdio, which is discarded).
  const repos = join(stateDir, "repos");
  const repoId = readdirSync(repos)[0]!;
  const logPath = join(repos, repoId, "runtime.log");
  expect(existsSync(logPath)).toBe(true);
  const log = readFileSync(logPath, "utf8");
  expect(log).toContain("runtime_started");
  // Structured: each line is a JSON object.
  const firstLine = log.split("\n").find((l) => l.length > 0)!;
  expect(() => JSON.parse(firstLine)).not.toThrow();
});

test("foreground startup writes the runtime_started record to stderr instead", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();

  const proc = Bun.spawn([CREWD, "serve", "--foreground", "--state-dir", stateDir, "--repo", repository, "--idle-seconds", "30"], { stdout: "ignore", stderr: "pipe" });

  try {
    // Read stderr until the runtime_started record shows up.
    const decoder = new TextDecoder();
    let stderr = "";
    const reader = proc.stderr.getReader();
    const deadline = Date.now() + 5000;
    while (Date.now() < deadline && !stderr.includes("runtime_started")) {
      const { value, done } = await reader.read();
      if (done) {
        break;
      }
      stderr += decoder.decode(value, { stream: true });
    }
    reader.releaseLock();

    expect(stderr).toContain("runtime_started");

    // The detached log must NOT exist for a foreground launch.
    const repos = join(stateDir, "repos");
    const repoId = readdirSync(repos)[0]!;
    const logPath = join(repos, repoId, "runtime.log");
    if (existsSync(logPath)) {
      expect(readFileSync(logPath, "utf8")).not.toContain("runtime_started");
    }
  } finally {
    proc.kill("SIGTERM");
    await proc.exited;
  }
});

test("a missing OMP_CREW_BINARY override fails before spawn", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  await expect(
    ensureRuntime({
      stateDir,
      repository,
      idleSeconds: 30,
      env: { OMP_CREW_BINARY: "/nonexistent/crewd-does-not-exist" },
    }),
  ).rejects.toBeInstanceOf(BinarySelectionError);
});

test("a relative OMP_CREW_BINARY override fails before spawn", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  await expect(
    ensureRuntime({
      stateDir,
      repository,
      idleSeconds: 30,
      env: { OMP_CREW_BINARY: "relative/crewd" },
    }),
  ).rejects.toMatchObject({ code: "not-absolute" });
});

test("a non-regular (directory) OMP_CREW_BINARY override fails before spawn", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  const dir = mkdtempSync("/tmp/bat-rt-dir-");
  await expect(
    ensureRuntime({
      stateDir,
      repository,
      idleSeconds: 30,
      env: { OMP_CREW_BINARY: dir },
    }),
  ).rejects.toMatchObject({ code: "not-regular" });
});

test("a non-executable OMP_CREW_BINARY override fails before spawn", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  const file = join(mkdtempSync("/tmp/bat-rt-ne-"), "crewd");
  writeFileSync(file, "#!/bin/sh\n");
  chmodSync(file, 0o644);
  await expect(
    ensureRuntime({
      stateDir,
      repository,
      idleSeconds: 30,
      env: { OMP_CREW_BINARY: file },
    }),
  ).rejects.toMatchObject({ code: "not-executable" });
});

test("an async spawn failure is logged by the error listener and surfaces as an unreachable runtime, not a crash (R18)", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  // Passes every selectBinary check (absolute, regular, executable) but
  // fails at exec time: the shebang interpreter does not exist, so spawn
  // emits an async `error` event instead of throwing.
  const file = join(mkdtempSync("/tmp/bat-rt-sh-"), "crewd");
  writeFileSync(file, "#!/nonexistent/interpreter\n");
  chmodSync(file, 0o755);

  const logged: string[] = [];
  const originalError = console.error;
  console.error = (...args: unknown[]) => {
    logged.push(args.map(String).join(" "));
  };
  try {
    await expect(
      ensureRuntime({
        stateDir,
        repository,
        idleSeconds: 30,
        env: { OMP_CREW_BINARY: file },
      }),
    ).rejects.toThrow();
    expect(logged.some((line) => line.includes("failed to spawn"))).toBe(true);
  } finally {
    console.error = originalError;
  }
}, 20_000);

test("a valid override is selected verbatim, bypassing the package resolver", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();
  let resolverCalled = false;

  const { client, childStarted } = await ensureRuntime({
    stateDir,
    repository,
    idleSeconds: 30,
    env: { OMP_CREW_BINARY: CREWD },
    packagedBinaryResolver: () => {
      resolverCalled = true;
      return "/should/not/be/used";
    },
  });
  openClients.push(client);

  expect(childStarted).toBe(true);
  expect(resolverCalled).toBe(false);
  const status = (await client.request("runtime/status")) as {
    running: boolean;
    binarySource: string;
  };
  expect(status.running).toBe(true);
  expect(status.binarySource).toBe("override");
});

test("a second ensureRuntime caller connects to the same runtime", async () => {
  const stateDir = newStateDir();
  const repository = newRepo();

  const first = await ensureRuntime(baseOptions(stateDir, repository));
  openClients.push(first.client);
  expect(first.childStarted).toBe(true);

  const second = await ensureRuntime(baseOptions(stateDir, repository));
  openClients.push(second.client);
  // The runtime already exists, so the second caller connects without
  // spawning its own child.
  expect(second.childStarted).toBe(false);

  const status = (await second.client.request("runtime/status")) as { running: boolean };
  expect(status.running).toBe(true);

  // Exactly one repo directory (one runtime) exists.
  const repos = join(stateDir, "repos");
  expect(readdirSync(repos).length).toBe(1);
});
