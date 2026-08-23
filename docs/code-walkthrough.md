# Code walkthrough

A guided tour for navigating, tracing, debugging, and testing the codebase. Read
[architecture.md](architecture.md) first for the *why*; this document is the *where and how*.

**Audience & purpose:** contributors navigating, debugging, or testing the codebase — a companion
to [getting-started.md](getting-started.md), the developer manual. If you're looking for what the
extension's tools *do* rather than how they're implemented, see [plugin-usage.md](plugin-usage.md)
instead.

New to Rust? Read the [Rust primer](rust-primer.md) alongside this — it teaches Rust using this
repository's own code.

## 1. Map of the source

### `crates/protocol` — the wire contract (start here)

Small, dependency-light, and the vocabulary for everything else.

| File | What lives there |
|---|---|
| `src/lib.rs` | Re-exports everything; the crate's public API is this one page |
| `src/ids.rs` | `uuid_id!` macro generating the 9 id newtypes (`ProjectId`, `RunId`, `PolicyViolationId`, …) |
| `src/version.rs` | `ProtocolVersion`, `VersionRange` |
| `src/rpc.rs` | JSON-RPC envelopes, `InitializeParams/Result`, `ClientAuth` roles, `RuntimeStatus`, `error_code` constants (`BatmanMethod` itself now lives in `method.rs`, re-exported here) |
| `src/method.rs` | `BatmanMethod` — every JSON-RPC method name, foundation and orchestration alike |
| `src/event.rs` | `EventEnvelope`, `RuntimeEvent`, `Timestamp`, `ContentClass`/`Classified<T>`, `RunFlags`, `DiagnosticLevel`, `EventSource`, `RuntimeEventKind` |
| `src/task.rs` | `TaskRef` |
| `src/worker.rs` | `WorkerProfileRef`, `Worker` |
| `src/run.rs` | `Run`, `RunSpec`, `RunState` (+ `can_transition_to`/`is_terminal`) |
| `src/message.rs` | `RunMessage`, `MessageKind`, `DeliveryState` |
| `src/approval.rs` | `ApprovalRequest`, `ApprovalDecision` |
| `src/coordination.rs` | worker-safe request/result types, `COORDINATION_PAYLOAD_MAX_BYTES`, `COORDINATION_RATE_LIMIT_PER_MINUTE`, `CoordinationAskPolicyParams`, `CoordinationChildDecision`, `CoordinationPeersParams`, `CoordinationPublishArtifactParams`, `CoordinationReportBlockedParams`, `CoordinationRequestChildParams`, `CoordinationSendParams`, `CoordinationTaskParams` |
| `src/workspace.rs` | `LeaseRequest`, `LeaseMode`, `ReleaseRequest`, `ApplyRequest`, `ApplyStrategy`, `ApplyResult`, `InspectRequest`, `InspectResult`, `IsolationKind`, `WorkspaceEvent`, `WorkspaceInfo`, `WorkspaceLease`, `WorkspaceState` |
| `src/display.rs` | `DisplayBackend`, `DisplayConfig`, `DisplayPlacement`, `DisplayStatus` |
| `src/artifact.rs` | `Artifact`, `ArtifactFetchResult`, `ArtifactFetchRequest`, `ArtifactKind`, `ArtifactListRequest`, `ArtifactListResult` |
| `tests/wire_contract.rs` | Proves camelCase + `deny_unknown_fields` on the wire |
| `tests/domain_contract.rs` | `RunState` lifecycle table, `RunFlags` field names, `BatmanMethod` orchestration variants |
| `tests/coordination_contract.rs` | Message kinds, delivery states, coordination request/result wire shapes |
| `tests/fixtures.rs` | Deserializes the golden fixtures through the real types |

### `crates/runtime` — the `crewd` daemon

| File | What lives there |
|---|---|
| `src/main.rs` | Thin entry point; calls `cli::run()` |
| `src/cli.rs` | clap definitions for `serve`/`status`/`stop`/`version`/`schema`/`monitor`/`audit`; maps outcomes to exit codes (73 = lost the singleton race) |
| `src/lifecycle.rs` | `serve()`/`status()`/`stop()`: flock singleton, lock metadata, idle shutdown, graceful-stop ordering, log routing, doctor integration |
| `src/doctor.rs` | `Doctor` — health checking with rollout gates, adapter availability, configuration validity |
| `src/recovery.rs` | `RecoveryCoordinator` — recovers every non-terminal run at startup (ownership, not age); `recover_paused`/`recover_waiting` gates; `DEFAULT_STALE_RUN_THRESHOLD` for the doctor's read-only `stale_runs` report |
| `src/paths.rs` | `RuntimePaths::resolve`, VCS-root discovery, `repository_id_from_canonical_root` |
| `src/security/mod.rs` | `StateRoot::resolve` precedence, `ensure_private_dir`/`ensure_private_file` (0700/0600, atomic) |
| `src/security/rules.rs` | Built-in redaction regex patterns (AWS keys, API keys, GitHub tokens) |
| `src/security/redaction.rs` | `Redactor`, `RawRuntimeEvent`, `PersistableEvent`, `SanitizedJson` — the redaction boundary |
| `src/db/actor.rs` | `DatabaseHandle` + the actor thread owning the SQLite connection |
| `src/db/migrations.rs` | PRAGMAs, migration 1 (`events`, `operations`), migration 2 (`worker_profiles`, `tasks`, `workers`, `runs`, `messages`, `approvals`) |
| `src/db/models.rs` | Row types (`ReplayedEvent`, `OperationIntent`, `Diagnostics`) |
| `src/ipc/mod.rs` | `ServerConfig`, `ClientPrincipal` + role method tables, `PeerCredentials` reader trait, `WorkerCredentialVerifier` trait, `IpcError` |
| `src/ipc/server.rs` | Socket bind (owner-only, SUN_LEN guard), accept loop, UID admission, idle bookkeeping, constructs `OrchestrationService`/`CoordinationBroker` and the one `events_tx` broadcast channel they share |
| `src/ipc/connection.rs` | Per-connection reader/writer split, initialize handshake, method dispatch (routes orchestration methods to `OrchestrationService`, `coordination/*` to `CoordinationBroker`), replay/subscribe |
| `src/domain/repository.rs` | `DomainRepository` — every projection-mutating command; `append_and_apply` (event + projection, one transaction); `Committed`, `embed_envelope`/`take_envelope` |
| `src/domain/transitions.rs` | `check_transition`, `TransitionError::Illegal` — the canonical `RunState` lifecycle relation |
| `src/service/orchestration.rs` | `OrchestrationService` — routes every Task/Worker/Run/Message/Approval/Reconcile method to `DomainRepository` or `service/query.rs` |
| `src/service/query.rs` | Read-only lookup closures (`task_get_op`, `run_state_op`, etc.) run through `DatabaseHandle::run_domain_op` |
| `src/service/run_driver.rs` | `RunDriver` trait, `RunDriverContext`, `FakeRunDriver` (`queued -> starting -> working`) |
| `src/adapter/trait.rs` | `Adapter` trait with `start`/`resume`/`send`/`cancel`/`dispose` |
| `src/adapter/registry.rs` | `AdapterRegistry` — implements `RunDriver` against four worker adapters, `AdapterAuthorization` trait, `FixtureAuthorization`/`DenyByDefaultAuthorization` |
| `src/adapter/event_sink.rs` | `DomainAdapterEventSink` — sanitizes, journals, and broadcasts adapter events |
| `src/adapter/run_lifecycle.rs` | `RunLifecycleSink` — applies `queued -> starting -> working` and the terminal edge from adapter evidence |
| `src/adapter/error.rs` | `AdapterError` — adapter-specific error types |
| `src/adapter/capability.rs` | `AdapterCapabilities` — capability declarations for each adapter |
| `src/adapter/mcp_config.rs` | MCP configuration generation for adapter processes |
| `src/adapter/profile.rs` | `WorkerProfile` — worker profile definitions |
| `src/adapter/profile_store.rs` | `ProfileStore` — adapter profile persistence |
| `src/adapter/terminal.rs` | Terminal adapter backend |
| `src/adapter/claude/mod.rs` | `claude stream-json` protocol adapter |
| `src/adapter/claude/command.rs` | Claude CLI command construction |
| `src/adapter/claude/protocol.rs` | Claude protocol types |
| `src/adapter/codex/mod.rs` | `codex app-server` protocol adapter |
| `src/adapter/codex/schema.rs` | Codex schema definitions |
| `src/adapter/copilot/mod.rs` | `copilot --acp` protocol adapter |
| `src/adapter/copilot/client.rs` | Copilot ACP client implementation |
| `src/adapter/copilot/compatibility.rs` | Copilot compatibility checks |
| `src/adapter/omp_rpc/mod.rs` | `omp --mode rpc` protocol adapter |
| `src/adapter/omp_rpc/client.rs` | OMP-RPC client implementation |
| `src/coordination/broker.rs` | `CoordinationBroker` — record-before-delivery messaging, `sweep_unacknowledged_as_unknown` |
| `src/coordination/scope_token.rs` | `ScopeTokenStore` (mint/verify), `PidAncestryChecker` |
| `src/coordination/rate_limit.rs` | `RateLimiter` — single 30-calls/minute/sender sliding window, charged by every journaling coordination call (`coordination/send`, `coordination/requestChild`, `coordination/publishArtifact`) |
| `src/coordination/mcp.rs` | MCP tool registry proxy |
| `src/coordination/mcp_protocol.rs` | MCP protocol types |
| `src/approval/service.rs` | `ApprovalService` — `request`/`decide`, ownership/idempotency/settled-run enforcement, `ApprovalCallback` seam |
| `src/supervisor/process.rs` | Process-group scoped spawn with bounded stdio and escalation |
| `src/supervisor/environment.rs` | Redacted environment snapshots |
| `src/supervisor/output.rs` | Rotating stdout/stderr capture |
| `src/workspace/lease.rs` | Lease arbitration |
| `src/workspace/materialize.rs` | Materializes workspace changes |
| `src/workspace/apply.rs` | Applies workspace changes |
| `src/workspace/inspect.rs` | Inspects workspace state |
| `src/workspace/copy.rs` | Workspace file copying utilities |
| `src/workspace/artifact_store.rs` | Stores build artifacts |
| `src/workspace/git.rs` | Git integration for workspace operations |
| `src/display/terminal.rs` | Terminal display backend |
| `src/display/herdr.rs` | Herdr display backend |
| `src/display/tmux.rs` | Tmux display backend |
| `src/canonical_json.rs` | Canonical, recursively key-sorted JSON bytes for hashing and durable storage |
| `src/config/merge.rs` | Configuration merging with strict unknown-key rejection |
| `src/policy/evaluate.rs` | `RuntimePolicy` with SHA-256 fingerprint, `RolloutGates` |
| `src/audit/export.rs` | JSONL export |
| `src/audit/retention.rs` | Event retention and pruning |
| `src/conformance/scenario.rs` | Adapter conformance test scenarios |
| `src/conformance/report.rs` | Conformance test reporting |
Integration tests in `crates/runtime/tests/` are the daemon's behavioural spec — one file per
subsystem (`paths`, `database`, `redaction_boundary`, `ipc`, `lifecycle`, `domain_repository`,
`orchestration_rpc`, `coordination`, `approval`, `adapter_contract`, `adapter_registry`,
`claude_adapter`, `codex_adapter`, `copilot_adapter`, `omp_rpc_adapter`, `supervisor`,
`workspace_apply`, `workspace_lease`, `workspace_materialize`, `display_registry`, `display_selector`,
`herdr_display`, `tmux_display`, `terminal_adapter`, `monitor_cli`, `audit`, `config`, `conformance`,
`coordination_mcp`, `redaction`). The lifecycle tests run the real compiled binary
(`env!("CARGO_BIN_EXE_crewd")`) as real processes.

### `crates/xtask` — build tooling

One file (`src/main.rs`): `generate [--check]` (schema + ts-rs bindings, deterministic,
temp-dir byte-compare in check mode) and `package --target <triple> --binary <path>` (installs a
binary into a leaf package with a deterministic manifest).

### `packages/extension` — the OMP extension (`@nikolasd/crew`)

| File | What lives there |
|---|---|
| `src/index.ts` | Default-export extension factory; registers `crew_health`, `crew_doctor`, `/crew-status`, `/crew-doctor`, the 11 orchestration tools (via `tools/index.ts`), OMP-native lifecycle listeners (`omp-native/`), and the embedded monitor (`monitor/controller.ts`) |
| `src/status.ts` | `getRuntimeStatus(ctx)` and `resolveClient(ctx)` — the shared status path and liveness-aware client resolver; reuses the cached connection while open, reconnects on demand
| `src/doctor.ts` | `crew_doctor` tool / `/crew-doctor` command — shells out to `crewd doctor --json`, no live connection required
| `src/client.ts` | `CrewClient` — NDJSON framing, byte-exact caps, request correlation, Ajv validation, and `isClosed` liveness flag for cache invalidation
| `src/runtime.ts` | `ensureRuntime` (connect-or-spawn, authenticates as `ompExtension`), `buildServeArgs`, `resolveOverride` (`OMP_CREW_BINARY` validation), `repositoryIdFromRoot` |
| `src/state.ts` | `resolveStateRoot(env, home)` — must stay semantically identical to Rust's `StateRoot::resolve` |
| `src/platform.ts` | `resolveCrewd` tuple mapping, integrity/version checks, typed errors, `detectLibc` |
| `src/integrity.ts` | `sha256File` |
| `src/approval-ui.ts` | Approval UI components |
| `src/tools/index.ts` | Registers all 11 tools with OMP |
| `src/tools/shared.ts` | `callOrchestration` — the one execute body every orchestration tool uses; maps `JsonRpcRemoteError` to a stable tool error |
| `src/tools/{profiles,workers,tasks,runs,workspaces,artifacts,children,violations,messages,approvals,reconcile}.ts` | `crew_profile`, `crew_worker`, `crew_task`, `crew_run`, `crew_workspace`, `crew_artifact`, `crew_child`, `crew_violation`, `crew_message`, `crew_approval`, `crew_reconcile` — see [plugin-usage.md](plugin-usage.md) for what each does |
| `src/omp-native/events.ts` | Normalizes `task:subagent:lifecycle\|progress\|event` bus payloads into `OmpNativeAgentFact` |
| `src/omp-native/reconcile.ts` | `OmpNativeReconciler` (150 ms progress coalescing, terminal-immediate), `reconcileAcrossRestart` (undetected parent-scoped runs become `lost`), `createOmpProcessEpoch`, `reconcileWithRuntime` |
| `src/monitor/model.ts` | `reduceEvent` — the pure event-reducer building `MonitorState` |
| `src/monitor/render.ts` | Turns `MonitorState` into the widget's concise lines + per-run status detail |
| `src/monitor/controller.ts` | `registerMonitor` — replay-first `session_start` wiring, `/crew [status <runId>]`, retry-on-reconnect |
| `src/monitor/compat.ts` | Test-only `assertCompatiblePiCodingAgentVersion` (never called at runtime — see [`engineering-lessons.md`](engineering-lessons.md#never-use-with--type-json--imports-at-extension-load-time)) |

Each module has a sibling `*.test.ts`. `client.test.ts` and `index.test.ts` spawn the real daemon.

### `packages/protocol-ts` — generated contract (`@nikolasd/batman-protocol`)

`src/generated/*.ts` and `schema/crew.schema.json` are build outputs — regenerate, never edit.
`src/validate.ts` is hand-written: it compiles Ajv validators once (`validateInitializeResult`,
`validateRuntimeStatus`, `validateEventEnvelope`, the JSON-RPC envelope validators) and exports
`assertValid` + `ValidationError`.

### `fixtures/` — cross-language golden files

If Rust and TypeScript must agree on something, a fixture pins it: protocol frames
(`fixtures/protocol/`), state-root precedence (`fixtures/state/`), repository-id hashing
(`fixtures/repo-id/`), and the status result shape (`fixtures/omp/`). Both language test suites
consume them, so unilateral drift fails tests.

## 2. Trace: what happens when OMP runs `/crew-status`

Follow this once with the files open and you will have seen every layer.

1. **Registration** — OMP loads the extension and calls the default export
   (`index.ts:crewExtension`), which registers the tool and command; both handlers call
   `getRuntimeStatus(ctx)` with the context from `context.ts:buildStatusContext`.
2. **Client acquisition** — `status.ts:resolveClient` checks the cached `CrewClient`'s `isClosed` flag.
   If the socket is still open, it returns the cached instance immediately. If the client is closed
   (daemon idle-exited or socket errored), it tears down the stale reference and calls
   `runtime.ts:ensureRuntime` to reconnect.
3. **Connect-or-spawn** — `ensureRuntime` computes the socket path
   (`resolveStateRoot` + `repositoryId`) and tries to connect. If nothing answers: `selectBinary`
   validates `OMP_CREW_BINARY` (or asks `platform.ts:resolveCrewd` for a packaged binary),
   spawns `crewd serve --state-dir … --repo … --idle-seconds …` detached, and retries with
   backoff (≤5 s).
4. **Daemon startup** — `cli.rs` parses args → `lifecycle.rs:serve` resolves `RuntimePaths`, takes
   the flock (loser exits 73), opens `DatabaseHandle` (migrations + PRAGMAs)
   runs `Doctor::check()` (rollout gates, adapter availability),
   appends a redacted `runtimeStarted` event through the `Redactor`, binds the owner-only socket
   (`ipc/server.rs:bind`), starts logging (`runtime.log` when detached).
5. **Handshake** — the client sends `initialize` (first frame, 4 MiB bootstrap cap). The server
   already checked the peer UID at accept time. `connection.rs` validates the version range,
   canonicalizes the ompExtension agent directory, negotiates `maxFrameBytes`, computes
   `nextSequence` via `max_sequence()`, and returns `InitializeResult` with the role's allowed
   methods. The client Ajv-validates the result.
6. **The call** — `client.request("runtime/status", …)` → dispatch checks the role table →
   `RuntimeStatus` comes back → Ajv validates → `status.ts` formats `content` text and returns the
   validated object as `details`. On any failure, `failureResult` returns
   `{ isError, code, message (generic), doctorCommand }` — no paths, no stack traces.

The event path is the same shape on the write side:
`RawRuntimeEvent → Redactor::sanitize → PersistableEvent → DatabaseHandle::append_event`
(commit, then reply) → `events/event` notification to subscribers / `events/replay` for
reconnecting clients.

## 3. Trace: submitting a run through `crew_run`, and the monitor observing it live

Same idea as §2, but through the orchestration surface — follow it once and you've seen how a
mutation, the durable journal, and the embedded monitor connect. This trace is the symbol-level
companion to the design-level sequence in
[`architecture.md` § Data Flow: Event Lifecycle](architecture.md#data-flow-event-lifecycle).

1. **The tool call** — the model calls `crew_run` with `{ op: "submit", taskId, workerId }`
   (`tools/runs.ts`); `execute` calls `ctx.getClient(cwd)` (the *same* cached `ompExtension`
   client every orchestration tool and the monitor share — see [`engineering-lessons.md`](engineering-lessons.md#cached-client-must-authenticate-with-the-union-of-all-roles) for why
   its role matters) and `callOrchestration(client, "run/submit", params)`
   (`tools/shared.ts`) — nothing more; no worker selection, no retry, no lifecycle inference here.
2. **Dispatch** — `connection.rs::dispatch` sees `BatmanMethod::RunSubmit` is one of the
   orchestration methods, forwards the raw params to `OrchestrationService::dispatch`
   (`service/orchestration.rs`), which the role table (Appendix A's "Role Table Summary" in `architecture.md`) already confirmed
   this connection's `ompExtension` principal may call.
3. **The mutation** — `run_submit` builds a `Run { state: queued, ... }` and calls
   `DomainRepository::submit_run` inside a `run_domain_op` closure. `append_and_apply`
   (`domain/repository.rs`) inserts the event row, learns its `sequence` from the rowid, rewrites
   `event_json` with the bare `RuntimeEvent::RunEvent { kind: RunQueued, ... }`, inserts the `runs`
   projection row, and commits — one transaction, both writes or neither.
4. **The broadcast** — back in `run_submit`, `embed_envelope`/`take_envelope` carry the returned
   `Committed.envelope` across the `run_domain_op` boundary, and `self.broadcast(&mut result)`
   sends it on `Shared.events_tx` *before* the JSON-RPC response is built. Any connection currently
   in `spawn_subscription` receives it as an `events/event` notification
   in the same tick — this is the fix for the bug described in [`engineering-lessons.md`](engineering-lessons.md#durable-mutations-must-broadcast-the-same-event-they-just-committed).
5. **The adapter seam** — `run_submit` then calls the injected `RunDriver` (the `AdapterRegistry`
   by default): it resolves the worker profile, checks `AdapterAuthorization` (deny-by-default in
   production, configurable in tests), constructs the matching adapter (Claude/Codex/Copilot/OMP-RPC),
   and spawns a supervised process via `Supervisor`. With no driver, it returns `adapter_unavailable`
   *after* the queued run already committed in step 3 — the run is never silently dropped just
   because nothing can start it yet. With the registry wired (production,
   `run_lifecycle.rs:261-273`), the run now advances `queued -> starting -> working` as the
   adapter journals evidence, and terminalizes on process exit.
6. **The monitor observes it** — `monitor/controller.ts`'s `client.subscribe` callback (already
   running from `session_start`) receives the notification from step 4, `model.ts::reduceEvent`
   builds/updates the run's row from the `RunEvent` payload, and `refresh()` calls
   `ctx.ui.setWidget` — the row appears in `/crew` without the extension ever polling or
   reconnecting.

## 4. Debugging playbook

**See what the daemon thinks is happening.**

```bash
crewd status --wait-seconds 5 --state-dir <root> --repo <repo>   # JSON snapshot
tail -f <root>/repos/<repo-id>/runtime.log | jq .                  # structured log (detached mode)
```

Find `<repo-id>`: it's the only directory under `<root>/repos/` for that repo, or compute it —
first 32 hex chars of `sha256` of the canonical VCS root path.

**Run the daemon in the foreground while iterating.** `--foreground` puts the same structured
records on stderr and keeps the process attached to your terminal:

```bash
RUST_LOG=debug ./target/debug/crewd serve --foreground --state-dir /tmp/bs --repo "$PWD"
```

(`tracing-subscriber`'s env-filter is compiled in; `RUST_LOG` controls verbosity.)

**Inspect the journal.** It's plain SQLite:

```bash
sqlite3 <root>/repos/<repo-id>/runtime.db \
  'SELECT sequence, timestamp, event_json FROM events ORDER BY sequence;'
sqlite3 <root>/repos/<repo-id>/runtime.db \
  'SELECT operation_id, kind, acknowledged_at FROM operations;'
sqlite3 <root>/repos/<repo-id>/runtime.db \
  'SELECT run_id, state, flags_protocol_unhealthy FROM runs;'   # orchestration projections
```

If you ever see a raw secret in there, that is a P0 — the redaction boundary exists to make it
impossible.

**Talk raw JSON-RPC to the socket.** Useful for protocol debugging without the TS client:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":"1","method":"initialize","params":{...}}' \
  | nc -U <root>/repos/<repo-id>/runtime.sock
```

(Steal a valid `params` object from `fixtures/protocol/initialize.request.json` and fix
`repository`/`agentDirectory` for your machine.)

**Decode common exits and errors.**

| Signal | Meaning |
|---|---|
| exit 73 + `already_running` JSON | Lost the singleton flock race — a daemon already serves this repo |
| `NOT_INITIALIZED` (-32001) | You sent a method before `initialize` |
| `INCOMPATIBLE_VERSION` (-32002) | Version ranges don't overlap protocol 1.0 |
| `METHOD_NOT_FOUND` for a method you know exists | Your role's method table hides it — check `ClientPrincipal::allowed_methods`; if it's `ompExtension`-only, also check you didn't authenticate as `display` (see [`engineering-lessons.md`](engineering-lessons.md#cached-client-must-authenticate-with-the-union-of-all-roles)) |
| `ILLEGAL_TRANSITION` (-32100) | The requested `RunState` edge isn't in the canonical lifecycle relation (`domain/transitions.rs`) — check `RunState::can_transition_to` |
| `adapter_unavailable` from `run/submit` | Expected without a wired `RunDriver` (none by default this milestone) — the run is still `queued`, check `run/list`/`run/get` |
| `RATE_LIMITED` from `coordination/send`, `coordination/requestChild`, or `coordination/publishArtifact` | More than 30 journaling calls/minute from one sender — the three methods share one 30-calls-per-minute budget per sender (`coordination/rate_limit.rs`) |
| Connection dropped with no JSON error | Peer-UID mismatch (dropped before parsing) or an over-cap frame |
| `ValidationError` in the TS client | The daemon sent a frame the schema rejects — regenerate bindings or find the drift |

**Orphan hunting.** Tests and smoke runs are disciplined about cleanup, but if something leaks:
`pgrep -fl crewd`, then `crewd stop` (preferred) or `kill <pid>`. The kernel releases the flock
on death, so the next start recovers automatically.

**Run conformance tests.** To verify adapter implementations match their protocol specs:

```bash
# Run all conformance tests
cargo test --test conformance

# Run specific adapter conformance
cargo test --test claude_adapter
cargo test --test codex_adapter
cargo test --test copilot_adapter
cargo test --test omp_rpc_adapter
```

**Export audit events.** For offline analysis:

```bash
crewd audit export --state-dir <root> --repo <repo> --output /tmp/audit.jsonl
```

## 5. Testing guide

**Philosophy:** integration tests exercise real things — real processes, real sockets, real SQLite
files, byte-scans of real WAL files. Mocks appear only at injection seams that exist for the
purpose (peer-credential reader, worker-credential verifier, packaged-binary resolver, uid
provider).

**Where to add a test:**

| You changed… | Put the test in… |
|---|---|
| A wire type / serde shape | `crates/protocol/tests/wire_contract.rs` (+ regenerate, + fixture if cross-language) |
| Domain record shape, `RunState` lifecycle edges, `RunFlags` | `crates/protocol/tests/domain_contract.rs` |
| Coordination message kinds, delivery states, request/result shapes | `crates/protocol/tests/coordination_contract.rs` |
| Path/identity/permission logic | `crates/runtime/tests/paths.rs` (+ `fixtures/repo-id` or `fixtures/state` + the mirrored TS test) |
| DB actor commands or migrations | `crates/runtime/tests/database.rs` |
| Anything touching what gets persisted | `crates/runtime/tests/redaction_boundary.rs` — extend the byte-scan |
| Foundation protocol methods, negotiation, roles | `crates/runtime/tests/ipc.rs` |
| Locking, shutdown, idle, CLI | `crates/runtime/tests/lifecycle.rs` (real-process tests; keep timers ~1 s) |
| `DomainRepository` transactions, projection rollback, event rebuild | `crates/runtime/tests/domain_repository.rs` |
| `task/worker/run/message/approval/reconcile` RPC methods | `crates/runtime/tests/orchestration_rpc.rs` — remember the broadcast half (see [`engineering-lessons.md`](engineering-lessons.md#durable-mutations-must-broadcast-the-same-event-they-just-committed) and §3 above) |
| Coordination broker behavior (bounds, rate limits, scope tokens) | `crates/runtime/tests/coordination.rs` |
| Approval ownership, idempotency, callback, recovery | `crates/runtime/tests/approval.rs` |
| Adapter contract and registry | `crates/runtime/tests/adapter_contract.rs`, `adapter_registry.rs` |
| Claude/Codex/Copilot/OMP-RPC adapters | `crates/runtime/tests/{claude,codex,copilot,omp_rpc}_adapter.rs` |
| Supervisor (process management) | `crates/runtime/tests/supervisor.rs` |
| Workspace operations (lease, apply, materialize) | `crates/runtime/tests/{workspace_lease,workspace_apply,workspace_materialize}.rs` |
| Display backends (terminal, herdr, tmux) | `crates/runtime/tests/{terminal,herdr,tmux}_adapter.rs`, `display_registry.rs`, `display_selector.rs` |
| Configuration merging, rollout gates | `crates/runtime/tests/config.rs` |
| Conformance test scenarios | `crates/runtime/tests/conformance.rs` |
| Coordination MCP proxy | `crates/runtime/tests/coordination_mcp.rs` |
| Redaction rules | `crates/runtime/tests/redaction.rs` |
| Audit export/retention | `crates/runtime/tests/audit.rs` |
| Monitor CLI | `crates/runtime/tests/monitor_cli.rs` |
| TS client/launcher/extension logic | Sibling `*.test.ts` in `packages/extension/src/` |
| Orchestration tool registration/schema/dispatch | `packages/extension/src/tools/tools.test.ts` |
| OMP-native event mapping, coalescing, restart/`lost` | `packages/extension/src/omp-native/reconcile.test.ts` |
| Monitor event-reducer or rendering | `packages/extension/src/monitor/model.test.ts` / `render.test.ts` |

**Conventions that reviews enforce:** test the real serialized JSON shape (not just round-trips);
no sleeps papering over races (event-driven waits with a deadline); every spawned process is
reaped even on assertion failure; test output stays pristine (a stray warning is a finding);
follow TDD — the suite's failure message before implementation is part of the evidence.

**Fast loops:**

```bash
cargo test -p batman-runtime --test ipc -- --nocapture some_test_name   # one Rust test, with output
bun test packages/extension/src/client.test.ts -t "frame"              # TS tests matching a name
```

## 6. Gotchas

- `crates/protocol/bindings/` fills with `.ts` files when you run `cargo test` (ts-rs side
  effect). It is gitignored scratch; the real bindings are `packages/protocol-ts/src/generated/`.
- Type-checking is a real gate: `bun run typecheck` (`tsc --noEmit` against the root
  `tsconfig.json`) runs locally, inside `bun run check`, and as its own CI job. `skipLibCheck` is
  on solely for three unfixable third-party `.d.ts` errors (pi-catalog's `models.json` import,
  pi-coding-agent's `wrapper.d.ts` variance, pi-mnemopi's optional `fastembed` peer); first-party
  errors are never silenced.
- `crewd schema` prints the schema **embedded at compile time** (`include_str!`). After changing
  protocol types, `bun run generate` *and* rebuild the binary, or the printed schema lags the
  types. `generate --check` in CI catches the committed-file half of this.
- Unix socket paths are capped (~104 bytes on macOS). Deep `--state-dir` paths fail fast with an
  explicit error — use `/tmp/...` in tests.
- Lock files are never deleted; ownership is the flock, not file existence. Don't "clean up"
  `runtime.lock` in scripts — deleting it while a daemon runs is harmless to the daemon but makes
  the metadata unreadable to `status`/`stop`.
- **Never resolve a peer package's own metadata (`import ... "@pkg/name/package.json" with {
  type: "json" }`, or equivalent) at extension-load time or module scope.** It resolves fine under
  `bun test`/`bun run` in this repo but hangs the real `omp` binary loading the extension (its own
  bundled module graph, different resolution entirely), and can crash a multi-file `bun test` run
  with an unrelated Bun resolver defect. If you need a peer's installed version, read its
  `package.json` with a plain `fs` walk (see `monitor/compat.ts`). Full story:
  [`engineering-lessons.md`](engineering-lessons.md#never-use-with--type-json--imports-at-extension-load-time).
- **A cached client shared by multiple callers needs the union of every role they need, not
  whichever role the first caller happened to need.** `ensureRuntime`'s client is shared by every
  orchestration tool and the monitor; it authenticates as `ompExtension` for exactly this reason.
  Full story: [`engineering-lessons.md`](engineering-lessons.md#cached-client-must-authenticate-with-the-union-of-all-roles).
- **A `DomainRepository` mutation that doesn't broadcast its `Committed.envelope` breaks the
  monitor silently** — no error, no test failure, just a widget that never updates for that one
  mutation. See §3 above before adding one. Full story:
  [`engineering-lessons.md`](engineering-lessons.md#durable-mutations-must-broadcast-the-same-event-they-just-committed).
- **Recovery runs automatically after each `serve` command**, before the daemon starts serving, and
  sweeps *every* run the journal still calls non-terminal — there is no age threshold on it, because
  nothing can be live at boot. If you're debugging why a stuck run transitions to `failed`/
  `cancelled`, `crates/runtime/tests/recovery.rs` is the matrix; `recover_paused`/`recover_waiting`
  are the only knobs. There's no flag to trigger recovery on demand — use `doctor`'s `stale_runs`
  check (five-minute silence threshold, read-only) to see a wedged run without forcing a restart.
- **Rollout gates must all be `true` before production use.** The `Doctor::check()` runs on
  every `serve` and `status` command. If any gate is unresolved, the doctor reports it and
  the runtime refuses to serve in production mode. Check your config files (`~/.crew/config.yaml`,
  `<repo>/.crew/config.yaml`) for `rollout_gates` fields.
- **Adapter authorization is deny-by-default in production.** The `DenyByDefaultAuthorization`
  rejects every worker unless `dev_override` is explicitly set. Tests inject `FixtureAuthorization`
  to allow/deny as needed. Production callers must supply a real `PolicyEvaluator` (see the
  Hardening plan's `PolicyEvaluator`, which owns model/adapter allowlists and ceilings).
