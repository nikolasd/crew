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

1. **`/crew health`** — connects to or spawns the daemon. If it fails, proceed to step 2.
2. **`/crew-install`** — downloads and verifies the crewd binary if it's missing. This is the fix for `runtime-not-installed`.
3. **`/crew doctor`** — works even with no live daemon. Provides a detailed health check of the environment.


## Live-control failures

- Start with `/crew`, not a poll loop: `runs`, `run <runId>`, and `crew_transcript` expose the durable replay.
- `BUDGET_EXCEEDED` means the subtask's snapshotted turn budget is exhausted. Do not resend; change the approved plan budget or stop/finish the run.
- `WorkerTimeout` is not a daemon kill. Choose `run/timeoutAck` `extend`, `crew_send` a nudge, or `run/timeoutAck` `abort`.
- `pane/reopen` only works for a live run with its attach socket still bound. A terminal run or absent socket is an honest refusal; use the transcript instead.
- `/crew clean` prunes only terminal/unassociated event history under the configured retention period and `maxRuns`; it never deletes a live run or task row.
## Error codes and fixes

Every Crew tool error has this shape: text `"<method> failed: <message>"`, `details: { code, message, data }`, `isError: true`. A JSON-RPC error uses code `-32602` for invalid arguments.

| `details.code` | Fix |
|---|---|
| `runtime-not-installed` | Run `/crew-install` to download the binary. |
| `checksum-mismatch` | Re-run `/crew-install`. The cached binary is corrupted or from a different release. |
| `version-mismatch` | Re-run `/crew-install`. The cached binary is for a different extension version. |
| `manifest-invalid` | Re-run `/crew-install`. The cached manifest is corrupt or for another platform. |
| `unsupported-platform` | Crew only supports macOS and glibc Linux, arm64/x64. Other platforms are not supported. |
| `connection-failed` | Run `/crew doctor` for a detailed check without needing a live daemon. |
| `http-error` (from `/crew-install`) | **This repository is private.** The download needs read access via a `GITHUB_TOKEN` or `GH_TOKEN` environment variable, or a local `gh auth login` session. Set one of those and retry the install. |
