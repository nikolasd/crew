# Release checklist — crew 0.5.0

> Status: **prep complete, NOT tagged.** Tag/release is held until an explicit `ship` from the
> repo owner (WP29 scope: prep only).

## Version

- [x] `crates/runtime/Cargo.toml` → `0.5.0`
- [x] `crates/protocol/Cargo.toml` → `0.5.0`
- [x] `packages/extension/package.json` → `0.5.0`
- [x] `.claude-plugin/marketplace.json` → `metadata.version` + `plugins[].version` → `0.5.0`
- [ ] `crates/xtask` (`0.4.0`) and `crates/fake-worker` (`0.1.0`) left at their own helper versions

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
SHA-256 checksums, an executable named `crewd`), emitted by `crew-xtask package-set`.

- [ ] All four `crewd-${leaf}` assets uploaded + executable bit intact
- [ ] All four `crewd-${leaf}.manifest.json` uploaded
- [ ] `release-manifest.json` uploaded
- [ ] `/crew-install` (download.ts) resolves each leaf by SHA-256 against the manifest

## Build / gate (CI)

- [ ] `bun run check` green (schema drift + build + all tests, `CREW_DISABLE_VENDOR_CLI=1`)
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo fmt --all --check`
- [ ] `bun run generate --check`
- [ ] Extension bundle (`packages/extension/dist/index.js`) refreshed in CI's environment:
      `refresh-bundle` workflow (once merged to `main`) or the container equivalent documented
      there. The bundle embeds Bun's platform-specific module shim — a darwin-arm64 rebuild does
      NOT byte-match CI's linux-x64 output (observed on Bun 1.3.14) — so build on linux-x64, the
      platform `bundle-check` verifies against, and commit the `dist-linux-x64` artifact.

## Conformance evidence

- [x] Fixture conformance (`tests/conformance`, `CREW_DISABLE_VENDOR_CLI=1`) green — no billed calls
- [x] TUI live harness (`crewd conformance --live --mode tui --adapter all`) exercises the real CLIs:
      `probe` + `cancellation_scope` pass for claude/codex/copilot/omp-rpc -- `cancellation_scope`
      proves `CancelScope::Worker` (process-kill) termination only. Turn-level interrupt
      (`interrupt_sequence`/Steer, `CancelScope::Turn`, the Esc-style byte sequence) remains
      UNPROVEN live for all four vendors; tracked post-0.5.0.
- [x] `read_only_start_and_progress` / `follow_up` mechanics fixed and proven for claude and
      omp-rpc (all runnable scenarios pass; reports on file). Two-phase prompt delivery
      (`adapter.rs`): the prompt text is typed once the vendor's stdin is wired (never before
      `INJECT_MIN_DELAY` post-spawn), and the single Enter is delivered only after the PTY has
      gone output-silent for `ENTER_IDLE_MIN` -- an idle TUI processes it exactly like a human's
      keystroke, independent of vendor startup-render speed; queue-style sends additionally split
      text and Enter with a 150ms gap (an atomic `text\r` is swallowed whole by codex).
- [x] codex: re-proven on current main (2026-08-27, main@2cde61e, post-#17/#18 adapter changes) --
      `probe` / `read_only_start_and_progress` / `follow_up` / `cancellation_scope` all pass
      (4/4 runnable), a byte-identical report to the earlier confirmed run. Full
      spawn -> type -> submit -> discover -> tail path proven.
      `release/live-conformance/codex-tui.json`.
- [x] copilot: re-run 2026-08-27 on current main -- folder-trust prerequisite solved (#15), and
      submit + transcript discovery now genuinely work (`start=Ok(())`, `session=true`; the
      2026-08-26 evening run's discovery failure is gone). `read_only_start_and_progress` and
      `follow_up` still fail (`first_message=false`/`saw_ack=false`), but the root cause is now
      CONFIRMED rather than inferred: the tailed session's own `events.jsonl` records a typed
      `session.error` (`errorType: quota`, `errorCode: quota_exceeded`, `statusCode: 402`,
      "You have exceeded your monthly quota") -- a genuine vendor billing wall, not an adapter
      defect. Re-run after quota refills. `release/live-conformance/copilot-tui.json`.
- [ ] `session_resume` stays skipped here by design: a single-process resume proves nothing about
      daemon-restart recovery; that is the separate serve -> stop -> serve end-to-end smoke below.
      Documented in `docs/manual-testing.md` §4f.1.

## Manual QA (docs/manual-testing.md)

- [x] §4f TUI pane attach + out-of-band input checklist added
- [ ] Daemon serve → status → stop smoke (no vendor call)
- [ ] Pane attach + OutOfBandInput journaling spot-check (no model call to observe the journal entry)

## Release

- [ ] Tag `v0.5.0` (HELD)
- [ ] GitHub Release created from the tag, assets uploaded
- [ ] Marketplace catalog (`marketplace.json` @ `0.5.0`) published
