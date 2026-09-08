# Crew live E2E runbook — v0.7.0

Supervised live test of crew, gating the v0.7.0 cut — the exact commit under test is recorded in
[`release/checklist-0.7.0.md`](checklist-0.7.0.md), which is also what gets ticked off during this
run. Every phase below either verifies a specific class of regression an earlier run surfaced, or
exercises what has changed since.

**The gate is open and no P1 remains.** CREW-52 made panes work under tmux and herdr for the first
time and added state root + socket to `/crew health`; CREW-61 closed the redaction class with a
compile-time guard and fixed four live leaks on the way. CREW-73 and CREW-74 then landed the pane
half of that work: an attach walks the remaining backend candidates before falling back to hidden,
and every requested-versus-actual divergence is journaled with the sequence it tried. The exact
commit under test is the one recorded in [`release/checklist-0.7.0.md`](checklist-0.7.0.md). The
one open ticket at the time this was last revised is CREW-69 (escalations carrying the worker's
question, deferred to post-E2E by ruling); it does not block the test.

Results from the run go under [`release/live-conformance/`](live-conformance/) once it completes.

P0 through P2 (the no-model-call phases) have already been dry-run against a recent `main` with no
model calls: zero product defects found; the interactive widget and health rows could not be
exercised without a real terminal session and remain unverified claims, not passes — see them
called out individually below.

Canonical procedures: `docs/manual-testing.md` (§ references on each phase). Debugging playbook:
`docs/code-walkthrough.md` §4.

Whoever drives the session makes every merge and release decision; keep a second person ready for
diagnosis and a rapid fix.

## Stop conditions

Abort the phase immediately and preserve state if any of the following happens:

- **Any redaction leak** — a raw secret, thinking content, or unredacted prompt text visible in the
  journal, dashboard, `/crew`, or an exported artifact. The worst class. Stop, do not clean up,
  capture `crewd audit export` first. CREW-61 closed four live instances, so these are the fields
  to eyeball in the export: `cleanupFailed.error`, a plan's `subtasks[].description`, and
  `paneDowngraded.reason`. Note the deliberate exception: `leaseAcquired.path` is an absolute
  filesystem path on purpose — it is that event's subject, `run/get` already returns it, and it is
  what makes a masked `cleanupFailed.error` survivable (join on `leaseId`). Do not report it as a
  leak.
- **Daemon crash loop** (respawn more than twice) — stop, collect `pgrep -fl crewd` plus the
  state-dir logs, hand off for diagnosis.
- **A billed runaway** (a worker looping on model calls) — cancel it with `crew_run` op `"cancel"`;
  if unresponsive, `pkill -f "crewd serve"`.

Anomalies that don't stop the test get one line each in a findings list (phase, expected, observed)
— filed as tickets after the run, never worked mid-run.

## P0 — Preflight

no model call · see `docs/manual-testing.md` §Prerequisites, §Owning what you test

The setup block below has `#` comments. Interactive zsh rejects a `#`-prefixed line when pasted
("command not found: #") — paste it one command at a time, or run `setopt interactive_comments`
first.

```bash
# from the repository root
git town sync                 # or your usual sync to the commit under test
cargo build                   # a clean build is expected; produces target/debug/crewd
./target/debug/crewd stop --repo "$PWD"; pkill -f "crewd serve" 2>/dev/null
# Use the LOCAL binary, never a PATH crewd (which may be an older release). This one always
# prints "runtime stopped" or "no runtime running for this repository".
# TRAP: `crewd stop` resolves the daemon via CREW_STATE_DIR. Run it in a shell WITHOUT that
# variable set and it reports "no runtime running" while the daemon is still alive. Every
# command in the session needs the SAME CREW_STATE_DIR; verify with `pgrep` before trusting
# a stop's message.
export CREW_STATE_DIR=/tmp/crew-e2e3-state && mkdir -p "$CREW_STATE_DIR"
# A BRAND-NEW PATH. Do not reuse a state dir from an earlier attempt: if it holds events
# carrying placement "embedded" (a value CREW-52 deleted), this binary refuses to replay
# that journal. The refusal is the documented breaking change working correctly (expect a
# legible error naming the remedy, not a panic) -- but it would stop the test dead. Start clean.
export OMP_CREW_BINARY="$PWD/target/debug/crewd"
unset CREW_DISABLE_VENDOR_CLI  # live test: vendor calls allowed, and billed
EXT="$PWD/packages/extension"  # the package DIRECTORY, not dist/index.js -- a file path
# silently drops all six provider categories (skills/rules/commands/prompts/hooks/tools);
# the directory form resolves package.json -> the committed dist AND discovers skills/ + rules/.
# The directory form runs dist/index.js, NEVER src/ -- if a mid-test fix lands in the
# extension, rebuild it before relaunching or you retest the old bundle with no warning.
```

| Check | Expect / verifies |
|---|---|
| Version check first: `omp --version` | `omp/18.1.13` — if the version printed differs, note it before continuing; nothing else in preflight detects a silent upgrade. The peer range `>=17.0.7 <19` covers it. |
| Leader model: use a capable paid model (`/model`), not a free fallback chain | The phases that need a live worker depend on the leader reliably following orchestration instructions — a weak leader tests the leader, not crew |
| Skills probe (once per session): ask the leader "list your available skills by name" | `crew-orchestration, crew-approvals, crew-troubleshooting, crew-recovery` listed — discovery requires the package-directory form of `--extension`; a file path establishes no package root, so skills and the delegation-guard rule silently drop. If missing with the directory form in use, **stop** — that is a regression to root-cause before any billed phase, not something to work around. |
| Vendor login: `claude --version` works and the CLI is authenticated (open it once interactively if unsure) | Prints a version and the CLI is signed in — do not pin the number, it drifts; note the value you see in the findings list |
| Pick the host for panes and stay in it (panes follow the host) | Terminal tab *or* tmux splits — note which; expected pane behavior differs |
| Environment sanity: `echo $CREW_DISABLE_VENDOR_CLI` | Empty — if set, the live phases silently test nothing real |

## P1 — Cold start: daemon, doctor, health

no model call · see `docs/manual-testing.md` §1

| Step | Expect / verifies |
|---|---|
| First after relaunch: `omp --extension "$EXT" --print "/crew health"` | `Binary source: override` answers — this is the one thing the skills probe does not prove: that the directory form also loads the extension module (entry-point resolution is a separate code path from skills discovery). Health answering settles it. |
| `omp --extension "$EXT" --print "/crew doctor"` | Config, state dir, and gates all pass — cheapest break-detector, no daemon spawn |
| `omp --extension "$EXT"` then `/crew health` | Daemon spawns; `Binary source: override` (proves the local build is under test, not a downloaded release); no Dashboard line yet — the dashboard is opt-in (default off, port 4747); it appears after P2's enable step, then in full with a token. Also verifies the socket path fits `sun_path` under the default state root. |
| Widget check (no command needed) | The Crew box appears on session start showing "Crew active, waiting for task submissions" — the healthy empty state, rendered immediately when the runtime connects; `/crew` just re-renders it on demand. Old failed runs appearing instead means you're on the default state root, not the fresh one. |

## P2 — Dashboard

no model call

| Step | Expect / verifies |
|---|---|
| Enable it (opt-in): `mkdir -p .omp && printf '{"dashboard": {"enabled": true}}' > .omp/crew.json` (repo-level, gitignored), then `./target/debug/crewd stop --repo "$PWD"` and rerun `/crew health` | Daemon restarts with the config; health now prints the Dashboard URL with a token — config is strict JSON, keys exactly `enabled`/`port` (unknown keys refuse startup); default port 4747 |
| Open the printed URL in a browser | Loads; the token disappears from the address bar — a valid `?token=` on the URL is exchanged for the `crew_dashboard` cookie via a 303 redirect so the secret leaves the URL, browser history, and any Referer. That exchange is the design, not a leak; a reload now works via the cookie, which is correct, not a gate failure. |
| No-cookie check (the real gate): `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:<port>/` and again with `?token=bad` | `401` both times — an unauthorized request reaches no handler at all, not even a 404 |

## P3 — First live run: small prompt

billed · see `docs/manual-testing.md` §3a–3b, §4f

Register a task and a claude worker, submit a short run (a one-paragraph question).

| Watch for | Expect / verifies |
|---|---|
| Model selection on first use | Asked once, then persisted to the repository's config; silent on later runs — the ask must appear if no model is configured yet for this adapter |
| Pane attach | Pane opens in the chosen host — note the pane reference in `/crew` ("pane attached: `<backend>`") |
| **If the pane is not in the preferred host** (CREW-73/74) | Not a failure by itself — an attach retries the remaining candidates, so a pane in a *different* host is the retry working. What must accompany it: a `paneDowngraded` event whose `requestedBackend` differs from its `actualBackend`, carrying `requestedPlacement`, the `attempted` sequence, and a redacted `reason`. A pane in a non-preferred host **with** that event is correct behaviour; **without** it is the finding, because the divergence then reached nobody. Grep the export for `paneDowngraded` (camelCase, the wire form). |
| **If no pane opens at all** (CREW-60) | The fallback is typed and journaled, not silent: `actualBackend` is `hidden`, `reason` names how many candidates were tried and the last failure, and `attempted` lists the backends resolution walked or tried in order (an entry means resolution reached that backend, not that a pane was attempted on it). The monitor shows a sticky downgrade flag a later unrelated event cannot overwrite. A missing pane *without* this event is the finding; a missing pane *with* it is the mechanism working, and `reason` says why. |
| **On `/crew reopen`** (CREW-74) | Reopen is single-attempt by design: pane-creation failure returns an **error to the caller**, not a hidden fallback, so expect a failed command rather than a downgrade event. A reopen that silently reports success with no visible pane is worth noting — the two paths where no backend is available journal a hidden attach and return success. |
| **Run completion** — the headline check (CREW-47/48) | On the worker's turn end, the run moves to `waitingUser` with the pane still open, and stays there. Only a delivered follow-up or a genuine user turn may resume it, and a thinking-only turn end is not a boundary at all. **Watch for any return to `working` you did not cause — that is a regression, and it is the single most important observation of the run.** |
| `crew_run` op `"result"` on the parked run (CREW-49) | Returns the full answer text and usage — a `null` result with a visible answer already in the pane is a regression |
| `crew_transcript` op `"replay"` on the same run (CREW-50) | Returns a normalized digest array — free to check, and the tool to reach for if anything else goes wrong |
| Journal | Full submit prompt journaled, redacted; check via `/crew run <runId>` or `crewd audit export` — the dashboard shows runs/usage, a prompt column is future work |
| Widget | Row shows state transitions plus `usage … in / … out ($…)` |

## P4 — Large prompt

billed · CREW-4

| Step | Expect / verifies |
|---|---|
| Submit a run whose prompt is >8 KB (e.g. paste a long file with an instruction at the end: "reply with the last word of this prompt and its byte length") | The worker sees the whole prompt — its answer proves the tail arrived. Journal carries the full redacted text. |
| **If the run fails to start with a paste error** | The bound is now on progress, not elapsed time: the error reports how many bytes of the prompt had already been accepted and how long was actually waited. A failure means either no byte was accepted for 2 seconds (the primary, progress-based signal), or the 90-second absolute backstop fired even while bytes kept advancing. Either can mean the vendor stopped reading its stdin, or that this host is too loaded to run the writer thread promptly — check host load first. **Before this phase, close other heavy local processes; if a paste failure still fires, check host load before treating it as a finding.** This is the fix's first live exercise. |

## P5 — Subagent dispatch: the isSidechain gate (required)

billed, required · ADR-0027; the isSidechain guard

Submit a run whose prompt *forces* subagents, e.g.: *"Use your Task/subagent tool to dispatch two
parallel subagents, each summarizing a different file in this repo; then combine their answers
yourself and finish."*

| Watch for | Expect / verifies |
|---|---|
| While subagents run and finish their turns | Run stays `working` (two guards apply here: the isSidechain rule, plus the content guard, since a thinking-only entry no longer ends a turn either) — a subagent's turn end must never settle the parent run. Premature `succeeded` mid-dispatch is a finding, not a pass. |
| `/crew` activity | `adapterNestedWorkerObserved` appears — the sidechains are seen, classified, and not misattributed |
| Parent finishes combining | Run settles to `waitingUser` exactly once, on the parent's turn end; `run/result` then returns the combined answer, not a subagent fragment; `op finish` closes it |

## P6 — Lifecycle honesty: stop, follow-up, finish

billed · ADR-0027 surface

| Step | Expect / verifies |
|---|---|
| Start a run, then cancel it mid-work: `crew_run` op `"cancel"` (there is no `crew_stop` tool) | Honest acknowledgement (no fabricated ok), run moves to `cancelled`, pane cleaned up |
| Send a follow-up message to a run in `waitingUser` | Accepted under the follow-up allowance; run resumes and re-settles, journaled as `runResumed` with `cause: "followUpMessage"`. Seeing the other value, `realUserTurn`, when *you* sent a follow-up, or seeing neither, is a finding — resumption is supposed to be caused, not inferred. |
| Close a `waitingUser` run: `crew_run` op `"finish"` | Run terminalizes; journal shows the finish with its actor — a run is a conversation the leader closes |

## P7 — Resilience: daemon restart, reopen, replay

no model call · see `docs/manual-testing.md` §3c; CREWATTACH1

| Step | Expect / verifies |
|---|---|
| `pkill -f "crewd serve"` with the session open, then take any crew action | Daemon respawns; the widget subscription heals without restarting the session |
| `/crew reopen` on a closed pane | Reopens; a stale pane is refused with a clean error, not a hang (the CREWATTACH1 liveness probe) |
| Quit and relaunch entirely, then `/crew runs` | Full state reconstructed from the journal — same runs, same states, same usage totals |

## P8 — Concurrent isolated runs (stretch)

stretch, billed · see `docs/manual-testing.md` §5

Two workers, two isolated workspaces, submitted together: leases don't collide, both settle
independently, `crew_peer_workspace` reads across. Run if P1 through P7 were clean and time
allows.

## P9 — Result chaining (stretch)

stretch, billed · see `docs/manual-testing.md` §6

Read run A's `run/result`, feed it as run B's prompt. Verifies the read-back path end to end (the
redaction boundary holds on read, not just write).

## P10 — Wrap: evidence and verdict

no model call

```bash
# NOTE the state dir: it must be the one P0 exported, not the default. Using the default
# state root here would export an unrelated journal and miss this entire test -- the
# evidence step silently succeeding on the wrong data.
crewd audit export --repo "$PWD" --state-dir "$CREW_STATE_DIR" --output /tmp/e2e-audit-$(date +%Y%m%d).jsonl
wc -l /tmp/e2e-audit-$(date +%Y%m%d).jsonl   # sanity: a run happened, so this is not 0
crewd stop --repo "$PWD"   # same shell, same CREW_STATE_DIR as P0 -- a stop under a
                            # different root reports "no runtime running" and leaves the
                            # daemon alive
pgrep -fl "crewd serve" || echo "stopped"   # trust this, not the stop's own message
```

- Findings list reviewed together: each becomes a ticket, a doc fix, or an accepted quirk — decided
  after the run, not during.
- **Pass bar for v0.7.0:** P1 through P7 all green, P5 (the subagent gate) green explicitly, zero
  redaction anomalies.
- Then: follow `release/checklist-0.6.0.md`'s shape to finish `release/checklist-0.7.0.md`, and
  file live conformance results into `release/live-conformance/`.
