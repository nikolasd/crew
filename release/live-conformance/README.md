# Live TUI conformance evidence (0.5.0)

Raw `crewd conformance --live --mode tui` reports, copied verbatim from the
WP29 run harness output (no fields altered — provenance preserved).

| File | Source (`/tmp`) | Adapter | runnable scenarios | `session_resume` |
|---|---|---|---|---|
| `claude-tui.json`   | `live-claude-v6.json`   | claude   | 4/4 pass | skipped |
| `omp-rpc-tui.json`  | `live-omprpc-v2.json`   | omp-rpc  | 4/4 pass | skipped |
| `codex-tui.json`    | `live-codex-v4.json`    | codex    | 3/4 (follow_up fail) | skipped |
| `copilot-tui.json`  | `live-copilot-v3.json`  | copilot | 2/4 (read_only + follow_up fail) | skipped |

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

## Vendor billing walls (not adapter defects)

- **codex**: `follow_up` fails because the workspace is out of credits
  (`usageLimitExceeded`). `read_only_start_and_progress` passed (a normalized
  message was tailed), so the spawn->type->submit->discover path is proven;
  only the second-turn follow-up is blocked.
- **copilot**: `read_only_start_and_progress` and `follow_up` both fail on the
  same out-of-credits error. Probe + spawn + cancellation mechanic is proven;
  the transcript-tailing scenarios are blocked on quota.
