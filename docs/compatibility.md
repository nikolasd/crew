# Crew Compatibility Guide

**Audience & purpose:** anyone deciding whether Crew supports their platform, or checking which
adapter-conformance scenarios currently pass against which vendor CLI version. This is a
compatibility *matrix*, not a general reference — for configuration, protocol methods, or the
CLI, see [getting-started.md](getting-started.md) (the developer manual),
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
  only — installed users get a prebuilt binary, see [getting-started.md](getting-started.md)).
- **Linux**: glibc only; musl (e.g. Alpine) is rejected by `platform_supported`, not silently
  degraded.
- **Display backends**: Herdr and tmux both require the matching binary to be installed and,
  for tmux, an already-running session — see [`cli-reference.md`](cli-reference.md#crewd-display-probe)
  for the read-only probe. The terminal backend is always available as a fallback.

## Adapter Compatibility

The table below is **generated from real `--live` conformance runs**, not from prose. Each row
records the version the adapter's own probe observed and how many canonical scenarios that run
proved.

Reproduce with (`CREW_DISABLE_VENDOR_CLI` must be **unset** — it suppresses vendor invocation):

```bash
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

### TUI live conformance (0.5.0)

WP29 added a TUI-mode live suite (`crewd conformance --live --mode tui`) that spawns the real
vendor TUI on a PTY and drives it through the same injection path the runtime uses. It exercises
`probe`, `read_only_start_and_progress`, `follow_up`, `cancellation_scope`, and `session_resume`.
`session_resume` is **skipped** on every adapter (a single-process resume is not a daemon
restart; transcript recovery across a real restart is a separate e2e, tracked as a follow-up).
"runnable" below = the four non-resume scenarios.

| Adapter | Runnable pass | Notes |
|---------|---------------|-------|
| Claude  | 4 / 4 | fully green (TUI) |
| Codex   | 3 / 4 | `follow_up` fails on out-of-credits; spawn→type→submit→discover proven. A ~40-min-later rerun ([`codex-tui-post-quota.json`](../release/live-conformance/codex-tui-post-quota.json)) could observe **no turns at all** — no typed vendor reason — consistent with full credit exhaustion |
| Copilot | 2 / 4 | `read_only_start_and_progress` + `follow_up` fail on out-of-credits; probe + cancel proven |

Version provenance: the live reports deliberately record **no** vendor version (`version: null`) —
the TUI harness does not pin one, so a report is evidence about the adapter injection path, not
about a specific CLI release. Version-pinned TUI wire behavior lives in the committed fixtures
instead: claude-tui `2.1.241`, codex-tui `0.149.1`, copilot-tui `1.0.80`. Note these are *newer*
than the headless captures in the table above (same CLIs, later releases). Gap: unlike the headless
fixtures, none of the `*-tui` fixtures has a `capture-manifest.yml` entry yet, so there is no
recorded recipe for re-capturing them against future CLIs — tracked with the open WP29 items.

Raw reports (verbatim, with an erratum on the overstated `session_resume` detail):
[`release/live-conformance/`](../release/live-conformance/).

## If a version isn't in either table

Neither table is exhaustive by design — an untested CLI version isn't assumed compatible just
because it's newer. If you hit a version gap:

1. Run the reproduce command for that adapter (above) and check whether it passes.
2. If it does, that's evidence worth adding here — open a PR updating the table.
3. If it doesn't, see [`docs/manual-testing.md`](manual-testing.md#4-worker-adapters) for how to
   isolate which scenario regressed.

For anything else — upgrading Crew itself, configuration, or the CLI — see
[`operations.md`](operations.md#upgrading), [`getting-started.md`](getting-started.md), and
[`cli-reference.md`](cli-reference.md).
