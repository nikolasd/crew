# Repository Guidelines

## Project Overview

Crew (B**orderline** **A**wesome **T**ool for **M**ultiagent **A**utomation by **N**ikolas) is an [Oh My Pi (OMP)](https://github.com/can1357/oh-my-pi) extension backed by a durable, repository-scoped local daemon. It supervises worker processes (Claude, Codex, Copilot, OMP-RPC), persists a durable event journal, recovers after crashes, and feeds display backends.

**Architecture split:** OMP decides what to do (task graph, scheduling, approvals, merge decisions). Crew ensures it happens and can be replayed.

Delivered via the OMP marketplace (git clone of this repo, extension + skills) plus a `crewd` (Rust) binary downloaded on demand as a GitHub Release asset. No OMP fork, no private APIs, no npm publication.

---

## Architecture & Data Flow

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

**Data flow for a task:**
1. OMP extension calls `crew_task` → registers task with runtime
2. OMP schedules worker → calls `crew_run` → runtime authorizes, acquires workspace lease, spawns adapter
3. Adapter (Claude/Codex/Copilot/OMP-RPC) runs the worker process, streams events
4. Events flow through `Redactor` (sanitizes secrets) → `DatabaseActor` (persists to SQLite) → broadcast to live subscribers
5. Completion triggers slot release, workspace apply, and approval callbacks

**Key invariant:** Intent is persisted before side effects; content is redacted before it becomes durable.

---

## Key Directories

| Path | Purpose |
|------|---------|
| `crates/protocol/` | Canonical Rust wire types — source of truth for all protocol types |
| `crates/runtime/` | `crewd` daemon: CLI, lifecycle, IPC server, SQLite, adapters, services |
| `crates/xtask/` | Codegen (schema + TS bindings) and platform package assembly |
| `packages/extension/` | OMP extension: client, tools, monitor, reconciliation |
| `packages/protocol-ts/` | Generated TS bindings + JSON Schema + Ajv validators |
| `packages/crew-*/` | Per-target release build staging (created on demand by `batman-xtask package`; gitignored, not committed) |
| `fixtures/` | Cross-language golden fixtures (protocol frames, state roots, configs) |
| `tests/` | Conformance test runner |
| `release/` | Release build inputs and evidence: `targets.json` (platform build matrix, read by xtask and CI) plus per-version release checklists and live adapter conformance results |
| `docs/` | Engineering documentation (start with `getting-started.md`, `architecture.md`; `cli-reference.md` and `plugin-usage.md` cover the two user-facing surfaces) |
| `scripts/` | Setup and build scripts |

---

## Development Commands

```bash
# One-step setup (JS deps + Rust build)
bun run setup

# Full check: schema drift + build + all tests
bun run check

# Generate TS bindings and JSON Schema from Rust types
bun run generate

# Build the OMP extension bundle
bun run build

# Format check (Biome for TS, cargo fmt for Rust)
bun run format:check
cargo fmt --all --check

# Clippy (warnings as errors)
cargo clippy --all-targets --all-features -- -D warnings

# Rust tests
cargo test
cargo test --test <test_name>          # specific test suite

# TypeScript tests
bun test
bun test packages/extension/src/...     # specific file
bun run typecheck                       # TypeScript compiler gate (own CI job)

# Manual testing with local changes
OMP_CREW_BINARY="$PWD/target/debug/crewd" \
  omp --extension ./packages/extension/src/index.ts

# Run crewd directly
crewd serve --repo /path/to/repo [--org-config ... --repo-config ... --user-config ...]
crewd status --repo /path/to/repo
crewd stop --repo /path/to/repo
crewd audit export --repo "$PWD" --state-dir "$HOME/.omp/batman" --output /tmp/audit.jsonl
```

---

## Code Conventions & Common Patterns

### Rust

- **Edition 2024, Rust 1.97.1** (pinned in `rust-toolchain.toml`)
- `cargo fmt` for formatting, `cargo clippy` for linting (warnings as errors)
- **Workspace dependencies** in root `Cargo.toml` — all crates reference via `.workspace = true`
- **Error handling:** `thiserror` for custom error types, `anyhow` for application errors
- **Async:** `tokio` runtime (multi-thread), `futures-util` for combinators
- **Database:** `rusqlite` with `rusqlite_migration` for versioned migrations; single-thread actor owns the SQLite connection
- **Logging:** `tracing` + `tracing-subscriber` (with `env-filter` and `json` features)
- **Serialization:** `serde` with `derive`; `serde_json` for JSON, `serde_yaml_ng` for YAML config
- **Self-referential crate pattern:** `extern crate self as batman_runtime;` in `lib.rs` so adapter submodules can use the crate's external path, allowing the same source to compile both inside the library and in standalone test binaries via `#[path = "..."]`

### TypeScript

- **Strict mode**, `ESNext` target, `Bundler` module resolution
- **Biome** for formatting (2-space indent, double quotes, semicolons, 320-char line width); linting disabled
- **Quote style:** double quotes. **Semicolons:** always.
- **Generated files** in `packages/protocol-ts/src/generated/` are NEVER hand-edited — regenerated by `bun run generate`
- **Validation:** Every message from the daemon is validated before reaching extension logic: envelopes and events via Ajv schemas; results via Ajv where a canonical protocol result type exists, structurally otherwise
- **Runtime:** Bun (not Node). `bunfig.toml` sets `exact = true` for lockfile

### Shared Patterns

- **Protocol types flow Rust → TypeScript:** `crates/protocol/` defines types with `serde` + `schemars` + `ts-rs` derives; `xtask generate` produces JSON Schema and `.ts` bindings
- **Configuration layers** (lowest → highest): org config → repo config → user config → per-run params. YAML with strict unknown-key rejection.
- **Event broadcast invariant:** Every domain mutation commits its event AND broadcasts the same `EventEnvelope` to live `events/subscribe` listeners in the same call. A mutation that appends without broadcasting silently breaks the embedded monitor.
- **Redaction boundary:** Raw vendor content → `Redactor.sanitize()` → `PersistableEvent` (private fields, no public constructor). Secrets never reach the journal.

### Naming

- Rust: `snake_case` for modules/functions, `PascalCase` for types, `SCREAMING_SNAKE_CASE` for constants
- TypeScript: `camelCase` for functions/variables, `PascalCase` for types/classes
- Tool names: `crew_<verb>` (e.g., `crew_task`, `crew_worker`, `crew_run`)
- Commands: `/crew-status`, `/crew-doctor`

---

## Important Files

| File | Role |
|------|------|
| `crates/protocol/src/lib.rs` | Protocol type definitions — the canonical source of truth |
| `crates/runtime/src/main.rs` | `crewd` entry point — thin CLI dispatcher |
| `crates/runtime/src/lib.rs` | Runtime library — all modules, `extern crate self` trick |
| `crates/runtime/src/lifecycle.rs` | Daemon lifecycle: serve, shutdown, idle timeout |
| `crates/runtime/src/ipc/server.rs` | JSON-RPC server over Unix domain socket |
| `crates/runtime/src/service/orchestration.rs` | Core orchestration: task/run/worker lifecycle (65KB) |
| `crates/runtime/src/adapter/registry.rs` | Adapter registry with capability negotiation |
| `crates/runtime/src/domain/repository.rs` | Domain persistence layer (52KB) |
| `crates/runtime/src/security/redaction.rs` | Content redaction before persistence |
| `crates/runtime/src/policy/evaluate.rs` | Policy evaluation with concurrency ceiling |
| `packages/extension/src/index.ts` | OMP extension entry point — registers all tools/commands |
| `packages/extension/src/client.ts` | JSON-RPC client with correlation table |
| `packages/extension/src/runtime.ts` | Runtime launcher with binary selection and retry |
| `packages/extension/src/tools/` | Orchestration tool implementations |
| `packages/extension/src/omp-native/` | OMP-native reconciliation and fact persistence |
| `packages/extension/src/monitor/` | Embedded /crew monitor (model, render, controller) |
| `packages/protocol-ts/src/index.ts` | Re-exports all generated TS types |
| `packages/protocol-ts/src/validate.ts` | Ajv validators for runtime messages |
| `biome.json` | Formatter config (TS/JS only; Rust uses cargo fmt) |
| `bunfig.toml` | Bun config (exact install) |
| `tsconfig.json` | Root TS config (strict, ESNext, Bun types) |
| `rust-toolchain.toml` | Rust 1.97.1 with clippy + rustfmt |
| `Cargo.toml` | Workspace definition and shared dependencies |
| `.claude-plugin/marketplace.json` | OMP marketplace catalog for the `crew` plugin |
| `git-town.toml` | Git Town config (main branch, GitHub forge) |

---

## Runtime/Tooling Preferences

- **Runtime:** Bun 1.3.14+ (pinned via `packageManager` field). NOT Node.js.
- **Package manager:** Bun workspaces. `bun install` for deps, `bun run <script>` for commands.
- **Exact install mode:** `bunfig.toml` sets `exact = true` — lockfile is strict.
- **Rust toolchain:** 1.97.1 via `rust-toolchain.toml`. Use `rustup` for automatic version pinning.
- **Formatter:** Biome for TS/JS (`bun run format`), `cargo fmt` for Rust. Linting disabled in Biome; use `cargo clippy` for Rust.
- **Distribution:** Extension + skills install via the OMP marketplace (`.claude-plugin/marketplace.json`, git clone of this repo — private, so needs GitHub read access via SSH key or `gh auth login`). The `crewd` binary downloads on demand as a GitHub Release asset via `/crew-runtime-install`, verified by SHA-256; that download needs `GITHUB_TOKEN`/`GH_TOKEN` set, or a local `gh auth login` session.
- **Test environment:** Set `CREW_DISABLE_VENDOR_CLI=1` to skip live vendor CLI calls (required in CI to avoid billed model calls).
- **Cross-platform:** macOS (arm64/x64) and glibc Linux (arm64/x64). Everything else rejected with typed error.

---

## Testing & QA

### Test Structure

- **Rust tests:** Integration tests in `crates/runtime/tests/` (adapter_contract, approval, audit, conformance, etc.) and unit tests inline with `#[cfg(test)]` modules
- **TypeScript tests:** Co-located `.test.ts` files alongside source in `packages/extension/src/`
- **Conformance tests:** `tests/conformance/` — golden-frame protocol conformance runner (`run.ts`, `assert-report.ts`)
- **Fixtures:** `fixtures/` — golden JSON/YAML for protocol frames, configs, state roots, repo IDs

### Running Tests

```bash
# All tests (Rust + TypeScript)
bun run check

# Rust only
cargo test --workspace

# TypeScript only
bun test

# Specific Rust test suite
cargo test --test adapter_contract

# Specific TS file
bun test packages/extension/src/runtime.test.ts

# Conformance (fixture mode, no live calls)
CREW_DISABLE_VENDOR_CLI=1 cargo test --test conformance

# Live conformance for specific adapter (requires credentials)
CREW_LIVE_CLAUDE=1 cargo test --test conformance
```

### CI Pipeline (`.github/workflows/ci.yml`)

Five jobs run on every push/PR:
1. **format** — `cargo fmt --check` + Biome format check
2. **clippy** — `cargo clippy` with `-D warnings`
3. **test** — `cargo test` + `bun test` on ubuntu-latest and macos-latest (with `CREW_DISABLE_VENDOR_CLI=1`)
4. **generate-check** — verifies generated code is up to date (`bun run generate --check`)
5. **security** — `cargo audit` + gitleaks scan

### Release (`.github/workflows/release.yml`)

Triggered by pushing a `v*` tag. Builds `crewd` for all 4 platforms, assembles per-target release manifests, builds the extension bundle, and uploads binaries + manifests as GitHub Release assets. Requires only the default `GITHUB_TOKEN` — no npm publish, no `NPM_TOKEN`.

---

## Non-Negotiable Invariants

1. **Rust types are canonical.** Generated TS bindings and JSON Schema are build outputs. Never hand-edit `packages/protocol-ts/src/generated/`.
2. **TypeScript validates every message** from the daemon before extension logic processes it — envelopes and events via Ajv schemas; results via Ajv where a canonical result type exists, structurally otherwise.
3. **SQLite runs with WAL, foreign keys, `synchronous=FULL`, and atomic versioned migrations.** Event journal is append-only.
4. **Intent persisted before side effects; content redacted before durability.**
5. **Supported platforms: macOS and glibc Linux on arm64/x64.** Typed rejection for everything else.
6. **OMP owns the task graph.** Rust never creates or edits OMP's task graph.
7. **Every domain mutation commits its event AND broadcasts** the same `EventEnvelope` to live subscribers in the same call.
