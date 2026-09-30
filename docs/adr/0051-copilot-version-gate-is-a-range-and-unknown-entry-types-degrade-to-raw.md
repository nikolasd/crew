# The Copilot TUI version gate is a range like the other three, and an unrecognised entry type degrades to `Raw` before that range is widened

* Status: Proposed
* Date: 2026-09-30
* Supersedes: *(none — this does not reverse a decision; it decides one that was inherited and never made)*
* Amends: *(none — no existing record states the gate policy; `docs/compatibility.md` is a documentation table, not a decision record)*

## Context and Problem Statement

Each TUI vendor decides, from a real `--version` probe, whether the installed CLI is one its
fixed argv and transcript-format assumptions were built against. Three of the four gates are
inclusive ranges. Copilot's is not: it is a discrete five-entry list, and **the reason it is
different is not one anybody ever decided.**

The rule was inherited with the code. It came from the headless ACP adapter — and the headless
adapters are gone. `grep -rn -- '--acp' crates/` returns zero hits, no `.rs` file contains
`CopilotAcpClient`, and the only remaining `acp` match in the TUI adapter is the word *ACP* inside
a comment at `crates/runtime/src/adapter/tui/copilot.rs:178`. Nothing in the repository now
negotiates ACP from a `--acp` flag. The reasoning survives only as prose: `docs/compatibility.md:226-228`
states it — *"ACP is a wire protocol, not a self-describing file format, so an untested build gets
no benefit of the doubt"* — and the table immediately below records the four gates side by side.

That prose is a rationale, not a record. The only ADR that touches the table did so to keep it
alive across a deletion, and said nothing about the policy it encodes: ADR-0026 records the four
headless adapter implementations as deleted, with one exception — *"The one exception is Copilot's
CLI/ACP-protocol-version compatibility table … which encodes empirically-verified facts about the
vendor CLI independent of headless vs. TUI dispatch — moved to `adapter/tui/copilot_compatibility.rs`
rather than deleted"* (`docs/adr/0026-headless-retirement.md:75-78`). It preserves the artefact and
is silent on the rule. **The gate's policy is therefore documented but never decided**, and it is
inheriting its justification from a protocol this repository no longer speaks.

### The implementation is looser than the policy it claims to enforce

The stated policy is exact match. `crates/runtime/src/adapter/tui/copilot_compatibility.rs:6` says
*"empirically verified against (exact match, never a "nearby" patch version assumed compatible)"*,
and `:66` says the same of the helper below it. The gate's own comment at
`crates/runtime/src/adapter/tui/copilot.rs:175-183` asserts the rule is *"never a range
extrapolation like the other vendors' gates, because this vendor ships breaking protocol changes
between patch releases."*

The implementation is a substring search over the vendor's entire `--version` prose, declared at
`crates/runtime/src/adapter/tui/copilot.rs:184` (`version_gate`), matching at `:188`
(`probed.contains(version)`). Against the five entries at
`crates/runtime/src/adapter/tui/copilot_compatibility.rs:36` (`COPILOT_KNOWN_CLI_VERSIONS`) that
admits `0.1.0.8000`, `1.0.800`, `1.0.81-beta.1` and `1.0.750` — each of which contains a verified
version as a substring while being a different version. **A patch release nobody verified passes the
gate the comment calls an exact-match gate**, which falsifies the exact-match policy stated in the
test's own comment at `crates/runtime/src/adapter/tui/copilot.rs:480-481`: *"An unverifiable newer
patch release must NOT pass the exact-match gate (this vendor's documented hard-gate policy)."*

The gate's comment is partly self-aware — `crates/runtime/src/adapter/tui/copilot.rs:181-183`
concedes *"the check is a substring match per known entry rather than a leading-token parse"* — and
it knowingly weakens the rule it asserts at `:177-180`. The single test,
`version_gate_accepts_only_empirically_verified_versions` at
`crates/runtime/src/adapter/tui/copilot.rs:472`, exercises only `9.9.99` (`:482-485`), which
contains no listed substring at all and therefore proves nothing about substring overreach.

A genuine exact-match predicate already exists and is not called by the gate:
`copilot_cli_version_known` at `crates/runtime/src/adapter/tui/copilot_compatibility.rs:68`. It
compares the full string with `==`.

### The other three gates, with their bounds

All inclusive, all implemented as a parsed leading `MAJOR.MINOR.PATCH` triple:

| Vendor | Bound | Declaration |
|---|---|---|
| Claude | `1.0.0` – `2.99.99` | `crates/runtime/src/adapter/tui/claude.rs:38-39` (`MIN_TESTED_VERSION`, `MAX_TESTED_VERSION`) |
| Codex | `0.100.0` – `0.199.99` | `crates/runtime/src/adapter/tui/codex.rs:35-36` |
| OMP | `18.0.0` – `18.99.99` | `crates/runtime/src/adapter/tui/omp.rs:52-53` |

Each records that one point in its range is empirically validated and the rest is untested
extrapolation. OMP's comment at `crates/runtime/src/adapter/tui/omp.rs:45-51` gives the reason
Copilot's gate was considered and explains why OMP did not need one: an OMP transcript
self-describes its schema, so drift degrades to typed `Raw` events rather than corrupting the
journal.

### Unknown entry types: three of four degrade to `Raw`, and Copilot is the one that does not

`map_entry` at `crates/runtime/src/adapter/tui/copilot.rs:251` dispatches on the entry type. Its
catch-all arm at `:291` is `_ => (Vec::new(), None)`, commented at `:286-290` as *"Never surfaced,
never an error -- mirroring codex's token_count/exec_* handling."* Copilot is the only vendor that
discards them:

- Claude, `crates/runtime/src/adapter/tui/claude.rs:338` — `other => events.push(TuiEvent::Raw { … })`
- Codex, `crates/runtime/src/adapter/tui/codex.rs:341-346` — `other => (vec![TuiEvent::Raw { … }], None)`
- OMP, `crates/runtime/src/adapter/tui/omp.rs:497-502` — `_ => (vec![TuiEvent::Raw { … }], entry_id)`

**The ordering between these two changes is load-bearing, not incidental.** Copilot's `--version`
gate is what refuses to start on an unrecognised build. Widening it to a range first, with the
catch-all arm still discarding, converts a clean refusal into a session that starts and silently
loses journal entries for every entry type it has not been taught. The `Raw` fallback lands first.

### What Copilot is not missing, and must not be "fixed"

All four vendors drop unknown *content block* types deliberately. `crates/runtime/src/adapter/tui/claude.rs:536`,
`crates/runtime/src/adapter/tui/codex.rs:397` and `crates/runtime/src/adapter/tui/omp.rs:565` each
drop the catch-all, and the reason is that `thinking` is the model's hidden reasoning and must never
surface. Copilot has no equivalent arm because its content is a bare string with no block type. That
arm is correct as written, and this ADR does not touch it.

### The first evidence ever gathered for this gate

On 2026-09-28, Copilot CLI `1.0.89` completed a live ACP handshake: every field the repository names
at `crates/runtime/src/adapter/tui/copilot_compatibility.rs:13-14` (`protocolVersion`,
`agentCapabilities`, `agentInfo`) was present and correctly named, `session/new` returned a session
id, and one `session/prompt` returned `stopReason: "end_turn"` with a usage block. `1.0.89` is
field-name compatible and is refused today for one reason only: the list stops at `1.0.81`.

This is the **first probe evidence gathered for this gate at all**. The five existing entries were
each reprobed as the CLI's own auto-updater moved the build machine through them, and the ACP
handshake evidence the table's doc comment describes now lives in the live/fixture conformance
suites rather than beside the table. Nothing before this ADR ever tested the gate against a version
it had not verified.

## Decision Drivers

* A gate's stated policy and its implementation must not disagree. The comment at
  `crates/runtime/src/adapter/tui/copilot.rs:180-181` is currently falsified by `:188`.
* The justification for the gate must name a protocol this repository actually speaks.
* An entry type Crew has not been taught must not be silently discarded.
* One vendor's gate must not refuse a build that is demonstrably compatible, when the other three
  tolerate a range.
* The fallback must land before the range widens, or the widening converts a refusal into silent loss.

## Considered Options

1. Keep the exact-match list, and fix only the substring overreach by calling the existing
   `copilot_cli_version_known` predicate.
2. Widen the gate to a range immediately, leaving the catch-all arm as it is.
3. Add the `TuiEvent::Raw` fallback for unrecognised entry types first, then widen the gate to a
   range.
4. Add the fallback, keep the list, and revisit the range separately.
5. Add the fallback to content-block handling as well, for symmetry with the other three vendors.

## Decision Outcome

**Chosen: option 3, in that order — the `Raw` fallback lands first, then the gate becomes a range.**

Decision 1. An unrecognised Copilot entry type degrades to `TuiEvent::Raw` carrying the entry type
and the entry id, mirroring the arms at `crates/runtime/src/adapter/tui/codex.rs:341-346` and
`crates/runtime/src/adapter/tui/omp.rs:497-502`, and replacing the discarding arm at
`crates/runtime/src/adapter/tui/copilot.rs:291`. The arms that are deliberately empty stay empty
and are enumerated by name so a later reader does not read this as reversing them.

Decision 2. Copilot's `version_gate` becomes a parsed inclusive range, in the same shape as the
other three. The range is derived from the evidence already in the repository, not chosen freely:
`1.0.73` is the earliest verified entry, so the gate admits `1.0.x` from `1.0.73` upward, which
covers `1.0.89` and every patch the vendor has shipped since. A parsed leading-token comparison
replaces the substring search, so `1.0.800` and `0.1.0.8000` are refused rather than admitted by
accident.

Decision 3. The range is the *gate*; it is not a claim of semantic compatibility. The narrower
claim — that a build inside the range negotiates the field names this adapter understands — stays
separately evidenced, by the conformance suites, per release.

Decision 4. The rationale in `docs/compatibility.md:226-228` is restated against the protocol this
repository actually speaks, and the table at `:230-235` is updated to show four ranges. Until that
edit lands, the documentation contradicts this ADR, and this ADR is the newer statement of the same
fact.

**The order is part of the decision.** Decision 1 is not a tidy-up for Decision 2. A range gate
without it would start sessions on builds whose entry types Crew cannot map, and each unmapped entry
would be dropped from the journal with no error, no event and no trace. Refusing to start is
visible and recoverable; starting and losing journal entries is neither. Decision 1 is therefore
prerequisite to Decision 2 and must land first.

### Positive Consequences

* The gate's documented policy and its implementation become the same thing.
* Copilot `1.0.89`, verified field-name compatible on 2026-09-28, runs.
* An entry type added by a future Copilot release reaches the journal as `Raw` instead of vanishing.
* The gate's justification names a real protocol, so it can be re-evaluated when the transport
  changes.
* Copilot's gate is finally shaped like the other three, so the four are comparable without a
  special case.

### Negative Consequences

* The range admits builds nobody probed. That is the deliberate trade: the other three gates already
  make it, each recording its range as one validated point plus untested extrapolation, and a refusal
  on an unlisted patch blocks a compatible CLI.
* A protocol-level break *inside* the range now starts a session instead of refusing it. The `Raw`
  fallback bounds the damage — unmapped entries degrade rather than corrupt — but it does not
  prevent a break, and this is the real cost of the change.
* The substring search is deleted, which removes a behaviour some may have come to rely on
  accidentally.

### Pros and Cons of the Options

**Option 1 — keep the list, fix only the substring overreach.** Rejected as a complete answer. It
makes the implementation match the stated policy and is a genuine improvement on its own, but it
leaves the policy itself unexamined and undecided, and it still refuses `1.0.89`, which was
verified compatible. It fixes the falsification without answering what the gate is for.

**Option 2 — widen the range first, fallback later.** Rejected, and rejected for the reason this ADR
exists to prevent. It achieves the widest visible gain — `1.0.89` runs on the first change — at the
cost of making the failure mode worse and invisible. Before Decision 1 lands, the widened gate
admits builds whose unmapped entry types are still discarded at
`crates/runtime/src/adapter/tui/copilot.rs:291`. A user debugging that sees a journal missing
entries with no error anywhere, which is harder to diagnose than a refusal and loses data. Ordering
the two the other way round is the one arrangement in which the two changes together are worse than
neither.

**Option 3 — the chosen one.** The fallback is the safety net that makes a range honest: it is what
allows an unverified patch to be admitted without also allowing it to lose data. Ordering is
explicit, so an implementer following this ADR cannot take Decision 2 first.

**Option 4 — fallback now, range deferred indefinitely.** Rejected as an answer to the owner's
question, which was both changes in order. The fallback is uncontroversial on its own, but stopping
there leaves the gate refusing a verified-compatible build and leaves the substring overreach live.
The fallback is a prerequisite, not a substitute.

**Option 5 — also add `Raw` to content-block handling.** Rejected on the evidence, and doing it
would be a defect. The dropping arms at `crates/runtime/src/adapter/tui/claude.rs:536`,
`crates/runtime/src/adapter/tui/codex.rs:397` and `crates/runtime/src/adapter/tui/omp.rs:565` are
deliberate: `thinking` is the model's hidden reasoning and must never surface. Copilot's content is
a bare string with no block type, so it has no such arm to change. Symmetry here is a leak of model
reasoning into the journal, not parity.

## What would reverse this

A Copilot release inside the decided range that breaks the field names this adapter reads, in a way
the `Raw` fallback does not bound. That would be the first evidence bearing on this gate from the
other direction, and it would make a narrower gate — a minor-version range, or a per-release verified
list — the right instrument. The `1.0.89` handshake is evidence that a verified-compatible build is
refused today; a verified-incompatible build admitted by the range is the mirror image and would
weigh against it.

## Links

* [0026](0026-headless-retirement.md) — deletes the headless adapters and moves this table; states
  no gate policy.
* [0037](0037-protocol-first-control-plane.md) — drives workers over the vendors' own protocols, so
  the version gate is part of the transport assumption this ADR re-examines.
* [0014](0014-flat-op-discriminator-over-zod-discriminated-unions.md) — a discriminator's unknown
  arm, and why this repository's bias is to preserve what it does not recognise.
