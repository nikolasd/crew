# Contributing to Crew

Thank you for your interest in contributing to Crew! This document provides guidelines and instructions for contributing to the project.

**Audience & purpose:** contributors — the process guide (branch/PR/release flow, non-negotiable
invariants). For the technical *how* of building and testing Crew itself, see
[`docs/development.md`](docs/development.md) (Crew Development Guide), the developer manual.

## Development Environment

### Prerequisites

- **Rust** (stable channel, tracked by `rust-toolchain.toml` — always the latest stable release, no fixed version)
  - Recommended: install via [rustup](https://rustup.rs) — automatically tracks the `stable` channel per-directory
  - Alternative: `brew install rust` (no automatic per-directory tracking; verify with `rustc --version`)
- **Bun** (version 1.3.14 or later)
  - Install via Homebrew: `brew install oven-sh/bun/bun`

### Setup

```bash
# Clone the repository
git clone https://github.com/nikolasd/crew.git
cd crew

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
# ... (see docs/development.md for full list)
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
- Edition 2024, Rust stable (whatever `rust-toolchain.toml` currently tracks)

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
docs/                     Engineering documentation (start here: docs/development.md)
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
   - Covers one concern — a focused fix, feature, or refactor. An unrelated cleanup you noticed
     along the way is its own PR, reviewed and merged on its own.
   - Passes all tests (`bun run check`)
   - Follows the non-negotiable invariants
   - Includes documentation updates if needed
   - Has a clear description of what changes and why

2. Submit your PR:
   - There is no PR template — write a clear description covering what changed, why, and how you
     verified it (test output, manual-testing steps run, etc.)
   - Docs-only changes are welcome as their own PR — the CI pipeline classifies a PR as docs-only
     and skips the heavier build/test jobs accordingly (see `.github/workflows/ci.yml`), so keeping
     a docs change free of unrelated source edits keeps it on that faster path.
   - Link any related issues
   - Request review from maintainers

3. Address review feedback:
   - Respond to all comments
   - Make requested changes
   - Update tests if needed

4. Merging: the maintainer merges once CI is green. If review is still in progress when CI turns
   green, say so rather than treating green CI alone as ready-to-merge — reporting review state
   alongside CI state (e.g. "green; review still in progress" vs. "green; review complete") lets
   the person merging choose knowingly instead of by default.

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
2. Assembles each target's release manifest (`cargo run -p crew-xtask -- package`), then validates the four together and emits one aggregate `release-manifest.json` (`package-set`)
3. Runs the fixture-mode conformance gate
4. Uploads the four `crewd-<target>` binaries, their four `.manifest.json` files, and `release-manifest.json` as GitHub Release assets on the tag — no package is published anywhere

**Requires:** only the default `GITHUB_TOKEN` (already available to the workflow) — no separate secret to configure.

**Release checklist, before tagging:**
- `packages/extension/dist/index.js` is rebuilt (`bun run build`) and the diff is committed — it's the exact file a marketplace-installed plugin loads, and CI's `bundle-check` job rejects a stale one. The `auto-commit-dist` workflow automatically handles this on PR branches that touch extension/protocol source (App-signed bot commit, then CI re-runs). For fork PRs or local verification, you can manually refresh via the `refresh-bundle` workflow (builds on linux-x64 + pinned Bun and uploads the artifact to commit). **Platform caveat:** the bundle embeds Bun's platform-specific module shim, so a rebuild on a different platform (e.g. macOS/arm64) does **not** byte-match CI's linux-x64 `bundle-check` (observed with Bun 1.3.14) — this is why the automation exists.
- `.claude-plugin/marketplace.json`'s versions are enforced automatically: `bun run generate --check` fails on any drift from `packages/extension/package.json`, so no manual check is needed.

## Documentation

When contributing, consider updating documentation:

- **docs/user-guide.md** (Crew User Guide) — the user manual: every tool/command an OMP session can call
- **docs/development.md** (Crew Development Guide) — the developer manual: building, configuring, and testing Crew from source
- **docs/code-walkthrough.md** (Crew Code Walkthrough) / **docs/rust-tutorial.md** (Learning Rust with this codebase as the textbook) — developer-manual companions: source map, debugging playbook, Rust-via-this-codebase tutorial
- **docs/manual-testing.md** — manual/QA verification procedures
- **docs/architecture.md** — system design (the C4-model "why")
- **docs/cli-reference.md** — `crewd` CLI command reference
- **docs/operations.md** — daemon lifecycle, crash recovery, install/upgrade procedures
- **docs/compatibility.md** — supported platforms and the adapter conformance matrix
- **docs/engineering-lessons.md** — hard-won lessons from real bugs, cross-referenced by file/ADR
- **docs/future-features.md** — consciously deferred features with decision triggers
- **docs/adr/** — architectural decisions

User-facing docs (`user-guide.md`, `cli-reference.md`, `operations.md`, `compatibility.md`) describe
shipped behavior only. A decision that has been made but not yet implemented belongs in an ADR or in
`future-features.md` until the code catches up — write it down there instead of in a user-facing doc,
even when the semantics are already settled and the wording is easy to draft early. One instance of
this: a widget-visibility decision was written into the user guide the same day it was decided,
ahead of the change that would make it true — worth checking that a doc changed alongside a decision
actually describes what shipped, not what was agreed.

Citations (in code comments, ADRs, or docs) may only point at something that survives, and "survives"
means "is in this repository" — an in-repo path, or an ADR or release record cited by path. Never a
local or gitignored file, and never an identifier that only resolves somewhere else: a tracker ticket,
a decision label, a review-register marker, or a bare pull-request number. A reader who cannot follow
a pointer has been given nothing, and the reasoning it stood in for is what they actually needed.

So write the reasoning itself. Not "the fix for the wrong-object bug" pointing at a number, but what
the bug was and why the code answers it. A comment that names a mechanism (`the fix at
`message/send``) is fine — that is a place in this repository. A comment that gestures at an unnamed
event ("the earlier fix", "that work") is the same defect wearing different clothes: the test is
whether the sentence tells the reader what happened without a lookup.

`bun run check` enforces this as its first step (`scripts/check-markers.ts`), and prints the file,
line, token and rule for anything it finds. CI's `markers` job runs the same scan on every pull
request and on `main` — including docs-only pull requests, which skip the test matrix but are the
changes most likely to introduce a marker.

What it scans is **what `git ls-files` reports**, not what happens to be on disk. Generated output
is gitignored precisely because it is not this repository's content, and a scan that read it would
fail on a clean checkout over markers copied into files nobody wrote. Two directories are exempt
beyond that, and both are deliberate:
`fixtures/`, whose files are byte-exact terminal recordings that cannot be edited without destroying
what makes them evidence, and `assets/`, where the logo's parts carry labels of the same shape.
`release/live-conformance/*.json` is exempt for the same reason as `fixtures/` — those reports are
harness output copied verbatim — while that directory's README, being authored prose, is not.

### Commit messages carry technical content only

No tool attribution, no agent names, no session links. A trailer of that kind says nothing about the
change and outlives every context in which it meant anything. The one exception is the dist-refresh
bot's `Co-authored-by: crew-bot[bot]` line: a real account made a real commit, and that is provenance
rather than noise.

CI enforces this over **the commits a pull request adds** (`scripts/check-trailers.ts`), never over
history. The distinction is deliberate and worth understanding, because it is the same distinction
that governs the citation rule above:

| what | checked by | scope |
|---|---|---|
| files | `scripts/check-markers.ts` | every file git tracks, must be zero |
| commit messages | `scripts/check-trailers.ts` | the pull request's own commits — attribution trailers **and** the marker rules above |
| history | nothing | immutable, out of scope |

The citation rule applies to commit messages too, subject and body: a commit message is read far
more often than the branch that produced it, and a squash merge keeps the subject forever. The
marker patterns are not restated in the trailer checker — it imports them, so the two surfaces
cannot drift into enforcing different things under one name.

"The pull request's own commits" excludes anything already on the target branch. That matters
because this repository merges `main` into a branch rather than rebasing it, so a synced branch's
range contains every commit `main` gained since the base — and GitHub appends `(#NNN)` to each
squash-merge subject. That trailing `(#NNN)` is generated at merge time by GitHub, is not a citation
anyone wrote, and is out of scope by construction. If a commit *body* carries an identifier and the
branch is already pushed, the maintainer edits the squash message at merge rather than anyone
rewriting history.

A file can be edited, so the marker guard demands zero everywhere. History cannot: commits already
on `main` carry trailers that predate this rule, and rewriting them would be a worse act than the
trailers are a problem. If the check fails, amend the message and force-push the branch — that is a
branch you own, not history anyone has built on.

## Questions?

- Open an issue for questions or discussions
- Check existing documentation in `docs/`
- Reach out to maintainers

Thank you for helping make Crew better!
