# Engineering Lessons

**Audience & purpose:** contributors debugging something that feels like it might have happened
before — a companion to [code-walkthrough.md](code-walkthrough.md)'s debugging playbook and the
developer manual ([getting-started.md](getting-started.md)). This document catalogs hard-won
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
`crates/runtime/src/config/merge.rs`

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

### A type checker is a gate only if it is run to failure
**Location:** `bun run typecheck`, CI `typecheck` job

`tsc --noEmit` was assumed green because the code "looked fine" and other checks passed. It is only a
gate once something has watched it fail and then pass. The same standard applies to any new test:
edit the production code to break it and confirm the test notices, or the test is decoration. The
cheapest version of this discipline is to write the assertion first and watch it fail for the right
reason.

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

This class is not confined to code. A stacked pull request displays green checks for a workflow that
never ran on it, because the real gate is filtered on `pull_request: branches: [main, master]` and a
stack's child targets its parent instead. Green ticks for a gate that did not execute are the same
failure wearing different clothes.

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
