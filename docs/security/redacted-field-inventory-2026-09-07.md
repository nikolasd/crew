# Redacted-field inventory — 2026-09-07

**Verified against:** `main` @ `0aae002bd47bbbd7d84fcdd29ffeb17c196830bb` (2026-09-07).

**This is a point-in-time audit record, not living documentation.** Every `file:line` reference
below was read on the commit above; construction sites drift as the codebase changes, and this
document is not updated to track that drift. Treat it as evidence of what was checked and what was
found on that date, not as a current index — re-run the sweep described below against `HEAD` rather
than trusting these line numbers still point at the right code. See [ADR-0006](../adr/0006-type-enforced-redaction-boundary.md),
which this document audits the surface of.

## Why this exists

A compile-time guard (`crates/protocol/src/event.rs`,
`every_reachable_string_field_is_redacted_or_allowlisted`) — added per
[ADR-0006](../adr/0006-type-enforced-redaction-boundary.md) — proves every `String`-typed field
reachable from `RuntimeEvent` is either `Redacted` or explicitly allowlisted with a stated reason.
That guard's own scope is honest about its limit: it enforces that a field is *typed* `Redacted`,
never that a `Redacted`-typed field was actually *populated* through the redactor. `Redacted`
deserializes from a bare wire string by design (stored events must round-trip), so a request field
typed `Redacted` can in principle be filled straight from caller JSON via
`Redacted::assert_runtime_authored` or a bulk `serde_json::from_value`/`from_str` — the type states
which boundary a value must cross; it is not evidence that it crossed one.

This audit is the check the guard cannot run itself: for every `Redacted`/`Option<Redacted>` field
declared in `crates/protocol/src/`, trace it backward to every construction site, and classify
whether the value reaching it is (a) caller- or vendor-supplied text that passes through
`Redactor::sanitize_fragment`/`Redactor::redact_text` first, (b) a value that is genuinely
runtime-authored (a fixed sentence, an id the daemon minted, an enum-derived string) and legitimately
uses `Redacted::assert_runtime_authored`, or (c) neither — a finding.

## Method

1. Enumerate every `Redacted`/`Option<Redacted>` field declared in `crates/protocol/src/`
   (`grep -n ": Redacted\|: Option<Redacted>" crates/protocol/src/*.rs`).
2. For each field, find every non-test construction site (`grep` for the enclosing struct/variant
   literal across `crates/runtime/src/`).
3. For each site, read backward from the value passed in to its actual origin — the RPC parameter,
   the adapter payload, or the fixed string literal — not just the immediate caller's type
   signature. `assert_runtime_authored` is a claim, not a check; verifying it means reading what is
   actually interpolated into the string, including the *declared type* of any interpolated
   variable (a `usize` cannot carry text; a `String` might).
4. Classify: sanitized caller/vendor text, genuinely fixed/runtime-minted text, or unpopulated
   (never constructed with real content anywhere in the runtime — not a leak, but worth naming so
   nothing else cites it as carrying live data that it doesn't).

## Findings table

| Field | Construction site(s) | Input source | Sanitized? |
|---|---|---|---|
| `SubtaskSpec.description` | `orchestration.rs:2892` | Caller JSON (`plan` param) | Yes — `redact_caller_text`, per-subtask, after bulk `serde_json::from_value` at `:2878` |
| `RunMessage.payload` | `orchestration.rs:2343`; `broker.rs:256` (`send`); `broker.rs:661` (`publish_artifact`) | Caller (`message/send`) / worker (`coordination/*`) | Yes — `redact_caller_text`/`redact_worker_text` at all three sites |
| `RunPromptEvent.prompt` | `orchestration.rs:1080-1091` | Caller (`run/submit`) | Yes — `sanitize_fragment` inline |
| `ApprovalEvent.reason` | `orchestration.rs:2753` | Caller (`approval/decide`) | Yes — `redact_caller_text`, empty-after-redaction rejected |
| `ChildEvent.reason` (deny) | `orchestration.rs:3328` | Caller (`coordination/child/decide`) | Yes — `redact_caller_text` |
| `ChildEvent.reason` (request) | `broker.rs:584` (`request_child`) | Worker (`coordination/requestChild`) | Yes — `redact_worker_text` |
| `PlanDecided.reason` | `orchestration.rs:2938` | Caller (`plan/decide`) | Yes — `redact_caller_text` |
| `AdapterMessageEvent.text` | `event_sink.rs:304,312` | Vendor (adapter transcript) | Yes — `self.sanitize()` = `sanitize_fragment` |
| `AdapterToolEvent.detail` | `event_sink.rs:338,353` | Vendor | Yes — `self.sanitize()` |
| `AdapterProtocolHealthEvent.detail` | `event_sink.rs:383` | Vendor | Yes — `self.sanitize()` |
| `WorkerQuestion.question` | `event_sink.rs:408` | Vendor | Yes — `self.sanitize()` |
| `AdapterNestedWorkerEvent.vendor_child_id`/`vendor_parent_ref` | `event_sink.rs:393-394` | Vendor | Yes — `self.label()` = `redact_text` |
| `RuntimeEventKind::PolicyViolationRecorded.vendor_child_id`/`vendor_parent_ref` | `repository.rs:2507-2508`, fed from `event_sink.rs:456-464` | Vendor, via the already-labeled `AdapterNestedWorkerEvent` | Yes — inherits the `label()` pass; never reapplied, never needs to be |
| `WorkspaceEvent::PaneDowngraded.reason` | `display/coordinator.rs:547`, sanitized at `:530-541` | Vendor (multiplexer stderr) | Yes — `sanitize_fragment` inline |
| `Diagnostic.message` (4 sites: `display/coordinator.rs:568`, `recovery.rs:852`, `orchestration.rs:2504,2690`) | see above | Mixed: 2 fixed sentences with no interpolated text (`orchestration.rs:2504`; `coordinator.rs:220-228`, which interpolates `self.max_live_panes` — declared `usize` at `coordinator.rs:116`, cannot carry text), 2 third-party text (`recovery.rs` via `self.redact()` at `:639`; `orchestration.rs:2680` via `redact_caller_text`) | Yes everywhere third-party text is carried; the two fixed-sentence sites are honest `assert_runtime_authored` uses — verified by the declared type of what they interpolate, not just by reading the call site's comment |
| `WorkspaceEvent::CleanupFailed.error` | `cli.rs:679`; `orchestration.rs:1666,1687,1997` | Git/filesystem teardown error text | Yes — `sanitize_fragment` (`cli.rs`) / `redact_caller_text` (all three `orchestration.rs` sites) |
| `EscalationRaised.question` | `repository.rs:1589`; only two callers (`run_lifecycle.rs:453`, `repository.rs:1773`) | — | Both callers pass `None`. **Never populated with real content anywhere in the runtime.** Not a live gap by itself — see "Consequence of a dead field" below. |
| `EscalationAnswered.answer` | `repository.rs:1540` | Reads back `escalations.answer`, itself written from `message.payload` (already sanitized) | Yes — `Redacted::from_sanitized` on an already-redacted column, traced to its source rather than trusted from its own comment |
| `RuntimeEventKind::PolicyViolation.reason` | Declared at `event.rs` (~line 598); zero non-test construction sites anywhere in the runtime (`grep -rn "RuntimeEventKind::PolicyViolation\s*{"` returns nothing) | — | Dead. Confirms its own doc comment's claim ("this variant has zero construction sites in the runtime"). |
| `PolicyViolationDecided.resolved_by` | `repository.rs:2634`, `Redacted::assert_runtime_authored(principal_instance_id)` | `principal.instance_id` — the connecting client's **self-declared, wire-supplied identity from the `initialize` handshake** (`crates/protocol/src/rpc.rs:56`, `ClientAuth::OmpExtension.instance_id: String`), never validated in shape or content anywhere in `authenticate()` (`connection.rs:268-312`) | **FINDING — the only one.** See below. |

## The one finding

`PolicyViolationDecided.resolved_by` was, as of this audit, the sole case where a `Redacted` field
was populated via `assert_runtime_authored` — the "trust me" constructor — on a value that is not
actually runtime-minted. It is the connecting `ompExtension` client's own `instance_id`, chosen
freely at connection time with no format, length, or character validation anywhere in the protocol
or the connection handshake. The claim that this was safe rested entirely on the ownership guard
inside `resolve_policy_violation`'s transaction (the value written is the one the guard just
authorized, never free text a caller chose) — `repository.rs:2627-2633` said as much in its own
words: *"if that guard ever loosens, this claim becomes false and must move to the redactor."* The
same root cause (an unvalidated client `instance_id` made durable under an identifier's name) also
reached `plans.owner_client_instance_id` and `tasks.owner_client_instance_id`, outside this
document's `Redacted`-field scope but sharing the same root cause.

**Closed after this audit.** Commit `02a0b54` (2026-09-08) bounds `instance_id` once, at the
connection handshake, before any of the three columns above receive it: non-empty, at most 128
bytes, restricted to `[A-Za-z0-9._-]` — a bound rather than a format, chosen so a legitimate client
whose id scheme changes is never rejected, but excluding whitespace, control characters, and
base64's `/`/`+`/`=` so the field cannot carry a smuggled multi-line payload or credential. This
closes the finding without moving the field through the redactor, which the fix's own commit
message treats as the better answer for an identifier that is bounded rather than free text.

No other `Redacted` field in `crates/protocol/src/` was found populated by a bulk
`serde_json::from_value`/`from_str` deserialize without a subsequent per-field sanitize call.
`PlanSpec`/`SubtaskSpec` (`orchestration.rs:2878`) is the only bulk-deserialize onto a
`Redacted`-bearing type, and each subtask's `description` is individually re-sanitized in the loop
that follows (`:2890-2893`) before the value is used.

## Consequence of a dead field

`EscalationRaised.question` being provably unpopulated everywhere had a consequence beyond its own
row: the compile-time guard's own allowlist reason for the sibling `EscalationRaised.reason` field
(`event.rs:1475`) reads *"the worker's text travels in the sibling `question: Option<Redacted>`"* —
describing a data flow that, per this audit, does not exist. Nothing leaks because `reason` really
is a fixed machine code regardless (the safety holds on its own clause), but the second clause did
rhetorical work it wasn't entitled to. **The lesson generalizes: finding a dead field is only half
the check — the other half is asking what else cites it as live.** That correction was made by the
same commit (`02a0b54`) that closed "The one finding" above, not this document, since it's a claim
inside the guard's own allowlist rather than a `Redacted`-population site — the allowlist reason now
cites `EscalationRaised.reason`'s two actual production construction sites instead.

## What this document does not cover

- Fields outside `crates/protocol/src/`'s `Redacted` type entirely: the plain `String` columns
  named in "The one finding" above (`plans.owner_client_instance_id`,
  `tasks.owner_client_instance_id`, `policy_violations.resolved_by`) — closed by the same
  handshake-bound fix (commit `02a0b54`) rather than by a separate audit.
- Anything added to the protocol after `0aae002`. The compile-time guard (see
  [ADR-0006](../adr/0006-type-enforced-redaction-boundary.md)) will catch a new *unaccounted*
  `String` field; it will not catch a new `Redacted` field populated the same way `resolved_by`
  was. That is exactly the gap this document exists to name, and exactly why it needs re-running
  from scratch against a current commit rather than trusted as still accurate.
