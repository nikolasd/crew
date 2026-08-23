# `crewd` CLI Reference

`crewd` is the Crew runtime daemon binary. Most users never invoke it directly — the OMP
extension spawns and connects to it automatically (see [`plugin-usage.md`](plugin-usage.md), the
user manual, and [`packages/extension/src/runtime.ts`](../packages/extension/src/runtime.ts)).

**Audience & purpose:** anyone who needs to run `crewd` by hand: debugging, scripting, CI, or
writing a new display backend or supervisor integration. A companion reference to both
[`plugin-usage.md`](plugin-usage.md) (the user manual) and [`getting-started.md`](getting-started.md)
(the developer manual) — this document is pure flag reference, not a workflow guide; see
[`operations.md`](operations.md) for the procedures these flags are used in.

Every subcommand's built-in `--help` is generated from the same `clap` definitions this document
was written from ([`crates/runtime/src/cli.rs`](../crates/runtime/src/cli.rs)) — if the two ever
disagree, trust `--help` and file a bug against this file.

Examples below invoke `crewd` bare. Nothing puts it on your `PATH`: an installed runtime lives
at `<state-root>/bin/<version>/crewd` (default `~/.omp/batman/bin/<version>/crewd`, fetched by
`/crew-runtime-install` and invoked by absolute path from the extension) and a local build at
`target/debug/crewd` or `target/release/crewd`. Alias or symlink it, or substitute the full
path in every command.

## Before you start: state directories

Almost every subcommand takes `--state-dir` and `--repo`. Getting `--state-dir` wrong is the most
common way to talk to the wrong runtime (or no runtime at all), because **`crewd` resolves it
differently depending on how you invoke it:**

- **When the extension spawns `crewd`**, it always passes an explicit `--state-dir` computed by
  [`resolveStateRoot`](../packages/extension/src/state.ts), which mirrors the Rust
  `StateRoot::resolve` precedence exactly:
  1. `CREW_STATE_DIR` (must be absolute)
  2. `$XDG_STATE_HOME/omp/batman` when `XDG_STATE_HOME` is set (must be absolute)
  3. `$HOME/${PI_CONFIG_DIR:-.omp}/batman`
- **When you run `crewd` by hand and omit `--state-dir`**, the CLI does *not* apply that
  precedence at all — `serve`/`status`/`stop`/`monitor`/`doctor` each fall back to the bare
  relative path `.crew` in your current directory, erroring out immediately if it doesn't exist.
  This is a different default from the one the extension uses, on purpose (the extension's launcher
  and this bare fallback are separate code paths) — so a `crewd status --repo .` run in a random
  shell will not find the runtime OMP is using unless you pass the same `--state-dir` the extension
  resolved.

**To talk to the same runtime OMP is using**, compute the state root with the precedence above (or
just read it off `crew_health`'s output, which reports the runtime's identity) and pass it
explicitly:

```bash
crewd status --state-dir "$HOME/.omp/batman" --repo "$PWD"
```

For `serve`/`status`/`stop`/`monitor`/`doctor`, `--state-dir` is a **state root**: `crewd` hashes
the canonicalized repository path into a `repository-id` and derives the actual per-repository
runtime directory as `<state-dir>/repos/<repository-id>/`, containing `runtime.sock`, `runtime.lock`,
`runtime.db`, and `runtime.log`. `crewd audit export` is the one exception — see its entry below.

## Commands

### `crewd serve`

Starts the runtime for one repository and serves until stopped.

```bash
crewd serve --repo <path> [--state-dir <path>] [--idle-seconds <n>] [--foreground]
              [--org-config <path>] [--repo-config <path>] [--user-config <path>]
```

| Flag | Required | Default | Meaning |
|---|---|---|---|
| `--repo` | yes | — | Repository this runtime instance serves |
| `--state-dir` | no | `.crew` (relative to cwd) | State root; see above |
| `--idle-seconds` | no | none — runs until signalled | Exit after this many seconds with zero connections and zero active runs |
| `--foreground` | no | `false` | Log structured records to stderr instead of `runtime.log` |
| `--org-config` / `--repo-config` / `--user-config` | no | none | Explicit paths for the three config layers (see `docs/architecture.md`'s config-merge notes) |

**Single-instance enforcement:** `serve` takes an exclusive, non-blocking advisory `flock(2)` on
`<runtime-dir>/runtime.lock` (not an `O_EXCL` create — the lock file itself is never deleted, only
the kernel-held `flock` is released, on clean shutdown or process death). If another `crewd serve`
already holds it, the new process prints the live holder's identity (`pid`, `instance_token`,
`runtime_version`, `project_id`, `socket_path`) as JSON on stdout and exits **73** (`EX_TEMPFAIL`).

**Shutdown sequence** on `SIGINT`/`SIGTERM`, an accepted in-band `runtime/shutdown` (refused with
`-32602` while any run is live or another connection is open, unless the request carries
`force: true`), or idle timeout: journal a
`RuntimeStopping` event → close the database actor → remove `runtime.sock` → release the flock. The
socket's disappearance is therefore proof the journal already shut down cleanly.

Before accepting any connection, `serve` runs crash recovery once: every run left in `queued`,
`starting`, or `working` is transitioned to `failed`, however recent its last event — `serve` holds
the single-instance lock and its adapter registry starts empty, so no such run can still have a live
process. Runs in `paused`/`waitingUser`/`waitingPeer` are left alone by default (they're
intentionally waiting on a human or peer), unless recovery is explicitly configured to also cancel
those. `doctor`'s `stale_runs` check is the live-daemon counterpart: it reports runs silent for
longer than five minutes without transitioning anything.

### `crewd status`

Prints the runtime's `runtime/status` snapshot as JSON and exits.

```bash
crewd status --repo <path> [--state-dir <path>] [--wait-seconds <n>]
```

`--wait-seconds` retries the connection for up to N seconds — useful right after `serve` starts, to
avoid a startup race. Without it, `status` attempts to connect exactly once.

### `crewd stop`

Gracefully stops the runtime serving a repository.

```bash
crewd stop --repo <path> [--state-dir <path>]
```

Reads `runtime.lock`; if no live holder is found (the flock is actually free), exits with **1** and
prints `no runtime running for this repository` — it never signals a pid it can't confirm is alive,
closing the recycled-pid race a bare `kill(pid, 0)` would leave open. Otherwise sends `SIGTERM` to
the recorded pid and polls for up to 10 seconds for `runtime.sock` to disappear. Prints
`runtime stopped` and exits **0** on success; exits nonzero with a timeout error if the daemon
doesn't shut down in time.

### `crewd lease release`

Force-releases a workspace lease by id, directly against the lease database — no daemon may be
running.

```bash
crewd lease release --repo <path> --lease-id <id> [--yes] [--state-dir <path>]
```

The operator remedy for a lease whose owning session correlation was never persisted (the extension
crashed before the upsert that would have recorded it): `workspace/release` is owner-gated and a new
session is a different principal, so such a lease is unreleasable over RPC until `reconcile/omp`
rebinds its task — and unreleasable, full stop, when no correlation survives to rebind.
`crewd doctor` reports these as stale and names this command as the remedy.

Guardrails: refused while a runtime serves this repository (its monitors could never see the
out-of-band write — release over RPC or `crewd stop` first), and an `active` lease is refused
without `--yes`, because releasing it strips a run's workspace claim. The operation intent is
persisted to the audited `operations` table before the release runs, and the release journals
`LeaseReleased` — so `events/replay` and `audit export` never show a `LeaseAcquired` with no
terminating event. A non-shared lease's materialized worktree is torn down exactly as the runtime's
own release path does; if teardown fails, the row moves to `cleanupFailed` so the doctor keeps
reporting the leaked directory.

Prints `lease <id> released` and exits **0**. An unknown id exits **1**; an already-released lease
exits **2**.

### `crewd monitor`

Replays journaled events, then keeps tailing live events until interrupted (`Ctrl-C`) — there is no
separate "live" flag; catch-up and live-tail are the same continuous stream.

```bash
crewd monitor --repo <path> [--state-dir <path>] [--run-id <id>]
```

Omit `--run-id` to watch every run in the project; pass it (full, untruncated form) to filter to one
run. If the socket disappears mid-session (daemon restart), `monitor` reconnects automatically and
resumes from the last sequence it saw, rather than exiting.

### `crewd doctor`

Runs a diagnostic check catalog against the same paths `serve` would use — it diagnoses the state a
daemon actually writes, not a directory only `doctor` believes in.

```bash
crewd doctor --repo <path> [--state-dir <path>] [--json]
              [--org-config <path>] [--repo-config <path>] [--user-config <path>]
```

Exits **0** if every check passes, **1** if any check fails (the process still ran to completion —
this is a reported failure, not an abort). A fatal condition that prevents the catalog from running
at all (unresolvable paths, unreadable config) exits with a generic failure instead and, in `--json`
mode, prints `{"healthy": false, "error": "..."}`.

Check catalog, in run order:

| Check | Verifies |
|---|---|
| `database_connectivity` | Journal is reachable (`SELECT count(*) FROM events`) |
| `configuration_valid` | Merged policy's `max_workers`/`concurrency_ceiling` are nonzero, retention period parses, security regexes compile |
| `state_dir_writable` | State dir exists, is mode `0700` owned by the current uid, and accepts a write-then-remove probe |
| `platform_supported` | OS/arch is one of the four supported targets; on Linux, that a glibc (not musl) loader is present |
| `binary_integrity` | `current_exe()` resolves |
| `socket_permissions` | If `runtime.sock` exists: it's actually a socket, owned by the current uid, mode `0600` |
| `schema_compatibility` | If `--repo` is a Crew source checkout, its committed `packages/protocol-ts/schema/crew.schema.json` matches the binary's own rendered schema; passes trivially (not applicable) for any other `--repo` |
| `adapter_claude_available`, `adapter_codex_available`, `adapter_copilot_available`, `adapter_omp_rpc_available` | Each vendor CLI is reachable |
| `display_available` | If `display.backend` forces a specific backend, that backend reports available; otherwise a real backend (Herdr or tmux) is available -- the always-available terminal fallback never satisfies this on its own |
| `disk_space` | State dir's filesystem has ≥ 512 MiB free |
| `stale_workspaces` | Counts workspace leases whose worktree vanished, failed cleanup, or have sat `allocating` past a 10-minute grace period (abandoned before materialization completed) |
| `stale_runs` | Counts runs stuck in a non-terminal state with no live adapter |
| `rollout_gates_resolved` / `rollout_gate_<gate>` | Unresolved rollout gates; `native_discovery_reviewed` unresolved blocks authorization outright, the rest are advisory |

A check that's missing required context (no db handle, no policy, etc.) reports a failure prefixed
`skipped:` rather than silently passing.

`crew_doctor` (the OMP tool/`,/crew-doctor` command) runs this same check catalog without
requiring a live runtime connection — see [`plugin-usage.md`](plugin-usage.md).

### `crewd version`

Prints `crewd <version>` and exits 0. Equivalent to `crewd --version` (clap's built-in flag,
same `CARGO_PKG_VERSION`).

### `crewd schema`

Prints the canonical JSON Schema document to stdout.

```bash
crewd schema
```

**Caveat:** this reads `packages/protocol-ts/schema/crew.schema.json` as a path relative to the
current working directory, not relative to the binary. It only works run from (or under) a checkout
of this repository — an installed leaf-package binary run from an arbitrary directory will fail to
find the file. Use `bun run generate --check` (which runs from the repo root) instead of this
command for CI drift checks.

### `crewd audit export`

Exports journaled events to a JSONL file.

```bash
crewd audit export --repo <path> --state-dir <path> --output <path> [--from <ts>] [--to <ts>]
```

`--state-dir` here means the same *state root* as every other subcommand: `audit export` resolves
the per-repository runtime directory via `RuntimePaths::resolve(state_root, repo)` — exactly what
`serve`/`status`/`stop`/`monitor`/`doctor` do
([`run_audit_export`](../crates/runtime/src/cli.rs)). If the resolved database does not exist (e.g.
this repository was never served under this state root), the command refuses with an error rather
than silently opening — and thereby creating — an empty one.

Other details:
- `--from`/`--to` should be RFC3339 timestamps. They are **not validated** — the raw strings go into
  a lexicographic SQL comparison against RFC3339-formatted values, so a malformed timestamp produces
  a wrong (usually empty) result set rather than an error.
- `--output` is **required**. The export always writes to the given file path; it never writes to
  stdout.
- An empty result still writes an empty file (so a consumer can tell "nothing in range" apart from
  "the export never ran").
- Data is exported exactly as journaled — it was already redacted at write time, so there's no
  second redaction pass on export.

### `crewd coordination-mcp`

Serves the worker-coordination MCP proxy for one run over stdio: `initialize`/`tools/list`/
`tools/call`, proxied to the `coordination/*` runtime methods.

```bash
crewd coordination-mcp --repo <path> --run-id <id> [--state-dir <path>]
```

This is an internal integration point — it's how a supervised worker process (Claude/Codex/Copilot/
OMP-RPC) reaches Crew's coordination tools, not something a human runs directly. It authenticates
using `CREW_WORKER_SCOPE_TOKEN`, which it reads from (and removes from) its own inherited
environment.

### `crewd display probe`

Probes one display backend's status without activating it.

```bash
crewd display probe <herdr|tmux|terminal> [--json]
```

Prints availability, active state, version, and dimensions (when known). Never starts or attaches
to the backend — this is read-only.

### `crewd conformance`

Runs the fixture or live conformance suite for one adapter (or all four) and writes a JSON report.

```bash
crewd conformance --adapter <claude|codex|copilot|ompRpc|all> (--fixture | --live) --output <path>
```

Exactly one of `--fixture`/`--live` must be set. `--fixture` runs entirely offline against golden
frames; `--live` shells out to the real vendor CLI (gated by adapter-specific env vars, e.g.
`CREW_LIVE_CLAUDE=1`) and reports a structured `{adapter, mode: "live", passed: false, error}`
entry rather than a hard process failure if the vendor CLI is unavailable or refuses (e.g. out of
credits). The report is written to `--output` and also printed to stdout.

Each scenario in the report is `pass`, `fail`, or `skipped` — never collapsed to a boolean. `skipped`
means the scenario was never attempted (e.g. `CREW_DISABLE_VENDOR_CLI=1` suppresses every real
vendor-CLI spawn) and counts as neither proof nor disproof: it leaves the capability it would gate
at its declared value in `effective_capabilities`, and only a genuine `fail` downgrades it (R68).
`passed: true` on the top-level report still requires every scenario to have `pass`ed — a skip marks
the report unpassed even though it downgrades nothing, so a skip is never mistaken for full proof.

### `crewd adapters`

Runs every adapter's fixture conformance suite and prints declared vs. effective capabilities — the
same evidence the adapter registry uses to gate what a worker profile is allowed to claim.

```bash
crewd adapters [--json]
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | `stop`: no runtime was running. `doctor`: the check catalog ran but reported unhealthy. |
| `73` | `serve`: another instance already holds the repository's lock (`EX_TEMPFAIL`) |
| nonzero (generic failure) | Any other error: bad arguments, unresolvable paths, connection failure, a `stop` that timed out waiting for shutdown, `doctor` aborting before the catalog could run, `display probe` given an unknown backend, `conformance` failing to write its report, etc. |

See [`docs/architecture.md`](architecture.md#error-codes) for the separate table of JSON-RPC-level
error codes (`-32700`…`-32004`) returned *inside* the protocol, as opposed to the CLI's own process
exit codes above.
