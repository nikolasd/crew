// Downloads and verifies the `crewd` binary GitHub Release asset for a
// given version/target, caching it under the Crew state root at the exact
// path `resolveCrewd` (`platform.ts`) later reads from.
//
// Modelled on `doctor.ts`'s split: this module is the pure worker; `install.ts`
// builds its context and shapes the tool/command result.
//
// nikolasd/batman is a PRIVATE repository, so a plain `releases/download/
// <tag>/<asset>` URL always 404s -- that browser-facing URL is never
// token-authenticated. Assets are instead fetched through the GitHub REST
// API: a release-by-tag lookup returns each asset's download `url`, then
// each asset is fetched from that url with an `Accept:
// application/octet-stream` header. A `token` is forwarded as `Authorization:
// Bearer <token>` on every request; omitting it only works if the repository
// is ever made public.

import { chmodSync, mkdirSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { sha256File } from "./integrity";
import { BinaryIntegrityError, parseManifest, runtimeCacheDir } from "./platform";

/** The GitHub REST API base for this repository's releases. */
const API_BASE_URL = "https://api.github.com/repos/nikolasd/batman";

/** Machine-readable reason a download failed for a non-integrity reason. */
export type RuntimeDownloadErrorCode = "http-error" | "write-failed";

/**
 * Thrown when a release/asset cannot be fetched or located, or the verified
 * binary cannot be written to the cache. Integrity failures (checksum/version/
 * target mismatch, malformed manifest) throw {@link BinaryIntegrityError}
 * instead -- see `platform.ts`.
 */
export class RuntimeDownloadError extends Error {
  readonly code: RuntimeDownloadErrorCode;

  constructor(code: RuntimeDownloadErrorCode, message: string) {
    super(message);
    this.name = "RuntimeDownloadError";
    this.code = code;
  }
}

/** Inputs to {@link downloadRuntime}. */
export interface DownloadRuntimeOptions {
  /** The extension version to download; also the release tag, as `v${version}`. */
  readonly version: string;
  /** The target triple (e.g. `darwin-arm64`) whose asset to fetch. */
  readonly target: string;
  /** Absolute Crew state root the binary is cached under. */
  readonly stateRoot: string;
  /** Forwarded as `Authorization: Bearer <token>` on every request; required while the repository stays private. */
  readonly token?: string;
  /** Injected in tests; defaults to global `fetch`. */
  readonly fetchImpl?: typeof fetch;
  /** Injected in tests; defaults to {@link API_BASE_URL}. */
  readonly apiBaseUrl?: string;
}

/** The result of a successful {@link downloadRuntime} call. */
export interface DownloadRuntimeResult {
  /** Absolute path to the verified, cached `crewd` binary. */
  readonly path: string;
  readonly version: string;
  readonly target: string;
  readonly sizeBytes: number;
}

/** One asset entry from a GitHub release-by-tag API response. */
interface ReleaseAsset {
  readonly name: string;
  readonly url: string;
}

/**
 * Downloads `crewd` for `options.target`/`options.version` from the
 * matching GitHub Release, verifies its SHA-256 against the published
 * manifest, and caches both under `runtimeCacheDir(stateRoot, version)`.
 *
 * Ordering invariant: the manifest is validated before any binary bytes are
 * fetched, the binary is verified before it is renamed into place, and the
 * manifest is written to the cache only after the binary has landed --
 * `resolveCrewd` relies on a present manifest always implying an
 * already-verified sibling binary.
 *
 * Compatibility note: this looks for `crewd-<target>` assets, which exist
 * starting with the first release tag published after the batman -> crew
 * rename; any release before that tag shipped `batcave-<target>` assets
 * instead and cannot be installed by this function.
 */
export async function downloadRuntime(options: DownloadRuntimeOptions): Promise<DownloadRuntimeResult> {
  const fetchImpl = options.fetchImpl ?? fetch;
  const apiBaseUrl = options.apiBaseUrl ?? API_BASE_URL;
  const tag = `v${options.version}`;
  const binaryName = `crewd-${options.target}`;
  const manifestName = `${binaryName}.manifest.json`;

  const releaseUrl = `${apiBaseUrl}/releases/tags/${tag}`;
  const assets = await fetchReleaseAssets(fetchImpl, releaseUrl, options.token);
  const manifestAsset = findAsset(assets, manifestName, releaseUrl);
  const binaryAsset = findAsset(assets, binaryName, releaseUrl);

  const manifestRaw = await fetchAssetText(fetchImpl, manifestAsset.url, options.token);
  const manifest = parseManifest(manifestRaw, manifestAsset.url);

  if (manifest.version !== options.version) {
    throw new BinaryIntegrityError("version-mismatch", `manifest at ${manifestAsset.url} declares version ${manifest.version}, but ${options.version} was requested`);
  }
  if (manifest.target !== options.target) {
    throw new BinaryIntegrityError("manifest-invalid", `manifest at ${manifestAsset.url} declares target ${manifest.target}, but ${options.target} was requested`);
  }

  const binaryBytes = await fetchAssetBytes(fetchImpl, binaryAsset.url, options.token);

  const dir = runtimeCacheDir(options.stateRoot, options.version);
  const finalPath = join(dir, "crewd");
  const manifestPath = join(dir, "manifest.json");
  const tmpPath = join(dir, `.crewd.${process.pid}.tmp`);

  try {
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    writeFileSync(tmpPath, binaryBytes);
    chmodSync(tmpPath, 0o755);
  } catch (err) {
    throw new RuntimeDownloadError("write-failed", `failed to write ${tmpPath}: ${(err as Error).message}`);
  }

  const actualSha256 = sha256File(tmpPath);
  if (actualSha256 !== manifest.sha256) {
    try {
      unlinkSync(tmpPath);
    } catch {
      // Best-effort cleanup; the mismatch is the error that matters.
    }
    throw new BinaryIntegrityError("checksum-mismatch", `checksum mismatch for ${binaryAsset.url}: manifest ${manifestAsset.url} declares ${manifest.sha256}, computed ${actualSha256}`);
  }

  try {
    renameSync(tmpPath, finalPath);
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  } catch (err) {
    throw new RuntimeDownloadError("write-failed", `failed to finalize ${finalPath}: ${(err as Error).message}`);
  }

  return { path: finalPath, version: manifest.version, target: manifest.target, sizeBytes: manifest.sizeBytes };
}

/** Headers for a GitHub API request; adds `Authorization` only when a token is given. */
function githubHeaders(token: string | undefined, accept: string): Record<string, string> {
  return token === undefined ? { Accept: accept } : { Accept: accept, Authorization: `Bearer ${token}` };
}

/** Fetches the release-by-tag API response and returns its `assets` array. */
async function fetchReleaseAssets(fetchImpl: typeof fetch, releaseUrl: string, token: string | undefined): Promise<ReleaseAsset[]> {
  const response = await fetchImpl(releaseUrl, { headers: githubHeaders(token, "application/vnd.github+json") });
  if (!response.ok) {
    throw new RuntimeDownloadError("http-error", `failed to fetch release ${releaseUrl}: HTTP ${response.status}`);
  }
  const raw: unknown = await response.json();
  const assets = (raw as { assets?: unknown }).assets;
  if (!Array.isArray(assets)) {
    throw new RuntimeDownloadError("http-error", `release ${releaseUrl} response has no assets array`);
  }
  return assets.map((asset) => ({
    name: String((asset as { name?: unknown }).name ?? ""),
    url: String((asset as { url?: unknown }).url ?? ""),
  }));
}

/** Finds a release asset by exact name, or throws a descriptive `RuntimeDownloadError`. */
function findAsset(assets: readonly ReleaseAsset[], name: string, releaseUrl: string): ReleaseAsset {
  const asset = assets.find((candidate) => candidate.name === name);
  if (asset === undefined) {
    throw new RuntimeDownloadError("http-error", `release ${releaseUrl} has no asset named ${name}`);
  }
  return asset;
}

/** Fetches one release asset's raw bytes, mapping a non-`ok` response to `RuntimeDownloadError`. */
async function fetchAsset(fetchImpl: typeof fetch, assetUrl: string, token: string | undefined): Promise<Response> {
  const response = await fetchImpl(assetUrl, { headers: githubHeaders(token, "application/octet-stream") });
  if (!response.ok) {
    throw new RuntimeDownloadError("http-error", `failed to download ${assetUrl}: HTTP ${response.status}`);
  }
  return response;
}

async function fetchAssetText(fetchImpl: typeof fetch, assetUrl: string, token: string | undefined): Promise<string> {
  return (await fetchAsset(fetchImpl, assetUrl, token)).text();
}

async function fetchAssetBytes(fetchImpl: typeof fetch, assetUrl: string, token: string | undefined): Promise<Buffer> {
  return Buffer.from(await (await fetchAsset(fetchImpl, assetUrl, token)).arrayBuffer());
}
