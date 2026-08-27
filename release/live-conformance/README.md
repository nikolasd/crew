# Live TUI conformance evidence (0.5.0)

Raw `crewd conformance --live --mode tui` reports, copied verbatim from the
WP29 run harness output (no fields altered — provenance preserved).

| File | Source (`/tmp`) | Adapter | runnable scenarios | `session_resume` |
|---|---|---|---|---|
| `claude-tui.json`   | `live-claude-v6.json`   | claude   | 4/4 pass | skipped |
| `omp-rpc-tui.json`  | `live-omprpc-v2.json`  | omp-rpc  | 4/4 pass | skipped |
| `codex-tui.json`    | `codex-tui-live-0827.json` (2026-08-27, current main@2cde61e) | codex | 4/4 pass, unchanged from the 2026-08-26 rerun (byte-identical report) | skipped |
| `copilot-tui.json`  | `copilot-tui-live-0827.json` (2026-08-27, current main@2cde61e) | copilot | 2/4 (read_only + follow_up fail — CONFIRMED vendor monthly quota wall, not a capture defect) | skipped |
| `copilot-tui-2026-08-26-transcript-capture.json` | `copilot-tui-live-rerun.json` (2026-08-26 eve) | copilot | 2/4 (superseded diagnosis: discovery itself failed then, see below) | skipped |
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

## Vendor billing walls (not adapter defects) — status as of 2026-08-27

- **codex**: RESOLVED, re-proven on current main. The 2026-08-27 rerun
  (`codex-tui.json`, main@2cde61e, post-#17/#18 adapter changes) is
  byte-identical to the 2026-08-26 evening report: all four runnable
  scenarios still pass (4/4). The pre-refill `codex-tui-post-quota.json`
  (v6) is retained only as historical exhaustion evidence.
- **copilot**: RECLASSIFIED BACK to a confirmed vendor billing wall — not
  a capture defect. The 2026-08-26 evening diagnosis above (a
  transcript-capture/discovery failure) was the **untrusted-workspace**
  problem #15 fixed: discovery now works (`start=Ok(())`, `session=true`
  in the 2026-08-27 report, vs. `start=Err(...)`/`session=false` on
  2026-08-26 — compare `copilot-tui.json` against
  `copilot-tui-2026-08-26-transcript-capture.json`, kept here for that
  contrast). With discovery fixed, the real remaining failure is the
  vendor's own monthly quota: `read_only_start_and_progress` and
  `follow_up` both fail with `first_message=false`/`saw_ack=false` — no
  turn ever produced output — and the tailed session's own
  `~/.copilot/session-state/6a33ac51-337f-4da7-8f4d-91a3c218c98a/events.jsonl`
  (copilot CLI 1.0.81, cwd `/private/tmp/crew-smoke-proj`) records a typed
  `session.error`:
  ```json
  {"type":"session.error","data":{"errorType":"quota","statusCode":402,"errorCode":"quota_exceeded","message":"You have exceeded your monthly quota"}}
  ```
  independently re-verified against that raw session file, not just the
  harness's summary report. Tracked as a billing wall, same as codex's
  earlier one — not an open adapter defect.

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

## Rerun 2026-08-27

Fresh live TUI conformance ran again on current `main@2cde61e` (post-#17/#18
adapter changes), from a trusted `CREW_LIVE_CWD`:

- **codex**: `codex-tui.json` replaced with the fresh run — byte-identical to
  the 2026-08-26 evening report. All four runnable scenarios still pass;
  `session_resume` skipped by design.
- **copilot**: `copilot-tui.json` replaced with the fresh run; the previous
  report is kept as `copilot-tui-2026-08-26-transcript-capture.json` for
  contrast. `probe` and `cancellation_scope` pass; `read_only_start_and_progress`
  and `follow_up` fail. The diagnosis changed from the 2026-08-26 evening
  entry above: discovery itself no longer fails (that was the untrusted-
  workspace problem #15 fixed — this run's `start=Ok(())`/`session=true`
  proves it), so the failure is now root-caused, not just suspected, as the
  vendor's own monthly quota wall — the tailed session's `events.jsonl`
  records a typed `session.error` with `errorCode: quota_exceeded`
  (`statusCode: 402`), independently re-verified against the raw session
  file on disk, not just the harness's own summary report. See the
  "Vendor billing walls" section above for the full detail.
