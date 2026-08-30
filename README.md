![Crew Logo](./assets/logo/banner/transparent/crew-banner-2560x640.png)

Crew is an [Oh My Pi (OMP)](https://github.com/can1357/oh-my-pi) extension backed by a durable, repository-scoped local daemon. OMP stays the brain — task intake, scheduling, worker selection, approvals, merge decisions, synthesis. Crew is the hands: it supervises worker processes, speaks harness adapter protocols, persists a durable event journal, recovers after crashes, and feeds display backends.

Everything is delivered through the OMP marketplace (extension + skills, git-cloned from this repository) plus a `crewd` daemon binary downloaded on demand as a verified GitHub Release asset — no OMP fork, no private APIs, no npm publication.

## Why Crew?

Multiagent automation is hard. Most frameworks either:

- **Put all the intelligence in the agent** (risky, hard to debug, no recovery)
- **Put all the intelligence in your code** (complex, brittle, no replay)

Crew splits the difference: **OMP decides what to do, Crew ensures it happens and can be replayed.**

Key benefits:

- **Durable event journal.** Every action is persisted before it executes. Crash? Replay from the last known state.
- **Redaction by construction.** Secrets never reach the journal. Enforced at the type level.
- **SQLite-backed.** Query your automation history with SQL. No proprietary format.
- **Adapter-agnostic.** Claude, Codex, Copilot, OMP-RPC — plug in any worker.
- **No model calls required for monitoring.** Check runtime status, task state, or run history without spending a token.

If you're building multiagent systems that need to be auditable, recoverable, and debuggable, Crew is your foundation.

## Installation

Crew consists of two components, installed in two steps:
- **Plugin**: the OMP extension + skills, pulled from this repository via the OMP marketplace
- **Binary**: the `crewd` runtime daemon, downloaded as a verified GitHub Release asset

```
/marketplace add nikolasd/crew
/marketplace install crew@crew
```

**Exit and start a new `omp` session.** `/reload-plugins` does not reload extension modules,
so `/crew-install` (and every `crew_*` tool) only exists once a fresh session has
loaded the installed module. Then:

```
/crew-install
/crew health
```

This repository is public. The OMP marketplace clones it via HTTPS — no authentication required.
`/crew-install` downloads the `crewd` binary via the GitHub REST API. While the repository is public,
a `GITHUB_TOKEN` or `GH_TOKEN` environment variable is optional but recommended: without one, you may
hit GitHub's unauthenticated API rate limit (60 requests/hour). With a token (from `export
GITHUB_TOKEN=...`, `export GH_TOKEN=...`, or a local `gh auth login` session), the limit is 5,000
requests/hour. The binary is cached under your Crew state root.

Once installed, [`docs/user-guide.md`](docs/user-guide.md) (Crew User Guide) is the user manual: every tool and
command the extension registers, and the recommended flow for running a task through it.

**To uninstall:**
```
/marketplace uninstall crew@crew
```

## Development

For contributors building or modifying Crew itself (not for end users — see [Installation](#installation) above). [`docs/development.md`](docs/development.md) (Crew Development Guide) is the developer manual — start there for the full setup/build/test/config walkthrough.

**Prerequisites:** Bun 1.3.14+, macOS or glibc Linux on arm64/x64, and Rust — via [rustup](https://rustup.rs) (recommended: automatically tracks the `stable` channel `rust-toolchain.toml` pins to, always the latest stable release) or your system package manager. For the full OMP integration you also need OMP ≥ 17.0.7.

```bash
git clone https://github.com/nikolasd/crew.git
cd crew
bun run setup               # installs JS deps + builds the crewd runtime
bun run check               # schema drift check + build + all tests
```

To exercise the extension against your local changes before opening a PR, load it from its source path directly:

```bash
OMP_CREW_BINARY="$PWD/target/debug/crewd" \
  omp --extension ./packages/extension/src/index.ts
```

Ask the model to use `crew_task`, `crew_worker`, and `crew_run`, then open `/crew` to watch runs live. See [docs/user-guide.md](docs/user-guide.md) for the full tool reference and [docs/manual-testing.md](docs/manual-testing.md) for the full walkthrough. For running `crewd` directly instead of through OMP, see [docs/cli-reference.md](docs/cli-reference.md).

`packages/extension/dist/index.js` is committed to git and verified in CI (a `bundle-check` job rebuilds and diffs it), since it's the entry point the marketplace-installed plugin loads. Any change under `packages/extension/src/` must be followed by `bun run build` and committing the rebuilt bundle.

> **Bundle refresh:** the `auto-commit-dist` workflow automatically refreshes the committed bundle on
> any PR that touches `packages/extension/src/` or `packages/protocol-ts/` — an App-signed bot commit is
> created with the rebuilt bundle, and CI re-runs on it to verify. For fork PRs or local verification
> before pushing, you can manually refresh via the `refresh-bundle` workflow (builds on linux-x64 +
> pinned Bun and uploads the artifact to commit). **Platform caveat:** the bundle embeds Bun's
> platform-specific module shim, so a rebuild on a different platform (e.g. macOS/arm64) does **not**
> byte-match CI's `bundle-check` — observed with Bun 1.3.14. The automation exists to handle this.

## Contributing

Contributions are welcome. Before submitting a PR:

1. Read [`docs/development.md`](docs/development.md) (Crew Development Guide) and [`docs/architecture.md`](docs/architecture.md) (Crew Architecture).
2. Run `bun run check` — schema drift, build, and all tests must pass.
3. Follow the [Non-Negotiable Invariants](CONTRIBUTING.md#non-negotiable-invariants) — changes that weaken them will be rejected.
4. Use descriptive commit messages. Reference issue numbers when applicable.
5. Describe what changed and why, link related issues, and request review. (There is no PR
   template to fill out — just write a clear description.)

For detailed guidelines, see [`CONTRIBUTING.md`](CONTRIBUTING.md). For the release/publishing process, see [CONTRIBUTING.md's Releasing section](CONTRIBUTING.md#releasing).

## Author

Crew was created by **Nikolas Demiridis** as part of the [Oh My Pi](https://github.com/can1357/oh-my-pi) ecosystem.

For questions, issues, or contributions, please open a GitHub Issue on this repository.

## License

This project is licensed under the [MIT License](LICENSE). See the LICENSE file for full terms.

## Known Limitations

This is a pre-1.0 project. The review backlog is empty — the one open item is an unreproduced test-flake watch, tracked in the maintainer's local, gitignored `REVIEW.md` (engineering lessons from closed reviews live in [`docs/engineering-lessons.md`](docs/engineering-lessons.md)). What remains below are environment and protocol walls, verified against the current codebase. Every adapter is installed and authenticated here, and live TUI conformance runs against all four — claude, codex, and omp-rpc are fully green, while copilot alone is blocked on a confirmed vendor monthly-quota wall (raw reports under [`release/live-conformance/`](release/live-conformance/) with an erratum). None of the below is a "requires a vendor CLI" caveat.

- **ACP v1 has no durable session handle, so Copilot cannot resume across processes.** A session
  that completed a real turn answers `session/load` with `Resource not found`, which fails
  `session_resume` and `runtime_restart`. A protocol wall, not an adapter defect.
- **ACP v1 exposes no subagent-observation variant**, so Copilot's vendor-side delegation cannot be
  normalized to `NestedWorkerObserved`. Pending a newer ACP version.
- **Copilot's turn-dependent scenarios are unprovable on this account's monthly quota.** The
  tailed session's own event log records a typed `session.error` (`errorCode: quota_exceeded`,
  `statusCode: 402`) — independently confirmed, not inferred from a timeout. Refilling the
  workspace makes the two remaining scenarios provable with no code change.

Consciously deferred features, each with a decision trigger, live in
[`docs/future-features.md`](docs/future-features.md).
