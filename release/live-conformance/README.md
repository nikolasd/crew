# Live TUI conformance evidence (0.5.0)

Raw `crewd conformance --live --mode tui` reports, copied verbatim from the
WP29 run harness output (no fields altered — provenance preserved).

| File | Source (`/tmp`) | Adapter | runnable scenarios | `session_resume` |
|---|---|---|---|---|
| `claude-tui.json`   | `live-claude-v6.json`   | claude   | 4/4 pass | skipped |
| `omp-rpc-tui.json`  | `live-omprpc-v2.json`  | omp-rpc  | 4/4 pass | skipped |
| `codex-tui.json`    | `codex-tui-live-rerun.json` (2026-08-26 eve) | codex | 4/4 pass (credits refilled) | skipped |
| `copilot-tui.json`  | `copilot-tui-live-rerun.json` (2026-08-26 eve) | copilot | 2/4 (read_only + follow_up fail) | skipped |
| `codex-tui-post-quota.json` | `live-codex-v6.json` | codex | 2/4 (pre-refill exhaustion; now superseded) | skipped |

"runnable" = every scenario except `session_resume`, which is skipped by
design (see below).

## Erratum — `session_resume` detail

Each raw report's `session_resume` scenario carries the detail:

> genuine restart recovery is proven by the separate serve->stop->serve
> end-to-end smoke (WP29), not this report

This is **overstated**. The serve->stop->serve smoke that passed in WP29 was
**vendor-free** (`crewd serve -> status -> stop`, no vendor task). The
*transcript-recovery-across-a-real-daemon-restart* case — i.e. run a vendor
task, stop the daemon, restart it, and assert the transcript is recovered —
is a **separate end-to-end smoke that has NOT yet run**. It is tracked as a
post-0.5.0 follow-up. Treat `session_resume` as skipped, not as proof of
daemon-restart recovery.

## Vendor billing walls (not adapter defects) — status as of 2026-08-26 evening

- **codex**: RESOLVED. The 2026-08-26 evening rerun (`codex-tui.json`) passes
  all four runnable scenarios (4/4). The earlier `follow_up` /
  `read_only_start_and_progress` failures were a credit wall (`usageLimitExceeded`)
  and are gone now that the workspace quota has refilled. The pre-refill
  `codex-tui-post-quota.json` (v6) is retained only as historical exhaustion
  evidence; it is superseded by the healthy rerun.
- **copilot**: RECLASSIFIED — NOT a credit wall. The real copilot CLI spawns
  (probe + `cancellation_scope` pass), so this is not quota. The failure is a
  **transcript-capture** issue: within 120s no session file containing the
  adapter's nonce appears under `~/.copilot/session-state`, so the TUI adapter
  cannot tail the vendor transcript (`read_only_start_and_progress` and the
  cascading `follow_up` fail). Likely a copilot-CLI version / session-store
  mismatch versus the fixture-pinned `copilotVersion: 1.0.80`. Tracked as a
  separate capture/adapter fix, independent of billing.

## `codex-tui-post-quota.json` — later rerun, weaker evidence

`live-codex-v6.json` (13:35, ~40 min after v4) is a **newer** codex rerun in which no turn could be
observed at all: `read_only_start_and_progress` failed with `first_message=false` (session started,
no first message) and `follow_up` with `saw_ack=false` — **without** the typed
`usageLimitExceeded` reason v4 captured. This is consistent with the workspace being fully out of
credits by then (the TUI shows a quota notice the harness cannot parse as a typed refusal), but it
is *not* proof of that. It does not contradict `codex-tui.json` (v4), which remains the evidence
that the spawn→type→submit→discover path works when credits exist; treat v6 as documentation of
the post-exhaustion state only.

## Provenance correction (2026-08-26)

The copies first committed to this directory were not byte-identical to their `/tmp` sources: the
`session_resume` detail sentence had been reworded in-place (~115 bytes per file) instead of leaving
the correction to this erratum. All four files above have been replaced with byte-exact copies of
their sources (`cmp`-verified); the overstatement is corrected here only.

## Rerun 2026-08-26 (evening)

Both `codex` and `copilot` live TUI smokes were rerun via
`crewd conformance --live --mode tui` after the WP29 follow-up work:

- **codex**: 4/4 runnable (credits refilled). `codex-tui.json` replaced with the
  fresh run; `session_resume` skipped by design.
- **copilot**: still 2/4, but the root cause is now confirmed to be a
  session-state transcript-capture failure, **not** a credit wall (the CLI
  runs). `copilot-tui.json` replaced with the fresh run; the failure needs its
  own capture/adapter fix.

The OS-process `serve -> stop -> serve` IPC transcript-recovery test
(`tests/lifecycle.rs::real_daemon_survives_serve_stop_serve_with_ipc_transcript`)
now covers the daemon-restart seam that `session_resume`'s skipped detail had
overstated.
