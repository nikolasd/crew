# What a tool result may carry is decided by capability, not by durability

* Status: Accepted
* Date: 2026-08-31

## Context and Problem Statement

Two `crewd` processes, each resolved against a different state root and neither aware of the other,
cost an operator real debugging time chasing behaviour in the wrong database. Nothing in any tool
result said which database a running daemon was reading or which socket it was listening on, so the
answer had to be inferred from process arguments or logs.

The fix is obvious — print them. The question it raised is not: **what governs whether a value may
appear in a tool result at all?**

The first answer offered was that results are safe because they are not journaled. That is true and
proves less than it appears. A result reaches the leader's session transcript, which is a wider
surface than a local log — the same reasoning that had already been used to argue that printing a
dashboard URL in a command's output is a *broader* exposure than writing it to a log file. So
"results aren't journaled" cannot be the test, because the transcript is a distribution channel of
its own.

Worse, the two candidate values are not alike. A dashboard URL and a socket path look equally like
"internal detail", and treating them the same way — either both fine because neither is journaled,
or both sensitive because both are internal — gets one of them wrong.

## Decision Drivers

* A rule for this is needed repeatedly and by different people. "Should this go in the result?"
  recurs, and re-deriving the answer each time produces inconsistent answers with confident
  reasoning behind each.
* The rule must distinguish values that *grant* something from values that merely *describe*
  something, because that is the distinction the two motivating fields fall on either side of.
* Durability is a real property but the wrong axis. Journaling determines whether a value persists
  and is exportable; it says nothing about what holding the value lets someone do.
* An operator debugging the wrong database is a real cost, and a rule that forbids printing the
  state root would have to justify that cost.

## Considered Options

* Gate on durability: anything not journaled may appear in a result.
* Gate on capability: a value may appear in a result unless holding it grants access that the
  holder would not otherwise have.
* Gate on sensitivity by category: treat paths, ids and URLs as internal and withhold all of them.

## Decision Outcome

**A value may appear in a tool result unless holding it is capability-granting.** The test is what
possession lets someone *do*, not where the value ends up.

Applied to the values that raised the question:

* **The dashboard URL is capability-granting**, so it is treated as a secret. The dashboard binds to
  loopback, and loopback is not access control — it keeps other *hosts* out and does nothing about
  other local users or processes, which is precisely why a bearer token exists. Whoever holds the
  URL holds the token, and the field's own doc says so.
* **The daemon socket path is not capability-granting.** The socket is guarded by
  `admit_same_uid` in `crates/runtime/src/security/mod.rs` plus an owner-only directory check, with
  a test asserting a mismatched peer uid is rejected even when the directory check passes. Publishing
  the path grants a local attacker nothing they lacked, because knowing where the socket is was never
  what kept them out.
* **The state root is genuinely new information** — it reveals the username in an absolute path —
  and is strictly weaker than the bearer credential already in the same output by an earlier,
  deliberate decision. It is printed, absolute, so two daemons pointed at different roots are
  distinguishable at a glance.

Both fields are on `RuntimeStatus` in `crates/protocol/src/rpc.rs`, and the reasoning above is
recorded beside them in the type.

### Why the durability test was rejected rather than refined

It answers a different question correctly. Journaling decides whether a value becomes durable and
exportable — whether it reaches `audit export`, `events/replay` and every future reader of the
journal. That matters enormously, and it is the subject of
[ADR-0006](0006-type-enforced-redaction-boundary.md). It has nothing to say about whether a value in
a transient response grants its holder access.

Keeping it as the test would have produced a system where the same value is withheld from a result
because something like it is journaled, and a genuine credential is permitted because it is not.

### The distinction this decision does not collapse

A rule about journaled fields already exists and is *not* in tension with this one: an absolute path
must not appear in a journaled pane reference, because there it would persist into replay and audit.
`RuntimeStatus` appears nowhere in `crates/protocol/src/event.rs` — it is a response type, not an
event — so the two rules govern disjoint surfaces. Recorded because the apparent conflict is the
first objection a careful reader raises, and the resolution is structural rather than a judgement
call.

### Positive Consequences

* The recurring question has one answer, and the answer is mechanical enough to check: name what
  holding the value lets someone do.
* The two-daemon failure becomes self-diagnosing — the operator reads the state root rather than
  inferring it.
* The existing decision to print a tokenized dashboard URL is now justified by a stated rule rather
  than being an exception to an unstated one.

### Negative Consequences

* **The state root reveals a username**, in output that reaches a session transcript. That is
  accepted rather than mitigated, on the grounds that it is strictly weaker than a bearer credential
  the same output already carries deliberately.
* Applying the test requires knowing what actually guards a resource, which is not always local to
  the decision. The socket case turns on `admit_same_uid` and a directory check being correct — if
  either regressed, the path would silently become capability-granting without this rule changing.
  The rule is therefore only as good as the guarantee it points at, and it points at a named,
  tested one for that reason.
* A category-based rule would have been easier to apply without reading any code. This one demands
  a specific answer about a specific resource each time.

## Pros and Cons of the Options

### Capability-granting test (chosen)

* Good, because it separates values that grant access from values that merely describe the system,
  which is the distinction that actually matters to a reader of a result.
* Good, because it forces an answer about a real guarantee rather than a category judgement.
* Bad, because it can only be applied by someone willing to establish what guards the resource, and
  it inherits the correctness of that guard.

### Durability test

* Good, because it is trivially checkable — either the value is journaled or it is not.
* Bad, because it answers a question about persistence and exportability, not about access, and a
  result reaches the session transcript regardless. It would permit a credential and could forbid a
  harmless path.

### Category-based sensitivity

* Good, because it needs no investigation and is uniformly applicable.
* Bad, because it is wrong in both directions at once: it withholds the state root, whose absence
  caused the incident, and gives no reason to treat a tokenized URL differently from an unguarded
  path.

## Links

* Distinct from [ADR-0006](0006-type-enforced-redaction-boundary.md), which governs durable content.
  This decision governs transient responses, and the two surfaces do not overlap — `RuntimeStatus`
  is not an event type.
* Rests on [ADR-0009](0009-role-based-authorization-from-the-connection-not-per-call.md)'s
  connection-level admission, which is what makes the socket path safe to publish.
* The socket guarantee is proven by
  `admit_same_uid_rejects_a_mismatched_peer_uid_even_with_owner_only_directory` in
  `crates/runtime/src/security/mod.rs` — the test that makes "publishing the path grants nothing"
  a checkable claim rather than an assertion.
* Rendered by `packages/extension/src/status.ts`.
