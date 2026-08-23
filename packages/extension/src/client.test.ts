import { afterAll, beforeAll, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readdirSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { join } from "node:path";

import type { InitializeParams, InitializeResult, RuntimeStatus } from "@nikolasd/batman-protocol";
import { CrewClient, JsonRpcRemoteError, ValidationError } from "./client";

const REPO_ROOT = join(import.meta.dir, "..", "..", "..");
const CREWD = join(REPO_ROOT, "target", "debug", "crewd");

let serverProc: Bun.Subprocess | undefined;
let stateDir: string;
let repoDir: string;
let socketPath: string;

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/** Locates the runtime socket the daemon creates under `<state>/repos/<id>/`. */
function findSocket(state: string): string | undefined {
  const reposDir = join(state, "repos");
  if (!existsSync(reposDir)) {
    return undefined;
  }
  for (const entry of readdirSync(reposDir)) {
    const candidate = join(reposDir, entry, "runtime.sock");
    if (existsSync(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

async function waitForSocket(state: string): Promise<string> {
  for (let attempt = 0; attempt < 200; attempt++) {
    const socket = findSocket(state);
    if (socket !== undefined) {
      return socket;
    }
    await sleep(25);
  }
  throw new Error("runtime socket did not appear");
}

function ompInitParams(agentDir: string, maxFrameBytes: number): InitializeParams {
  return {
    client: { name: "@nikolasd/crew", version: "0.1.0" },
    supported: { min: { major: 1, minor: 0 }, max: { major: 1, minor: 0 } },
    repository: { canonicalPath: agentDir, vcsRoot: agentDir },
    auth: { role: "ompExtension", instanceId: "omp-1", agentDirectory: agentDir },
    capabilities: { eventReplay: true, maxFrameBytes },
    lastSequence: null,
  } as InitializeParams;
}

beforeAll(async () => {
  // Ensure the binary the TypeScript client talks to exists.
  const build = Bun.spawnSync(["cargo", "build", "-p", "batman-runtime"], { cwd: REPO_ROOT });
  if (build.exitCode !== 0) {
    throw new Error(`cargo build failed: ${build.stderr.toString()}`);
  }

  // Short base dir: Unix socket paths are length-bounded (macOS SUN_LEN).
  stateDir = mkdtempSync("/tmp/bat-ts-s-");
  repoDir = mkdtempSync("/tmp/bat-ts-r-");
  mkdirSync(join(repoDir, ".git"));

  serverProc = Bun.spawn([CREWD, "serve", "--foreground", "--state-dir", stateDir, "--repo", repoDir], { stdout: "pipe", stderr: "pipe" });

  socketPath = await waitForSocket(stateDir);
}, 180_000);

afterAll(() => {
  serverProc?.kill("SIGTERM");
});

test("initialize negotiates protocol 1.0, project id, and the minimum frame size", async () => {
  const client = new CrewClient({ socketPath });
  try {
    const result: InitializeResult = await client.initialize(ompInitParams(repoDir, 1024 * 1024));
    expect(result.negotiated).toEqual({ major: 1, minor: 0 });
    expect(typeof result.projectId).toBe("string");
    expect(result.projectId.length).toBeGreaterThan(0);
    // Runtime max is 4 MiB, client offered 1 MiB -> negotiated is 1 MiB.
    expect(result.capabilities.maxFrameBytes).toBe(1024 * 1024);
    expect(result.principal.role).toBe("ompExtension");
  } finally {
    client.close();
  }
});

test("runtime/status reports a healthy, running runtime", async () => {
  const client = new CrewClient({ socketPath });
  try {
    await client.initialize(ompInitParams(repoDir, 1024 * 1024));
    const status = (await client.request("runtime/status")) as RuntimeStatus;
    expect(status.running).toBe(true);
    expect(status.protocol).toEqual({ major: 1, minor: 0 });
    expect(status.activeRuns).toBe(0);
    expect(status.protocolHealthy).toBe(true);
  } finally {
    client.close();
  }
});

test("subscribe replays the durable RuntimeStarted event", async () => {
  const client = new CrewClient({ socketPath });
  try {
    await client.initialize(ompInitParams(repoDir, 1024 * 1024));
    const firstEvent = await new Promise<{ sequence: number; event: { type: string } }>((resolve) => {
      client.subscribe(0, (event) => resolve(event as unknown as { sequence: number; event: { type: string } }));
    });
    expect(firstEvent.sequence).toBe(1);
    expect(firstEvent.event.type).toBe("runtimeStarted");
  } finally {
    client.close();
  }
});

test("a frame offer below the protocol minimum is rejected with INVALID_PARAMS", async () => {
  const client = new CrewClient({ socketPath });
  try {
    await expect(client.initialize(ompInitParams(repoDir, 1024))).rejects.toMatchObject({
      code: -32602,
    });
  } finally {
    client.close();
  }
});

test("an inbound message with an unknown field is rejected before reaching caller logic", async () => {
  // A hand-rolled server that answers `initialize` with an InitializeResult
  // carrying an extra, unknown field. The client must reject it via schema
  // validation rather than hand it back.
  const fakeSocketPath = mkdtempSync("/tmp/bat-ts-f-") + "/fake.sock";
  const fakeServer: Server = createServer((socket: Socket) => {
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => {
      buffer += chunk;
      const newline = buffer.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(buffer.slice(0, newline)) as { id: unknown };
      const result = {
        runtime: { name: "fake", version: "0.0.0" },
        negotiated: { major: 1, minor: 0 },
        projectId: "018f1435-2e2b-7c1a-9d4b-6a1e2f3c4d5b",
        principal: { role: "ompExtension", instanceId: "omp-1" },
        allowedMethods: ["runtime/status"],
        capabilities: { maxFrameBytes: 1048576, peerCredentialsVerified: true },
        nextSequence: 1,
        unexpectedField: "should be rejected",
      };
      socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result })}\n`);
    });
  });

  await new Promise<void>((resolve) => fakeServer.listen(fakeSocketPath, resolve));

  const client = new CrewClient({ socketPath: fakeSocketPath });
  try {
    await expect(client.initialize(ompInitParams(repoDir, 1024 * 1024))).rejects.toBeInstanceOf(ValidationError);
  } finally {
    client.close();
    await new Promise<void>((resolve) => fakeServer.close(() => resolve()));
  }
});

test("an inbound frame exceeding the negotiated cap is rejected before dispatch and tears down the connection", async () => {
  // A hand-rolled server that negotiates a small `maxFrameBytes`, then sends
  // one complete, newline-terminated frame that exceeds it. The client must
  // reject on size alone -- never parsing/validating/dispatching the frame --
  // and must tear down the connection.
  const fakeSocketPath = mkdtempSync("/tmp/bat-ts-f-") + "/fake.sock";
  const NEGOTIATED_MAX_FRAME_BYTES = 100;
  let serverSocket: Socket | undefined;
  let requestCount = 0;

  const fakeServer: Server = createServer((socket: Socket) => {
    serverSocket = socket;
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => {
      buffer += chunk;
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        const request = JSON.parse(line) as { id: unknown };
        requestCount += 1;

        if (requestCount === 1) {
          const result = {
            runtime: { name: "fake", version: "0.0.0" },
            negotiated: { major: 1, minor: 0 },
            projectId: "018f1435-2e2b-7c1a-9d4b-6a1e2f3c4d5b",
            principal: { role: "ompExtension", instanceId: "omp-1" },
            allowedMethods: ["runtime/status"],
            capabilities: {
              maxFrameBytes: NEGOTIATED_MAX_FRAME_BYTES,
              peerCredentialsVerified: true,
            },
            nextSequence: 1,
          };
          socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result })}\n`);
        } else {
          // A single complete frame, well over the negotiated 100-byte cap.
          const oversized = JSON.stringify({
            jsonrpc: "2.0",
            id: request.id,
            result: { padding: "x".repeat(NEGOTIATED_MAX_FRAME_BYTES * 4) },
          });
          socket.write(`${oversized}\n`);
        }
        newline = buffer.indexOf("\n");
      }
    });
  });

  await new Promise<void>((resolve) => fakeServer.listen(fakeSocketPath, resolve));

  const client = new CrewClient({ socketPath: fakeSocketPath });
  try {
    await client.initialize(ompInitParams(repoDir, 1024 * 1024));

    const serverSocketClosed = new Promise<void>((resolve) => {
      serverSocket?.once("close", () => resolve());
    });

    await expect(client.request("runtime/status")).rejects.toThrow(/exceeds the negotiated maximum/);

    // The connection must be torn down, not just the one request rejected.
    await expect(client.request("runtime/status")).rejects.toThrow();
    await serverSocketClosed;
  } finally {
    client.close();
    await new Promise<void>((resolve) => fakeServer.close(() => resolve()));
  }
});

test("a malformed artifact/fetch result (missing contentBase64) is rejected by schema validation (R55)", async () => {
  const fakeSocketPath = mkdtempSync("/tmp/bat-ts-f-") + "/fake.sock";
  let requestCount = 0;

  const fakeServer: Server = createServer((socket: Socket) => {
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => {
      buffer += chunk;
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        const request = JSON.parse(line) as { id: unknown };
        requestCount += 1;

        if (requestCount === 1) {
          const result = {
            runtime: { name: "fake", version: "0.0.0" },
            negotiated: { major: 1, minor: 0 },
            projectId: "018f1435-2e2b-7c1a-9d4b-6a1e2f3c4d5b",
            principal: { role: "ompExtension", instanceId: "omp-1" },
            allowedMethods: ["artifact/fetch"],
            capabilities: { maxFrameBytes: 1048576, peerCredentialsVerified: true },
            nextSequence: 1,
          };
          socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result })}\n`);
        } else {
          // An artifact/fetch result missing `contentBase64` entirely.
          const result = {
            artifact: {
              artifactId: "018f1435-2e2b-7c1a-9d4b-6a1e2f3c4d5c",
              kind: "patch",
              sha256: "0".repeat(64),
              byteLength: 3,
              mediaType: "text/x-patch",
              storagePath: "sha256/00/000",
              runId: null,
            },
            nextOffset: null,
            complete: true,
          };
          socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result })}\n`);
        }
        newline = buffer.indexOf("\n");
      }
    });
  });

  await new Promise<void>((resolve) => fakeServer.listen(fakeSocketPath, resolve));

  const client = new CrewClient({ socketPath: fakeSocketPath });
  try {
    await client.initialize(ompInitParams(repoDir, 1024 * 1024));
    await expect(client.request("artifact/fetch", { artifactId: "018f1435-2e2b-7c1a-9d4b-6a1e2f3c4d5c" })).rejects.toBeInstanceOf(ValidationError);
  } finally {
    client.close();
    await new Promise<void>((resolve) => fakeServer.close(() => resolve()));
  }
});

test("a malformed run/result result (runId not a string) is rejected by schema validation (R55)", async () => {
  const fakeSocketPath = mkdtempSync("/tmp/bat-ts-f-") + "/fake.sock";
  let requestCount = 0;

  const fakeServer: Server = createServer((socket: Socket) => {
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => {
      buffer += chunk;
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        const request = JSON.parse(line) as { id: unknown };
        requestCount += 1;

        if (requestCount === 1) {
          const result = {
            runtime: { name: "fake", version: "0.0.0" },
            negotiated: { major: 1, minor: 0 },
            projectId: "018f1435-2e2b-7c1a-9d4b-6a1e2f3c4d5b",
            principal: { role: "ompExtension", instanceId: "omp-1" },
            allowedMethods: ["run/result"],
            capabilities: { maxFrameBytes: 1048576, peerCredentialsVerified: true },
            nextSequence: 1,
          };
          socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result })}\n`);
        } else {
          // A run/result result with `runId` typed as a number instead of a string.
          const result = { runId: 1 };
          socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result })}\n`);
        }
        newline = buffer.indexOf("\n");
      }
    });
  });

  await new Promise<void>((resolve) => fakeServer.listen(fakeSocketPath, resolve));

  const client = new CrewClient({ socketPath: fakeSocketPath });
  try {
    await client.initialize(ompInitParams(repoDir, 1024 * 1024));
    await expect(client.request("run/result", { runId: "r-1" })).rejects.toBeInstanceOf(ValidationError);
  } finally {
    client.close();
    await new Promise<void>((resolve) => fakeServer.close(() => resolve()));
  }
});

test("a null result for a validator-less method is rejected by the structural object guard (R55)", async () => {
  const fakeSocketPath = mkdtempSync("/tmp/bat-ts-f-") + "/fake.sock";
  let requestCount = 0;

  const fakeServer: Server = createServer((socket: Socket) => {
    let buffer = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => {
      buffer += chunk;
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        const request = JSON.parse(line) as { id: unknown };
        requestCount += 1;

        if (requestCount === 1) {
          const result = {
            runtime: { name: "fake", version: "0.0.0" },
            negotiated: { major: 1, minor: 0 },
            projectId: "018f1435-2e2b-7c1a-9d4b-6a1e2f3c4d5b",
            principal: { role: "ompExtension", instanceId: "omp-1" },
            allowedMethods: ["task/upsert"],
            capabilities: { maxFrameBytes: 1048576, peerCredentialsVerified: true },
            nextSequence: 1,
          };
          socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result })}\n`);
        } else {
          socket.write(`${JSON.stringify({ jsonrpc: "2.0", id: request.id, result: null })}\n`);
        }
        newline = buffer.indexOf("\n");
      }
    });
  });

  await new Promise<void>((resolve) => fakeServer.listen(fakeSocketPath, resolve));

  const client = new CrewClient({ socketPath: fakeSocketPath });
  try {
    await client.initialize(ompInitParams(repoDir, 1024 * 1024));
    await expect(client.request("task/upsert", { taskId: "t" })).rejects.toBeInstanceOf(ValidationError);
  } finally {
    client.close();
    await new Promise<void>((resolve) => fakeServer.close(() => resolve()));
  }
});

test("JsonRpcRemoteError carries the JSON-RPC error code", () => {
  const err = new JsonRpcRemoteError(-32602, "bad", undefined);
  expect(err.code).toBe(-32602);
  expect(err).toBeInstanceOf(Error);
});
