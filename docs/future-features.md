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

None of these exist in `BatmanMethod` today. The built-in monitor works without them, subscribing to `events/replay` + `events/subscribe`.

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

## Real Pane/Window Creation for Worker Visibility

**References:** `crates/runtime/src/display/tmux.rs` (`TmuxDisplay::create_pane`), `crates/runtime/src/display/herdr.rs` (`HerdrDisplay::create_pane`), `crates/runtime/src/display/terminal.rs`, `crates/runtime/src/service/orchestration.rs` (`start_queued_run`), `crates/runtime/src/adapter/registry.rs`

### What it is

Every worker run should be visibly started for the user, in whichever display backend is in play: a new tmux pane/window when the tmux backend is selected, a new Herdr pane when Herdr is selected, a new terminal window when falling back to the plain terminal backend. A worker should never just be a silent background process the user has no window onto.

### Current state (verified 2026-08-21)

`TmuxDisplay::create_pane` and `HerdrDisplay::create_pane` are fully implemented and unit-tested — they genuinely call `tmux split-window`/`new-window -P -F` and `herdr pane split`/`run`/`report-agent` respectively, and would open a real pane if invoked. But neither is called anywhere in the orchestration path — the only call sites in the whole `crates/runtime/src` tree are inside `HerdrDisplay`'s own `#[cfg(test)]` module.

`start_queued_run` (`orchestration.rs`) only calls `DisplayRegistry::resolve`, which checks backend *availability* and picks a name — it explicitly never activates one: "the registry resolves availability without activating a backend, so no vendor pane id exists yet." It then journals a `DisplayPaneAttached` event with an empty pane-id placeholder, for bookkeeping/replay only.

`TerminalDisplay` has no pane/window-opening capability at all today — no `create_pane` equivalent exists for it, so there is currently no code path that opens a new terminal window either.

The worker process itself is always spawned by the supervisor (`crates/runtime/src/supervisor/process.rs`) with fully piped stdio, in its own process group, regardless of which display backend was selected. The only thing a user can currently see live is the embedded read-only `/crew` monitor inside OMP, which tails the event journal — not a real pane or window running the worker. Checked the TypeScript extension (`packages/extension/src/`) too: nothing there shells out to `tmux`/`herdr` or opens a terminal either.

### Why this hasn't been wired up

- The event-stream model (`events/replay` + `events/subscribe`, surfaced via the embedded monitor) already gives a working way to watch a run, so nothing forced this further.
- Wiring `create_pane` into `start_queued_run` requires design decisions that haven't been made yet: what actually runs inside the opened pane/window (the raw vendor CLI directly, or `crewd monitor --run-id <id>` tailing that run's events), who closes the pane/window when the run settles vs. when the daemon restarts mid-run, and what happens if pane creation itself fails partway through a run that's already started.
- `TerminalDisplay` additionally needs real window-spawning logic added (an OS-specific "open a new terminal emulator running X" command) — it currently does nothing beyond reporting itself as always-available.

### Decision trigger

This is the maintainer's stated expectation for how Crew should behave — every worker visibly started in a pane (tmux/Herdr) or a new terminal window (plain-terminal fallback) — so treat this entry as scoping outstanding wiring work, not as an indefinitely-deferred nice-to-have. Revisit as soon as the pane-lifecycle questions above are settled.

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

## Org Config: URL or File Path Support

**Specified by:** TODO.md Feature Requests section (retired 2026-08-06)
**References:** `crates/runtime/src/config/merge.rs` (`load_layer`)

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
**References:** `crates/runtime/src/config/merge.rs`, `crates/runtime/src/config/`

Four thin, undesigned ideas from the same backlog, grouped here since they
all touch the config-loading path and none has been scoped past a one-line
description.

### Config templates

**What it is:** A way to scaffold a new org/repo/user config from a named
template (e.g. `crew config init --template minimal`) instead of writing
YAML from scratch.

**Why deferred:** No concrete template catalog or user request exists yet —
just the idea that one might help onboarding.

**Decision trigger:** Implement when onboarding friction from blank-config
authoring becomes a reported problem, and a first template's shape is
actually specified.

### Config schema validation before load

**What it is:** Validate an org/repo/user config file against a JSON Schema
before `LayeredConfig::load` merges it, producing a precise error (which
field, which file) instead of a parse failure or a runtime surprise from an
unrecognized/misspelled key.

**Why deferred:** `parse_config_file` already fails closed on invalid YAML;
the marginal gain is a better error message for typos in optional/unknown
fields, not a correctness gap. No schema exists to validate against yet —
would need to be generated from the config structs.

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
