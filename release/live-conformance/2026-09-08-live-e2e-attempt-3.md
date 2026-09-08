# Live end-to-end run — 2026-09-08, attempt 3

**Verdict: FAIL. The v0.7.0 release gate stays closed.**

Build under test: `main` @ `cab041a`, `crewd` built from source (binary mtime 19:56, after
`cab041a` at 17:57; contains the CREW-74 string; no source newer than the binary).
Host: omp 18.1.13, claude CLI 2.1.263, herdr 0.8.2, load 3.4 on 18 cores.
State root `/tmp/crew-e2e3-state` (created empty; the default `~/.omp/crew` was untouched).
Target repository: a freshly created and committed repo (`fcb70e5`) — **new to the vendor CLI**,
which turned out to matter more than anything else on this list.

Written from the run's own observation log, not from the runbook's pre-run text. Where this record
states that something works, it is reporting what was observed on 2026-09-08 and nothing more.

---

## Phases executed

| Phase | Scope | Result |
|---|---|---|
| P0 | Preflight — versions, build freshness, state isolation | **PASS** |
| P1 | Cold start — daemon, doctor, health | **PASS**, with finding F1 |
| P2 | Dashboard — auth gate, cookie exchange, binding | **PASS** |
| P3 | First live run — small prompt | **partial**; headline check passes, ends in a run wrongly declared `lost` |
| P4 | Large prompt — 18,454 bytes | **PASS on retry** (tail delivered), with findings F13–F15 |
| P5 | Subagent dispatch — the `isSidechain` gate | **NOT RUN** |
| P6 | Lifecycle honesty — stop, follow-up, finish | **NOT RUN** |
| P7 | Resilience — daemon restart, reopen, replay | **NOT RUN** |
| P8 | Concurrent isolated runs | **NOT RUN** |
| P9 | Result chaining | **NOT RUN** |

**P5–P9 were not run.** They are recorded here as not run, not omitted. The pass bar for this gate
is P1–P7 all green, P5 explicitly — so the bar was not met, and could not have been met by this
attempt regardless of what P5–P7 would have shown.

**Why the run stopped after P4** (maintainer's decision, ~19:35 local): two further P1 candidates
had appeared — a parked run declared `lost` after five minutes, and a parked run flooding the
journal at ~30 rows/second — on top of the fabricated-success class and the first-run gate. Seventeen
findings with five P1 candidates is a fix wave, and P5–P7 are worth more against a fixed build than
against this one. **No stop condition in the runbook's sense fired**: the redaction sweep was clean,
there was no crash loop, and no billed runaway.

---

## The five P1 candidates

Named here rather than left to ticket references, so this record stands alone.

### 1. A failed start fabricates success, and leaves no durable trace (F4, F17 → CREW-78)

Observed twice. On run `01a08216` the submit RPC returned `start failed: no transcript containing
nonce … within 8s` to the leader, while the run it had already journaled as `working` was neither
failed nor cancelled. The vendor process ran on for 45 seconds, exited 0, and the run was recorded
**`succeeded`** — with no turn, no message, no vendor session, and a null result. On run `01a08251`
the same shape recurred after a truncation failure.

Four mechanisms compound:

- `fail_start` (`crates/runtime/src/adapter/tui/adapter.rs:1266-1292`) calls `terminate()` and then
  emits `ProcessExited` from `exit_signals()` **unconditionally, with no failure marker**. The trust
  dialog had been dismissed by hand, so claude exited 0 — and the *failure path* journaled
  `ProcessExited{0}`.
- `terminal_state_for` (`crates/runtime/src/adapter/run_lifecycle.rs:608-618`) maps `Some(0)` to
  `succeeded` with no turn check. This is ADR-0023's single-exit logic, never revisited by ADR-0027
  for TUI vendors, where a zero exit means only that the terminal closed.
- The terminal state is committed **inside** `start()`, before it returns `Err`, so orchestration's
  error handling cannot correct it. `walk_to`'s terminal guard — which stops later edges — is
  precisely what makes it uncorrectable.
- **Nothing durable records that a start failed.** The error path emits only
  `WorkspaceEvent::LeaseReleased` (`crates/runtime/src/service/orchestration.rs:1644-1651`) and
  returns `Err`. The diagnosis exists only in the RPC response to the leader, which is never
  journaled.

The last point deserves emphasis in a conformance record: **a failed start is durably
indistinguishable from a run that worked.** Two reviewers independently searched the 15,340-event
export for the P4 failure and each concluded the export must be the wrong window. It was not —
failures are simply not written. Against `README.md:18` ("Every action is persisted before it
executes. Crash? Replay from the last known state"), a replay of run `01a08251` shows a success that
never happened.

**F17 is attributed:** the maintainer closed both Ghostty tabs by hand at 18:49:43. Run `01a08253`
then went `displayPaneDetached` → `adapterProcessExited{0}` → `runWorking` → `runSucceeded` in the
same second, with no leader action and no new turn. So the sequence is *"a human closes a parked
worker's terminal ⇒ the run is recorded succeeded"* — not an unattended failure. The instantaneous
`runWorking` is a transition-table artefact (`waitingUser → succeeded` is not a legal direct edge, so
`walk_to` hops through `working`); the artefact is benign, the `succeeded` it reaches is not.

Open decision for the maintainer: **`failed` or `lost`?** The recommendation on record is `failed` —
`lost` means the supervisor could not observe *how* the process exited, and here it observed a clean
exit perfectly well.

### 2. First-run vendor prompts are invisible, and crew types into them (F10, F13 → CREW-79)

**Root cause of the P3 incident**, observed by the maintainer in the worker's tab: the claude CLI
paused on its interactive *"allow access to this repository folder"* trust question — a first launch
in a brand-new project directory. Claude Code writes its session file only after that question is
answered, so no transcript carrying the nonce could appear within crew's 8-second discovery window.

`wait_for_readiness` (`adapter/tui/adapter.rs:1508-1567`) treats **any bytes** on the PTY as
readiness — a single `rx.recv()`, no content inspection — and then pastes the task prompt into
whatever has focus (`:1542-1545`). The submit path then waits only for output to go *quiet*
(`wait_for_output_idle`, `:1086`) — a static modal is quiet — and sends Enter (`:1093`).

So on a first launch in an untrusted directory, **crew pastes arbitrary text into a vendor security
prompt and presses Enter**, unaware, and journals none of it (`OutOfBandInput` carries no content).
No vendor file has any dialog, trust, or login awareness — zero hits across claude, codex, copilot
and omp — and no `TuiVendor` hook exists for it. No document mentions the trust prompt.

**Not established, and it should be answered deliberately rather than inferred:** what Claude's
dialog does with that Enter. The maintainer dismissed it by hand here, so this run does not tell us.
That is the difference between "a wasted run" and "crew can auto-accept a filesystem-trust prompt".

**The precedent that sets the priority.** This exact failure was diagnosed and fixed five weeks
earlier — for a different vendor, in test-only code.
`crates/runtime/src/adapter/tui/copilot_conformance.rs:913`, `ensure_copilot_workspace_trusted`,
describes it precisely ("blocks on a first-run trust modal that swallows the injected prompt, so
transcript discovery times out") and even tells the operator to run the CLI once and choose Trust.
Every reference to it lives inside that one file, reachable only from the conformance entry point:
**zero production call sites, for Copilot or any other vendor.**

**Erratum against this directory's own README.** `docs/compatibility.md:148` — and the README in this
directory — record the untrusted-workspace problem as *"since fixed"*. That is true of one vendor's
test harness and of nothing a user touches. This record supersedes that characterisation.

CREW-69's `question` field is the missing channel: it has two production call sites, both
post-discovery, both passing `None`. A third would have to be written at `fail_start`.

### 3. A parked run is declared `lost` after five minutes (F12 → CREW-80)

Run `01a08219` settled to `waitingUser` at 17:39:17 and was never touched. At 17:50:22, seq 249
recorded `workerTimeout kind=inactivity sinceMs=302380` and seq 250 recorded `runLost` — terminal,
while the vendor process was still alive (its exit was journaled only at 18:39:45, when its tab was
closed).

**This is deliberate.** `timeout_sweep.rs::settle_abandoned_turn` is a named ADR-0027 wave-3
exception, and the module says so: *"the runtime's 'journal, never decide' stance still holds for
everything except the one case the leader provably walked away from."* But `crew-orchestration`'s
skill and ADR-0025 both promise that a `WorkerTimeout` is a fact the leader acts on — extend, nudge,
or abort. **Whether the runtime should decide here is a product question for the maintainer**, not
something the code can settle.

**And the clock it rests on is corrupted by finding 4 below.** `activity.touch()` re-arms the
inactivity deadline from `RunLifecycleSink::emit` (`adapter/run_lifecycle.rs:540-544`) for *any*
journaled event except `ProcessExited` — `OutOfBandInput` included. On run `01a08219` the last
spurious echo landed at 17:45:20.480; 17:45:20.480 + 302.38s = 17:50:22.86, against a `workerTimeout`
at 17:50:22.894. **A match, not a correlation.** The "five minutes of inactivity" was measured from a
viewer echo, not from the turn that ended six minutes earlier. Here it delayed the auto-settle;
nothing in the design constrains the direction, so a differently-timed echo fires it *early*, against
a leader that was about to act.

### 4. A parked run floods the journal at ~30 rows/second (F7, F9, F16 → CREW-81)

Between 18:41:46 and 18:49:43 — the eight minutes run `01a08253` sat parked in a tab — the journal
grew from 706 to 15,338 events: **7,308 `outOfBandInput` plus 7,309 `runFlagsEvent`**, into an
append-only SQLite table with `synchronous=FULL`. P3's runs produced roughly one per second; this
was thirty times that. In the final export, 7,602 `runFlagsEvent` rows stand against 19 `runEvent`
rows.

`serve_viewer` (`crates/runtime/src/display/attach.rs:418-427`) calls `on_user_input(bytes)` on
**every socket read returning n>0** — no batching, no debounce, no size floor, no content filter.
Each read yields one durable `outOfBandInput` row plus one `set_run_flag(NeedsReconciliation)` write,
which is the observed 1:1 pairing. The viewer is `crewd attach`, run in the tab by the pane
coordinator (`crates/runtime/src/display/coordinator.rs:506-508`).

Every one of those writes queues through the single database actor: `run_domain_op` sends down a
bounded `mpsc` of capacity 32 (`crates/runtime/src/db/actor.rs:23,140`) to one owner thread. Viewer
telemetry therefore contends with run submissions, message sends and approvals, and scales with the
number of open panes rather than with work being done. No stall was observed; the contention is
structural.

**Standing hypothesis, not established:** with nobody typing, the bytes are a terminal answering the
vendor TUI's escape queries at redraw rate. Nobody could determine what writes with no human present
without running something. This should be confirmed before a fix is designed around it.

Relatedly, `AdapterVendorSessionEvent` is emitted per transcript entry carrying a `sessionId`
(`crates/runtime/src/adapter/tui/claude.rs:276-284`) — 41 identical rows for one session id on one
run. The repetition was anticipated in a comment (`adapter/tui/adapter.rs:1393-1394`) and never
bounded; deduping on change makes it the on-change event its name implies.

### 5. (Counted within 1) Fabricated success on the P4 truncation

Recorded under finding 1; noted here so the count of five P1 candidates reads correctly: F4 and F17
are one ticket, and the P4 recurrence is the same defect observed a second time.

---

## What worked, and should be said plainly

A record of a failed run is not a record that nothing works. Observed good on 2026-09-08:

- The daemon, `doctor` (15 checks passed), `health`, and state-root isolation — the default
  `~/.omp/crew` was untouched throughout, socket mode `srw-------`.
- **The dashboard auth gate.** `/`, `/?token=bad`, `/nonexistent` and `/api/runs` all returned `401`
  with no handler reached and no 404 leak; a valid token produced `303` to `/` with
  `Set-Cookie: crew_dashboard=…; Path=/; HttpOnly; SameSite=Strict`; the listener bound to
  `127.0.0.1:4747` only.
- **The CREW-74 display fallback did exactly its job**: herdr failed, tmux was unavailable, osWindow
  succeeded, and the downgrade was journaled with the full `attempted: [herdr, tmux, osWindow]`
  sequence and surfaced by the monitor. A pane in a non-preferred host *with* the event is the
  mechanism working.
- **The CREW-4 recorded-prompt comparison caught a real composer-level truncation** (3,150 of 18,409
  characters) and failed the start loudly. ADR-0030's progress bound did not miss this: it tracks
  bytes accepted at the PTY boundary and explicitly hands composer-side truncation to the
  transcript comparison, which worked.
- Prompt journaling on submit (ADR-0028), the result read on a settled turn, and the headline P3
  check: the run settled to `waitingUser`, stayed there under monitoring, and returned the correct
  answer.
- **The redaction boundary.** The export was swept and was clean; the dashboard token is absent from
  it.

---

## Findings and their tickets

| # | Finding | Ticket | Sev |
|---|---|---|---|
| F1 | Widget renders the empty-state box on an empty journal, contrary to D22 and two documents; no `/crew widget` toggle exists | CREW-77 | P3 |
| F4, F17 | Fabricated success; a start failure leaves no durable trace | CREW-78 | P1 |
| F10, F13 | Readiness accepts any bytes; paste-and-Enter into vendor dialogs; no dialog detection; the test-only Copilot trust helper | CREW-79 | P1 |
| F12 | Parked run declared `lost` at five minutes; clock re-armed by echoes | CREW-80 | P1 |
| F7, F9, F16 | Attach-socket echo storm; two fsync'd rows per read; vendor-session duplication | CREW-81 | P1 |
| F5 | herdr broken on 0.8.2; no version gate on the backend | CREW-82 | P2 |
| F14 | `[crew:<nonce>]` in the prompt trips vendor injection defences | CREW-83 | P2 |
| F15 | A retry never journals its prompt | CREW-84 | P2 |
| F11 | `crew_transcript` replay discards payload; no run filter | CREW-85 | P2 |
| F6 | Usage never implemented for any TUI adapter | CREW-86 | P3 |
| F2, F3 | Model ask bypassed when the leader supplies a model; `taskId` error message unhelpful | CREW-87 | P3 |
| F8 | Milestone digests need timestamps so late delivery reads as history | CREW-88 | P3 |

### Notes on two findings that are decisions, not just defects

**F14 and F13 are in tension and should be scoped together.** Crew appends `[crew:<nonce>]` to the
delivered prompt for transcript discovery, and the model reads it; the P4 worker refused its task
citing the tag. But `verify.rs` (CREW-13) locates the transcript entry *by* that nonce
(`adapter/tui/verify.rs:70`) in order to diff recorded against injected text — and that is the check
which caught the P4 truncation. **Relocating the nonce satisfies discovery cleanly and blinds the
detector that caught F13.** The cheaper fix: the worker refused an *unexplained* token, and nothing
anywhere — no skill, no rule, no document — says what `[crew:<nonce>]` is. A self-describing tag,
documented in a skill, addresses the refusal without touching either consumer. Evaluated on the tag
alone, the worker's refusal was correct behaviour.

**The P4 instruction shape contributed independently.** A document followed by an instruction reads
as injection to a safety-trained model. That is a test-design lesson for the runbook, not a product
defect, and it should not be folded into F14.

**Journal content is untrusted input.** During diagnosis, a sub-agent flagged an apparent
prompt-injection attempt inside the journal export. It was P4's own test prompt, and the sub-agent
correctly treated it as data. The general point stands for anything that reads journals, including
future diagnosis: nothing marks journal content as untrusted, and it contains whatever a prompt
contained.

---

## Provenance

Observations are the run's own log, recorded live as the run proceeded. Code claims were verified at
the cited lines by two reviewers independently, with load-bearing claims spot-checked a third time.
Where something could not be established from a read-only position — what Claude's dialog does with
an Enter, what writes to the attach socket with no human present, whether the readiness race explains
the truncation — this record says so rather than inferring.

Two corrections made during diagnosis are recorded because they changed conclusions: the belief that
fixing orchestration's error path would remove the fabricated success (it would not — the terminal
state is committed inside `start()`), and the belief that the P4 export was the wrong window (it was
not — start failures are not journaled).
