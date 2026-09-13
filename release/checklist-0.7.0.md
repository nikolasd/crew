# Release checklist — crew 0.7.0

## Scope

0.7.0 is a **wave-2 bugfix and hardening release**: run-settlement/result-reporting fixes from the
live E2E re-run, a compile-time redaction guard enforcing that every `RuntimeEvent`-reachable
`String` field is redacted or allowlisted (see [ADR-0006](../docs/adr/0006-type-enforced-redaction-boundary.md)
and `docs/security/redacted-field-inventory-2026-09-07.md`), a breaking display-placement deletion,
dashboard hardening, and adapter model-name resolution against omp's own catalogue. This checklist
documents wave 2's full set of fixes and follow-ups, the later ones minted after this checklist's
first draft from the redaction-allowlist review and the redacted-field inventory sweep, as of
`origin/main` @ `743d7ae` (2026-09-07).

### Merged to main

| Fix | Commit(s) | What |
|---|---|---|
| Resumption causality | fd3635c, 853a46b | Resumption is caused, not inferred: a `waitingUser` run stays settled until an explicit follow-up or a genuine user transcript entry; the second commit (853a46b) guards the forwarding case and excludes tool-result entries. See [ADR-0027](../docs/adr/0027-turn-end-settles-a-run.md). |
| Result fold boundary | dc01131, f97712f | `run/result` skips content-free (tool-only) turn boundaries; the second commit (f97712f) fixes a misattributed ADR citation and adds the missing property test. See [ADR-0027](../docs/adr/0027-turn-end-settles-a-run.md)'s amendment. |
| `crew_transcript` array validation | c5986f7 | `crew_transcript`'s `request()` path validates array results via the schema map (was throwing on every call). See `docs/engineering-lessons.md`'s "A promise about behavior is only tested by running it". |
| Milestone digest freshness | e9e96d5 | Extension milestone digest freshness guard (replay vs. live via `EventDeliveryMeta.replay`) + unknown-adapter lookup race (`enrichRun()` awaited before listener fan-out) |
| Display placement deletion (part 1) | 86ba675 | **Breaking:** `Embedded` display placement deleted; backends resolve their own natural form by default; `runtime/status`'s `state_root`/`socket_path` fields added so a two-daemon mixup is diagnosable (`crates/runtime/src/ipc/server.rs`). See "Breaking changes" below and [ADR-0029](../docs/adr/0029-placement-follows-the-backend-embedded-deleted.md). |
| Model-name resolution | d0dc661 | Adapter model-name resolution against omp's own catalogue (`omp models ls --json`): alias table, exact→alias→unique-substring→ambiguous-refuses→unknown-passes-through resolution order. See `docs/manual-testing.md` §3e. |
| Dashboard replay-on-connect | merged as d2190c4, rewritten to **0c10f62** | Dashboard: replay the journal on SSE connect; honest disconnect state. See "History rewrites" below and `docs/manual-testing.md` §8. |
| Copy-drift test fix (rider) | e05b590 | The copy-drift test added alongside this same change never actually checked anything; fixed (`packages/extension/src/tools/tools.test.ts`) |
| Resume-cause event kind | 25a9874 | Journals the resume cause as its own event kind (`ResumeCause`, `crates/protocol/src/event.rs`). See [ADR-0029](../docs/adr/0029-placement-follows-the-backend-embedded-deleted.md)'s Links. |
| Bundle path-filter widening | c9f83cd | `auto-commit-dist.yml`'s path filter widened — a packaging-only change could land without its bundle refreshed |
| `PaneDowngraded` event | 07686be | Typed `PaneDowngraded` event, sticky monitor flag, digest, for a resolved-backend pane-creation failure. See [ADR-0029](../docs/adr/0029-placement-follows-the-backend-embedded-deleted.md). |
| Compile-time redaction guard | 79b301c, 3c205c4, e4c52f8 | **CLOSED.** The first two commits (79b301c, 3c205c4) redact `WorkspaceEvent::CleanupFailed.error` and `plan/propose`'s `description`/`task_text`; the third (e4c52f8) adds the compile-time guard enforcing the redaction obligation at declaration for every `RuntimeEvent`-reachable `String` field. First CI appearance (commit e4c52f8): **all checks pass**, including the guard's own test and the full workspace suite — the guard did not need to grow a caveat to land clean. See [ADR-0006](../docs/adr/0006-type-enforced-redaction-boundary.md). |
| Dashboard token rejection | 19711c5 | Dashboard stops retrying on a rejected token; logs a replay failure instead of hanging. See `docs/manual-testing.md` §8. |
| Redaction allowlist reasons rewritten | 0aae002 | Vendor-ref allowlist entries in `NON_REDACTED_STRING_FIELDS` rewritten to cite the actual redaction mechanism (with file:line) instead of describing expected contents. See `docs/engineering-lessons.md`'s "A reason must name the property that makes the field safe, not the intent behind it". |

Also merged, supporting but not wave-2-numbered: commit 3a77aac (three documentation gaps: `PaneDowngraded`, `RunResumed`, false test citations) and commit 743d7ae (removed the tracked `.claude/settings.json` — Serena hook config referencing two scripts that don't exist in this repo; hooks now come from each user's own `settings.local.json`).

### Not yet merged / follow-up work (tracked, out of scope for this cut unless noted)

- **Redaction allowlist reasons, follow-up (OPEN as of this writing)**: corrects a wrong doc reference introduced by the redaction allowlist rewrite above (commit 0aae002) (`vendorParentRef`'s description named a snake_case sibling field that doesn't exist on its camelCase object) and drops an internal Rust type name from a shipped schema description. Small, no behavior change — confirm merged before tagging.
- **Dashboard vendor logos** (P2, cosmetic): dashboard missing adapter/vendor logos. In progress, reported PR-ready pending one force-push. Not required for 0.7.0 functionally; maintainer has ruled it in scope for this cut if it lands in time. See `docs/manual-testing.md` §8.
- **Redacted-field inventory**: inventory of every `Redacted`-typed protocol field's construction site, confirming each is genuinely sanitized or genuinely runtime-authored. Audit complete (one finding, folded into the unvalidated-`instance_id` item below); the durable write-up is `docs/security/redacted-field-inventory-2026-09-07.md`.
- **Bracketed-paste CI flake** (P2, possibly P1): the bracketed-paste guard (see [ADR-0030](../docs/adr/0030-paste-delivery-bounded-on-progress.md)) fails intermittently on the macOS CI runner. In progress, not a 0.7.0 code change.
- **Unvalidated `instance_id` in `Redacted` fields** — fixed in commit 02a0b54: `instanceId` is now bounded at the handshake, and the four guard reasons that assumed an unvalidated client identity were corrected. See `docs/security/redacted-field-inventory-2026-09-07.md`'s "The one finding".
- **Backticked-name allowlist scoping** — fixed in commit c00ba0e: backticked property references are now scoped to their own object instead of resolving globally against the whole schema, and `ALLOWED_UNRESOLVED_BACKTICKED_NAMES`/its siblings gained the reverse-staleness check they lacked. See `docs/engineering-lessons.md`'s "Put the reason beside the entry it justifies, and check the list against itself".
- **Escalations carry the worker's question** (P2, pending, unassigned): `EscalationRaised.question` ships as an optional field that no code path populates — surfaced by the redacted-field inventory sweep. Maintainer has ruled escalations must carry the worker's actual question. Not fixed in this release. See `docs/future-features.md`'s "Escalations Carry the Worker's Actual Question".
- **Paste-write bound on progress** — fixed in commit 127a64c: the paste-write bound now measures write progress (`PASTE_STALL_WINDOW`, 2s with no bytes accepted) rather than elapsed time, with `PASTE_CHUNK_WRITE_TIMEOUT` (now 90s, up from 10s) as the absolute backstop behind it. See "Breaking changes and operational notes" below and [ADR-0030](../docs/adr/0030-paste-delivery-bounded-on-progress.md) — a paste failure is still worth a host-load check during the supervised E2E, since that run is the fix's first live exercise.

### Breaking changes and operational notes

- **`Embedded` display placement deleted (commit 86ba675).** A pre-0.7.0 state directory whose event journal contains a `placement: "embedded"` value will refuse to replay under this binary rather than silently misinterpreting it — this is the intended failure mode, not a bug. **Anyone testing or upgrading against an existing state directory needs a fresh one** (`CREW_STATE_DIR` pointed at a new, empty path); replaying an old journal containing the deleted value produces a legible refusal naming the remedy, not a panic. See [ADR-0029](../docs/adr/0029-placement-follows-the-backend-embedded-deleted.md).
- **Compile-time redaction guard (commits 79b301c–e4c52f8).** Every `String`-typed field reachable from `RuntimeEvent` must now be either `Redacted` (built via `Redactor::sanitize_fragment`/`Redactor::redact_text`, or `Redacted::assert_runtime_authored` when no caller/vendor text can reach it) or explicitly listed in `NON_REDACTED_STRING_FIELDS` with a stated reason — enforced by a test, not a convention. A future change that adds a new `String` field to `RuntimeEvent` without satisfying one of those two paths fails CI, not review. See [ADR-0006](../docs/adr/0006-type-enforced-redaction-boundary.md).
- **omp 18.1.13 was the tested host version at the time this checklist was written** (upgraded from 18.0.11 during wave 2; the peer range `>=17.0.7 <19` covers it). The plugin-root `--extension` directory-form requirement (F4) was diagnosed on 18.0.11 and re-confirmed present on 18.1.13. *(This is a point-in-time record — the installed version has since moved past 18.1.13; check the currently installed version rather than treating this line as current.)*
- **Two `main` history rewrites landed today**, both attribution cleanups, both verified identical-tree:
  - `d2190c4` → **`0c10f62`** (dashboard replay-on-connect): the squash-merge commit's message carried the `Claude-Session:` trailer twice (GitHub concatenated two source commit messages); the maintainer force-pushed-with-lease a rewrite removing both lines. Verified independently: `git show -s --format=%T d2190c4` and `...0c10f62` both print `f1f880d…` (identical tree); `d2190c4`'s message contains `Claude-Session` twice, `0c10f62`'s contains it zero times; `d2190c4` is no longer an ancestor of `origin/main`, `0c10f62` is.
  - Standing check adopted from here forward, run after every merge: `git show -s --format=%B origin/main | grep -c Claude-Session` → must print `0`.
- **Model-name resolution (commit d0dc661)** changes adapter behavior for `crew_profile`/model selection: names now resolve through omp's own catalogue (exact match → known alias → unique provider-scoped substring → ambiguous refuses and lists candidates → unknown passes through with a visible note), not a hardcoded table.
- **A large-prompt run start failing on a loaded host is now much less likely (fixed in commit 127a64c).** The paste-write bound previously measured elapsed time, not write progress — under host oversubscription (observed ~21% failure rate at ~2× load in testing), a starved-but-still-advancing write could trip the same timeout a truly hung vendor would. It now measures progress directly (no bytes accepted for 2 seconds is the primary signal; a 90-second absolute backstop, up from 10 seconds, still covers a genuinely hung vendor). The supervised E2E is this fix's first live run. **If a large-prompt start still fails, check host load before treating it as a vendor or adapter regression.** See [ADR-0030](../docs/adr/0030-paste-delivery-bounded-on-progress.md).

## Version

- [ ] `crates/runtime/Cargo.toml` → `0.7.0`
- [ ] `crates/protocol/Cargo.toml` → `0.7.0`
- [ ] `packages/extension/package.json` → `0.7.0` (source of truth for `check_version_coherence`)
- [ ] `.claude-plugin/marketplace.json` → `metadata.version` + `plugins[].version` → `0.7.0`
- [ ] `Cargo.lock` refreshed via `cargo check --workspace` (never hand-edited)
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
leaves are rebuilt at 0.7.0 with wave-2's Rust fixes (run settlement, redaction, display placement)
baked into the binary.

- [ ] All four `crewd-${leaf}` assets uploaded + executable bit intact
- [ ] All four `crewd-${leaf}.manifest.json` uploaded
- [ ] `release-manifest.json` uploaded (version 0.7.0, per-leaf sha256s present)
- [ ] `/crew-install` (download.ts) resolves each leaf by SHA-256 against the manifest —
      install-path chain verified against the live release: leaf manifest sha256 == downloaded
      binary sha256 == entry in release-manifest.json; binary executes (`crewd 0.7.0`)

## Build / gate (CI)

- [ ] `bun run check` green (schema drift + build + all tests, `CREW_DISABLE_VENDOR_CLI=1`) — all
      CI checks green on the release PR/merge (CI only — includes `build`; never run locally on
      this branch)
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo fmt --all --check`
- [ ] `bun run generate --check` (version coherence at 0.7.0)
- [ ] Extension bundle (`packages/extension/dist/index.js`) refreshed in CI's environment:
      `refresh-bundle` workflow (once merged to `main`) or the container equivalent documented
      there. The bundle embeds Bun's platform-specific module shim — a darwin-arm64 rebuild does
      NOT byte-match CI's linux-x64 output (observed on Bun 1.3.14) — so build on linux-x64, the
      platform `bundle-check` verifies against, and commit the `dist-linux-x64` artifact. This is
      the ONE sanctioned `dist/index.js` change; it must land on the bundle-refreshed main commit
      that gets tagged. (Commit c9f83cd widened the path filter that auto-commits this, specifically
      so a packaging-only change can't skip it again.)

## Conformance evidence

- [ ] Fixture conformance (`tests/conformance`, `CREW_DISABLE_VENDOR_CLI=1`) green via CI — no
      billed calls
- [ ] The compile-time redaction guard (`every_reachable_string_field_is_redacted_or_allowlisted`,
      `crates/protocol/src/event.rs`; see [ADR-0006](../docs/adr/0006-type-enforced-redaction-boundary.md))
      passes as part of `cargo test --workspace` — confirms no
      wave-2 change (or anything landing after) reintroduced an unaccounted `String` field on
      `RuntimeEvent`
- [ ] **Live end-to-end test (supervised) — ATTEMPT 3 RUN 2026-09-08 against `main` @ cab041a;
      FAILED, gate stays closed.** Record: `release/live-conformance/2026-09-08-live-e2e-attempt-3.md`.
      P0-P4 executed; **P5-P9 not run**. Pass bar is P1-P7 all green (P5 explicitly), so the bar was
      not met and could not have been met by this attempt. Seventeen findings, five P1 candidates —
      fabricated success with a start failure leaving no durable trace, first-run vendor
      prompts invisible with paste-and-Enter into them, a parked run declared `lost` at
      five minutes, and a parked run flooding the journal at ~30 rows/s.
      **v0.7.0 does not cut on this attempt**; P5-P7 wait for attempt 4 against the fixed build.
      No stop condition fired: redaction sweep clean, no crash loop, no billed runaway. The wave-2 fixes
      listed above, including the display-placement breaking change, are gated on a supervised live E2E
      re-run before tagging. The runbook is `release/live-e2e-runbook-0.7.0.md` (companion to
      `docs/manual-testing.md`), phases **P0 through P10** — P0 is preflight (no model call,
      cross-references `docs/manual-testing.md` §Prerequisites and §Owning what you test); the
      later phases exercise cold start, the dashboard's no-cookie token gate, a real billed run,
      turn settlement/`waitingUser` behavior, run termination, and full-restart replay. The runbook
      is written against omp 18.1.13 and `main` @ 0aae002 — **re-verify it is current against
      743d7ae before running it**, and use a brand-new `CREW_STATE_DIR` (an old one carrying a
      pre-display-placement-deletion `"embedded"` placement event will legibly refuse to replay, not silently
      misbehave — see "Breaking changes" above).
  - [ ] Supervised live E2E re-run completed against this checklist's commit — **attempt 3
        (2026-09-08) did not complete: stopped after P4 by maintainer decision once five P1
        candidates had accumulated.** *(Historical from here: a later attempt ran against the
        fixed build and itself did not complete; a further attempt was then cancelled outright
        when the control-plane direction changed. This checkbox and the two lines above it are a
        point-in-time record of attempt 3 specifically, not a live status; the current state of
        the live-E2E gate is tracked outside this repository, not on this line.)*
  - [ ] Each wave-2 behavioral claim above (resumption causality, result fold boundary,
        `crew_transcript` array validation, display placement deletion, model-name resolution,
        dashboard replay-on-connect, resume-cause event kind, `PaneDowngraded` event, dashboard
        token rejection) observed matching its stated fix during the E2E
  - [x] Findings from the run, if any, filed as new tickets rather than blocking this checklist on
        a full re-triage — **done for attempt 3: seventeen findings recorded** (see the attempt-3
        record). Two of these are maintainer decisions rather than defects: whether
        a cleanly-exited run that did no work is `failed` or `lost`, and whether the
        runtime may auto-settle a parked run at all, given ADR-0025 and the skill promise a leader
        choice.
- [ ] Live adapter conformance evidence: carries forward from v0.6.0 — no NEW adapter-selection
      behavior beyond the model-name resolution work (TypeScript-only, `packages/extension`; no
      Rust adapter changes) — see `release/checklist-0.6.0.md` and `release/live-conformance/*.json`.
      That resolution logic itself is exercised by its own test suite, not by the live
      adapter-conformance harness; confirm during the E2E above that model selection resolves as
      expected against a real omp catalogue.

## Manual QA (docs/manual-testing.md)

**Complete BEFORE tagging v0.7.0 — the interactive surface is largely unchanged, but run
settlement, result scanning, display placement, and model resolution are all substantially
revised.** Use `docs/manual-testing.md` as the base walkthrough (§1 the daemon through OMP, §2 the
embedded monitor, §3 the orchestration tools) together with the supervised E2E runbook referenced
above — the runbook is the fuller, ordered (P0–P10) sequence to actually run through; this section
is what gets ticked off during it, not a separate pass.

- [ ] Supervised live E2E (see Conformance evidence above) completed and its checklist items ticked
- [ ] Settlement/resumption behavior (see [ADR-0027](../docs/adr/0027-turn-end-settles-a-run.md)) matches the live run state during interactive test
- [ ] Result scanning returns the first real answer, not an empty tool-only boundary
- [ ] A display backend resolves its natural placement with `Embedded` absent from any new event
      (see [ADR-0029](../docs/adr/0029-placement-follows-the-backend-embedded-deleted.md)) — confirm on a FRESH state directory, not a carried-over one
- [ ] Model-name resolution behaves per its documented resolution order against a live
      omp catalogue (`docs/manual-testing.md` §3e)

## Release

- [ ] Tag `v0.7.0` — annotated, on the bundle-refreshed main commit (see Build/gate above)
- [ ] **A plain `v0.7.0` tag push does NOT trigger `release.yml`** — its push trigger matches only
      suffixed `v[0-9]+.[0-9]+.[0-9]+-*` tags. Publish via `workflow_dispatch` with `ref: v0.7.0`;
      the workflow publishes iff the ref is a `v*` tag whose value matches
      `packages/extension/package.json`'s version. Two distinct failure modes to know apart when
      dispatching: a `ref` that IS a `v*` tag but whose value does NOT match the package version
      hard-fails the run (red = wrong tag — the mismatch is caught and the run stops); a `ref` that
      is a branch name (not a tag at all) runs a silent dry run that publishes nothing (green but
      empty — you passed a branch, not a tag, and the workflow has nothing to fail on).
- [ ] All 9 assets verified (4 binaries + 4 manifests + `release-manifest.json`)
- [ ] Install-path chain verified against the live release (download a leaf binary, sha256 matches
      both manifests, executes reporting `crewd 0.7.0`)
- [ ] Marketplace catalog (`marketplace.json` @ `0.7.0`) published
- [ ] Post-merge trailer check on the tagged commit: `git show -s --format=%B v0.7.0 | grep -c
      Claude-Session` → `0`
