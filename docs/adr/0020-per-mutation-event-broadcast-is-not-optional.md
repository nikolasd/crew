# Per-mutation event broadcast is not optional

* Status: Accepted
* Date: 2026-07-24

## Context and Problem Statement

The embedded monitor's live-update half (ADR-0019) depends entirely on every domain mutation
notifying live `events/subscribe` listeners the moment it commits. That invariant was violated for
one commit: `Shared.events_tx` had a subscriber (`spawn_subscription`) but no publisher anywhere —
none of the fifteen-plus mutation call sites across `OrchestrationService`, `ApprovalService`,
`CoordinationBroker`, and `RunDriverContext` ever called `.send()` on it. The compiler cannot catch
"this mutation appended an event but forgot to broadcast it" — so where, mechanically, should
broadcasting happen, and how is the rule made durable enough that it doesn't quietly regress again
the next time someone adds a mutation?

## Decision Drivers

* Broadcasting must happen for *every* mutation, with no exceptions, or the monitor (and any
  future live consumer) silently falls out of sync for exactly the mutations someone forgot.
* `DomainClosure` (the boxed closure `run_domain_op` executes on the actor thread) is constrained
  to return a plain `serde_json::Value` — changing that signature ripples through every existing
  call site regardless of which broadcasting design is chosen.
* Whatever the mechanism, it needs a concrete, repeatable pattern a reviewer can check for at a
  glance on every new mutation.

## Considered Options

* Broadcast from the async service layer (`OrchestrationService`, `ApprovalService`,
  `CoordinationBroker`), each holding its own `events_tx: broadcast::Sender<EventEnvelope>`.
  `Committed` gained an `envelope` field; `domain::{embed_envelope, take_envelope}` smuggle that
  envelope across the `run_domain_op` `Value` boundary, so each service calls
  `self.broadcast(&mut result)` immediately after `.await`, before building its JSON-RPC response.
* Broadcast from inside the DB actor thread itself, immediately after the transaction commits —
  requiring `DomainClosure`'s return type to change (e.g., to `(Value, Vec<EventEnvelope>)`) and
  `events_tx` to be threaded into `DatabaseHandle::start` and its one call site.
* A separate poller periodically diffing the `events` table and broadcasting whatever's new since
  its last scan.

## Decision Outcome

Chosen option: broadcast from the async service layer. This kept `DatabaseHandle::start`'s
signature and its one call site untouched, and colocated the broadcast call with the JSON-RPC
response construction it happens right before — one small, reviewable unit per mutation, rather
than a change to the actor's own command-handling loop.

### Positive Consequences

* No changes needed to `DatabaseHandle`'s public API or its construction call site.
* Every mutation's broadcast call sits directly next to its response-building code, making the
  pattern easy to spot-check by reading one function.
* Two regression tests (`events_replay_round_trips_committed_mutation_events`,
  `events_subscribe_delivers_live_notifications_for_orchestration_mutations`) now exist
  specifically to catch a reintroduction of this bug — and the second one's failure mode (hanging
  forever, rather than a clean assertion failure) is a deliberately faithful reproduction of what
  the original bug actually looked like in practice.

### Negative Consequences

* This is a convention, not a compiler-enforced invariant — a genuinely new mutation call site
  could still be written without the broadcast call, and nothing besides a test (which must itself
  be written) will catch it. This tradeoff is explicit: option (b), broadcasting from the actor
  thread, would have made the invariant impossible to skip at the cost of a wider
  `DatabaseHandle` API change touching every existing call site. If this bug class recurs, that
  tradeoff should be revisited.

## Pros and Cons of the Options

### Broadcast from the async service layer (chosen)

* Good, because it requires no change to `DatabaseHandle`'s API or the actor's command loop.
* Bad, because it relies on convention (and tests) rather than the type system to catch an
  omission.

### Broadcast from inside the DB actor, atomically with commit

* Good, because it would make skipping the broadcast a genuine impossibility — the actor could
  guarantee it happens in the same critical section as the commit.
* Bad, because it requires changing `DomainClosure`'s return type and threading `events_tx` into
  `DatabaseHandle::start`, a wider-reaching change to a Foundation-era API for a benefit this
  project chose to get via tests instead.

### Periodic polling/diffing

* Good, because it would need no change to any mutation call site at all.
* Bad, because it reintroduces latency and complexity (tracking "what's new since last scan")
  that a direct broadcast at commit time avoids entirely, and directly conflicts with the
  replay-then-live design's promise of no separate modes (ADR-0019).

## Links

* Narrated in `../journal.md`, commit `49233a5` (bug #3)
* Required by [ADR-0019](0019-monitor-is-one-reducer-over-replay-and-live-no-separate-modes.md)
* Checklist documented in `../development.md`, "Adding a new domain mutation"
