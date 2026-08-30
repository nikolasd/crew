# Manual Testing Guide

**Audience & purpose:** contributors doing pre-release or post-change QA — a companion to
[development.md](development.md), the developer manual. Not for end users; if you're
looking for how to *use* Crew rather than verify a change to it, see
[user-guide.md](user-guide.md).

Every automated suite (`bun run check`) runs without a model call and without a human watching a
screen. Some things can only be verified by actually running `omp`, calling a tool with a real
model, and looking at what comes back — this document is the complete, current list of those
checks: what to run, in what order, exactly what you should see, and what it means if you don't.

Run these after any change that touches the daemon lifecycle, the IPC layer, an orchestration RPC
method, an OMP tool, or the monitor — the automated suites can tell you a function returns the
right value; only these checks can tell you the *whole system*, wired together, still behaves the
way `architecture.md` says it does.

## Prerequisites

Same as [development.md](development.md#prerequisites): Rust 1.97.1+, Bun 1.3.14+,
`omp` ≥ 17.0.7 on your `PATH`. Build both sides first:

```bash
bun run setup   # installs JS deps + builds the crewd runtime
bun run build   # bundles the OMP extension to dist/index.js, loaded below
```

## Environment variables and configuration

Several environment variables control Crew's behavior. Set these once per shell session:

```bash
# Override the state directory location (must be absolute)
export CREW_STATE_DIR=/path/to/state

# Vendor CLIs (claude, codex, copilot, the local omp model server) are ordinary installed
# dependencies. Live conformance and the availability probe run by default -- no gate needs to be
# set to exercise a real vendor CLI. Set this only to forbid observation-only vendor invocation
# (live conformance suites, the availability probe, #[ignore]d live tests) on a machine without
# the CLIs installed, or in CI:
export CREW_DISABLE_VENDOR_CLI=1

# Path override for the crewd binary (bypasses packaged binary discovery)
export OMP_CREW_BINARY="$PWD/target/debug/crewd"
```

**State directory resolution** (in precedence order) — applied identically whether the extension
resolves `--state-dir` before spawning `crewd`, or you omit `--state-dir` on a bare `crewd`
invocation (`StateRoot::resolve`, `crates/runtime/src/security/mod.rs`):
1. `CREW_STATE_DIR` (must be absolute)
2. `$XDG_STATE_HOME/omp/crew` when `XDG_STATE_HOME` is set (must be absolute) -- or its legacy
   `$XDG_STATE_HOME/omp/batman` sibling, if only that one exists
3. `$HOME/${PI_CONFIG_DIR:-.omp}/crew` -- or its legacy `$HOME/${PI_CONFIG_DIR:-.omp}/batman`
   sibling, if only that one exists

**Configuration file locations** (in precedence order, lowest to highest). The CLI itself has no
auto-discovery and no `CREW_CONFIG`-style environment variable — each layer is loaded only from a
path passed explicitly via repeatable `--config <path>`; the extension resolves and passes these
for you:
1. **User config** — `~/.omp/crew.json`
2. **Project config** — `<repo>/.omp/crew.json`

Configuration files are strict JSON (`crew.json`, spec §10) with unknown-key rejection at every
depth, failing closed with the exact JSON path that named the unknown key. Example:

```json
// ~/.omp/crew.json
{
  "limits": { "maxConcurrentWorkers": 4 },
  "retention": { "period": "30d", "maxRuns": 20 },
  "display": { "backend": "auto" },
  "security": {
    "patterns": [
      "AKIA[0-9A-Za-z]{16}",
      "sk-[a-zA-Z0-9]{32}"
    ]
  }
}
```

**Security notes:**
- `security.patterns` is additive across layers (concatenated, never replaced), so a lower layer's
  redaction patterns can never be silently dropped by a higher one. An org pattern that fails to
  compile as a regex refuses the daemon's startup rather than degrading to built-in rules only.
- Vendor CLIs are ordinary installed dependencies; live conformance and the availability probe run by default. `CREW_DISABLE_VENDOR_CLI=1` should always be set in CI jobs or unattended runs — it forbids observation-only vendor invocation and guarantees no billed model call is made.

## Owning what you test

Two things get rebuilt independently — the `crewd` binary and the extension bundle
(`packages/extension/dist/index.js`) — and it's easy to run a check below while actually still
exercising the *old* build of either one. Before trusting any result in this document, make sure
you're exercising the build you just made.

**The extension side has two speeds.** Point `--extension` directly at
`packages/extension/src/index.ts` for fast iteration — Bun runs TypeScript directly, so an edit
takes effect on the next `omp` invocation with no build step. Only switch to `bun run build` +
`packages/extension/dist/index.js` (what every example below uses) when you want to test the exact
bundle that ships — `dist/index.js` is a separate build output and silently lags your source if you
edit `src/` and forget to rebuild it.

**The daemon side does not restart itself.** `crewd` runs detached, and OMP connects-or-spawns
(ADR-0008, §1 below) — if a daemon started from your *previous* build of `crewd` is still alive, a
fresh `omp` session just reconnects to it over its Unix socket. Rebuilding `crewd` and re-running a
check without killing the old daemon first tests nothing new. After any Rust change:

```bash
crewd stop --repo "$PWD"   # or: pkill -f "crewd serve"
```

before your next `omp` invocation. `--state-dir` can be omitted here — the bare CLI resolves the
same state root the extension does (see
[cli-reference.md's state-directory note](cli-reference.md#before-you-start-state-directories))
— but only pass it explicitly when your current shell's environment might not match the one the
extension used to spawn the daemon (a different `$CREW_STATE_DIR`/`$XDG_STATE_HOME`, or none set
in one shell and set in the other).

**Confirm identity, don't assume it.** §1's expected output includes `Binary source: override` —
that's the extension reporting it resolved `crewd` via `OMP_CREW_BINARY`, not a downloaded release
(`platform.ts`'s `resolveCrewd`). If you ever see `Binary source: package` when you meant to test a
local build, `OMP_CREW_BINARY` isn't set, or isn't pointing where you think.

**Cheapest first check, no daemon needed.** `crew_doctor` / `/crew doctor` (or
`cargo run -p crew-runtime -- doctor`) verifies config parsing, the state directory, and rollout
gates without spawning anything — run it before any section below when you just want to know "did
I break something obvious," without paying for a daemon spawn or a model call.

**Match the section to what you touched** — you don't need to run all six every time:

| You touched | Run |
|---|---|
| IPC/lifecycle/journal/daemon startup | §1 |
| Event model or monitor rendering | §2 |
| An orchestration RPC method or tool | §3 |
| Adapter code (claude/codex/copilot/omp-rpc) | §4b first (free); §4c only if the change could affect real vendor-CLI behavior |
| Workspace lease/apply/isolation | §5 |
| `run/result` / redaction / read-back | §6 |

## 1. The daemon through OMP (no model call, no extension CLI needed)

The daemon is tested through the OMP extension, which handles the full lifecycle: connecting to
an existing daemon or spawning a new one (the connect-or-spawn design, ADR-0008). This is the
lowest layer you can actually exercise end-to-end.

```bash
export OMP_CREW_BINARY="$PWD/target/debug/crewd"
EXT="$PWD/packages/extension/dist/index.js"

omp --extension "$EXT" --print "/crew health"
```

Expect:

```
Crew runtime: running
Protocol: 1.0 (healthy: true)
Project: 18f82a46-....
Active runs: 0
Schema version: 1
Uptime: 0s
Binary source: override
```

Run it again — same command, same repo:

```bash
omp --extension "$EXT" --print "/crew health"
```

Expect the **same** `Project` id, with a **higher** `Uptime`. That's the connect-or-spawn design
(ADR-0008) reconnecting to the daemon it just started, not spawning a second one. If you rebuilt
`crewd` between these two invocations and still see the same `Project` id, that's the gotcha in
["Owning what you test"](#owning-what-you-test) above — you reconnected to the daemon from your
*old* build, not the new one.

**What this verifies:** The extension's `ensureRuntime()` can derive the per-repo Unix socket
path (SHA-256 of the canonical VCS root, `<stateDir>/repos/<repoId>/runtime.sock`), spawn
`crewd serve` detached, connect with bounded exponential backoff, negotiate protocol v1.0, and
serve JSON-RPC requests. The daemon's single-instance flock locking, journal-before-shutdown, and
`AdapterRegistry` wiring into `ServerConfig.run_driver` all happen inside `lifecycle::serve()`.

**What you can verify here:** The CLI's `serve`/`status`/`stop`/`schema` subcommands are fully
implemented and can be used directly. You can test the daemon's file-locking behavior (exit code
73 on double-serve), detached logging to `runtime.log`, and idle-timeout exit through the CLI
itself, not just through the extension.

### Direct CLI testing (alternative to extension)

The `crewd` CLI now provides direct access to all daemon operations:

```bash
# Start a daemon in the foreground
crewd serve --repo /path/to/repo --state-dir /path/to/state --foreground

# Query a running daemon's status
crewd status --repo /path/to/repo --state-dir /path/to/state

# Stop a running daemon
crewd stop --repo /path/to/repo --state-dir /path/to/state

# Display runtime events (replay + live)
crewd monitor --repo /path/to/repo --state-dir /path/to/state

# Export events to JSONL
crewd audit export --repo /path/to/repo --state-dir /path/to/state --output events.jsonl

# Print the JSON Schema
crewd schema

# Print the version
crewd version
```

All commands accept `--state-dir` (defaults to `.crew` if it exists) and `--repo` (required).
The `serve` command additionally accepts `--idle-seconds` (optional, makes the daemon exit after
N seconds with no connections and no active runs) and `--foreground` (logs to stderr instead of
`runtime.log`).

## 2. The embedded monitor (`/crew` slash command, no model call)

The OMP extension registers an embedded monitor driven by a pure `model.ts` reducer over
`EventEnvelope`s and a `render.ts` formatter. It's accessed via the `/crew` slash command —
**or** via the `crewd monitor` CLI subcommand.

```bash
# Through OMP extension (interactive)
omp --extension "$EXT"          # no --print: stays open, interactive
```

Type `/crew`. Expect the widget to appear above the editor, showing run rows. An empty state
renders "No Crew runs yet."

**What this verifies:** The monitor controller subscribes to the daemon's event stream via
`CrewClient.subscribe(fromSequence, cb)`, persists `lastSequence` via a custom
`pi.appendEntry('crew-monitor', {sequence})` session entry for replay-on-restart, and updates
the widget on every event. The controller handles `session_start` (connect, then show the widget
only if the journal has runs — a run-free session stays hidden until the first run event, and a
dead daemon stays silent) and `session_shutdown` (unsubscribe).

### Direct CLI monitor (alternative to extension)

```bash
crewd monitor --repo /path/to/repo --state-dir /path/to/state
```

This connects as a `display` principal, replays every event from sequence 0, renders one line per
contributing envelope, then follows new events live until interrupted (`SIGINT`/`SIGTERM`). A
transient disconnect reconnects and replays from the highest sequence already rendered plus one,
so no visible line is duplicated.

Options:
- `--run-id <id>` — Render only the run matching this id (full, un-truncated form)

**What this verifies:** The CLI's `monitor` command connects to the runtime, replays events, and
renders them as plain-text lines until interrupted. This is the same logic as the embedded
monitor but exposed as a CLI subcommand for direct testing.

## 3. The orchestration tools (needs a real model call)

The 11 orchestration tools (`crew_profile`, `crew_worker`, `crew_task`, `crew_run`, `crew_workspace`, `crew_artifact`, `crew_child`, `crew_violation`, `crew_message`, `crew_approval`, `crew_reconcile` — see [`user-guide.md`](user-guide.md) for what each does) are regular OMP tools the model *chooses* to call — this
genuinely needs a model, and each step below takes something like ten seconds to a couple of
minutes. Work in a scratch repository, never this one:

```bash
mkdir -p /tmp/crew-smoke && cd /tmp/crew-smoke && git init -q && git commit -q --allow-empty -m init
```

### 3a. Create a task, a worker, and submit a run

```bash
omp --extension "$EXT" --print \
  'Use crew_task to upsert a task. Then use
   crew_worker to create a worker with fingerprint "sha256:smoke" and adapter "fake". Then use
   crew_run to submit a run for that task against that worker with prompt "smoke test".
   Report the taskId and workerId plainly.'
```

Expect the model to report a `taskId` and `workerId`, and to say `run/submit` failed with
`adapter_unavailable` — and, importantly, that it **can't** report a `runId` from that call. That
last part is correct, not a bug in the model: `run/submit`'s error response is
`ServiceError { code, message }`, with no `data` field at all, so the caller genuinely has no way
to learn the run's id from that one call alone.

The run was still committed as `queued` underneath. Look it up with a second call, using the
`taskId` from the response above:

```bash
omp --extension "$EXT" --print \
  'Use crew_run with op "list" and taskId "<taskId from above>" to find the run that was just
   submitted. Report the runId and state plainly.'
```

Expect `state: queued`. The run is preserved even though nothing could start it — `run/submit`
never pretends a run started that it can't back, and it never drops the run just because no
adapter exists to run it (ADR-0013).

### 3b. Watch it live — two processes, on purpose

Open an **interactive** session and leave it running. This is a different invocation from the
`--print` calls above, and it matters that it stays open for the rest of this step:

```bash
omp --extension "$EXT"          # no --print: stays open, interactive
```

Type `/crew`. Expect one line, replayed from the daemon's journal the instant this session
started — it never touched the task/worker/run above, this is a brand-new session:

```
<runId-prefix> · queued · run queued
```

Now, **without closing that session**, open a *second* terminal and run the message-send call
there — a separate, short-lived process that connects to the same daemon and exits on its own:

```bash
omp --extension "$EXT" --print \
  'Use crew_message to send a "question" on runId "<runId from 3a>" from workerId
   "<workerId from 3a>", taskId "<taskId from 3a>", payload "should I proceed?".'
```

Go back to the **first** terminal — the one you never touched during that second call — and look
at it again. Expect it to have updated on its own, with zero input from you:

```
<runId-prefix> · queued · messageRecorded recorded
```

That's the live-broadcast path: the first session was already subscribed to the daemon's event
stream, and the message-send (from a *different* process) got pushed to it over the socket it
already had open — no reconnect, no re-typed `/crew`, no polling.

Only the trailing "latest activity" field changes here; the run's own `state` stays `queued`
throughout, because this scenario never starts an adapter. A real `crew_run` against a
configured worker profile walks `queued -> starting -> working` and terminalizes on process exit
(`crates/runtime/src/adapter/run_lifecycle.rs`).

### 3c. Replay after a full restart

Close the first session entirely (`Ctrl+C` or `/exit`) and start a **third**, completely fresh
one:

```bash
omp --extension "$EXT"
```

Type `/crew` again. Expect the *same* final line, replayed cold by a session that has never
seen any of this before:

```
<runId-prefix> · queued · messageRecorded recorded
```

Nothing is lost, nothing duplicates. This is a genuinely different property from 3b's
live-broadcast test — 3b required the watching session to *stay open the whole time*; this one
requires it to be fully torn down and restarted. Both must hold; neither one proves the other.

### 3d. What this walkthrough can't cover

Approval creation (`ApprovalService::request`) is only ever invoked by an adapter reporting it
needs human sign-off, and there is no `approval/request` RPC method — so there is no way to
trigger it from a live `omp` session. Exercise that half of the flow with:

```bash
cargo test -p crew-runtime --test approval
```

which drives `ApprovalService` directly, the same way this walkthrough can't.

### Clean up

Use `crewd stop` to gracefully shut down the daemon:

```bash
crewd stop --repo /tmp/crew-smoke --state-dir /tmp/crew-state
```

Or, if the daemon is not responding:

```bash
# Find the daemon process:
pgrep -fl crewd

# Kill it if still running:
pkill -f "crewd serve"

# Remove the scratch repo:
rm -rf /tmp/crew-smoke
```

## 4. Worker adapters

Steps 1-3 never spawn a real Claude/Codex/Copilot/OMP-RPC process. This section covers the four
supervised adapters, their conformance suites, and the worker coordination MCP surface.

The `crewd conformance` and `crewd adapters` CLI subcommands (see
[`cli-reference.md`](cli-reference.md#crewd-conformance)) run the same fixture/live suites as
the `cargo test` commands below and write a JSON report; `crewd adapters --json` is the quick
one-shot check that every adapter's fixture suite still passes. Use the CLI when you want a report
file or to check outside a Rust dev environment; use `cargo test` (below) when you want the
integration test harness's own assertions and `#[ignore]`/live gating.

### 4a. Prerequisites

Four vendor CLIs, plus everything from the top-level [Prerequisites](#prerequisites) above:

```bash
claude --version   # verified baseline: Claude Code 2.1.217 (2.1.220 verified to work)
codex --version    # verified baseline: codex-cli 0.145.0 (exact match required for the
                    # schema-compatibility check — see 4b)
copilot --version  # verified baseline: GitHub Copilot CLI 1.0.73 (1.0.75 verified to work)
omp --version       # verified baseline: omp/17.0.7 (17.1.1 verified to work)
```

None of these baselines are a hard requirement — the conformance test suites *measure* what the
installed CLI actually supports rather than trusting the version string; a newer patch version
that still passes every fixture scenario is fine. Codex is the one exception: its adapter checks
the installed binary's own generated JSON-RPC schema against a committed compatibility manifest,
so an incompatible **schema** change (not just a version bump) fails that one check specifically,
independent of everything else.

`OMP_CREW_BINARY` (the same override from the top-level Prerequisites) is how you point a real
`omp` session at your dev build rather than a packaged release — set it once per shell:

```bash
export OMP_CREW_BINARY="$PWD/target/debug/crewd"
```

Build the daemon:

```bash
cargo build -p crew-runtime
```

### 4b. Per-adapter smoke, fixture mode (no model call)

Fixture mode runs the conformance suites against committed JSONL fixtures under
`fixtures/adapters/<name>/` — zero model calls, zero vendor CLI invocations. Run via `cargo test`:

```bash
# All four adapters, fixture mode (crewd CLI, black-box):
cargo test -p crew-runtime --test conformance

# claude-tui's own committed fixture (`fixtures/adapters/claude-tui/`):
cargo test -p crew-runtime --test claude_tui_fixture
```

Fixture mode is TUI-sourced now (crew-v2 gap-closure WP-C, spec §4.6) — the headless control
plane this section used to also exercise via a per-adapter test file
(`claude_adapter`/`codex_adapter`/`copilot_adapter`/`omp_rpc_adapter`) is retired; those files are
deleted along with it. Each vendor's own scenario probes live under `adapter::tui::*_conformance`
(exercised via `conformance` and `tui_adapter` above), not a standalone per-adapter test binary.

Expected shape (one array element per adapter for the full test; a single-element array otherwise):

```json
[
  {
    "adapter": "claude-tui",
    "mode": "fixture",
    "version": "2.1.220",
    "declaredCapabilities": { "protocol": "structured", "resume": "session", ... },
    "effectiveCapabilities": { "protocol": "structured", "resume": "session", ... },
    "scenarios": [
      { "name": "probe", "passed": true, "detail": "claude --version reported ...; authReady=true" },
      { "name": "read_only_start_and_progress", "passed": true, "detail": "..." },
      ...
    ],
    "passed": true
  }
]
```

"Pass" for one adapter means top-level `"passed": true` — every entry in `scenarios` has its own
`"passed": true`. `effectiveCapabilities` only ever narrows `declaredCapabilities`, never widens
it: a scenario failure downgrades exactly the capability it disproves (e.g. a failed `approval`
scenario forces `approvals` to `"none"`) and leaves everything else untouched. If `"passed": false`
anywhere, read that scenario's own `detail` first — it names concretely what failed, not just that
something did.

Every adapter's fixture report should show `"passed": true` throughout, with these documented,
intentional exceptions — genuine gaps or environment dependencies, reported honestly rather than
papered over with a fabricated pass:

| Adapter | Scenario(s) | Why |
|---|---|---|
| `codex-tui` | `follow_up`, `cancellation_scope`, `session_resume`, `runtime_restart` | The installed `codex-cli` does not write a thread's rollout file to disk until a turn actually runs — resuming/following up/cancelling a turn on a never-turned thread needs a real (billed) turn, which fixture mode must never make. Live mode (4f.1) proves what it can for real when its gate is set. |
| `copilot-tui` | `session_resume`, `runtime_restart` | The installed CLI (1.0.75) does not persist a never-prompted session across a process boundary — proving full persistence needs a real turn. |
| `copilot-tui` | `unexpected_child_observation` | ACP protocol v1 has no `session/update` variant this adapter maps to a nested-worker observation — a genuine, currently-unimplemented gap. |

### 4c. Live mode (requires a real vendor CLI session; makes a real, billed model call)

The headless control plane this section used to test directly against a per-adapter test file
(a real, billed call reached via `cargo test --test claude_live -- --ignored`, etc.) is retired
(crew-v2 gap-closure WP-C, spec §4.6) — those test files are deleted along with it. **See 4f.1
below**, the TUI live conformance harness (`crewd conformance --live --mode tui`), which is now
the only live path against a real vendor CLI.

### 4d. AdapterRegistry wiring

`AdapterRegistry` (the `RunDriver` implementation this section's conformance suites feed into) is
wired into the running daemon: `lifecycle::serve()`'s `ServerConfig` sets `run_driver` to an
`AdapterRegistry` instance (`cargo test -p crew-runtime --test adapter_registry` exercises it
directly).

However, whether the registry starts an adapter is still gated by `PolicyEvaluator`: the
`limits.maxConcurrentWorkers` concurrency ceiling (from `crew.json`), and a nested-worker check
that denies unexpected child workers pre-authorization. The wider org-governance surface this
evaluator used to also enforce -- model/adapter allowlists, a required-capability list, cost
ceilings, and the `native_discovery_reviewed` rollout gate -- is retired (crew-v2 gap-closure WP5;
see
[`future-features.md`](future-features.md#org-governance-enforcement-modeladapter-allowlists-cost-ceilings-rollout-gates)):
that surface was config-sourced from the YAML org layer removed in that WP, which was never
actually reachable in production.

Practically: submitting a run through a live `omp` session with a real adapter's vendor CLI
installed **will** attempt to start the adapter as long as the concurrency ceiling isn't reached
and the worker isn't an unexpected nested one. A denial names the dimension that refused (for
example `concurrency ceiling 8 reached; 8 active runs`), and an absent vendor CLI still reports
`adapter_unavailable` — a separate, availability-level answer.

To exercise the registry's own start/reject/authorize/construct logic directly:

```bash
cargo test -p crew-runtime --test adapter_registry
```

### 4e. Worker MCP coordination tools

The *supervised* path (a real adapter's vendor process calling `crew_task`/`crew_send`
through its injected MCP config) is reachable from a live `omp` session once the vendor CLI is
installed and the merged org policy permits the adapter and model. Exercising it costs real
model calls, so the deterministic check below drives the same MCP server directly instead.

The MCP server side is now a real CLI subcommand (`crewd coordination-mcp --state-dir
<path> --repo <path> --run-id <id>`, wired in 2026-08-02 — previously the argv every adapter's
MCP config already built pointed at a subcommand that didn't exist, so `coordination-mcp`
failed immediately with clap's unrecognized-subcommand error) and the scope-token-authenticated
in-process/subprocess plumbing behind it are fully built and independently tested against a
real compiled `crewd` binary, driven as a genuine MCP client would:

```bash
cargo test -p crew-runtime --test coordination_mcp
```

That suite spawns the real `crewd coordination-mcp --state-dir ... --repo ... --run-id ...`
subprocess, drives it over real stdio exactly as a supervised vendor CLI's own MCP client would,
and verifies `crew_task`/`crew_peers`/`crew_send`/`crew_request_child`/
`crew_publish_artifact`/`crew_report_blocked`/`crew_ask_policy` all land in a real
`CoordinationBroker` behind a real `Server` — including the scope/authorization negative cases
(missing, expired, wrong-run, post-vendor-exit, or unrelated-process credentials all fail; a
verified descendant of the same live vendor process may reconnect).


### 4f. TUI pane attach + out-of-band input (journal check needs no model call)

All four adapters (claude, codex, copilot, omp-rpc) default to **TUI mode**: each worker runs as the real vendor CLI spawned on a PTY inside a pane owned by a display backend (herdr / tmux / terminal). A viewer (or the harness) can type into that pane. Every burst of keystrokes written to a pane is journaled as a `RuntimeEvent::OutOfBandInput { backend, pane_ref }` — the keystrokes themselves are never recorded, only that input happened and on which pane — and the run's `needsReconciliation` flag is set. This is the redaction-boundary guarantee for interactive control: a human steering a live run leaves an auditable trace without leaking typed content.

Manual check (observing the journal needs no model call; only *starting* the run does):

```bash
export OMP_CREW_BINARY="$PWD/target/debug/crewd"
EXT="$PWD/packages/extension/dist/index.js"

# Terminal A: start a daemon + a TUI run against one of the four adapters,
# then leave it open. See user-guide.md for the exact run tool.
omp --extension "$EXT"

# Terminal B: tail the journal for the OutOfBandInput event
crewd monitor --repo "$PWD" --state-dir "$HOME/.omp/crew" | grep -i OutOfBandInput
```

Attach to the run's pane via the active display backend (e.g. `tmux attach -t <pane-ref>` for tmux, or the herdr/terminal viewer), type a few characters, and confirm:
- Terminal B shows one `OutOfBandInput` event per pane-write burst, carrying only `backend` + `pane_ref` — **no keystroke text**.
- The run's `needsReconciliation` flips true (visible via `/crew` after a `crew_reconcile`, or `crewd audit export --repo "$PWD" --state-dir "$HOME/.omp/crew" --output /tmp/audit.jsonl` and grep for the flag).

#### 4f.1 TUI live conformance harness

`crewd conformance --live --mode tui` walks the scenario set against the real interactive vendor CLIs on a PTY (`tui` is the only accepted `--mode` value and its default — the headless control plane this also used to reach is retired, `--mode headless` is a typed rejection now, crew-v2 gap-closure WP-C). `--adapter` takes `all` or one of `claude`, `codex`, `copilot`, `ompRpc`; `--output <path>` writes the JSON report.

```bash
# billed model calls for claude/codex/copilot; omp-rpc reaches a model only when a turn runs
CREW_DISABLE_VENDOR_CLI=0 CREW_LIVE_CWD=/tmp/crew-smoke-proj \
  ./target/debug/crewd conformance --live --mode tui --adapter all --output /tmp/live.json
```

Observed per-vendor outcomes (this release):
- `probe` — **pass**: real vendor CLI reachable, declared capabilities intact, adapter spawns it on a PTY.
- `cancellation_scope` — **pass**: `cancel(CancelScope::Worker)` terminates the vendor process and a `ProcessExited` is journaled.
- `read_only_start_and_progress` / `follow_up` — **pass for claude and omp-rpc**; codex passed
  both earlier the same day and is currently blocked by an explicit vendor credit wall
  (`usage_limit_exceeded` recorded in its rollout); copilot's turns are refused with an explicit
  monthly-quota error after submit+discovery were proven. The original failures were never a
  "vendor limitation": the adapter typed the prompt and its Enter as one atomic write at a fixed
  moment after spawn. Whether the vendor's TUI processes that Enter depends on where its render
  loop is when the bytes land -- too early (stdin not yet wired) or mid-layout and the CR is
  swallowed; machine load shifts that timeline, which is why identical bytes submitted on some
  runs and not others. The fix is in `crates/runtime/src/adapter/tui/adapter.rs`: two-phase
  delivery (text once stdin is wired, Enter only after `ENTER_IDLE_MIN` output silence), plus a
  150ms text-to-Enter gap on queue-style sends (an atomic `text\r` is swallowed whole by codex).
  If a scenario fails, check the vendor's own session store first: `~/.claude/projects/`,
  `~/.codex/sessions/`, `~/.copilot/session-state/` (note: current copilot versions write
  `<session-id>/events.jsonl` inside per-session directories, not flat files),
  `~/.omp/agent/sessions/<raw-cwd-slug>/` (omp slugs the cwd as given; it does not resolve
  `/tmp` to `/private/tmp` the way claude does). A rollout/session file containing your prompt
  but no assistant reply means billing, not the adapter.
- `session_resume` — **skipped**: a single-process resume is not a daemon restart; genuine restart recovery is proven by the separate serve→stop→serve end-to-end smoke, not this report.

## 5. Cross-agent workspace isolation (requires a real adapter)

This section verifies that two parallel runs execute in separate git worktrees, each with its own
isolated workspace. It exercises `run/submit` with `workspaceMode: "isolated"`, the two-phase lease
acquisition (allocating → materialize → activate), and the `crew_peer_workspace` coordination
tool for cross-workspace review.

### 5a. Prerequisites

Everything from [§4a](#4a-prerequisites) above, with an org policy that permits the adapters and
models you intend to run, and the daemon built and ready:

```bash
export OMP_CREW_BINARY="$PWD/target/debug/crewd"

mkdir -p /tmp/crew-cross-agent && cd /tmp/crew-cross-agent && git init -q && git commit -q --allow-empty -m init
```

### 5b. Register profiles and create workers

Use `crew_profile` to register two profiles (one per adapter), then `crew_worker` to create
workers with those `profileId`s:

```bash
omp --extension "$EXT" --print \
  'Use crew_profile to register a Claude profile with adapter "claude", model "<your model>",
   source "manual-test", and startupOptions {"claude":{}}. Then register a second profile for
   "codex" with startupOptions {"codex":{}}. Report both profileIds plainly.'
```

Then create two workers, each with one `profileId`:

```bash
omp --extension "$EXT" --print \
  'Use crew_worker to create two workers, one with profileId "<profileId1>" and one with
   profileId "<profileId2>". Report both workerIds plainly.'
```

### 5c. Submit two concurrent isolated runs

Create a task, then submit two runs with `workspaceMode: "isolated"`:

```bash
omp --extension "$EXT" --print \
  'Use crew_task to upsert a task.
   Then use crew_run to submit two runs: one for workerId "<workerId1>" and one for
   workerId "<workerId2>", both with the same taskId, workspaceMode "isolated", and prompt
   "Create a file hello.txt containing your adapter name". Report both runIds and workspacePaths
   plainly.'
```

**Expected:** Both `run/submit` calls return `Ok` with distinct `runId`s and distinct
`workspacePath`s, each of the form `/tmp/crew-workspace-<projectId>/<runId>`. Confirm on disk:

```bash
git -C /tmp/crew-cross-agent worktree list
```

Both workspace paths should appear as detached worktrees.

### 5d. Poll runs to completion

```bash
omp --extension "$EXT" --print \
  'Use crew_run with op "get" for runId "<runId1>" and runId "<runId2>". Report each run
   state and workspacePath plainly.'
```

Each response must carry `workspacePath` and `workspaceMode: "gitWorktree"`.

### 5e. Cross-workspace review via `crew_peer_workspace`

Verify the worker coordination surface can resolve a peer's workspace. From the worker MCP side,
`crew_peer_workspace { peerRunId: "<other runId>" }` returns the peer's `path`,
`isolationKind`, and `state`. A call with a `runId` belonging to a different task fails with
`"peerRunId is not a run on this task"`.

### 5f. Clean up

Release both workspaces via `crew_workspace` with `op: "release"`, then verify the worktrees
are gone:

```bash
git -C /tmp/crew-cross-agent worktree list
```

The worktrees should no longer appear. Clean up the scratch directory:

```bash
rm -rf /tmp/crew-cross-agent
```

## 6. Reading a finished run's output (`run/result` — needs a real model call)

Verifies Gap 2 of the multiagent-cooperation design: the model can read a worker's final
answer and chain it into a second run. Work in a scratch repository:

```bash
mkdir -p /tmp/crew-result-smoke && cd /tmp/crew-result-smoke && git init -q && git commit -q --allow-empty -m init
```

> **Known limitation (verified live 2026-08-21, Claude Code 2.1.238):** the Claude adapter keeps
> the vendor CLI process alive after its final answer (stdin stays open for follow-up steering),
> and run completion is keyed solely on process exit — so a live Claude run never reaches
> `succeeded` on its own, and both scenarios below, as written, hang at the "poll until terminal"
> step after making their billed call. Until the grace-window completion fix lands (see the
> multiagent-cooperation spec's decision log), settle the run with `crew_run { op: "cancel" }`
> once the answer has arrived; `op: "result"` then returns the journaled `resultText` and `usage`
> with `state: "cancelled"` — that read-back path is proven working.

### 6a. One run, one answer

```bash
omp --extension "$EXT" --print \
  'Use crew_profile to register a Claude profile (adapter "claude", a model of your choice,
   startupOptions {"claude":{}}, source "manual-test"), crew_worker to create a worker from
   that profileId, crew_task to upsert a task, and crew_run to submit a run with prompt
   "Reply with exactly the word pomegranate and nothing else". Poll crew_run op "get" until
   the state is terminal, then call crew_run op "result" and report resultText and usage
   plainly.'
```

Expect: `resultText` containing exactly `pomegranate`; `usage.inputTokens` and
`usage.outputTokens` both > 0; `state: "succeeded"`. Calling `op: "result"` while the run is
still `working` is refused with `run <id> is not finished (state: working)` — that refusal is
correct behavior, not a bug: a partial answer is never returned.

### 6b. Chaining: A's answer becomes B's prompt

```bash
omp --extension "$EXT" --print \
  'Call crew_run op "result" for runId "<runId from 6a>". Then submit a second run on the
   same worker and task whose prompt embeds that resultText: "The previous worker said:
   <resultText>. Reply with the fruit it named, uppercased." Poll it to terminal, read its
   result, and report it plainly.'
```

Expect the second run's `resultText` to contain `POMEGRANATE`.

**What this verifies:** the full read-back path — journaled `adapterMessageFinal` → redaction
boundary → `run/result` fold → Ajv-validated result → the model composing the next prompt from
it. This is the chaining primitive every multi-worker synthesis flow builds on. Clean up as in
§3's "Clean up".

## Reading the widget line

The `/crew` widget is a rounded border (drawn by
`packages/extension/src/monitor/render.ts::assembleBox`) with an icon header
(`renderWidgetHeader`) spliced directly into the top border line. Each row inside the box is
prefixed with a per-state Nerd Font icon (`render.ts::stateIcon`) before the state word, and
colored per-state (`render.ts::stateColor`). Underneath the icon, the joined structure of each row
(rendered by `renderRowLine`) is unchanged:

```
<first 8 chars of runId> · <icon> <state> · [adapter/model] · [flags] · [pending approvals] · [workspace mode] · <latest activity>
```

— joined by ` · `, with any part that's undefined simply omitted. In this walkthrough there's no
real adapter, so you'll only ever see the run id, `state` (always `queued` here), and
`latestActivity`, which is set per event kind (`packages/extension/src/monitor/model.ts`):

| Event | `latestActivity` |
|---|---|
| `RunEvent` | `"run " + state` (e.g. `"run queued"`, `"run starting"`, `"run working"`) |
| `MessageEvent` | `"${kind} ${deliveryState}"` (e.g. `"messageRecorded recorded"`) |
| `ApprovalEvent` | `"approval requested: <action>"` or `"approval decided"` |
| `ChildEvent` | `"child worker requested"`, `"child worker accepted"`, or `"child worker request denied"` |
| `AdapterProtocolHealthEvent` | `"protocol healthy"` or `"protocol unhealthy: <detail>"` — the vendor's own error subtype/stop reason when one was journaled |
| `PolicyViolationRecorded` | `"policy violation: <code>"` |
| `PolicyViolationDecided` | `"violation decided: <resolution>"` |
| `AdapterUsageEvent` | `"usage <inputTokens> in / <outputTokens> out"`, plus `" ($<costUsd>)"` when cost was reported |
| `AdapterArtifactEvent` | `"artifact <artifactKind> <artifactId>"` |
| `DisplayEvent` | `"pane attached: <backend> (<paneRef>)"` or `"pane detached: <backend>"` |
| `WorkspaceEvent` | `"workspace <kind>"` |

(`RunFlagsEvent` updates the row's flags but sets no `latestActivity` of its own.)

The widget caps at `MAX_WIDGET_ROWS` rows (now 7) — not 10 — because the host's `ctx.ui.setWidget`
truncates array-content widgets at 10 total *lines*, and the border chrome (2 lines, plus a
possible overflow line) has to fit inside that same 10-line budget alongside the rows. When
truncated, the box appends `"… N more; use /crew run <runId> for full details."` as its last
row, before the bottom border. The `/crew run <runId>` detail block is a labeled multi-line
dump: Run/Task/Worker/State/Harness-model/Flags/Pending approvals/Workspace mode/Latest
activity/First seen/Last event.

## If something doesn't match

See [code-walkthrough.md's §4 debugging playbook](code-walkthrough.md#4-debugging-playbook) first —
most manual-test surprises (`METHOD_NOT_FOUND`, an empty `/crew`, connect timeouts) are covered
there with the exact cause. If a step in this document produces something not described here or
there, that's either a real regression or a gap in this document — both are worth fixing; open an
issue or extend this file, the same way the `run/submit` error-shape gap above was found by
running the walkthrough for real and getting confused by it.

## Pane Liveness Checks

When testing `/crew attach` or pane persistence workflows, verify the CREWATTACH1 liveness marker is being sent correctly: the attach socket must send `CREWATTACH1\n` as its first bytes. A probe that doesn't see the marker within 250ms will mark the pane as stale and return -32602 (Invalid params). This guards against fork-inherited stale sockets being mistaken for live panes. See [cli-reference.md § Attach Socket Liveness](cli-reference.md#attach-socket-liveness-crewattach1-marker).
