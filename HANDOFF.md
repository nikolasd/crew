# crew v2 Handoff

Date: 2026-08-27
Repo: /Users/nikolasdemiridis/Personal/Repos/batman
Current HEAD: dcf3745 Merge branch 'main' into review-c1-vendor-resume (branch `review-c1-vendor-resume`)

> **Focus for the next session:** finish the interim-review remediation (C1 fix is IN PROGRESS on this branch — 12 modified, uncommitted files under `crates/runtime/src/adapter/tui/` were deliberately left untouched by this handoff), then WP29.

## Starting Point (how this project came to be)

- Began as a greenfield brainstorm for an Oh-My-Pi plugin ("crew"): OMP as team leader spawning Claude Code / Codex / Copilot / OMP workers as visible, steerable, resumable sub-agents.
- Spec v1 (greenfield) was superseded when the BATMAN codebase in this repo was adopted as the base after a four-track exploration and a keep/discuss/drop comparison.
- Governing artifacts: spec `docs/superpowers/specs/2026-08-22-crew-v2-design.md`; plan `docs/superpowers/plans/2026-08-22-crew-v2-gap-closure.md` (29 work packages, 9 phases); comparison `docs/superpowers/2026-08-21-batman-vs-crew-comparison.md`; execution ledger `.superpowers/sdd/2026-08-22-crew-v2-gap-closure/progress.md` (rulings, deferrals, per-WP records).

## Key Decisions (user-made, recorded in spec §2 + ledger)

- Keep BATMAN's Rust daemon architecture (reversed the spec-v1 in-process-only decision); rename everything batman→crew (incl. crates, state root w/ legacy fallback, GitHub repo → nikolasd/crew).
- Real vendor TUIs in panes; control = PTY keystroke injection + transcript tailing, never screen scraping. Accepted costs recorded in spec §2.3.
- Vendor resume wired (resume = same run continuing; retry = new run). All three workspace isolation modes kept.
- crew JSON config replaced YAML; org-governance enforcement (allowlists, cost ceilings, rollout gates) retired with it (ruling: "refined option A").
- Headless adapters kept during migration; per-vendor keep/drop decision deferred to WP29 (user decides).
- Dashboard daemon-hosted, localhost, read-only.

## Outcomes

- **WP1–WP28 complete and merged to main** (PRs #2–#10 squash-merged + later phase merges). All four vendors (claude/codex/copilot/omp) have TUI adapters and default to `mode: tui`. Full gate green at review time (`CREW_DISABLE_VENDOR_CLI=1 bun run check`, exit 0, 69 suites).
- **WP29 not started**: live smoke (`CREW_LIVE_*`), per-vendor headless keep/drop (user's call), release readiness.
- **Interim whole-implementation review (2026-08-26, read-only, 3 tracks)** verdict: faithful to plan/spec, high quality, but not WP29-ready. Findings:
  - **C1 (Critical):** `registry.rs` `tui_transcript_path_for_session` hardcodes the Claude vendor; recovery calls it for every TUI run → codex/copilot/omp runs silently terminalize on daemon restart with misleading Claude-path reasons. An expired conditional deferral.
  - **I1:** `record_escalation_raised` writes its projection row outside the event transaction (correct sibling pattern exists in the same file at the WorkerQuestion auto-escalation).
  - **I2:** repeated-failure escalation SQL counts ANY prior failure, not consecutive; no open-escalation dedupe.
  - **I3:** migrations 11–14 lack the v(N−1)→vN step tests (8/9/10 have them).
  - **Minors:** M1 sub-batch duplicate-ToolStarted window never discharged (ledgered to WP15, unaddressed — fold into WP29 resume verification); M2 interrupt/compose conventions unvalidated for 3 vendors → WP29 must assert "turn actually stopped"; M3 `attach_turn_budget` two autocommit writes; M4 fixture provenance READMEs missing for codex/copilot/omp-tui; M5 stray dist rebuild + WATCHDOG.yml (both since handled — see 94c89b8).
  - **Latent:** TUI runs authorized against headless capability sets — currently harmless (evaluator ignores capabilities); tripwire test added at f4ae87d.
  - **Process lesson:** conditional deferrals expire silently — sweep every "deferred/carry" ledger line at phase end asking "has the trigger fired?"

## Most Recent State (this branch, in flight)

- Remediation underway per the accepted batch plan: 94c89b8 closed M5 (dist), f4ae87d added the capability-parity tripwire (latent item), and the uncommitted changes across `crates/runtime/src/adapter/tui/*` are the C1 per-vendor resume fix in progress (plus, judging by scope, possibly I-batch work). **Do not assume these files' state — read them, finish/verify, gate, commit.**
- Remaining after C1 lands: I1, I2 (single-function fixes, siblings in `domain/repository.rs` to copy), I3 + M4 (hygiene commit), then WP29 with the M1/M2 checklist additions.

## Commands

- Full gate: `CREW_DISABLE_VENDOR_CLI=1 bun run check` (never set both old+new kill-switch vars — breaks `vendor_cli_availability`).
- Focused: `cargo test -p crew-runtime --test <suite>`; `bun test packages && bun run typecheck`.
- Git: branches via `git town hack`/`append`, push via `git town sync`, never plain `git push`; deletions/history rewrites are the user's to run.
- Dist bundle: rebuild ONLY under pinned Bun 1.3.14 (or via `refresh-bundle.yml`); local newer Bun produces CI-failing drift.

## Testing Notes

- Known flake: `concurrent_cancelling_violations_are_both_idempotent_successes` (orchestration_rpc) — root-caused as an over-tight assertion (both outcomes legal per production's own contract); a broadened-assertion fix was specified in the ledger but verify whether it ever landed.
- Live vendor tests are env-gated (`CREW_LIVE_*`), never CI; fixtures are recorded/scrubbed (claude-tui: recorded-headless@2.1.241 — see its README).

## Outstanding Work

1. Finish + commit the in-flight C1 fix on this branch (per-vendor dispatch via a shared `vendor_for_kind`-style helper; thread the derived transcript path into `ResumeContext` to delete the double-derivation seam; per-vendor recovery tests).
2. I1 + I2 fixes; I3 migration step tests; M4 READMEs; M3 single-transaction budget attach.
3. WP29: live smoke per vendor (with "turn actually stopped" and duplicate-ToolStarted checks), per-vendor headless keep/drop (USER decides), release (version bump, `crewd` assets, `crew_install` e2e).
4. Finish-phase artifact: the rulings roll-up from the ledger (every "Ruling:" line, chronological) — owed to the user at plan completion.

## Suggested Skills for Next Session

- superpowers:subagent-driven-development (plan execution resumes from the ledger — read it first)
- superpowers:test-driven-development, superpowers:verification-before-completion
- satori-collab:writing-plain-language (user prefers plain-language conversational replies)
