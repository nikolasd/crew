# Protocol doc comments split by sigil: `///` ships, `//` stays internal

* Status: Accepted
* Date: 2026-08-30

## Context and Problem Statement

`crates/protocol/` is the canonical source of the wire protocol (ADR-0002): its types are derived
into the JSON Schema and the generated TypeScript bindings every other consumer reads. `schemars`
lifts a `///` doc comment verbatim into the schema's `description` field for the item it documents;
a `//` comment desugars to nothing a derive macro can see at all. Both sigils compile identically
and read identically in an editor, so nothing distinguishes "this explains the wire shape to a
consumer" from "this is implementation history for the next contributor" except which one was
typed. Left unmanaged, that invisible distinction leaks two ways: internal rationale, maintainer
names, and ticket-numbered history end up shipped in a schema external tooling parses; and a
backticked reference inside a shipped description can name something that only exists in Rust, or
under a different name on the wire, with nothing to catch it.

## Decision Drivers

* The shipped schema and generated TypeScript bindings are read by tooling and by developers who
  have never seen this repository's Rust source or its commit history.
* Protocol types accumulate real design history (why a field is optional, what a past bug taught,
  which alternative was rejected) that is valuable to a future contributor and actively unwanted in
  a schema an external consumer parses.
* A doc comment can contain a code fence that is also a doctest; changing which sigil holds it can
  silently stop that test from running at all, with no red build to notice (see
  `docs/engineering-lessons.md`, "Changing a doc comment's sigil is a test-suite edit when that
  comment holds a code fence").
* A human reviewer checking every backticked identifier in every shipped description against the
  actual schema, by hand, on every change, does not scale and was not happening.

## Considered Options

* No convention — leave every doc comment `///`, accepting that internal history ships in the
  schema.
* A style-guide convention only (documented, not enforced) — `///` for consumer-facing text, `//`
  for internal notes, checked in review.
* The sigil split, enforced by an automated guard over the rendered schema.

## Decision Outcome

Chosen option: the sigil split, enforced mechanically rather than by review discipline.
Consumer-facing description text — what a field means, what values it can take, what a caller or
reader needs to know — stays `///` and ships. Internal rationale — why a design choice was made,
what bug it fixed, which ticket drove it, maintainer-facing history — moves to `//`, invisible to
`schemars` and therefore never rendered into `crew.schema.json` or the generated TypeScript. The
convention is load-bearing, not aspirational, because `crates/protocol/src/schema.rs` derives a set
of tests directly from the rendered schema rather than from the source:

* Every backticked *type* name (PascalCase) in a shipped description must resolve to something
  real — a `$defs` type key, an actual enum/const value anywhere in the schema, or an explicit,
  reason-carrying entry in an allowlist for the rare case where a name is deliberately unresolved
  (e.g., naming a retired variant specifically because it no longer exists).
* A backticked *property* name (lowercase) is checked differently: *scoped to its own object*, not
  resolved globally against the whole schema — a name that happens to be a real property somewhere
  else no longer satisfies a reference that is locally wrong. A lowercase name that is a property
  nowhere at all passes as out of scope for this check (it could be a CLI flag or a config key) —
  including a Rust snake_case field name whose camelCase wire form is a property somewhere, which
  is not caught by either check as originally shipped.
* The allowlist itself is checked for staleness: an entry naming a field that no longer exists is
  reported, not silently carried forward as a stale exemption waiting to attach to some future,
  unrelated field of the same name.
* A shipped description is checked for the bare word "None" — a Rust-ism that means nothing to a
  TypeScript or JSON Schema reader, who sees `null`.

Where prose needs to leave `///` but a code fence inside it must keep running as a doctest, the
rescue is a dedicated `#[cfg(doctest)]` item to hold the fence, rather than leaving it stranded in a
comment sigil that no longer executes it.

### Positive Consequences

* The shipped schema and TypeScript bindings read as documentation for the wire format alone —
  no maintainer names, ticket numbers, or Rust-only rationale a consumer has no context for.
* A wrong or drifted reference inside a shipped description is a test failure at the point it's
  introduced, not something a future reader discovers by trusting a comment that turned out to be
  false.
* The guard's own history is itself evidence for the decision: three separate, unrelated schema
  changes have tripped it before merge — a doc citing a Rust type name instead of its wire form, a
  sibling reference that only held in one branch of a `oneOf`, and a stale allowlist entry from a
  bug the field it was written for no longer has. All were caught by the guard, not by review.

### Negative Consequences

* Every new protocol doc comment costs a decision — which sigil — that a single-sigil convention
  would not require, and getting it wrong in either direction (internal detail shipped, or
  consumer-relevant text hidden) is possible until the guard runs.
* The guard adds real maintenance surface of its own: a legitimate new cross-object reference or a
  genuinely unresolvable name needs an allowlist entry with a real, checkable reason, not just a
  name — an entry whose reason merely restates that the name is unresolved fixes nothing and will
  itself be flagged as unjustified in review.

## Pros and Cons of the Options

### The sigil split, mechanically enforced (chosen)

* Good, because the schema stays clean without depending on every future contributor remembering a
  style rule while writing an unrelated change.
* Good, because "clean" is verified against the actual rendered output, not asserted about the
  source.
* Bad, because it is more machinery than a two-line style guide, and that machinery needs its own
  tests to stay trustworthy.

### Style-guide convention only

* Good, because it costs nothing to write down.
* Bad, because a convention nobody's build enforces degrades exactly like the "remember to redact"
  convention ADR-0006 rejected for the same reason: it depends on every future author, forever,
  remembering a rule with no compiler or test behind it.

### No convention

* Good, because it is the least effort of the three.
* Bad, because it is the status quo this decision was written to leave — internal history shipped
  verbatim to every external consumer of the schema, with no mechanism even flagging it.

## Links

* Guard implementation: `crates/protocol/src/schema.rs`
* Doctest-sigil trap: `docs/engineering-lessons.md`, "Changing a doc comment's sigil is a
  test-suite edit when that comment holds a code fence"
* Related: [ADR-0002](0002-rust-canonical-protocol-with-generated-bindings.md) (the schema and
  bindings this decision keeps clean are themselves generated from these same Rust types), and
  [ADR-0006](0006-type-enforced-redaction-boundary.md) (the same "a convention nobody's build
  enforces eventually fails" reasoning, applied there to redaction and here to doc comments)
* The lowercase-property-name gap this ADR states above (a snake_case field name whose camelCase
  wire form is a property somewhere, resolving as out of scope rather than as a leak) is CREW-75,
  closed by a fourth resolution case in the same sibling-property check.
