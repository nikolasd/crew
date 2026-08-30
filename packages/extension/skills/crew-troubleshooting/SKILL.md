---
name: crew-troubleshooting
description: >-
  Use when any Crew tool returns an error, "crew isn't working", "crew won't start",
  or the runtime fails to connect. Fires on tool errors, connection failures, missing runtime,
  or any Crew-related diagnostic request.
---

## How Crew tools work

Essential facts invisible from the tool schemas:

- **Profile-first worker provisioning:** Reserved adapters (claude, codex, copilot, ompRpc) require profiles. Use `crew_profile { adapter, model, startupOptions: { <adapter>: { mode: "tui" } } }` (startup options are tagged by adapter kind, e.g. `{ claude: { mode: "tui" } }`) first, then `crew_worker { profileId }`, then `crew_run`. See the crew-orchestration skill for the full flow.
- **Crew stores no task text of its own.** The `prompt` argument must be supplied on every `run/submit` **and every `run/retry`**. Retry does not remember the prior prompt — you must pass it again.
- **Every Crew tool returns the daemon's JSON result verbatim under `details`.** Read ids (`taskId`, `workerId`, `runId`, `leaseId`, etc.) from there. Never invent or guess them.

## Diagnostic ladder

Follow this sequence to diagnose Crew problems:

1. **`/crew health`** — connects to or spawns the daemon. Also reports the embedded dashboard's live URL (token included) when it's enabled — see "Read surfaces" below. If it fails, proceed to step 2.
2. **`/crew-install`** — downloads and verifies the crewd binary if it's missing. This is the fix for `runtime-not-installed`.
3. **`/crew doctor`** — works even with no live daemon. Provides a detailed health check of the environment, including a stale-workspace-lease check (see the crew-recovery skill).

## Live-control failures

- Start with `/crew`, not a poll loop: `runs`, `run <runId>`, and `crew_transcript` expose the durable replay.
- `BUDGET_EXCEEDED` means the subtask's snapshotted turn budget is exhausted. Do not resend; change the approved plan budget or stop/finish the run.
- `WorkerTimeout` is not a daemon kill. Choose `run/timeoutAck` `extend`, `crew_send` a nudge, or `run/timeoutAck` `abort`. An `extend` refusal (`-32602`, no tracked timeout) can mean the run settled in the meantime — expected and benign, not a fault; see crew-orchestration.
- `pane/reopen` works for **any** run — including a terminal one — whose attach socket answers the current liveness-marker handshake within a bounded timeout; a dead or absent socket is an honest refusal regardless of the run's state. This is deliberate (ADR-0027 wave 3): a TUI vendor's process outlives its own turn, so a settled or even finished run can still have a live pane behind it, and that's exactly the moment someone wants to look at it. The probe can produce a false *negative* (refusing a genuinely-live pane whose handshake happened to be slow) but is designed to never produce a false positive (claiming a dead socket is live) — if reopen refuses, trust it and fall back to the transcript rather than retrying rapidly.
- `/crew clean` prunes only terminal/unassociated event history under the configured retention period and `maxRuns`; it never deletes a live run or task row.

## Read surfaces

- `/crew health` / `crew_health` — daemon status, protocol/schema version, active run count, and the dashboard URL (token included) when `dashboard.enabled` — this token now lives in whatever holds this session's context (transcript, logs) every time health is checked, model-initiated or not.
- `/crew config path | print [effective|defaults|schema] | init [global] [force]` — inspect or scaffold `crew.json`. No set/edit subcommand: to change a value, edit the file `path` locates directly.
- `/crew doctor` — environment diagnostics without needing a live daemon (stale workspace leases, display backend availability, and more).

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
| `not-absolute` / `not-found` / `not-regular` / `not-executable` / `no-binary` | An `OMP_CREW_BINARY` override is set to a bad path (not absolute, missing, not a regular file, not executable) or no override is set and no packaged binary could be resolved. Fix or unset `OMP_CREW_BINARY`, or run `/crew-install`. |
| `write-failed` (from `/crew-install`) | The downloaded binary or manifest couldn't be written to the Crew state root — check disk space and permissions there. |
| `http-error` (from `/crew-install`) | Usually a GitHub API rate limit (the repository is public, so no token is required, but an unauthenticated request is capped at 60/hour vs. 5,000/hour authenticated). Set a `GITHUB_TOKEN` or `GH_TOKEN` environment variable, or run `gh auth login`, then retry the install. |
| `model-not-configured` (from `crew_profile`) | No model is configured yet for this adapter. Ask the user which model to use, then call `crew_profile` again with it. |
| `model-conflict` (from `crew_profile`) | The requested model differs from one already configured for this adapter. `crew_profile` never overwrites it — edit the repository's `.omp/crew.json` directly (`/crew config path` locates it) to change the stored value. |

For daemon crash recovery, a `lost` run, or reattaching after your own session dropped, use the crew-recovery skill.
