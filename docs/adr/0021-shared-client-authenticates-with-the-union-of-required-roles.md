# A shared client authenticates with the union of every caller's role

* Status: Accepted
* Date: 2026-07-24

## Context and Problem Statement

`index.ts::getClient()` caches exactly one `BatmanClient` per extension session and hands it to
every caller that needs one — `batman_status`, all eight orchestration tools, and the monitor.
`ensureRuntime()` (written in Foundation, when `batman_status` was the only caller) authenticated
that client as the read-only `display` role. Six commits later, every write-capable orchestration
tool sharing that same client failed `method ... is not available to this client`, silently,
because nothing checked the relationship between what the shared client could do and what its
growing set of callers actually needed. When one connection is shared across callers with
different needs, which role should it authenticate as, and how does the answer stay correct as
more callers get added?

## Decision Drivers

* Re-authenticating per call isn't available — `initialize` happens once per connection, not per
  request.
* Maintaining N separate connections, one per role actually needed, is real added lifecycle
  complexity (each needs its own connect/reconnect/close handling) for a problem that has a
  simpler fix if the roles happen to nest.
* The fix must be provably safe for the *existing* caller (`batman_status`), not just correct for
  the new ones.

## Considered Options

* The one shared client authenticates as whichever role is a strict superset of every current
  (and reasonably foreseeable) caller's needs — in this case, `ompExtension`, whose allowed-method
  table is a documented superset of `display`'s.
* Maintain multiple connections, one per role, and route each tool through the connection matching
  its own minimum-privilege role.
* Re-authenticate or "upgrade" the role of an existing connection per call — not supported by the
  protocol as designed (ADR-0004/ADR-0009): role is fixed at `initialize`.

## Decision Outcome

Chosen option: authenticate the one shared client as `ompExtension`. This is safe for
`batman_status` specifically because `ClientPrincipal::allowed_methods()` for `ompExtension` is a
strict superset of `display`'s — every method `display` could call, `ompExtension` can also call,
plus everything else. The fix required changing exactly one function (`runtime.ts::initParams`)
and no changes anywhere else.

### Positive Consequences

* One connection, one lifecycle (open at first use, close at `session_shutdown`) — no added
  connection-management complexity.
* Fixed the entire class of "orchestration tool fails silently" bugs in one place, rather than
  patching each tool's call site individually.
* Established a reusable principle, not just a one-off patch: any future feature reusing this same
  cached client inherits a role broad enough for its needs automatically, as long as the superset
  relationship holds.

### Negative Consequences

* "Safe" depends on the superset relationship between roles continuing to hold as roles evolve —
  this is now a fact that requires the same explicit care as the invariant it protects (documented
  in `architecture.md`'s role-table section and cross-referenced from this ADR), not something the
  type system verifies.
* If a future caller ever needed a role that *isn't* a superset-compatible extension of
  `ompExtension` (a genuinely narrower, conflicting privilege boundary), this pattern would stop
  being sufficient and a second connection would become the correct answer — worth revisiting if
  that situation arises.

## Pros and Cons of the Options

### Shared client, broadest necessary role (chosen)

* Good, because it fixes every current and most future callers with one change, and needs no new
  connection-lifecycle code.
* Bad, because its safety is a documented fact about role relationships, not a compiler-checked
  one.

### Multiple connections, one per role

* Good, because each connection would carry exactly the minimum privilege its caller needs, with
  no reliance on superset relationships holding.
* Bad, because it multiplies connection lifecycle management (N connect/reconnect/close paths
  instead of one) for a benefit (strict minimum privilege per tool) that doesn't currently matter,
  since every caller lives inside the same trusted extension process anyway.

### Per-call role upgrade

* Good, because it would let each call use its own precise role without any superset reasoning.
* Bad, because the protocol doesn't support it — role is bound once, at `initialize`, by design
  (ADR-0009) — and adding per-call re-authentication would be a significant protocol change to
  solve a problem the superset approach already solves more cheaply.

## Links

* Narrated in `../journal.md`, commit `49233a5`
* Relies on the role-table superset relationship established by
  [ADR-0009](0009-role-based-authorization-from-the-connection-not-per-call.md)
