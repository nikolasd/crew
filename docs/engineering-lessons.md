# Engineering Lessons

**Audience & purpose:** contributors debugging something that feels like it might have happened
before — a companion to [code-walkthrough.md](code-walkthrough.md)'s debugging playbook and the
developer manual ([development.md](development.md)). This document catalogs hard-won
lessons from smoke testing, production incidents, and debugging. These are the kind of things that
should be discovered by reading documentation, not by trial and error.

**Reference:** These lessons are cross-referenced by file/ADR, not by `architecture.md` section number — that document was later rewritten onto the C4 model and no longer has numbered `§N` sections.

---

## IPC and Client Management

### Cached client must authenticate with the union of all roles

**Location:** `packages/extension/src/runtime.ts::ensureRuntime` (see [ADR-0021](adr/0021-shared-client-authenticates-with-the-union-of-required-roles.md))

A cached client shared across callers with different role needs must authenticate with the *union* of every role its callers need, not whatever the first caller happened to need.

**The bug:** `ensureRuntime` originally hardcoded `ClientAuth::Display` (read-only) for what its own doc comment called "a launcher connection" — correct for `crew_health`, the only caller when it was written. `index.ts::getClient()` later cached and reused that exact client for every orchestration tool too, so every mutation (`task/upsert`, `run/submit`, ...) failed `-32601 method ... is not available to this client` — silently, since `display`'s method table is a strict *subset* of `ompExtension`'s and nothing checks that relationship at compile time.

**The fix:** `ensureRuntime` now authenticates as `ompExtension` unconditionally, safe for every existing caller because `ompExtension`'s allowed methods are always a superset.

**The lesson:** When one cached connection is shared across callers with different needs, its role must be the *union*, not whatever the first caller happened to need — and a role table's superset/subset relationships between roles are exactly the kind of fact that belongs in a comment next to the role definition, not just in reviewers' heads.

---

## Extension Loading and Module Resolution

### Never use `with { type: "json" }` imports at extension-load time

**Location:** `packages/extension/src/monitor/compat.ts`

A static `import ... with { type: "json" }` at module scope can hang the extension or corrupt `bun test`.

**The bug:** `compat.ts` originally did `import pkg from "@oh-my-pi/pi-coding-agent/package.json" with { type: "json" }` at module scope, called from `registerMonitor` at extension-load time. That import resolves fine under `bun run`/`bun test` in this repo's own `node_modules`, but **hangs forever** the instant the real `omp` binary (itself a compiled, bundled Bun executable) loads the extension file and tries to resolve that exact subpath from *its own* bundled module graph — confirmed by bisecting a minimal repro down to the bare import statement with no call.

**The fix:** The version check now lives only in a test (`render.test.ts`), matching the plan's own framing of it as a "no-model fixture", never called from production code; and the check itself now reads the peer's `package.json` via a plain filesystem walk from `import.meta.dir`, never through Bun's module resolver.

**The lesson:** Never run a `with { type: "json" }` import — or any dynamic resolution of a peer package's own metadata — at extension-load time or module scope in code the real `omp` binary will load; if you need a peer's installed version, read the file directly.

---

## Persistence and Event Broadcasting

### Durable mutations must broadcast the same event they just committed

**Location:** `crates/runtime/src/domain/repository.rs::append_and_apply`, `crates/runtime/src/ipc/connection.rs::replay` (see [ADR-0020](adr/0020-per-mutation-event-broadcast-is-not-optional.md))

A durable mutation must broadcast the same event it just committed, in the same call.

**The bugs (two separate issues):**

1. **Full vs. bare envelope:** `DomainRepository::append_and_apply` stored the *full* `EventEnvelope` (with `sequence`, `timestamp`, ...) into `event_json`, but `ipc/connection.rs::replay()` expects that column to hold only the bare `RuntimeEvent` — it reconstructs the envelope from the `events` table's own `sequence`/`timestamp`/`project_id`/`run_id` columns. Every `events/replay` call therefore failed to deserialize once any mutation had committed.

2. **No publisher for broadcast channel:** `Shared.events_tx` (the `tokio::sync::broadcast` channel `ipc/connection.rs::spawn_subscription` reads from) had a subscriber but **no publisher anywhere** — none of the 15+ mutation call sites across `OrchestrationService`, `ApprovalService`, `CoordinationBroker`, and `RunDriverContext` ever called `.send()` on it. A monitor connected before a mutation committed would never observe it without reconnecting (which re-triggers `events/replay` — itself broken by the first bug).

**The fix:** Storage now writes the bare event; `Committed` now carries the full `EventEnvelope`; and `domain::{embed_envelope, take_envelope}` smuggle it across the `run_domain_op` closure boundary (whose closures are constrained to return a plain `serde_json::Value`) so every service broadcasts after every commit.

**The lesson:** This is now invariant #7 in the README, and it is not enforced by the type system — the compiler cannot catch "this new mutation appended an event but forgot to broadcast it". Any new `DomainRepository` mutation method **must** be wired through `embed_envelope`/`take_envelope`/`self.broadcast(&mut result)` at its call site, matching every existing sibling in `service/orchestration.rs`, or the monitor will silently show stale state for that mutation alone.

**Regression tests:** `crates/runtime/tests/orchestration_rpc.rs`'s `events_replay_round_trips_committed_mutation_events` and `events_subscribe_delivers_live_notifications_for_orchestration_mutations` are the regression tests for both halves; the latter reproduced the bug as an infinite hang, not a clean failure — run it with a test-runner timeout if you ever suspect a new mutation has regressed this.

---

## Run Lifecycle

### A state edge driven by evidence must identify the cause, not merely correlate with it

**Location:** `crates/runtime/src/adapter/run_lifecycle.rs`'s `observe_vendor_activity` (CREW-47)

**The bug:** a run parked at `waitingUser` by a finished turn was resumed to `working` by *any*
journaled non-exit event, on the premise that "the vendor produced something, so the leader must have
steered it". The premise is false: the vendor also writes post-turn bookkeeping. In the failing
session the resume was triggered by Claude's own `system` entry of subtype `turn_duration` — the
record of how long the turn took — and by `bridge-session`, `cost-state` and `last-prompt` entries,
each of which carries a `sessionId` and therefore produced a `SessionMeta` event, i.e. **the runtime
re-identifying its own session file resumed the run**. Settle, reverse, settle, reverse, then a
five-minute inactivity timeout. The run row tracked every edge faithfully; the state was faithfully
wrong.

The instructive part is the latch. `observe_turn_ended` deliberately reopened the
`working_observed` latch so the next event would be re-evaluated — added to fix the opposite bug, a
real follow-up turn stranding the run in `waitingUser` forever. Both failures are the same ambiguity
resolved in opposite directions: leave the latch set and genuine steering is ignored; reopen it and
bookkeeping resurrects the run. No latch tuning fixes either, because the signal cannot tell them
apart.

**The lesson:** when a state edge is driven by journaled evidence, the evidence has to identify the
*cause*, not co-occur with it. If two different causes produce identical observations, no threshold,
latch or ordering change will separate them — the fix is either a signal that distinguishes them
(here: a real user-turn entry, non-sidechain and not a `tool_result`) or an edge caused explicitly by
the actor that already knows (here: the service that delivered the follow-up clears the flag itself,
because it does not need to deduce what it just did). Reach for "caused" before "inferred": the
runtime usually knows something it is otherwise trying to detect.

**Regression tests:** `a_trailing_session_meta_entry_never_resumes_a_settled_run`,
`a_delivered_follow_up_resumes_a_settled_run`,
`a_real_user_turn_resumes_a_settled_run_through_the_production_sink_stack`.

### A documented state machine with no production writer is inert

**Location:** `crates/runtime/src/adapter/registry.rs`, `crates/runtime/src/adapter/run_lifecycle.rs` (see [ADR-0023](adr/0023-run-state-edges-from-adapter-evidence.md))

ADR-0012 defined the run-lifecycle relation and ADR-0013 shipped `FakeRunDriver` as its only
implementer. The real `AdapterRegistry` — the production `RunDriver` — never called
`transition_run` anywhere in `run_one` or `watch_settlement`; grepping `crates/runtime/src/adapter/`
for `transition_run` returned zero hits. Every real run's row stayed `queued` however successfully
its vendor process ran and exited; `run/get`, `run/list`, the `/crew` monitor, and the approval
flow all read a value that was wrong for every real run, and only a daemon restart
(`RecoveryCoordinator`) ever terminalized anything.

**The lesson:** A state machine whose only exerciser is a test fake reads as implemented in
review — the `FakeRunDriver`-only `queued -> starting -> working` sequence looked like coverage of
a real path, because the relation itself (`RunState::can_transition_to`) was thoroughly tested and
the fake drove it end to end. It wasn't: the fake is never wired into a live `omp` session. Grep
for production call sites of the transition function itself, not for the transition table or its
unit tests — a well-tested relation with zero production callers is exactly as broken as an
untested one.

**The fix:** `RunLifecycleSink` wraps each run's `AdapterEventSink` and applies the evidence table
(`ProcessStarted` -> `starting`, first non-exit payload -> `working`, `ProcessExited` ->
`succeeded`/`failed`/`lost`) after the inner sink journals each event, walking every intermediate
hop the legal-edge table forces and never overwriting a terminal state.

**Regression tests:** `crates/runtime/src/adapter/run_lifecycle.rs`'s 9 unit tests
(`process_started_moves_a_queued_run_to_starting` through
`vendor_output_never_reopens_working_on_a_run_that_started_waiting`) are unaffected. The
end-to-end proofs against real processes named here at the time this lesson was written --
`crates/runtime/tests/run_lifecycle.rs`'s `a_real_worker_process_walks_its_run_from_queued_into_working`
and `a_real_worker_process_exit_settles_its_run`, `crates/runtime/src/adapter/claude/mod.rs`'s
`run_state_tests` module, and `crates/runtime/tests/copilot_adapter.rs`'s
`a_supervised_process_exit_is_reported_with_its_real_status` -- all drove a real process through
the *headless* control plane, which crew-v2 gap-closure WP-C retired. `claude/mod.rs` and
`copilot_adapter.rs` are deleted outright; `run_lifecycle.rs`'s two proofs kept their coverage by
switching from the deleted `OmpRpcAdapter` to `support::spawn_evidence_adapter::SpawnEvidenceAdapter`
(`tests/support/spawn_evidence_adapter.rs`) -- a small, protocol-agnostic, test-only `Adapter` that
spawns a real OS process and forwards only its spawn/exit evidence, built specifically to keep this
lesson's "a real process, not just a test-fake" property true once the headless control plane it
had borrowed for that purpose was gone. This lesson's regression coverage is intact.

---

## Workspace Leases and Resource Cleanup

### A resource acquired before a fallible step must be released on every path out of it

**Location:** `crates/runtime/src/service/orchestration.rs::start_queued_run`, `::workspace_acquire`; `crates/runtime/src/workspace/lease.rs::stale`

Two-phase acquisition (claim first, then do the fallible work that finishes the claim) makes the
in-between state invisible to any check written only against the *finished* state's shape.

**The bugs (two distinct triggers of one defect, closed together):** `start_queued_run` and
`workspace_acquire` each acquire a workspace lease (an `allocating`-state row), then materialize a
worktree or copy, then activate the lease with the real path, and — for `start_queued_run` only —
start the adapter. Nothing released the lease on a failure in any of the fallible steps after
`acquire`. `materialize()` failing left an `allocating` row nothing would ever touch again; a
`driver.start` failure left an `active` lease and a real worktree with no owner. `run/retry`
re-runs the whole sequence for a new `RunId`, so a driver that reliably failed to start leaked one
row per retry. The `materialize()`-failure case was worse than the `driver.start` case: the only
check meant to catch it, `LeaseService::stale()`, filtered on "a non-empty path that no longer
exists" — a signal an `allocating` row can never produce, since its path is empty by construction
until `activate()` runs. The doctor check written for exactly this residue was structurally blind to
it, not merely untested against it.

**The fix:** `abandon_lease`/`abandon_and_announce` helpers now run on every fallible step past
`acquire()` in both functions, mirroring `workspace_release`'s existing release-then-teardown-then-
`cleanupFailed` ordering rather than inventing a second convention. Teardown is deliberately
best-effort: propagating a `git worktree remove` failure (expected when the worktree was never
created) would replace the caller's real error with an unrelated cleanup artifact. `stale()` was
widened to also flag any row still `allocating` past `ALLOCATING_LEASE_GRACE` (ten minutes)
regardless of path emptiness, so a lease abandoned before materialization even started is no longer
invisible to the doctor.

**The lesson:** A doctor-style health check keyed only on the *finished* shape of a resource (here,
"a real path that vanished") cannot see a failure that happens before that shape ever exists. When a
resource is claimed in more than one commit, write the check against the intermediate state
directly — an age threshold on the claim itself, here — not only against the terminal one.

**Regression tests:** `crates/runtime/tests/orchestration_rpc.rs`'s
`start_queued_run_releases_the_lease_when_materialize_fails`,
`workspace_acquire_releases_the_lease_when_materialize_fails`, and
`start_queued_run_releases_the_lease_and_worktree_when_driver_start_fails`;
`crates/runtime/tests/workspace_lease.rs`'s
`stale_never_flags_an_allocating_lease_within_the_grace_period` and
`stale_flags_an_allocating_lease_that_outlived_the_grace_period`;
`crates/runtime/tests/doctor.rs`'s
`stale_workspaces_fails_when_an_allocating_lease_outlives_the_grace_period`.

---

## Security and Redaction

### A redaction denylist is only as good as the shapes it was actually tested against

**Location:** `crates/runtime/src/security/redaction.rs::Redactor::new` (see [ADR-0006](adr/0006-type-enforced-redaction-boundary.md))

A pattern that looks like it covers a vendor's API keys is worthless if it was written against a
remembered key format rather than the one that vendor issues.

**The bug:** the built-in `api_key` rule was `sk-[A-Za-z0-9]{16,}` — a plausible-looking `sk-`
pattern that matched none of the keys the vendors this codebase drives actually issue. Anthropic's
`sk-ant-api03-…` and OpenAI's `sk-proj-…` both put hyphens (and base64url underscores) inside the
token, immediately after the three characters the pattern accepted. Every unit test asserting the
rule worked used a hand-written `sk-ABCDEFGHIJKLMNOPQRSTUVWX` literal that shared the pattern's
assumption, so the whole test suite agreed with the bug. Classification (`Secret`/`Thinking`
fragments) is the primary boundary and was unaffected — but the denylist exists precisely for the
case where a vendor narrates a key back inside `Visible` text, which is exactly what it could not
catch.

**The fix:** `(^|[^A-Za-z0-9_-])sk-[A-Za-z0-9_-]{16,}`, with tests written from the vendors'
documented key shapes rather than from the pattern. Constraining what may *precede* the token is
load-bearing in the other direction: the widened character class accepts `-`, so an unconstrained
version swallows ordinary hyphenated prose — `disk-space-check-failed` contains a legal
`sk-space-check-failed`. The first attempt used a leading `\b`, which is **not** sufficient: `-` is
a non-word character, so `\b` still admits `pre-sk-space-check-failed`. The preceding character has
to be matched and constrained to something outside the token alphabet, then re-emitted through the
rule's `${1}` replacement so the surrounding text is not eaten with the secret.

**The lesson:** when a redaction/denylist pattern is added or widened, the test input must come from
the real producer's format, never from the pattern's own shape — and every widening needs a
paired negative test proving normal text is still untouched, because over-redaction of diagnostics
is a silent failure too. `\b` in particular is a trap when the token alphabet contains `-` or `_`:
it asserts a *word* boundary, which those characters do not create.

**Regression tests:** `anthropic_shaped_api_key_is_redacted`,
`openai_project_shaped_api_key_is_redacted`,
`hyphenated_prose_is_not_mistaken_for_an_api_key`,
`hyphen_delimited_prose_is_not_mistaken_for_an_api_key`,
`two_adjacent_api_keys_are_both_redacted`,
`sanitize_json_redacts_an_anthropic_shaped_key_at_any_depth` (all in `security/redaction.rs`), plus
`crates/runtime/tests/redaction_boundary.rs`, which carries an Anthropic-shaped key through the real
append path and byte-scans the database, WAL, log, and replay output for it.

---

## Coordination Bounds

### A bound enforced at one call site is not an enforced policy

**Location:** `crates/runtime/src/coordination/broker.rs::{send, request_child, publish_artifact}`

A doc comment that asserts a broker-wide invariant is only as load-bearing as the single inline
check it was written beside — and a second, stricter enforcement layer can hide its absence from
every surface a user actually exercises.

**The bug:** the byte bound and the per-sender rate limit lived inline in `send()` while the
struct's own doc comment described them as properties of the whole broker. The two methods added
later — `request_child()` and `publish_artifact()` — inherited the claim without the code: the
direct JSON-RPC path had no size bound at all (the server's default 4 MiB frame cap was the only
bound in sight), and `publish_artifact`'s journaled message could be looped without throttling of
any kind. The MCP tool surface had its own stricter argument bounds, so every test that drove
through that layer saw a broker that *looked* bounded; the gap was invisible from the tool
surface.

**The fix:** `reject_oversized()` and `charge_rate_limit()` as named helpers on
`CoordinationBroker` that every journaling method calls — the byte bound on each worker-supplied
string that can become durable content, and the per-sender rate-limit charge as soon as the
sender's identity is resolved. The rate-limit key is the run's own `worker_id` row read through
`run_participants()`, never a caller-supplied parameter, so a single shared window covers `send`,
`requestChild`, and `publishArtifact` alike and a worker cannot evade it by rotating between
methods. Quarantine keeps its position ahead of the rate-limit charge so a quarantined worker
still sees `POLICY_QUARANTINED`, not `RATE_LIMITED`.

**The lesson:** when a doc comment on a type asserts an invariant, the enforcement belongs in a
named helper the type's methods must call, not inline in the first method that needed it — and a
second enforcement layer (here `mcp_protocol`'s stricter argument bounds) can hide the absence of
the first from every test that only drives the outer layer. Test the innermost layer directly.

**Regression tests:** `crates/runtime/tests/coordination.rs`'s
`coordination_request_child_rejects_a_reason_over_64_kib`,
`coordination_publish_artifact_rejects_free_text_over_64_kib`,
`coordination_publish_artifact_accepts_a_description_at_the_limit`,
`coordination_publish_artifact_draws_on_the_same_per_sender_budget_as_send`, and
`coordination_request_child_draws_on_the_same_per_sender_budget_as_send`.

## Domain Writes and Concurrency

### A check in one database round trip cannot guard a write in the next

**Location:** `crates/runtime/src/domain/repository.rs::resolve_policy_violation`;
`crates/runtime/src/policy/violation.rs::ViolationService::decide`

**The bug:** `decide` read a single snapshot in one `run_domain_op` round trip, evaluated
ownership, conflict, and terminal-run checks against that one in-memory snapshot, then wrote
unconditionally in a second, separate round trip. The database actor
(`crates/runtime/src/db/actor.rs`) is a single `std::thread` processing one whole boxed closure at
a time off a bounded channel -- it serializes closures, never a caller's sequence of decisions
about them. Two concurrent `decide` calls for the same violation could both read
`resolution: None` from their own snapshot round trip before either reached the write, and the
write itself carried no guard (`UPDATE ... WHERE violation_id = ?4`, no `resolution IS NULL`), so
both committed and both fired contradictory side effects -- one clearing quarantine, the other
cancelling the same run.

**The fix:** move the guard into the same transaction as the write. `resolve_policy_violation`'s
`apply` closure now runs the `UPDATE` with a `WHERE resolution IS NULL` guard, checks the
affected-row count, and -- for `"release"` -- reads the run's state from the same transaction
before deciding whether it has settled. A refused write returns `Err`, which discards the whole
transaction: the appended event and the rejected write both vanish together, at no cost in
sequence numbers (`events.sequence` is a plain `INTEGER PRIMARY KEY`, not `AUTOINCREMENT`).
`ViolationService::decide` still makes the same two round trips -- a snapshot, then a write --
but ownership is the only check left against the snapshot's in-memory result; the conflict and
terminal-run checks that used to run against that snapshot, and could go stale before the write,
now run inside the write's own transaction instead. `PolicyViolationSnapshot` shrank with them,
since resolution/run-state gating no longer needs to leave the guarded transaction.

**The lesson:** with a single-threaded database actor that serializes whole closures, a
multi-round-trip service method is not a transaction, no matter how sequential it reads. A guard
belongs in the same closure as the write it protects, and its verdict -- not an earlier read --
must be the caller's only source of truth about whether the write happened.

**Regression tests:** `crates/runtime/tests/policy_violation.rs`'s
`concurrent_release_and_cancel_admit_exactly_one_decision`,
`concurrent_identical_releases_journal_one_event_and_report_already_decided`,
`deciding_the_same_resolution_twice_sequentially_stays_idempotent`, and
`releasing_a_violation_whose_run_settles_mid_decide_is_refused`.

### An entire-struct write built on a stale read silently discards a concurrent field update
**Location:** `crates/runtime/src/domain/repository.rs::set_run_flag`, `write_run_flags`

`set_run_flags` read the whole `RunFlags` struct, handed it to a callback, then wrote the whole
struct back. Any flag a *different* caller set while the callback ran was inside the snapshot's blind
spot and was overwritten on write-back — no error, no conflict, just a lost update. Read-modify-write
of an aggregate is only safe when the read and the write are the same transaction; otherwise the
granularity of the write has to match the granularity of the intent. Fix: mutate one named flag at a
time, reading current state inside the database actor's own closure. The lesson generalizes past
flags — any "load object, mutate in memory, save object" against shared state has this shape.

### A root-cause lesson attaches to a pattern, not to the service the fix landed in

**Location:** `crates/runtime/src/domain/repository.rs::decide_approval`;
`crates/runtime/src/approval/service.rs::ApprovalService::decide`

**The bug:** the same check-then-act race as [a check in one database round trip cannot guard a
write in the next](#a-check-in-one-database-round-trip-cannot-guard-a-write-in-the-next), in the
approval service. `decide` read one snapshot, checked it in memory for a conflicting decision and
a settled run, then wrote unconditionally in a second round trip -- `decide_approval`'s `UPDATE
approvals SET ... WHERE approval_id = ?4` carried no `decision IS NULL` guard, so two concurrent
`approval/decide` calls for the same approval could both read `decision: None`, both write, both
journal an `approvalDecided` event, and both invoke the adapter callback: one telling the waiting
worker to proceed, the other to stand down. `decide_approval` has carried this shape since
approvals first landed (Part II); the two `decide` methods had simply drifted into structurally
identical code with identical exposure.

**The fix:** identical in shape to the policy-violation fix -- the guard lives in
`decide_approval`'s `append_and_apply` closure (`WHERE decision IS NULL`, affected-row check, the
existing decision read back in the same transaction to separate "already decided" from "never
existed", the terminal-run check in the same transaction ordered behind the `UPDATE`), a refused
write rolls back the appended event with it, and the service matches on the guard's verdict
instead of re-deriving it from the stale snapshot. `ApprovalSnapshot` no longer carries `decision`
or `run_state` -- the two fields the guard now owns -- but still carries `run_id` (the pending
`working`-transition target) and `run_flags` (read on a failed callback to set
`protocolUnhealthy`). Ownership and `humanRequired` remain read fields no *decision* write
mutates, so a losing racer's decision cannot invalidate either pre-check; ownership itself can
still change between the snapshot read and the guarded write through the unrelated reconcile
path's task-ownership rebind, an interleaving R70 does not touch. Part XIX's
`AlreadyResolved`/`RunSettled` variants, already generalized with `kind`/`id`/`existing` fields,
are reused as-is.

**The lesson:** the technical rule is the previous entry's. The organizational one is new, and it
is why this entry exists separately: a fix that closes a root cause in one service is not done
until every sibling implementing the same shape is swept for it. `decide_approval`'s identical
exposure outlived the policy-violation fix until that fix's own adversarial review swept the
sibling and registered R70 -- the second instance was found by the first fix's review, not by the
pattern.

**Regression tests:** `crates/runtime/tests/approval_decide_race.rs` --
`concurrent_approve_and_deny_admit_exactly_one_decision`,
`concurrent_identical_approvals_journal_one_event_and_invoke_the_callback_once`,
`deciding_the_same_decision_twice_sequentially_stays_idempotent`, and
`deciding_an_approval_whose_run_has_already_settled_is_refused` -- the approval-side mirror of the
four `policy_violation.rs` tests named in the previous entry.

---

## Recovery and Startup Sweeps

### A startup sweep and a periodic sweep have different risk models
**Location:** `crates/runtime/src/recovery.rs`

The crash-recovery sweep reused an age threshold (`stuck_threshold`) borrowed from the periodic
sweep, so any run younger than that threshold went unrecovered after a daemon crash — a blind spot
exactly where a crash is most likely to have just happened. The threshold was never an age heuristic;
it was a guard against false positives in a *running* daemon, where a young run is probably alive.
A daemon that has just booted provably has no live workers of its own, so the same guard becomes a
bug. When reusing a predicate across two callers, check what the predicate was protecting against,
not just what it computes.

## Determinism and Content Addressing

### A dependency feature enabled for one tool silently redefines every hash in the workspace

**Location:** `Cargo.toml`; `crates/runtime/src/canonical_json.rs`;
`crates/runtime/src/security/redaction.rs`; `crates/runtime/src/adapter/profile.rs`; and
`crates/runtime/src/config/merge.rs` (since orphaned — the same sorting now lives in
`config/crew.rs::fingerprint`)

**The bug:** `serde_json`'s `preserve_order` feature was correctly enabled for the conformance
fixture-capture scrubber, which must reproduce vendor frames in their original key order. It also
made every `serde_json::Map` insertion-ordered. Three unrelated boundaries then treated
document-order-dependent bytes as content: `Redactor::sanitize_json` persisted operation payloads,
`WorkerProfile::fingerprint` named registered profiles, and `RuntimePolicy::compute_fingerprint`
named merged policies. Two comments incorrectly claimed the workspace did not enable
`preserve_order`, while `config.rs::fingerprint_is_deterministic` only compared two equal merge
errors from locked fixtures; it never computed a fingerprint.

**The lesson:** Enforce determinism at the boundary that requires it, rather than relying on a
dependency default that another subsystem may legitimately change. `canonical_json` delegates
owned trees to serde_json's first-party `Value::sort_all_objects` and retains a borrowed cloning
wrapper for comparisons. `sort_all_objects` landed in serde_json 1.0.129, which `Cargo.toml` now
declares as the exact source minimum. `sanitize_json` and `WorkerProfile::fingerprint` sort their
owned trees in place; the fingerprint's canonical `Value` is a throwaway serialization, not a
mutation of the stored profile. Policy hashing and the raw side of the permission-envelope
comparison use the wrapper. Canonicalizing both comparison sides ensures that a difference means
redaction changed content, not that one side happened to arrive in another key order. The profile's
final sort is defense in depth: the current struct field order and its already-canonical sanitized
`permissionEnvelope` make it redundant today, but a future free-form field must not silently
weaken the fingerprint.

**Regression tests:** The redaction test varies top-level, nested, and array-object key order and
asserts byte-identical sanitized JSON. Its collision test varies the insertion order of two source
keys that redact to one key and confirms that source-key sorting makes the lexicographically
greatest source key win. The config and adapter-contract tests vary YAML and permission-envelope
key order before asserting equal fingerprints, and an unsorted benign envelope remains valid.
These tests vary the order each contract claims to ignore instead of repeating one construction and
calling the result deterministic.

---

## Verification Discipline

### A promise about behavior is only tested by running it
**Location:** `crates/runtime/src/conformance/`, `crates/xtask` capture tooling

Two instances, one shape. Conformance "fixture mode" promised zero model calls and still spawned
vendor binaries on probe scenarios, because the suite asserted the promise by construction rather
than observing it — the test and the bug shared an assumption, so the test could never fail on it.
Separately, the fixture-capture tool reported "changed / unchanged" by reading back the file it had
just written, grading its own homework; it always reported unchanged. In both cases reading the code
would have confirmed the promise and running it would not. Assert on observed behavior — a spawn
count, the previously-committed bytes — never on the code path you believe you took.

A third instance, same shape (CREW-50): `crew_transcript` called `events/replay` through the client's
generic `request()` path, whose fallback for a method with no registered validator demands a JSON
*object* — and `events/replay` returns a bare array, so every real call failed validation. The tool's
own test suite could not see it, because `leader.test.ts` fakes `client.request` itself: the test
mocked the very component whose behaviour was wrong. **A test that mocks the class under test cannot
observe that class's bug.** The fix's own test calls a real client over a real socket, and the
positive counterpart is worth naming too — the class was then closed by enumerating every method's
return shape (`worker_list_op` -> `{"workers": ..}`, and zero `Ok(json!([..]))` or `Ok(Value::Null)`
anywhere in the service layer), which established that `events/replay` was the only non-object result
rather than the only one anyone had noticed.

### A type checker is a gate only if it is run to failure
**Location:** `bun run typecheck`, CI `typecheck` job

`tsc --noEmit` was assumed green because the code "looked fine" and other checks passed. It is only a
gate once something has watched it fail and then pass. The same standard applies to any new test:
edit the production code to break it and confirm the test notices, or the test is decoration. The
cheapest version of this discipline is to write the assertion first and watch it fail for the right
reason.

### A payload crosses several boundaries, and clearing the one you are thinking about tells you nothing about the others

**Location:** `.github/workflows/auto-commit-dist.yml` (CREW-26)

The auto-commit workflow passed the ~484 KB bundle to `jq` as `--arg distContent "$DIST_B64"`. Two
people reviewed the size question and both checked it against the *API's* limit — the base64
inflation to ~645 KB, compared against the GraphQL mutation's documented ceiling, orders of magnitude
clear. It died on the first live run with `Argument list too long`: Linux caps a single `exec`
argument at 128 KB (`MAX_ARG_STRLEN`), and no amount of headroom against the API mattered because
the bytes never reached the API.

**The lesson:** a payload crosses several boundaries on its way somewhere — argv, environment, pipe
buffer, request body, column width — and each has its own limit. Clearing the boundary you are
thinking about is not evidence about the others, and the one that bites is usually the boring one
nobody was thinking about. When a size looks safe, ask *what carries it there* and check every hop,
not the destination. The fix was `--rawfile`, which bypasses argv entirely.

### A citation establishes that a mechanism exists, not that your use of it is safe

**Location:** CREW-26's design iterations

That same workflow's design cited GitHub documentation for every claim it made, and the two blocking
defects that survived to late review — `GITHUB_TOKEN` not retriggering workflow runs, and a
`contents:write` credential sitting in a job that executes PR-authored build scripts — are both
things the docs state plainly. Citing a mechanism is not the same as checking that your particular
use of it holds. Reading the twenty lines of the existing `bundle-check` job would have surfaced the
credential exposure immediately, because `bun install` and `bun run build` are right there in it.

**The lesson:** a citation answers "does this mechanism work the way I said?" and never "is what I am
doing with it correct?" Prefer reading the neighbouring code that already does the same thing.
Sibling of the entry above: same failure, one layer down.

### Finding an instance's call site is not the same as enumerating the write's call sites

**Location:** `crates/runtime/src/service/orchestration.rs::message_send`,
`crates/runtime/src/coordination/broker.rs` (CREW-28, then CREW-33)

CREW-28 closed an unredacted message payload by adding redaction at `message/send`, and described
`INSERT INTO messages` as having a single entry point. It has two: `send_internal`, reached from the
coordination broker, is the other, and the broker held no redactor at all — so `coordination/askPolicy`
and `coordination/reportBlocked` kept writing worker-supplied text unredacted after the "fix".

The measurement that produced that false confidence was `grep -c "Redactor\|sanitize\|Classified"`
over *one file*, which had been correct twice before. It answers "does this file redact?" and reads
as though it answered "is this write redacted?"

**The lesson:** when closing a leak, enumerate the callers of the **write**, not of the symptom you
found. `grep -rn "INSERT INTO <table>"` and then the callers of whatever wraps it; the same for the
event-append path. A fix scoped to the call site you happened to find leaves every sibling door open,
and the next person will reasonably read the fix as having closed the class.

**Postscript — the same author, one layer over, three commits later.** CREW-33 fixed the broker by
enumerating its *routes*: `requestChild`, `askPolicy`, `reportBlocked`. It missed
`coordination/publishArtifact`, which builds its `RunMessage` **directly** rather than funnelling
through `send`, so it reached `messages.payload` unredacted through a door none of those three
routes pass. CREW-34 found it only because the compiler demanded a claim once the field was typed.

The right enumeration was never the routes; it was the constructions — `grep -rn "RunMessage {"`.
That command had already been run, during CREW-34's own scoping, and its output was read as a
*count* ("11 sites, bounded, fine") rather than as a list to classify one by one. So the lesson
above is correct and was not enough on its own: **enumerate the constructions of the durable value,
not the entry points that reach them**, and when a grep answers with a number, the number is not the
finding — the list is.

That this recurred for the person who wrote the entry, while writing the entry, is the strongest
argument for the type-level fix that followed (`Redacted`, described in
[ADR-0028](adr/0028-submit-prompt-is-journaled-redacted-run-intent.md)'s closing section): a lesson
has to be remembered at the moment it applies, and a compile error does not.

### A measurement can be wrong in a way that looks like an answer

**Locations:** `git merge-base --is-ancestor` under squash-merge; `git diff A B` versus a revert;
`gh pr checks` field parsing; `grep -c` over a single file

The recurring failure across the CREW-27..34 audit was not code that was wrong. It was *measurements
that looked like proof*. Four worth naming, because each returned a plausible answer to a question
slightly different from the one asked:

- **`git merge-base --is-ancestor <branch> origin/main` says UNMERGED for every merged branch**, because
  this repo squash-merges: the squash commit is new, so an original tip is never an ancestor. It
  answers "is this branch's *history* in main" when the question is "is this branch's *work* in main".
  Under squash-merge those differ for every branch, always. Use `gh pr list --head <branch> --state all`,
  or grep main for the content.
- **`git diff origin/main <branch>` on a branch that is merely behind looks exactly like a revert.**
  A hunk showing `-#[ignore = ...]` for a just-merged fix reads as "this PR undoes it"; it was the
  two-endpoint diff showing main's own newer commit. `git log main..branch -- <file>` distinguishes
  them: no commits touching the file means the three-way merge will keep main's version.
- **`gh pr checks` parsed by whitespace field misreads every matrix job.** `build (macos-latest,
  x86_64-apple-darwin, darwin-x64)` puts the *check name's* second word where the status is expected,
  so passing jobs read as blank and were reported "still running" for several updates. `--json
  name,state` is the fix; the tell was `mergeStateStatus: CLEAN` contradicting "pending", explained
  away twice before being believed.
- **`grep -c "Redactor\|sanitize" <one file>` answers "does this file redact", not "is this write
  redacted".** It was correct twice, which is what made it trusted the third time — when the write had
  a second entry point in a file nobody had grepped.
- **A JSON walker that reads `enum[0]` reports a 23-value branch as one value.** Collecting a schema's
  wire values with `(branch.get("enum") or [None])[0]` read `RuntimeEventKind`'s 40 values as 18, and
  the missing 22 included the exact name under investigation — reported as a dangling reference when it
  was live. Union every `const` and every `enum` element recursively, and never per-branch.
- **A membership check on a *type* cannot answer a question about a *diff*.** Verifying that
  `Classified` was untouched by testing whether the type is a `$defs` key never asked whether the diff
  touched *mentions* of the name, so it could not distinguish the shipped description (converted,
  correctly) from the `Debug`-impl mention (kept, correctly) — and confidently reported the opposite of
  the artifact. Check the diff, not the inventory.

**The lesson:** the first measurement you reach for is usually the one that cannot distinguish the two
cases you care about. A local rebuild cannot tell "the bundle is stale" from "I am on the wrong
platform"; both produce a large diff. Before trusting a check, ask what *else* would produce this
same output. And when a cheap check contradicts an expensive one, the cheap one is the suspect — the
contradiction is the signal, not the noise to be explained away.

### An instrument you do not read against your own conclusion is not a check

**Locations:** an `Option`-field/`skip_serializing_if` audit script (CREW-46/47 review); a `grep` for
test functions in `crates/runtime/tests/run_result.rs` (CREW-49 rider review)

**The bug:** two failures of the same shape, neither of them a bug in the instrument.

A scan built to find shipped descriptions that say "absent" while the field serializes as `null`
printed `BAD event.rs reason` among seven flags. That output was then quoted to a colleague as
evidence of the scan's quality — "discriminating, not noisy, it correctly separates the two
same-named `reason` fields" — while the same review's hand-written keep-list said one of those two
fields needed no fix. The tool was right, its output was on screen, and it was never diffed against
the conclusion it contradicted. The keep-list shipped and a colleague caught it two reviews later.

Separately, a review claimed a property had no regression test. The grep behind that claim returned
only helper functions from a file of twenty tests, because the pattern matched `fn` at the wrong
indentation. **Zero tests in a test file is an impossible result** — and it was accepted as an answer
instead of as a broken instrument. The reviewer fell back to reading the PR diff, saw one new test,
and asked for a test that had existed on `main` for weeks; the engineer added a duplicate in good
faith.

**The lesson:** these are distinct from a measurement that answers the wrong question (see the entry
above), and they need a different remedy — not a better instrument, but reading the one you have.
Two habits close them. **Diff the output against what you already wrote down:** if a tool you built
to check your work disagrees with your conclusion, that is the entire value of having built it, and
quoting it approvingly while contradicting it is the opposite of using it. **Treat an
impossible-shaped result as a failure, not an answer:** no tests in a test file, no matches in a file
you know contains the string, an empty list where the domain guarantees at least one — each means the
instrument broke, and a fallback reached for at that moment inherits none of its authority.

**Regression tests:** N/A — process lessons. The concrete residue is the duplicate test dropped from
the CREW-49 rider before merge, and `an_empty_content_user_entry_is_not_a_real_user_turn`, which
exists because the same review's other findings were read properly.

### Thresholds calibrated on an idle machine are not thresholds

**Locations:** `pane_reopen_refuses_a_stale_socket_file_with_no_listener`,
`is_live_has_no_false_positives_under_fork_and_cpu_load`,
`a_multi_line_prompt_reaches_the_pty_framed_as_one_intact_paste`, the conformance kill-switch fixture test

Four tests in this suite fail under CPU contention and pass alone. Individually each reads as a flake;
together they are one mistake made four times — a timing threshold chosen while the machine was quiet.

- The pane-reopen test's connect probe raced a `fork()`ed child holding an inherited fd (CREW-30) —
  proven at 1.13% under fork load, 0% in 40,000 clean iterations.
- The liveness race test declared `MAX_DURATION: 60s` and ran 52 minutes, because the deadline is
  checked at the *top* of each iteration and so bounds iterations attempted, not wall clock (CREW-38).
- The paste test's per-chunk write timeout expired when the mock vendor was starved of CPU: "the
  vendor stopped consuming input 2 of 5 chunks into a 4533 byte prompt". Passes alone in 1.62s.
- The conformance kill-switch fixture test: flaky under load, clean alone in 7.4s.

**The lesson:** a timeout written on a quiet machine encodes the machine, not the requirement. Ask what
the threshold is *for* — if it exists to catch a hang, it can be generous; if it exists to bound a
gate's runtime, it has to be enforced somewhere that a single stalled iteration cannot skip. And
`RUST_TEST_THREADS=1` does not save you: it serialises tests within a binary and does nothing about
other processes on the box.

### `any` and `all` disagree on the empty set, and the flip that looks stricter is the one that opens

**Location:** `crates/runtime/src/adapter/tui/claude.rs`'s `is_real_user_turn` (CREW-47 rider)

**The bug:** a predicate deciding whether a vendor transcript entry is a real user turn — and
therefore whether it may resume a run parked at a finished turn — tested
`blocks.iter().any(|b| b.type != "tool_result")`. Review argued that was fail-open for mixed content
(a `tool_result` alongside any other block counted as a real turn) and that `all` fails closed, in a
guard whose whole purpose is preventing a false resume. The flip was correct for every non-empty
input and inverted the empty one: `[].iter().any(..)` is `false`, `[].iter().all(..)` is **`true`**.
So a `content: []` entry went from "not a user turn" to "resume the run" — the exact failure the
ticket existed to close, restored in a corner, by the change made to harden it.

**The lesson:** `any` and `all` are not stricter-and-weaker; they are stricter-and-weaker *on
non-empty input* and exactly opposite on the empty set, where `any` is `false` and `all` is
vacuously `true`. Every flip between them changes the empty-set answer, and the reasoning that
motivates the flip — "we want the stricter predicate" — is precisely what conceals it, because it is
reasoning about the non-empty case. When flipping, state the empty case out loud and test it: the
guard here needed `!blocks.is_empty() && blocks.iter().all(..)`, three extra words that no amount of
thinking about mixed content would have produced.

**Regression test:** `an_empty_content_user_entry_is_not_a_real_user_turn`.

### A test's name is a claim, and it is the claim people trust

**Location:** `run_timeout_ack_extend_rearms_nudge_noops_and_abort_cancels` (CREW-40)

That test asserted `rearmed: true` on a run whose activity clock was never tracked — the harness's fake
drivers bypass the adapter event pipeline that alone populates it. So the assertion re-exercised the
very lie the ticket existed to fix, and the test's *name* promised a re-arm it never observed. The fix
renamed it as well as correcting it, and moved the genuine proof to a unit test against `ActivityClock`
that sets up a really-tracked run.

**The lesson:** a name in a test list is read far more often than a body. When a test's evidence
changes, the name is part of what has to change — and a name asserting more than the body proves is the
same defect as a comment asserting more than the code does, in the place people are least likely to
check.

## Health Checks (`doctor`)

### A check scoped to the Crew source tree must not run against `--repo`

**Location:** `crates/runtime/src/doctor.rs::check_schema_compatibility`

`schema_compatibility` compared the binary's own rendered protocol schema against
`<repo_root>/packages/protocol-ts/schema/crew.schema.json` — but `repo_root` is `--repo`, the
arbitrary project Crew is running against, not necessarily a checkout of Crew's own source.
That schema document is only ever committed inside the Crew monorepo itself.

**The bug:** For every ordinary `--repo` (which is the entire point of the flag — ADR-none, this
was just never exercised against a non-Crew repo), the file is absent, and the check failed
unconditionally with a "no such file" `ConfigError` — a permanently-red check on every real-world
install. It masked a second, unrelated latent bug: `crates/runtime/tests/doctor.rs`'s
`doctor_with_nonexistent_state_dir` asserted stderr contained "No such file", which this
check's own failure text happened to satisfy for the wrong reason — the test wasn't exercising
the state-dir path it claimed to at all.

**The fix:** A missing schema document at `--repo` now means "not applicable" (`Ok`), not
"broken" — this check only fires when the document exists and disagrees with the binary,
i.e., only inside a Crew dev checkout where drift is real. Fixed the masked test to assert
what nonexistent-but-creatable state dirs actually do (get provisioned, same as `serve`) instead
of a coincidental error string.

**The lesson:** A health check that reads a path relative to the *target* the daemon operates on
must not assume that target is the daemon's own source tree, even when writing/testing the check
only ever happens inside that source tree. And a test asserting on a substring of an error message
should treat a coincidental match as a red flag to investigate, not a green light — it can hide
an entirely different bug behind the one being tested.

## Live Adapter Prompting

### A prompt injected as one atomic write is at the mercy of the vendor's render loop
**Location:** `crates/runtime/src/adapter/tui/adapter.rs::run_pipeline`

The TUI adapter wrote `prompt + "\r"` in a single `write_input` at a fixed delay after spawn. Whether
the vendor TUI processes the Enter depends on where its render loop is when the bytes land; under load
the timing shifts, so identical injections sometimes submit and sometimes render-but-never-send. The
trap is treating "the prompt shows in the box" as "the prompt was submitted" — the transcript is the
only proof. Fix: deliver text and Enter as two *phase-separated* writes (text at first output; Enter
only after a measured idle window), so submission never depends on render timing.

### An unframed prompt's own newlines are submit keystrokes
**Location:** `crates/runtime/src/adapter/tui/input.rs`, `adapter.rs::run_pipeline`

A multi-line prompt written raw to a PTY is not one message: every `\n` inside it reaches the vendor
as Enter. The vendor submits the first line, starts a turn, drops or queues the rest, and whatever
remains in the composer when the adapter's own submit byte arrives is submitted as "the prompt" — so
a long prompt arrives as a mid-sentence *tail fragment*, with nothing anywhere reporting an error.
Fix: frame the text as one bracketed paste so every byte is content rather than keystrokes, chunk it,
and bound each write so a vendor that stops reading fails loudly instead of silently truncating.

Note which explanations this rules out. The kernel tty layer was the intuitive suspect, but
`supervisor/pty.rs` writes with `write_all`, which loops on partial writes and blocks rather than
dropping; and a *tail* fragment means the head was lost, which is not what buffer overflow produces.
The symptom's shape disqualified the theory before any code was read.

### A nonce appended to a prompt cannot prove the prompt arrived
**Location:** `crates/runtime/src/adapter/tui/verify.rs`, `adapter/tui/discovery.rs`

Transcript discovery finds a vendor's session file by grepping for a nonce appended to the injected
prompt. That proves the *tail* arrived — precisely the half that survives the truncation above — so it
can never detect a lost head. The check that can is comparing the text the vendor actually recorded
against the text that was sent, which is why `TranscriptFormat::recorded_prompt` exists.

Two details worth copying. The comparison reports the *shape* of a mismatch (head lost, tail lost,
middle fragment) and the two lengths, never the prompt text: prompts are user content and the message
becomes an error string. And the accessor defaults to `None`, so a vendor whose user-entry shape is
unknown disables verification rather than failing runs over it — a check that cannot judge must abstain,
not guess.

### Two writers to one WAL file must both set a busy timeout
**Location:** `packages/extension/src/ownership.test.ts::seedTestData`

The test opens a second connection to the daemon-owned `runtime.db` and writes directly. With no
`busy_timeout`, a momentary lock held by the live daemon throws `SQLITE_BUSY`. The daemon itself sets
`busy_timeout=5000` on that database; the test connection must match it. A second writer to a live
WAL file without a busy timeout is a latent flake, not a logic bug.

## Reported Outcomes

### A success report must describe what happened, not what was requested
**Locations:** `crates/runtime/src/service/orchestration.rs` (`pane/reopen`),
`crates/runtime/src/display/pane_socket.rs`

`pane/reopen` treated the socket *file existing* as proof of a live pane. A Unix socket file outlives
its listener, and nothing removed sockets left by a daemon that died without cleaning up — so a
leftover file made the call **succeed**, returning `{backend: "hidden", paneRef: ""}` for a pane that
did not exist. The gate was answering "is there a file here" while its caller asked "is there a pane
here".

The fix is one definition of liveness — a connect probe, the only portable proof of a listener —
shared by the reopen gate and by a startup sweep that unlinks the dead ones. Both consumers now ask
the same question, so they cannot disagree. Note the sweep is keyed on liveness and never on
ownership: the pane directory is per-*user* and shared across every repository, so another
repository's live sockets sit beside this daemon's dead ones and must survive.

The general form: a value describing an *intention* must never be returned as though it described an
*outcome*. When reviewing any result field, ask whether it is measured after the fact or predicted
before it — and if predicted, say so in its name or its docs.

This class is not confined to code. A stacked pull request displayed green checks for a workflow that
never ran on it: the gate was filtered on `pull_request: branches: [main, master]`, and a stack's
child targets its parent instead. Green ticks for a gate that did not execute are the same failure
wearing different clothes. (Closed by widening the trigger to all base branches — CREW-22 — but the
next entry is about why that sentence deserved to be more than a footnote.)

### A status string is a claim, and needs the same evidence a state edge does

**Locations:** the six instances tabled below, spanning `crates/runtime/src/`, `docs/`,
`.github/workflows/`, and `crates/runtime/src/dashboard/page.rs`

The previous entry closes with "this class is not confined to code." Six instances later, that
sentence is the lesson rather than an aside, and the instances have nothing in common except their
shape: **an artefact asserted something nobody had checked.** Not one was a logic error. Each was a
string — a result field, a doc comment, a diagram label, a status indicator — that a reader would
reasonably take as measured, and that nothing measured.

| Artefact | Claimed | Actually | Closed by |
|---|---|---|---|
| `pane/reopen`'s result | a live pane | a socket file whose listener was gone | a connect probe, `display::pane_socket::is_live` |
| `run/submit`'s `result["display"]` | where the pane was placed | the placement *requested*; no consumer anywhere reads it | open when this entry was written (CREW-11) |
| A stacked PR's green checks | the gate passed | the gate never ran on that base | widening the `pull_request` trigger (CREW-22) |
| `architecture.md`'s component diagram and list | `config/merge.rs` performs the config merge and fingerprint | the module was not declared in `config/mod.rs` and never compiled | deleting it, and crediting `config/crew.rs` (CREW-23) |
| `plan.rs`'s own doc comment | its RPCs are unimplemented | they were implemented | correcting the comment (CREW-23) |
| The dashboard's `reconnecting…` | a retry that can succeed | the daemon had idle-exited; `EventSource` retries forever and nothing respawns it | counting failures, then naming the real cause |

Two things make this a pattern worth a rule rather than six independent fixes.

**The claims were all technically true or once-true.** `reconnecting…` was literally accurate — the
browser really does keep retrying. `merge.rs` really did contain merge code. `plan.rs`'s comment was
correct when written. Truth is not the property that failed; *warrant* is. Each artefact stated
something its author had no way to know at the moment the reader would read it, and none carried the
evidence that would have made it checkable.

**Every one of them survived a green build.** That is the uncomfortable half. Types, `clippy -D
warnings`, and 67 test binaries do not read prose, and a result field that is never consumed cannot
be contradicted by a test. This class is invisible to exactly the tooling the rest of this codebase
relies on.

[ADR-0023](adr/0023-run-state-edges-from-adapter-evidence.md) already settled the general
principle for one domain: a run-state edge must derive from adapter evidence, never from inference.
Nothing established the same standard for prose. The rule is the generalisation:

> Any string a human will read as a status claim needs the same evidentiary standard as a run-state
> edge. If it is predicted rather than measured, either measure it or say in the string that it is a
> prediction.

In practice, when writing or reviewing any status-shaped artefact, ask **what would have to be true
for this to be a lie, and does anything check that?** Concretely: a result field naming an outcome
should be measured after the fact or renamed to admit it is a request; a "retrying" indicator should
be able to distinguish a transient failure from a permanent one, and say which; a diagram naming a
module should be checkable with `git grep` for its `mod` declaration; a doc comment describing
behaviour should name the test that pins it.

**The sibling with no gate at all.** While implementing the dashboard fix, a new doc comment was
placed directly above an existing one. The two merged into a single block, which then documented the
new function — leaving the older function undocumented and the new one carrying a description of
behaviour it does not have. It compiled, `clippy` was clean, and all 67 test binaries passed, because
nothing in the toolchain checks that a doc comment describes the item beneath it. Caught by reading
the diff, which is the only thing that catches it. Doc comments are the one artefact in this
repository with *no* automated gate whatsoever, which is precisely why they accumulate this failure —
and why a doc comment asserting an invariant should name the test that enforces it, so the claim
degrades into a findable dangling reference instead of a quiet lie.

### A protocol type's doc comment is a shipped artifact, not an internal note

**Location:** `crates/protocol/src/`, `packages/protocol-ts/schema/crew.schema.json`

`schemars` lifts Rust doc comments into the JSON Schema's `description` fields — the *whole* comment,
not the first paragraph. Measured on the committed schema: `Redacted`'s description is 3,386 characters
across 14 paragraphs, including its "What it does not prevent" section. Across the file, descriptions
are **25,267 bytes in 207 fields, 21% of the 118 KB schema** — and `validate.ts` imports that schema at
runtime, so all of it ships in the extension bundle.

That means ADR numbers, ticket IDs and sentences like "the claim became true at CREW-28" are wire
artifacts, addressed to a reader who has none of that context.

**The lesson:** `///` on a protocol type is consumer-facing API prose; `//` is not lifted, because `///`
desugars to `#[doc = "..."]` and `//` desugars to nothing — a derive macro can only see attributes, so
this is a language guarantee rather than a `schemars` behaviour. Put the contract in `///` and the
history in `//` directly beside it. Split by *audience*, not by paragraph: a consequence like "this
field may be absent on older events, treat absence as false" is wire-relevant even though the reason
for it (`#[serde(default)]`, append-only journal) is not.

Avoid `#[schemars(description = "...")]` for this: it leaves two descriptions of one field in the same
file with nothing checking they agree — see the next entry.

### Cite the source, do not copy it — a lifted quote is a second copy with no guard

**Locations:** `docs/architecture.md`'s redaction section, `packages/protocol-ts/src/index.ts`,
`.github/workflows/ci.yml` ↔ `build-matrix.yml`, `crates/xtask`'s two codegen allowlists

Prose describing a guarantee drifts toward the guarantee the writer wishes existed. `architecture.md`'s
redaction section claimed `Redacted` was "unconstructible except via the Redactor" and that you "cannot
accidentally construct a Redacted field with unredacted input" — through four review passes, while the
type's own doc comment said the opposite in careful detail, and while the same section described the
second constructor two paragraphs later. The fix was to lift from the doc comment, which had already
been argued into correctness.

But lifting has its own failure mode, and this repo has three instances of it: the hand-maintained
`protocol-ts` barrel that drifts from generated bindings (caught by `generate --check`), the `changes`
job duplicated across two workflows (caught by a comment, because nothing mechanical can), and two
codegen allowlists that must agree with nothing checking that they do. **A verbatim quote is a second
copy** — and worse than a paraphrase in one way: a paraphrase invites checking, while a quote looks
like the source.

**The lesson:** prefer a short true sentence plus a pointer to the definition over a reproduced
paragraph. The pointer sends the reader to the copy that cannot go stale, because it lives beside the
code. If a copy is unavoidable, say in the text that it *is* a copy and name its source, so whoever
finds a discrepancy knows which side is authoritative instead of guessing.

## Structural Limits and Long-Lived Consumers

### A path-length limit is a property of the layout, not an edge case
**Location:** `crates/runtime/src/paths.rs`, `crates/runtime/src/ipc/server.rs`

The pane attach socket lived at `<state_root>/repos/<32-hex>/panes/<36-uuid>.sock` — a fixed 97-byte
suffix after `$HOME`. macOS caps `sun_path` at 104, so even `/Users/x` overflowed: the path could not
fit for *any* real home directory, on every run, for every user. Both call sites guarded and rejected
correctly, and tests passed because they bound sockets under `/tmp`, sidestepping the layout entirely.

A limit that a layout can never satisfy is not an edge case to guard, it is a design error to fix —
and a test fixture that avoids the production layout will never reveal it. Compute the worst case from
the layout itself (longest realistic prefix, maximum-length components) and assert against the
platform bound, rather than testing a path that happens to fit.

### A subscription that heals only on user action is not self-healing
**Location:** `packages/extension/src/monitor/controller.ts`

The monitor subscribed once at session start. When the daemon restarted or idle-exited, the client
went dead and nothing noticed: the widget stayed blind until the user happened to type `/crew`, whose
handler reconnected as a side effect. Tool calls self-healed their own client, which made the
subsystem *look* resilient while the one long-lived consumer was not.

Fix: react to the close rather than waiting for a caller — a close listener plus backoff. Two
constraints matter. Reconnect must not spawn a daemon that deliberately idle-exited (ADR-0008), so the
automatic path connects only, and spawning stays on user-initiated paths. And the shutdown handler
must set a flag rather than only cancelling a pending timer, because shutdown's own client close fires
the listener and re-arms it.

## Build Determinism

### A committed build artifact can depend on the build host's platform
**Location:** `packages/extension/dist/index.js`, CI `bundle-check`

The bundle embeds Bun's platform-specific module shim; a darwin-arm64 rebuild diverges from a
linux-x64 rebuild even at the same Bun version. `bundle-check`'s byte-exact contract is only
satisfiable from CI's platform. Produce the committed artifact in CI's environment (`refresh-bundle`
workflow), or document that local rebuilds must happen on linux-x64. Don't weaken the check to
"close enough" — the bytes matter.

The non-obvious part is knowing *when* the bundle is stale. "Does this feel like a TypeScript change"
is the wrong test — a change can read as pure Rust and still touch extension source. The mechanical
test is:

```
git diff --name-only origin/main...HEAD -- packages/extension/src packages/protocol-ts
```

Non-empty means refresh the bundle. `packages/protocol-ts` counts because
`packages/protocol-ts/src/validate.ts` imports `crew.schema.json` at runtime for the Ajv validators,
so the schema is *embedded in the bundle* — generated TypeScript types are erased at build time and
do not affect it, but a schema change does.

**Run it after committing.** `origin/main...HEAD` compares *commits*, so it reports nothing while
the regenerated files are still sitting in the working tree — which is exactly the moment you reach
for it, right after `bun run generate`. An empty result then means "you have not committed yet", not
"no refresh needed", and the two are indistinguishable from the output. Either commit first, or check
`git status` alongside it.

**And do not try to confirm staleness by rebuilding locally.** On any machine that is not linux-x64
the rebuilt bundle differs from CI's *always*, because Bun embeds a platform-specific module shim —
so a local `bun run build` followed by `git diff` produces a large diff whether or not the committed
bundle is stale, and it cannot distinguish the two. That diff is not evidence. The content test is:
grep the fresh build and the committed bundle for a string only the change introduces (a new event
name, say). Present in one and absent in the other is proof; a byte diff is not.

---

## Protocol Evolution

### Retiring a journaled wire value is three rules, not one

**Location:** `crates/protocol/src/display.rs` — `DisplayBackend::Terminal` (WP9) and
`DisplayPlacement::Embedded` (CREW-52)

**The bug:** WP9 retired the `Terminal` display backend by deleting the enum variant. Nothing else.
`DisplayBackend` is journaled, so any event log carrying `backend: "terminal"` stops deserializing —
`events/replay`, crash recovery and `audit export` all fail on it. Nobody noticed because no local
journal predates WP9; the exposure is entirely other people's data.

CREW-52 was then about to do it again to `DisplayPlacement::Embedded`, whose removal was approved on
the strength of a good argument (no backend implements it — herdr and tmux refuse it, osWindow ignores
it, `hidden` has no pane to place). `placement` is journaled too, and `"embedded"` was present in the
maintainer's own default-root journal, not merely in a test fixture.

Deleting it is not merely risky, it is **incoherent with two of this repo's stated invariants**:
replayed events must pass the extension's Ajv validation (invariant 2), so the value must remain in
the JSON Schema; and Rust types are canonical (invariant 1), so a value that must remain in the schema
must remain in the Rust enum. "Just delete it" cannot be reconciled with either.

**The lesson:** retiring a value that has ever been journaled is three obligations, and doing one of
them is how you get a latent replay failure in somebody else's repository.

1. **Stop producing it** — remove every construction site.
2. **Keep accepting it on replay, forever** — the append-only journal cannot be rewritten, so the
   deserializer owes old rows a definition for as long as they exist.
3. **Reject it as input** — otherwise the retirement is half-done: the default stops using it while
   any client can still request it, and a schema that must keep listing it advertises it as live.
   This asymmetry (accept on read-back, refuse as a request parameter) is not expressible in the enum
   itself; it is a rule at the request boundary and will not exist unless someone writes it.

And say **which kind of dead** it is, because the schema cannot: `Embedded` was always meaningless
(no implementing backend ever existed), while `Terminal` was a real, working backend deliberately
dropped. Both would otherwise sit in the schema looking equally alive.

**Our exception, stated so the rule is not misread:** the maintainer ruled pre-release that this
repo's own journals may be discarded, which moots obligation 2 *for us, this once*. That ruling is
about our data, not about the rule — and obligation 3 survives it untouched.

**Regression tests:** specified in CREW-52 as `a_journaled_legacy_embedded_placement_still_replays`
and `a_journaled_legacy_terminal_backend_still_replays`, plus typed-rejection tests for the
request-boundary half. Not yet landed at the time of writing; the entry records the rule, not a
completed fix.

---

## Composition and Defaults

### A default trait-method body is permission to say nothing, and in a wrapper chain silence is wrong

**Location:** `crates/runtime/src/adapter/event_sink.rs`'s `AdapterEventSink::note_real_user_turn`
(CREW-47, and its rider)

**The bug:** a new out-of-band signal was added to the sink trait with a default no-op body, so
existing implementors would keep compiling. Production wraps them:
`SettlementSink::wrap(RunLifecycleSink::wrap(..))`. `SettlementSink` therefore sits between the
adapter that raises the signal and the only sink that acts on it — and without an explicit forwarding
override it would have inherited the default no-op and swallowed the signal silently. The fix worked
in unit tests because they compose `RunLifecycleSink` directly, bypassing the exact layer where the
hazard lives. It was found by tracing the real composition order by hand, not by any test; and the
one-line forwarding override was itself untested and deletable with green CI until a review asked for
a test that composes the production order.

**The lesson:** a defaulted trait method is a decision an author is allowed not to make, and in a
decorator chain the default answer — do nothing, forward nothing — is the wrong one. Removing the
default body converts a silent runtime no-op into a compile error: every implementor must state
whether it forwards, acts, or deliberately drops, and a reviewer can see which. This is the same
principle as `crew_protocol::Redacted`'s two claim-named constructors, applied to composition instead
of to text: the failure mode being eliminated is *silence*, not error.

Two details worth carrying. Ceremony is acceptable *here* — where ADR-0006 rejected it for the
redaction write path — because there is no narrower place to put the obligation (the risk is
structural to wrapping, not to any field), and because the compiler cannot skip ceremony it enforces.
And when the default was removed, the compiler enumerated implementors that a careful manual scan had
missed, including integration-test sinks nobody had counted — a hand-built inventory of a trait's
implementors is exactly the kind of list that misses entries, and the compiler does not have that
failure mode.

**Regression test:** `a_real_user_turn_resumes_a_settled_run_through_the_production_sink_stack`,
which composes the real production order rather than the inner sink alone.

---

## Test Suite Integrity

### A mechanical type-level fix can compile clean while leaving a test's evidence invalid

**Location:** `crates/runtime/tests/kill_switch_authorization.rs`
(`the_kill_switch_never_shrinks_effective_capabilities`)

**The bug:** crew-v2 gap-closure WP-C deleted the headless control plane and retargeted
`run_fixture_conformance` from `AdapterMode::Headless` to `AdapterMode::Tui`. Swapping the enum
value this test passed compiled clean and would have looked done: the type checker has no opinion
on what a test's assertions actually establish, only on whether the code that produces the values
they check type-checks. Run for real, the test panicked -- TUI fixture mode's only live dependency
is `PROBE`'s real `--version`/binary check, so the kill switch now skips only that (ungated)
scenario, never Codex's `FOLLOW_UP`/`SESSION_RESUME` the way headless fixture mode's live-turn
dependency once did. The test's own assertion said otherwise, and would have kept saying otherwise
-- silently proving a claim about the *previous* control plane against evidence the current one no
longer produces -- if the enum swap alone had been trusted as "the fix."

**The lesson:** A mechanical, type-checking fix to a test after a dependency changes underneath it
proves only that the test *compiles* against the new shape -- never that it still proves what its
name and assertions claim. The guard is running the test and re-deriving, from what actually
happens, whether its evidence still holds -- not inferring compilation success. Before trusting a
retargeted test, run it deliberately unfixed first (only the mechanical change applied) to observe
the *actual* failure, exactly as this session did here: the panic's message named precisely which
assertion had gone stale, which is what made the real fix (in `crates/runtime/src/conformance/report.rs`'s
unit-level R68 proof and this file's rewritten assertions) targeted rather than guessed at. The same
methodology already used throughout this codebase's regression tests ("verify by breaking it") applies
just as much to fixing an existing test as to writing a new one.

**Regression tests:** N/A -- this is a process lesson about how a fix was verified, not a
production code path a test can pin. The corrected test itself
(`the_kill_switch_never_shrinks_effective_capabilities`) and `conformance::report`'s
`a_skipped_scenario_leaves_its_gated_capability_declared` are what the fix left behind.

---

### A semantics change needs two tests: one that distinguishes, one that preserves

**Location:** `crates/runtime/tests/run_result.rs` —
`run_result_reads_an_answer_that_follows_a_content_free_boundary` and
`run_result_reads_up_to_the_first_turn_end_not_a_later_one` (CREW-49)

**The bug:** CREW-49 changed `run/result`'s fold boundary from "the first turn-end" to "the first
turn-end that already has result text accumulated before it". Two tests cover it and neither is
sufficient alone, which is the point. The new test — text arriving after a content-free boundary is
still returned — **distinguishes** the new behaviour from the old: it fails on the pre-change code.
The older test — text A, boundary, text B, boundary, expect A — **preserves** the property the
change promised not to weaken, and an unconditional `break` (the pre-change implementation) satisfies
it just as well. Ship only the first and someone may later delete the break entirely, silently
reading later turns and rewriting an answer the leader has already read. Ship only the second and the
original bug returns untouched.

A review of that change asked for the preservation test as though it were missing; it had been on
`main` for weeks, and the grep behind the claim had failed (see "An instrument you do not read..."
above). The duplicate was dropped before merge. That mistake is worth recording alongside the
lesson, because a reviewer demanding a test that already exists is the same question asked badly:
*which production change would make this test fail?* Asked properly of the existing test, the answer
is "removing the break" — which is exactly the half the new test does not cover.

**The lesson:** for a semantics change, ask that question of each test and compare the answers. If
two tests would fail on the same production change, you have one test under two names, and the
suite looks larger than it is. If some plausible change would break neither, the pair is incomplete.
This is the `compile_fail`/positive doctest discipline applied to behaviour instead of
compilation — the negative alone is weak evidence, and so is the positive.

**Regression tests:** the pair itself.

### Changing a doc comment's sigil is a test-suite edit when that comment holds a code fence

**Location:** `crates/protocol/src/event.rs` (`Redacted`, now `RedactedBoundaryDoctests`) -- CREW-45

**The bug:** CREW-45 moved maintainer-facing history out of protocol doc comments, because
`schemars` lifts a `///` comment verbatim into `crew.schema.json`'s `description` while a `//`
comment desugars to nothing a derive macro can see. `Redacted`'s doc comment was the largest such
block at 3,386 bytes -- and buried in it, below four sections of prose, sat the `compile_fail` /
positive doctest pair written by CREW-29 as the executable proof that a bare `String` cannot
populate a caller-carrying field. Moving the block wholesale to `//`, which is what the change was
mechanically *about*, would have deleted both tests. Not disabled them, not failed them: a doctest
only runs from a `///`, `//!` or `#[doc]` comment, so under `//` the code fences become ordinary
prose and simply stop existing as tests. There is no error, no warning, and no diff signal that
distinguishes "prose moved" from "proof deleted" -- both are the same two characters, on adjacent
lines, in the same hunk. `cargo test` stays green because the tests it no longer runs cannot fail.

A `compile_fail` doctest is doubly exposed here. It is already the weaker half of its pair (it
passes on *any* compilation error, which is why the positive twin sits beside it), and it is the
half whose disappearance is least likely to be noticed, because nothing downstream depends on it
having run.

**The lesson:** Treat any edit that changes a doc comment's *sigil* as an edit to the test suite,
not to documentation, whenever that comment contains a code fence. The guard is a count taken
**before** the edit -- `cargo test --doc -p crew-protocol`, note the number and the names -- and
compared after; that comparison is the only thing standing between a prose reorganization and the
silent removal of a proof. Where the prose must leave the `///` but the doctests must keep running,
the rescue is to move the fences to their own `#[cfg(doctest)] struct FooDoctests;` item: the tests
still run, and the schema never sees a wall of Rust. This codebase already records that *a test's
name is a claim*; the sharper case is that a test can stop making any claim at all without its name
changing, or its file changing, or anything going red.

**Regression tests:** The rescued pair itself, on `RedactedBoundaryDoctests` in
`crates/protocol/src/event.rs` -- one `compile_fail`, one positive, verified as the same two tests
before and after the move rather than assumed. `schema_compatibility_passes_against_the_committed_schema`
covers the other half of CREW-45 (that the shipped descriptions actually changed).

---

## Deletion Sweeps

### A deletion sweep must sweep claims, not just references

**Location:** crew-v2 gap-closure WP-A/B/C, broadly -- exemplar:
`crates/runtime/src/adapter/tui/omp.rs`'s `base_args` doc comment (WP-C review round 1, I-1)

**The bug:** Across three work packages of deletions, nearly every finding that survived review was
the same shape: a true prose claim -- "validated by X", "proven by Y", "this module owns that
confirmation" -- that quietly stopped being true the moment its referent changed or was deleted,
with nothing in the toolchain forcing anyone to notice. The exemplar: WP-C's own inventory step
(`grep -rn 'adapter::(claude|codex|copilot|omp_rpc)::'`) correctly found and classified every code
*reference* to the headless adapters before deleting them -- and still missed that
`adapter/tui/omp.rs`'s `base_args` doc comment made an *assertion about* the headless adapter
(the model selector "is validated ... by the headless adapter's probe") without ever naming it as a
path or symbol the grep could match. The inventory swept symbols correctly; the claim hid in prose
a symbol-grep cannot see.

**The lesson:** `grep` finds references (a path, a type name, an import); it does not find claims (a
doc comment asserting something is true of, checked by, or owned by a thing being deleted). A
deletion sweep's inventory step -- however systematic -- only ever proves reference-completeness,
never claim-completeness. Before trusting an inventory as done, separately ask: what does the
surviving code (or docs, or a release checklist) *say* about the thing being deleted, independent
of whether it names it directly? That second pass is prose-shaped, not grep-shaped -- it means
reading, not searching -- and is exactly what this WP's review rounds kept surfacing one deletion
sweep after the next: a stale contract comment, a checklist never updated, a schema description
never touched, a citation pointing at the wrong sibling test. None of these were reference bugs; all
of them were claims a grep-based inventory has no way to catch.

**Regression tests:** N/A -- this is a review-process lesson, not a single code path. The concrete
instance this WP left behind is fixed (`OmpTuiVendor::preflight` restores the enforcement `base_args`
claims, with its own tests); the transferable practice is the takeaway.
