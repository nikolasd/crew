# A run is a conversation the leader closes; a vendor's turn-end is durable evidence, not a terminal state

* Status: Accepted
* Date: 2026-08-29
* Amends: [0023](0023-run-state-edges-from-adapter-evidence.md)

## Context and Problem Statement

ADR-0023 established that a run's durable `RunState` follows adapter evidence, and gave the evidence
table exactly one terminal driver: `ProcessExited`. That was correct when every adapter exited once
per turn. ADR-0025 made the TUI path primary and ADR-0026 retired the headless path without
revisiting the table, and a TUI vendor never exits — it returns to its prompt and waits.

The consequence is that no TUI run can reach a terminal state on its own. Every terminal consumer
degrades with it: `run/result` hard-refuses a non-terminal run, `run/retry` requires a terminal prior
run, and `watch_settlement` — which releases the concurrency slot — only fires on process exit, so a
finished-but-alive worker pins its slot indefinitely. The first live end-to-end test of the plugin
had to cancel every run in order to read its answer; the limitation was already documented in
`docs/manual-testing.md` as known.

The evidence needed to fix this is already present and already discarded. Codex maps `task_complete`
and Copilot maps `assistant.turn_end` to `TuiEvent::TurnEnded`, and the adapter's own handler for
that event is an empty match arm. Claude was believed to have no equivalent signal; it does. Its
JSONL transcript carries `message.stop_reason` on every assistant entry, where `end_turn` and
`stop_sequence` mean the turn is over and `tool_use` means it continues. Measured across the 25 most
recent transcripts on a development machine: 4426 assistant entries, none with a null or absent
`stop_reason`. The Claude transcript format reads `/message/content` and never looks at it.

That measurement removes the reason the earlier (out-of-repo, since mooted) plan needed a heuristic
quiet window for Claude. All three vendors expose a deterministic turn boundary.

## Decision Drivers

* Reading a finished run's answer must not require cancelling it. This is the reported pain, and any
  design that leaves it unfixed has failed regardless of its other merits.
* A vendor's turn boundary says the *turn* ended. It does not say the *task* succeeded — Codex's
  marker is literally `task_complete` whatever the outcome. ADR-0015 and ADR-0023 both established
  that this codebase names uncertainty (`lost`) rather than fabricating a fact it never observed.
* Follow-up steering is the entire reason the TUI vendors are kept alive after their answer. A design
  that ends the run at the first turn boundary forecloses it.
* Run terminal state and vendor session lifetime are currently the *same* event. `watch_settlement`
  evicts the adapter, disposes it, releases the concurrency slot, and emits `DisplayPaneDetached` in
  one place. Any change that separates "the turn is done" from "the session is gone" must decide,
  explicitly, what happens to each of those four things.
* ADR-0020's invariant is not negotiable: every domain mutation commits its event and broadcasts the
  same envelope in the same call. ADR-0023's ordering is not negotiable either: the terminal edge is
  durable *before* the settlement signal that releases the concurrency slot.
* OMP owns the task graph (ADR-0025). The runtime does not decide when work is finished.

## Considered Options

* **Auto-complete on turn-end.** Map `TurnEnded` to a terminal edge in `RunLifecycleSink`, where
  `ProcessExited` is handled today, leaving the process and pane alive.
* **An explicit `run/finish` RPC, leader-driven.** The leader reads the output and settles the run
  when satisfied.
* **Hybrid.** `TurnEnded` becomes durable, broadcast evidence driving a *non-terminal* state; the
  leader settles the run explicitly; the runtime settles it on its own only as a backstop.

### Auto-complete on turn-end

Requires no protocol change, no leader cooperation, and works for a run nobody is watching. It also
inherits ADR-0023's ordering property for free, since the edge would be applied in the same
`RunLifecycleSink::emit` that already sequences the terminal commit ahead of the settlement signal.

Against it: it claims `succeeded` on evidence that only says the turn ended; it forecloses multi-turn
steering, because the first turn boundary ends the run; and it silently changes three behaviours that
deserve explicit decisions — the concurrency slot's meaning, `pane/reopen`'s refusal of terminal runs
(which would make every finished worker's pane un-reopenable, at exactly the moment someone wants to
look at it), and the fact that a still-alive process keeps appending residue to a run whose result has
already been read.

### An explicit `run/finish` RPC

Semantically honest — the leader is the only party that knows whether the task is done — and it keeps
multi-turn runs natural while leaving the settlement seam untouched, since finish can tear down
exactly as `run/cancel` already does.

Against it: it adds protocol surface, tool plumbing and skill documentation, and it does not fix the
reported symptom until an OMP-side change ships in lockstep. On its own it also reintroduces the
current bug under a nicer name — a leader that dies or loses interest leaves runs `working` forever.

### Hybrid

Turn-end is journaled and broadcast as evidence, driving `working -> waitingUser` — an
already-legal, already-modelled non-terminal state meaning "the runtime is not the blocker", which
`next_hop` already routes through. `run/result` becomes readable for a run in `waitingUser` that has
at least one turn-end. The leader settles explicitly via `run/finish`; an inactivity backstop settles
an abandoned run without one.

Against it: it is two-phase rather than one change, and it borrows `waitingUser`, whose existing
meaning in the monitor is "the worker asked a question" — so the two need to be distinguishable.

## Decision Outcome

Chosen option: **the hybrid**, delivered in two waves. The maintainer's answers to the four open
questions this design turned on are recorded here as part of the decision:

1. **A run is a conversation the leader closes**, not a single turn. This is the load-bearing answer;
   everything below follows from it. It is why turn-end is evidence rather than a terminal edge.
2. **`waitingUser` is reused, with a distinguishing run flag** — no new state and no migration. The
   monitor must be able to tell "turn done, leader's move" from "the worker asked a question".
3. **The inactivity backstop may settle a run the leader never finished, to `lost`, never
   `succeeded`.** Result text stays readable. A success reached by silence would be a fabricated
   fact, which ADR-0015's precedent forbids.
4. **The concurrency slot means "an active turn"**, so it is released at turn-end. The pane
   coordinator gains its own cap on live idle sessions, so open TUIs cannot pile up unbounded.

### Wave 2 — no protocol-breaking change

* `TuiEvent::TurnEnded` is mapped on all three vendors. Claude's mapping reads
  `/message/stop_reason`, treating `end_turn` and `stop_sequence` as the boundary and `tool_use` as
  continuation. Two guards: an entry with `isSidechain` is a subagent's turn and must never settle the
  parent run, and an entry carrying `isApiErrorMessage` is a failed turn rather than a completed one.
* The event is journaled and broadcast in the same call, per ADR-0020.
* The `working -> waitingUser` edge is applied inside `RunLifecycleSink`, where `ProcessExited` is
  handled, so ADR-0023's commit-before-settlement ordering is preserved by construction rather than
  by a new argument.
* `run/result` reads a run in `waitingUser` that has at least one turn-end. **The fold boundary is
  the first turn-end at or after the run's start**, so a later turn's residue can never silently
  rewrite an answer the leader has already read; a leader wanting a later turn asks for it
  explicitly.

### Wave 3 — after wave 2 and the pane-attach fixes land

* `run/finish`, shaped exactly like `run_cancel`: a guarded `transition_run` in one database-actor
  closure, the committed envelope broadcast in the same call, side effects afterward, and
  `degradedControl` set if tearing down a live vendor process fails. It differs from cancel in one
  way: `cancelled` is a legal edge from every non-terminal state, but `waitingUser -> succeeded` is
  not, and `waitingUser` is exactly where a settled turn parks — so finish *walks* through `working`,
  one guarded commit and broadcast per hop, rather than widening ADR-0012's relation to make the hop
  legal. `outcome` is stated by the leader (`succeeded` or `failed`), never inferred from the
  vendor's turn markers.
* The inactivity backstop settling to `lost`, gated on `waitingUser` **and** `turnSettled` together
  in one transaction. That pairing is the safety property: a `waitingUser` that means "the worker
  asked a question", an approval wait, a `waitingPeer` and a `paused` run must all survive a timeout
  untouched. It is also the second thing the `turnSettled` column buys beyond snapshot readability —
  without it there is no way to distinguish an abandoned answer from a run that is merely quiet.
* `pane/reopen`'s gate re-keyed from run state to pane liveness, and the stale-socket sweep
  (CREW-15). One definition of "live pane" — a connect probe, the only portable proof of a listener —
  shared by the gate and the sweep. The sweep is keyed on liveness and never on ownership, because
  the pane directory is per-*user* and shared across every repository, so another repository's live
  sockets sit beside this daemon's dead ones and must survive.
* Two caps, deliberately separate (see "The concurrency bound" below).

### The concurrency bound

The concurrency slot is released at the first of a turn boundary or a process exit, so the ceiling
means "actively taking a turn" rather than "has a session open". A follow-up turn on an existing run
is **never refused**: it is admitted on that run's own burst allowance, which cannot stack because a
run has one PTY and one turn at a time. Such an admission is journaled as evidence, so burst usage is
visible rather than discovered by finding more turns running than the ceiling allows.

That leaves a bound to state honestly, because releasing the slot at turn-end **removes** the bound
that exists today. Today a run pins its slot forever, so live runs never exceed the ceiling, and that
accident is what currently bounds concurrent turns. Once the slot is released, a leader can
accumulate arbitrarily many parked runs — each admitted singly under the ceiling — and then steer
them all at once.

So a second, independent cap is required: `limits.maxLiveSessions` bounds the sessions that *exist*,
including those parked between turns, enforced at new-run admission in the adapter registry with a
typed refusal. The resulting bound is:

> concurrent active turns ≤ `limits.maxConcurrentWorkers` + `limits.maxLiveSessions`

Default `maxLiveSessions` is 16, four times the default ceiling of 4: parked sessions are the normal
resting state for TUI workers, so the cap has to leave room for several finished-but-steerable runs
per actively-working one while still bounding a session that never closes anything. It is clamped up
to at least the ceiling, since a smaller value would refuse new runs before the ceiling ever applied.

**The pane cap is not the concurrency bound**, and it is worth being explicit because it looks like
one. `PaneCoordinator`'s cap bounds panes — screen real estate — and a run resolving to `hidden`
occupies no screen, so it neither consumes that cap nor is refused by it. A hidden-display setup
therefore has no pane cap at all, which would leave concurrent turns unbounded if the pane cap were
doing this job. The resource worth bounding for concurrency is the vendor process and its PTY, which
exists whether or not the user wants windows — hence the session cap living in the registry, where
live sessions are actually tracked.

The refusal lands on `run/submit`, not on `message/send`: declining to create the N+1th worker is the
same shape of refusal the concurrency ceiling already has, whereas failing to steer a worker that
already exists would be a new and worse failure mode.

### Positive Consequences

* Reading a finished answer no longer requires cancelling the run, which removes the
  `docs/manual-testing.md` limitation and the workaround the first live test had to invent.
* Multi-turn steering survives, so keeping the vendor process alive continues to buy something.
* No run ever reports `succeeded` on evidence that only proves a turn boundary.
* The three consequences of splitting run state from session lifetime become deliberate wave-3
  decisions instead of side effects of a one-line change.
* `run/retry`'s invariant is untouched: a retry still always creates a new run inheriting the prior
  run's `TaskId`.

### Negative Consequences

* A run still needs an explicit settle (or the backstop) to reach a terminal state; wave 2 fixes the
  blocking problem — reading the result — but not that.
* `waitingUser` now carries two meanings separated only by a flag. A consumer that reads the state
  and ignores the flag will mis-describe a finished turn as a pending question.
* Retrying a run whose vendor process is still alive would spawn a second process against one vendor
  session. Under this design `run/finish` tears the session down before the run becomes retryable, so
  the hazard is contained rather than fixed; if a path to retry-while-alive appears, it must resume
  the live session or refuse with a typed error, never double-spawn silently.
* The timeout sweep's "journal only, never change state" property (WP19) is deliberately broken by
  the wave-3 backstop. That was written when a stuck run was still visibly `working`; once a settled
  turn is distinguishable, silence is actionable evidence rather than an unexplained gap.

## Amendment (2026-08-31, CREW-49)

Wave 2's fold boundary ("the first turn-end at or after the run's start") was too literal. Live
evidence (CREW-48): the vendor emitted `end_turn` twice for one answer, the first entry's content
entirely a `thinking` block. CREW-48's own content guard on the turn-boundary detector now excludes
that case outright -- a thinking-only entry is not a boundary at all, so it stops being a problem
here too. But the guard deliberately still admits a turn that ends having produced only *tool*
activity (a `tool_use` block, no `text`) as a real boundary, since that turn genuinely did end. This
case is not the one observed live; it is the one the guard's own design leaves reachable, and this
fold has to handle it on that basis: a run whose first turn is tool-only, with the actual answer
written in a second turn (reached via CREW-47's resumption paths), would have `run/result` read the
first, empty boundary and report `resultText: null` for a run that, moments later, plainly has an
answer.

The fold boundary is now **the first turn-end that already has some result text accumulated before
it**, not the first turn-end outright (`query::run_result_events_op`). A content-free boundary is
skipped past rather than stopped at. This does not weaken the property the original boundary
protects: skipping an empty turn can never silently rewrite an answer the leader has already read,
because there was no answer at that boundary to protect in the first place. A boundary that does
carry text is still exactly where this stops -- a leader wanting a turn beyond that one still has to
ask for it explicitly, unchanged from wave 2's own guarantee.

The usage fold is unaffected by this change in shape (it already accumulates across every event the
scan visits, tool-only turns included); the amendment only changes which turn's *text* is reported,
never how spend is totalled.
