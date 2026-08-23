// Selects the `crewd` binary this extension runs: a validated
// `OMP_CREW_BINARY` development override (or its pre-rename name,
// `OMP_BATMAN_BINARY` -- see `runtime.ts`), or the
// platform-specific binary downloaded from a GitHub Release and cached
// locally, verified by SHA-256 checksum and extension-version match before
// it is ever spawned.
//
// `resolveCrewd` takes `platform`/`arch`/`libc`/`env`/`stateRoot` explicitly
// (rather than reading `process.platform`/`process.arch`/`process.env`
// itself) so it stays pure and hermetically testable; production wiring
// lives in `context.ts` and `doctor.ts`.

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { BinarySelectionError, resolveOverride, type SelectedBinary } from "./runtime";
import { sha256File } from "./integrity";

import pkg from "../package.json" with { type: "json" };

/** This extension's own version; a cached binary's manifest must match it exactly. */
const EXTENSION_VERSION: string = pkg.version;

/** The four target triples the foundation ships prebuilt `crewd` binaries for. */
export type SupportedTarget = "darwin-arm64" | "darwin-x64" | "linux-arm64-gnu" | "linux-x64-gnu";

/** Every {@link SupportedTarget}, in a stable order, for error messages and iteration. */
const SUPPORTED_TARGETS: readonly SupportedTarget[] = ["darwin-arm64", "darwin-x64", "linux-arm64-gnu", "linux-x64-gnu"];

/**
 * Thrown when `platform`/`arch`/`libc` do not map to a supported target
 * triple (Windows in any form, or Linux with a non-glibc libc such as musl).
 */
export class UnsupportedPlatformError extends Error {
  /** Machine-readable reason, mirrored by `status.ts`'s failure mapping. */
  readonly code = "unsupported-platform";
  readonly platform: string;
  readonly arch: string;
  readonly libc: string | undefined;

  constructor(platform: string, arch: string, libc: string | undefined) {
    super(`unsupported platform: platform=${platform} arch=${arch} libc=${libc ?? "unknown"} ` + `(supported: ${SUPPORTED_TARGETS.join(", ")})`);
    this.name = "UnsupportedPlatformError";
    this.platform = platform;
    this.arch = arch;
    this.libc = libc;
  }
}

/** Machine-readable reason a cached binary failed integrity validation. */
export type BinaryIntegrityErrorCode = "manifest-invalid" | "checksum-mismatch" | "version-mismatch";

/**
 * Thrown before any spawn when a cached `crewd` binary's manifest is
 * missing/malformed, its SHA-256 does not match the manifest, its manifest's
 * `target` does not match this platform, or its manifest's `version` does
 * not match this extension's version. Never thrown for an
 * `OMP_CREW_BINARY` override -- override binaries are not checksummed.
 */
export class BinaryIntegrityError extends Error {
  readonly code: BinaryIntegrityErrorCode;

  constructor(code: BinaryIntegrityErrorCode, message: string) {
    super(message);
    this.name = "BinaryIntegrityError";
    this.code = code;
  }
}

/**
 * The deterministic checksum/provenance payload written alongside every
 * cached `crewd` binary -- both the one `/crew-runtime-install` downloads
 * (`download.ts`) and the one a manual `cp` into the cache directory
 * provides for local testing.
 */
export interface LeafManifest {
  readonly name: string;
  readonly version: string;
  readonly target: string;
  readonly sha256: string;
  readonly sizeBytes: number;
}

/**
 * Maps a platform/arch/libc tuple to its release target triple, or throws
 * {@link UnsupportedPlatformError}.
 */
export function resolveTarget(platform: string, arch: string, libc: string | undefined): SupportedTarget {
  const target = mapTarget(platform, arch, libc);
  if (target === undefined) {
    throw new UnsupportedPlatformError(platform, arch, libc);
  }
  return target;
}

/**
 * The version-scoped directory a downloaded `crewd` and its manifest live
 * in. Shared with `download.ts` so the two can never disagree on the path.
 */
export function runtimeCacheDir(stateRoot: string, version: string): string {
  return join(stateRoot, "bin", version);
}

/**
 * Resolves the `crewd` binary to run.
 *
 * Order:
 * 1. A valid absolute executable `OMP_CREW_BINARY` (or legacy `OMP_BATMAN_BINARY`) in `env` wins outright
 *    -- source `"override"`. No checksum or version validation is performed
 *    for an override; validation applies only to the cached binary.
 * 2. Otherwise, `platform`/`arch`/`libc` are mapped to one of the four
 *    supported target triples (or a typed {@link UnsupportedPlatformError}).
 *    The cached binary at `<stateRoot>/bin/<version>/crewd` is
 *    SHA-256-verified against its sibling `manifest.json`, and the
 *    manifest's `target` and `version` must match, before returning --
 *    source `"package"`.
 */
export function resolveCrewd(platform: string, arch: string, libc: string | undefined, env: Readonly<Record<string, string | undefined>>, stateRoot: string): SelectedBinary {
  const override = resolveOverride(env);
  if (override !== undefined) {
    return override;
  }

  const target = resolveTarget(platform, arch, libc);
  const dir = runtimeCacheDir(stateRoot, EXTENSION_VERSION);
  const binPath = join(dir, "crewd");
  const manifestPath = join(dir, "manifest.json");

  if (!existsSync(binPath) || !existsSync(manifestPath)) {
    throw new BinarySelectionError("runtime-not-installed", `no crewd binary installed for version ${EXTENSION_VERSION}; run /crew-runtime-install to download it, or set OMP_CREW_BINARY to a local build`);
  }

  const manifest = readManifest(manifestPath);

  if (manifest.target !== target) {
    throw new BinaryIntegrityError("manifest-invalid", `manifest at ${manifestPath} declares target ${manifest.target}, but this platform requires ${target}`);
  }

  const actualSha256 = sha256File(binPath);
  if (actualSha256 !== manifest.sha256) {
    throw new BinaryIntegrityError("checksum-mismatch", `checksum mismatch for ${binPath}: manifest ${manifestPath} declares ${manifest.sha256}, ` + `computed ${actualSha256}`);
  }

  if (manifest.version !== EXTENSION_VERSION) {
    throw new BinaryIntegrityError("version-mismatch", `cached binary is version ${manifest.version}, but this extension is version ${EXTENSION_VERSION}`);
  }

  return { path: binPath, source: "package" };
}

/**
 * Parses and validates a manifest JSON payload, whether read from disk
 * ({@link readManifest}) or fetched over the network (`download.ts`).
 * `sourceLabel` is embedded in error messages -- a file path for the former,
 * the asset URL for the latter.
 */
export function parseManifest(raw: string, sourceLabel: string): LeafManifest {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (err) {
    throw new BinaryIntegrityError("manifest-invalid", `manifest at ${sourceLabel} is not valid JSON: ${(err as Error).message}`);
  }

  if (typeof parsed !== "object" || parsed === null || typeof (parsed as Partial<LeafManifest>).sha256 !== "string" || typeof (parsed as Partial<LeafManifest>).version !== "string" || typeof (parsed as Partial<LeafManifest>).target !== "string" || typeof (parsed as Partial<LeafManifest>).sizeBytes !== "number") {
    throw new BinaryIntegrityError("manifest-invalid", `manifest at ${sourceLabel} is missing required fields "sha256"/"version"/"target"/"sizeBytes"`);
  }

  return parsed as LeafManifest;
}

/** Reads and parses a cached binary's `manifest.json`. */
function readManifest(manifestPath: string): LeafManifest {
  let raw: string;
  try {
    raw = readFileSync(manifestPath, "utf8");
  } catch (err) {
    throw new BinaryIntegrityError("manifest-invalid", `unable to read manifest at ${manifestPath}: ${(err as Error).message}`);
  }
  return parseManifest(raw, manifestPath);
}

/** Internal platform→target mapping, returns undefined for unsupported platforms. */
function mapTarget(platform: string, arch: string, libc: string | undefined): SupportedTarget | undefined {
  if (platform === "darwin" && arch === "arm64") {
    return "darwin-arm64";
  }
  if (platform === "darwin" && arch === "x64") {
    return "darwin-x64";
  }
  if (platform === "linux" && arch === "arm64" && libc === "glibc") {
    return "linux-arm64-gnu";
  }
  if (platform === "linux" && arch === "x64" && libc === "glibc") {
    return "linux-x64-gnu";
  }
  return undefined;
}

/**
 * Best-effort Linux libc detection: `"glibc"`, `"musl"`, or `undefined` when
 * undetermined (which `resolveCrewd` treats as unsupported). Not meaningful
 * off Linux. Foundation-scope heuristic: checks Node's build report for a
 * glibc runtime version, then falls back to checking for musl's well-known
 * dynamic loader paths.
 */
export function detectLibc(platform: string = process.platform): string | undefined {
  if (platform !== "linux") {
    return undefined;
  }

  try {
    const report = (process.report?.getReport() as { header?: { glibcVersionRuntime?: string } })?.header;
    if (report?.glibcVersionRuntime) {
      return "glibc";
    }
  } catch {
    // Fall through to musl detection below.
  }

  const muslLoaders = ["/lib/ld-musl-x86_64.so.1", "/lib/ld-musl-aarch64.so.1"];
  if (muslLoaders.some((loader) => existsSync(loader))) {
    return "musl";
  }

  return undefined;
}
