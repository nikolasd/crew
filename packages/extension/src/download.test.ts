import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { downloadRuntime, RuntimeDownloadError } from "./download";
import { BinaryIntegrityError, runtimeCacheDir } from "./platform";

const VERSION = "1.2.3";
const TARGET = "darwin-arm64";
const API_BASE_URL = "https://api.example.test/repos/x/y";
const RELEASE_URL = `${API_BASE_URL}/releases/tags/v${VERSION}`;
const MANIFEST_ASSET_URL = `${API_BASE_URL}/releases/assets/1`;
const BINARY_ASSET_URL = `${API_BASE_URL}/releases/assets/2`;

interface FakeResponse {
  readonly status: number;
  readonly body: BodyInit;
}

/** Routes `fetch` calls by exact URL; an unrouted URL fails the test loudly instead of silently. */
function fakeFetch(routes: Record<string, FakeResponse>): typeof fetch {
  return (async (input: RequestInfo | URL) => {
    const url = String(input);
    const route = routes[url];
    if (route === undefined) {
      throw new Error(`unexpected fetch to ${url}`);
    }
    return new Response(route.body, { status: route.status });
  }) as typeof fetch;
}

/** The release-by-tag API response body, naming each asset's download URL. */
function releaseJson(overrides: { manifestName?: string; binaryName?: string } = {}): string {
  return JSON.stringify({
    assets: [
      { name: overrides.manifestName ?? `crewd-${TARGET}.manifest.json`, url: MANIFEST_ASSET_URL },
      { name: overrides.binaryName ?? `crewd-${TARGET}`, url: BINARY_ASSET_URL },
    ],
  });
}

/** Builds a manifest JSON payload; `sha256` defaults to the real digest of `binaryBytes`. */
function manifestJson(overrides: Partial<{ version: string; target: string; sha256: string; sizeBytes: number }>, binaryBytes: Buffer): string {
  return JSON.stringify({
    name: "crewd",
    version: overrides.version ?? VERSION,
    target: overrides.target ?? TARGET,
    sha256: overrides.sha256 ?? createHash("sha256").update(binaryBytes).digest("hex"),
    sizeBytes: overrides.sizeBytes ?? binaryBytes.length,
  });
}

describe("downloadRuntime: success", () => {
  test("verifies and caches the binary, returning its path/version/target/sizeBytes", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const binaryBytes = Buffer.from("fake-crewd-binary-fixture-bytes");
    const manifest = manifestJson({}, binaryBytes);
    const fetchImpl = fakeFetch({
      [RELEASE_URL]: { status: 200, body: releaseJson() },
      [MANIFEST_ASSET_URL]: { status: 200, body: manifest },
      [BINARY_ASSET_URL]: { status: 200, body: binaryBytes },
    });

    const result = await downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL });

    const dir = runtimeCacheDir(stateRoot, VERSION);
    expect(result).toEqual({ path: join(dir, "crewd"), version: VERSION, target: TARGET, sizeBytes: binaryBytes.length });
    expect(readFileSync(join(dir, "crewd"))).toEqual(binaryBytes);
    expect(JSON.parse(readFileSync(join(dir, "manifest.json"), "utf8"))).toEqual(JSON.parse(manifest));
  });

  test("forwards the token as a Bearer Authorization header on every request", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const binaryBytes = Buffer.from("fake-crewd-binary-fixture-bytes");
    const manifest = manifestJson({}, binaryBytes);
    const seenAuth: (string | undefined)[] = [];
    const fetchImpl = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seenAuth.push((init?.headers as Record<string, string> | undefined)?.Authorization);
      const url = String(input);
      if (url === RELEASE_URL) return new Response(releaseJson(), { status: 200 });
      if (url === MANIFEST_ASSET_URL) return new Response(manifest, { status: 200 });
      if (url === BINARY_ASSET_URL) return new Response(binaryBytes, { status: 200 });
      throw new Error(`unexpected fetch to ${url}`);
    }) as typeof fetch;

    await downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL, token: "test-token" });

    expect(seenAuth).toEqual(["Bearer test-token", "Bearer test-token", "Bearer test-token"]);
  });

  test("omits the Authorization header entirely when no token is given", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const binaryBytes = Buffer.from("fake-crewd-binary-fixture-bytes");
    const manifest = manifestJson({}, binaryBytes);
    const seenAuth: (string | undefined)[] = [];
    const fetchImpl = (async (input: RequestInfo | URL, init?: RequestInit) => {
      seenAuth.push((init?.headers as Record<string, string> | undefined)?.Authorization);
      const url = String(input);
      if (url === RELEASE_URL) return new Response(releaseJson(), { status: 200 });
      if (url === MANIFEST_ASSET_URL) return new Response(manifest, { status: 200 });
      if (url === BINARY_ASSET_URL) return new Response(binaryBytes, { status: 200 });
      throw new Error(`unexpected fetch to ${url}`);
    }) as typeof fetch;

    await downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL });

    expect(seenAuth).toEqual([undefined, undefined, undefined]);
  });
});

describe("downloadRuntime: release/asset lookup", () => {
  test("a non-ok release-by-tag response throws RuntimeDownloadError with code http-error", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const fetchImpl = fakeFetch({ [RELEASE_URL]: { status: 404, body: "not found" } });

    try {
      await downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL });
      throw new Error("expected downloadRuntime to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(RuntimeDownloadError);
      expect((err as RuntimeDownloadError).code).toBe("http-error");
    }
  });

  test("a release missing the requested manifest asset throws RuntimeDownloadError with code http-error, without ever fetching the binary", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const fetchImpl = fakeFetch({
      [RELEASE_URL]: { status: 200, body: releaseJson({ manifestName: "crewd-linux-x64-gnu.manifest.json" }) },
    });

    try {
      await downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL });
      throw new Error("expected downloadRuntime to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(RuntimeDownloadError);
      expect((err as RuntimeDownloadError).code).toBe("http-error");
    }
  });

  test("a non-ok binary asset response throws RuntimeDownloadError with code http-error", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const binaryBytes = Buffer.from("unused");
    const manifest = manifestJson({}, binaryBytes);
    const fetchImpl = fakeFetch({
      [RELEASE_URL]: { status: 200, body: releaseJson() },
      [MANIFEST_ASSET_URL]: { status: 200, body: manifest },
      [BINARY_ASSET_URL]: { status: 500, body: "" },
    });

    try {
      await downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL });
      throw new Error("expected downloadRuntime to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(RuntimeDownloadError);
      expect((err as RuntimeDownloadError).code).toBe("http-error");
    }
  });
});

describe("downloadRuntime: manifest validation", () => {
  test("a manifest declaring a different version throws BinaryIntegrityError before the binary is ever fetched", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const binaryBytes = Buffer.from("unused-if-manifest-rejected-first");
    const manifest = manifestJson({ version: "9.9.9" }, binaryBytes);
    // Deliberately no BINARY_ASSET_URL route: `fakeFetch` throws if the
    // binary is fetched, which would surface as a non-BinaryIntegrityError
    // rejection and fail this assertion.
    const fetchImpl = fakeFetch({
      [RELEASE_URL]: { status: 200, body: releaseJson() },
      [MANIFEST_ASSET_URL]: { status: 200, body: manifest },
    });

    await expect(downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL })).rejects.toBeInstanceOf(BinaryIntegrityError);
  });

  test("a manifest declaring a different target throws BinaryIntegrityError", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const binaryBytes = Buffer.from("unused");
    const manifest = manifestJson({ target: "linux-x64-gnu" }, binaryBytes);
    const fetchImpl = fakeFetch({
      [RELEASE_URL]: { status: 200, body: releaseJson() },
      [MANIFEST_ASSET_URL]: { status: 200, body: manifest },
    });

    await expect(downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL })).rejects.toBeInstanceOf(BinaryIntegrityError);
  });
});

describe("downloadRuntime: checksum verification", () => {
  test("a binary that does not match the manifest's sha256 throws BinaryIntegrityError and leaves no cache files behind", async () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-dl-"));
    const actualBytes = Buffer.from("actually-downloaded-bytes");
    const manifest = manifestJson({ sha256: "0".repeat(64) }, actualBytes);
    const fetchImpl = fakeFetch({
      [RELEASE_URL]: { status: 200, body: releaseJson() },
      [MANIFEST_ASSET_URL]: { status: 200, body: manifest },
      [BINARY_ASSET_URL]: { status: 200, body: actualBytes },
    });

    try {
      await downloadRuntime({ version: VERSION, target: TARGET, stateRoot, fetchImpl, apiBaseUrl: API_BASE_URL });
      throw new Error("expected downloadRuntime to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(BinaryIntegrityError);
      expect((err as BinaryIntegrityError).code).toBe("checksum-mismatch");
    }

    const dir = runtimeCacheDir(stateRoot, VERSION);
    expect(existsSync(join(dir, "crewd"))).toBe(false);
    expect(existsSync(join(dir, "manifest.json"))).toBe(false);
  });
});
