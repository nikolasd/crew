# Injectable `RunDriver` seam, fake by default

* Status: Accepted
* Date: 2026-07-24

## Context and Problem Statement

The orchestration extension needed to ship a complete vertical slice — task/worker/run records,
RPC methods, OMP tools, and a monitor — before any real worker adapter (Claude, Codex, Copilot,
OMP-RPC) existed; those ship in a later milestone entirely. Should `run/submit` block on adapters
existing, or is there a way to prove the whole surface works without them?

## Decision Drivers

* Adapters are explicitly out of scope for this milestone; blocking on them would block the entire
  milestone.
* `run/submit` still needs a real, tested state transition path (`queued -> starting -> working`)
  to prove the lifecycle machinery (ADR-0012) actually works end to end, not just in isolation.
* Production, with no adapter registry wired up, must have a real, documented, non-crashing
  behavior — not a stub that lies about success.

## Considered Options

* A `RunDriver` trait (`start(ctx) -> Future<Result<(), String>>`) injected as
  `Option<Arc<dyn RunDriver>>` on `OrchestrationService`; `None` by default in production, with
  `run/submit` returning `adapter_unavailable` *after* durably committing the queued run;
  `FakeRunDriver` (drives the real transitions through the real domain repository) injected only
  by tests.
* Block this milestone entirely until the Worker Adapters plan lands a real driver.
* Ship a stub adapter that unconditionally reports success without doing anything.

## Decision Outcome

Chosen option: the injectable seam with a fake, tested implementation and an honest `None`
default. `run/submit` always durably commits the run as `queued` first; only afterward does it
attempt to start a driver, and a missing driver produces a real, typed
`adapter_unavailable` error while the queued run stays exactly where it is — never dropped, never
silently marked as started.

### Positive Consequences

* The entire orchestration vertical slice (protocol, persistence, RPC, tools, monitor) shipped and
  is fully tested against `FakeRunDriver` without waiting on a single real adapter.
* The Worker Adapters milestone has an exact, already-tested contract to implement against
  (`RunDriver::start`) with zero required changes to `run/submit`'s own logic.
* "No adapter wired up" is a first-class, documented, tested outcome — not a crash, not a lie, not
  an undefined state.

### Negative Consequences

* Until adapters ship, every real `run/submit` against a production daemon returns
  `adapter_unavailable` — which is correct but must be clearly documented (and is: see
  `development.md`'s smoke-testing walkthrough) so it isn't mistaken for a bug during the
  Orchestration Extension milestone's own review.

## Pros and Cons of the Options

### Injectable seam, fake by default (chosen)

* Good, because it decouples "does the lifecycle/persistence/RPC/UI surface work" from "does a
  real adapter exist," letting both be developed and tested independently.
* Bad, because production behavior without a driver (`adapter_unavailable` forever) needs explicit
  documentation to avoid looking broken.

### Block on real adapters

* Good, because it would avoid ever shipping a "fake" anything.
* Bad, because it would have delayed the entire orchestration milestone — task records, RPC
  methods, tools, and the monitor — behind a dependency (real adapters) that belongs to a
  different milestone entirely.

### Always-succeeding stub adapter

* Good, because it requires the least code.
* Bad, because it would make `run/submit` lie: a run reported as `working` with no process
  actually running behind it is exactly the kind of fabricated evidence ADR-0012 exists to
  prevent.

## Links

* Narrated in `../journal.md`, commit `c468073`
* Depends on [ADR-0012](0012-explicit-run-lifecycle-relation-runtime-evidence-only.md)
