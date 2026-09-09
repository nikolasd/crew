# Type-enforced redaction boundary before persistence

* Status: Accepted
* Date: 2026-07-23

## Context and Problem Statement

The daemon journals step-by-step agent activity, which can include secrets (API keys, tokens) and
hidden reasoning that must never reach durable storage — not the SQLite file, not the WAL, not the
log. A convention ("remember to redact before writing") is only as strong as the discipline of
whoever adds the next call site. How can this be made structurally impossible to bypass, rather
than merely discouraged?

## Decision Drivers

* This is a hard security requirement, not a best-effort one — a single leaked secret in a durable
  file is a real incident, not a degraded-quality bug.
* New code paths that produce events will be added by people who haven't read every prior
  decision; the boundary must protect them even if they don't know it's there.
* Accidental logging (`tracing::debug!(?value)`) is a realistic leak vector and must be covered
  too, not just the intentional persistence path.

## Considered Options

* Types with private fields and no public constructor (`PersistableEvent`, `SanitizedJson`),
  producible only by passing raw, classified input through a `Redactor`; the database actor
  accepts only these types, never a raw string or `serde_json::Value`.
* A runtime assertion or lint that scans string content for secret-shaped patterns before any
  write.
* Trust code review to catch any new call site that skips redaction.

## Decision Outcome

Chosen option: type-enforced redaction. `RawRuntimeEvent`'s content fields are `Classified<String>`
(tagged `Visible`, `Thinking`, or `Secret`); `Redactor::sanitize` drops `Thinking`/`Secret`
fragments entirely and masks regex-matched secrets inside `Visible` text; the resulting
`PersistableEvent`/`SanitizedJson` have no public constructor, so the only way to produce one is
through the redactor. `DatabaseHandle::append_event` accepts only `PersistableEvent`. There is no
raw-string append API to reach for instead.

### Positive Consequences

* A violation is a compile error, not a runtime check that might not fire on every code path.
* `Classified<T>`'s hand-written `Debug` impl prints `<redacted>` for non-visible content, so even
  an accidental `{:?}` debug log can't leak a secret.
* `crates/runtime/tests/redaction_boundary.rs` proves the property end to end by byte-scanning the
  actual database file, WAL, and log — not just asserting on the redactor's return value in
  isolation.

### Negative Consequences

* Every new event source needs an explicit `Raw... -> Classified -> Redactor::sanitize ->
  Persistable` translation step; there is no shortcut, by design.
* The redaction regex denylist itself is intentionally small (API-key/bearer-token shapes);
  classification is the primary boundary, and expanding the denylist is separate, ongoing work
  (tracked as a known deferred item).

## Pros and Cons of the Options

### Type-enforced boundary (chosen)

* Good, because the type system, not a developer's memory, is what prevents a leak.
* Bad, because it adds ceremony (a translation step) to every new event source.

### Runtime scanning/lint before write

* Good, because it requires no upfront type design.
* Bad, because it only catches what the scanner's patterns happen to match, and a bypass (any new
  raw-write call site) is just as easy to add as a compliant one.

### Trust code review

* Good, because it costs nothing to set up.
* Bad, because a security boundary that depends on every future reviewer noticing every future
  omission is not a boundary, it's a hope.

## Links

* Narrated in `../journal.md`, commit `8cd8ad8`
* Proven by `crates/runtime/tests/redaction_boundary.rs`
* A compile-time guard added later (2026-09-01) closes one specific gap this ADR's original scope
  didn't name explicitly: every `String`-typed field reachable from `RuntimeEvent` must be either
  `Redacted` or explicitly allowlisted with a stated reason
  (`crates/protocol/src/event.rs::every_reachable_string_field_is_redacted_or_allowlisted`).
* [`docs/security/redacted-field-inventory-2026-09-07.md`](../security/redacted-field-inventory-2026-09-07.md)
  is a point-in-time audit of the gap that guard cannot close on its own: a field typed
  `Redacted` states which boundary a value must cross, not that it did. Read it for when this
  boundary's actual construction sites were last swept, not as a current index.
