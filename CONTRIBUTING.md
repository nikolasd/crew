# Contributing to Crew

Thank you for your interest in contributing to Crew! This document provides guidelines and instructions for contributing to the project.

**Audience & purpose:** contributors — the process guide (branch/PR/release flow, non-negotiable
invariants). For the technical *how* of building and testing Crew itself, see
[`docs/getting-started.md`](docs/getting-started.md), the developer manual.

## Development Environment

### Prerequisites

- **Rust** (version 1.97.1, pinned in `rust-toolchain.toml`)
  - Recommended: install via [rustup](https://rustup.rs) — automatically respects the pinned version
  - Alternative: `brew install rust` (no automatic version pinning; verify with `rustc --version`)
- **Bun** (version 1.3.14 or later)
  - Install via Homebrew: `brew install oven-sh/bun/bun`

### Setup

```bash
# Clone the repository
git clone https://github.com/nikolasd/batman.git
cd batman

# Install JS deps and build the crewd runtime in one step
bun run setup
```

## Running Tests

### Rust Tests

```bash
# Run all Rust tests
cargo test

# Run specific test suite
cargo test --test adapter_contract
cargo test --test approval
cargo test --test audit
# ... (see docs/getting-started.md for full list)
```

### TypeScript Tests

```bash
# Run all TypeScript tests
bun test

# Run specific test file
bun test packages/extension/src/approval-ui.test.ts
```

### Full Test Suite

```bash
# Run all tests (Rust + TypeScript)
bun run check
```

## Code Style

### Rust

- Follow [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Use `cargo fmt` to format code: `cargo fmt --all`
- Use `cargo clippy` to check for common issues: `cargo clippy --all-targets --all-features -- -D warnings`
- Edition 2024, Rust 1.97.1 minimum

### TypeScript

- Use TypeScript with strict mode
- Generate bindings from Rust protocol types (never hand-edit generated files)
- Validate every daemon message before extension logic: envelopes/events via Ajv, results via Ajv where a canonical protocol result type exists, structurally otherwise (see invariant 2 below)

## Non-Negotiable Invariants

These hold everywhere in the codebase; changes that weaken them will be rejected in review:

1. **Rust types are canonical.** `packages/protocol-ts/src/generated/` and `packages/protocol-ts/schema/crew.schema.json` are build outputs (`bun run generate`). Generated files are never hand-edited.

2. **TypeScript validates every message** received from the daemon before it reaches extension logic: the JSON-RPC envelope and every event notification are Ajv schema-validated; result payloads are Ajv-validated for every method with a canonical protocol result type and structurally validated (must be a JSON object) otherwise.

3. **SQLite runs with WAL**, foreign keys, `synchronous=FULL`, and atomic versioned migrations; the event journal is append-only.

4. **Intent is persisted before side effects; content is redacted before it becomes durable.**

5. **Supported platforms are macOS and glibc Linux on arm64/x64** — everything else is rejected with a typed error, never a silent fallback.

6. **OMP owns the task graph**, scheduling, worker selection, policy, approvals, and merge/synthesis decisions — Rust never creates or edits OMP's task graph; a retry always creates a new run and a harness replacement always creates a new worker and run.

7. **Every domain mutation commits its event and broadcasts the same `EventEnvelope` to live `events/subscribe` listeners in the same call** — a mutation that appends without broadcasting silently breaks the embedded monitor.

## Repository Layout

```
crates/protocol/          Canonical Rust wire types (source of truth for the protocol)
crates/runtime/           The crewd daemon: CLI, lifecycle, IPC server, SQLite journal, security,
                          domain persistence, orchestration/coordination/approval services
crates/xtask/             Codegen (schema + TS bindings) and platform package assembly
packages/extension/       The OMP extension: client, launcher, platform loader, orchestration
                          tools, OMP-native reconciliation, embedded /crew monitor
packages/protocol-ts/     Generated TypeScript bindings + JSON Schema + Ajv validators
fixtures/                 Cross-language golden fixtures (protocol frames, state roots, repo ids)
docs/                     Engineering documentation (start here: docs/getting-started.md)
```

## Making Changes

### Before You Start

1. Check existing issues and PRs to avoid duplicate work
2. Read the relevant documentation in `docs/`
3. Understand the non-negotiable invariants above

### Making Changes

1. Create a new branch for your changes: `git checkout -b feature/my-feature`
2. Make your changes
3. Run tests: `bun run check`
4. Commit with a clear, descriptive commit message
5. Push and create a Pull Request

### Commit Messages

- Use clear, descriptive commit messages
- Reference issue numbers when applicable
- Follow conventional commits format if possible

## Pull Request Process

1. Ensure your PR:
   - Passes all tests (`bun run check`)
   - Follows the non-negotiable invariants
   - Includes documentation updates if needed
   - Has a clear description of what changes and why

2. Submit your PR:
   - There is no PR template — write a clear description covering what changed, why, and how you
     verified it (test output, manual-testing steps run, etc.)
   - Link any related issues
   - Request review from maintainers

3. Address review feedback:
   - Respond to all comments
   - Make requested changes
   - Update tests if needed

## Releasing

Maintainers cut a release by pushing a version tag, not by publishing manually:

```bash
# Bump the version in packages/extension/package.json first;
# `bun run generate --check` (CI's generate-check job) enforces that
# .claude-plugin/marketplace.json stays in lockstep with it.
git tag v<version>
git push origin v<version>
```

Pushing a `v*` tag triggers [`.github/workflows/release.yml`](.github/workflows/release.yml), which:
1. Builds `crewd` for macOS ARM/Intel and Linux x64/ARM
2. Assembles each target's release manifest (`cargo run -p batman-xtask -- package`), then validates the four together and emits one aggregate `release-manifest.json` (`package-set`)
3. Runs the fixture-mode conformance gate
4. Uploads the four `crewd-<target>` binaries, their four `.manifest.json` files, and `release-manifest.json` as GitHub Release assets on the tag — no package is published anywhere

**Requires:** only the default `GITHUB_TOKEN` (already available to the workflow) — no separate secret to configure.

**Release checklist, before tagging:**
- `packages/extension/dist/index.js` is rebuilt (`bun run build`) and the diff is committed — it's the exact file a marketplace-installed plugin loads, and CI's `bundle-check` job rejects a stale one.
- `.claude-plugin/marketplace.json`'s versions are enforced automatically: `bun run generate --check` fails on any drift from `packages/extension/package.json`, so no manual check is needed.

## Documentation

When contributing, consider updating documentation:

- **docs/plugin-usage.md** — the user manual: every tool/command an OMP session can call
- **docs/getting-started.md** — the developer manual: building, configuring, and testing Crew from source
- **docs/code-walkthrough.md** / **docs/rust-primer.md** — developer-manual companions: source map, debugging playbook, Rust-via-this-codebase tutorial
- **docs/manual-testing.md** — manual/QA verification procedures
- **docs/architecture.md** — system design (the C4-model "why")
- **docs/cli-reference.md** — `crewd` CLI command reference
- **docs/operations.md** — daemon lifecycle, crash recovery, install/upgrade procedures
- **docs/compatibility.md** — supported platforms and the adapter conformance matrix
- **docs/engineering-lessons.md** — hard-won lessons from real bugs, cross-referenced by file/ADR
- **docs/future-features.md** — consciously deferred features with decision triggers
- **docs/journal.md** — the commit-by-commit narrative of how the codebase got this way
- **docs/adr/** — architectural decisions

## Questions?

- Open an issue for questions or discussions
- Check existing documentation in `docs/`
- Reach out to maintainers

Thank you for helping make Crew better!
