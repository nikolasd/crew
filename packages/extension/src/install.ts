// The shared install action behind both the `crew_runtime_install` tool
// and the `/crew-runtime-install` command: download and verify the
// `crewd` binary for this platform, so subsequent `resolveCrewd` calls
// (`platform.ts`) find a populated cache instead of throwing
// `runtime-not-installed`.

import { homedir } from "node:os";

import { downloadRuntime, RuntimeDownloadError } from "./download";
import { BinaryIntegrityError, detectLibc, resolveTarget, UnsupportedPlatformError } from "./platform";
import { resolveStateRoot } from "./state";

import pkg from "../package.json" with { type: "json" };

/** A text content block, structurally compatible with OMP's `TextContent`. */
export type InstallTextContent = {
  type: "text";
  text: string;
};

/** Context needed to run the install action. */
export interface RuntimeInstallContext {
  /** The extension version to install; also the release tag, as `v${version}`. */
  readonly version: string;
  /** This platform's target triple. */
  readonly target: string;
  /** Absolute Crew state root the binary is cached under. */
  readonly stateRoot: string;
  /** GitHub token for this private repo; forwarded as `Authorization: Bearer <token>`. */
  readonly token?: string;
}

/** Successful result from the install action. */
export interface RuntimeInstallSuccess {
  readonly isError?: false;
  readonly content: [InstallTextContent];
  readonly details: {
    readonly version: string;
    readonly target: string;
    readonly path: string;
    readonly sizeBytes: number;
  };
}

/** Failure result from the install action. */
export interface RuntimeInstallError {
  readonly isError: true;
  readonly content: [InstallTextContent];
  readonly details: {
    readonly code: string;
    readonly message: string;
  };
}

export type RuntimeInstallResult = RuntimeInstallSuccess | RuntimeInstallError;

/**
 * Builds the install context for the current process: resolves the state
 * root and this platform's target triple, and reads this extension's own
 * version (the release tag `runRuntimeInstall` downloads).
 *
 * @throws {UnsupportedPlatformError} if this platform/arch/libc has no
 * supported target triple.
 */
export function buildRuntimeInstallContext(env: Readonly<Record<string, string | undefined>>, home: string = homedir()): RuntimeInstallContext {
  return {
    version: pkg.version,
    target: resolveTarget(process.platform, process.arch, detectLibc()),
    stateRoot: resolveStateRoot(env, home),
    token: resolveGitHubToken(env),
  };
}

/**
 * Downloads and verifies the runtime for `ctx`, shaped as a tool/command
 * result. Never throws: failures are reported as a {@link RuntimeInstallError}
 * instead.
 *
 * Unlike `status.ts`'s deliberately sanitized failure path, these messages
 * may include the asset URL and the local cache path -- both are the user's
 * own paths and are the actionable content for a manual install.
 */
export async function runRuntimeInstall(ctx: RuntimeInstallContext): Promise<RuntimeInstallResult> {
  try {
    const result = await downloadRuntime({ version: ctx.version, target: ctx.target, stateRoot: ctx.stateRoot, token: ctx.token });
    return {
      content: [{ type: "text", text: `Crew runtime installed: crewd ${result.version} (${result.target})\nPath: ${result.path}` }],
      details: { version: result.version, target: result.target, path: result.path, sizeBytes: result.sizeBytes },
    };
  } catch (err) {
    const code = installErrorCode(err);
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      content: [{ type: "text", text: `Runtime install failed: ${message}` }],
      details: { code, message },
    };
  }
}

/**
 * Builds the install context for the current process and runs the install,
 * in one call that never throws. This is the entry point both the
 * `crew_runtime_install` tool and the `/crew-runtime-install` command
 * use -- it exists because {@link buildRuntimeInstallContext} itself can
 * throw {@link UnsupportedPlatformError}, before {@link runRuntimeInstall}'s
 * own try/catch would ever run.
 */
export async function installRuntimeForEnv(env: Readonly<Record<string, string | undefined>>, home?: string): Promise<RuntimeInstallResult> {
  let ctx: RuntimeInstallContext;
  try {
    ctx = buildRuntimeInstallContext(env, home);
  } catch (err) {
    const code = installErrorCode(err);
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      content: [{ type: "text", text: `Runtime install failed: ${message}` }],
      details: { code, message },
    };
  }
  return runRuntimeInstall(ctx);
}

/** Maps a download/integrity/platform error to its machine-readable `code`. */
function installErrorCode(err: unknown): string {
  if (err instanceof RuntimeDownloadError || err instanceof BinaryIntegrityError || err instanceof UnsupportedPlatformError) {
    return err.code;
  }
  return "unknown-error";
}

/**
 * Resolves a GitHub token for downloading release assets from this private
 * repository: `GITHUB_TOKEN`, then `GH_TOKEN`, then a local `gh auth token`
 * session. Returns `undefined` (not thrown) when none is available -- the
 * resulting `http-error` from `downloadRuntime` already names the failed
 * request, and `unknown-error` here would only obscure it.
 */
function resolveGitHubToken(env: Readonly<Record<string, string | undefined>>): string | undefined {
  return env.GITHUB_TOKEN || env.GH_TOKEN || tryGhAuthToken();
}

/** Best-effort: a token from the locally authenticated `gh` CLI, if any. */
function tryGhAuthToken(): string | undefined {
  try {
    const result = Bun.spawnSync(["gh", "auth", "token"]);
    if (result.exitCode !== 0) {
      return undefined;
    }
    const token = result.stdout.toString("utf8").trim();
    return token.length > 0 ? token : undefined;
  } catch {
    return undefined;
  }
}
