# Future Features

**Audience & purpose:** maintainers deciding what to build next. A design parking lot for
consciously deferred features — nice-to-have, not blocking any planned milestone. Each entry
includes the concrete scenarios that would justify implementation. For genuinely open
implementation gaps (as opposed to deferred nice-to-haves): the release checklists and
live-conformance evidence under [`release/`](../release/) track what remains before a version
ships, and [`docs/adr/`](adr/) records how each one was resolved — those, not this document, are
the source of truth for unfinished work.

**Status:** All deferred. Revisit when a scenario becomes real.

---

## Display RPC Registration Surface

**Specified by:** Workspaces/Displays plan, Task 5  
**Deferred per:** the M2/M3 gap-closure review's decision to defer this  
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

You run both Herdr (terminal) and a web dashboard. Herdr crashes. The `DisplaySelector` still thinks Herdr is available — registered at startup, with nothing telling the runtime it died — so a new run gets routed to a dead backend.

Partly narrowed since this was written: `display::pane_socket::is_live` established a connect-probe definition of pane liveness, used by `pane/reopen` and by the startup sweep that unlinks dead sockets ([ADR-0027](adr/0027-turn-end-settles-a-run.md)). That answers "is this *pane* alive" for a Crew-owned attach socket. It does not answer "is this *backend* still able to accept new panes", which is what routing needs, and it says nothing about a backend that never had a Crew socket in the first place. So the scenario stands, but its premise is no longer "nothing detects a dead backend" — it is that pane-level liveness does not generalize to backend-level routing.

**Cross-note:** the crewd-served web monitor must be scoped as a **read-only viewer** built on `events/subscribe` + `events/replay`. If it is instead allowed to receive routed runs, it becomes a display backend and this entry's trigger fires — a registration surface would then be required rather than deferred.

**3. Multi-tenant / shared daemon**

Five developers share one Crew daemon, each with their own display client (different terminals, different machines). `display/list` lets the operator see who's connected and what backends are active. `display/unregister` lets a dev's client clean up when they disconnect, so stale registrations don't pollute the pool.

**4. Operator visibility in `crewd doctor`**

Doctor's display check currently checks if Herdr/tmux *could* work. With `display/list`, it could report which backends are actually registered and live right now, giving operators real-time visibility into display infrastructure health.

### Why deferred

- Only one display client exists today (the built-in monitor), and it works fine without registration
- The event-stream model (`events/replay` + `events/subscribe`) already supports read-only third-party viewers
- No operator has asked for a third-party display client
- Adds protocol surface area and state management (registry, heartbeat expiry) for a use case that doesn't exist yet
- Would be needed only if Crew becomes a shared daemon serving multiple independent display clients — a post-M4, multi-tenant scenario

### Decision trigger

Implement when any of the above scenarios becomes real (a third-party client is being built, a shared-daemon deployment is planned, or an operator asks for display visibility).

---

## Nameable Workers (Optional Display Name)

**References:** `crew_worker` / `worker/create`
(`crates/runtime/src/service/orchestration.rs`), the monitor widget
(`packages/extension/src/monitor/`), the dashboard's run table and worker cards
(`crates/runtime/src/dashboard/`)

### What it is

An optional display name on `worker/create`, carried on the worker row and shown
wherever a worker is currently identified by a truncated id — the monitor widget,
the dashboard's run table, and its worker cards.

### Why deferred

Every surface now labels a worker `adapter · model` in that runtime's brand
colour, which is what a reader actually wants to know and needs no new field to
produce. A name only starts earning its keep once that label stops
disambiguating, and it is not free: a name is a second identity for a thing that
already has one, so it needs a uniqueness decision (or an explicit decision not
to have one), it has to survive a harness replacement that creates a new worker,
and every surface has to decide what to show when it is absent. None of that is
worth settling before the label is observably insufficient.

### Decision trigger

The maintainer asks again once several workers on the *same* adapter and model
run concurrently — at that point every row reads identically and only the
truncated id separates them, which is precisely the readability problem the
brand labels were introduced to fix.

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
**References:** `crates/runtime/src/adapter/tui/copilot.rs` (the headless
`adapter/copilot/client.rs` this originally cited was removed by crew-v2 gap-closure's headless
retirement — see [ADR-0026](adr/0026-headless-retirement.md)), TODO.md
item 50 (retired)

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
**References:** `crates/runtime/src/adapter/tui/copilot_compatibility.rs` (moved from the retired
headless `adapter/copilot/compatibility.rs` by crew-v2 gap-closure's headless retirement — see
[ADR-0026](adr/0026-headless-retirement.md)), TODO.md item 51 (retired)

### What it is

Detect and report when a Copilot-driven worker spawns an unexpected
sub-agent ("nested worker"), matching the `NestedWorkerObserved` policy
signal the Claude and Codex adapters already raise.

### Why deferred

ACP v1 has no `session/update` variant for a vendor-spawned subagent at
all — there is no message to observe. `normalize.rs` correctly drops
unrecognized updates to zero events rather than fabricate a
`NestedWorkerObserved`. A test already pins this (formerly in the now-deleted
`copilot_adapter.rs`, moved with the rest of this table to
`adapter/tui/copilot_compatibility.rs`'s own inline tests by crew-v2 gap-closure): it fails
if `COPILOT_MAX_ACP_PROTOCOL_VERSION` is ever raised without a corresponding mapping added, so the
gap can't silently regress into a false negative.

### Decision trigger

Implement when GitHub Copilot ships an ACP version newer than v1 that adds
a session-update variant for vendor-spawned subagents. Same trigger and
version check as the token-usage entry above — revisit both together.

---

## Org Governance Enforcement (Model/Adapter Allowlists, Cost Ceilings, Rollout Gates)

**Specified by:** the crew-v2 gap-closure ruling (2026-08-22); supersedes the entries below
**References:** `crates/runtime/src/policy/evaluate.rs`, `crates/runtime/src/config/mod.rs`

### What it is

Before crew-v2, `RuntimePolicy` (fed by an org/repo/user YAML config layer) let an org
centrally impose: a model allowlist, an adapter allowlist, a required-capability list, a
per-run and a daily cost ceiling, and a `native_discovery_reviewed` rollout gate blocking
authorization of vendor-discovered nested workers. `PolicyEvaluator::evaluate` enforced all
five before every run's authorization.

crew.json (`crew::CrewConfig`) deliberately does not model this org-governance
surface — the org config layer was retired outright. That ruling deleted the
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

## Headless Control Plane

**Specified by:** the crew-v2 gap-closure ruling (2026-08-22)
**References:** [`docs/adr/0026-headless-retirement.md`](adr/0026-headless-retirement.md),
[`docs/adr/0025-crew-v2-tui-control-plane.md`](adr/0025-crew-v2-tui-control-plane.md)

### What it is

Before crew-v2, each of the four worker adapters (Claude, Codex, Copilot, OMP-RPC) had two
independent implementations: a headless one driving each vendor's own non-interactive/JSON
protocol directly (`claude stream-json`, `codex app-server`, `copilot --acp`, `omp --mode rpc`),
and a TUI one driving the real interactive CLI on a PTY. That ruling deleted every headless
implementation, its fixtures, and its conformance suite. `mode: "headless"` stays deserializable
(an old journal or config naming it must still parse) but is typed-rejected at both
config-validation and adapter-dispatch time — never silently remapped to `tui` and never silently
accepted.

### Status: no longer deferred

This entry recorded a deferred option: implementing a non-interactive control plane again, designed
fresh against the vendors' current protocols rather than by resurrecting the deleted code. That has now
happened for one vendor — claude is driven over its streaming-JSON protocol by a new adapter, selected
explicitly per worker and never by default. It is experimental, under evaluation, and not yet
recommended for use. The `headless` mode name remains typed-rejected; the new adapter does not revive it.

The trigger this entry anticipated was a deployment with no pseudo-terminal available. That is not why
the work was done. The reason was that the failures of the terminal control plane were consistently
failures of reading a screen rather than of any one vendor — a different argument from the reasoning
this entry originally recorded, and one it did not consider.

What remains genuinely deferred is the same treatment for the other three vendors, and the question of
which control plane is primary. Both are under evaluation by a pending ADR rather than deferred here, so
this entry stays only as the record of a decision trigger that fired for a reason nobody wrote down in
advance.

---

## Config: Templates, Schema Validation, Versioning, Encryption

**Specified by:** TODO.md "Other Potential Features" backlog (retired 2026-08-06)
**Status:** written against the pre-crew-v2 YAML org/repo/user config
(`LayeredConfig`/`merge.rs`), removed by the crew-v2 gap-closure ruling in favor of `crew.json`
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
fields, not a correctness gap. A schema now exists (`crew-config.schema.json`, generated by `render_config_schema()` and
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

## True tabs for Terminal.app (and pre-1.3.0 Ghostty)

**Specified by:** deferred from the host-terminal pane-following feature's first cut (2026-08-29)
**References:** `crates/runtime/src/display/os_window.rs`,
[`docs/adr/0025-crew-v2-tui-control-plane.md`](adr/0025-crew-v2-tui-control-plane.md)

### What it is

That work made worker panes follow the host terminal instead of always assuming Terminal.app, and two
of the three supported targets already open a **real tab** in the window the user is looking at:

| Target | Shipped behaviour |
|---|---|
| iTerm2 | real tab (`create tab with default profile`, then `write text`) |
| Ghostty 1.3.0+ | real tab (`new tab in window 1 with configuration {command:...}`) |
| Ghostty pre-1.3.0 | plain new window (`open -na Ghostty --args -e`) |
| Terminal.app, or an absent/unrecognized hint | plain new window (`do script`, then `activate`) |

What remains deferred is a real tab for the last two rows. Both report
`DisplayPlacement::Window` honestly rather than claiming a tab they did not open, so this is a UX
gap, not a correctness one.

### Why deferred

**Terminal.app has no externally-triggerable tab-creation command.** Its AppleScript dictionary
exposes `do script`, which opens a window; there is no supported "open a tab in the frontmost
window running this command". The known workaround is synthesizing a ⌘T keystroke through System
Events, which requires Accessibility permission, breaks silently when that permission is absent or
revoked, and depends on keyboard-shortcut configuration the user can change. Trading a reliable
window for an unreliable tab is the wrong direction — especially against a backend whose previous
bug was opening windows the user never saw.

**Ghostty pre-1.3.0 has no AppleScript tab command at all.** The shipped code feature-detects this
by attempting the command and falling back, rather than checking a version string, so those installs
degrade to a window automatically and resolve themselves as users upgrade. A non-AppleScript CLI
path for tab creation is tracked upstream as
[ghostty-org/ghostty#12136](https://github.com/ghostty-org/ghostty/issues/12136) ("CLI: support
opening new tabs in an existing window"), still unimplemented — AppleScript is the path that shipped
in 1.3.0. If the CLI path lands it would also serve installs where scripting is unavailable or
undesirable.

The honest framing: for iTerm2 and current Ghostty, "panes follow the host" already means a tab in
the window you are working in. For Terminal.app it means a foregrounded window of the right
application, which is a large improvement over the earlier behaviour of an un-activated window of
the *wrong* application, and may simply be good enough.

### Decision trigger

Either of:

1. **Upstream support lands** — Terminal.app gains a scriptable tab command (unlikely), or Ghostty
   ships a CLI/IPC path worth using in place of AppleScript.
2. **Real complaints about the new-window UX.** Not speculation about it: an operator on
   Terminal.app saying the window-per-worker behaviour is disruptive in practice. Until then the
   window is reliable and the tab is not.

Explicitly *not* a trigger: wanting parity across terminals for its own sake. The placement is
reported honestly per backend, so a caller that cares can already tell what it got.

---

## True Graceful Stop (`crew_stop { outcome: "done" }`)

**Specified by:** the skills-audit tool-surface fixes (2026-08-30)
**References:** `packages/extension/src/tools/leader.ts` (`registerStopTool`), `crates/runtime/src/service/orchestration.rs` (`run_cancel`)

### What it is

`crew_stop`'s two `outcome` values used to imply a real behavioral distinction: `'done'` documented
as "graceful wrap-up then soft cancel (the worker finishes its current turn)" versus `'abort'`'s
"immediate cancel". The extension backed this by sending `run/cancel` a `mode: "soft"` parameter for
`'done'`. The daemon's `run_cancel` handler never reads a `mode` parameter at all -- both outcomes
call the identical immediate-cancel path (`CancelScope::Worker`, which kills the vendor process
right away). The only real difference between the two outcomes is that `'done'` fires a courtesy
`message/send` follow-up a moment before the same kill; there is no grace period, and no
server-side distinction between "let it finish its turn" and "stop it now."

That fix removed the dishonesty (removed the ignored `mode` parameter, rewrote the tool description to
say plainly that both outcomes kill immediately). This entry tracks the feature the old description
was describing, in case it's ever worth actually building: a `run/cancel` (or dedicated `run/stop`)
mode that gives a live vendor process a bounded window to reach its own turn boundary --
`waitingUser`, a natural pause point -- before the hard kill, rather than killing mid-turn every
time.

### Why deferred

- No adapter today exposes a "wrap up your current turn" signal a supervisor could send and wait on
  -- building this would mean either a new coordination-channel message every adapter kind has to
  handle, or a bounded wait against activity/turn-end evidence that already exists (the same
  `TurnEnded` fact `run/finish`/`run/result` read) without actually asking the vendor to hurry.
- No operator has reported that immediate-kill loses meaningful in-flight state today. The
  vendor's own transcript/journal already captures everything up to the kill; what a grace period
  would additionally protect is unclear without a concrete failure report.
- A naive implementation (block `crew_stop` until turn-end or a timeout) would make a tool the
  leader expects to be fast into one with an unbounded-feeling latency tail, trading one honesty
  problem (claims a distinction that isn't there) for a responsiveness one.

### Decision trigger

Either of:

1. **A concrete report of lost in-flight state** from an immediate kill that a bounded grace window
   would have prevented -- not speculation that graceful stops are "generally better."
2. **An adapter gains a real "wrap up now" signal** (a coordination message a vendor process can act
   on to reach its own turn boundary early) for an unrelated reason, making a genuine graceful stop
   cheap to wire rather than a new mechanism built just for this.

Explicitly *not* a trigger: wanting the two outcome names to mean something different from each
other for its own sake -- the current fix (say what each one actually does) already resolves that
without inventing new server behavior.

---

## `run/list` Canonical Result Type

**Specified by:** the RunMessage codegen fix (2026-08-30)
**References:** `crates/protocol/src/run.rs` (`Run`), `crates/runtime/src/service/query.rs`
(`run_list_op`/`row_to_run_json`), `crates/runtime/src/service/orchestration.rs:1130` (`run_list`)

### What it is

`message/list` and every other multi-row read RPC now has a canonical protocol result
type, schema-validated via Ajv on the extension side. `run/list` is the one holdout: its result is
still validated only structurally (any JSON object passes), the same gap `RunMessage` had before
that fix closed it.

The gap isn't an oversight of the same shape, though — it was investigated, not just noticed.
`run_list_op`'s actual wire response (`row_to_run_json`) includes two fields the existing `Run`
protocol type doesn't declare at all: `createdAt` (required) and `policyFingerprint` (optional, the
merged-policy snapshot a run was authorized under). `Run` is not dormant: it's the *write-side*
domain type, constructed at submit time (`repo.submit_run(&run, ...)`) before either field exists or
is assigned, and used that way at `event_sink.rs:815,1033,1335`, `run_lifecycle.rs:580`,
`orchestration.rs:1020`, `audit/retention.rs:300`, and `adapter/registry.rs:2428`. `run/list`'s
actual response is the *read-side* shape — a genuinely different struct, not a partially-filled-in
version of the same one.

### Why deferred

Closing this cleanly needs one of two real design choices, not a mechanical fix:

1. **A second, read-side struct** (e.g. `RunSummary`, holding everything `row_to_run_json` returns)
   distinct from `Run`, with a mapping between them wherever both are in scope. More wire surface,
   two types to keep in sync by hand where their fields overlap.
2. **Redefine `Run` itself** to carry `created_at`/`policy_fingerprint` as optional fields, changing
   what every write-side call site constructs (each would need to either supply `None` explicitly or
   rely on a default) and blurring `Run`'s current contract as "the shape submit-time code
   constructs" into "the shape submit-time code constructs, plus fields it never has yet."

Neither is a wire-safety fix on the order of `RunMessage`'s -- both are a real API-shape decision
with call-site consequences, so that fix reported this rather than picking one under an unrelated
tool-surface-honesty fix's scope.

### Decision trigger

Either of:

1. **A `run/list` consumer needs schema-validated fields beyond what structural checking already
   covers** — a concrete bug or drift traced to `run/list`'s unchecked shape, not a hypothetical.
2. **A third read-side list RPC needs the same treatment**, making the "just `Run` isn't reusable
   here" case worth solving once with a general read-side-result convention rather than per-method.

Explicitly *not* a trigger: closing the gap for symmetry with `message/list` alone -- the two
methods' underlying shapes are different in a way that matters, not merely presented differently.

---

## Escalations Carry the Worker's Actual Question

**Specified by:** maintainer ruling, deferred to post-E2E implementation
**References:** `crates/protocol/src/event.rs` (`EscalationRaised.question: Option<Redacted>`),
`crates/runtime/src/adapter/run_lifecycle.rs` (repeated-failure trigger),
`crates/runtime/src/domain/repository.rs` (`raise_write_violation_if_declared_read_only`,
`record_adapter_event`'s `WorkerQuestion` handling)

### What it is

`EscalationRaised.question` is populated at only one of its three effective call sites today. A
worker's own question (`kind = 'question'`, raised when the vendor transcript surfaces one) already
carries real, sanitized text — that path works and is the existence proof the field's shape is
right. The other two — a run's second consecutive failure (`reason: "repeated_failure"`) and a
write-shaped tool running against a subtask declared read-only (`reason: "write_violation"`) — pass
`None`. Both should instead carry something that tells OMP what actually happened, not just the
fixed code naming the kind of escalation it is.

### Why deferred

Ruled in scope, but implementation is deliberately scheduled after the supervised live E2E rather
than folded into the v0.7.0 cut: neither site's exact text is a mechanical fill-in. The
`write_violation` site needs no new data (the tool name and the plan/subtask ids are already local
values at the call site), but the `repeated_failure` site does — nothing in scope there today
captures what the run was doing when it failed twice, only that it did. Closing that gap is a real
design choice (thread the exited process's exit code/signal one call frame down, versus querying the
run's last visible message from the journal), not something to pick under an unrelated ticket's
scope.

### Decision trigger

Already triggered — implement after the v0.7.0 supervised E2E. Whoever picks this up still needs to
settle, at the `repeated_failure` site specifically, what evidence to surface and whether it needs
plumbing beyond what's already in scope there — the `write_violation` site has no equivalent
question, since everything it needs is already a local value at its call site.

---

## Legacy `/crew-status`, `/crew-doctor`, `/crew-config` Forwarders (2026-08-29)

**References:** `packages/extension/src/index.ts:5-6`

### What it is

Three deprecation forwarders — `/crew-status`, `/crew-doctor`, `/crew-config` — kept alongside their
replacements (`/crew health`, `/crew doctor`, `/crew config`) so a script or muscle-memory habit built
against the pre-consolidation command names keeps working. The module's own entry-point comment
documents them, verbatim: "The legacy `/crew-status`, `/crew-doctor`, `/crew-config` are deprecation
forwarders removed in the release after next" (`index.ts:5-6`).

### Why deferred

The forwarders cost nothing to keep running today and removing them before anyone who depended on
the old names has had a release cycle to notice would trade a small maintenance saving for a real
breakage. The comment's own "release after next" already states when removal is due; there is
nothing to design, only a date to keep.

### Decision trigger

The comment was written against 0.6.0, the version shipped at the time — so "next" is 0.7.0 and "the
release after next" is 0.8.0. Remove the three forwarders (and this entry) as part of the 0.8.0
checklist.

---

## How to use this document

1. **Adding a future feature:** Append a new section with the feature name, what it is, concrete scenarios that justify it, why it's deferred, and a decision trigger.
2. **Revisiting:** When a scenario becomes real, implement the feature and remove it from this document.
3. **Closing without implementing:** If a feature is no longer relevant, remove it from here and record the retirement in an ADR (or in [engineering-lessons.md](engineering-lessons.md) if the reason is a lesson rather than a decision).

This document is **not** a TODO list — it's a design parking lot. Items here are consciously deferred, not forgotten.
