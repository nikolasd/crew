---
name: crew-troubleshooting
description: >-
  Use when any Crew tool returns an error, "crew isn't working", "crew won't start",
  or the runtime fails to connect. Fires on tool errors, connection failures, missing runtime,
  or any Crew-related diagnostic request.
---

## How Crew tools work

Two facts are invisible from the tool schemas but essential for correct use:

- **Crew stores no task text of its own.** The `prompt` argument must be supplied on every `run/submit` **and every `run/retry`**. Retry does not remember the prior prompt — you must pass it again.
- **Every Crew tool returns the daemon's JSON result verbatim under `details`.** Read ids (`taskId`, `workerId`, `runId`, `leaseId`, etc.) from there. Never invent or guess them.

## Diagnostic ladder

Follow this sequence to diagnose Crew problems:

1. **`/crew-status`** — connects to or spawns the daemon. If it fails, proceed to step 2.
2. **`/crew-runtime-install`** — downloads and verifies the crewd binary if it's missing. This is the fix for `runtime-not-installed`.
3. **`/crew-doctor`** — works even with no live daemon. Provides a detailed health check of the environment.

## Error codes and fixes

Every Crew tool error has this shape: text `"<method> failed: <message>"`, `details: { code, message, data }`, `isError: true`. A JSON-RPC error uses code `-32602` for invalid arguments.

| `details.code` | Fix |
|---|---|
| `runtime-not-installed` | Run `/crew-runtime-install` to download the binary. |
| `checksum-mismatch` | Re-run `/crew-runtime-install`. The cached binary is corrupted or from a different release. |
| `version-mismatch` | Re-run `/crew-runtime-install`. The cached binary is for a different extension version. |
| `manifest-invalid` | Re-run `/crew-runtime-install`. The cached manifest is corrupt or for another platform. |
| `unsupported-platform` | Crew only supports macOS and glibc Linux, arm64/x64. Other platforms are not supported. |
| `connection-failed` | Run `/crew-doctor` for a detailed check without needing a live daemon. |
| `http-error` (from `/crew-runtime-install`) | **This repository is private.** The download needs read access via a `GITHUB_TOKEN` or `GH_TOKEN` environment variable, or a local `gh auth login` session. Set one of those and retry the install. |
