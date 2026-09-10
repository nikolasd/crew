# Crew Compatibility Guide

**Audience & purpose:** anyone deciding whether Crew supports their platform, or checking which
adapter-conformance scenarios currently pass against which vendor CLI version. This is a
compatibility *matrix*, not a general reference — for configuration, protocol methods, or the
CLI, see [development.md](development.md) (the developer manual),
[architecture.md](architecture.md), and [cli-reference.md](cli-reference.md) respectively; this
document exists only for the two tables below.

## Supported Platforms

Crew supports the following platform/architecture combinations — the full list, not a
milestone-in-progress. This is [`release/targets.json`](../release/targets.json)'s build matrix,
which is also the exhaustive set `crewd` accepts at runtime
(`crates/runtime/src/doctor.rs`'s `platform_supported` check):

| Platform | Architecture | libc |
|----------|--------------|------|
| macOS   | arm64 (Apple Silicon) | — |
| macOS   | x64 (Intel) | — |
| Linux   | arm64 | glibc |
| Linux   | x64 | glibc |

Anything outside this table — musl Linux, Windows, 32-bit, or any other architecture — gets a
typed rejection at startup, never a silent fallback (this is a project invariant, not a gap
pending a future milestone). Two reasons this boundary is permanent rather than provisional:
the security model assumes a Unix domain socket and same-user peer-credential admission (no
Windows equivalent is implemented), and the packaged binaries are built against glibc, not musl.

### Platform-Specific Notes

- **macOS**: Requires Xcode Command Line Tools for native compilation from source (contributors
  only — installed users get a prebuilt binary, see [development.md](development.md)).
- **Linux**: glibc only; musl (e.g. Alpine) is rejected by `platform_supported`, not silently
  degraded.
- **Display backends**: Herdr and tmux both require the matching binary to be installed and,
  for tmux, an already-running session — see [`cli-reference.md`](cli-reference.md#crewd-display-probe)
  for the read-only probe. The terminal backend is always available as a fallback.

## Adapter Compatibility

**The table and the four per-adapter sections immediately below are historical.** They were
generated from the headless control plane's `--live` conformance runs. crew-v2 gap-closure
retired that control plane entirely — deleted, not kept inert (`mode: "headless"` stays
deserializable for old configs/journals but is typed-rejected;
[`docs/adr/0026-headless-retirement.md`](adr/0026-headless-retirement.md)) — so none of the
commands below are reproducible against the current tree. Kept only as a record of which vendor
CLI versions were once verified compatible; **for current, reproducible compatibility evidence,
see "TUI live conformance" further below, which is unaffected by this retirement.**

The table below is **generated from real `--live` conformance runs**, not from prose. Each row
records the version the adapter's own probe observed and how many canonical scenarios that run
proved.

Reproduce with (`CREW_DISABLE_VENDOR_CLI` must be **unset** — it suppresses vendor invocation):

```bash
# NOT REPRODUCIBLE: --live now always runs the TUI suite (the only mode left); this command
# is preserved verbatim as a record of how the headless-era numbers below were captured.
./target/debug/crewd conformance --adapter <claude|codex|copilot|ompRpc> --live \
  --output /tmp/live-<adapter>.json
```

| Adapter | Observed version | Scenarios passing |
|---------|------------------|-------------------|
| Claude  | `2.1.222`           | 14 / 14 |
| Codex   | `codex-cli 0.146.0` | 9 / 14  |
| Copilot | `1.0.78`            | 11 / 14 |
| OMP-RPC | `omp/17.2.7`        | 14 / 14 |

A scenario short of 14 is recorded below with its cause. None of them is an unproven assertion:
each carries the vendor's or the environment's own explanation.

### Claude Adapter
- **Protocol**: Claude Code CLI over stdio
- **Status**: Stable — the only adapter whose live suite is fully green
- **Live result**: 14 / 14 scenarios pass against `2.1.222`

### Codex Adapter
- **Protocol**: `codex app-server` JSON-RPC over stdio (`initialize` → `thread/start` → `turn/start`)
- **Status**: Stable; live turn-dependent scenarios currently unprovable on this account
- **Live result**: 9 / 14 against `codex-cli 0.146.0`. `result_usage_artifacts`, `follow_up`,
  `cancellation_scope`, `session_resume`, and `runtime_restart` all share one cause — the account
  cannot run a turn:

  > `usageLimitExceeded: Your workspace is out of credits. Ask your workspace owner to refill in order to continue.`

  This is an account condition, not an adapter defect: `codex login status` reports
  `Logged in using ChatGPT`, `initialize`/`thread/start` succeed, and the turn is refused
  server-side after ~3s. Refill the workspace and the five scenarios become provable with no code
  change.

### Copilot Adapter
- **Protocol**: Agent Client Protocol (ACP) over NDJSON stdio (`copilot --acp`) — not the GitHub
  Copilot HTTP API
- **Status**: Stable; cross-process session resume is a protocol wall, not a defect
- **Live result**: 11 / 14 against `1.0.78` (`authReady=true`). Three scenarios fail for two
  distinct ACP v1 limitations:
  - `session_resume` and `runtime_restart` — a session that completed a real turn cannot be
    reloaded from a new process: `session/load` answers
    `Resource not found: Session <id> not found`. ACP v1 has no durable session handle, so the
    adapter cannot resume across processes.
  - `unexpected_child_observation` — ACP v1's `session/update` schema has no variant this adapter
    could map to `NestedWorkerObserved`, so vendor-side delegation is unobservable. Real gap,
    pending a newer ACP version. See [`future-features.md`](future-features.md) for the decision
    trigger.

The CLI version compared against the table below is the `agentInfo.version` field reported by
the real ACP `initialize` handshake, **not** the output of `copilot --version` (which prints, for
example, `GitHub Copilot CLI 1.0.78.` — note the trailing period — plus a separate
`copilot update` notice line). An installed CLI version is trusted only after it has been
empirically verified with a real handshake; a version not in the table is refused, never assumed
"nearby" compatible.

| CLI Version | ACP Protocol Version |
|--------------|----------------------|
| 1.0.73       | 1                    |
| 1.0.75       | 1                    |
| 1.0.78       | 1                    |

Supported ACP protocol version range: 1–1 (`COPILOT_MIN_ACP_PROTOCOL_VERSION` through
`COPILOT_MAX_ACP_PROTOCOL_VERSION`). A negotiated protocol version outside this range is refused
with `AdapterError::incompatible_version`, since this adapter's normalizer only understands the
v1 field names.

### OMP-RPC Adapter
- **Protocol**: Crew-driven `omp --mode rpc` NDJSON frames over stdio
- **Status**: Stable — **14 / 14, fully green**
- **Live result**: 14 / 14 against `omp/17.2.7`, `passed: true`, reproduced on three consecutive
  runs with zero local providers in `omp`'s catalog.

### TUI First-Run Gate Detection

Claude, Codex, Copilot, and OMP-RPC TUI sessions all classify what the vendor's terminal is
actually showing and refuse to write into anything they don't recognize: Copilot's folder-trust
dialog defaults its selection to "Yes", so an unattended Enter would grant filesystem trust — the
same hazard already closed for Codex. OMP-RPC has no comparable trust dialog (its own first-run
flow is a global, skippable setup wizard, not a per-repository gate, and never blocks anything an
unattended Enter or Esc could grant); its predicate recognizes its normal prompt and otherwise
fails closed — including on the wizard itself, which gets no special-cased gate — per
`release/live-conformance/2026-09-10-copilot-omp-first-run.md`.

**omp prerequisite:** its ready-screen predicate keys on the welcome screen's own Tips panel, which
omp's own `startup.quiet` config setting suppresses entirely (it "suppresses all startup chrome
including the splash", per omp's settings documentation). An operator who has set `startup.quiet`
in their omp config must unset it for a crew worker to ever be recognized as ready — crew's own
launch never requests quiet mode, but it cannot override a user's existing config. A run that fails
with "no recognizable prompt or first-run gate appeared" naming `startup.quiet` in the message is
this exact case, not a genuinely novel screen.

All three trust-shaped gates (Claude's workspace trust, Codex's directory trust, Copilot's folder
trust) resolve the same way: trust the repository once with that vendor, in its own session
(outside crew), or add its documented config entry —

| Vendor | File | Entry |
|---|---|---|
| Claude | `~/.claude.json` | `projects["<repo root>"].hasTrustDialogAccepted = true` |
| Codex | `~/.codex/config.toml` | `[projects."<repo root>"] trust_level = "trusted"` |
| Copilot | `~/.copilot/config.json` | `"trustedFolders": [ "<repo root>" ]` — seed both the path and its realpath |

— and the trust then covers every worker crew launches from a worktree of that repository too:
Claude and Codex key trust on the git main-checkout root and treat a `git worktree add` workspace
as the same repository, and this is measured (not merely documented) for Copilot as well, which
stores the repository root's realpath in `trustedFolders` regardless of which worktree a worker
launched from — see the same record's "Copilot worktree trust inheritance" section. Crew never
writes any of these entries itself: detect-and-escalate is the whole mechanism, and the
escalation's own question names this one-time step.

### TUI live conformance (0.5.0)

**Current.** Unlike everything above, this subsection is reproducible against the tree as it
stands today — `--mode tui` is the only live mode there is now.

A TUI-mode live suite (`crewd conformance --live --mode tui`) was added that spawns the real
vendor TUI on a PTY and drives it through the same injection path the runtime uses. It exercises
`probe`, `read_only_start_and_progress`, `follow_up`, `cancellation_scope`, and `session_resume`.
`session_resume` is **skipped** on every adapter (a single-process resume is not a daemon
restart; transcript recovery across a real restart is a separate e2e, tracked as a follow-up).
"runnable" below = the four non-resume scenarios.

| Adapter | Runnable pass | Notes |
|---------|---------------|-------|
| Claude  | 4 / 4 | fully green (TUI) |
| Codex   | 4 / 4 | fully green (TUI) — credits refilled; re-run 2026-08-27 on current main (after the intervening adapter changes) is byte-identical to the 2026-08-26 evening report. The earlier out-of-credits state ([`codex-tui-post-quota.json`](../release/live-conformance/codex-tui-post-quota.json), no turns observable at all) is retained only as historical exhaustion evidence |
| Copilot | 2 / 4 | `read_only_start_and_progress` + `follow_up` fail — CONFIRMED vendor monthly quota wall (`session.error`, `errorCode: quota_exceeded`, independently verified against the raw tailed session file, not just the harness's summary); probe + cancel proven. Not a capture defect: an earlier 2026-08-26 diagnosis blamed transcript-capture/discovery, but that was an untrusted-workspace bug fixed by commit `0ca5d5b` (2026-08-27) — discovery itself now succeeds here (`start=Ok(())`, `session=true`). **Erratum, resolved:** the fix used to be a preflight check inside this adapter's own conformance harness only (`ensure_copilot_workspace_trusted`), not wired into the production adapter. The production adapter now classifies Copilot's folder-trust dialog directly (see "TUI First-Run Gate Detection" above), so the harness exercises the real production path instead; the harness-only preflight was retired in the same change — see `release/live-conformance/2026-09-09-vendor-first-run-gates.md` and `release/live-conformance/2026-09-10-copilot-omp-first-run.md` |

Version provenance: the live reports deliberately record **no** vendor version (`version: null`) —
the TUI harness does not pin one, so a report is evidence about the adapter injection path, not
about a specific CLI release. Fixture-captured versions are *newer* than the historical headless
captures in the table above (same CLIs, later releases; the headless control plane those came from
is retired, see the historical notice above) — see "TUI vendor CLI version gates" below for the
exact pins. Gap: there is no recorded recipe for re-capturing the `*-tui` fixtures against future
CLIs (`capture-manifest.yml` governed recapturing the now-deleted headless fixtures specifically
and was deleted along with them by crew-v2 gap-closure; it never covered `*-tui`) — tracked
as an open item from that release.

Raw reports (verbatim, with an erratum on the overstated `session_resume` detail):
[`release/live-conformance/`](../release/live-conformance/).

### TUI vendor CLI version gates (current)

Each `TuiVendor::version_gate` decides, from a real `--version` probe, whether an installed CLI is
one this adapter's fixed argv/transcript-format assumptions were built against
(`AdapterError::incompatible_version` otherwise). Three of the four tolerate a version *range*
(their on-disk transcript format self-describes its own schema, so drift behind an unvalidated
minor release degrades gracefully rather than corrupting the journal); Copilot's is a discrete,
exact-match list instead (ACP is a wire protocol, not a self-describing file format, so an
untested build gets no benefit of the doubt):

| Vendor | Gate | Current range/list |
|---|---|---|
| Claude | Range | `1.0.0` – `2.99.99` |
| Codex | Range | `0.100.0` – `0.199.99` |
| OMP | Range | `18.0.0` – `18.99.99` |
| Copilot | Exact match | `1.0.73`, `1.0.75`, `1.0.78`, `1.0.80`, `1.0.81` (all negotiate ACP protocol version 1; `COPILOT_MIN_ACP_PROTOCOL_VERSION`/`COPILOT_MAX_ACP_PROTOCOL_VERSION` are both `1`) |

Version-pinned TUI wire *behavior* (as opposed to the gate above, which only decides
compatible/incompatible) lives in the committed fixtures instead — the exact CLI version each
vendor's fixture was captured against: claude-tui `2.1.241`, codex-tui `0.149.1`, copilot-tui
`1.0.80`. These predate `1.0.81` being added to Copilot's known-versions list above (the gate was
widened after fixture capture) and are all still within their vendor's current range/list.

## If a version isn't in either table

Neither table is exhaustive by design — an untested CLI version isn't assumed compatible just
because it's newer. If you hit a version gap:

1. Run the reproduce command for that adapter (above) and check whether it passes.
2. If it does, that's evidence worth adding here — open a PR updating the table.
3. If it doesn't, see [`docs/manual-testing.md`](manual-testing.md#4-worker-adapters) for how to
   isolate which scenario regressed.

For anything else — upgrading Crew itself, configuration, or the CLI — see
[`operations.md`](operations.md#upgrading), [`development.md`](development.md), and
[`cli-reference.md`](cli-reference.md).
