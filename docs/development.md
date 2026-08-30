# Crew Development Guide

**Audience & purpose:** the Crew developer manual, for contributors building Crew from source and
working in this codebase day to day — setup, the patterns and invariants this repo actually
enforces, and where to find more. For running/debugging an already-installed `crewd` (flags,
troubleshooting workflows), see [`cli-reference.md`](cli-reference.md) instead — that content used
to live here and has moved. Its other companions are [`code-walkthrough.md`](code-walkthrough.md)
(source map and debugging playbook), [`rust-tutorial.md`](rust-tutorial.md) (Rust via this codebase),
and [`manual-testing.md`](manual-testing.md) (QA verification steps).

> **Just want to use Crew, not build it?** See [README.md's Installation section](../README.md#installation) — `/marketplace add nikolasd/crew` then `/marketplace install crew@crew` installs the extension, then a session restart, then `/crew-install` in the new session downloads the runtime binary; no build step, no GitHub access needed (the repository is public). Then see [`user-guide.md`](user-guide.md), the user manual. This guide is for developing Crew itself.

## Prerequisites

Before you begin, ensure you have the following installed:

- **Rust stable** (tracked by `rust-toolchain.toml` — always the latest stable release, no fixed version)
  - Recommended: install via [rustup](https://rustup.rs) — automatically tracks the `stable` channel per-directory
  - Alternative: `brew install rust` (no automatic per-directory tracking; verify with `rustc --version`)
- **Bun 1.3.14+** (pinned by `packageManager` in `package.json`)
  - Install via Homebrew: `brew install oven-sh/bun/bun`

## Installation

### Clone the Repository

```bash
git clone git@github.com:nikolasd/crew.git
cd crew
```

### Build

```bash
# Install JS deps and build the crewd runtime in one step
bun run setup

# Bundle the OMP extension (required before manual testing loads dist/index.js)
bun run build
```

`packages/extension/dist/index.js` is committed to git and verified in CI (a `bundle-check` job
rebuilds it and fails on any diff) — it's the exact file a marketplace-installed plugin loads, since
a git clone never runs `bun install`/`bun run build` itself. Any change under
`packages/extension/src/` must be followed by re-running `bun run build` and committing the
rebuilt `dist/index.js`.

> **Platform caveat:** the bundle embeds Bun's platform-specific module shim, so a rebuild on a
> different platform (e.g. macOS/arm64) does **not** byte-match CI's linux-x64 `bundle-check` —
> observed with Bun 1.3.14. Refresh the committed bundle via the `refresh-bundle` workflow (builds
> on linux-x64 + pinned Bun and uploads the artifact to commit), or a linux-x64 build, before
> committing — otherwise CI rejects the bundle as stale.

## Configuration

### Configuration Layers

Crew uses a strict JSON configuration file, `crew.json`, layered with later layers winning a
field-by-field deep merge (`security.patterns` is the one exception — additive, concatenated
across layers rather than replaced):

1. Built-in defaults (lowest precedence)
2. User file (`~/.omp/crew.json`)
3. Project file (`<repo>/.omp/crew.json`)
4. Per-run `policyOverrides` (overrides everything)

Every level rejects unknown keys, failing closed with the exact JSON path that named the unknown
key (`"adapters.claude.notAField"`, `"limits.bogusField"`, ...) — see
`crates/runtime/src/config/crew.rs`.

### Configuration File Locations

The extension resolves the user and project files itself and passes each one that exists to
`crewd serve --config <path>` (repeatable, lowest precedence first); a path that doesn't exist is
simply an absent layer, not an error. Calling `crewd serve`/`crewd doctor` directly, you pass
`--config` yourself — there's no auto-discovery or `CREW_CONFIG`-style environment variable at the
CLI layer, only what the extension does for you.

### Configuration File Example

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

### CrewConfig

The merged configuration deserializes into an immutable `CrewConfig` (spec §10) — approval mode,
concurrency/timeout/turn-budget limits, display backend preference, per-adapter config, workspace
defaults, dashboard, retention, and (additive-across-layers) security patterns. A thin
`RuntimePolicy` adapter (`crates/runtime/src/config/mod.rs`) exposes the fields the runtime's
redaction, workspace, concurrency, retention, and doctor checks read, plus a SHA-256 fingerprint
(`crew::fingerprint`) so two runtimes that resolved the same layers can prove they landed on the
identical effective policy without comparing documents byte-for-byte. The full field list lives in
`crates/runtime/src/config/crew.rs` — it's the single source of truth; a struct copied here would
just be one more place for it to go stale.

`dashboard` (`enabled`, `port`) turns on a read-only HTTP projection served by the daemon itself —
`crates/runtime/src/dashboard/`. Off by default. Two properties matter when working on it: every
route requires a per-run bearer token, because a TCP listener cannot check peer credentials the way
the IPC socket's `admit_same_uid` does and loopback alone keeps out other hosts but not other local
users; and the page re-fetches the server-side projection rather than reducing events itself, so
there is deliberately no second reducer to keep in sync. Both constraints are written into that
module's own doc comment, along with why the per-run transcript is read from the journal and never
from a vendor's transcript file.

## Usage

Examples below invoke `crewd` bare — nothing puts it on your `PATH`; alias it or substitute the
full path (see [`cli-reference.md`](cli-reference.md) for where installed and source-built binaries
live). To point the extension at your local build, set
`OMP_CREW_BINARY="$PWD/target/debug/crewd"` — resolution details in
[How the extension finds and starts `crewd`](#how-the-extension-finds-and-starts-crewd) below.

### Start the Server

`--repo` is required; `--state-dir` should point at the same state root the OMP extension would
resolve for you (see [`cli-reference.md`](cli-reference.md#before-you-start-state-directories) —
omitting it falls back to a bare `.crew` in the current directory, which is *not* that location):

```bash
crewd serve --repo "$PWD" --state-dir "$HOME/.omp/crew"
```

With explicit configuration layers, lowest precedence first:

```bash
crewd serve --repo "$PWD" --state-dir "$HOME/.omp/crew" \
  --config ~/.omp/crew.json \
  --config .omp/crew.json
```

### How the extension finds and starts `crewd`

What `crew_health` reports as "Binary source", and what `OMP_CREW_BINARY` is for:

1. On first use in a session, the extension tries to connect to the repository's existing runtime
   socket. If one answers, it's reused — no process is spawned.
2. If nothing answers, it picks a binary in two tiers: `OMP_CREW_BINARY` (an absolute, executable
   path) wins outright if set — this is the local-development override, and it skips checksum/
   version validation entirely. Otherwise it looks for `<state root>/bin/<version>/crewd`, verifies
   its SHA-256 and version against a sibling `manifest.json` (and rejects a manifest whose `target`
   doesn't match this platform), and only trusts it once that check passes. That cache is populated
   by `/crew-install`, which downloads both files from this extension version's GitHub
   Release. The state root itself resolves as `CREW_STATE_DIR` (env var) →
   `$XDG_STATE_HOME/omp/crew` → `$HOME/${PI_CONFIG_DIR:-.omp}/crew`, except that each of the last
   two falls back to its legacy `batman`-named sibling directory when only that one exists (a
   pre-rename install), so existing installs keep working without moving any data.
3. It spawns `crewd serve` detached, with `CREW_BINARY_SOURCE` set to `override` or `package`
   accordingly (the "Binary source" field `crew_health` reports), then retries connecting with
   bounded exponential backoff. If a different concurrent caller won the daemon's single-instance
   lock in the meantime, this session simply connects to that winner.

### Run Status Check with Doctor

`crewd status` requires a live runtime — it queries `runtime/status` over the socket:

```bash
crewd status --repo "$PWD" --state-dir "$HOME/.omp/crew"
```

For diagnostics that don't require a live runtime, use `doctor` instead — it runs the full check
catalog (database connectivity, state directory permissions, platform support, schema
compatibility, adapter availability, disk space, stale runs/workspaces, and more — see
[`cli-reference.md`](cli-reference.md#crewd-doctor) for the complete list):

```bash
crewd doctor --repo "$PWD" --state-dir "$HOME/.omp/crew" --json
```

**Note:** there is no `--recover` flag on `status` or `doctor`. Crash recovery is not something you
trigger manually — it runs automatically, once, every time `serve` starts, before the socket
accepts any connection (see [Crash Recovery](#crash-recovery) below). `doctor`'s `stale_runs` check
is the read-only, live-daemon counterpart: it reports a run that has been silent for longer than
five minutes without acting on it.

### Stop the Server

```bash
crewd stop --repo "$PWD" --state-dir "$HOME/.omp/crew"
```

### Audit Export

Export audit events to JSONL format. `--state-dir` here is the same state root every other
subcommand takes — `audit export` derives the per-repository runtime directory from it plus
`--repo`, exactly like `serve`/`status`/`doctor` (see
[`cli-reference.md`](cli-reference.md#crewd-audit-export)):

```bash
crewd audit export --repo "$PWD" --state-dir "$HOME/.omp/crew" --output /tmp/audit.jsonl
```

## Security and recovery, in brief

Two invariants worth knowing before you touch either area — full detail in
[Developer practices](#developer-practices) below and, for the flags/workflows around them,
[`cli-reference.md`](cli-reference.md):

- **Redaction is a boundary, not a step.** [`Redactor`] is the *only* path from raw vendor content
  to a [`PersistableEvent`] — it drops `Thinking`/`Secret` fragments entirely and rewrites built-in
  regex matches (API keys, bearer tokens, GitHub PATs, AWS keys, JWTs) with `[REDACTED:<rule id>]`
  markers; org-configured `security.patterns` (`crew.json`, additive across layers, never replacing
  built-in coverage) are applied on top. `PersistableEvent`'s fields are private with no public
  constructor, so nothing can construct one by skipping `Redactor::sanitize`.
- **Crash recovery runs once, automatically, at `serve` startup** — there's no flag to trigger it on
  demand. `RecoveryCoordinator` transitions every run the journal still calls non-terminal
  (`queued`/`starting`/`working`) to `failed`, unconditionally on recency, because `serve` holds the
  single-instance lock and starts with an empty adapter registry — no such run can still have a live
  process. `paused`/`waitingUser`/`waitingPeer` runs are left alone unless `RecoveryConfig` opts in.
  `doctor`'s `stale_runs` check is the live-daemon counterpart — see
  [`cli-reference.md`](cli-reference.md#crewd-doctor) for the full check catalog, and its
  [Troubleshooting](cli-reference.md#troubleshooting) section for what to do when a run wasn't
  recovered the way you expected.

## Testing

### Run All Tests

```bash
cargo test
```

### Run Specific Test Suite

```bash
cargo test --test adapter_contract
cargo test --test adapter_registry
cargo test --test approval
cargo test --test audit
cargo test --test claude_tui_fixture
cargo test --test config
cargo test --test conformance
cargo test --test coordination
cargo test --test coordination_mcp
cargo test --test database
cargo test --test display_registry
cargo test --test display_selector
cargo test --test doctor
cargo test --test domain_repository
cargo test --test herdr_display
cargo test --test ipc
cargo test --test lifecycle
cargo test --test monitor_cli
cargo test --test orchestration_rpc
cargo test --test paths
cargo test --test recovery
cargo test --test redaction
cargo test --test redaction_boundary
cargo test --test supervisor
cargo test --test terminal_adapter
cargo test --test tmux_display
cargo test --test tui_adapter
cargo test --test tui_claude_registry
cargo test --test vendor_cli_availability
cargo test --test workspace_apply
cargo test --test workspace_lease
cargo test --test workspace_materialize
```

### Test Coverage

The test suite's Rust integration test files (`crates/runtime/tests/`) cover:
- Adapter contract and registry
- Approval workflows
- Audit and redaction
- All four TUI vendor adapters (Claude, Codex, Copilot, OMP-RPC) — the headless control plane
  these once ran alongside is retired (crew-v2 gap-closure WP-C; deserializable but rejected, see
  [`docs/adr/0026-headless-retirement.md`](adr/0026-headless-retirement.md))
- Configuration and merging
- Conformance testing
- Coordination and MCP integration
- Database operations
- Display registry and selection
- Doctor diagnostics and crash recovery
- Domain repository
- IPC and lifecycle
- Supervisor and terminal adapters
- Tmux display management
- Vendor CLI availability probing
- Workspace operations (apply, lease, materialize)

## Developer practices

What this codebase actually enforces, beyond what any single file's tests show:

### Protocol types flow one way: Rust → TypeScript

`crates/protocol/` is the canonical wire contract — every request/result/event type derives
`serde` (wire format), `schemars` (JSON Schema), and `ts-rs` (TS bindings). Never hand-edit
`packages/protocol-ts/src/generated/` or the schema file directly; after any change under
`crates/protocol/`, run:

```bash
bun run generate
```

This regenerates the TS bindings and `packages/protocol-ts/schema/crew.schema.json` from the Rust
source. CI's `generate-check` job re-runs the same generation and fails the build on any diff — the
generated output and the Rust source can never quietly drift apart. On the TypeScript side, every
message the extension receives from the daemon is validated before it reaches extension logic
(`packages/protocol-ts/src/validate.ts`): envelopes and events via Ajv schemas, results via Ajv
where a canonical protocol result type exists, structurally otherwise.

### A domain mutation is not done until it's both journaled and broadcast

Every domain mutation must commit its event **and** broadcast that same `EventEnvelope` to live
`events/subscribe` listeners, in the same call. An append without a broadcast silently breaks the
embedded `/crew` monitor and any other live subscriber — it's a real, previously-hit bug class, not
a hypothetical one. Two regression tests guard it directly:
`events_replay_round_trips_committed_mutation_events` and
`events_subscribe_delivers_live_notifications_for_orchestration_mutations`. If you add a new
mutation path and suspect you've regressed this, run the affected test with an explicit runner
timeout — the failure mode is a hang (a subscriber waiting for a broadcast that never comes), not a
clean assertion failure.

### The redaction boundary is an invariant, not a call site to remember

Section above; the point worth restating here is that it's enforced structurally
(`PersistableEvent` has no public constructor reachable except through `Redactor::sanitize`), not
by convention — the same pattern this codebase reaches for repeatedly: make the unsafe path
impossible to construct, rather than trusting every call site to remember a rule. `HostProgramHint`
(a closed enum with a `#[serde(other)]` catch-all, so untrusted `$TERM_PROGRAM` content can never
reach an `osascript` invocation unmapped) is the same pattern applied to a different boundary.

### Test-first, and a failing test before any fix

This repo expects tests written before the code that makes them pass — for new features and for bug
fixes alike. A bug fix without a regression test is treated as incomplete, not merely stylistic.
`crates/runtime/tests/` (grep it for the exact suite name to pass to `cargo test --test`) and
co-located `.test.ts` files are both large and current; when changing behavior near existing tests,
extend them rather than deleting coverage to make a change land more easily.

### Review discipline: verify, don't relay

A design or approach worth merging is expected to survive independent scrutiny before it lands —
not just look plausible on a first read. Two artifacts capture that discipline once a review closes:
[`docs/engineering-lessons.md`](engineering-lessons.md) records past bugs alongside the specific
invariant or test that now closes each one (read it before assuming a gap in unfamiliar code is
accidental), and `docs/adr/0001...`–`0027...` capture the design rationale for structural choices —
check there before assuming something is arbitrary. A maintainer-local `REVIEW.md` (gitignored, not
in a fresh clone) tracks open findings by severity against current code, not planning docs.

## Contributing

We welcome contributions! Please see the [CONTRIBUTING.md](../CONTRIBUTING.md) file for guidelines.

### Development Setup

1. Clone the repository
2. Run `bun run setup` — installs JS deps, builds the crewd runtime
3. Run tests: `bun run check`
4. Make your changes
5. Submit a pull request

### Code Style

- Follow [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Use `cargo fmt` to format code: `cargo fmt --all`
- Use `cargo clippy` to check for common issues: `cargo clippy --all-targets --all-features -- -D warnings`

## Getting Help

- **Documentation**: See the other files in [`docs/`](.) — start with [architecture.md](architecture.md) and [code-walkthrough.md](code-walkthrough.md). For running `crewd` day-to-day, see [`cli-reference.md`](cli-reference.md) (full flag reference and troubleshooting workflows) and [`operations.md`](operations.md) (lifecycle procedures).
- **Issues**: Open a GitHub Issue on this repository

## License

This project is licensed under the [MIT License](../LICENSE). See the LICENSE file for full terms.
