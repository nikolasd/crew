# Headless control plane retirement

* Status: Accepted
* Date: 2026-08-27

## Context and Problem Statement

Before crew v2, every worker adapter (Claude, Codex, Copilot, OMP-RPC) shipped two independent
implementations: a headless one driving each vendor's own non-interactive/JSON protocol directly
(`claude stream-json`, `codex app-server`, `copilot --acp`, `omp --mode rpc`), and a TUI one
driving the real interactive vendor CLI on a PTY (ADR-0025). Crew v2's design made the TUI path
primary; the headless path kept running alongside it, covered by its own fixtures and
conformance suite, with no operator depending on it specifically.

Carrying two adapter implementations per vendor doubles the code, the fixtures, and the
conformance surface that must be kept correct — for a path crew-v2 gap-closure's audit found no
real deployment reaching. The design spec (§4.6) calls for retiring it outright rather than
leaving it permanently inert and untested.

## Decision Drivers

* An unreachable-in-practice code path is a liability (staleness risk, false sense of coverage),
  not a free option — matching the reasoning that already retired org-governance enforcement
  (`docs/future-features.md`, "Org Governance Enforcement").
* Old journals and configs that recorded `mode: "headless"` before this retirement must still
  *deserialize* — replay and `crewd status`/`doctor` must not hard-crash on history.
* A retired mode must never be silently remapped (e.g. to `tui`) or silently accepted: a caller
  that explicitly asked for headless must get a typed, explicit refusal, not different behavior
  than requested.
* crew-v2 gap-closure's own process rule: adapter deletions are a same-commit, all-call-sites
  operation — a half-retired mode (accepted in config but refused at dispatch, or vice versa) is
  worse than either fully-kept or fully-removed.

## Considered Options

* Keep the headless adapters, permanently inert behind a feature flag.
* Remove `mode: "headless"` entirely, including from the wire schema — a value naming it becomes
  a hard parse error.
* Keep `mode: "headless"` deserializable (schema/enum variant intact) but reject it with a typed
  error at every point that would otherwise act on it (config validation, adapter-registry
  dispatch, resume/recovery eligibility, the `crewd conformance`/`crewd adapters` CLI surface).

## Decision Outcome

Chosen option: **deserializable but rejected**. `AdapterMode::Headless` remains a valid enum
variant in both `crates/protocol` (wire) and the config/profile layers, so:

* A journal entry or config file written before this retirement still parses.
* Every path that would previously have dispatched to a headless adapter now returns a typed
  error naming the retirement (`ConfigError::HeadlessModeRetired`,
  `RegistryError::HeadlessControlPlaneRetired`, and the equivalent `recovery.rs` resume-eligibility
  and `cli.rs` conformance-mode rejections) instead of silently falling back to `tui` or
  succeeding as if nothing changed.
* The four headless adapter implementations (`adapter::{claude,codex,copilot,omp_rpc}`), their
  fixtures, and their conformance suites are deleted — not kept dark behind a flag — since dead
  code that nothing exercises is exactly the staleness risk this retirement exists to remove. The
  one exception is Copilot's CLI/ACP-protocol-version compatibility table
  (`adapter/copilot/compatibility.rs`), which encodes empirically-verified facts about the vendor
  CLI independent of headless vs. TUI dispatch — moved to `adapter/tui/copilot_compatibility.rs`
  rather than deleted.
* `crewd conformance --mode headless` and any journaled/configured `mode: "headless"` produce the
  same class of typed, explicit rejection rather than three different failure shapes across three
  call sites.

This supersedes one sentence of ADR-0025 ("TUI and headless adapters share lifecycle, budget,
timeout, and ownership semantics") — headless adapters no longer exist to share anything; TUI
adapters are now the entire adapter layer.

### Positive Consequences

* Old history stays replayable without special-casing every reader against a variant that no
  longer has a code path.
* A caller that explicitly asks for the retired mode gets an explicit, correctly-worded answer
  ("retired... use mode: tui") instead of a generic type error, a silent remap, or — worse — a
  crash deep in adapter dispatch.
* One vendor implementation per adapter halves the surface `docs/compatibility.md` and the
  conformance fixtures need to track going forward.

### Negative Consequences

* A deployment that genuinely needs a non-interactive, PTY-free control plane (e.g. a CI runner
  with no pane backend available at all) has no in-tree path back to one; see
  `docs/future-features.md`'s "Headless Control Plane" entry for the decision trigger to revisit.
* Two enums (`config::crew::AdapterMode`, `adapter::profile::AdapterMode`) each carry a variant
  that can never again be constructed by anything but historical deserialization — a shape that
  is easy to misread as "still supported" without this ADR for context.

## Links

* ADR-0025 — crew v2 TUI control plane (this ADR amends one sentence of it)
* `docs/future-features.md` — "Headless Control Plane" (decision trigger to revisit)
* `docs/superpowers/specs/2026-08-22-crew-v2-design.md` §4.6
* `.superpowers/sdd/2026-08-22-crew-v2-gap-closure/wp-c.md`, `wp-c-report.md`
