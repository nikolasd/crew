# Future Features

**Audience & purpose:** maintainers deciding what to build next. A design parking lot for
consciously deferred features — nice-to-have, not blocking any planned milestone. Each entry
includes the concrete scenarios that would justify implementation. For genuinely open
implementation gaps (as opposed to deferred nice-to-haves): the open-items backlog is the
maintainer's local, gitignored `REVIEW.md` (not present in a fresh clone; its resolution
history lives in [`journal.md`](journal.md)) — that record, not this one, is the single
source of truth for unfinished work.

**Status:** All deferred. Revisit when a scenario becomes real.

---

## Display RPC Registration Surface

**Specified by:** Workspaces/Displays plan, Task 5  
**Deferred per:** M2/M3 gap-closure Decision #6  
**References:** `crates/protocol/src/method.rs`, `.../2026-07-22-crew-workspaces-displays.md` (Task 5), `.../2026-07-27-crew-m2-m3-gap-closure.md` (Decision 6)

### What it is

Four RPC methods for display client lifecycle management:

| Method | Purpose |
|---|---|
| `display/register` | Client announces itself to the runtime |
| `display/heartbeat` | Client signals liveness (expiry-based) |
| `display/unregister` | Client tears down cleanly |
| `display/list` | List all registered, live displays |

None of these exist in `CrewMethod` today. The built-in monitor works without them, subscribing to `events/replay` + `events/subscribe`.

### Scenarios that would justify implementation

**1. Third-party display as a first-class routing target**

An operator builds a web dashboard that wants to be a *display backend* — not just a read-only viewer, but a target the `DisplaySelector` can route runs to. Today the selector picks from a static list wired at daemon startup. `display/register` would let backends dynamically announce themselves, making the selector's pool live and extensible.

**2. Liveness-aware display routing**

You run both Herdr (terminal) and a web dashboard. Herdr crashes. Today the `DisplaySelector` still thinks Herdr is available (registered at startup, nothing tells the runtime it died). A new run gets routed to the dead backend, events go nowhere. `display/heartbeat` with expiry would detect the crash and remove it from the available set.

**3. Multi-tenant / shared daemon**

Five developers share one Crew daemon, each with their own display client (different terminals, different machines). `display/list` lets the operator see who's connected and what backends are active. `display/unregister` lets a dev's client clean up when they disconnect, so stale registrations don't pollute the pool.

**4. Operator visibility in `crewd doctor`**

Doctor's display check (TODO #21) currently checks if Herdr/tmux *could* work. With `display/list`, it could report which backends are actually registered and live right now, giving operators real-time visibility into display infrastructure health.

### Why deferred

- Only one display client exists today (the built-in monitor), and it works fine without registration
- The event-stream model (`events/replay` + `events/subscribe`) already supports read-only third-party viewers
- No operator has asked for a third-party display client
- Adds protocol surface area and state management (registry, heartbeat expiry) for a use case that doesn't exist yet
- Would be needed only if Crew becomes a shared daemon serving multiple independent display clients — a post-M4, multi-tenant scenario

### Decision trigger

Implement when any of the above scenarios becomes real (a third-party client is being built, a shared-daemon deployment is planned, or an operator asks for display visibility).

---

## Central organization configuration

**Context:** Crew retired the organization lock/policy system in favor of explicit repository-scoped configuration layers.

### What it is

A centrally managed, signed or otherwise authenticated organization configuration source that can distribute common adapter profiles, security patterns, and retention defaults without reviving a daemon-side task-graph authority.

### Why deferred

Local config layers are deterministic, offline-safe, and inspectable. A central source adds authentication, cache, availability, precedence, and incident-response semantics. It is not a replacement for OMP's task graph or per-run approval decisions.

### Decision trigger

Implement only when an organization operates multiple Crew repositories and needs centrally administered policy/profile defaults with a concrete authentication and offline-cache design.

---


## Copilot Adapter: Token Usage / Cost Reporting

**Blocked by:** ACP protocol version 1 (Copilot CLI)
**References:** `crates/runtime/src/adapter/copilot/client.rs`, TODO.md item 50 (retired)

### What it is

Report per-run token usage and cost for Copilot-driven workers, matching the
usage data the Claude and Codex adapters already surface.

### Why deferred

ACP v1's `session/update` frames carry no usage/cost fields at all — this is
a protocol limitation, not a gap in this codebase. The adapter already
reports `usage: none` honestly rather than fabricating a number. No amount
of local implementation work can produce data the vendor CLI never sends.

### Decision trigger

Implement when GitHub Copilot ships an ACP version newer than v1 that adds
usage/cost fields to session updates. Check `copilot --version` /
`agentInfo.protocolVersion` against `COPILOT_MAX_ACP_PROTOCOL_VERSION`
(`crates/runtime/src/adapter/copilot/compatibility.rs`) periodically, or when
bumping the pinned Copilot CLI version.

---

## Copilot Adapter: Nested-Worker (Unexpected Child) Observation

**Blocked by:** ACP protocol version 1 (Copilot CLI)
**References:** `crates/runtime/src/adapter/copilot/compatibility.rs`, `crates/runtime/tests/copilot_adapter.rs`, TODO.md item 51 (retired)

### What it is

Detect and report when a Copilot-driven worker spawns an unexpected
sub-agent ("nested worker"), matching the `NestedWorkerObserved` policy
signal the Claude and Codex adapters already raise.

### Why deferred

ACP v1 has no `session/update` variant for a vendor-spawned subagent at
all — there is no message to observe. `normalize.rs` correctly drops
unrecognized updates to zero events rather than fabricate a
`NestedWorkerObserved`. A test in `copilot_adapter.rs` already pins this:
it fails if `COPILOT_MAX_ACP_PROTOCOL_VERSION` is ever raised without a
corresponding mapping added, so the gap can't silently regress into a
false negative.

### Decision trigger

Implement when GitHub Copilot ships an ACP version newer than v1 that adds
a session-update variant for vendor-spawned subagents. Same trigger and
version check as the token-usage entry above — revisit both together.

---

## Org Governance Enforcement (Model/Adapter Allowlists, Cost Ceilings, Rollout Gates)

**Specified by:** crew-v2 gap-closure WP5 ruling (2026-08-22); supersedes the entries below
**References:** `crates/runtime/src/policy/evaluate.rs`, `crates/runtime/src/config/mod.rs`

### What it is

Before crew-v2, `RuntimePolicy` (fed by an org/repo/user YAML config layer) let an org
centrally impose: a model allowlist, an adapter allowlist, a required-capability list, a
per-run and a daily cost ceiling, and a `native_discovery_reviewed` rollout gate blocking
authorization of vendor-discovered nested workers. `PolicyEvaluator::evaluate` enforced all
five before every run's authorization.

crew.json (spec §10, `crew::CrewConfig`) deliberately does not model this org-governance
surface — the design spec retires the org config layer outright (§2.2/§12). WP5 deleted the
enforcement and the corresponding `RuntimePolicy` fields rather than keeping them
permanently inert, since that YAML layer was never actually wired up end to end (the
extension passed no config-path flags) and so was unreachable in every real deployment.
The wire-adjacent `PolicyViolationKind`/`PolicyError` enum variants
(`ModelNotAllowed`, `AdapterNotAllowed`, `CapabilityMissing`, `NativeDiscoveryUnacknowledged`,
`CostCeiling*`) stay declared, marked `Deprecated`, so a journaled event from before this
retirement stays deserializable; nothing constructs them any more.

Nested-worker *safety* is unaffected: the per-child record-intent-until-accepted/denied flow
(`coordination`'s child request + `policy/violation/decide`, `policy::violation`) is a
separate, untouched mechanism. What's gone is only the config-sourced pre-authorization gate
and the mid-run cost-ceiling enforcement, both of which required a config surface that no
longer exists.

### Why deferred

No operator has asked for centrally-managed policy since the rename; crew v2's scope is a
single-repo, single-operator tool. Reintroducing this needs a real config surface (the org
config layer this depended on is itself deferred below) before there's anything for it to
read.

### Decision trigger

Implement when an operator needs centrally-managed policy across a fleet of repos/machines —
the same trigger as the org config layer's own return, below, since this enforcement has no
inputs without it.

---

## Org Config: URL or File Path Support

**Specified by:** TODO.md Feature Requests section (retired 2026-08-06)
**Status:** the local org config layer this describes was itself retired by the crew-v2
design (spec §2.2/§12; removed by crew-v2 gap-closure WP5) — the entry below is preserved as
a record of the pre-crew-v2 idea, relevant again only if the org config layer itself returns.
**References:** `crates/runtime/src/config/merge.rs` (`load_layer`, no longer part of the
crate -- orphaned, pending deletion)

### What it is

Org-level config currently loads only from a local file path
(`--org-config /etc/crew/org.yaml`). This would let it also accept an
`http://`/`https://` URL, fetching and parsing the YAML remotely — e.g.
`--org-config https://config.example.com/org.yaml` — so a central org can
publish one config that every install pulls from instead of distributing
a file to each machine.

### Why deferred

No operator has asked for this; it's a speculative convenience, not a
reported gap. It also isn't a small bolt-on: it adds a network dependency
and a new failure mode (DNS, TLS, timeout, transient outage) to daemon
startup, which today is entirely offline-safe. Doing it properly means
deciding TLS certificate validation policy, timeout/retry behavior, and
whether/how to cache the last-fetched config so a network blip doesn't
prevent the daemon from starting at all.

### Decision trigger

Implement when an operator needs centrally-managed org config across
multiple machines/repos and file distribution (config management, shared
filesystem, etc.) is a real deployment problem for them.

---

## Config: Templates, Schema Validation, Versioning, Encryption

**Specified by:** TODO.md "Other Potential Features" backlog (retired 2026-08-06)
**Status:** written against the pre-crew-v2 YAML org/repo/user config
(`LayeredConfig`/`merge.rs`), removed by crew-v2 gap-closure WP5 in favor of `crew.json`
(`crates/runtime/src/config/crew.rs`). The four ideas below are unaffected in spirit (crew.json
could equally use templates/schema validation/versioning/encryption) but any implementation
would target `crew.rs`'s `load_layers`, not the paths named below.
**References:** `crates/runtime/src/config/crew.rs` (current), `crates/runtime/src/config/merge.rs`
(orphaned, pending deletion)

Four thin, undesigned ideas from the same backlog, grouped here since they
all touch the config-loading path and none has been scoped past a one-line
description.

### Config templates

**What it is:** A way to scaffold a new org/repo/user config from a named
template (e.g. `crew config init --template minimal`) instead of writing
YAML from scratch.

**Why deferred:** No concrete template catalog or user request exists yet —
just the idea that one might help onboarding. `crewd config init` now
partly answers this: it scaffolds a starter config from the full built-in
default snapshot, so blank-YAML authoring is no longer the starting point.
What's still missing is a *catalog* of named templates (`--template
minimal` or similar) for different shapes of setup, not just the one
built-in default.

**Decision trigger:** Implement a template catalog when onboarding friction
from the single built-in default becomes a reported problem, and a first
named template's shape is actually specified.

### Config schema validation before load

**What it is:** Validate an org/repo/user config file against a JSON Schema
before `LayeredConfig::load` merges it, producing a precise error (which
field, which file) instead of a parse failure or a runtime surprise from an
unrecognized/misspelled key.

**Why deferred:** `parse_config_file` already fails closed on invalid YAML;
the marginal gain is a better error message for typos in optional/unknown
fields, not a correctness gap. A schema now exists (#16:
`crew-config.schema.json`, generated by `render_config_schema()` and
exposed via `crewd schema`), so the remaining gap is narrower than it was
— it's wiring that already-generated schema into `crew.rs`'s
`load_layers` to validate a config file *before* merge, not generating one
from scratch.

**Decision trigger:** Implement if misconfigured deployments (typo'd keys
silently ignored, wrong types) become a recurring support cost.

### Config versioning and migration

**What it is:** A version field in config files plus a migration path, so an
older config format keeps loading (auto-upgraded) after a breaking config
schema change, instead of requiring every operator to hand-edit their files.

**Why deferred:** The config schema hasn't broken compatibility yet — there
is no migration to write. Speculative versioning infrastructure ahead of a
real breaking change is pure overhead.

**Decision trigger:** Implement the first time a config schema change would
otherwise break existing deployed config files.

### Config encryption for sensitive values

**What it is:** Allow secrets embedded in config (API keys, tokens) to be
stored encrypted at rest rather than plaintext YAML, decrypted on load.

**Why deferred:** No sensitive values currently live in Crew's own config
files — credentials for adapters are handled by each vendor CLI's own auth
(e.g. `codex login`), not stored in `crew.yaml`. Encrypting a file that
holds no secrets today is speculative.

**Decision trigger:** Implement if/when a config field is added that must
hold a real secret (e.g. a remote-config auth token, once URL-based org
config from the entry above is built).

---

## How to use this document

1. **Adding a future feature:** Append a new section with the feature name, what it is, concrete scenarios that justify it, why it's deferred, and a decision trigger.
2. **Revisiting:** When a scenario becomes real, implement the feature and remove it from this document.
3. **Closing without implementing:** If a feature is no longer relevant, move it to `docs/journal.md` with a "retired" note and remove it from here.

This document is **not** a TODO list — it's a design parking lot. Items here are consciously deferred, not forgotten.
