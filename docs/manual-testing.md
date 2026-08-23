# Manual testing

**Audience & purpose:** contributors doing pre-release or post-change QA — a companion to
[getting-started.md](getting-started.md), the developer manual. Not for end users; if you're
looking for how to *use* Crew rather than verify a change to it, see
[plugin-usage.md](plugin-usage.md).

Every automated suite (`bun run check`) runs without a model call and without a human watching a
screen. Some things can only be verified by actually running `omp`, calling a tool with a real
model, and looking at what comes back — this document is the complete, current list of those
checks: what to run, in what order, exactly what you should see, and what it means if you don't.

Run these after any change that touches the daemon lifecycle, the IPC layer, an orchestration RPC
method, an OMP tool, or the monitor — the automated suites can tell you a function returns the
right value; only these checks can tell you the *whole system*, wired together, still behaves the
way `architecture.md` says it does.

## Prerequisites

Same as [getting-started.md](getting-started.md#prerequisites): Rust 1.97.1+, Bun 1.3.14+,
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

**State directory resolution** (in precedence order):
1. `CREW_STATE_DIR` (must be absolute)
2. `$XDG_STATE_HOME/omp/batman` when `XDG_STATE_HOME` is set (must be absolute)
3. `$HOME/${PI_CONFIG_DIR:-.omp}/batman`

**Configuration file locations** (in precedence order, lowest to highest). There is no
auto-discovery and no `CREW_ORG_CONFIG`-style environment variable for any layer — each is
loaded only from the path passed explicitly via `--org-config`/`--repo-config`/`--user-config`:
1. **Org config** — path passed to `--org-config`
2. **Repo config** — path passed to `--repo-config`, conventionally `<repo>/.crew/config.yaml`
3. **User config** — path passed to `--user-config`, conventionally `~/.crew/config.yaml`

Configuration files are YAML with strict unknown-key rejection (fails closed with line/column diagnostics). Example:

```yaml
# ~/.crew/config.yaml
max_workers: 4
concurrency:
  ceiling: 8
retention: "30d"
display:
  backend: auto
models:
  allowlist:
    - "gpt-4"
    - "claude-3-opus"
security:
  patterns:
    - "AKIA[0-9A-Za-z]{16}"  # AWS access key pattern
    - "sk-[a-zA-Z0-9]{32}"  # API key pattern
rollout_gates:
  vendor_terms_accepted: true
  retention_configured: true
  model_allowlist_set: true
  concurrency_explicit: true
  native_discovery_reviewed: true
  ornith_identity_set: true
```

**Security notes:**
- Adapter authorization is decided entirely by org policy — `models`, `adapters`, `capabilities.required`, `concurrency`, `cost`, and the `native_discovery_reviewed` rollout gate. No environment variable grants or withholds it.
- Vendor CLIs are ordinary installed dependencies; live conformance and the availability probe run by default. `CREW_DISABLE_VENDOR_CLI=1` should always be set in CI jobs or unattended runs — it forbids observation-only vendor invocation and guarantees no billed model call is made.

## 1. The daemon through OMP (no model call, no extension CLI needed)

The daemon is tested through the OMP extension, which handles the full lifecycle: connecting to
an existing daemon or spawning a new one (the connect-or-spawn design, ADR-0008). This is the
lowest layer you can actually exercise end-to-end.

```bash
export OMP_CREW_BINARY="$PWD/target/debug/crewd"
EXT="$PWD/packages/extension/dist/index.js"

omp --extension "$EXT" --print "/crew-status"
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
omp --extension "$EXT" --print "/crew-status"
```

Expect the **same** `Project` id, with a **higher** `Uptime`. That's the connect-or-spawn design
(ADR-0008) reconnecting to the daemon it just started, not spawning a second one.

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

The 11 orchestration tools (`crew_profile`, `crew_worker`, `crew_task`, `crew_run`, `crew_workspace`, `crew_artifact`, `crew_child`, `crew_violation`, `crew_message`, `crew_approval`, `crew_reconcile` — see [`plugin-usage.md`](plugin-usage.md) for what each does) are regular OMP tools the model *chooses* to call — this
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
cargo test -p batman-runtime --test approval
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
cargo build -p batman-runtime
```

### 4b. Per-adapter smoke, fixture mode (no model call)

Fixture mode runs the conformance suites against committed JSONL fixtures under
`fixtures/adapters/<name>/` — zero model calls, zero vendor CLI invocations. Run via `cargo test`:

```bash
# All four adapters, fixture mode:
cargo test -p batman-runtime --test conformance

# Individual adapters:
cargo test -p batman-runtime --test claude_adapter
cargo test -p batman-runtime --test codex_adapter
cargo test -p batman-runtime --test copilot_adapter
cargo test -p batman-runtime --test omp_rpc_adapter
```

Expected shape (one array element per adapter for the full test; a single-element array otherwise):

```json
[
  {
    "adapter": "claude",
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
| `codex` | `follow_up`, `cancellation_scope`, `session_resume`, `runtime_restart` | The installed `codex-cli` does not write a thread's rollout file to disk until a turn actually runs — resuming/following up/cancelling a turn on a never-turned thread needs a real (billed) turn, which fixture mode must never make. Live mode (4c) proves all four for real when its gate is set. |
| `copilot` | `session_resume`, `runtime_restart` | The installed CLI (1.0.75) does not persist a never-prompted session across a process boundary — proving full persistence needs a real turn. |
| `copilot` | `unexpected_child_observation` | ACP protocol v1 has no `session/update` variant this adapter maps to a nested-worker observation — a genuine, currently-unimplemented gap. |

### 4c. Per-adapter smoke, live mode (requires a real API key/session; makes a real, billed model
call for the adapters that reach one)

Vendor CLIs are ordinary installed dependencies: none of the commands below need an opt-in
environment variable, and each test file handles its own gating internally.

```bash
mkdir -p /tmp/crew-conformance-live && cd /tmp/crew-conformance-live && git init -q && git commit -q --allow-empty -m init

# Claude — needs an authenticated `claude` CLI session (run `claude auth status` first if unsure).
# `#[ignore]`d: an explicit `--ignored` run is itself the signal a human wants the live call.
cargo test -p batman-runtime --test claude_live -- --ignored

# Codex — needs $OPENAI_API_KEY (or an authenticated `codex` CLI session) in the environment.
# `#[ignore]`d for the same reason.
cargo test -p batman-runtime --test codex_adapter -- --ignored

# Copilot — needs an authenticated `copilot` CLI session (`copilot` itself manages this, not an
# env var this adapter reads directly). Not `#[ignore]`d: its real-binary test only performs the
# `initialize` + `session/list` handshake, which never invokes a model, so it runs in every
# default `cargo test` and simply skips if `copilot` is not on PATH.
cargo test -p batman-runtime --test copilot_adapter

# OMP-RPC — no cloud API key needed. The harness resolves a cloud selector from `omp`'s built-in
# catalog of 583 models; no local model server is required. Not `#[ignore]`d: its real-binary
# tests exercise zero-model-call stdio probes and run on every `cargo test`, skipping only if
# `omp` is not on PATH.
cargo test -p batman-runtime --test omp_rpc_adapter
```

Run each from inside `/tmp/crew-conformance-live` (a disposable repo — some live scenarios spawn
a real vendor process with that directory as its `cwd`), and reference credentials only as the
environment variable name, never the value, exactly as shown above.

Set `export CREW_DISABLE_VENDOR_CLI=1` first to forbid the Claude and Codex live tests from
making their real, billed call. The Copilot and OMP-RPC real-binary tests above never invoke a
model at all, so the switch has nothing to suppress for them — they are safe by construction,
and are instead protected in CI simply by the `copilot` or `omp` binary being absent.

**What "no paid model call" means here, precisely:** every 4b (fixture) test is *guaranteed*
zero model calls — a design invariant, proven by the test code never invoking a model. A 4c
(live) test that reaches a model, run with the kill switch unset, is the opposite: it
deliberately makes a real, billed call for whichever scenarios that adapter's own live suite
defines as needing one (the default posture: prove as much as possible in fixture mode, reserve
live mode for the few properties — a real vendor process schema/handshake, mostly — that only a
live process can prove at all). Always set
`export CREW_DISABLE_VENDOR_CLI=1` in a CI job or an unattended run, so a stray `--ignored`
invocation degrades to an honest skip instead of a charge.

### 4d. AdapterRegistry wiring

`AdapterRegistry` (the `RunDriver` implementation this section's conformance suites feed into) is
wired into the running daemon: `lifecycle::serve()`'s `ServerConfig` sets `run_driver` to an
`AdapterRegistry` instance (`cargo test -p batman-runtime --test adapter_registry` exercises it
directly).

However, whether the registry starts an adapter is decided by the merged org policy, evaluated by
`PolicyEvaluator`: the `models` and `adapters` allowlists (empty means "all allowed"), any
`capabilities.required` entries checked against the adapter's conformance-proven capability set,
the `concurrency` ceiling, the `cost` ceilings, and the `native_discovery_reviewed` rollout gate
for adapters that can observe vendor-created child workers.

Practically: submitting a run through a live `omp` session with a real adapter's vendor CLI
installed **will** attempt to start the adapter as long as the merged policy permits it. A denial
names the dimension that refused (for example `adapter 'codex' is not authorized`), and an absent
vendor CLI still reports `adapter_unavailable` — a separate, availability-level answer.

To exercise the registry's own start/reject/authorize/construct logic directly:

```bash
cargo test -p batman-runtime --test adapter_registry
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
cargo test -p batman-runtime --test coordination_mcp
```

That suite spawns the real `crewd coordination-mcp --state-dir ... --repo ... --run-id ...`
subprocess, drives it over real stdio exactly as a supervised vendor CLI's own MCP client would,
and verifies `crew_task`/`crew_peers`/`crew_send`/`crew_request_child`/
`crew_publish_artifact`/`crew_report_blocked`/`crew_ask_policy` all land in a real
`CoordinationBroker` behind a real `Server` — including the scope/authorization negative cases
(missing, expired, wrong-run, post-vendor-exit, or unrelated-process credentials all fail; a
verified descendant of the same live vendor process may reconnect).


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
`packages/extension/src/monitor/render.ts::assembleBox`) with a bat-icon header
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

The widget caps at `MAX_WIDGET_ROWS` rows (now 7) — not 10 — because the host's `ctx.ui.setWidget`
truncates array-content widgets at 10 total *lines*, and the border chrome (2 lines, plus a
possible overflow line) has to fit inside that same 10-line budget alongside the rows. When
truncated, the box appends `"… N more; use /crew status <runId> for full details."` as its last
row, before the bottom border. The `/crew status <runId>` detail block is a labeled multi-line
dump: Run/Task/Worker/State/Harness-model/Flags/Pending approvals/Workspace mode/Latest
activity/First seen/Last event.

## If something doesn't match

See [code-walkthrough.md's §4 debugging playbook](code-walkthrough.md#4-debugging-playbook) first —
most manual-test surprises (`METHOD_NOT_FOUND`, an empty `/crew`, connect timeouts) are covered
there with the exact cause. If a step in this document produces something not described here or
there, that's either a real regression or a gap in this document — both are worth fixing; open an
issue or extend this file, the same way the `run/submit` error-shape gap above was found by
running the walkthrough for real and getting confused by it.
