import { describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { sha256File } from "./integrity";
import { BinaryIntegrityError, resolveCrewd, resolveTarget, runtimeCacheDir, UnsupportedPlatformError } from "./platform";
import { BinarySelectionError } from "./runtime";

import pkg from "../package.json" with { type: "json" };

const EXTENSION_VERSION: string = pkg.version;

interface CacheFixtureOptions {
  readonly binaryBytes?: Buffer;
  readonly sha256?: string;
  readonly version?: string;
  readonly target?: string;
}

/**
 * Materializes `<stateRoot>/bin/<EXTENSION_VERSION>/{crewd,manifest.json}`
 * in a fresh temp state root, so integrity tests never depend on a real
 * committed binary. The cache directory is always keyed by
 * `EXTENSION_VERSION` -- exactly as `resolveCrewd` computes it -- so a
 * mismatched `version` option only corrupts the manifest's own `version`
 * field, which is what `resolveCrewd` actually validates.
 */
function makeCache(options: CacheFixtureOptions = {}): string {
  const stateRoot = mkdtempSync(join(tmpdir(), "bat-state-"));
  const dir = runtimeCacheDir(stateRoot, EXTENSION_VERSION);
  mkdirSync(dir, { recursive: true });
  const binaryBytes = options.binaryBytes ?? Buffer.from("fake-crewd-binary-fixture-bytes");
  const binPath = join(dir, "crewd");
  writeFileSync(binPath, binaryBytes);
  chmodSync(binPath, 0o755);

  const manifest = {
    name: "crewd",
    version: options.version ?? EXTENSION_VERSION,
    target: options.target ?? "darwin-arm64",
    sha256: options.sha256 ?? sha256File(binPath),
    sizeBytes: binaryBytes.length,
  };
  writeFileSync(join(dir, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  return stateRoot;
}

describe("resolveTarget: tuple mapping", () => {
  test("darwin/arm64 maps to darwin-arm64", () => {
    expect(resolveTarget("darwin", "arm64", undefined)).toBe("darwin-arm64");
  });

  test("darwin/x64 maps to darwin-x64", () => {
    expect(resolveTarget("darwin", "x64", undefined)).toBe("darwin-x64");
  });

  test("linux/arm64/glibc maps to linux-arm64-gnu", () => {
    expect(resolveTarget("linux", "arm64", "glibc")).toBe("linux-arm64-gnu");
  });

  test("linux/x64/glibc maps to linux-x64-gnu", () => {
    expect(resolveTarget("linux", "x64", "glibc")).toBe("linux-x64-gnu");
  });
});

describe("resolveTarget: unsupported platforms", () => {
  test("win32/x64 throws UnsupportedPlatformError with the exact platform/arch/libc", () => {
    try {
      resolveTarget("win32", "x64", undefined);
      throw new Error("expected resolveTarget to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(UnsupportedPlatformError);
      const unsupported = err as UnsupportedPlatformError;
      expect(unsupported.platform).toBe("win32");
      expect(unsupported.arch).toBe("x64");
      expect(unsupported.libc).toBeUndefined();
    }
  });

  test("win32/arm64 throws UnsupportedPlatformError with the exact platform/arch/libc", () => {
    try {
      resolveTarget("win32", "arm64", undefined);
      throw new Error("expected resolveTarget to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(UnsupportedPlatformError);
      const unsupported = err as UnsupportedPlatformError;
      expect(unsupported.platform).toBe("win32");
      expect(unsupported.arch).toBe("arm64");
      expect(unsupported.libc).toBeUndefined();
    }
  });

  test("linux/x64/musl throws UnsupportedPlatformError with the exact platform/arch/libc", () => {
    try {
      resolveTarget("linux", "x64", "musl");
      throw new Error("expected resolveTarget to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(UnsupportedPlatformError);
      const unsupported = err as UnsupportedPlatformError;
      expect(unsupported.platform).toBe("linux");
      expect(unsupported.arch).toBe("x64");
      expect(unsupported.libc).toBe("musl");
    }
  });

  test("linux/arm64/musl throws UnsupportedPlatformError with the exact platform/arch/libc", () => {
    try {
      resolveTarget("linux", "arm64", "musl");
      throw new Error("expected resolveTarget to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(UnsupportedPlatformError);
      const unsupported = err as UnsupportedPlatformError;
      expect(unsupported.platform).toBe("linux");
      expect(unsupported.arch).toBe("arm64");
      expect(unsupported.libc).toBe("musl");
    }
  });

  test("resolveCrewd surfaces the same UnsupportedPlatformError before ever touching the cache", () => {
    expect(() => resolveCrewd("win32", "x64", undefined, {}, "/does/not/matter")).toThrow(UnsupportedPlatformError);
  });
});

describe("resolveCrewd: cache resolution", () => {
  test("resolves to source package when the cache is populated and checksum/version/target match", () => {
    const stateRoot = makeCache();
    const result = resolveCrewd("darwin", "arm64", undefined, {}, stateRoot);
    expect(result).toEqual({ path: join(runtimeCacheDir(stateRoot, EXTENSION_VERSION), "crewd"), source: "package" });
  });

  test("an absent cache directory throws BinarySelectionError with code runtime-not-installed", () => {
    const stateRoot = mkdtempSync(join(tmpdir(), "bat-state-empty-"));

    try {
      resolveCrewd("darwin", "arm64", undefined, {}, stateRoot);
      throw new Error("expected resolveCrewd to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(BinarySelectionError);
      expect((err as BinarySelectionError).code).toBe("runtime-not-installed");
    }
  });
});

describe("resolveCrewd: integrity", () => {
  test("flipping one byte of the binary causes BinaryIntegrityError before spawn", () => {
    const stateRoot = makeCache();
    const binPath = join(runtimeCacheDir(stateRoot, EXTENSION_VERSION), "crewd");
    const bytes = readFileSync(binPath);
    bytes[0] = (bytes[0]! ^ 0xff) & 0xff;
    writeFileSync(binPath, bytes);

    try {
      resolveCrewd("darwin", "arm64", undefined, {}, stateRoot);
      throw new Error("expected resolveCrewd to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(BinaryIntegrityError);
      expect((err as BinaryIntegrityError).code).toBe("checksum-mismatch");
    }
  });

  test("a manifest version that does not match the extension version fails", () => {
    const stateRoot = makeCache({ version: "0.0.1-does-not-match" });

    try {
      resolveCrewd("darwin", "arm64", undefined, {}, stateRoot);
      throw new Error("expected resolveCrewd to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(BinaryIntegrityError);
      expect((err as BinaryIntegrityError).code).toBe("version-mismatch");
    }
  });

  test("a manifest target for another platform fails before the version check", () => {
    const stateRoot = makeCache({ target: "linux-x64-gnu" });

    try {
      resolveCrewd("darwin", "arm64", undefined, {}, stateRoot);
      throw new Error("expected resolveCrewd to throw");
    } catch (err) {
      expect(err).toBeInstanceOf(BinaryIntegrityError);
      expect((err as BinaryIntegrityError).code).toBe("manifest-invalid");
    }
  });
});

describe("resolveCrewd: override precedence", () => {
  test("a valid absolute executable override wins before cache resolution, source override, no checksum performed", () => {
    // A deliberately corrupt manifest (wrong sha256) that would fail
    // integrity validation if the cache were ever consulted.
    const stateRoot = makeCache({ sha256: "0".repeat(64) });

    const overrideDir = mkdtempSync(join(tmpdir(), "bat-override-"));
    const overridePath = join(overrideDir, "crewd");
    writeFileSync(overridePath, "#!/bin/sh\nexit 0\n");
    chmodSync(overridePath, 0o755);

    // Does not throw despite the corrupt cache manifest: the override
    // check happens first and returns before the cache is ever read.
    const result = resolveCrewd("darwin", "arm64", undefined, { OMP_CREW_BINARY: overridePath }, stateRoot);

    expect(result).toEqual({ path: overridePath, source: "override" });
  });

  test("the legacy OMP_BATMAN_BINARY name still works when OMP_CREW_BINARY is unset", () => {
    const stateRoot = makeCache({ sha256: "0".repeat(64) });

    const overrideDir = mkdtempSync(join(tmpdir(), "bat-override-"));
    const overridePath = join(overrideDir, "crewd");
    writeFileSync(overridePath, "#!/bin/sh\nexit 0\n");
    chmodSync(overridePath, 0o755);

    const result = resolveCrewd("darwin", "arm64", undefined, { OMP_BATMAN_BINARY: overridePath }, stateRoot);

    expect(result).toEqual({ path: overridePath, source: "override" });
  });
});
