# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> This repo also has `AGENTS.md` at the root, which is the canonical, exhaustive reference (directory
> table, full invariant list, per-language conventions, CI job breakdown). Read it when you need detail
> beyond what's below. This file is a working summary tuned for day-to-day edits.

## What this is

Crew is an [Oh My Pi (OMP)](https://github.com/can1357/oh-my-pi) extension backed by a durable,
repository-scoped local daemon (`crewd`, Rust). **OMP decides what to do** (task graph, scheduling,
worker selection, approvals, merge/synthesis decisions). **Crew ensures it happens and can be
replayed** — it supervises worker processes (Claude, Codex, Copilot, OMP-RPC), persists a durable
SQLite event journal, recovers after crashes, and feeds display backends (herdr, tmux, terminal).

Two deliverables, one repo: the OMP extension + skills (`@nikolasd/crew`, installed via the OMP
marketplace — git-cloned, not npm-published) and `crewd` (Rust daemon binary, downloaded on demand
as a GitHub Release asset), communicating over JSON-RPC 2.0 on bounded NDJSON over a per-repository
Unix domain socket.

## Commands

```bash
# Setup (JS deps + Rust build) — run this first
bun run setup

# Full CI-equivalent check: schema drift + format + build + all tests
bun run check

# Regenerate TS bindings + JSON Schema from Rust protocol types (after editing crates/protocol/)
bun run generate

# Build the OMP extension bundle
bun run build

# Formatting
bun run format:check   # Biome (TS/JS)
bun run format:write
cargo fmt --all --check
cargo fmt --all

# Linting (Rust only; Biome linting is disabled in this repo)
cargo clippy --all-targets --all-features -- -D warnings

# Tests
cargo test --workspace                       # all Rust tests
cargo test --test <test_name>                # e.g. cargo test --test adapter_contract
bun test                                     # all TypeScript tests
bun test packages/extension/src/runtime.test.ts   # one TS file
bun run typecheck                            # TypeScript compiler gate (own CI job)

# Conformance tests (golden-frame protocol checks, crates/runtime/src/conformance/ + tests/conformance/)
CREW_DISABLE_VENDOR_CLI=1 cargo test --test conformance   # fixture mode (what CI runs)
CREW_LIVE_CLAUDE=1 cargo test --test conformance          # live, needs vendor credentials

# Manual exercise against local changes (no publish needed)
OMP_CREW_BINARY="$PWD/target/debug/crewd" \
  omp --extension ./packages/extension/src/index.ts

# crewd CLI directly
crewd serve --repo /path/to/repo [--org-config ... --repo-config ... --user-config ...]
crewd status --repo /path/to/repo
crewd stop --repo /path/to/repo
crewd audit export --repo "$PWD" --state-dir "$HOME/.omp/batman" --output /tmp/audit.jsonl
```

`CREW_DISABLE_VENDOR_CLI=1` skips live vendor CLI calls — set it for any local test run to avoid
billed model calls; CI always sets it. Rust test suite names live under `crates/runtime/tests/`
(`adapter_contract`, `adapter_registry`, `approval`, `audit`, `coordination`, `redaction_boundary`,
`workspace_lease`, etc.) — grep that directory when you need the exact name for `cargo test --test`.

## Architecture

```
OMP Extension (TypeScript)  ──JSON-RPC 2.0 over NDJSON──>  crewd daemon (Rust)
                                                                      │
                                                                      ├── Adapter Registry ──> Worker Processes
                                                                      ├── SQLite Journal (WAL, append-only)
                                                                      ├── Coordination Broker (scope tokens, rate limiting)
                                                                      ├── Approval Service
                                                                      ├── Display Backends (herdr, tmux, terminal)
                                                                      └── Workspace Operations (lease, materialize, apply)
```

**Task lifecycle:** `crew_task` registers a task → `crew_run` authorizes it, acquires a
workspace lease, and spawns an adapter → the adapter (Claude/Codex/Copilot/OMP-RPC) runs the vendor
CLI and streams normalized events → events pass through `Redactor` (strips secrets) →
`DatabaseActor` (commits to SQLite) → broadcast to live subscribers (the embedded `/crew`
monitor, display backends) → completion releases the slot, applies the workspace, fires approval
callbacks.

**Key invariant driving most design decisions:** intent is persisted before side effects, and
content is redacted before it becomes durable.

Full C4-style diagrams (system context → container → component, plus the event-lifecycle sequence
diagram and a role/permission table) are in `docs/architecture.md`. Design rationale for anything
below is captured as an ADR in `docs/adr/0001...` through `0024...` — check there before assuming a
structural choice is accidental.

### Where things live

| Path | What |
|---|---|
| `crates/protocol/` | Canonical Rust wire types (`serde` + `schemars` + `ts-rs` derives) — the source of truth for the whole protocol |
| `crates/runtime/` | The `crewd` daemon: CLI, lifecycle, IPC server, SQLite actor, adapters, orchestration/coordination/approval services, workspace ops, security |
| `crates/xtask/` | Codegen (TS bindings + JSON Schema) and platform package assembly |
| `packages/extension/` | The OMP extension: JSON-RPC client, runtime launcher, tool implementations (`crew_task`, `crew_worker`, `crew_run`, ...), OMP-native reconciler, embedded `/crew` monitor |
| `packages/protocol-ts/` | Generated TS bindings + JSON Schema + Ajv validators — **never hand-edit `src/generated/`**, run `bun run generate` |
| `packages/crew-*/` | Per-platform `crewd` binary leaf directories — release build staging, created on demand by `batman-xtask package`, uploaded as GitHub Release assets, never committed |
| `fixtures/` | Cross-language golden fixtures (protocol frames, state roots, repo ids, configs) that Rust and TS tests both read |
| `tests/conformance/` | Golden-frame adapter conformance runner |
| `release/` | Release build inputs and evidence — `targets.json` (platform build matrix, read by xtask and CI) plus per-version release checklists and live adapter conformance results |
| `docs/` | Start with `getting-started.md` and `architecture.md`; `engineering-lessons.md` documents past bugs and the invariant that closed each one |

### Cross-language coupling to keep in mind

- Protocol types flow **Rust → TypeScript**, never the reverse: edit `crates/protocol/`, then run
  `bun run generate` to regenerate `packages/protocol-ts/src/generated/` and the JSON Schema. CI's
  `generate-check` job fails the build if generated output has drifted from source.
- Every message the extension receives from the daemon is validated before it reaches extension
  logic (`packages/protocol-ts/src/validate.ts`): envelopes and events via Ajv schemas; results
  via Ajv where a canonical protocol result type exists, structurally otherwise.
- `packages/extension/src/state.ts`'s `resolveStateRoot` must stay semantically identical to Rust's
  `StateRoot::resolve` (`crates/runtime/src/paths.rs`) — they resolve the same on-disk state root
  independently, in two languages.
- Every domain mutation must commit its event **and** broadcast the same `EventEnvelope` to live
  `events/subscribe` listeners in the same call — an append without a broadcast silently breaks the
  embedded monitor. There are regression tests guarding this
  (`events_replay_round_trips_committed_mutation_events`,
  `events_subscribe_delivers_live_notifications_for_orchestration_mutations`); if you suspect a new
  mutation path regressed it, run with a test-runner timeout — the failure mode is a hang, not a
  clean assertion failure.

### Non-negotiable invariants

These are enforced in review, not just style preference:

1. Rust types in `crates/protocol/` are canonical; generated TS/JSON Schema are build outputs only.
2. TypeScript validates every daemon message before extension logic touches it — envelopes/events via Ajv, results via Ajv where a canonical type exists, structurally otherwise.
3. SQLite runs WAL + foreign keys + `synchronous=FULL` + versioned migrations; the event journal is append-only.
4. Intent persisted before side effects; content redacted before durability.
5. Supported platforms: macOS and glibc Linux, arm64/x64 only — everything else gets a typed rejection, never a silent fallback.
6. OMP owns the task graph; Rust never creates or edits it. A retry always creates a new run; a harness replacement always creates a new worker and run.
7. Every domain mutation commits its event and broadcasts it in the same call (see above).

## Conventions worth knowing before editing

- **Rust**: edition 2024, toolchain pinned at 1.97.1 (`rust-toolchain.toml`). Workspace deps live in
  root `Cargo.toml`, referenced via `.workspace = true`. `thiserror` for typed errors, `anyhow` at
  the application boundary. `tokio` multi-thread runtime. A single-thread actor owns the one
  `rusqlite::Connection` (`crates/runtime/src/db/actor.rs`) — don't reach for a connection pool.
  `crates/runtime/src/lib.rs` uses `extern crate self as batman_runtime;` so adapter submodules can
  be compiled both inside the library and as standalone test binaries via `#[path = "..."]`.
- **TypeScript**: strict mode, ESNext, Bundler resolution, runs on **Bun, not Node**. Biome formats
  (2-space indent, double quotes, semicolons, 320-char width); Biome linting is disabled — there is
  no TS linter in this repo. Tests are co-located `.test.ts` files.
- **Config**: layered YAML (org → repo → user → per-run params), strict unknown-key rejection.
  `crates/runtime/src/config/merge.rs` produces an immutable, SHA-256-fingerprinted `RuntimePolicy`.
- **Naming**: Rust `snake_case`/`PascalCase`/`SCREAMING_SNAKE_CASE` as usual; TS `camelCase`/`PascalCase`.
  Tool names follow `crew_<verb>` (`crew_task`, `crew_worker`, `crew_run`, ...); commands are
  `/crew-status`, `/crew-doctor`.

## Source-of-truth docs (read before assuming a gap is unintentional)

- `REVIEW.md` (gitignored — present on the maintainer's machine, not in fresh clones) — open implementation gaps and findings by severity, verified against the current code (not planning docs); check before re-reporting something already tracked. Resolution history: `docs/journal.md`.
- `docs/future-features.md` — items deliberately deferred, each with a decision trigger.
- `docs/engineering-lessons.md` — past bugs and the invariant/test that now guards against each.
