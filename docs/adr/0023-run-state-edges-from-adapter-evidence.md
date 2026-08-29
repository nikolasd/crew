# Run-state edges derive from adapter evidence; an unobservable exit is `lost`

* Status: Accepted, amended by [0027](0027-turn-end-settles-a-run.md)
* Date: 2026-08-16

## Context and Problem Statement

ADR-0012 defined the run-lifecycle relation (`RunState::can_transition_to`) and the rule that only
runtime evidence may drive it. ADR-0013 shipped `FakeRunDriver` as the relation's only
implementation, satisfying tests but nothing else. The real `AdapterRegistry` — the production
`RunDriver` wired by `lifecycle::serve()` — never called `transition_run` anywhere in `run_one` or
`watch_settlement`; the only production call sites were `run/cancel`, the approval service's
`working <-> waitingUser` toggle, the policy-violation service, and the boot recovery sweep. Every
real run's row therefore stayed `queued` however successfully its vendor process ran and exited;
`run/get`, `run/list`, the `/batman` monitor, and the approval flow all read a value that was wrong
for every real run, and only a daemon restart (`RecoveryCoordinator`) ever terminalized anything
(REVIEW.md R69). How should the adapter layer apply the edges its own journaled evidence already
proves happened, without duplicating or contradicting the existing legal-edge table or the existing
terminalization paths?

## Decision Drivers

* ADR-0012's relation and its illegal-edge table must stay the single source of truth — no second,
  looser check anywhere in the adapter layer.
* `started_at` is stamped only on the `starting` edge (`DomainRepository::transition_run`); skipping
  that edge on the way to `working` would silently lose it.
* The settlement signal that releases a run's concurrency slot (`SettlementSink`) must never fire
  before the terminal state it corresponds to is durable, or another run could be authorized while
  this one still reads non-terminal.
* `working` must never be applied to a run sitting in `waitingUser` or `paused` — vendor output
  arriving mid-approval must not silently reopen it.
* A cancelled run's row is already terminal by the time its process exit arrives; that terminal
  state must win over whatever the exit status says.

## Considered Options

* **Apply edges from adapter evidence, in the per-run event sink, walking every intermediate hop.**
  Wrap each run's `AdapterEventSink` in a `RunLifecycleSink` that, after the inner sink journals an
  event, computes the edge that event is evidence of and commits (and broadcasts) it —
  `ProcessStarted` -> `starting`, any other non-exit payload -> `working`, `ProcessExited` ->
  `succeeded`/`failed`/`lost` depending on exit code and signal. When the legal-edge table forces an
  intermediate stop (e.g. `queued -> working` is illegal), walk `starting` first, then `working`,
  so the table and `started_at` stamping stay authoritative regardless of the target.
* **Transition inside `registry::watch_settlement`, at settlement time.** Simpler to locate (one
  function already watches every run to completion) but the settlement signal fires immediately
  after this function observes completion, which would release the concurrency slot before the
  terminal state is durable — reintroducing the race the drivers above rule out.
* **Map an unobservable exit (no code, no signal) to `succeeded` or `failed`.** Simpler for callers
  that only expect two terminal values, but fabricates a fact the runtime never actually observed;
  ADR-0015 already established the precedent of naming uncertainty (`lost`) rather than guessing.
* **Make `working` reachable from any non-terminal state.** Would remove the need to special-case
  `waitingUser`/`paused`, but would let a stray vendor-output event silently pull a run out of an
  approval wait or a pause.

## Decision Outcome

Chosen option: apply edges from adapter evidence inside the per-run sink, walking every
intermediate hop, with the terminal edge committed before the settlement signal that releases the
concurrency slot. `RunLifecycleSink::wrap` sits between `DomainAdapterEventSink` and the adapter,
so every journaled `AdapterEvent` is also evidence for exactly one lifecycle edge:

| evidence | edge |
|---|---|
| `ProcessStarted` | `queued -> starting` |
| any other payload except `ProcessExited` | up to `working` |
| `ProcessExited { exit_code: Some(0), signal: None }` | `-> succeeded` |
| `ProcessExited` with a non-zero code or a signal | `-> failed` |
| `ProcessExited` with no code and no signal | `-> lost` |

Edges are forward-only (`working` only applies from `queued`/`starting`) and a terminal state
always wins — every walk stops the moment it observes a terminal row, and `transition_run` itself
rejects an illegal edge and appends nothing even if a concurrent commit wins the race. An
unobservable exit is `lost`, naming the runtime's uncertainty rather than guessing success or
failure.

### Positive Consequences

* Every real run now leaves `queued` as its vendor process actually makes progress; `run/get`,
  `run/list`, the `/batman` monitor, and the approval flow read a value that means what it says,
  closing REVIEW.md's only Critical item.
* The terminal edge is durable before the settlement signal fires, so no other run can be
  authorized while this one still reads non-terminal.
* No second legal-edge table: every hop this sink walks is still checked by
  `RunState::can_transition_to`, so ADR-0012's relation stays the only source of truth.

### Negative Consequences

* Fixing this exposed two secondary defects that had to move with it: Copilot's
  `CopilotClientEvent::ProcessExited` previously carried no exit status at all (now
  `exit_code`/`signal` are real), and a violation-cancel ordering race in the policy-violation
  service. Both are covered by this change's tests; R12 (Claude error-result subtypes) and R13
  (violation-cancel's warning not distinguishing "no running adapter" from a kill failure) remain
  open and untouched.
* A run's lifecycle now depends on the same evidence walk producing exactly one terminal edge per
  run; any future adapter that journals `ProcessExited` more than once, or out of order relative to
  a cancellation, must be audited against this sink's forward-only/terminal-wins guarantees.

## Pros and Cons of the Options

### Apply edges from adapter evidence in the per-run sink (chosen)

* Good, because the terminal edge lands before the concurrency-slot-releasing settlement signal.
* Good, because it reuses ADR-0012's relation and `DomainRepository::transition_run` verbatim — no
  parallel state machine.
* Bad, because every adapter's evidence stream must be trusted to journal `ProcessExited` exactly
  once per run.

### Transition inside `watch_settlement`

* Good, because it is the one place that already watches every run to completion.
* Bad, because the settlement signal it emits immediately after would race the durability of the
  terminal state, letting another run get authorized too early.

### Map an unobservable exit to a guessed `succeeded`/`failed`

* Good, because callers would only ever see two terminal values.
* Bad, because it fabricates a fact — exactly the guessing ADR-0015 already rejected in favor of
  naming uncertainty.

### Make `working` reachable from any state

* Good, because it would need no `waitingUser`/`paused` special-casing.
* Bad, because a stray vendor-output event could then silently reopen an approval wait or a pause.

## Links

* Narrated in `../journal.md`, Part XVI
* Implements the evidence-only rule from [ADR-0011](0011-omp-retains-task-graph-authority.md)
* Applies the relation defined by
  [ADR-0012](0012-explicit-run-lifecycle-relation-runtime-evidence-only.md)
* Replaces `FakeRunDriver` (the only prior implementer) named in
  [ADR-0013](0013-injectable-run-driver-seam-fake-by-default.md)
* Reuses the "name uncertainty, don't guess" precedent from
  [ADR-0015](0015-omp-native-facts-as-non-owning-mirror-lost-on-omission.md)
* Commits alongside the per-mutation broadcast invariant from
  [ADR-0020](0020-per-mutation-event-broadcast-is-not-optional.md)
