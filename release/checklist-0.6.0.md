# Release checklist — crew 0.6.0

## Scope

0.6.0 is a **TypeScript-only feature release**: the `/crew` slash-command surface is normalized
under one dispatcher with subcommands (`health`, `run`, `runs`, `export`, `clean`, `reopen`,
`doctor`, `config`), the `/crew status <runId>` vs `/crew-status` naming collision is resolved
(`/crew status <runId>` → `/crew run <runId>`; `/crew-status` → `/crew health`), the monitor
handler's two standing bugs are fixed (silent unknown-subcommand fallthrough, missing headless
output), and `/crew-status`/`/crew-doctor`/`/crew-config` become one-release deprecation
forwarders. `/crew-install` is unchanged and permanent. See
`docs/superpowers/specs/2026-08-27-crew-command-normalization.md` for the full design and decision
record.

**No Rust behavior changes since v0.5.0.** `crates/runtime` and `crates/protocol` are rebuilt at
0.6.0 purely for version coherence (`crewd --version` must match the npm package); their code is
otherwise untouched by this release.

## Version

- [x] `crates/runtime/Cargo.toml` → `0.6.0`
- [x] `crates/protocol/Cargo.toml` → `0.6.0`
- [x] `packages/extension/package.json` → `0.6.0` (source of truth for `check_version_coherence`)
- [x] `.claude-plugin/marketplace.json` → `metadata.version` + `plugins[].version` → `0.6.0`
- [x] `Cargo.lock` refreshed via `cargo check --workspace` (never hand-edited)
- [ ] `crates/xtask` (`0.4.0`), `packages/protocol-ts` (`0.1.0`), and `crates/fake-worker`
      (`0.1.0`) left at their own helper versions — deliberately outside the coherence check

## Release assets (GitHub Release, from `release.yml`)

`release/targets.json` is the single source of truth for the four leaves. Each leaf produces two
assets; the downloader (`download.ts`) fetches exactly `crewd-${leaf}` and
`crewd-${leaf}.manifest.json`, so the pairing is a contract — never rename one without the other:

| Leaf | Binary asset | Manifest asset |
|---|---|---|
| `darwin-arm64` | `crewd-darwin-arm64` | `crewd-darwin-arm64.manifest.json` |
| `darwin-x64` | `crewd-darwin-x64` | `crewd-darwin-x64.manifest.json` |
| `linux-arm64-gnu` | `crewd-linux-arm64-gnu` | `crewd-linux-arm64-gnu.manifest.json` |
| `linux-x64-gnu` | `crewd-linux-x64-gnu` | `crewd-linux-x64-gnu.manifest.json` |

Plus the aggregate `release-manifest.json` (shared version, identical schema fingerprint, real
SHA-256 checksums, an executable named `crewd`), emitted by `crew-xtask package-set`. All four
leaves are rebuilt at 0.6.0 even though their Rust source is unchanged from 0.5.0 — the version
string is baked into the binary (`CARGO_PKG_VERSION`) and the manifest.

- [ ] All four `crewd-${leaf}` assets uploaded + executable bit intact
- [ ] All four `crewd-${leaf}.manifest.json` uploaded
- [ ] `release-manifest.json` uploaded (version 0.6.0, per-leaf sha256s present)
- [ ] `/crew-install` (download.ts) resolves each leaf by SHA-256 against the manifest —
      install-path chain verified against the live release: leaf manifest sha256 == downloaded
      binary sha256 == entry in release-manifest.json; binary executes (`crewd 0.6.0`)

## Build / gate (CI)

- [ ] `bun run check` green (schema drift + build + all tests, `CREW_DISABLE_VENDOR_CLI=1`) — all
      CI checks green on the release PR/merge
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo fmt --all --check`
- [ ] `bun run generate --check` (version coherence at 0.6.0)
- [ ] Extension bundle (`packages/extension/dist/index.js`) refreshed in CI's environment:
      `refresh-bundle` workflow (once merged to `main`) or the container equivalent documented
      there. The bundle embeds Bun's platform-specific module shim — a darwin-arm64 rebuild does
      NOT byte-match CI's linux-x64 output (observed on Bun 1.3.14) — so build on linux-x64, the
      platform `bundle-check` verifies against, and commit the `dist-linux-x64` artifact. This is
      the ONE sanctioned `dist/index.js` change; it must land on the bundle-refreshed main commit
      that gets tagged.

## Conformance evidence

- [ ] Fixture conformance (`tests/conformance`, `CREW_DISABLE_VENDOR_CLI=1`) green via CI — no
      billed calls
- [x] Live adapter conformance evidence **carries over from v0.5.0** — no adapter/Rust changes in
      this release, so no new live run is needed. Citing the existing evidence: `probe` +
      `cancellation_scope` proven for claude/codex/copilot/omp-rpc, `read_only_start_and_progress`
      / `follow_up` proven for claude and omp-rpc, codex re-proven post-#17/#18, copilot blocked
      only on a vendor billing wall (not an adapter defect) — see `release/live-conformance/*.json`
      (`claude-tui.json`, `codex-tui.json`, `codex-tui-post-quota.json`, `copilot-tui.json`,
      `copilot-tui-2026-08-26-transcript-capture.json`, `omp-rpc-tui.json`) and
      `release/checklist-0.5.0.md`'s "Conformance evidence" section for the full detail and the
      still-UNPROVEN turn-level interrupt (`interrupt_sequence`/Steer) carried forward as a known
      gap, not a 0.6.0 regression.

## Release

- [ ] Tag `v0.6.0` — annotated, on the bundle-refreshed main commit (see Build/gate above)
- [ ] **A plain `v0.6.0` tag push does NOT trigger `release.yml`** — its push trigger matches only
      suffixed `v[0-9]+.[0-9]+.[0-9]+-*` tags. Publish via `workflow_dispatch` with `ref: v0.6.0`;
      the workflow publishes iff the ref is a `v*` tag whose value matches
      `packages/extension/package.json`'s version.
- [ ] All 9 assets verified (4 binaries + 4 manifests + `release-manifest.json`)
- [ ] Install-path chain verified against the live release (download a leaf binary, sha256 matches
      both manifests, executes reporting `crewd 0.6.0`)
- [ ] Marketplace catalog (`marketplace.json` @ `0.6.0`) published
- [ ] Resolve the still-open 0.5.0 marketplace-catalog row (`release/checklist-0.5.0.md`'s
      Release section): mark it superseded by 0.6.0's publication, or verify the 0.5.0 catalog
      entry actually shipped, before or alongside publishing 0.6.0's
