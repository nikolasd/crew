# A parked run is settled on its leader's connection actually being gone, never on silence alone

* Status: Accepted
* Date: 2026-09-09
* Amends: [0027](0027-turn-end-settles-a-run.md)

## Context and Problem Statement

ADR-0027's wave 3 gave the inactivity sweep a backstop: a run parked in `waitingUser` with
`turnSettled` set, quiet past the inactivity deadline, was settled to `lost`. That was written when
the only failure this daemon could observe was silence, and it conflated two genuinely different
situations under one signal: a worker gone quiet while its leader is still there, still watching,
still free to steer or finish it; and a leader that has itself vanished, leaving a parked run nobody
will ever finish or retry. Silence looks identical from the runtime's side in both cases, but only
the second one is actually abandoned.

The maintainer ruled the backstop out on exactly that basis: silence alone is never evidence a leader
gave up, only the leader's own connection actually being gone is. Acting on that ruling required a
signal this daemon did not yet track — leader *connection* identity as its own liveness concern,
independent of the worker/vendor process activity `ActivityClock` already measures. It also surfaced
a second, un-mentioned failure mode once the first was fixed: an in-memory notion of "who is
connected" starts empty on every daemon restart regardless of what was true a moment before, so
naively treating "no live connection recorded yet" as "gone" would settle every parked run on every
restart — a regression far worse than the backstop this decision removes.

## Decision Drivers

* Never settle a run because it was quiet; settle it only because its owning leader is provably gone.
* Never strand a run forever either — an abandoned leader is not something a human will notice and
  cancel by hand.
* Reuse `run_cancel`'s existing settle mechanism rather than invent a second one: the ordering and
  broadcast invariants ADR-0020 requires were already solved once, and a second, parallel
  implementation is a second place they could quietly diverge.
* A daemon restart is not a leader event. The daemon's own downtime must never be charged against a
  leader's grace window, or a long-stopped daemon would settle every reconnecting leader's runs the
  moment it came back up.
* More than one physical connection can legitimately share one leader instance id (the
  `crew-extension` fallback id, used by any leader session with no real session id of its own), so
  correctness cannot assume one instance id maps to one connection.

## Considered Options

* **Keep the inactivity backstop.** Rejected outright by the maintainer's ruling above — it conflates
  worker silence with leader abandonment, which are not the same fact.
* **Track leader liveness as a boolean or a set of connected ids.** Fails the shared-instance-id case
  (a second physical connection under the same id would overwrite or be indistinguishable from the
  first) and both reconnect-race orderings (a fresh connect racing a stale disconnect can land either
  order).
* **A bare refcount, re-checked at `count == 0` when a timer wakes.** Handles the shared-id and
  race-ordering cases, but has a real bug: a leader that disconnects, reconnects, and disconnects
  again — all within one grace window — can leave a *stale* timer (armed for the first disconnect)
  waking after the window and reading `count == 0`, which is true, but for the *second* disconnect,
  not the one the timer was spawned to watch. It would settle a leader that has only been gone a
  fraction of the configured window.
* **A refcounted registry recording the instant of each true 1→0 transition.** Chosen — see below.

## Decision Outcome

Chosen option: a refcounted `LeaderRegistry`, keyed by leader instance id, living in
`ipc::server::Shared` beside the existing `active_connections` counter but populated from inside
`ipc::connection::handle` rather than `Server::admit` — the negotiated instance id does not exist
until the `initialize` handshake completes, deep inside `handle`, which `admit`'s spawn wrapper never
sees.

Each entry is `{count, zero_since}`, not a bare count. `zero_since` records the instant of the
entry's most recent 1→0 transition and is cleared on any 0→1 — the property that closes the bare-
refcount bug above: a grace-window timer waking later can tell "the same zero it was spawned to
watch" apart from "a fresher zero from an intervening reconnect/disconnect cycle" (see
`ipc::leader_registry::Entry::zero_since`'s own doc comment for the worked timeline). The disconnect
grace window itself is 60 seconds (`LEADER_DISCONNECT_GRACE_WINDOW`): short enough that an abandoned
run does not tie up a concurrency slot indefinitely, long enough to absorb an ordinary restart or
reconnect blip — the case this must never fire for. The decrement that can bring an entry to zero is
a drop guard (`LeaderConnectionGuard`), not a statement paired by hand at each exit from `handle`'s
dispatch loop: that loop's decrement needs the instance id, which only exists once execution is
already inside it, so unlike `active_connections` it cannot be bookkept from the spawn wrapper, and a
future early `return` would otherwise leak the registration silently, failing this feature closed
with nothing to notice. The guard's own `on_gone` callback fires exactly on the drop that causes the
true 1→0 transition, arming the grace-window timer from inside `Drop::drop` itself, so arming is as
structural as the decrement it rides on.

Teardown reuses `run_cancel`'s own mechanical shape rather than a second implementation of it,
factored into a shared `cancel_and_settle_run` helper parameterized on the target `RunState`:
`run_cancel` itself still asks for the literal `"cancelled"`, checked against the requesting
principal; the grace-window path asks for `RunState::unrendered_verdict()` and passes no principal,
bypassing the ownership check the same way the removed backstop did. `unrendered_verdict()` is the
one function that owns the word "cancelled" for an evidence-free settlement, so this path and the
process-exit-after-settled-turn path that already called it can never drift on what word they use.
Before settling, a `WorkerTimeout { kind: LeaderGone }` fact is journaled first — the same
report-before-decide shape the inactivity sweep already uses for `Inactivity`/`Total` — so the
journal always states the reason before the state changes because of it.

Removed entirely: the inactivity sweep's `settle_abandoned_turn` path (ADR-0027 wave 3). A parked run
whose leader is still connected is never settled by the runtime now, however long it stays silent —
the inactivity fact is still journaled, for `run/timeoutAck` to react to, but it no longer drives a
state change on its own. `lost` returns to ADR-0023's original, narrower meaning: the evidence itself
is missing or ambiguous, never "the leader went quiet."

**Daemon restart.** At startup, nothing has connected to the freshly constructed `LeaderRegistry`
yet, so every leader that owns a non-terminal run is — from this process's own perspective —
disconnected as of right now. Each such owner is seeded with `zero_since` set to the daemon's own
startup instant, and the same grace-window timer is armed for it as a live disconnect would get. A
leader that reconnects within the window clears it exactly as an ordinary reconnect does; one that
never returns gets its runs settled through the identical path a live disconnect uses. The clock
deliberately starts at the daemon's *own* startup instant, never at a run's last-seen activity: a
leader cannot reconnect to a daemon that is not running, so any time the daemon spent stopped is the
daemon's own absence, not the leader's, and must never be charged against the window — doing
otherwise would settle live, reconnecting leaders' runs on every restart that followed a stop longer
than the window, which is exactly the destructive direction this feature exists to avoid.

**Shared-instance-id hazard.** The `"crew-extension"` fallback id — used by any leader session with
no real session id of its own — means more than one physical leader process can share a single
registry entry. Refcounting keeps the *mechanism* safe under that sharing: both connect/disconnect
orderings resolve correctly by construction (plain addition does not care which side's `+1`/`-1` is
observed first), and the entry only reads "gone" once every sharer has disconnected. It does not
resolve the pre-existing ownership ambiguity the shared id already carries elsewhere in this
protocol — which of several processes sharing the id actually "owns" a given run — and this decision
inherits that ambiguity rather than creating or fixing it.

### Positive Consequences

* A run is never settled on a worker's silence alone; only a leader's connection actually being gone,
  for the full grace window, can do that — closing exactly the gap the maintainer's ruling named.
* An abandoned leader's runs are no longer stranded forever: they settle within one grace window of
  the leader's last connection actually going away, restart included.
* The settle path shares its mechanical shape (and its ordering/broadcast guarantees) with
  `run_cancel`, so a future change to that shape cannot fix one call site and silently miss the
  other.
* `lost` regains ADR-0023's original single meaning, removing the two-meanings-under-one-flag
  ambiguity ADR-0027 wave 3 introduced for it.

### Negative Consequences

* A leader whose connection drops for reasons other than actually leaving (a long GC pause, a flaky
  local socket) and does not reconnect within 60 seconds has its parked runs settled the same as a
  leader that genuinely gave up — the grace window trades a false negative (settling too early) for
  eliminating the false positive (settling on silence) the backstop had; it does not eliminate every
  failure mode.
* The in-memory registry adds real state to reason about across a restart, which is why the
  restart-seeding half of this decision exists at all; a bug in that seeding has the same destructive
  shape (settling live runs) as the failure mode this whole decision exists to prevent.
* The shared-instance-id ownership ambiguity named above is now load-bearing for a second mechanism
  (grace-window teardown) beyond whatever already depended on it; a future fix to that ambiguity must
  account for this decision's use of it too.

## Links

* Amends [0027](0027-turn-end-settles-a-run.md), whose Decision point 3 is split by this ADR: the
  backstop-settles-on-silence half is superseded here; the "never `succeeded`" half is inherited
  unamended (a grace-window settlement is `RunState::unrendered_verdict()`, never `succeeded`,
  exactly as the backstop it replaces never was).
* Related: [ADR-0023](0023-run-state-edges-from-adapter-evidence.md) (`lost`'s original, narrower
  meaning, restored here), [ADR-0016](0016-coordination-scope-tokens-bound-to-run-and-pid-ancestry.md)
  (the same "reuse a settle mechanism rather than parallel it" reasoning, applied there to
  coordination scope and here to run teardown), [ADR-0020](0020-per-mutation-event-broadcast-is-not-optional.md)
  (the broadcast-in-the-same-call invariant this decision's teardown path inherits from
  `cancel_and_settle_run`).
* Implementation: `crates/runtime/src/ipc/leader_registry.rs` (the registry and its drop guard),
  `crates/runtime/src/service/orchestration.rs` (`settle_leader_gone`,
  `seed_leader_registry_after_restart`, `cancel_and_settle_run`), `crates/runtime/src/timeout_sweep.rs`
  (the backstop's removal).
