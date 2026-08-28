# The BATMAN journal — a narrative of how this got built

**Audience & purpose:** anyone curious how the codebase got this way — optional reading for either
manual's audience, not required for either. This is the companion to
[the Rust primer](rust-primer.md). The primer teaches you Rust using this codebase as the
textbook; this document tells you the *story* of the codebase itself — every commit, in order,
with the problem it solved, the decision made, the alternatives that lost, and the test that
proved it. Read [architecture.md](architecture.md) for the finished design and
[code-walkthrough.md](code-walkthrough.md) for how to navigate it; read this when you want to
know *why* it looks the way it does, and what it looked like before it did.

Two hundred and sixteen commits, nine milestones, one running theme: **OMP is the brain, BATMAN is
the hands.** Every decision below either draws that boundary more precisely or discovers where it
had blurred. Where a decision is significant enough to outlive its commit, it has a matching entry
in [`docs/adr/`](adr/) — this journal narrates the *how*; the ADRs record the *what was decided* in
a form meant to survive being read out of context, years from now, by someone who wasn't here.
Parts I–IV (the first 99 commits) close with the very first version of this document; no new ADR
was written for anything in Parts V–IX below — none of those decisions were judged significant
enough to outlive their commit, and this journal says so rather than inventing one to look complete.

---

## Part I — Foundation: proving the shape works before it does anything useful

The Foundation milestone's only goal was a vertical slice: OMP loads an extension, the extension
talks to a daemon it can start and reconnect to, one event survives a restart, and one tool
returns real status — no model call anywhere. Twelve commits, one working day and a half.

### 1. `build: scaffold batman workspaces` (e62e5ec)

Every project's first commit is a lie of omission — it looks like nothing, but it's the only
commit that fixes the shape of everything after it. This one decided: two package managers, one
repo. `Cargo.toml` at the root declares a workspace of three crates (`protocol`, `runtime`,
`xtask`) before any of them have real content; `package.json` + `bunfig.toml` declare the mirror
image on the TypeScript side (`packages/extension`, `packages/protocol-ts`). `rust-toolchain.toml`
pins the Rust version so "works on my machine" isn't a debugging step later.

The decision that actually matters here — external extension plus a *separate* Rust binary,
rather than one process — doesn't show up as code in this commit at all. It shows up as the
*absence* of code: no attempt to embed Rust in the OMP process, no attempt to write orchestration
logic in TypeScript. That absence is [ADR-0001](adr/0001-omp-extension-with-separate-rust-daemon.md).
Everything from here on is either building the two sides of that boundary or building the thing
that lets them talk.

### 2. `feat(protocol): define initialization and event envelopes` (480d428)

First real code, and it goes into `crates/protocol`, not `crates/runtime` — on purpose. Before a
single line of daemon logic exists, the wire types exist: `EventEnvelope`, `RuntimeEvent`,
`Timestamp` (a canonical UTC RFC 3339 string, never a raw `DateTime` leaking construction-timezone
ambiguity to the wire), the eight UUIDv7 id newtypes (`ids.rs`'s `uuid_id!` macro), and the
JSON-RPC envelope shapes in `rpc.rs`. Every one of these derives `Serialize`, `Deserialize`,
`JsonSchema` (schemars), and `TS` (ts-rs), and every struct carries
`#[serde(rename_all = "camelCase", deny_unknown_fields)]`.

That last attribute pair is a decision, not a style choice: `deny_unknown_fields` means an
extension talking to a *newer* daemon that added a field it doesn't understand gets a hard error
instead of silently ignoring data — protocol drift becomes loud immediately instead of quietly
corrupting behavior six months later. `tests/wire_contract.rs` (also in this commit) exists
entirely to prove that promise: it serializes real values and asserts the JSON keys are
camelCase, and that an unknown field is rejected.

This is also where "Rust is canonical" stopped being an aspiration and became a fact you could
point at: nothing in `packages/` yet, because nothing in `packages/` is allowed to define a wire
type first. [ADR-0002](adr/0002-rust-canonical-protocol-with-generated-bindings.md) is the
decision this commit enacts.

### 3. `build(protocol): generate schema and TypeScript bindings` (700380f)

Having a canonical Rust type is only half the promise — the other half is that TypeScript never
hand-writes a competing definition. This commit builds the machine that makes that automatic:
`crates/xtask`'s `generate` command walks every `#[ts(export)]` type, emits one `.ts` file per
type into `packages/protocol-ts/src/generated/`, and emits one JSON Schema 2020-12 document
(`batman.schema.json`) from a synthetic `ProtocolDocument` root struct that references every
request/result/event type so nothing gets forgotten.

The `--check` mode (generate into a temp directory, byte-compare against what's committed) is the
part that makes this durable rather than aspirational: `bun run generate --check` runs in every
`bun run check`, so a Rust type change that forgot to regenerate fails CI, not a code review three
weeks later. `fixtures/protocol/initialize.{request,response}.json` — golden files deserialized
through *both* language's types in their respective test suites — is the second half of the same
idea: it's not enough that Rust and TypeScript each think they agree, a shared fixture makes them
prove it against the same bytes.

### 4. `feat(runtime): derive secure repository state paths` (8f8e70a)

Now the daemon crate gets its first real logic, and it's not about sockets or events — it's about
*where anything lives at all*. `security/mod.rs`'s `StateRoot::resolve(env, home)` implements a
three-tier precedence (`BATMAN_STATE_DIR` > `XDG_STATE_HOME/omp/batman` > `$HOME/.omp/batman`),
and `paths.rs`'s `RuntimePaths::resolve` turns a repository path into a stable, private,
per-repository directory: canonicalize it, walk parents for a `.git` marker (directory or file —
worktrees have a file), hash the canonical root to get a `repository-id`, derive a `ProjectId`
from the same hash.

Two decisions worth noticing because they're easy to get wrong quietly: the resolver takes
`env`/`home` as *explicit parameters*, never reading `std::env::var` directly — so a test can drive
every precedence tier from a fixture instead of mutating process-global state (and racing every
other test that also wants to mutate it). And directories are created *with* mode `0700` at
creation time (`ensure_private_dir`), not created-then-chmod'd — there is no window where the
directory exists world-readable. `fixtures/state/state-root-cases.json` is shared between this
Rust resolver and its TypeScript twin (`state.ts`, same commit) — two implementations, one truth
table, so they can never drift about which environment variable wins.

### 5. `feat(runtime): add SQLite journal actor` (8cd8ad8)

This is the commit the Rust primer's Day 6 was written to explain, and it's the one that decided
two things you'll meet constantly for the rest of the project.

First: **SQLite, and only SQLite** — `db/migrations.rs` opens a private (mode `0600`) file, sets
`journal_mode=WAL`, `foreign_keys=ON`, `synchronous=FULL`, `busy_timeout=5000`, and migrates with
`rusqlite_migration`. No Postgres, no Redis, no container — the Global Constraints ruled those out
before this commit was written, but this commit is where "no cloud dependency" became "one file
on disk with WAL turned on." [ADR-0003](adr/0003-sqlite-as-the-sole-persistence-engine.md).

Second, and more interesting to actually *read*: `rusqlite::Connection` isn't `Send` in the way
that makes it easy to share across async tasks, and SQLite wants one writer. So `db/actor.rs`
gives the connection to exactly one dedicated `std::thread`, and every other part of the program
talks to it by sending a `Command` enum value down a bounded `tokio::mpsc` channel, each carrying
a `oneshot::Sender` for the reply. A write command's reply is sent *only after* `tx.commit()`
succeeds — "the call returned" is defined to mean "it's durable," never "it's queued."
[ADR-0005](adr/0005-single-thread-actor-owns-the-sqlite-connection.md) is this pattern; you'll see
it reused, unmodified, for every mutation added in the next twenty commits.

The same commit lands `security/redaction.rs` — `Classified<T>` (a value tagged `Visible`,
`Thinking`, or `Secret`), `Redactor::sanitize` (drops `Thinking`/`Secret` outright, masks
regex-matched secrets in `Visible` text), and `PersistableEvent`/`SanitizedJson` — types with
private fields and *no public constructor*, so the only way to produce one is to pass through the
redactor. `crates/runtime/tests/redaction_boundary.rs` doesn't just unit-test the redactor; it
pushes a fixture with visible text, a secret, and a thinking block through the *real* append path
and then byte-scans `runtime.db`, the WAL file, and `runtime.log` for the raw secret bytes. If this
test ever fails, it fails because a secret reached disk, not because an assertion string changed.
[ADR-0006](adr/0006-type-enforced-redaction-boundary.md).

### 6. `feat(ipc): add negotiated runtime socket protocol` (4ed1b14)

Now the two sides get a way to talk. `ipc/server.rs` binds a Unix domain socket at mode `0600`;
`ipc/connection.rs` splits each connection into one reader task and one serialized writer task
(fed by a bounded channel, so a live event notification can never interleave mid-frame with an
RPC response); the wire format is newline-delimited JSON with a 4 MiB bootstrap cap
(`tokio_util::codec::LinesCodec`), negotiated down after `initialize` to
`min(client offer, runtime max)`, with a 64 KiB protocol floor.

Every one of those numbers is a decision with a "why," and [ADR-0004](adr/0004-json-rpc-2-over-bounded-ndjson-on-a-unix-socket.md)
is where they're written down together rather than scattered across code comments: JSON-RPC 2.0
because both sides already speak JSON-serializable Rust/TS types; NDJSON because it's trivially
line-buffered on both ends without a length-prefix framing layer; a Unix socket, never a TCP
listener, because the security boundary is "same machine, same user" and a TCP port would have to
re-derive that from scratch.

The other half of this commit is authorization: `ClientAuth` (a role-tagged enum —
`ompExtension`/`workerMcp`/`display`, each with different required fields) authenticates a
connection into a `ClientPrincipal`, and `ClientPrincipal::allowed_methods()` returns the *exact*
list of methods that role may call. Dispatch consults this table, never a client-supplied method
name against a blanket allow-list — a method outside the caller's table returns
`METHOD_NOT_FOUND`, indistinguishable from a method that doesn't exist at all. This is
[ADR-0009](adr/0009-role-based-authorization-from-the-connection-not-per-call.md), and it's the
single decision that made every later milestone's new methods safe to add by just extending a
match arm's list, never by writing a new permission check from scratch.

`packages/extension/src/client.ts` is the TypeScript mirror: incremental UTF-8 buffering, a
`Map<string, PendingRequest>` correlation table, and — this is worth calling out on its own —
**Ajv validation of every single inbound frame** before it ever reaches extension logic. A
response with an extra field the schema doesn't know about is rejected client-side, the same
`deny_unknown_fields` promise from commit 2, enforced a second time on the receiving end.

### 7. `feat(runtime): manage detached repository daemons` (18a76fd)

A socket is useless if nothing's listening, and nothing should have to be listening *manually*.
This commit is the daemon lifecycle: `lifecycle.rs`'s `serve()` takes an exclusive `flock` on a
persistent `runtime.lock` file before doing anything else. Exactly one process wins the race; the
loser reads the winner's metadata (written under the held lock) and exits with code **73**,
printing machine-readable `already_running` JSON. There's no lock *file deletion* anywhere in this
design — staleness is implicit, because the kernel releases the flock the instant the owning
process dies, crash or clean exit alike. [ADR-0007](adr/0007-repository-scoped-singleton-via-kernel-flock.md)
picked this over the more obvious "write a PID file, check if that PID is alive" because PID
liveness checks race PID reuse and need a second mechanism (a lock, usually) to be correct anyway
— so skip straight to the lock and let it double as the liveness check.

Idle shutdown (`--idle-seconds N`: exit after N seconds with zero connections and zero active
runs) and graceful stop (SIGTERM or the in-band `runtime/shutdown` RPC: append a redacted
`runtimeStopping` event, close the database actor, remove the socket, release the lock, exit 0 —
in that order, so "the socket is gone" *means* "the journal is closed") round out the daemon side.

The TypeScript side, `runtime.ts::ensureRuntime`, is the connect-or-spawn half:
try to connect first; if nothing answers, validate/select a binary, spawn it *detached*
(`stdio: "ignore"`, `.unref()`, deliberately omitting `--foreground` so the daemon owns its own log
file), and retry connecting with bounded exponential backoff for up to five seconds. If a
concurrent caller won the startup race, this caller just connects to the winner instead of
failing — the flock from the Rust side is what makes that safe. [ADR-0008](adr/0008-connect-or-spawn-with-idle-self-shutdown.md)
is the pair of these: no system service to install, no daemon to remember to start — the first
tool call that needs it starts it, and it shuts itself down when nobody needs it anymore.

### 8. `feat(extension): expose batman runtime status` (7ef7e49)

First payoff: `batman_status` (a tool) and `/batman-status` (a command), both calling the same
`getRuntimeStatus(ctx)` — reuse a cached client or `ensureRuntime`, request `runtime/status`,
Ajv-validate the result, return concise text plus the validated object as `details`. On failure:
`isError: true`, a machine-readable `code`, a **generic** message, and a runnable `doctorCommand`
— never a stack trace, a filesystem path, or an environment value, because those failure paths are
exactly the ones a user might paste into a bug report or a screen-share.

This is the first commit where the vertical slice is actually *whole*: OMP loads the extension,
the extension starts or reconnects to the daemon, negotiates the protocol, and returns real status
without a model call anywhere in the path. Everything built before this commit was necessary but
invisible; this is the first commit you could demo.

### 9. `build: add batcave platform package loader` (39596bc)

Foundation's last piece of real engineering: how does a *packaged* extension (not a dev checkout)
find its daemon binary? `platform.ts::resolveBatcave` maps `(platform, arch, libc)` to one of four
npm `optionalDependencies` leaf packages (`@nikolasd/batman-darwin-arm64`, `-darwin-x64`,
`-linux-arm64-gnu`, `-linux-x64-gnu`) and rejects everything else — musl, Windows, anything — with
a typed `UnsupportedPlatformError`, never a silent fallback to source-building or a generic binary.
For a packaged binary it verifies a SHA-256 against the leaf's `manifest.json` and requires the
leaf's version to equal the extension's own version, so a half-updated `node_modules` fails loud
instead of running a stale binary silently. [ADR-0010](adr/0010-platform-binaries-as-npm-optional-leaf-packages.md).

`OMP_BATMAN_BINARY` — a validated absolute-path override, checked *before* any spawn attempt for
existence/regularity/executability — bypasses all of that for development, which is exactly the
escape hatch every commit and every smoke test from here on actually uses; there's no committed
binary in this repository at all, by design (`crates/xtask package` installs one into a leaf
locally, for a *release* to publish, not for this repo to ship).

### 10. `fix(runtime): close foundation review gaps` (f6237dd)

Every real project has this commit: the one where you stop building forward and go back over what
you just built with fresh eyes. Twelve files touched, none of them new features — hardening the
`repository_id_from_canonical_root` hashing (a new fixture, `repo-id-cases.json`, pins the exact
algorithm against both languages), tightening a couple of `security/mod.rs` permission checks,
closing a database-actor edge case, extending `runtime.test.ts`'s override validation. Nothing
here is a story on its own; together, it's the difference between "the happy path works" and "the
happy path works and the unhappy paths fail the way they're supposed to."

### 11–12. `docs: add README, architecture, onboarding, and Rust primer docs` / `docs: update project title formatting` (5a6c746, 92b6e21)

Foundation's closing act: four documents (`README.md`, `architecture.md`, `code-walkthrough.md`,
`rust-primer.md`) written the moment the code they describe stopped changing under them, not
after. This is the convention this journal and the ADRs in `docs/adr/` are extending, not
inventing — Foundation set the precedent that a milestone isn't done until someone who wasn't in
the room can read four documents and rebuild the mental model from scratch. The title-formatting
fix is exactly as small as it sounds: a one-line polish pass, included here only because skipping
it would make this journal's commit list not match `git log`.

---

## Part II — Orchestration Extension: giving OMP something durable to point at

Foundation proved the shape works. The Orchestration Extension's job was to make that shape *mean*
something: stable task/worker/run records, six tools a model can actually call, a lifecycle no one
can cheat, audited messaging, correlated approvals, and a monitor that shows all of it live — all
without moving one gram of scheduling authority out of OMP and into Rust.
[ADR-0011](adr/0011-omp-retains-task-graph-authority.md) is the decision this entire Part either
draws more precisely or discovers where it had blurred: Rust persists OMP-supplied intent
verbatim and enforces only the transitions its own runtime evidence gives it standing to make —
never a scheduling, retry, merge, or worker-selection decision, no matter how convenient one
would be to make locally. Twelve commits, and the last two are where the plan met reality.

### 13. `feat(protocol): define orchestration records and methods` (3d604af)

Task 1 of the orchestration plan, and it repeats commit 2's move exactly: define the vocabulary in
Rust before writing a single line of runtime logic. `task.rs` (`TaskRef`), `worker.rs`
(`WorkerProfileRef`, `Worker`), `run.rs` (`Run`, `RunSpec`, `RunFlags`, and the ten-variant
`RunState`), `message.rs` (`RunMessage`, `MessageKind`, `DeliveryState`), `approval.rs`
(`ApprovalRequest`), and a new `method.rs` extending `BatmanMethod` with eighteen orchestration
methods.

`RunState::can_transition_to` is the decision this commit is really about: the ten states and
their legal edges are written once, as an explicit relation, and *nothing* transitions a run
except by passing through it. `crates/protocol/tests/domain_contract.rs` — 474 lines, the single
largest test file added this milestone — asserts every one of the 28 legal edges is accepted and
every illegal edge (26 of them, including every self-transition and every edge out of a terminal
state) is rejected. That table-driven exhaustiveness is deliberate: a state machine you can get
*mostly* right is worse than one you get demonstrably completely right, because "mostly" fails
exactly when nobody's watching. [ADR-0012](adr/0012-explicit-run-lifecycle-relation-runtime-evidence-only.md).

`RunFlags`'s six booleans (`degradedControl`, `needsReconciliation`, `protocolUnhealthy`,
`policyQuarantined`, `workspaceDirty`, `childrenActive`) are independent on purpose — none of them
is *derivable* from `RunState`, because a run can be `working` and `protocolUnhealthy`
simultaneously (an approval callback failed, but the run itself is fine) and collapsing that into
the state enum would either lose information or explode the state count combinatorially.

### 14. `feat(protocol): add orchestration domain types and extend dispatch` (cb172d5)

The next thirty minutes of the same task, and the commit message is honest about where it landed:
"3/6 [domain repository tests] pass, 3 fail pending DomainRepository" — this commit is
deliberately mid-flight, TDD in the literal sense the roadmap's skill reference asked for. The
runtime side gets just enough to route the new `BatmanMethod` variants to `METHOD_NOT_FOUND`
(rather than an unhandled-match compile error) and extend the role tables so the *next* commit has
somewhere to plug real dispatch in. `crates/runtime/tests/domain_repository.rs` — 724 lines —
already exists here, red, waiting.

### 15. `chore(protocol-ts): regenerate bindings for orchestration domain types` (84f1912)

A one-file, nineteen-line commit that exists only because `bun run generate --check` said so.
Worth including in this journal for exactly one reason: it's proof the codegen promise from
commit 3 still held eighteen commits and a full milestone later, without anyone having to remember
to run it — the check failed, someone ran `bun run generate`, and this is the diff that fixed it.

### 16. `feat(runtime): persist orchestration projections` (879b421)

Task 2, and the payoff for Task 1's contract. `db/migrations.rs`'s `MIGRATION_2` adds six
normalized tables (`worker_profiles`, `tasks`, `workers`, `runs`, `messages`, `approvals`) with
real foreign keys, alongside the append-only `events` journal from commit 5 — the durable log
stays authoritative; the tables are a queryable *projection* of it, explicitly documented as
rebuildable if they ever diverge (they can't, by construction, but the doc comment says so anyway
for the reader who's suspicious).

`domain/repository.rs`'s `DomainRepository` is where the actor pattern from Day 6/commit 5 pays
rent a second time: every mutating command — `upsert_task`, `create_worker`, `submit_run`,
`transition_run`, and eight more — runs through `append_and_apply`, one SQLite transaction that
appends the event, learns its assigned `sequence` from the rowid, updates the projection row, and
commits. A projection-update failure rolls the event insert back too. `domain/transitions.rs`'s
`check_transition` is the enforcement point for commit 13's lifecycle table — called *before* any
event is appended, so an illegal edge appends nothing at all, not even a "rejected" event.

Six of six `domain_repository.rs` tests, red since commit 14, go green in this commit. That's the
TDD cycle closing, visible in the git history rather than asserted in a commit message.

### 17. `feat(runtime): expose orchestration RPC methods` (c468073)

Task 3: `OrchestrationService` routes every Task 1 method to `DomainRepository` or a read-only
query closure (`service/query.rs`). Two decisions worth pulling out of the diff:

`db/actor.rs` grows a generic `DomainOp` command carrying a boxed closure
(`Box<dyn FnOnce(&mut Connection) -> Result<Value, DomainError>>`) — *not* a generic
"run arbitrary SQL" escape hatch on `DatabaseHandle`. The closure still only calls
`DomainRepository` methods; the genericity is in *which* typed command runs on the actor thread,
never in *what SQL* runs. This is the seam that lets `OrchestrationService`, `ApprovalService`,
and `CoordinationBroker` (three commits away) all share one actor without `DatabaseHandle`
growing a public surface wide enough to bypass the redaction/transaction discipline.

And `service/run_driver.rs`'s `RunDriver` trait, with its one production-shaped implementation
being a *fake*: `FakeRunDriver` drives `queued -> starting -> working` through the same domain
transitions a real adapter would use, and production `ServerConfig::run_driver` defaults to
`None`. `run/submit` without a driver returns `adapter_unavailable` —*after* the queued run is
durably committed, never before, and never by dropping the run. [ADR-0013](adr/0013-injectable-run-driver-seam-fake-by-default.md)
is why this milestone could ship the entire orchestration RPC surface, tools, and monitor without
a single real worker adapter existing yet: the seam is real, the implementation behind it is
deferred on purpose, and "no adapter" is a documented, tested, *durable* outcome rather than a
crash or a lie.

The commit message also owns two real bugs the new tests caught before anyone else did:
`submit_run` had bound `run.started_at` where `created_at` belonged, which would have violated a
`NOT NULL` constraint the moment it ran for real; and four call sites had typo'd `ProjectId::new()`
where `self.project_id` belonged, which would have silently corrupted event provenance — every
event from those sites would have carried a *fresh random* project id instead of the real one.
Both are the kind of bug that a type system can't catch (both sides type-check fine) and only a
test that asserts the *actual value*, not just "it compiled," will find. Worth noting for anyone
skimming this journal for "why does the test suite bother asserting exact IDs instead of just
`is_ok()`" — this is why.

### 18. `feat(extension): add orchestration tools` (16f9a23)

Task 4, and OMP finally gets something to *call*: `batman_task`, `batman_worker`, `batman_run`,
`batman_message`, `batman_approval`, `batman_reconcile`. Every tool's `execute` body is the same
four lines (`tools/shared.ts::callOrchestration`): call the RPC method, shape the result as
`{ content, details }`, or map a `JsonRpcRemoteError` to a non-throwing `{ code, message, data,
isError: true }`. No tool selects a worker, retries, mutates OMP's own todos, approves, or infers
lifecycle state — that discipline isn't a comment, it's a *consequence* of the tool body being too
thin to do any of those things even if someone tried.

The one design wrinkle worth its own paragraph, because the commit message spells it out and it's
a genuinely useful lesson: each tool's parameters use a **flat discriminated field**
(`op: "upsert" | "get"`, checked with a runtime `if`), not a Zod `discriminatedUnion`. The commit
message is blunt about why — combining `z.discriminatedUnion` with this codebase's generic
`ToolDefinition<TParams>` type hit a real TypeScript compiler limit ("excessively deep
instantiation"), and the flat-object-plus-runtime-dispatch shape sidesteps it with *zero* behavior
change, just a less fashionable type. [ADR-0014](adr/0014-flat-op-discriminator-over-zod-discriminated-unions.md)
exists specifically so the next person who reaches for `discriminatedUnion` here finds the
tombstone before they hit the same wall.

### 19. `feat(extension): reconcile OMP native agents` (bfd6620)

Task 5, and the one piece of this milestone that touches OMP's *own* state without ever writing to
it. `omp-native/events.ts` normalizes the installed `task:subagent:lifecycle|progress|event`
bus payloads into `OmpNativeAgentFact` — a status bucket (`working`/`succeeded`/`failed`/`lost`)
deliberately *distinct* from `RunState`, because these are parent-scoped facts BATMAN observes,
never a `Run` row BATMAN itself transitions.

`OmpNativeReconciler` coalesces non-terminal `progress` updates for 150ms (so a noisy stream of
"still running" doesn't spam re-renders) but lets every terminal event through *immediately* and
never lets a stale, still-in-flight coalesced update regress a fact that already went terminal —
a race between "the agent just finished" and "a progress update that started before it finished is
still in the coalescing window" resolves in favor of the truth that already landed.

`reconcileAcrossRestart(priorFacts, currentEpoch)` is the sharpest invariant in the whole
milestone: an OMP-native, parent-scoped agent that a *new* OMP process doesn't re-report becomes
`lost` — never `succeeded`, and never silently promoted into a runtime-scoped `Run`. This is the
project's answer to "what happens when the thing watching a process disappears and comes back": it
doesn't guess optimistically, it doesn't guess pessimistically either in the sense of pretending
nothing happened — it names the uncertainty and moves on. [ADR-0015](adr/0015-omp-native-facts-as-non-owning-mirror-lost-on-omission.md).

### 20. `feat(runtime): broker audited worker messages` (3172d99)

Task 6, the biggest single commit of the milestone (19 files, 2186 insertions), and it fills a
seam Foundation deliberately left open: `RejectAllWorkerVerifier` — the foundation default that
rejects every `workerMcp` connection outright, because there was nothing yet worth letting one
talk to. This commit builds what a supervised vendor process actually gets: `coordination/task`,
`coordination/peers`, `coordination/send`, `coordination/requestChild`,
`coordination/publishArtifact`, `coordination/reportBlocked`, `coordination/askPolicy` — a
worker-safe surface with **no** task-dependency, ownership, or merge mutation reachable from it,
enforced by registering these methods *only* in the `workerMcp` role table.

The trust mechanism is a scope token: `ScopeTokenStore::mint` binds a token to
`{ projectId, taskId, workerId, runId, vendorProcessIdentity, expiresAt }` the instant a vendor
process is (would be) launched; `verify(token, peer_pid)` checks the run/expiry AND that
`peer_pid` is a live descendant of the recorded vendor process, via a portable ps-based
parent-pid walk (`PidAncestryChecker` / `SystemPidAncestryChecker`) that explicitly reports
"unsupported" on platforms without trustworthy peer-process identity rather than accepting an
unverifiable reconnect silently. Token *bytes* are the `HashMap` key and nothing else — never
journaled, never logged, never in a `Debug` output. [ADR-0016](adr/0016-coordination-scope-tokens-bound-to-run-and-pid-ancestry.md)
picked this over a static shared secret (revocable but not scoped to a specific process) or mTLS
(scoped and revocable, but a lot of certificate-lifecycle machinery for a same-machine boundary
that already has Unix-socket UID admission doing the outer layer of the job).

`CoordinationBroker::send`'s delivery semantics are the other half of this commit:
**record-before-delivery** — commit `recorded` first (one durable event, one projection row), then
attempt delivery and commit the outcome. A crash between the two commits leaves a message
`sent`/`recorded`; `sweep_unacknowledged_as_unknown`, run once at startup after journal recovery,
settles anything left in a non-terminal delivery state to `unknown` — and *never resends
automatically*. [ADR-0017](adr/0017-record-before-delivery-message-semantics.md) chose "the
sender finds out delivery is uncertain" over "the runtime silently retries and maybe double-sends"
— for a message that might be `assign` or `cancel`, an unwitnessed duplicate is a much worse
failure mode than an honestly-reported "I don't know."

`domain/repository.rs::request_child`/`decide_child` complete the loop this milestone's
`coordination/requestChild` needs: the requesting run enters `waitingPeer`, records
`ChildWorkerRequested`, and *only the runtime* — never a worker, never Rust guessing — applies the
matching transition back to `working` once OMP answers through `coordination/child/decide`.

### 21. `feat: add correlated worker approvals` (534d3db)

Task 7. `ApprovalService::request` is the seam an adapter calls when a vendor process reports it
needs a human's sign-off: atomically create the request and transition the run
`working -> waitingUser` in one durable event — a decision *paused*, not a decision made. `decide`
enforces, in order: ownership (only the connected principal whose `instanceId` currently owns the
task may decide — a disconnected former owner, even if somehow still holding a socket, is
rejected); idempotency (an identical repeat decision is a silent no-op, never re-invoking the
adapter callback a second time); settled-run rejection (a decision cannot target an already
terminal run).

The sequencing inside `decide` is the part worth memorizing: **record the decision, then invoke
the callback** — never the other way around. On callback success, the run returns to `working`.
On callback failure, the decision is *kept* and the run is marked `protocolUnhealthy` instead of
asking the human again. [ADR-0018](adr/0018-approval-decided-before-callback-never-re-ask-on-failure.md)
is the reasoning: re-asking on a callback failure means the human might approve the same action
twice under two different approval IDs (confusing, and a real "did I actually agree to this"
audit gap), while "kept the decision, flagged the plumbing as broken" degrades gracefully and
leaves a clean signal for whoever's watching `protocolUnhealthy` to go fix the actual adapter.

`approval-ui.ts::showApprovalDialog` is the human-facing half — worker, requested action, *redacted*
arguments, policy reason, approval id — shown only when OMP's own policy marked the decision
`humanRequired: true`, and a dialog timeout leaves the request pending rather than picking a
default. `batman_approval`'s `decide` op checks that flag through `approval/list` before deciding
whether to show the dialog at all, so a model can't skip the human by simply not calling the tool
that would have surfaced them.

### 22. `feat(extension): render the embedded BATMAN monitor` (aabc950)

Task 8, and the milestone's UI. `monitor/model.ts::reduceEvent` is a pure function — one row per
`runId`, built from the `TaskEvent`/`WorkerEvent`/`RunEvent`/`RunFlagsEvent`/`MessageEvent`/
`ApprovalEvent`/`ChildEvent` variants, a no-op for any sequence not newer than what's already
applied (so replaying the same event twice, on reconnect, changes nothing), and structurally
incapable of letting a raw message payload or secret-classified content into the view — only
kind-based labels ever reach it. `render.ts` turns that state into the widget's concise lines (at
most ten rows, with `/batman status <runId>` as the always-available "no, really, show me
everything" escape hatch — a fuller view is a command away, never a silent truncation).

`controller.ts::registerMonitor` is where the "replay-first" idea from the roadmap becomes literal
code, and it's the single most important sentence in this milestone's design:
**there is no separate replay mode.** On `session_start`, read the last persisted sequence from the
session's own custom entry (`pi.appendEntry`), and call `client.subscribe(fromSequence, onEvent)`
— which itself drains `events/replay` first and *then* starts delivering live `events/event`
notifications, but every single event, replayed or live, flows through the exact same
`reduceEvent` call. [ADR-0019](adr/0019-monitor-is-one-reducer-over-replay-and-live-no-separate-modes.md)
is why: a second code path for "catching up" is a second place for a bug to hide, and a monitor
that behaves identically whether it's rebuilding six hours of history or reacting to one live
event is a monitor you only have to reason about once.

`compat.ts::assertCompatiblePiCodingAgentVersion` — a check that the installed
`@oh-my-pi/pi-coding-agent` falls in the pinned `[17.0.7, 18.0.0)` range this monitor's two
surfaces (`pi.appendEntry`, `ctx.ui.setWidget`) are verified against — is written here as a
*test-only* fixture, deliberately never called from `registerMonitor` itself. That restraint
turned out to be exactly half-right, and the next commit is the story of the half that wasn't.

### 23. `fix(runtime): broadcast committed events and authenticate as ompExtension` (49233a5)

This is the commit where "all eight tasks are implemented and every test passes" met "does it
actually work when a real `omp` binary loads it and a real model calls the tools" — and the answer,
on first honest attempt, was no, three times over. This journal exists partly to make sure that
sentence gets read, because a milestone's test suite passing and a milestone *working* are not the
same claim, and the gap between them is exactly what this commit closes.

**The first bug** was in the very restraint praised at the end of the last section: `compat.ts`'s
`import pkg from "@oh-my-pi/pi-coding-agent/package.json" with { type: "json" }`, called from
`registerMonitor` at extension-load time despite the doc comment's "test-only" intent — the
*intent* was right, the *code* still called it from production. That import resolves instantly
under `bun test` in this repo's own `node_modules`, and hangs forever the moment the real `omp`
binary — itself a compiled, bundled Bun executable with its own module graph — tries to resolve
that exact subpath. Bisected down to the bare import statement with no call at all; fixed by
moving the check to test-only code that actually stays test-only, and, since the check itself is
worth keeping for CI, rewriting it to read the peer's `package.json` via a plain filesystem walk
instead of Bun's module resolver.

**The second bug** was subtler and older: `ensureRuntime()` — Foundation's function, written back
in commit 7, when `batman_status` was the only caller and read-only was the correct role — still
authenticated as `display`. Six commits and six new mutation tools later, `index.ts::getClient()`
was caching and reusing that *exact same client* for every one of them. Every mutation failed
`-32601 method ... is not available to this client`, silently, because `display`'s method table is
a strict subset of `ompExtension`'s and nothing checks that relationship until a real call hits it.
Fixed by switching the shared client to `ompExtension` outright — safe for the one existing
caller, because a superset relationship, once you notice it, makes the fix obviously non-breaking.
[ADR-0021](adr/0021-shared-client-authenticates-with-the-union-of-required-roles.md).

**The third bug** was the deepest, and the one that would have kept biting quietly forever if the
smoke scenario hadn't been run for real: `domain/repository.rs::append_and_apply` stored the
*full* `EventEnvelope` into `event_json`, but `replay()` expects that column to hold only the bare
`RuntimeEvent` — so every `events/replay` call failed to deserialize the instant any mutation had
committed. And separately, worse: `Shared.events_tx` — the broadcast channel `spawn_subscription`
reads from — had a subscriber and **no publisher anywhere**. None of the fifteen-plus mutation
call sites across `OrchestrationService`, `ApprovalService`, `CoordinationBroker`, and
`RunDriverContext` had ever called `.send()` on it. Fixed both: storage writes the bare event now;
`Committed` carries the full envelope; `domain::{embed_envelope, take_envelope}` smuggle that
envelope across the `run_domain_op` closure boundary (which is constrained to return a plain
`serde_json::Value`, for reasons that go back to commit 17's generic `DomainOp`) so every service
can broadcast it after every commit. [ADR-0020](adr/0020-per-mutation-event-broadcast-is-not-optional.md).

Two regression tests were added, and their failure modes are worth remembering on their own:
`events_replay_round_trips_committed_mutation_events` failed cleanly against the pre-fix code.
`events_subscribe_delivers_live_notifications_for_orchestration_mutations` did not fail — it
**hung forever**, waiting on a notification that would never arrive, which is exactly what
happened live, in a real terminal, when this bug was first noticed as "the monitor shows nothing."
A test that hangs instead of failing is a worse developer experience but a *more honest*
reproduction of the actual bug, and that's the reason it's kept in this exact shape rather than
wrapped in a timeout that would turn a hang into a tidy red X.

Verified live, in that order — a real `omp` session against the fixed build upserts a task,
creates a worker, submits a run (`adapter_unavailable`, correctly, because no adapter is wired —
the run stays `queued`, not dropped), sends a message, and the embedded `/batman` widget reflects
every one of those mutations without a reconnect. Restarting `omp` against the same repository —
a fresh daemon, since the old one had already idle-timed-out — replayed the identical state from
the durable journal. All 222 Rust tests and 107 TypeScript tests passed, and this time "passed"
meant something, because the thing they were testing had just been driven for real.

### 24. `docs: document the orchestration extension milestone` (fd86ade)

The closing act, mirroring commits 11–12: every document written or extended to match code that
had just stopped moving under it — README's status line, eight new sections in `architecture.md`
(including §18, "Lessons from the smoke scenario," which is the same three bugs from commit 23
told as a permanent design note rather than a one-time fix), a full runnable smoke-test walkthrough
in `getting-started.md`, source-map and gotcha entries in `code-walkthrough.md`, and — because the
project's plan document had every checkbox still unchecked despite every task being done — a pass
through the Obsidian vault's plan and roadmap documents to make the paper trail match the commit
trail. This journal, and the ADRs in `docs/adr/`, are the next entry in that same trail.

## Part III — Worker Adapters: giving BATMAN something real to work with

The Orchestration Extension proved the shape could hold state durably. Part III's job was to
give that state *teeth*: a real contract every vendor harness implements, a process supervisor
every adapter shares, four real adapters (Claude, Codex, Copilot, OMP-RPC), a worker-coordination
MCP surface those adapters' own vendor CLIs can call into, and a conformance runner that decides —
per adapter, per scenario — which of a worker's *declared* capabilities OMP is actually allowed to
schedule against. Thirty-four commits, and the running discipline is the same one Part II
established for the domain layer: declare the contract in a trait before any adapter implements
it, and never let a capability reach OMP that a real scenario hasn't proven.

### 25. Journal, ADRs, and two documentation fixes (52587d4, 14c2f20, 0bab8e9, 20bc763)

Before touching a single adapter, four small commits settle the paper trail. `52587d4` is, quite
literally, the commit that wrote the first 24 sections of *this document* and the MADR-format
records under `docs/adr/` — the journal narrating its own origin is as good a proof as any that
"write the docs the moment the code stops moving" (Part I, commits 11–12) held past Foundation.
`14c2f20` fixes a runId gap and a session-topology error the smoke-testing walkthrough had been
carrying since commit 23. `0bab8e9` adds `git-town.toml`, the branch-workflow config this
project's own contribution flow runs on. `20bc763` splits a dedicated `manual-testing.md` out of
`getting-started.md`, the document every later hardening commit's "verified live" claim in this
journal ultimately points back to.

### 26. `feat(runtime): define worker adapter contract` (6e57787)

The seam every adapter implements: `probe/start/resume/send/respondToApproval/cancel/snapshot/dispose`,
object-safe, returning `AdapterFuture<T>` (a boxed future resolving to `Result<T, AdapterError>`),
parameterized by an `AdapterEventSink` passed into `start`/`resume` rather than being a trait
method itself. No method has a default body — every adapter must decide explicitly what each
operation means for it, even if that decision is "return `capability_unsupported`" — nothing here
silently no-ops.

`ProbeResult` is what an adapter *claims*: protocol kind, resume/steering/approvals/usage/nesting/
native-view/workspace-control/durability capability, each a closed strict enum where an unknown
wire value is a hard deserialize error, never silently coerced to a default. Declaring a
capability here is not the same as it being production-approved — the conformance runner built
later in this Part strips any capability whose fixture scenario failed before OMP ever sees it.
This is the same "declare, then prove" discipline commit 17's `RunDriver` seam established for
scheduling; this trait is where it repeats for adapters specifically.

### 27. `feat(runtime): supervise worker process groups` (8c3e1c8)

Every adapter launches its supervised vendor process through `supervisor/process.rs` rather than
calling `tokio::process::Command` directly, so every worker gets the same process-group,
output-bounding, and cancellation-escalation guarantees regardless of which adapter owns it.
`Supervisor::spawn` takes a `SpawnSpec` and returns a `ManagedProcess`; escalation is
SIGINT → SIGTERM → SIGKILL on configurable timings, and `RotatingCapture` bounds captured
stdout/stderr so a runaway or crashed worker's output is truncated, never unbounded.

`EnvironmentPolicy` repeats commit 4's discipline at the process-spawn boundary:
`allowed_env_names: Vec<String>` carries variable *names* only — there is no field anywhere in
this module that could hold an inherited variable's *value*, so a value can never reach the
worker profile snapshot, the durable journal, or a log line through this type, structurally, not
by convention.

### 28–31. The four adapters (f61908a, f6db711, 1e81a57, 1ad4e44)

Four commits, one shape repeated four times, each against a genuinely different vendor wire
protocol: `CodexAdapter` over `codex --app-server`'s JSON-over-stdio; `OmpRpcAdapter`, which
doesn't spawn a process at all but reuses the extension's own Unix-socket connection
(authenticated as `workerMcp`) to let a nested worker call back into OMP; `ClaudeAdapter` over the
installed `claude` CLI's `stream-json` mode; `CopilotAdapter` over `gh copilot`'s ACP
(Agent-Communication-Protocol) mode.

Every one of the three process-spawning adapters shares the same concurrency model: a single
background task owns the `ManagedProcess` exclusively once `start`/`resume` spawns it (its
`write_stdin`/`next_stdout_frame` both require `&mut self`, so no other caller may touch it
directly); `send`/`cancel`/`dispose` talk to that task through an internal `SessionCommand`
channel instead; `snapshot` reads a small `Arc<Mutex<..>>` of session facts the background task
updates as it normalizes frames. And every one draws the same line around what the default test
run may do: `probe()` is exercised for real against the installed CLI (version/auth-readiness
checks only) — never a model call. `start()`/`resume()`/`send()` are real, complete
implementations, but actually calling them would write a real prompt to a real vendor process's
stdin, which *would* invoke the model the instant the CLI reads it — so the default adapter test
suites never call them past their own pre-start guard clauses. A `#[ignore]`d `<adapter>_live.rs`
end-to-end test, gated on `BATMAN_LIVE_<ADAPTER>=1`, is what actually exercises the
spawn+stdin+reader-task path for each.

### 32. `fix(adapter): add omp-rpc host tools / host URI scheme support` (2d34035)

OMP-RPC's host tools need a way to say "this call is coming from a nested worker, not from OMP
itself." The fix: an `omp-rpc://<runId>/<workerId>/<method>` host URI scheme the adapter parses to
recover the run/worker/method it's being asked to act on behalf of — the piece that makes
`OmpRpcAdapter` usable as the vehicle for a *child* worker's coordination calls, not just the
top-level one.

### 33. `feat(runtime): wire all four worker adapters into adapter::mod` (31d849a)

The wiring commit: `adapter::mod.rs` now exports `claude`/`codex`/`copilot`/`omp_rpc`, and
`AdapterKind` is a real four-variant enum a later registry (commit 42) matches on to construct
whichever adapter a worker profile names. Nothing schedules against these yet — that's still
`FakeRunDriver` at this point — but the four adapters now exist as a coherent module, not four
independent commits nobody has connected together.

### 34–36. Coordination MCP: identity, schema, and a real subprocess (82e807c, 7c26e6e, b4c1e0a)

Before an adapter can inject worker-safe tools into its vendor process, the tools themselves have
to exist as an MCP server something can actually spawn. `82e807c` fixes a scope bug in
`coordination/send`: the sender identity was being read from the request payload instead of the
authenticated connection's own scope binding — the kind of bug that lets a caller claim to be
someone else simply by naming them in a field nobody cross-checks, closed by deriving identity
from the socket's own credentials every time, never from anything the caller wrote. `7c26e6e`
defines the MCP tool schemas (`batman_task`, `batman_send`, and their siblings) and an in-process
dispatch table mapping each to the matching `coordination/*` JSON-RPC method. `b4c1e0a` is the
part a vendor CLI can actually exec: `batcave coordination-mcp` — a stdio Model Context Protocol
server that reads `BATMAN_WORKER_SCOPE_TOKEN` from its inherited environment and *removes it
immediately* (never forwarded to anything this subprocess might itself spawn, because it spawns
nothing), connects back to the owner-only repository socket authenticated as `workerMcp`, and
proxies MCP `initialize`/`tools/list`/`tools/call` on stdio to the corresponding `coordination/*`
call over that connection. It never reads the SQLite database directly — every operation goes
through the same authenticated socket any other `workerMcp` client would use.

The bind-race this subprocess has to survive is documented, not papered over: a scope token is
reserved by `ScopeTokenStore::reserve_token` before the vendor process (and therefore, possibly,
this MCP subprocess) has started, and only *bound* to a real pid afterward — so
`connect_and_authenticate` retries only an `InvalidToken`-shaped rejection, for up to two seconds,
and lets every other rejection reason (`NoCredentialStore`, `OutsideAncestry`, `RunNotLive`) fail
immediately, because none of those are transient and masking them behind a multi-second retry
would only delay a real failure, never fix one.

### 37. `feat(runtime): add per-adapter coordination MCP launch helpers` (633c94e)

`mcp_config.rs`: the argv/env/config each adapter's command builder needs to inject
`coordination-mcp` into its supervised vendor process — `coordination_mcp_argv` (separate
arguments, never shell-joined, so no path can be split or injected by embedded whitespace),
`coordination_mcp_env` (only `BATMAN_WORKER_SCOPE_TOKEN`, nothing else added to the vendor's
environment), and `coordination_mcp_config_document` (the `{"mcpServers":{"batman":{...}}}` shape
both Claude's `--mcp-config` file and Copilot's `--additional-mcp-config` inline argument carry —
identical shape, different delivery). `codex_mcp_overrides` is the odd one out: Codex's
`-c key=value` overrides parse as TOML, not JSON, so this module also carries a from-scratch TOML
basic-string escaper — every value it embeds is escaped completely against the full control-
character table the spec requires, not just the two characters a filesystem path happens to use
today.

Every adapter's own native MCP/plugin/skill/hook discovery stays on throughout: nothing here ever
adds a flag that suppresses or replaces it, only one additional named server (`"batman"`)
alongside whatever the vendor CLI already loads from the user or project's own configuration.

### 38. `fix(runtime): reject worker-safe coordination calls once a run has settled` (e1f0898)

A run that has reached a terminal state must not accept any further coordination call — not
`send`, not `requestChild`, not `askPolicy`. Every coordination method now checks the run's state
before attempting anything, and a call against a terminal run returns a rejection
indistinguishable from a method that doesn't exist, never a "permission denied" that would leak
which runs exist to a caller who shouldn't be able to tell. This is the safety net that makes
commit 20's record-before-delivery semantics safe under the scope-token model: if a crash leaves a
message `sent`/`recorded`, the startup sweep settles it to `unknown`, and no further call can
target that run until it is explicitly restarted.

### 39. `feat(runtime): add AdapterMcpConfig reserve/activate helper` (fe6d4e3)

The lifecycle glue between the scope-token mechanism (commit 20) and the per-adapter launch
helpers (commit 37): `AdapterMcpConfig::reserve` mints a scope token bound to
`{projectId, taskId, workerId, runId, vendorProcessIdentity, expiresAt}` the instant a vendor
process is (would be) launched; the corresponding `activate`/`bind` step is what
`ScopeTokenStore::bind` does once the real pid is known. An adapter holds an
`Option<AdapterMcpConfig>` — `None` for a caller (chiefly existing tests) that never asked for
worker-MCP tools at all, so every existing constructor keeps compiling and behaving unchanged.

### 40. Injecting the MCP config into Claude, Codex, and Copilot (bfa8dc8, 1f46410, f6b624a)

Three commits, one already-designed shape (commit 37) landing in three adapters: Claude's
`build_mcp_injection` reserves a token and writes a `--mcp-config` file at owner-only `0600`
permissions, naming only the `coordination-mcp` command/args — never the token itself; Codex's
equivalent writes the same information as `-c` TOML overrides on the `codex app-server` command
line instead of a file; Copilot's writes it as an inline `--additional-mcp-config` argument. All
three delete their config artifact (file or none) once the session ends, and all three treat the
scope token's bytes as the only thing that must never be journaled, logged, or appear in a
`Debug` output — the vendor process's own environment is the token's one legitimate home.

### 41. Answering the host tool calls in OMP-RPC, and a formatting pass (167fddc, f55b36c)

OMP-RPC has no separate MCP subprocess to inject anything into at all — `omp --mode rpc`'s "host
tools" are invoked over the *same* RPC channel the adapter already owns (a `host_tool_call` frame
on its stdout, answered with a `host_tool_result` on its stdin), so `167fddc` wires that in-process
bridge to `CoordinationBroker::execute_tool_call` — the same dispatch table `coordination-mcp`'s
stdio proxy resolves to, just reached without a socket, because the runtime process making this
call is the vendor's own parent, never a descendant of it, and so could never authenticate over
the scope-token socket even if it tried (ancestry is checked in the wrong direction for that path).
`f55b36c` is a pure `rustfmt` pass over pre-existing coordination/approval files — no behavior
change, included here only because it's a real commit in `git log` and this journal's rule from
commit 11 is to never let its own list drift from the one `git` actually recorded.

### 42. `feat(runtime): add adapter registry and conformance runner scaffolding` (90aa259)

The `AdapterRegistry`: implements `RunDriver` (commit 17's seam) by resolving a run's immutable
worker profile, gating start on conformance-derived effective capabilities through an injected
`AdapterAuthorization`, constructing the matching `Adapter`, and owning it for the run's lifetime
in a run-indexed table. `AdapterAuthorization` ships two implementations: `FixtureAuthorization`
(a deterministic allow/deny toggle, tests only) and `DenyByDefaultAuthorization` (the production
default — denies every worker unless a development override is explicitly set, replaced later by
a real `PolicyEvaluator`). The commit's own doc comment is explicit that production callers "must
not ship a permissive production authorization implementation" — a rule the later M4 policy work
(commit 56) exists specifically to satisfy for real.

Alongside it, `crate::conformance`: `run_fixture_conformance(kind)` runs an adapter's fixture
scenarios (zero model calls, always safe) and returns a `ConformanceReport`;
`run_live_conformance(kind)` runs its live scenarios (real model calls, gated per adapter) and
returns the same report shape. This is the scaffolding every later "expand conformance" commit in
this Part fills in.

### 43. `feat(runtime): add adapters and conformance CLI subcommands` (d26e253)

`batcave adapters --json` (every registered adapter kind and its current conformance status) and
`batcave conformance --adapter <kind> [--live] --output <path>` (runs one adapter's suite and
writes the report). These are the only CLI surfaces that expose adapter or conformance data —
the extension's own monitor reads the daemon's state directly, never CLI output, so these two
subcommands exist purely for a human (or CI) to inspect the same facts from outside a running
session.

### 44. Expanding every adapter's fixture suite to fourteen scenarios (c79e0c3, 5f30a9c, 0e3fb11, a983081)

Claude, Codex, Copilot, and OMP-RPC each get a full conformance suite covering the Worker Adapters
plan's Task 8 scenario list verbatim: probe, read-only start/progress, isolated write, follow-up,
approval, every cancellation scope, session resume, vendor reconnect, runtime restart,
result/usage/artifacts, native discovery, redaction, managed-nesting rejection, and unexpected-
child observation — fourteen names, at the time these four commits landed. Not every scenario
applies to every adapter (`VENDOR_RECONNECT` is OMP-RPC-specific; a foreign adapter reports it
`passed: true` with a detail explaining it is not applicable, never silently omits it — omission
and "not applicable" have to stay distinguishable from a scenario nobody ran). This fourteen-name
list is not where it ends: a later commit (55, `aa25584`) removes two of them from the *required*
set once the underlying capability gaps turn out to be either permanent (a protocol wall) or
genuinely optional — `crate::conformance::scenario::ALL` is twelve names by the time this Part is
over, and this journal's honesty rule (commit 23) means saying so here rather than leaving the
"fourteen" claim uncorrected.

### 45. Proving it against reality: a bug fix, integration tests, and an honest catalog (7b5e065, 44fd31f, 7920453, bb60ccd)

Four commits closing out the conformance work the way commit 23 closed out Part II: by actually
running it. `7b5e065` adds CLI integration tests that spawn the real compiled `batcave` binary
against `adapters --json` and `conformance ... --output`, so the CLI's output is checked against
the daemon's real state, not just against itself. `44fd31f` documents, in `manual-testing.md`,
exactly which CLI version and which `BATMAN_LIVE_<ADAPTER>` gate each adapter's live suite needs,
so a human can run one without reading the source.

`7920453` is a real bug the live suite itself caught: Copilot's `resume` scenario was calling
`CopilotAdapter::resume` with a hardcoded, stale session id instead of the id `start` had actually
returned — a mistake that would always fail resume, silently proving nothing about session
persistence at all. Fixed by threading the real returned id through. Exactly the class of bug
this journal has flagged before (commit 17): both sides type-check fine, and only an assertion on
the *actual value* — not "it compiled" — catches it.

`bb60ccd` is the closing move: every fixture-mode scenario that still fails gets sorted into one of
four honest categories — a fixture-mode proof limit (a real live run resolves it today), a
protocol wall (only a future vendor protocol version could resolve it — Copilot's ACP v1 has no
child-observation event at all), a genuine implementation gap (a concrete fix shape exists), or an
environment dependency (needs a reachable vendor CLI or model selector this environment doesn't
have). Separating "worth fixing" from "genuinely can't be resolved short of a vendor change" in
writing is what lets the *next* milestone (Part IV) decide which of these to actually close instead
of re-discovering the same list from scratch.

### 46. `chore: add Serena project config and initialization scripts` (beb67d7)

Serena project configuration and initialization scripts, so the project-management tooling this
team uses can track BATMAN's own tasks and milestones the same way it tracks any other project —
no runtime behavior change, included for the same reason commit 41's `rustfmt` pass was: it is a
real commit, and this journal's list is the commit list, not a curated subset of it.

---

## Part IV — Hardening, Display, & Release: making it production-ready

Part III proved BATMAN could run real workers. Part IV's job was to make that trustworthy enough
to actually ship: real workspace isolation instead of a bare path, real terminal-multiplexer
display instead of a `String`, the structural gaps the plan itself had left open (deny-by-default
authorization *in production*, task content actually reaching an adapter, worker-coordination MCP
actually wired at construction time), layered configuration with org-level locks, a policy
evaluator that replaces the fixture authorization stub for real, and the release scaffolding to
build and publish a `batcave` binary at all. Forty-one commits, and — matching this journal's own
rule about not letting a claim outlive its evidence — this Part also names, plainly, the two
pieces that are still stubs at the end of it.

### 47. `feat(protocol): define workspaces and artifacts` (1272177, f58cd2d)

The wire types for workspace operations, defined the same way every other milestone in this
journal starts: vocabulary before logic. `InspectRequest`/`InspectResult`, `ApplyRequest`/
`ApplyResult`/`ApplyStrategy`, `LeaseMode` (`Exclusive` or `Shared`), `IsolationKind` (`Shared`,
`GitWorktree`, or `Copy`), `WorkspaceInfo`/`WorkspaceState`, and `Artifact`/`ArtifactId`/
`ArtifactKind`. `InspectResult` is designed to carry *evidence*, not an opinion: dirty file count,
untracked file count, recent commit ids, and a patch artifact — the same "durable proof, not a
narrated summary" instinct behind commit 5's redaction boundary and commit 20's audited messaging.

### 48. Workspace scaffolding: materialization, inspection, and a missing dispatch arm (a140979, 987f0e5, 2dac4c9, f607ec6, f08c07c)

Five commits standing up the workspace subsystem's skeleton before it does anything real:
`WorkspaceMaterializer`/`LeaseService` modules and their first tests; a batch of display-backend
test files staged ahead of the display work itself (commit 51); `WorkspaceInspector`/
`WorkspaceApplier` types wired into dispatch; a fixed match arm for the `workspace/*` and
`artifact/*` methods that the previous commit had left unreachable (a compile-time-safe version
of the same "route to `METHOD_NOT_FOUND`, never a panic" discipline commit 14 established for
orchestration methods); and the TypeScript protocol bindings regenerated to match. Nothing here
touches a real filesystem yet in a load-bearing way — that's the next commit.

### 49. Real isolation: git worktrees, file copies, and two symlink bugs (feb1648, 3f18e22, 54985d5)

`feb1648` makes `IsolationKind::GitWorktree` and `IsolationKind::Copy` actually materialize a
working directory: a real `git worktree add` for the former, a real recursive copy (excluding
`.git`) for the latter. Two bugs surfaced immediately, and both are the kind that only exist
because symlinks are a filesystem feature most copy logic gets wrong on the first pass: `3f18e22`
fixes symlink *escape* detection in `WorkspaceMaterializer::validate_path` — a path containing `..`
or resolving through a symlink to somewhere outside the lease root has to be rejected before any
copy or worktree operation touches it, not discovered after the fact. `54985d5` fixes the copy
operation itself: `CopyIsolation::copy` was following symlinks and copying their *targets*'
contents instead of recreating the symlink as a symlink — fixed by checking
`std::fs::symlink_metadata` (which does not follow the link) *before* any `is_dir`/`is_file` check
(which does), so a symlink is always recreated as a symlink, and only a resolved directory or
regular file is ever actually recursed into or copied.

### 50. `feat(workspace): implement real inspect/apply with artifact store` (211811f)

`WorkspaceInspector` now runs real `git diff`/`git status` commands and persists the resulting
patch to `ArtifactStore`, an in-memory (optionally on-disk) content store keyed by `ArtifactId`
with bounded, base64-chunked fetch (`fetch_chunked(id, offset, length)` never loads a whole large
artifact into one response). `WorkspaceApplier` fetches an artifact back out and applies it via
`ApplyPatch` (a real `git apply`) or `CherryPick` (a real `git cherry-pick`), validating the
caller's `expected_target_revision` against the workspace's actual current HEAD *before* mutating
anything and returning `STALE_REVISION` on a mismatch rather than applying against a workspace
that has moved out from under the caller since it last inspected.

### 51. `feat(display): implement display backends with Herdr/Tmux/Terminal` (199011a)

The first cut of the display subsystem: a `DisplayBackendTrait` (`activate`/`status`/
`is_available`), three implementations (`HerdrDisplay`, `TmuxDisplay`, `TerminalDisplay`, the
last one always available as a fallback), a `DisplayRegistry` holding all three, and a
`DisplaySelector` with ordered fallback. At this stage compatibility gating is a bare version
floor (Herdr ≥ 0.1.0, tmux ≥ 3.0) and `activate` does little beyond confirming the backend is
usable — real pane-level operations arrive later, inside the M2/M3 gap-closure squash (commit 55).

### 52. `feat(runtime): complete Task 9 - Terminal adapter with registry integration` (071c9bb)

The fourth "adapter," and the odd one out: `TerminalDisplay`-backed `TerminalDegraded` control for
when a structured adapter's protocol has gone unhealthy and the only remaining option is
terminal-screen automation. `CommandRunner` is injected (never a bare `std::process::Command`
call), so the adapter's own tests never spawn a real terminal multiplexer, and `AdapterRegistry`'s
`run_one` gains the match arm that resolves a `TerminalDegraded` worker profile to this adapter
instead of one of the four `AdapterKind` variants — `TerminalDegraded` was defined back in commit
27's `StartupOptions` enum specifically as the identity that wraps *any* underlying harness rather
than replacing one of the four reserved kinds, and this is the commit that gives it a real
implementation.

### 53. `feat(runtime): wire AdapterRegistry into daemon lifecycle` (f64a61d)

The moment `AdapterRegistry` stops being test-only scaffolding and becomes what `lifecycle::serve()`
actually constructs `ServerConfig::run_driver` from — with `FixtureAuthorization { allow: true }`,
not yet the deny-by-default production policy (that swap is commit 55). Alongside it:
`RunDriver::send_follow_up`, the seam a live message can be forwarded through to an already-running
adapter instance rather than requiring a second `start()` call, and a rename of every adapter's
artifact tracking from `Vec<String>` to `Vec<serde_json::Value>` so a structured artifact (not just
a bare path) can travel the same field. `TODO.md` gets a matching trim — a gap this
commit just closed no longer belongs on the open list.

### 54. Redesigning `batman_task` around a natural-language description (8cf3e72, 975b710)

Two commits undoing a design mistake the same week it shipped. `8cf3e72` is the first patch:
default `ownerClientInstanceId` to `extCtx.sessionManager.getSessionId()` instead of requiring the
model to supply an OMP-internal session id it has no reason to know. `975b710` goes further and
removes the requirement entirely: `batman_task` now accepts a single natural-language
`description`, and the tool itself generates `taskId` (a fresh UUIDv4), resolves
`ownerClientInstanceId` from the session, and defaults `revision` to `0` — the model describes
*what to do*, and the extension owns every protocol detail underneath it. This is the same lesson
commit 18's flat-discriminator ADR taught about tool ergonomics, applied one layer up: a tool
whose parameters mirror the wire protocol exactly is easy to implement and unpleasant for a model
to actually call correctly, and unpleasant-to-call is a defect independent of whether the
implementation underneath it is correct.

### 55. `refactor(conformance): drop OMP-RPC artifact and Copilot subagent gaps from required scenarios` (aa25584)

This is Part IV's version of commit 23 — a single merged pull request, eight phases, that goes
back over "the plan says it's done" with the same fresh eyes commit 10 brought to Foundation, and
finds four real structural gaps still open underneath a green test suite.

**Phase 2 (A4):** `DenyByDefaultAuthorization` replaces `FixtureAuthorization` in
`lifecycle::serve()` at last — every worker is denied unless `BATMAN_DEV_ALLOW_ALL_WORKERS=1` is
explicitly set, closing the exact gap commit 42's own doc comment had flagged as a "must not ship"
item three commits earlier in wall-clock time.

**Phase 3 (A5):** `RunSpec`/`RunDriverContext` grow an optional `prompt: Option<String>`, and it
is threaded all the way to `StartSpec::prompt` — a run's initial content actually reaches its
adapter at start time now, not just at the database-projection layer. `OrchestrationService::message_send`
is wired to `RunDriver::send_follow_up` (commit 53's seam) for live delivery to an already-running
adapter; critically, a failed follow-up delivery — the normal case for a `queued` run with no
adapter running yet — never fails the RPC call or the durably recorded message, it journals a
`Diagnostic(follow_up_delivery_failed)` event instead. The same "don't drop the run, don't lie
about the outcome" instinct from commit 17's `adapter_unavailable` design, one layer further along
the run's life.

**Phase 4 (A6):** `AdapterRegistry::new` now accepts `Option<AdapterMcpConfig>`, threaded into
every Claude/Codex/Copilot adapter it constructs from a resolved `batcave` binary path via
`current_exe()`. OMP-RPC's in-process bridge needs a `CoordinationBroker` instead, which cannot
exist yet at registry-construction time (the real broker only exists after `Server::bind` returns,
which is necessarily *after* the registry must already be handed to `ServerConfig::run_driver`) —
so `AdapterRegistry::set_broker` is a documented post-construction setter for exactly that
ordering constraint, not a design smell. `lifecycle::serve()` also stops unconditionally rejecting
every worker-MCP reconnect: a real `ScopeTokenStore` becomes the server's `worker_verifier`,
replacing the Foundation-era `RejectAllWorkerVerifier` default in production for the first time.
This phase also fixes a message-duplication bug where both `AdapterAuthorization` implementations
had double-wrapped their own rejection string through `RegistryError::AuthorizationDenied`.

**Phase 5:** OMP-RPC's `ApprovalsCapability::Observable` claim gets backed by real state.
`extension_ui_request` confirm/select frames now produce a `PendingApproval`, surfaced through
`snapshot()`'s `state_summary` — never through the event sink, since `AdapterEventPayload` has no
approval variant for this path. Every other `extension_ui_request` (`setWidget`, `notify`, ...)
still produces zero events, deliberately; `respond_to_approval` stays `capability_unsupported` by
design, because no `extension_ui_response` wire path exists to answer one — a capability the
adapter is honest about *not* having, rather than one it silently drops a call against.

**Phase 6:** `batcave monitor` — the CLI-side twin of the extension's embedded widget (commit 22).
Connects as a `display` principal, replays every event from sequence 0, renders one line per
contributing run event via a reducer (`apply_and_render`) that deliberately mirrors the embedded
TypeScript monitor's own `reduceEvent`/`eventPatch`, then follows live events until interrupted.
`--run-id` filters to one run; omitted, it renders every run in the project. No extra redaction
logic is needed here — `EventEnvelope`'s fields are already fully sanitized before reaching the
wire (commit 5's boundary), so there is no raw classified content at this layer left to filter.

**Phase 7:** Herdr and tmux get real pane-level fidelity, replacing commit 51's bare version-floor
gate. Herdr's compatibility check becomes a real `herdr status --json` probe requiring *exact*
client/server protocol equality (cached 5 seconds), grounded against the installed Herdr 0.7.5
binary's real output shape; pane operations (split → run → move/close → report-agent) are
sequenced so a partial failure cleans up only the pane just created, and ownership is tracked
in-memory so this backend never closes a pane it didn't open. tmux gains real pane creation via
`new-window`/`split-window -P -F` (tmux's own print-format convention — no output parsing needed
to recover a created pane id) and now additionally requires a real, already-running tmux session
before reporting itself available, never starting an ambient server as a side effect of a mere
check.

**Phase 8:** A guard test for Copilot's permanent gap. `unexpected_child_observation` is
unresolvable *only* while `COPILOT_MAX_ACP_PROTOCOL_VERSION == 1` — ACP v1 genuinely has no
`session/update` variant for a vendor-spawned subagent. The test inspects `normalize.rs`'s own
source text and fails, with a clear message, if that constant is ever raised without a matching
`NestedWorkerObserved`-producing branch also landing — manually verified by temporarily bumping
the constant to 2 and confirming the guard fires, then reverting. The final `refactor` step then
drops `RESULT_USAGE_ARTIFACTS` and `UNEXPECTED_CHILD_OBSERVATION` from the *required* scenario
list (`crate::conformance::scenario::ALL` goes from fourteen names to twelve) — not because either
gap got fixed, but because both are now honestly optional rather than falsely required, closing
the "genuine implementation gap" and "protocol wall" categories commit 45's honest catalog had
just finished sorting them into.

### 56. `feat(policy): merge immutable runtime configuration` (2ce75b4)

M4's Task 1: layered YAML configuration — org → repo → user → per-run params — with strict
precedence (higher layers win) and org-level field *locks* that prevent any lower layer from
overriding a specific value no matter how it's spelled. `ConfigLayer`'s ordering and
`RuntimePolicy`'s SHA-256 fingerprint are the load-bearing pieces: the fingerprint means two
runtimes that resolved configuration from the same layered inputs can prove, without comparing
the documents byte-for-byte, that they landed on the identical effective policy. Unknown top-level
keys fail closed with line/column diagnostics — the same `deny_unknown_fields` promise from
commit 2, now enforced at the YAML-parsing boundary instead of the JSON wire boundary.

`PolicyEvaluator` (in a sibling `policy` module) is the real `AdapterAuthorization` implementation
commit 42's doc comment had been waiting for: model allowlist enforcement, a concurrency ceiling
using `AtomicUsize::fetch_update`'s compare-and-swap loop (eliminating the TOCTOU race a naive
check-then-increment would have between two concurrent `authorize()` calls), nested-worker policy,
and security-pattern enforcement. `EffectivePolicy` (commit 27's environment-allowlist type) and
`RuntimePolicy` (this commit's org/repo/user-merged policy) are two distinct types on purpose — the
commit's own module doc calls out that the similar names describe unrelated concerns and are never
interchangeable, a naming collision worth documenting rather than silently avoiding by renaming
one of them into obscurity.

### 57. `feat(security): add org-configurable redaction rules and audit module` (f503b9a)

`OrgRedactionRule`: a compiled regex plus a human-readable id (parsed from an inline `# comment`
after the pattern string, or generated from the pattern's index), loaded from an org configuration
document's `security.patterns` array and applied *alongside* — never instead of — the built-in
rules `Redactor::sanitize` already enforces from commit 5. An organization can add redaction
coverage; it can never use this mechanism to remove any of the built-in coverage, because the
built-in rules are compiled into `Redactor` itself and this module never touches that code path.

The `audit` module lands here too: `Export` (JSONL export of events, the implementation behind
`batcave audit export`) and `Retention` (event pruning by age). The commit message is direct about
scope: this is where the module is *introduced*, and `Retention::prune` is, as of this commit,
still a documented stub that returns `Ok(())` without touching the database — real pruning logic
is explicitly deferred, not silently assumed to already work.

### 58. Recovery and Doctor: two commits, two stubs, said plainly (9f51832, 31088c0)

`RecoveryCoordinator` and `Doctor` both land as real types with real public APIs and real doc
comments describing what they *will* check — a database-connectivity probe, state-directory
accessibility, rollout-gate resolution, adapter availability, configuration validity — but neither
commit message hides what's actually inside: both are explicitly titled "stub implementation
ready for full integration." `RecoveryCoordinator` carries `#[expect(dead_code)]` on its own
struct definition, and as of this journal, neither type is constructed anywhere in
`lifecycle::serve()` — a crash leaves a run in a non-terminal state exactly the way commit 7's
socket-disappearance-means-journal-closed design always intended it to, but nothing yet walks the
journal afterward to reconcile it, and `batcave status --recover`'s stated recovery path is not
yet wired to this coordinator. `Doctor::check_database`/`check_state_dir`/`check_configuration`
each carry an explicit `// This is a stub implementation` comment naming exactly what a full
version would additionally do. This journal's rule from commit 23 is to say when something is
real and proven versus merely scaffolded; for these two modules, as of the commits documented
here, the honest statement is: the shape is real, the checks are not yet.

### 59. `feat(release): add CI workflows and xtask for release artifacts` (ebddc6e)

`.github/workflows/release.yml`: a tag-triggered (`v*.*.*`) matrix build across macOS
(aarch64/x86_64), Linux (x86_64-gnu), and Windows, each producing a `batcave-<target>` binary
uploaded as a build artifact and, on an actual version tag, attached to a GitHub Release via
`softprops/action-gh-release`. The commit's own message claims SHA-256 checksums for every
artifact; the workflow committed here packages and uploads binaries but does not yet compute or
publish those checksums as a separate step — a gap this journal's own honesty rule means noting
rather than repeating the claim unchecked.

The commit also adds a *second*, standalone `xtask/` crate at the repository root — distinct from,
and duplicating part of, the `crates/xtask` package tooling commit 3 and commit 9 already built.
This duplicate scaffolding survives for the rest of Part IV and is only removed in the very last
commit of this journal (63, `037bda2`), which is the more honest place to tell that story: a
mistake made here, caught and fixed several commits later.

### 60. `feat(conformance): add TypeScript conformance gates runner` (2b6b53e)

The TypeScript-side counterpart to `batcave conformance`: `packages/extension/src/conformance/index.ts`'s
`runConformance(config)` shells out to the compiled `batcave` binary, supports both `fixture` and
`live` modes, and produces a `ConformanceReport` shaped for CI consumption; `formatConformanceSummary`
renders it as a human-readable pass/fail table. This is the piece that lets a CI pipeline invoke
the same adapter conformance suite Part III built, from a `bun` script, without a developer
manually running the Rust binary and parsing its JSON by hand.

### 61. `feat(runtime): implement all CLI commands` (ddc42e2)

The commit title is exact about what it fixes: `cli.rs`'s `serve`/`status`/`stop`/`monitor`/
`schema`/`audit export` subcommands are wired to their real `lifecycle.rs`/`audit::Export`
implementations for the first time as a complete set — `serve` acquiring the single-instance lock
and starting the IPC server, `status` querying `runtime/status` and printing JSON, `stop` signaling
a live runtime and waiting for socket removal, `monitor` replaying and following events as a
`display` principal, `schema` printing the canonical JSON Schema, and `audit export` delegating to
commit 57's `Export` module. `manual-testing.md` gains a full environment-variables section
(`BATMAN_STATE_DIR`, `BATMAN_ORG_CONFIG`, `BATMAN_DEV_ALLOW_ALL_WORKERS`, `BATMAN_LIVE_<ADAPTER>`,
`OMP_BATMAN_BINARY`) and documents exactly where the state directory and configuration files
resolve to on disk. All 144 library tests and both monitor CLI integration tests pass against the
now-complete surface.

### 62. Sixteen documentation commits, closing the same way every milestone has (cea94b7, baa214c, d8088d2, 6ee06fd, 275eb72, b92e38d, 1f960ac, 114168d, 9f1f313, 358d45f, 73fe828, e7b4f7f, ed1458e, 60eed81, 47904f4, 3e81422)

Sixteen commits, every one of them documentation, mirroring commits 11–12 and commit 24's closing
acts one more time: `docs/m4-hardening-release.md` (a 609-line API reference covering
configuration, security, recovery, doctor, release, and conformance, plus a migration guide and an
explicit "known gaps" list — the same honesty this journal has been asking for, written into the
project's own docs this time); `getting-started.md` rewritten twice over (once broad, once
specifically to cover every M4 feature) and then corrected three times for smaller factual slips
(Homebrew install steps, `bun install` over `npm`/`yarn`, and a wrong claim that Git comes
pre-installed on the target platforms); `CONTRIBUTING.md` added and then the same pre-installed-Git
claim removed from it too; `TODO.md` opened at the repository root to track a real, specific
feature request (org config as a URL, not just a file path) rather than losing it to a chat log;
`358d45f` fixing that exact ambiguity in the org-config documentation before `TODO.md` even
finishes making the case for supporting the URL form; and `architecture.md` restructured twice —
first two incremental updates to reflect the implementation status accurately, then a full
rewrite onto the C4 model (Context, Containers, Components, Code), trading roughly 580 lines of
one structure for 320 of another while explicitly preserving every technical claim, because a
document that describes the *finished* design (as `architecture.md`'s own stated purpose, set in
Part I's closing act, requires) is more useful organized by zoom level than by writing order.
`code-walkthrough.md` and `manual-testing.md` each get a final pass to match the actual M4
codebase rather than the plan that preceded it.

### 63. `docs: update rust-primer.md with verified codebase references, remove dead xtask/ directory` (037bda2)

The last commit in this journal's history, and it closes two loose threads at once. The
`rust-primer.md` update re-verifies every source reference the primer makes against the codebase
as it actually stands at the end of Part IV, rather than as it stood when each "Day" was first
written — the same "docs describe what's actually there" discipline every milestone's closing act
has repeated. And the root-level `xtask/` directory commit 59 introduced — a duplicate of
`crates/xtask`'s release-packaging role that had sat unused for the rest of Part IV — is deleted
outright. Not a deprecation, not a redirect: the dead duplicate is simply gone, and
`crates/xtask` (the one commit 3 and commit 9 actually wired into `bun run generate` and the
platform-package loader) remains the only implementation of that role. A fitting note to end on:
the same rule this journal has followed since commit 10 — finding your own mistake and removing
it outright is not a footnote, it's the work.

---

## Part V — Distribution honesty: finding the one true install method

Part IV closed with `037bda2`, and the very next commit (`1ee09b9`) wrote the first version of
this journal — the one that narrated Parts I–IV as "ninety-nine commits, four milestones." That
commit's own message is worth recording here because it is this document talking about itself:
"every commit hash from git log is referenced; every ADR link resolves; stubs are documented as
stubs, not working code." Part V picks up immediately after, and its very first commits are a
documentation-accuracy sweep discovering exactly the kind of drift that discipline exists to catch
— module counts wrong, dangling section references, a corrupted file. The rest of the Part is a
longer, more interesting version of the same instinct: repeatedly trying an install method, finding
it doesn't actually work end-to-end, and removing it rather than leaving it half-documented.

### 64. A documentation-accuracy sweep, immediately (7898f25, 042b8ab, 333163a, 14f0e2a, 374447c, 984b221)

Six commits, all docs, none of them adding a feature — the project checking its own homework right
after writing it down. `7898f25` records four adapter conformance gaps straight from test output.
`042b8ab` merges `known-gaps.md` into `known-limitations.md` and deletes `m4-hardening-release.md`
outright as redundant with doc comments already in the source. `333163a` fixes two module-count
typos this journal itself would have inherited (`crates/protocol`: 13 → 14; `crates/runtime`: 16 →
18, both undercounts that had missed newly-added modules) and updates the runtime file map to
match the actual directory. `14f0e2a` fixes a real omission in `architecture.md`'s Level 3
diagram: the `config`/`policy` modules existed in the code but not in the picture, and the fix is
careful to record that `PolicyEvaluator` implements `AdapterAuthorization` but is **not yet wired**
into production (`ServerConfig::default()` still used `DenyByDefaultAuthorization`) — accurate for
this exact commit, a claim Part VIII later needs to update again as the wiring changes.

`374447c` and `984b221` are the two commits worth reading in full if you want to see what
"describe the system as it stands, not as it was written" actually costs when it's skipped even
once. `374447c` finds that an earlier documentation edit had **literally injected the elision
markers a file-reading tool leaves behind** (`…`, `331:`, `355:`) as if they were real file content
— corrupting `getting-started.md`'s Testing/Troubleshooting/Contributing/License sections outright
— alongside a false claim that BATMAN was MIT-licensed when `Cargo.toml` actually said
`license.workspace = true → "UNLICENSED"` at the time, and three fabricated support channels
(`docs.batman.dev`, a Discord, a support email) with no grounding anywhere in the repository.
`984b221` finds that `architecture.md` had been rewritten onto the C4 model (this journal's own
Part IV, commit 62) with zero numbered `§N` sections left in it, while five other documents —
`engineering-lessons.md`, `code-walkthrough.md`, `rust-primer.md`, `known-limitations.md`, even
`README.md` — still cited `§4` through `§18` as if that structure still existed. Every dangling
reference is redirected to what actually exists (mostly `engineering-lessons.md`'s own anchors and
the matching ADRs), and the same pass catches a real diagram bug — two nodes referenced in mermaid
edges but never declared in the subgraph, rendering unlabeled — plus five extension files missing
from both the diagram and the key-components list. Two commits, zero new features, and together
they are the best argument in this journal for why "the docs describe what's actually there" has
to be re-checked, not assumed to hold once it's been true.

### 65. Legal and cosmetic housekeeping (1eb4b33, 7d7cc1f, 3fd3cb2, d5b9af4)

Four small commits: an acronym-expansion formatting fix, a `.gitignore` entry for macOS's
`.DS_Store`, logo/favicon assets in two color variants, and — the one with actual consequence —
`d5b9af4` adding a real `LICENSE` file (MIT, Oh My Pi copyright) and flipping the workspace
`Cargo.toml` from `license = "UNLICENSED"` to `"MIT"`, which every sub-crate inherits via
`license.workspace = true`. This is the fix that makes `374447c`'s corrected claim (three commits
earlier, "BATMAN is *not* MIT-licensed yet") retroactively become true — the kind of ordering this
journal's honesty rule cares about: the correction came first, the fact catching up to it came
second, and neither commit pretends otherwise.

### 66. Six attempts at "how does a user actually install this" (7db3edd, 8495889, 9124642, f34605d, c21f32a, fea66b6)

This is where the saga starts, and it's worth reading as a sequence rather than six independent
commits, because the sequence *is* the point. `7db3edd` rewrites `README.md` for first-time
visitors — a "Why BATMAN?" section, a 5-minute get-started path, and (for the first time) a
"Known Limitations" section stating plainly what doesn't work yet. `8495889` splits Installation
into "For users" (Homebrew or pre-built binaries) and "For developers" (build from source) — a
reasonable-sounding split that `9124642`, one commit later, discovers describes infrastructure that
doesn't exist: no Homebrew formula, no GitHub Releases, no pre-built binaries, full stop. `9124642`
fixes the claim to be honest that users currently must build from source too. `f34605d` then
*builds* the Homebrew formula the doc had promised (`Formula/batman.rb`, platform detection for
four targets) — but notes explicitly that GitHub Releases must exist before the formula's tap can
actually resolve anything, so it's still not usable yet. `c21f32a` is a one-line fix inside that
formula: the GitHub owner was hardcoded to `can1357` (the upstream OMP author) instead of
`nikolasd` (this repository's actual owner) in both the formula and the README — the kind of typo
that would have made the formula 100% non-functional for anyone who tried it, caught before anyone
did. `fea66b6` adds a `curl | bash` install script as a second parallel path, explicit that this
one is meant to actually work once releases exist, not a placeholder.

### 67. Building the release pipeline the install methods above were waiting on (c759a19, e298fb1)

`c759a19` adds a `Publish` subcommand to `xtask` that reads the version from
`packages/extension/package.json`, creates a `v<version>` git tag, and pushes it — the one command
meant to trigger `release.yml`'s existing binary-build-and-publish pipeline. `e298fb1` documents
that command in the README. Both commits are straightforward; the interesting part is what happens
to this exact command four commits later.

### 68. Realizing "runtime-only" was never the actual requirement (4eb3db1, b6e1e1d, b773cb6)

`4eb3db1` notices that everything built so far (`install.sh`, the Homebrew formula) installs only
the `batcave` **runtime** binary — a user still has to separately install the OMP **extension**
themselves, which was never actually documented as a step. Rewritten to install both: the runtime
to `~/.batman/bin/batcave`, the extension to `~/.batman/lib/node_modules/@satori/batman`, no root
privileges required, uninstall reduced to `rm -rf ~/.batman`. `b6e1e1d` goes further and makes a
**local** variant (`install-local.sh`) that works *right now*, on this machine, without any
published release at all — copying the locally-built binary out of
`packages/batman-darwin-arm64/bin/batcave` and `bun add`-ing the extension from the local
checkout — because every method built in commits 66–67 still depended on a release that had never
actually been published. `b773cb6` is the honesty pass on top: the README is rewritten to state,
without hedging, what works **right now** (the local installer, on macOS ARM) versus what's
described for a future that doesn't exist yet (GitHub Releases, Homebrew) — closing a gap where an
earlier version of the doc implied things worked on a fresh clone that, in fact, immediately failed
without a prior build step.

### 69. The one true method, arrived at by elimination (4606fde)

The commit that ends the saga, and it's worth reading in full for how thoroughly it closes the
book on everything commits 66–68 built. Verified against a live `omp` binary rather than inferred
from its `--help` text: `omp install <npm-spec>` (an alias of `omp plugin install`) installs to
`~/.omp/plugins` — a user-owned directory, no root required — and it resolves the extension
package **and** its matching `@satori/batman-<platform>` leaf package (containing `batcave`)
*together*, via the existing npm `optionalDependencies` mechanism this project had already built
for exactly this purpose back in Foundation (commit 9, [ADR-0010](adr/0010-platform-binaries-as-npm-optional-leaf-packages.md)).
One command, both halves, registered for automatic discovery on every future `omp` launch — no
`--extension` flag needed, unlike every prior approach in this saga. `omp plugin uninstall` /
`omp plugin upgrade` give a real, symmetric lifecycle for free.

This supersedes and deletes every previously-proposed method in one commit: `Formula/batman.rb`,
`scripts/install.sh`, `scripts/install-local.sh`, and `xtask`'s `Publish` subcommand (commit 67 —
tagging is two plain git commands, documented in the README, not worth a bespoke wrapper). What's
added instead is the plumbing `omp install` actually needs to work: `publishConfig.registry` on the
extension and all four platform leaf packages (a placeholder URL, documented as a placeholder, not
claimed functional), `.npmrc` scoping the `@satori` npm scope to that registry, and
`release.yml` repurposed from uploading GitHub Release binary assets (which fed the now-deleted
paths) to building all four platform binaries, assembling leaf packages via `xtask package`, and
`bun publish`-ing all five packages to the private registry on tag push — catching two pre-existing
bugs in that workflow file along the way (a renamed GitHub Action, and a `--access public` flag
that had been silently overriding the extension's own restricted `publishConfig`). The README is
rewritten a final time around three sections that actually match reality: Installation (one
method), Development (contributor build-from-source), Publishing (tag + CI, for maintainers) — no
more "For users"/"future" sections narrating infrastructure that doesn't exist.

### 70. Closing the loop: a real contributor setup and doc reconciliation (278af15, b9c16fc, e5b431a, c450639, 4d50bb8)

`278af15` removes leftover Serena MCP configuration and scripts — unrelated tooling that had
drifted into the repo. `b9c16fc` answers a question implicit since Foundation: is there a real,
single-command setup for a *contributor* (as opposed to an end user)? No — `bun install` only
bootstraps the JS workspace; nothing guaranteed the pinned Rust toolchain was present. Fixed with
`scripts/setup.sh` (verifies `cargo`/`bun` are on `PATH`, *warns* rather than silently building
against a version mismatch when `rustup` isn't managing the toolchain — deliberately not assuming
`rustup`, since this was verified on a machine with Rust installed directly via Homebrew, no
`rustup` at all) and `bun run setup` wiring it into `package.json`. The same commit fixes a stale
`github.com/your-org/batman` placeholder in `CONTRIBUTING.md` that had never been updated to the
real repository, and reconciles its Setup section to the same command so the two docs stop
drifting from each other. `e5b431a` propagates the same "two install methods, name them explicitly"
discipline into `getting-started.md` and `manual-testing.md`, replacing three loose manual build
commands with the verified `bun run setup` / `bun run build` pair everywhere. `c450639` adds a
design spec for the monitor-widget work Part VI covers next. `4d50bb8` removes `scripts/install.sh`
outright — it duplicated `omp install`'s own resolution logic and, on inspection, had three real
bugs (a checksum check broken by whitespace-sensitive grep against pretty-printed JSON, a version
precheck that queried the wrong package unauthenticated and picked the *oldest* published version
instead of the latest, and a hardcoded `/usr/local/bin` target with no writability fallback) — this
journal's running theme of "if a mistake is yours, remove it outright, don't deprecate it" holding
one more time. The same commit fixes a dead anchor link, an unclosed code fence that had been
silently swallowing half of `README.md` into a malformed code block, adds the `bun install`/
`bun run build` steps `release.yml`'s publish job had been missing (verified via
`bun publish --dry-run` before and after — without this, a real release would have published the
extension package missing the exact `dist/index.js` file its own `exports` field points at), and
moves the Publishing section out of the user-facing README into `CONTRIBUTING.md`'s new Releasing
section, where a maintainer-only, `SATORI_NPM_TOKEN`-gated procedure actually belongs.

## Part VI — The widget gets a border

A short, self-contained Part: six feature commits and two real bugs, entirely inside the embedded
monitor's rendering layer. Part II (commit 22) shipped the monitor as plain text lines; this Part
gives it a bordered box, per-state icons and colors, and finds two genuinely subtle rendering bugs
in the process of doing it.

### 71. Design-first, again (bf7ec0f, 3b1a8f1)

Two documentation commits before any rendering code: `bf7ec0f` simplifies the widget's border
design down to hand-assembled strings (rejecting a fancier approach in favor of one that's easy to
reason about character-by-character — exactly the kind of decision that matters once the bugs in
commit 73 show up), and `3b1a8f1` writes the implementation plan the next four commits execute.

### 72. Icons, header, and box (a963fb8, c80f4e4, f24584b)

`a963fb8` adds per-`RunState` icon and color lookups. `c80f4e4` adds `renderWidgetHeader`, which
splices a bat-icon header directly into the box's top border line. `f24584b` adds
`renderWidgetBox`, assembling the bordered box around the existing row content with per-state
color applied.

### 73. A UTF-16 surrogate-pair bug, caught by its own test's tautology (6c17348)

Every Nerd Font glyph this widget uses (`BAT_ICON`, every `STATE_ICONS` entry) lives on an astral
Unicode plane, meaning it's stored in JavaScript as a UTF-16 **surrogate pair** — two 16-bit code
units for one visual character. `assembleBox`'s width/padding/fill arithmetic measured every line
with plain `.length`, which counts code *units*, not code *points* — so every icon-bearing line
(every content row, and the header-carrying top border) was over-measured by exactly one unit
relative to the plain bottom border and the icon-free empty-state line, misaligning the box border
by one column. The fix is a `codePointLength` helper (`Array.from(text).length`, which iterates by
code point) swapped in at the four `.length` measurement sites — no arithmetic constant changed,
only what the arithmetic measures.

The more interesting part of this commit is what it says about the *existing* "equal total width"
test: it had been comparing `.length` to `.length`, which is tautologically true no matter how
padding is computed, so it could never have caught this bug even in principle. Rewritten to compare
`codePointLength` instead, plus a second test isolating the exact case that exposed the original
bug (a header carrying an icon, a content line that doesn't) — both verified to fail against a
reverted, `.length`-based `assembleBox` before confirming they pass against the fix. This is the
same lesson this journal has repeated since commit 17: an assertion that can't fail regardless of
the bug is worse than no assertion, because it looks like coverage.

### 74. Wiring it into the live extension, then a second real bug (045f60a, 6a56b15)

`045f60a` renders the bordered widget for real inside the extension. `6a56b15` finds that the OMP
host's `ctx.ui.setWidget` truncates array-content widgets at **10 total lines** — not 10 rows, as
the pre-existing `MAX_WIDGET_ROWS = 10` constant had assumed. Once `renderWidgetBox` wraps content
in a 2-line border, the worst case (10 rows + 1 overflow line + 2 border lines = 13 lines) blew
past that cap, and the host's own truncation silently ate the bottom border — rendering a box that
never visually closes. Fixed by lowering `MAX_WIDGET_ROWS` to 7, so the worst case (2 border + 7
rows + 1 overflow = 10) fits exactly, with the header comment rewritten to state the real
10-total-lines constraint instead of the wrong 10-rows one. The same commit deletes a now-dead
`renderWidgetLines` function (no production callers once `controller.ts` called `renderWidgetBox`
directly) along with its three tests — confirmed, before deleting, that the behaviors those tests
covered (empty state, overflow) remain covered by `renderWidgetBox`'s own tests — and documents a
residual limitation left deliberately unfixed: some terminals render Nerd Font glyphs as visually
double-width despite being a single code point, so the border can still be off by one cell per icon
in those terminals; a full fix needs `wcwidth`/east-asian-width logic, explicitly out of scope here.
`docs/manual-testing.md`'s "Reading the widget line" section (which [`code-walkthrough.md`](code-walkthrough.md)
and this journal both point readers at) is updated in the same commit to describe the real
bordered/iconed/colored format and the corrected 7-row cap, not the pre-Part-VI plain-text shape.

## Part VII — M2/M3 gap closure: doctor, CI, and an honest stub

The "M2/M3 gap-closure" plan named a batch of things the project had claimed were done but weren't
fully wired: a real `doctor` command, a CI workflow that runs on every push (not just release
tags), a conformance gate on release, and operator-facing docs that hadn't been split out yet. This
Part closes most of that list — and is unusually candid about the one piece it closes with a stub
instead of a real implementation, which is exactly the point.

### 75. Naming the gaps before closing them (10a95bb)

Seven new TODO items (10–16), found by re-reading the M2/M3 plan against the running code: no
`coordination-mcp` CLI entry point despite the plan marking it "Closed" (Part VIII closes this for
real), no `batcave display probe` subcommand despite the same claim, crash recovery as a single
untested file instead of the planned kill-point-tested coordinator, no CI workflow on ordinary
pushes/PRs, no conformance gate on releases, no `doctor` command or `/batman-doctor` OMP command at
all, and operator docs not yet split out per the plan. Every item below traces back to one of these
seven.

### 76. Compile errors and a corrupted CLI function, fixed before anything else could proceed (d61050b, 0aac0cd, 339cd39)

`d61050b` fixes a batch of compile errors blocking the doctor/config work: duplicate `#[error]`
attributes on `DbError`, an unnamed-lifetime issue in `config/merge.rs`, a `ToSql` trait issue in
`retention.rs`, a missing `is_blocked()` method on `RolloutGates`, and a `Serve` command pattern
match that hadn't been updated for new config fields. `0aac0cd` finds `run_doctor` itself was
corrupted — three nested, duplicate `match` blocks where one clean block belonged — and adds the
missing `Serialize` derive to `DoctorResult`/`FailedCheck` so `--json` output can actually be
produced. `339cd39` adds the first integration tests for `batcave doctor`: missing database,
JSON-output mode against a missing database, a nonexistent state directory, a nonexistent
repository — verifying both exit codes and output shape.

### 77. A real doctor, reachable from a chat session (0231a8f, b78d38b)

`0231a8f` adds `packages/extension/src/doctor.ts` (`runDoctorCommand`, `buildDoctorContext`) and
registers `batman_doctor`/`/batman-doctor` — the tool shells out to the `batcave` binary directly
rather than going through a live runtime connection, which is the entire point: it's the diagnostic
that works precisely when `batman_status` can't. `b78d38b` marks the TODO item closed, citing the
4/4 passing integration tests and a manual smoke test.

### 78. A CI workflow, immediately trimmed to what actually exists (f20abd3, 8531a05)

`f20abd3` adds the first CI workflow to run on every push/PR (not just release tags): format,
clippy, test (Rust + TypeScript, on Ubuntu and macOS), `generate --check`, and a security job
(`cargo audit` + a secret scanner). `8531a05`, one commit later, removes the JS/TS half of the
format job — no formatter was actually configured yet, so that check could only ever pass
vacuously. (Part VIII's commit 94 fixes this properly by adding Biome.)

### 79. A conformance gate that starts as a no-op, and is caught being one (cbdef62, 41f6bca, a165436, a17a500, c813368, 9516797, e469725, c950d8f, 647ab1a, 94659ab, 2289f0d, 3da37ff, c9ab423, 366b6f4)

This is the longest single arc in Part VII, and it's the clearest example in this journal of a
team catching its own premature "done" claim in writing, in real time, across a dozen small
commits. `cbdef62` adds `tests/conformance/run.ts` and `assert-report.ts` as explicit **stubs** —
`run.ts` writes empty reports, `assert-report.ts` only checks that expected fields are present, and
the commit message says so plainly. `41f6bca` wires a conformance job into the release workflow
ahead of publish, with the same honesty: "conformance job is a stub that always passes." `a165436`
writes the *first* versions of `docs/compatibility.md` and `docs/operations.md` (Task 7 of the same
plan) — a detail worth pausing on, since both documents exist, in evolved form, at the center of
the documentation review this very journal entry belongs to; their earliest ancestor's commit
message is explicit that "only verified claims from actual codebase" made it in. `a17a500` is an
unrelated `clippy`-driven cleanup landing in the same window (deriving `Default` for
`NestedViolationAction` instead of hand-writing it). `c813368` records five pre-existing, unrelated
`adapter_registry.rs` failures in the release checklist rather than hiding them. `9516797` marks
Tasks 14–16 (conformance gates, doctor, operator docs) "completed" in TODO.md — and `e469725`, one
commit later, walks that back with more precision: Task 14 specifically is only "partially
implemented — structural gate wired, but the conformance runner is a stub," because `run.ts`/
`assert-report.ts` write empty reports that a real check would need to reject. `c950d8f` folds that
same honesty into `README.md`'s Known Limitations. `647ab1a` fixes a release checklist file that
had accidentally accreted invalid Markdown after its JSON content (caught because it stopped
parsing under `python3 -m json.tool`).

Then the gate is actually hardened, in three steps: `94659ab` makes `assert-report.ts` throw if any
adapter reports zero scenarios, or if none of its scenarios passed — turning the gate from a
guaranteed-pass no-op into something that can genuinely fail CI, while noting plainly that the
*real* fix (spawning `batcave conformance` for real reports) still doesn't exist yet. `2289f0d`
closes the loop: `release.yml`'s stub report generator now produces an *empty* report on purpose,
which the hardened validator correctly rejects — the gate is now "intentionally blocking release,"
its own commit message's words, until the real runner exists. `3da37ff`, `c9ab423`, and `366b6f4`
are three small follow-up fixes to `assert-report.ts` itself (a duplicated header/import block, a
genuinely missing `readFileSync` import, a stray blank line) — the kind of typo that a stub
implementation, precisely because nothing depended on it working yet, could carry for a commit or
two before being noticed.

### 80. Recovery gets tests before it gets wired (85ea9b9, 1dfa6c9)

`85ea9b9` adds integration tests for crash recovery — explicitly framed as "stub verification,"
since `RecoveryCoordinator` (Part IV, commit 58) still isn't constructed anywhere in
`lifecycle::serve()` at this point; the tests prove the coordinator's own logic works in isolation,
not that it's reachable in production. `1dfa6c9` records the test status in the release checklist.
Part VIII's commit 84 is where the coordinator finally gets wired in for real — and, in a detail
worth flagging now so it doesn't read as a contradiction later, wired in with `#[expect(dead_code)]`
still attached, because the wiring and the *removal from the live daemon lifecycle* turn out to be
two different, sequential decisions this journal narrates in order as they actually happened.

### 81. Two real runtime bugs, found while hardening retention and redaction (7c05d19, 5afa064, d1ac7bb)

`7c05d19` fixes `retention::prune()`: the cutoff timestamp was bound as an `i64` against a column
the schema stores as RFC3339 **text**, a type mismatch that would have made every prune query
compare the wrong representation; and the terminal-state list it filtered against used states that
don't exist (`"completed"` instead of the real `RunState` names `succeeded`/`failed`/`cancelled`/
`lost`) — meaning, before this fix, retention could never have correctly identified which runs were
safe to prune. `5afa064` wires org-configured redaction patterns (Part IV, commit 57) all the way
through `AdapterRegistry::new()` and `DomainAdapterEventSink::new()`, adding a fail-open fallback
in the event-sink construction path that mirrors the one already in `lifecycle.rs` — a decision
this journal flags now because Part IX's review cycle (R14) later finds this exact fallback and
asks whether it's reachable with different behavior than the startup path; the answer at review
time is no, because both paths reuse the same already-validated pattern list, but the trap remains
structurally present for a future change to fall into. `d1ac7bb` retires `known-limitations.md`
outright, folding its two still-uncaptured sections into `TODO.md` and repointing every other
document at `TODO.md` instead — the same "one source of truth for open gaps" discipline `TODO.md`'s
own header still states today — and corrects a stale claim caught in the process:
`PolicyEvaluator` *was* actually wired into `lifecycle.rs` by this point, contradicting a doc that
still described `DenyByDefaultAuthorization` as the only implementation in use.

## Part VIII — The TODO validation era: coordination MCP, policy violations, and workspace isolation

This Part has no single plan behind it the way Parts I–VII each did — it's a sustained, repeated
cycle of the same move: re-read `TODO.md` (or the Obsidian vault plans behind it) against the
running code, find what's stale, fix what's fixable, and write down what's still genuinely open.
Twenty-two commits are pure "the tracking document drifted, here's the correction" work; the rest
are the real features that cycle turned up as missing.

### 82. Coordination MCP gets a CLI entry point, and cross-checking it finds two more bugs (2537d25, 80069a3)

`2537d25` closes the single largest gap Part VII's commit 75 had named: every worker adapter's MCP
launch config (Part III, commit 37) had always pointed at a `coordination-mcp` CLI subcommand that
simply didn't exist — `clap` rejected it outright with an unrecognized-subcommand error the moment
any adapter tried to use it. The fix wires `Command::CoordinationMcp` to the already-implemented,
already-tested `coordination::mcp::run` proxy from Part III, commit 36. The pre-existing
`coordination_mcp.rs` test suite (9 tests, unmodified) goes from a mix of failures to 9/9 — 4 tests
that had been failing with "closed the connection before responding" now pass because the proxy
actually serves stdio, and the other 5 (rejecting missing/expired/mismatched/revoked-token
connections) had been *coincidentally* passing all along against clap's own unrelated
unrecognized-subcommand exit code — verified, after the fix, that they now fail for the real
documented reason instead. A full workspace regression check with `--no-fail-fast` and the change
reverted via `git stash` confirms six pre-existing, unrelated failures aren't new. `80069a3`, found
during the same review pass, fixes three unrelated drift bugs: a hardcoded test expectation for
`ompExtension`'s allowed-methods list that had never been updated after `policy/violation/decide`
was added to the real dispatch table, and two doctests (`recovery.rs`, `doctor.rs`) that used
`Arc<DatabaseHandle>` without importing `Arc` at all, failing `cargo test --doc`.

### 83. A TODO rewrite that finds real gaps, then a second one that fixes a real schema bug (360f0df, 015fafd)

`360f0df` closes the coordination-mcp item, adds three newly-discovered gaps (missing
`batcave conformance`/`adapters` CLI subcommands the Worker Adapters plan's own Task 8 required,
conformance reports omitting the canonical `result_usage_artifacts` scenario, an untracked Copilot
CLI version), and corrects a second stale claim: the `workerMcp` credential store was **not**
reject-all in production by this point — `ScopeTokenVerifier` had already been wired in via
`lifecycle::serve()` the same way `PolicyEvaluator` was — a correction that also lands in
`architecture.md`'s Role Table Summary, verified line-by-line against `ipc/mod.rs`'s real
`allowed_methods()` and `protocol/method.rs`'s real wire names rather than trusted from memory
(exactly the verification this current documentation pass repeats for the same table). `015fafd`
finds the actual root cause behind five long-failing `adapter_registry.rs` tests: its shared setup
helper inserted raw rows into columns that had never been migrated (`workers.task_id`,
`adapter_kind`, `profile_kind`, `status`; `runs.status`, `updated_at`) and omitted two `NOT NULL`
columns the real schema requires — including a foreign key TODO.md's own note had mis-described as
pointing at `adapter_profiles` when the real table is `worker_profiles`. Fixing the shared fixture
exposes a second, fully latent bug underneath it: an assertion checking for `"already started"` or
`"duplicate"` in an error string that the real `RegistryError::DuplicateStart` message never
contains at all (`"run {id} already has a running adapter instance"`) — invisible before because
every one of the five tests crashed in the broken shared setup *before* reaching that assertion.

### 84. Three more TODO rewrites, a provenance-unclear commit handled with unusual explicitness, and a full-suite validation sweep (b22f693, 9702090, 16d6972, 30ef336, cee535c)

`b22f693` closes the `adapter_registry.rs` item and, in the same rewrite, finds that
`tests/domain_repository.rs` — 723 lines, claiming in its own module doc to verify the real
`DomainRepository` — never actually imports or calls that type at all, instead maintaining a
separate, hand-copied schema that had already drifted from the real migrated one. Not a functional
bug (the real `DomainRepository` is correctly tested elsewhere), but misleading coverage that would
keep drifting further from reality the longer it went unnoticed — tracked, not fixed, in this
commit. `9702090` adds the implementation plan for that schema fix. `16d6972` is the one commit in
this entire journal whose own message states it doesn't know who wrote the change it's committing:
a dead `org_security_patterns` field and some needless test cleanup were found already staged in
the working tree, present and unchanged for roughly 38 hours of otherwise-active work, attributable
to no session's own history. Committed anyway, on the user's explicit instruction, only after
independently verifying the change compiles and all 8 redaction unit tests still pass — this
journal's honesty rule extending, for one commit, to "the provenance of this exact diff is
genuinely unknown, and that fact is worth recording rather than glossing over with a plausible
authorial attribution."

`30ef336` is the most thorough validation pass in this Part: every open TODO item checked against
`cargo test --workspace --no-fail-fast` *and*, for the first time in any validation pass, a full
`bun test` run. Result: zero regressions among previously-tracked items, one stale claim corrected
(the OMP-RPC approval-normalization gap had, in fact, already been fixed and was proven by a
passing conformance test — though the artifact-production half of that same item remained
genuinely open), and two new gaps the `bun test` run surfaced for the first time:
`runtime/status.binarySource` always reporting `"unknown"` because `cli.rs` never read the
`BATMAN_BINARY_SOURCE` environment variable the extension had been setting all along, and a stale
tool/command list in `index.test.ts` that predated `batman_doctor`. `cee535c` closes out three
fully-executed implementation plans (this one, the coordination-mcp CLI fix, and the monitor widget
work) by deleting them from the repo's scratch-plan folder and gitignoring it going forward — it's
agent working space, not permanent documentation.

### 85. Real work the TODO cycle turned up: workspace RPC wiring and a proof that cancel kills a real OS process (ae8f279, 4c639ff)

`ae8f279` routes `WorkspaceAcquire`/`WorkspaceGet`/`WorkspaceRelease`/`WorkspaceInspect`/
`WorkspaceApply`/`ArtifactList`/`ArtifactFetch` from `connection.rs` to
`OrchestrationService::dispatch` for the first time — previously every one of those methods,
despite being fully implemented in Part IV, was unreachable over the wire, rejected with
`METHOD_NOT_FOUND` before `OrchestrationService` ever saw the request. `4c639ff` adds the test this
journal's own recurring theme (commit 17, commit 45) keeps asking for: not "does `run/cancel`
return `Ok`," but does the underlying OS process actually die. It constructs a real `OmpRpcAdapter`
against the `fake-worker` fixture, submits a run through the full RPC surface, calls `run/cancel`,
and polls until the fake-worker's real OS pid is confirmed dead — closing the gap between a prior
test (proving `ManagedProcess::terminate()` kills a process in isolation) and the real adapter chain
end to end. It does not prove `SIGKILL` escalation (`fake-worker`'s mode dies on the first `SIGINT`,
so escalation is never exercised by this particular test) — noted explicitly rather than implied.

### 86. Closing the `policy/violation/decide` stub for real (364dee4, d9bb6ff)

`364dee4` is the single largest feature commit in this Part (24 files, 2 new), and it closes a stub
this journal has mentioned twice already (Part IV commit 55's Phase 8 note, and every mention of
`policy/violation/decide` since). `ViolationService::record()` is idempotent — the quarantine/cancel
action applies exactly once, but a `PolicyViolationRecorded` event journals on every observation,
so a second identical observation is provably not silently dropped, just not re-actioned.
`ViolationService::decide()` enforces ownership, refuses to re-decide an already-decided violation,
and refuses `release` outright against a run that's already terminal — the same three-part
enforcement shape (ownership, idempotency, settled-run rejection) commit 21's `ApprovalService`
established for a structurally identical problem. `MIGRATION_4` adds the `policy_violations` table;
`PolicyViolationId` becomes the ninth UUIDv7 newtype in `crates/protocol/src/ids.rs` (the eight from
Foundation, commit 2, gain a peer). `DomainAdapterEventSink` calls `record()` whenever a
`NestedWorkerObserved` event arrives and the run's effective nested-capability isn't `Managed` —
covering both the `None` and `Observable` cases, since either one means the observation itself was
already outside what the run was authorized to do. Enforcement gates land in three call sites that
previously had none: `message/send`, `workspace/apply`, and `coordination/publishArtifact` all now
check `Run.flags.policyQuarantined` and return a new dedicated error code
(`POLICY_QUARANTINED`, -32101) — a run that's quarantined is *actually* blocked from further
progress, not just marked as such for a UI to display. A `nested_violation_action` config knob
(`Quarantine` / `Cancel` / `QuarantineAndCancel`) threads the policy's own choice of remedy from
`RuntimePolicy.rollout_gates` through to `ViolationService`. Four new integration tests prove the
shape holds: quarantine actually blocks `message/send` until released, `decide` is forbidden for a
non-owning client, `release` is refused against a terminal run, and a second observation on an
already-actioned run never double-cancels. `d9bb6ff` is a one-line follow-up removing a stray
leftover header line from the TODO entry this commit closed.

### 87. Naming what's still unreachable from a chat session (b00e863, 633a7d7, 1ee41db, 499e659, b590002)

`b00e863` names a specific, narrow gap: `profile/register` (and a `profileId` field on
`batman_worker`) had no OMP tool wrapping it at all, so a real Claude/Codex/Copilot worker
genuinely could not be created from a live chat session, even though every byte of the underlying
RPC and runtime machinery had worked and been tested since Part III. `633a7d7` is a far larger
sweep: all eight Obsidian vault planning documents re-read in full, one independent reviewer per
document, each verified against the *running code* rather than trusting the plan's own prose. The
Foundation (M0) plan had nothing new to report — already fully implemented, matching this
journal's own Part I. Everywhere else turned up real gaps, most significantly that
`PolicyEvaluator` enforced only two of the six authorization dimensions the Hardening plan actually
specified: cost ceilings and adapter-kind allowlisting had no implementation at all, not even a
stub. `1ee41db` is a one-line, immediately-actionable fix that same sweep produced: the default
concurrency ceiling (applied whenever a layered config omits `concurrency.ceiling` entirely) was
raised from 2 to 8 — 2 having been discovered as impractically low for real use. `499e659` documents
the same `profile/register` gap from a second angle (items 15–16: several RPC methods, not just
`profile/register`, had no OMP tool wrapper — `policy/violation`, `coordination/child`,
`workspace/*`, `artifact/*` were all fully implemented in the runtime and completely unreachable
from a chat session). `b590002` is a pure bookkeeping fix, but a thorough one: a concurrent session
had renumbered nearly every TODO item but only partially updated the internal "item N"
cross-references those items make to each other, leaving several pointing at the wrong item.
Fixed with a script mapping every old number to its new one and checking every "item N" mention in
the body text against that mapping — 27 stale references across 14 items found and fixed, including
two range mentions that no longer corresponded to contiguous ranges at all, spelled out explicitly
instead of left as a range. Re-run after the fix: zero mismatches.

### 88. Tool descriptions, a real conformance CLI, and a genuinely missing events-table column (a033371, 631cacb, 6fcc20b, 7a78a06)

`a033371` rewrites every OMP tool's description to explain when to use it, its key operations, and
typical workflows — aimed squarely at helping a model choose the right tool and invoke it
correctly, not at documenting the RPC shape underneath it (that's what `architecture.md` and
`plugin-usage.md` are for). `631cacb` closes four TODO items at once: `scenario::ALL` had only 12
entries where it needed 14 (missing `RESULT_USAGE_ARTIFACTS` and `UNEXPECTED_CHILD_OBSERVATION`),
which was silently causing three adapters' conformance tests to panic — fixed by extending the
array and adding the missing Copilot scenario function, which in turn exposed that the OMP-RPC
adapter's own `conformance.rs` had never wired either scenario into `build_scenarios()` at all (one
function didn't exist yet; the other existed behind `#[allow(dead_code)]` and was simply never
called). The same commit adds real `batcave conformance`, `batcave adapters`, and
`batcave display probe` CLI subcommands, wired to logic that had already existed and already been
tested — unblocking the conformance release gate Part VII's commit 79 had built as an honest stub.
And it finds a genuinely missing piece of the events schema: the `events` table had no
`task_id`/`worker_id` columns at all, even though `append_and_apply` had been building them into the
in-memory `EventEnvelope` for live broadcast the whole time — they simply evaporated on persist,
and `events/replay` had been hardcoding both to `None` ever since. Fixed with a new migration and
threading both columns through the insert and replay paths (two more columns,
`parent_worker_id`/`vendor_event_ref`, are added in the same migration but remain `NULL` — no write
path supplies them yet, tracked as a separate, still-open gap rather than silently populated with a
guess). `6fcc20b` and `7a78a06` are the paired doc-fix/feature-implementation halves of the same
change: `RecoveryCoordinator` is documented as wired into `lifecycle.rs`'s startup sequence but
still carrying `#[expect(dead_code)]` — the wiring this journal flagged as pending back in commit
80 landing for real, described precisely as "wired but not yet live" rather than either extreme.

### 89. Fixing a bug that would have broken the cross-agent scenario before it could ever start (07619b0, b8994b3, 383bcf1, 354371a)

Four commits, and together they're the difference between "workspace isolation exists in the type
system" and "two workers can actually run in two different git worktrees at the same time." `07619b0`
persists `isolation_kind` in the `workspace_leases` table for the first time (it had always been
hardcoded to `"shared"` before this commit, regardless of what was actually requested) and moves
lease acquisition to two phases — an `allocating` row inserted first, promoted to `active` only
after the workspace is actually materialized — so that isolated workspaces (`GitWorktree`/`Copy`)
can coexist with each other and with shared workspaces, since they occupy disjoint paths and no
longer need the old global write-exclusion to stay safe. `b8994b3` finds the bug that same
restructuring was needed to fix: `workspace_acquire`'s original implementation called
`materialize()` and then discarded its result with `let _ = materialize()` — meaning it created a
*real* git worktree on disk but returned a *fake* `/tmp/ws-…` path to the caller, and leaked the
`allocating` row forever if materialization failed. The rewrite makes the response carry the real,
persisted path from `activate()`, and releases the lease on materialization failure instead of
leaking it. `383bcf1` threads that real path all the way to where it matters: `RunDriverContext`
gains an optional `workspace_path`, `run_one` uses it as the adapter's working directory instead of
the repository root whenever one is present, and `run/submit` acquires an isolated lease whenever
`workspaceMode` is `"isolated"` or `"copy"` — the commit message states plainly what this makes
possible for the first time: two runs with `workspaceMode: "isolated"` now execute in two genuinely
separate git worktrees. `354371a` fixes the two bugs that would have made all of this untestable
from an actual chat session: `batman_run`'s submit path was silently dropping the `prompt`
parameter (every worker would have started with an empty instruction), and `batman_worker`'s create
path was silently dropping `profileId` (Claude and Codex workers would have failed
`PROFILE_REQUIRED` immediately) — both parameters had existed in the schema and simply never made
it into the RPC call.

### 90. The eighth tool, and letting a worker see its peer's workspace (a47c191, 4f0d154, 114291d, 11477e6)

`a47c191` adds `batman_profile` (wrapping `profile/register`) and `batman_workspace` (wrapping
`workspace/acquire|get|release|inspect`) — the two tool gaps commits 87 and 89 had already named as
blocking the cross-agent scenario — bringing the OMP tool count to eight. `4f0d154` adds
`CoordinationPeerWorkspace`, a new RPC method letting a worker resolve a same-task peer's workspace
path/mode/isolation-kind/state for direct cross-workspace code review, exposed as an eighth
worker-safe MCP tool (`batman_peer_workspace`) alongside a fix that `batman_peers` had been omitting
each peer's `runId` from its response the whole time. `114291d` updates every document this journal
has been checking for staleness throughout this Part — `architecture.md`, `code-walkthrough.md`,
`manual-testing.md` (which gains a new §5 for the cross-agent workspace-isolation scenario),
`getting-started.md` — to say "eight tools" and "RecoveryCoordinator is wired," and is explicit that
this journal's own earlier "six tools" references (Foundation-era, Part II) are deliberately left
unchanged as historical record rather than silently updated to match the current count. `11477e6`
closes four TODO items this Part's work resolved (workspace-mode threading, the two new tools, peer
workspace resolution) while leaving one open on purpose — worker-MCP artifact list/fetch was
deliberately excluded from the plan's scope, not forgotten — and removes crash recovery from
README's Known Limitations now that it's genuinely wired.

## Part IX — A hardening squash, then a review that finds what it missed

Part IX closes this journal, and it does so with two very different textures back to back. The
first eleven commits are a wide, parallel-authored hardening pass across almost every subsystem at
once — each commit's own message is a single terse line with no body, which this journal notes
plainly rather than inventing detail the commits themselves don't provide. The second half is the
opposite: a formal, four-reviewer codebase review (`REVIEW.md`) that re-reads the entire hardened
system with fresh eyes and finds four critical, production-blocking bugs the tests had missed —
followed by the same-day discipline this journal has praised since commit 10: finding them, fixing
them, and writing down exactly what was fixed and what's still open.

### 91. A parallel hardening squash across nine subsystems (38c8c3f, 6a08785, 4fad81c, fc5f9db, cb6842f, 274b0d5, 4621add, 7a7a4c0, 56507fa, 9f85dc3, 02f3426)

`38c8c3f` adds the Biome formatter and a CI format gate for TypeScript — the gap Part VII's commit
78 had explicitly deferred, closed here for real; its own commit message notes that TypeScript
formatting changes from this point on travel with the commits that own them, rather than arriving
as a single repo-wide reformatting diff. `6a08785` regenerates the shared schema/TS-bindings
codegen in one reproducible commit, keeping Rust protocol definitions and their generated output
never more than one commit apart — the same discipline `bun run generate --check` has enforced
since Foundation, commit 3. The next seven commits are titled by subsystem rather than by story —
`feat(runtime/db,domain): harden event persistence and recovery`,
`feat(runtime/policy,security): enforce run policy and fail closed`,
`feat(runtime/workspace): harden leases, conflicts, and artifact limits`,
`feat(runtime/adapter): harden live conformance and event normalization`,
`feat(runtime/ipc): expose workspace, artifact, child, and display workflows`,
`feat(runtime/cli): add audit export, doctor checks, and startup sweeps`,
`feat(extension): add OMP tools and restart reconciliation` — and none of the seven carries a
commit body beyond that one line. This journal records that plainly rather than reconstructing a
narrative these commits didn't write down themselves: each is a substantial, subsystem-scoped
hardening pass, landed together, and the accurate account of *what* changed in each is the source
tree at that commit and the tests that shipped with it, not a retrospective story. `9f85dc3` adds a
release provenance matrix and makes the conformance gate real (superseding Part VII's honest stub).
`02f3426` is a documentation commit refreshing closed gaps and pruning completed items out of
`TODO.md` — the routine maintenance this journal has shown recurring throughout Part VIII, once
more, after a large batch of work lands.

### 92. One eager-cleanup fix and one flaky-test hardening (3907e8f, a79d4ee)

`3907e8f` makes subscription-forwarder tasks exit as soon as the writer half of a connection
closes, instead of waiting for another broadcast to notice — closing out TODO item 49 and a small
resource-cleanup gap in `ipc/connection.rs`. `a79d4ee` fixes a genuine race in the lifecycle lock
tests: the *losing* process in a singleton-flock race can exit the instant it observes the winner's
lock, before the winner has finished opening its database and binding its socket — a test asserting
the winner's socket exists *immediately* after the loser exits was racing the winner's own startup.
Fixed by using the test suite's existing bounded wait instead of an instantaneous assertion.

### 93. A four-reviewer codebase review, and four critical fixes the same day (889cbd8, b004857, 6a4c506, 86244da, 3678b99, cafa0e0, 26dcf07)

`889cbd8` is `REVIEW.md`'s first commit — the document this documentation pass has been
cross-referencing throughout Parts VII and VIII. Its own method section is worth restating here
because it's a real methodology, not filler: the tree was split across four parallel reviews
(runtime core; adapters/policy/security; TypeScript/OMP integration; build/docs/release), every
Critical and High finding was re-read against its cited source before inclusion, and leads that
turned out to be strengths rather than bugs were removed rather than kept for volume. Four Critical
findings came out of it, and three are fixed in this journal's very next three commits — the same
same-day-fix discipline this journal has praised in every prior review-shaped commit (23, 45, 55)
holding one more time.

`6a4c506` fixes **R1**: the extension authenticated every runtime connection with the constant
`instanceId: "batman-extension"`, while `batman_task upsert` recorded the real OMP session ID as
`ownerClientInstanceId` — meaning approval and policy-violation decisions, which require exact
identity equality, could *never* be decided by the session that created them. Fixed by threading an
optional `sessionId` through `EnsureRuntimeOptions`, `initParams`, `tryConnect`,
`connectWithBackoff`, `ensureRuntime`, `getClient`, `statusContextFor`, and all eleven tool call
sites — closing the status-path gap in the same commit rather than leaving it as a known follow-up.
`86244da` fixes **R2**, the single highest-impact bug this review found: each successful worker
authorization incremented `PolicyEvaluator`'s `active_runs` counter, but `PolicyEvaluator` was
immediately erased behind the `AdapterAuthorization` trait object, whose interface had no release
method at all — meaning after `concurrency_ceiling` **cumulative** runs (not concurrent — every run
ever authorized, forever), the daemon would permanently refuse every new run until restart. Ordinary
sustained use would eventually and silently disable the runtime's core function. Fixed by adding a
`release()` method to the trait (a no-op for `FixtureAuthorization`, a real `decrement_runs()` call
for `PolicyEvaluator`), called by the adapter completion watcher after evicting a settled adapter,
and by `run_one` on every post-authorize error path — defended by an integration test that books a
`concurrency_ceiling: 1` slot through the real `PolicyEvaluator`, proves a second run is denied,
releases the slot through the trait object, and proves the ceiling denial clears. `b004857` fixes
**R3** and **R4** together, both in the release pipeline: R3 was that the Linux ARM64 release
target built on an x86_64 GitHub runner with no AArch64 cross-linker installed at all (fixed by
installing `gcc-aarch64-linux-gnu` and setting the matching `CARGO_TARGET_*`/`CC_*`/`AR_*`
environment variables, plus a new dry-run CI workflow exercising every release target on every
push); R4 was that GitHub's artifact-upload/download cycle silently strips the executable bit
`xtask package` had set, which the package-set assembly step correctly rejected — meaning even
after R3's fix, no release could complete without a person noticing the rejection and manually
`chmod +x`-ing something. Fixed by removing the release workflow's destructive flatten loops and
having both the package-set and publish jobs run `find ... -name batcave -exec chmod +x {} +` after
every artifact download, restoring the bit the platform itself removes.

`3678b99` records the resolutions for all four in both `TODO.md` and `REVIEW.md` — R2–R4 fully
closed, R1 (the identity fix) marked partially closed pending a dedicated end-to-end test rather
than claimed complete on the strength of the fix alone. `cafa0e0` adds that test: two live-daemon
integration tests proving the full `sessionId → instanceId → ownerClientInstanceId` chain — the
positive case seeds a task/approval/violation owned by session A, connects as A, and confirms both
decide calls succeed; the negative case seeds the same data but connects as session B, confirming
both decisions are rejected with "does not own." `26dcf07` marks R1 and TODO item 68 fully resolved
on the strength of that test — the same pattern this journal has called out since commit 21: no
fix is recorded as closed until the test proving it exists, not just the diff implementing it.

### 94. Repo guidance for future sessions, and the exact commits this documentation pass grew out of (eba1556, 60e8fa3, d1ef420, 0f670dc)

`eba1556` adds `AGENTS.md` (the canonical, exhaustive directory table and invariant reference) and
`CLAUDE.md` (a working summary that defers to it) — the two files whose own text this journal's
Parts VII through IX have been cross-checking claims against throughout. `60e8fa3` is the direct
ancestor of the documentation review this very journal entry is part of: it adds
`docs/cli-reference.md` and `docs/plugin-usage.md` as new documents for the first time (covering
every `batcave` subcommand/flag and all eleven orchestration tools respectively), and rewrites
`docs/operations.md` to remove content that had never been true — invented Homebrew/apt uninstall
steps, a fabricated Herdr-restart feature, a fake compatibility matrix — while fixing real,
verified inaccuracies (the lock mechanism, the state-dir default, missing subcommands) and
deferring to the new `cli-reference.md` for flags instead of duplicating them. The same commit fixes
a fabricated `--port` flag and `--recover` flag in `getting-started.md`, a fabricated config
auto-discovery path, a wrong `Redactor::new()` call, an incomplete health-check list, a stale test
count, and permission-error guidance that told readers to `chmod 755` their state directory —
directly contradicting that same document's own `0700`/`0600` security claims two sections above
it. `d1ef420` adds `batcave capture`, automated tooling to regenerate adapter conformance fixtures
from real vendor CLI output (a deterministic scrubber replacing session IDs, timestamps, costs, and
paths with stable placeholders while preserving the correlation IDs conformance suites assert on,
so re-capturing an unchanged CLI is byte-identical) — replacing what had been, until this commit,
hand-authored fixture JSON. `0f670dc`, the commit this journal's Part IX ends on, adds `release/` to
`AGENTS.md` and `CLAUDE.md`'s directory tables (a top-level, cross-language directory that both the
Rust build tooling and CI had been reading without either guidance document mentioning it) and
gitignores the release manifest CI generates fresh on every run — closing this journal at the same
kind of small, unglamorous accuracy fix it opened Part V with, which is a fitting place to stop:
the discipline this document has narrated since commit 10 is still the same discipline in commit
217, wherever the next one after this journal's own writing turns out to be.

## Part X — REVIEW.md's second pass: seven more fixes, eleven doc corrections, and the residue that outlived them

Part IX closed on four Critical fixes landed the same day they were found. The seven High findings
from that same first review round (R5-R11) got the identical same-day discipline, across the fix
commits `8331a34 9720c63 8457de5 6bd6a00 f9e95c4 797d5e6 e8204da 44093d4 e4befb8 bcff4ce 143e1b3`.
Unlike R1-R4, every one of these seven left a smaller, real gap behind — not a regression, but a
residual defect the fix itself introduced or exposed. This journal records both halves, because a
"resolved" that quietly grew a new open item is not the same story as a clean close.

**R5** — a `humanRequired` approval could be decided by the model itself, with no human in the
loop. Fixed by adding a `DecidedBy` enum (`Human`/`Model`) to the protocol and rejecting a `Model`
decision against a `human_required` approval in `ApprovalService::decide`; the extension fails
closed with no UI path around it. Left behind: **R34** — the fix persists `decided_by` via
`serde_json::to_string`, storing the JSON-quoted `"human"` instead of the bare token every other
scalar-enum column in the same file uses, so `WHERE decided_by = 'human'` returns nothing, forever.

**R6** — a cached runtime client that had died silently broke every tool call until `batman_status`
happened to be invoked. Fixed by exposing `BatmanClient.isClosed` and routing every construction
site through a `resolveClient()` that reconnects on a closed cache; defended by `reconnect.test.ts`.
Left behind: **R39** — the fix's own repair path correctly pairs `controller.stop()` with clearing
`subscribedClient`, but the `session_shutdown` handler calls only `controller.stop()`, so a monitor
that lives through a session shutdown without that pairing can end up permanently unable to
reconnect.

**R7** — `run/retry` created a queued run and then never started its adapter. Fixed by routing
`run_retry` through the same `start_queued_run` helper `run_submit` already used. Verified, not just
fixed: `orchestration_rpc.rs` proves a retried run actually starts. No gap left behind — the shared
helper closes the class of bug outright.

**R8** — the release conformance gate ignored aggregate failure; a stub could pass green. Fixed
(`de07022`) by gating `batcave conformance --fixture` against a committed
`fixtures/conformance/fixture-mode-baseline.json`. Left behind: **R44** — the capture tool that
produces that baseline is calibrated against exactly one of the eleven committed fixtures (its
scrubber only recognizes `claude/initialize.jsonl`'s placeholder ID family as already-canonical),
and its `unchanged` flag is computed by reading back the file it just wrote, not by comparing
against what was committed before the write — so the safety net the gate depends on is itself
unproven beyond the one fixture it was built against.

**R9** — release version checks validated the git tag but not the packages actually assembled for
distribution. Fixed (`bb209eb`) by having `package-set` verify each leaf's own version and adding a
`version-gate` CI job that checks the tag against `v<version>` before any build work starts. No gap
left behind.

**R10** — artifact APIs claimed task-level isolation but were scoped project-wide, so one task could
read another's patches. Fixed (`44093d4`) by stamping `Artifact.run_id` at the point of production
and scoping `artifact/list`/`artifact/fetch` by `owner_client_instance_id`, proven by a dedicated
cross-owner isolation test. Left behind two gaps, both still open: **R35** — `artifact/fetch` reads
and hashes the full content *before* the ownership check runs, a timing side-channel distinguishing
"exists but not yours" from "doesn't exist" by latency alone; and **R36** — the isolation tests
hand-seed `run_id` on their fixtures rather than exercising the real producers
(`WorkspaceApplier`/`WorkspaceInspector`), so reverting the producers' own stamping code back to
`run_id: None` would leave the entire test suite green.

**R11** — Copilot's vendor turn-stop reasons were discarded outright instead of being normalized
into protocol health/failure signals. Fixed (`bcff4ce`) via `copilot_normalize_stop_reason()`,
mapping every stop reason to a `ProtocolHealthChanged` event and a failure disposition, defended by
eight unit tests. Left behind: **R42** — the unknown-reason arm's detail string interpolates the
already-lowercased, `_`/`-`-stripped match binding instead of the original vendor `stop_reason`
text, so the one piece of diagnostic detail meant to help someone grep vendor docs for an
unrecognized reason has already been mangled past matching them.

**R47** — Claude and Codex adapters never emitted `ProcessExited`, so their concurrency slots leaked
on every completed run, permanently disabling the runtime after `concurrency_ceiling` cumulative
runs (the exact failure mode R2 closed for the mechanism as a whole, open again for two of the
four adapters). Fixed across five steps: added `TerminationOutcome::exit_signals()` and
`ManagedProcess::settle()` to the supervisor (`supervisor/process.rs`); Claude's `run_session` now
yields an outcome from all three break arms and emits `ProcessExited` after cleanup
(`adapter/claude/mod.rs`); Codex's `driver_loop` carries the exit through `InboundMessage` to the
pump (`adapter/codex/client.rs`), `spawn_pump` emits `ProcessExited` and leaves the loop, and both
`cancel` and `dispose` were fixed to not abort the pump before it reports (`adapter/codex/mod.rs`);
OMP-RPC's `run_pump` now emits `ProcessExited` on its terminate arm, not just stdout-closed
(`adapter/omp_rpc/mod.rs`); and the registry's completion watcher was replaced with
`SettlementSink` — a per-run oneshot that fires on the first `ProcessExited`, immune to broadcast
lag or late subscription (`adapter/event_sink.rs`, `adapter/registry.rs`). The registry's old
`is_process_exited_for` was deleted. Defended by new tests: `settle_reports_a_self_exit_code_without_escalating`
and `settle_escalates_a_process_that_will_not_exit_on_its_own` (`tests/supervisor.rs`),
`session_exit_tests` (`adapter/claude/mod.rs`), `pump_exit_tests` (`adapter/codex/mod.rs`), and
`settlement_tests` / `settlement_sink_tests` (`adapter/registry.rs`, `adapter/event_sink.rs`) — the
former using a real `DatabaseHandle::start()` harness with `tempfile::TempDir` so the DB actor
persists through the test. `PolicyEvaluator::release()` saturates at zero, so a double release is
safe, and the oneshot's exactly-once semantics guarantee the slot releases precisely once per run.
A full end-to-end integration test driving a Claude or Codex run through the real registry's
completion watcher and asserting the concurrency slot is returned does not yet exist — the existing
component tests prove emission and the mocked release path, but the integration gap remains.

### The documentation half: eleven doc-accuracy findings, most already stale on arrival

The same first review round filed eleven Low-severity documentation findings (R19, R21-R28) —
CLI flags that didn't exist, tool counts that were wrong, deleted modules still named, an installer
Homebrew never had. By the time each was re-verified on 2026-08-08, six (R19, R23, R24, R26, R27,
R28) had already been corrected by unrelated doc work and needed nothing further — recorded as
resolved on the strength of re-reading the current text, not a fix commit filed against this
review. Two (R21, R22) had regressed independently into `AGENTS.md`/`CLAUDE.md` after those files
were generated later than the original doc fixes — corrected in place during the same
consolidation that produced this journal entry's predecessor commits. **R25** went further than
"resolved": `release/0.1.0-checklist.json`, the file the finding was filed against, was deleted
outright in `7ab1447` rather than merely relabeled — re-verification on 2026-08-10 confirmed there
was nothing left to fix.

### Where this history lives now

`REVIEW.md` itself was restructured on 2026-08-12 from a full audit trail (every finding, resolved
or not, with its evidence) into an open-items-only backlog — R1-R11, R19, and R21-R28 no longer
appear there at all. R47 joined that list the same day it was resolved, pruned from `REVIEW.md`
and recorded here. This journal entry is now the only place resolution evidence for R1-R11 and
R47 is recorded; if you're looking for *why* R34, R35, R36, R39, R42, R44, or R67 exist, the
answer in every case is "as a byproduct of a fix directly above it in this entry." That same
2026-08-12 pass also ran a full fresh re-verification of every item that *was* still open, adding
twenty new findings (R47-R66) surfaced by reading the runtime core, the adapters, the conformance
harness, the TS extension, and the release/docs surface with fresh eyes — the most severe of them
(R47-R49) initially sitting at Critical, though R47 was resolved the same day it was found, and R48
and R49 both the next day (Parts XI and XII), clearing the Critical tier entirely.
R67 retains the integration-coverage residue — a reminder that a review closing its filed findings
is not the same claim as a system having no more bugs.

## Part XI — Halving the Critical pair: a ceiling that could not be enforced

Part X closed with R48 and R49 as the remaining Critical pair. R48 is now closed, and its shape is
worth recording because nothing about it was visible from the code that appeared to implement the
feature. Every piece of per-run cost enforcement existed and was wired: `config/merge.rs` read
`cost.ceiling_per_run_usd`, `policy/evaluate.rs` refused to authorize a run whose adapter could not
report usage (so the ceiling could never be silently unmeasurable), `AdapterRegistry` threaded the
ceiling into each run's `DomainAdapterEventSink`, and the sink accumulated `UsageReported.cost_usd`
and fired exactly once on the crossing event. The one thing that could not happen was the write.

`MIGRATION_4` had declared `policy_violations.vendor_child_id` and `vendor_parent_ref` `NOT NULL`,
back when a nested worker was the only kind of violation. A cost ceiling has no vendor child, and
`record_cost_ceiling` correctly journals both as `None` — which bound as SQL `NULL` and failed the
constraint on every single crossing. Because the insert is the first thing `record_cost_ceiling` does
and its error propagates with `?`, `apply_action` — the code that quarantines or cancels the run —
never ran at all, and the sole caller in `event_sink.rs` only logged a warning. A run could spend
without limit while the runtime reported nothing but one warn line.

Fixed by `MIGRATION_8`, a table rebuild (SQLite cannot drop a column constraint in place) that makes
both vendor columns nullable and preserves every existing row: an absent vendor child is now recorded
as an absence, matching what the code and the event payload already said. The sentinel-empty-string
alternative was rejected for the reason `record_policy_violation`'s own doc comment gives — an empty
id would be a lie rather than an absence.

The gap was as much a testing gap as a schema one: before this fix, nothing anywhere in the tree
touched the `policy_violations` table other than the migration that created it. Three tests now
defend it. `migration_8_makes_vendor_refs_nullable_and_preserves_existing_rows` (`db/migrations.rs`)
migrates to version 7, proves the old schema rejects the NULL insert, migrates to 8, and proves the
pre-existing row survived, that the `action`/`created_at` constraints did not get dropped along the
way, and that the resolution columns still work. `record_policy_violation_persists_absent_vendor_refs_as_null`
(`domain/repository.rs`) proves the repository writes real SQL NULLs against the production migration
list with foreign keys on. And `crossing_the_per_run_cost_ceiling_records_an_actionable_violation`
(`tests/orchestration_rpc.rs`) drives a full run whose adapter reports $2.50 against a $1.00 ceiling
and asserts the run comes back quarantined, the journaled event carries `cost_ceiling_exceeded` with
null vendor refs, and `policy/violation/decide` can release it — that last step being the one that
proves the projection row exists, since `decide` reads it rather than the journal.

R49, the other half of that pair, is closed in Part XII.

## Part XII — Closing the last Critical: a denylist blind to its own vendor

R49 was the last Critical, and the smallest diff in this journal: one character class. The built-in
`api_key` redaction rule read `sk-[A-Za-z0-9]{16,}` — a pattern that looks exactly like it covers
`sk-`-prefixed vendor keys and covers none of the ones this runtime is pointed at. Anthropic issues
`sk-ant-api03-<base64url>`; OpenAI issues `sk-proj-<base64url>`. Both put a hyphen three characters
in, right where the pattern demanded `[A-Za-z0-9]`. `bearer_token` needs a `Bearer ` prefix,
`github_pat` needs `ghp_`, `aws_access_key` is a different shape entirely, and `jwt` needs three
dot-separated segments — so nothing else in the built-in set caught it either.

Classification was never the failure. `Secret`- and `Thinking`-classified fragments are dropped
before they are ever scanned, and that is the primary boundary commit 5 built. The denylist exists
for the *second* case: a vendor CLI narrating a key back inside ordinary `Visible` text — an echoed
error, a debug line, a dumped environment. That is the one case it could not handle, and the
journal is append-only, so anything that got through was durable.

What made it invisible for so long is worth more than the fix. Every test that asserted the rule
worked used a literal written *from the pattern* — `sk-ABCDEFGHIJKLMNOPQRSTUVWX`, all alphanumeric,
sixteen-plus characters, matching by construction. The suite and the bug shared one assumption, so
the suite agreed with the bug. The new tests are written from the vendors' documented key shapes
instead, and `redaction_boundary.rs` — the test that byte-scans `runtime.db`, the WAL, `runtime.log`
and the replay output rather than trusting the redactor's return value — now carries an
Anthropic-shaped key through the real append path.

The widening needs a guard on the other side, and getting that guard right took two attempts worth
recording, because the first one was wrong in a way that reads as correct. Once the class accepts
`-`, ordinary hyphenated prose becomes matchable: `disk-space-check-failed` contains
`sk-space-check-failed`, nineteen characters of a perfectly legal match. The obvious fix is a
leading `\b`, and it does reject that string — the `sk` there follows `i`, a word character. It was
shipped that way. But `\b` asserts a *word* boundary, and `-` is not a word character, so
`pre-sk-space-check-failed` sails straight through it. The guard only looked airtight because the
one example it was tested against happened to be the one shape it handles.

The pattern is now `(^|[^A-Za-z0-9_-])sk-[A-Za-z0-9_-]{16,}`: the preceding character is matched
outright and constrained to something outside the token alphabet, then re-emitted through the rule's
`${1}` replacement so the surrounding text is not eaten along with the secret. That capture created
a second thing to prove. `replace_all` scans non-overlapping and resumes at the end of each match,
so if a match ate the separator *following* its key, the next key would begin with no delimiter left
to capture and would survive raw. It cannot: the trailing `[A-Za-z0-9_-]{16,}` stops at the first
character outside the token alphabet, so key 1's following separator is never consumed and is still
sitting there to serve as key 2's captured prefix. `two_adjacent_api_keys_are_both_redacted` pins
that down across a space, a comma, and mid-sentence separation.
`hyphenated_prose_is_not_mistaken_for_an_api_key` and
`hyphen_delimited_prose_is_not_mistaken_for_an_api_key` guard the two prose shapes; the second one
fails against the `\b` version, which is how the gap was caught. Over-redacting diagnostics is a
quieter failure than leaking a key, but it is still a failure.

With R49 closed the Critical tier is empty for the first time since the 2026-08-12 review pass. The
High tier is not; Part XIII closes two of the eight it listed.

## Part XIII — Two leaks, one lease: releasing what a failed start acquired

R41 and R50 were the same defect reached from two different steps. `start_queued_run` and
`workspace_acquire` each run the same four-step sequence for an isolated or copy workspace:
`LeaseService::acquire` (commits an `allocating` row), `WorkspaceMaterializer::materialize`
(`git worktree add` or a bounded copy), `LeaseService::activate` (records the real path), then, for
`start_queued_run` only, `RunDriver::start`. Every step after `acquire` could fail, and until now
none of them released what `acquire` had already committed. R41 named the last of those four steps;
R50 named the second. `run/retry` (Part X, R7) re-runs this whole sequence for a new `RunId` on every
attempt, so a driver that reliably fails to start turned one leak into one leaked row per retry.

The fix is two new helpers on `OrchestrationService`, `abandon_lease` and `abandon_and_announce`,
called from every fallible step past `acquire()` in both functions. They copy `workspace_release`'s
existing release-then-teardown-then-`cleanupFailed` ordering rather than inventing a second
convention: release the lease first (so the next acquisition is never blocked by a row nothing will
ever activate), then best-effort tear down whatever materialization reached disk. Teardown is
deliberately best-effort — `git worktree remove --force` fails outright on a worktree that was never
created, so propagating that error would replace the caller's real failure (the one the caller
actually needs to see and retry against) with an unrelated cleanup artifact.

Making that release honest surfaced a third bug, not named in either original finding: the first
draft of `abandon_and_announce` announced `LeaseReleased` unconditionally, including when
`release()` itself failed. A live monitor watching for that event would believe the workspace was
free while the row was still sitting in `allocating`/`active` with no owner ever coming back for it —
exactly the state the announcement claims doesn't exist. `abandon_lease` now returns a three-way
`AbandonOutcome` (`Released`, `ReleasedWithCleanupFailure`, `ReleaseFailed`), and only the first two
announce `LeaseReleased`; a genuine `ReleaseFailed` announces `CleanupFailed` instead, matching the
state `mark_cleanup_failed` actually wrote. That, in turn, meant the conflict and count queries in
`LeaseService::acquire` and `active_for_repository` needed a matching discriminator: a
`cleanupFailed` row is not automatically free just because it left the `active`/`allocating` states.
`released_at IS NULL` distinguishes the two cases directly from the column `release()` itself sets —
a `cleanupFailed` row whose `release()` call never succeeded still blocks a new `Shared`+`Write`
lease, exactly as an active one would; a `cleanupFailed` row whose `release()` succeeded and only the
disk teardown failed afterward does not.

R50 outranked R41 for a reason worth restating precisely: `LeaseService::stale()`'s only signal was
"a non-empty path that no longer exists on disk." An `allocating` row is empty-path by construction
until `activate()` runs, so a row abandoned between `acquire` and `materialize` — R50's own
trigger — could never produce a non-empty, missing path. The doctor check written for exactly this
residue was structurally blind to it, not merely untested against it. `stale()` now also flags any
row still `allocating` past `ALLOCATING_LEASE_GRACE` (ten minutes — well past a realistic
`git worktree add` or a copy bounded by `DEFAULT_COPY_MAX_BYTES`/`DEFAULT_COPY_MAX_FILES`, and longer
than `RecoveryConfig`'s five-minute stuck-run threshold, because a lease is abandoned by a *process*
death while a run is abandoned by an *event* gap) regardless of path emptiness.

Eight tests defend this. None is vacuous: each pins an assertion that only the specific branch this
fix added — a release call, a teardown call, a `CleanupFailed` announcement, or the widened
`stale()`/`active_for_repository()` predicates — can satisfy; reverting any one of those branches
while leaving the test in place fails it.
`start_queued_run_releases_the_lease_when_materialize_fails`
and `workspace_acquire_releases_the_lease_when_materialize_fails` (`tests/orchestration_rpc.rs`) drive
`run/submit`/`workspace/acquire` against a repository with no commits — `gitWorktree` isolation's
`git rev-parse HEAD` fails deterministically — and assert the journaled event pair is exactly
`leaseRequested`/`leaseReleased`, never `leaseAcquired`, and that the lease row itself reads
`released`/empty-path/`released_at` set, not leaked. `start_queued_run_releases_the_lease_and_worktree_when_driver_start_fails`
uses a real committed repository so materialization and activation genuinely succeed, then a driver
whose `start` always fails, and asserts the full `leaseRequested`/`leaseAcquired`/`leaseReleased`
triple plus that the worktree materialized before the failure was actually removed from disk.
`stale_never_flags_an_allocating_lease_within_the_grace_period` and
`stale_flags_an_allocating_lease_that_outlived_the_grace_period` (`tests/workspace_lease.rs`) pin the
grace boundary directly against a back-dated `acquired_at`. `an_unreleased_cleanup_failed_lease_still_blocks_a_new_shared_writer`
and `a_released_lease_with_a_failed_teardown_does_not_block_a_new_shared_writer` (same file) pin the
`released_at` discriminator in both directions. `stale_workspaces_fails_when_an_allocating_lease_outlives_the_grace_period`
(`tests/doctor.rs`) proves the doctor-level check — the one R50 said could never see this — now can.

With R41 and R50 both closed, the High tier drops to six: R33, R44, and R51-R54 remain.

## Part XIV — Fixture mode's broken promise: a kill switch only one caller ever asked about

R52 was a promise the code stated in three places and checked in none of them. The `conformance`
module's own doc calls fixture mode "default, always safe, zero model calls"; `CLAUDE.md` tells
contributors that `BATMAN_DISABLE_VENDOR_CLI=1` "skips live vendor CLI calls" and that CI always sets
it precisely to avoid billed calls; `release.yml`'s `conformance` job comments that "this gate uses
fixture mode only regardless — the switch is a second, independent safeguard." The narrow reading of
the promise held — no billed inference ever happened in fixture mode — and the load-bearing one did
not. Fixture mode spawned real vendor binaries, on all four adapters, whether or not the switch was
set.

The mechanism is a single asymmetry between two sibling functions. Every adapter's `live_report()`
opens with an early `if vendor_cli_invocation_disabled() { return Err(...) }`, so the switch is
honored before that path reaches anything that spawns. `fixture_report()` had no such guard, and
nothing anywhere in its call graph consulted the function either — yet that call graph still reached
real subprocesses in two distinct places. PROBE called `adapter.probe()`, which spawns
`claude --version`, `codex --version`, `copilot --acp`, or `omp --version` depending on the adapter.
And the five scenarios whose only proof is a live vendor process —
`READ_ONLY_START_AND_PROGRESS`, `FOLLOW_UP`, `SESSION_RESUME`, `RUNTIME_RESTART`,
`CANCELLATION_SCOPE` — spawned it directly, minus the four Codex already failed honestly on via
`requires_live_turn_scenario()`. This was caught by running it rather than by reading it:
`BATMAN_DISABLE_VENDOR_CLI=1 PATH=/usr/bin:/bin ./target/debug/batcave conformance --adapter claude --fixture`
exited 1 with `probe failed: ... failed to spawn "claude": No such file or directory (os error 2)`
in the scenario details, identically for the other three adapters. That distinction matters: the
evidence is an attempted spawn, not merely an unread environment variable.

The guard went into the shared helpers, not into `fixture_report()`, and that choice is the whole
design of the fix. Because every `live_report()` already returns early *before* reaching
`probe_scenario()`, `live_process_scenarios()`, `cancellation_scope_scenario()`, `real_client()`,
`resolve_conformance_selector()`, `resume_flag_probe()` and the rest, gating those helpers themselves
changes nothing about live mode's behavior while fixing the caller that never gated at all — one
check per real-spawn choke point instead of duplicated branching in two callers. What each gated
helper returns is deliberately asymmetric. A new shared `vendor_cli_required_scenario()` in
`crates/runtime/src/conformance/mod.rs` returns an honest **fail** for a scenario that has no
fixture-only proof, because skipping it must never be counted as proof that it works. PROBE is the
one exception and gets a **pass**, via `vendor_cli_skipped_probe()` — the wording extracted out of
`probe_availability()` in this same fix so every adapter reports the skip identically rather than
inventing its own phrasing. Turning a skipped probe into a denial would make every run in CI
unauthorized, which is why that one degrades upward.

Reconciling `fixture-mode-baseline.json` against real post-fix output then exposed a second defect
nobody previously had to notice: the baseline had been silently asserting vendor-CLI presence all
along. Its `"claude": []` and `"ompRpc": []` — zero expected failures — were only ever true on a
development machine that happened to have all four vendor CLIs installed and, for Claude,
authenticated. Nothing guaranteed that of any runner, and specifically not of `release.yml`'s bare
`ubuntu-latest` `conformance` job, which installs none of them. With the switch set and `PATH`
scrubbed to `/usr/bin:/bin`, so the guard's effect is provable even on a machine that does have all
four, `claude` gained five newly-honest failures (`read_only_start_and_progress`, `follow_up`,
`session_resume`, `runtime_restart`, `cancellation_scope`), `codex` gained one
(`read_only_start_and_progress` — its other four were already correctly listed, having never depended
on this fix), `copilot` gained three (`read_only_start_and_progress`, `follow_up`,
`cancellation_scope`), and `ompRpc` gained four (`follow_up`, `cancellation_scope`, `session_resume`,
`runtime_restart`). `PROBE` stayed a pass in all four, which is the proof that the fix distinguishes
"cannot prove, skip honestly" from "cannot prove, therefore broken."

That corrected baseline is correct specifically in the switch-set posture, and the fix accepts that
knowingly: both jobs that gate on it (`ci.yml`'s `test`, `release.yml`'s `conformance`) always set the
switch, so it is the posture that matters, but it does mean a `cargo test --workspace` run *without*
the switch on a machine with the vendor CLIs installed will now trip the same "baselined failure
unexpectedly passed" gate from the other direction — the correct trade, given the previous baseline
carried the mirror defect in the direction that actually shipped, which is R52 itself, and now
documented inline in the baseline file's own `switchComment` key.
`conformance_fixture_with_the_kill_switch_never_spawns_a_vendor_cli` (`crates/runtime/tests/conformance.rs`)
defends it the same way the bug was found: it runs the real compiled `batcave` binary for each of the
four adapters with the switch set and `PATH` scrubbed, asserts all fourteen scenarios still ran, that
no scenario detail contains `"failed to spawn"` or `"No such file or directory"`, and that `probe`
still passes. Asserting on the absence of an `ENOENT`-shaped detail — rather than on the switch being
read somewhere — is what makes it a real regression test.
`BATMAN_DISABLE_VENDOR_CLI=1 cargo test --workspace` passes (718 passed, plus the one pre-existing
unrelated failure `copilot_adapter::real_binary_initialize_and_session_list_never_invoke_a_model`,
which spawns the real `copilot` binary from the test itself, outside the conformance call graph
entirely, and fails identically on unmodified `main`); `cargo clippy -D warnings`,
`cargo fmt --all --check` and `bun run generate --check` are clean — no protocol types moved, this was
pure runtime logic.

Four pre-existing adapter test files moved in the same commit and belong in the regression net rather
than beside it: `crates/runtime/tests/claude_adapter.rs`, `codex_adapter.rs`, `copilot_adapter.rs` and
`omp_rpc_adapter.rs` each asserted "every provable scenario passes," which stopped being true once
switch-gated scenarios started reporting honest failures. Each now allows a scenario to fail *only* if
its detail names `DISABLE_VENDOR_CLI_ENV`, so the relaxation is scoped to the reason for the failure
and a genuine regression under the switch still fails the test. `copilot_adapter.rs`'s version was, as
committed, the only test defending copilot's `real_client()` guard specifically: that guard's
would-be regression signature is `copilot CLI not found on PATH`, which is not `ENOENT`-shaped and so
matched neither of the two marker strings the new CLI regression test originally checked. A follow-up
commit widened that marker list to include copilot's and `omp_rpc::resume_flag_probe`'s own
signatures, so all four guards are now covered directly by the single test as well.

One behavioral consequence of this fix reaches beyond conformance, and it is worth stating plainly
rather than leaving to be rediscovered. `adapter::registry` calls `run_fixture_conformance(kind)` on
every real run submission and feeds the resulting `effective_capabilities` straight into
`authorization.authorize(...)`, and `conformance::report::downgrade_on_scenario_failure` downgrades
`steering → None` when `FOLLOW_UP` fails and `resume → None` when `SESSION_RESUME` fails. Post-fix,
with the switch set, those two scenarios fail by construction on all four adapters — so a daemon whose
environment happens to carry `BATMAN_DISABLE_VENDOR_CLI=1` (an exported dev shell variable, an
inherited harness environment) will deny any run whose policy lists `steering` or `resume` in
`required_capabilities`, with a `CapabilityMissing` error that never mentions the switch. That sits in
tension with the invariant asserted in `DISABLE_VENDOR_CLI_ENV`'s own doc comment — a development
switch must never silently stop production work. Three things keep it a disclosure rather than a
regression: the impact is opt-in and config-gated, since an empty `required_capabilities` is
unaffected; the pre-fix behavior was strictly worse rather than better, because the same downgrade
already happened under the switch on any machine lacking the vendor CLIs — i.e. on every CI runner —
so the fix replaces machine-dependent authorization with deterministic authorization; and the
principled repair is a `skipped` discriminator on `ScenarioResult`, letting
`downgrade_on_scenario_failure` tell "disproved by a real attempt" from "never attempted because the
switch is set," which is a larger change than this fix should smuggle in. Making
`vendor_cli_required_scenario()` return a pass instead would fabricate proof and reintroduce exactly
the defect R52 closed, so that is not the answer. The discriminator is tracked as R68.

With R52 closed the High tier drops to five: R33, R44, R51, R53 and R54 remain (R68, opened by the
final review of this same fix, brings it back to six). As with every fix in this journal whose real
target is a machine this repo cannot stand up locally, final confirmation that the release pipeline's
`conformance` job goes green on a bare CI runner with no vendor CLI installed can only come from
GitHub Actions on the next push or tag.

## Part XV — Crash recovery's five-minute blind spot: the one crash it could not see

R51 in REVIEW.md ranked first among the High-severity items, on end-to-end functionality
completeness: `RecoveryCoordinator::recover()` runs exactly once per `serve()`
(`lifecycle.rs:149-151`), and pre-fix it only recovered runs whose most recent journaled event
predated a 300-second `RecoveryConfig::stuck_threshold` (`recovery.rs:68`). A daemon that crashes and
is restarted seconds later by a supervisor — systemd, a process manager, or a human hitting the up
arrow — leaves every mid-flight run's last activity seconds old, which fails that cutoff, and no
second sweep ever runs against that crash: retention re-ticks every 24 hours
(`lifecycle.rs:312-331`), but recovery never re-ticks. The runs most likely to need recovery — the
ones from the crash that just happened — were exactly the ones the sweep was built to ignore, and
they stayed `working`/`queued` with no live process behind them, forever.

The threshold wasn't merely too large; it was unsound at the one call site that used it. `serve`
holds the single-instance `flock` before running the sweep, and `AdapterRegistry`'s `running` map
(`adapter/registry.rs:151`) is constructed empty for this process
(`adapter/registry.rs:164-172` in the earlier read, `running_adapter`/`running_count` at
`:186-188`/`:193-195`) — so every non-terminal run visible at that moment provably has no live
supervisor behind it, however recent its last event. The module's own doc comment already asserted
this ("every run this sweep can see predates this process"); the age filter contradicted the
invariant the same file documented.

The fix replaces the single age-gated query with an explicit `SweepScope`: `EveryNonTerminal` for the
boot sweep (no cutoff parameter at all), and `StaleBeyond(Duration)` for the doctor's separate,
read-only `stale_runs` report, which *is* checking a live daemon and must not fail a run that is
merely quiet. Both scopes share one SQL projection (`STUCK_RUN_SELECT`, with `STALE_ONLY_PREDICATE`
appended only for `StaleBeyond`) so the "last activity" definition — most recent journaled event,
falling back to `created_at` — can never drift between the two readings. `stuck_threshold` is deleted
from `RecoveryConfig` outright rather than left present-and-ignored, so the trap cannot be re-armed
by a future caller reading the struct and assuming it still does something. The renamed constant,
`DEFAULT_STALE_RUN_THRESHOLD`, now lives beside its only consumer, exported from `lib.rs` for
`doctor.rs` to reach; `check_stale_runs`'s own doc comment, which had claimed the check counted runs
"with no live adapter" — something it never verified, since it only ever read timestamps — is
corrected in the same commit to describe what it actually does: count runs silent past the threshold,
read-only, never claiming a quiet run is a dead one.

R51's REVIEW.md entry offered a periodic re-sweep as the alternative fix; it is refused here, on
evidence rather than preference. No heartbeat exists anywhere in `AdapterEventPayload`
(`adapter/event_sink.rs:55-62`: only `ProcessStarted`/`ProcessExited`), and the vendor event
normalizers emit nothing for thinking or unrecognized frames, so a live, working run can go silent
for minutes without being dead — a time-based sweep against a *running* daemon would fail runs that
are merely quiet, trading one false negative for a worse false positive.
`AdapterRegistry::running_adapter` (`adapter/registry.rs:186-188`) could in principle supply real
liveness instead of a timestamp guess, but pursuing that here would have been solving a problem this
fix doesn't have: the deeper blocker, discovered while verifying this exact path, is that in
production nothing calls it either.

That deeper finding is worth stating plainly rather than left to be rediscovered. `AdapterRegistry`
is the real `RunDriver` (`impl RunDriver for AdapterRegistry`, `adapter/registry.rs:198`), and its
`start` (`:199-263`) reserves the run-id slot, calls `run_one` to spawn the adapter, and spawns
`watch_settlement` to wait for `ProcessExited` — but neither `run_one` nor `watch_settlement`
(`:337-367`) ever calls `transition_run`. `watch_settlement` only evicts and disposes the adapter,
releases the concurrency slot, and journals the display detach. Grepping `crates/runtime/src/adapter/`
for `transition_run` returns zero hits. The only production `transition_run` call sites are
`run_cancel` (`service/orchestration.rs:995`, user-triggered), the approval service (`working` →
`waitingUser`) and the policy violation service — none of them advance a run through
`queued → starting → working`, and none of them ever mark one `succeeded` or `failed` on its own
completion. The one place that transition sequence is actually implemented is `FakeRunDriver`
(`service/run_driver.rs:89-127`), used only by orchestration tests. Concretely: a real, successfully
completed run's row in `runs` never leaves `queued` on its own — which is why the boot sweep,
imprecise as it was, has been the *only* thing in this codebase that ever terminalizes a run at all,
and why its five-minute blind spot mattered far more than a recovery bug normally would. Tracked
separately as R69, filed Critical below.

`crates/runtime/tests/recovery.rs` no longer ages anything: `a_run_whose_last_event_is_seconds_old_is_
recovered_at_startup` replaces `fresh_non_terminal_run_is_not_recovered`, which had encoded the defect
itself as a passing invariant. Every sleep and the `RECOVERY_SETTLE` constant are gone, since nothing
needs to age past a threshold that no longer exists for the boot sweep; the twelve other kill-point
tests are otherwise unchanged in intent. Two new tests drive a real `Doctor` against the same seeded
database to prove the silence threshold survives exactly where it is still meaningful:
`the_doctors_stale_run_report_ignores_a_run_that_is_merely_recent` and
`the_doctors_stale_run_report_names_a_run_silent_past_the_threshold`, the latter back-dating both
`runs.created_at` and every journaled event's `timestamp` directly with raw SQL — the only remaining
consumer of age at all.

The end-to-end proof ran through the real compiled `batcave` binary, not just the test suite: seed a
`working` run into a real, migrated `runtime.db` with its activity set to the current instant, restart
`serve`, and read the log and the row back. Pre-fix, the second boot printed no
`crash_recovery_*` log line at all and the run's state read back `working`. Post-fix, the same script
logs `crash_recovery_transitioned_run` with `from_state=working to_state=failed`, and the row reads
back `failed`.

`BATMAN_DISABLE_VENDOR_CLI=1 cargo test --workspace` passes 719, one more than Part XIV's 718 (net:
one test removed, three added across the two rewritten files), with the same pre-existing,
environment-specific failure as every entry in this journal since Part XIV documented it:
`copilot_adapter::real_binary_initialize_and_session_list_never_invoke_a_model`, because the locally
installed Copilot CLI (1.0.80 at the time of this fix) is not yet in `COPILOT_KNOWN_CLI_VERSIONS`.
`cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --all --check`, and
`bun run generate --check` are all clean — no protocol type moved, so generation is unaffected.

With R51 closed, the High tier drops to five: R33, R44, R53, R54 and R68 remain. R69, opened by this
same fix's investigation into why the boot sweep is the only thing that ever closes a run, puts
Critical back to one.

## Part XVI — A state machine with no production writer: closing the last Critical

R69, opened by Part XV's own investigation, was BATMAN's last Critical open item: no production
path ever moved a run out of `queued`. `AdapterRegistry` is the real `RunDriver`
(`adapter/registry.rs:198`), but grepping `crates/runtime/src/adapter/` for `transition_run`
returned zero hits — neither `run_one` (`:389-488`) nor `watch_settlement` (`:345-367`) ever called
it. The only production call sites were `run_cancel` (user-triggered), the approval service's
`working <-> waitingUser` toggle, and the policy-violation service; none of them ever walked a run
through `queued -> starting -> working`, and none of them marked one `succeeded` or `failed` on its
own completion. `RunState::can_transition_to` (ADR-0012) was thoroughly tested, and `FakeRunDriver`
(ADR-0013) drove it end to end — but only in orchestration tests, never from a live `omp` session.
A well-tested relation with zero production callers is exactly as broken as an untested one; see
the new `## Run Lifecycle` entry in `engineering-lessons.md` for the lesson stated plainly.

The fix is `RunLifecycleSink` (`crates/runtime/src/adapter/run_lifecycle.rs`), wrapping each run's
`AdapterEventSink` so that, after the inner sink journals an event, the sink commits (and
broadcasts) the lifecycle edge that event is evidence of:

| evidence | edge |
|---|---|
| `ProcessStarted` | `queued -> starting` |
| any other payload except `ProcessExited` | up to `working` |
| `ProcessExited { exit_code: Some(0), signal: None }` | `-> succeeded` |
| `ProcessExited` with a non-zero code or a signal | `-> failed` |
| `ProcessExited` with no code and no signal | `-> lost` |

Edges are walked, never jumped — `queued -> working`, `starting -> succeeded`, and
`waitingUser -> succeeded` are all illegal in `RunState::can_transition_to`, so a target is reached
by committing each legal hop in turn, which is also what keeps `runs.started_at` correct
(`transition_run` stamps it only on the `starting` edge). Edges are forward-only: `working` applies
only from `queued`/`starting`, so vendor output arriving while a run sits in `waitingUser` or
`paused` never clobbers it. And a terminal state always wins — every walk stops the moment it
observes one, and `transition_run` itself rejects an illegal edge even if a concurrent commit wins
the race.

Two decisions were worth recording as ADR-0023 rather than left implicit. First, the terminal edge
is applied inside the per-run sink, before `SettlementSink` fires the signal that releases the
run's concurrency slot — transitioning instead inside `watch_settlement` was considered and
rejected, because that signal fires immediately after, which would let another run be authorized
while this one still read non-terminal. Second, an exit with no observable code and no signal maps
to `lost`, not a guessed `succeeded`/`failed` — the same "name the uncertainty" precedent ADR-0015
already set for OMP-native facts.

Fixing this exposed two secondary defects that had to move with it. `CopilotClientEvent::ProcessExited`
previously carried no exit status at all (`copilot/client.rs`); the sink's `terminal_state_for` needs
a real `exit_code`/`signal` to distinguish `succeeded` from `failed` from `lost`, so the client now
reports the supervised process's real termination outcome. And the policy-violation service's
cancel path had an ordering race against this same sink now applying edges concurrently, fixed in
`policy/violation.rs` and `coordination/broker.rs` alongside it. R12 (Claude error-result subtypes
not modeled) and R13 (violation-cancel's warning not distinguishing "no running adapter" from a
kill failure) stay open and untouched — this fix decides the terminal state from process exit
status only, which is the evidence R69 named.

The proof is layered, not just unit tests against a stub. `run_lifecycle.rs`'s 9 unit tests
(`process_started_moves_a_queued_run_to_starting` through
`vendor_output_never_reopens_working_on_a_run_that_started_waiting`) pin every edge and guard
against a real, migrated SQLite database. Two end-to-end tests then prove the same production
chain (`DomainAdapterEventSink` wrapped in `RunLifecycleSink`) against a genuinely spawned process:
`tests/run_lifecycle.rs`'s `a_real_worker_process_walks_its_run_from_queued_into_working` drives a
real `fake-worker --mode rpc` child through `OmpRpcAdapter` and polls the seeded run to `working`,
asserting the journaled `RunEvent` states are exactly `["queued", "starting", "working"]`; its
sibling `a_real_worker_process_exit_settles_its_run` disposes that same adapter and polls for a
terminal state. `adapter/claude/mod.rs`'s new `run_state_tests` module reuses the existing
`session_exit_tests` harness (a real `/bin/sh` + `Supervisor::with_escalation` + `run_session`, no
real Claude CLI involved) with the production sink chain in place of `RecordingSink`, proving a
clean exit walks `["starting", "working", "succeeded"]` and a non-zero exit walks to `failed`.
`tests/copilot_adapter.rs`'s new `a_supervised_process_exit_is_reported_with_its_real_status` spawns
`/bin/sh -c 'exit 3'` under `CopilotAcpClient::spawn_with_raw_args` and asserts `next_event()` yields
`ProcessExited { exit_code: Some(3), signal: None }` — the exact status the client silently
swallowed before this fix.

`BATMAN_DISABLE_VENDOR_CLI=1 cargo test --workspace` passes 735 of 736, with the same
pre-existing, environment-specific failure as every entry in this journal since Part XIV documented
it: `copilot_adapter::real_binary_initialize_and_session_list_never_invoke_a_model`, because the
locally installed Copilot CLI (1.0.80 at the time of this fix) is not yet in
`COPILOT_KNOWN_CLI_VERSIONS`. `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo fmt --all --check`, and `bun run generate --check` are all clean.

With R69 closed, **Critical drops to zero** for the first time this journal records. The High tier
is unchanged at five: R33, R44, R53, R54 and R68 remain.

## Part XVII — Skipped is not Fail: the discriminator R68 asked for

R68 was self-inflicted by the previous fix. Part XIV closed R52 by making every unattempted
vendor-CLI scenario report an honest `fail` rather than a fabricated `pass` — correct locally, but
`AdapterRegistry::run_one` feeds every fixture conformance report's `effective_capabilities`
straight into the production authorizer, and `downgrade_on_scenario_failure` read any `fail` as a
disproof. So `BATMAN_DISABLE_VENDOR_CLI=1` — a development and CI convenience — deterministically
stripped `steering` and `resume` from every adapter's effective capabilities, denying any run whose
policy required them, with an error that never named the switch as the cause. Two outcomes,
`pass`/`fail`, could not express three states: proved, disproved, and never attempted.

The fix, across three commits, gave `ScenarioResult` the third state the gate actually needed.
`7163ab1` introduced `ScenarioOutcome { Pass, Fail, Skipped }`, renamed the wire field `passed` to
`outcome`, and added `was_skipped()`/`proved()`/`disproved()` predicates — a pure refactor with no
behavior change beyond the JSON field rename, since nothing yet produced `Skipped`.
`ed6c5b3` then flipped every "not attempted" producer from `fail` to `skip`: the shared
`vendor_cli_skipped_probe()`/`vendor_cli_required_scenario()` helpers, Codex's
`requires_live_turn_scenario()`, Copilot's `real_client()` and `session_resume_probe()` refusals, and
OMP-RPC's `resume_flag_probe()` refusal — introducing a shared `VendorUnavailable { Skipped, Failed }`
so each producer keeps the deliberate-refusal-vs-attempted-failure distinction it already had, just
expressed in the new vocabulary. `downgrade_on_scenario_failure` and the availability gate both read
`disproved()` only, so a skip downgrades nothing and fabricates nothing — R68 and R52 both stay
closed by construction, not by convention.

A third commit, `bf38d95`, is the proof neither of the first two carried on its own: that the
production authorizer really does still approve a run the pre-fix behavior would have denied.
`crates/runtime/tests/kill_switch_authorization.rs` sets `BATMAN_DISABLE_VENDOR_CLI=1`, runs every
adapter's real fixture conformance suite, and asserts `effective_capabilities ==
declared_capabilities` for all four. That equality alone would hold vacuously on an all-`pass`
report regardless of whether the fix works, so the test also asserts, as a whole-run aggregate,
that at least one of the four reports carries a `was_skipped()` scenario — and, specifically for Codex, that `FOLLOW_UP` and
`SESSION_RESUME` are among the skips, since those are the two scenarios gating the `steering`/`resume`
capabilities the next assertion depends on. (An earlier draft of this test checked `!report.passed`
instead, which a genuine, unrelated `Fail` could also satisfy with zero scenarios actually skipped —
caught before landing and replaced with the direct `was_skipped()` check.) The test then takes
Codex's resulting effective set through the real `PolicyEvaluator::authorize()` against a policy
requiring `steering` + `resume` and asserts `Ok(())` — the exact denial R68 described, now proved
absent end to end rather than argued about layer by layer. It mutates the process-global
`BATMAN_DISABLE_VENDOR_CLI` variable via `unsafe { std::env::set_var }`, so the file holds exactly one
`#[tokio::test(flavor = "current_thread")]`, the same constraint `vendor_cli_availability.rs`
(Part XIV) already established. Two unit tests moved into `conformance/report.rs`'s own suite
alongside it: `a_skipped_scenario_leaves_its_gated_capability_declared` proves a skip alone changes
nothing, and `a_skip_never_masks_a_real_disproof_of_a_different_gate` proves a skip sharing a report
with a genuine failure of a *different* capability still lets that failure downgrade correctly — the
skip doesn't accidentally shield anything beyond its own gate.

`cargo test --workspace` passes both with and without the switch set — 736 passed either way, plus
the same pre-existing, environment-specific failure every entry since Part XIV has carried,
`copilot_adapter::real_binary_initialize_and_session_list_never_invoke_a_model` (locally installed
Copilot CLI 1.0.80 still isn't in `COPILOT_KNOWN_CLI_VERSIONS`), confirmed unrelated by reproducing
it identically on unmodified `main`. `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo fmt --all --check` (on the touched files; three pre-existing, untouched files carry their own
unrelated formatting drift), and `bun run generate --check` are all clean; `bun test` passes 149.

With R68 closed, the High tier drops to four: R33, R44, R53 and R54 remain.

## Part XVIII — One guard, three doors: the two coordination calls that journaled unmetered

The sentence that carried this finding lives in a doc comment. The one on `CoordinationBroker`
said it routed the coordination operations "enforcing message bounds, reply visibility, task
ownership, and the per-sender rate limit before any journaling." It reads like a property of the
whole struct, and for as long as that sentence was the review surface, that is exactly how it was
read. The enforcement was real — but it lived inline in one of the struct's three journaling
methods. `send()` checked the 64 KiB byte bound and the per-sender rate budget before every
append; `request_child()` and `publish_artifact()` went from liveness into the journal, and
neither referenced the limiter or the bound. A worker connection
reaches all three: the role table that has made new methods safe to add since [ADR-0009](adr/0009-role-based-authorization-from-the-connection-not-per-call.md)
grants the worker role the whole surface, and the MCP mirror and the direct JSON-RPC path both
land in the broker. So one socket — or one in-process RPC frame — could journal unbounded
`reason`, `artifactRef`, or `description` text, and `publishArtifact` was the door with the loop,
nothing after it stopping another call, each append fanning out to every live monitor subscriber
while the comment above the struct told the reader the guard was there.

Three things looked bounded, and none of them is the journal, which is the shape of the survival.
`mcp_protocol` caps its own tool arguments at 16 KiB, so the model-facing path was visibly
throttled — but that bound protects the tool layer, not the RPC underneath. The direct RPC path
carries no argument bound at all; its only ceiling is the 4 MiB IPC frame cap, a limit meant to
bound a line, a multiple of the byte bound the journal row was supposed to get. And the struct's
comment said the guard existed, which was true — for `send`. A guard that is true of one of three
doors and described as the guard of the broker passes every review that re-reads the comment and
every test that drives the layered path.

The finding, R53, was opened by the 2026-08-12 round that also opened R52, and it overstated the
`requestChild` half of its own write-up, which the fix had to correct before it could be trusted.
The write-up described a worker looping `requestChild` unthrottled; the state machine refuses it.
A run that is `working` and asked to request a peer moves to `waitingPeer`, and the transition
check in the domain repository rejects the second call before anything is appended — loop it
forever and you get one journaled request and a long list of rejections, never a flood. Its real
exposure was size, not volume. `publishArtifact` is the door that was actually both: unbounded on
its two free-text fields, and loopable, because nothing in it or after it stops another call.

The fix is deliberately small and deliberately shared. Two private helpers now sit between the
broker and the journal, and all three journaling methods call both, before append and before
broadcast, in a documented order. `reject_oversized` refuses any single free-text field longer
than 65 536 UTF-8 bytes — `COORDINATION_PAYLOAD_MAX_BYTES`, 64 KiB, the constant `send` has
always enforced — a per-field cap, with `INVALID_PARAMS` naming the field that crossed; it
checks the field's own length, never a serialized value, and the only other bound on the direct
path is the codec's 4 MiB IPC frame cap, which limits a line, not the row. `charge_rate_limit`
spends the caller's unit of the per-sender budget, `RateLimited` at thirty calls a minute, the
same budget, the same window. The key is never free caller input: `requestChild` and
`publishArtifact` read the run's own worker row through `run_participants`, an identity lookup
the broker already had, and `send` charges the worker identity its connection was authenticated
as — the bound scope down the tool path, a `senderWorkerId` field the RPC handler matches
against the connection's principal on the direct path — so a worker cannot spend another
worker's budget, and the existing smuggled-`senderWorkerId` test stands untouched. In
`publishArtifact`, quarantine keeps precedence over the limiter: the gate deliberately runs
ahead of the charge, so a punished worker is refused for quarantine before a budget unit is
spent, and cannot starve the shared meter by shouting at it — while `requestChild` and `send`
carry no quarantine gate at all. The constant's doc comment now says what it used to
hide — one budget, shared across the three methods — and `bun run generate --check` confirms a
comment moves neither schema nor bindings. The fix lands in `51d76e3`; `ea11417` lands the two
doc corrections the review afterward forced, on which gates cost budget and why quarantine comes
first.

It is proven by five tests `56c59cd` added to the coordination suite, and the falsifiability
check went the mechanical route — the two `charge_rate_limit` calls temporarily removed, the two
budget tests watched fail, the calls restored, the five watched pass:

- `coordination_publish_artifact_draws_on_the_same_per_sender_budget_as_send` — thirty accepted
  `coordination/send` calls, then one `publishArtifact` on the same scope token: JSON-RPC
  `-32006` on the thirty-first.
- `coordination_request_child_draws_on_the_same_per_sender_budget_as_send` — the same shape
  ending in `requestChild`: `-32006`, and `events/replay` from sequence zero contains no
  `childWorkerRequested`.
- `coordination_request_child_rejects_a_reason_over_64_kib` — a 65 537-byte `reason` gets
  `-32602`, and a following well-formed `requestChild` still succeeds: the run never left
  `working`.
- `coordination_publish_artifact_rejects_free_text_over_64_kib` — a 65 537-byte `artifactRef`
  and a 65 537-byte `description`, `-32602` twice, and `message/list` for the run returns
  `messages: []`.
- `coordination_publish_artifact_accepts_a_description_at_the_limit` — exactly 65 536 bytes
  succeeds, guarding `>` against `>=`.

Three of the seventeen existing coordination tests already exercised the same two constants
against `send` — a payload at the 64 KiB bound, one over it, and thirty-and-one messages in a
minute — and the suite went from seventeen tests to twenty-two. The adversarial review that
followed asked six questions and found the two that were real: the doc comment overstated
which gates cost budget, and the `requestChild` budget test's replay probe could have failed to
parse a replay frame and then silently skipped its assertion. Both are closed: the
clarification in `ea11417`, and the parse-or-scream hardening in `585d85c`. The rest came
back clean — no leak, no vacuous test, no changed signature — and the docs commit `00677ee`
carries the lesson to its proper places: a doc comment asserting an invariant is not
enforcement, `mcp_protocol`'s argument bounds are a second layer that masks the absence of the
first from every test that only drives the outer layer, and when a type's doc comment promises
an invariant, the enforcement belongs in a named helper the type's methods must call.

Measured on the final tree: `cargo fmt --all --check` and `cargo clippy --all-targets
--all-features -- -D warnings` clean; `BATMAN_DISABLE_VENDOR_CLI=1 cargo test --workspace
--no-fail-fast` — the flag because a plain run stops at the known copilot failure and a
fail-fast cargo then skips the twenty-nine test binaries that sort after it — ran all 49 to
completion: 744 passed, the two standing live-CLI ignores in the claude and codex adapters, and
the one standing failure every entry since Part XIV has carried, the locally installed Copilot
CLI 1.0.80 still not in `COPILOT_KNOWN_CLI_VERSIONS`; `bun run generate --check` clean; `bun test
packages` 139 passed, 0 failed, 334 assertions, fourteen files. The coordination suite within
that run: 22 passed, 0 failed.

With R53 closed, the High tier drops to three — R33, R44, and R54 remain.

## Part XIX — Two decisions, one violation: the guard that lived outside the transaction

`ViolationService::decide` read like a decision procedure: load a snapshot, check it for a
conflicting resolution, check it for a settled run, then write. All three checks ran against one
snapshot from a single `self.db.run_domain_op(...)` round trip; the write was a second, separate
round trip — and the actor (`crates/runtime/src/db/actor.rs`) is a single `std::thread` that
processes one whole boxed closure at a time, off a bounded channel. It serializes closures, never
a service's sequence of decisions about them. Two concurrent `decide` calls for the same
`violationId` could both have their snapshot read `resolution: None` before either wrote — the
actor has no way to know the two reads and two writes were meant to be one decision each — and
`resolve_policy_violation`'s `UPDATE policy_violations SET resolution = ?1 ... WHERE violation_id
= ?4` had no `resolution IS NULL` guard and no affected-row check. Both writes landed, both
callers proceeded past the write to fire their side effects: one call clears the run's
quarantine, the other cancels the run outright, on the same run.

It survived because the three checks *read* as a transaction. Nothing in the code said the round
trips could interleave; the doc comment on `PolicyViolationSnapshot` promised "everything
`ViolationService` needs to enforce ownership, idempotency, and the never-revive-a-terminal-run
invariant" — a snapshot is not a lock, and nothing enforced that the check and the later write
shared one point in time.

The fix moves the guard into the same transaction as the write. `resolve_policy_violation`'s
`apply` closure now runs `UPDATE ... WHERE violation_id = ?4 AND resolution IS NULL` and checks
the affected-row count: zero means either the row already carries a resolution — read back
inside the same transaction, so nothing else can have changed it since — or the violation never
existed. For `"release"`, the terminal-run check moved inside the same closure too, reading
`runs.state` from the same transaction and returning `RunSettled` if it's already terminal. The
`UPDATE` is deliberately ordered ahead of that check, so an already-decided violation is reported
as `AlreadyResolved` even if its run has separately settled — the same precedence the deleted
pre-checks had. Returning `Err` from an `append_and_apply` closure discards the whole transaction,
so a refused decision leaves neither the write nor the event it would have journaled;
`events.sequence` is a plain `INTEGER PRIMARY KEY`, not `AUTOINCREMENT`, so a rolled-back append
burns no sequence number either.

`ViolationService::decide` still makes the same two round trips it always did — a snapshot, then
a write — but ownership is now the only check left in the snapshot's in-memory result;
`PolicyViolationSnapshot` shrank to the four fields ownership actually needs (`run_id`, `task_id`,
`worker_id`, `owner_client_instance_id` — `resolution` and the run's state came out, since nothing
outside the guarded write may safely act on either). The conflict and terminal-run checks that
used to run against that stale in-memory snapshot now run inside the write's own transaction
instead. Two new `DomainError` variants, `AlreadyResolved` and `RunSettled`, carry the guard's
verdict back out; `ViolationService::decide` matches on them directly, and `From<DomainError> for
ServiceError` gained the same two arms so a future caller of the guarded write cannot surface a
caller-caused conflict as an internal error.

`crates/runtime/tests/policy_violation.rs` is new — `decide` had no dedicated Rust suite before
this, only RPC-level coverage that cannot interleave two calls. Two of the four tests drive
genuinely concurrent decisions with `tokio::join!(biased; ...)` in a single task, never
`tokio::spawn`.
Plain (non-`biased`) `join!` rotates which branch it polls first on every poll of the combined
future — a documented fairness mechanism, not an argument-order guarantee; an earlier draft of
this entry and the test file wrongly assumed argument order held on every poll, a mistake an
adversarial review caught. `biased;` pins polling to declaration order on every poll instead, so
the first-declared future always enqueues its next `run_domain_op` command before the second is
polled, making enqueue -- and thus processing -- order reproducible. The underlying guarantee the
tests defend does not actually need `biased`: since both calls share one task and the actor is a
strictly FIFO single consumer, their sends can never be simultaneous or unordered from the
actor's point of view, so the guarded `UPDATE ... WHERE resolution IS NULL` always admits exactly
one writer regardless of which call is enqueued first — `biased` only makes *which* call wins
reproducible, and every assertion still derives its expectation from whichever call actually
returned `Ok` rather than assuming a winner, as a second line of defense. `tokio::spawn` would
remove even that: each call would run on its own independently scheduled task, free to enqueue in
whatever order the executor picks. Checked empirically too: twenty runs of each concurrent test
with no flake, both before and after `biased` was added.
`concurrent_release_and_cancel_admit_exactly_one_decision` interleaves a `release` and a `cancel`
for the same violation and asserts exactly one `Decided`, one `Conflict`, one journaled
`PolicyViolationDecided` event, and only the winner's side effect visible in the run's projected
state. `concurrent_identical_releases_journal_one_event_and_report_already_decided` does the same
with two identical resolutions: one `Decided`, one `AlreadyDecided`, one event — the
idempotent-replay contract the deleted pre-check used to serve.
`deciding_the_same_resolution_twice_sequentially_stays_idempotent` proves the same idempotency
without concurrency, as a control.
`releasing_a_violation_whose_run_has_already_settled_is_refused` settles the run first,
sequentially, then calls `decide("release")` — no `join!`, no timing dependency — and still
proves exactly what changed: `PolicyViolationSnapshot` no longer carries `run_state`, so the
guard's own live read of `runs.state` inside `resolve_policy_violation`'s transaction is the only
thing left that can refuse it.

Falsifiability was checked mechanically, not asserted: with the `resolution IS NULL` guard and its
affected-row check removed, three of the four tests failed
(`concurrent_release_and_cancel_admit_exactly_one_decision`,
`concurrent_identical_releases_journal_one_event_and_report_already_decided`, and
`deciding_the_same_resolution_twice_sequentially_stays_idempotent` — a stronger falsification than
either alone requires); with the terminal-run check removed,
`releasing_a_violation_whose_run_has_already_settled_is_refused` failed. Both guards restored,
`git diff` against the committed tree came back empty, and all four passed again.

The adversarial review that followed asked six questions and found two defects that were already
real at that point, both caught and fixed within this same pass: the `join!` argument-order claim
in the test module doc and this Part's own prose (`biased;` added, the explanation corrected
above), and the "three separate round trips" misstatement of the pre-fix code's actual shape
(pre-fix `decide` always made exactly two round trips — one snapshot from which all three checks
were read in memory, one write — corrected above and in `engineering-lessons.md`). It also raised
two Warnings. The first: the fourth test's original design — racing `decide("release")` against
the run-settling transition through `tokio::join!`, exactly like the first two tests — rested its
"fully deterministic regardless of `biased`" claim on an actor-reply-timing assumption:
vanishingly unlikely to fail (a handful of CPU instructions against real SQLite work measured in
microseconds) but real, not a scheduling guarantee. That test was rewritten to settle the run
sequentially before calling `decide`, removing the timing dependency entirely rather than
accepting a weaker claim. The second: `ApprovalService::decide`
(`crates/runtime/src/approval/service.rs`) carries the identical unguarded-`UPDATE` pattern this
fix just closed for policy violations — a real, reachable race, structurally the same bug, in a
different service. It is out of this change's scope and is registered as R70 in `REVIEW.md`
rather than fixed here. Four Suggestions came back clean: two partial `# Errors` lists, one
FK-guarded-unreachable error reclassification (a missing `runs` row would surface `-32603` instead
of `-32602`, impossible today under `foreign_keys = ON` and no delete path), and one
side-effect-failure edge case (a decision can commit and then fail to apply its side effect,
identical before and after this diff) — all pre-existing, none altered by this change, none
warranting action here.

Measured on the final tree: `cargo fmt --all --check` and `cargo clippy --all-targets
--all-features -- -D warnings` clean; `BATMAN_DISABLE_VENDOR_CLI=1 cargo test --workspace
--no-fail-fast` ran all 50 test binaries to completion — 748 passed, the two standing live-CLI
ignores in the claude and codex adapters, and the one standing failure every entry since Part XIV
has carried, the locally installed Copilot CLI 1.0.80 still not in `COPILOT_KNOWN_CLI_VERSIONS`;
`bun run generate --check` clean (no `crates/protocol` type moved, so neither the schema nor the
bindings did either); `bun test packages` 139 passed, 0 failed, 334 assertions, fourteen files.

R54's fix drops the pre-existing High tier from three to two (R33, R44) on its own; the review
above immediately reopens it to three by surfacing R70, the identical bug in
`ApprovalService::decide`.

## Part XX — The same race, one service over: the approval that could be decided twice

`ApprovalService::decide` had the same shape Part XIX just finished fixing in the sibling service,
down to the round-trip count: one `self.load_snapshot(approval_id).await` round trip, checked in
memory for ownership, `humanRequired`, a conflicting decision, and a settled run, then a second,
separate `run_domain_op` round trip that wrote unconditionally. `db/actor.rs`'s single-owner
thread processes one whole boxed closure at a time off its bounded channel — it serializes
closures, never a service's sequence of round trips about them — so two concurrent
`approval/decide` calls for the same `approvalId` could both have their snapshot read
`decision: None` before either wrote. `decide_approval`'s `UPDATE approvals SET decision = ?1,
decided_at = ?2, decided_by = ?3 WHERE approval_id = ?4` carried no `decision IS NULL` guard and no
affected-row check. Both writes would land, both callers would proceed past the write to fire
their side effects — one call's `self.callback.acknowledge` telling the waiting worker to
proceed, the other's telling it to stand down — and both would journal an `approvalDecided`
event for a single decision that was supposed to happen exactly once.

It was not found by inspecting `ApprovalService` on its own; it was found by Part XIX's own
adversarial review of the policy-violation fix, which asked whether any sibling service carried
the identical shape and swept the codebase for it. `decide_approval` and
`resolve_policy_violation` had drifted into structurally identical code — same snapshot-then-write
split, same unguarded `UPDATE`, same reliance on an in-memory check that was stale the moment a
second caller's round trip landed between the first caller's read and its write — and the review
registered the second instance as R70 rather than fixing it in the same pass, the same discipline
that had produced R54 in the first place.

The fix is identical in mechanism, not just in shape. `decide_approval`'s `append_and_apply`
closure now runs `UPDATE approvals ... WHERE approval_id = ?4 AND decision IS NULL` and checks the
affected-row count inside the same transaction: zero means either a decision already exists — read
back with a second query inside that transaction, so nothing else can have changed it since — or
the approval never existed. The terminal-run check moved into the same closure too, reading
`runs.state` from the same transaction and returning `DomainError::RunSettled` if it is already
terminal. The `UPDATE` is deliberately ordered ahead of that check, so an already-decided approval
reports `AlreadyResolved` even when its run has separately settled — the same precedence
`resolve_policy_violation` uses for the identical ordering question, so the two services cannot
disagree about which fact wins when both are true. Returning `Err` from the closure discards the
whole transaction, so a refused decision leaves neither the write nor the event it would have
journaled, and `self.broadcast` — the next line in `ApprovalService::decide` after the guarded
write's `Ok` — is never reached on that path: a losing racer's transaction never commits and can
never broadcast. `DomainError::AlreadyResolved` and `RunSettled` needed no new variants; Part
XIX's generalization (`kind`/`id`/`existing` fields, not policy-violation-specific ones) is reused
as-is, and `ApprovalService::decide` matches on them directly instead of re-deriving a verdict from
the stale snapshot. On `AlreadyResolved` with `existing == decision`, `decide` returns
`DecideOutcome::AlreadyDecided` before `self.broadcast`, before `self.callback.acknowledge`, and
before the `working`-state transition that follows a fresh decision — an identical replay re-runs
none of a decision's side effects, only the first caller to actually change the row does.
`ApprovalSnapshot` no longer carries `decision` or `run_state` — the two fields the guard now
owns — but still carries `run_id` (the pending `working`-transition target) and `run_flags`
(read on a failed callback to set `protocolUnhealthy` instead of re-asking). No *decision* write
mutates `owner_client_instance_id` or `human_required`, so a losing racer's decision cannot
invalidate either pre-check; ownership itself can still change between the snapshot read and the
guarded write through the unrelated reconcile path's task-ownership rebind, an interleaving this
fix does not touch.

`crates/runtime/tests/approval_decide_race.rs` is new, 481 lines, four tests — `decide` had no
suite of its own that could interleave two calls. `approval.rs`'s existing coverage drives
`approval/decide` through the RPC harness: one `client.call(...)` awaited to completion before the
next is issued, which proves ownership, idempotency, settled-run rejection, and callback semantics
one at a time but can never have two decides in flight against the same approval at once. The new
file talks to `ApprovalService` directly so two `decide` futures can share one task. The first two
tests, `concurrent_approve_and_deny_admit_exactly_one_decision` and
`concurrent_identical_approvals_journal_one_event_and_invoke_the_callback_once`, race two `decide`
calls with `tokio::join!(biased; ...)` — never `tokio::spawn`, and never plain `join!`, which
rotates which branch it polls first on every poll of the combined future rather than guaranteeing
argument order, a mistake an earlier draft of the analogous R54 file made and this file's own doc
comment corrects. `biased;` pins polling to declaration order on every poll, making the actor's
enqueue order — and thus which call wins — reproducible; the guarantee the tests actually depend on
does not need `biased` at all, since both calls share one task and the actor is a strictly FIFO
single consumer, so their sends can never be simultaneous or unordered from the actor's point of
view regardless of enqueue order, and the guarded `UPDATE ... WHERE decision IS NULL` always admits
exactly one writer either way. Every assertion still derives its expectation from whichever call
actually returned `Ok`, as a second line of defense.
`deciding_the_same_decision_twice_sequentially_stays_idempotent` proves the same idempotency
without concurrency, as a control. `deciding_an_approval_whose_run_has_already_settled_is_refused`
deliberately does not `join!` `decide` against the run-settling transition, even though that would
look like the more direct test of "the run settles mid-decide": an adversarial review of the
analogous R54 test found a residual timing gap in that exact shape — `decide`'s first round trip
could in principle resolve inside a single poll, before the run-settling future is ever touched.
This test settles the run first, sequentially, with no timing dependency, then calls `decide`, and
still proves exactly what changed: `ApprovalSnapshot` no longer carries run state, so the guard's
own live read of `runs.state` inside `decide_approval`'s transaction is the only thing left that
can refuse it.

Verification ran the targeted suites named by this change — `cargo test --test
approval_decide_race --test approval --test policy_violation` — 21 tests (4 + 13 + 4), all
passing, `BATMAN_DISABLE_VENDOR_CLI=1` set throughout, with `approval_decide_race` repeated five
further times standalone to rule out a flaky interleaving: six consecutive runs in total, 4 of 4
passing on every run. Falsifiability was checked mechanically, the same way Part XIX checked it:
with the `decision IS NULL` guard and its affected-row check removed from `decide_approval`,
`concurrent_approve_and_deny_admit_exactly_one_decision`,
`concurrent_identical_approvals_journal_one_event_and_invoke_the_callback_once`, and
`deciding_the_same_decision_twice_sequentially_stays_idempotent` all failed — three tests, a
stronger falsification than any one alone requires. Guard restored, the terminal-run guard removed
instead: `deciding_an_approval_whose_run_has_already_settled_is_refused` failed on its own, exactly
the one test whose contract is that guard. Both restored, `git diff` against the committed tree
came back empty, and all four tests passed again.

The adversarial review this fix demanded of itself — the same six-question shape Part XIX's own
review used against `ViolationService` — found no defect in the guarded write path itself; it
surfaced one residual outside R70's mechanism, the reconcile-vs-decide ownership interleaving
noted below, left for separate registration rather than fixed in this pass.
Error classification: `ApprovalError::Conflict` and `RunSettled` both map to `-32602`
(`error_code::INVALID_PARAMS`) in `service/orchestration.rs`, never `-32603` — the misclassification
R54's own review was built to catch does not recur here. Pre-check safety: `grep -rn "UPDATE
approvals" crates/runtime/src` returns exactly the one guarded statement in `decide_approval`, and
neither `human_required` nor the task's owner is ever written by a *decision*, so a concurrent
decision cannot invalidate either pre-check by racing it — the grep was scoped to `approvals`;
`tasks.owner_client_instance_id` is separately mutated by the reconcile path's ownership rebind, a
real, reachable interleaving between reconcile and decide, but not R70's mechanism and out of this
fix's scope. Idempotent replay skips side effects:
traced above — `AlreadyDecided` returns before `broadcast`, before `callback.acknowledge`, and
before the `working` transition, exactly what `plugin-usage.md`'s decision semantics claim, and no
caller anywhere depends on the callback firing a second time. Rollback completeness: a losing
`append_and_apply` closure's `Err` discards its appended event with it, `self.broadcast` sits after
the guarded write's `Ok` arm and is unreachable from the `Err` arms, so a losing racer emits no
`approvalDecided` envelope to any subscriber — the "every mutation commits and broadcasts, exactly
once" invariant holds. Guard ordering: `decide_approval`'s `UPDATE` precedes its terminal-run check,
matching `resolve_policy_violation`'s identical ordering, so the two services cannot disagree about
which fact wins when an approval is already decided on an already-settled run. The one known
neighbor the review deliberately left alone: `decide_approval`'s `let _ = reason;` still discards
the approval's `reason` field end to end — that is R59, already open in `REVIEW.md`, unrelated to
this race, and unchanged by this fix.

## Part XXI — A feature flag for one tool, three broken content addresses

`d1ef420` enabled `serde_json`'s `preserve_order` feature for a sound reason. The conformance
fixture-capture scrubber must preserve a vendor frame's original object-key order: recapturing a
frame must not rewrite a committed fixture merely because the scrubber rebuilt its map. But that
feature changes `serde_json::Map` from its default sorted behavior to insertion order. Two equal
JSON values assembled through different insertion histories now serialize to different bytes.
The feature was local in intent and global in effect.

Three boundaries had silently turned those bytes into content addresses. `Redactor::sanitize_json`
serializes operation intent and acknowledgement payloads for durable storage;
`WorkerProfile::fingerprint` hashes the profile content that profile registration persists and later
serves as `profileRef.fingerprint`; and `RuntimePolicy::compute_fingerprint` hashes the merged
policy persisted with each run. Each boundary promised an identity property that JSON object order
cannot supply. The first two comments still claimed the workspace did *not* enable
`preserve_order`. The one test named for the third property,
`config.rs::fingerprint_is_deterministic`, was worse than missing coverage: it merged the locked
fixtures twice, both merges rejected the locked `max_workers` setting, and it compared the two
equal error strings without ever computing a fingerprint.

Removing `preserve_order` would have made the three hashes appear deterministic again, but at the
cost of rewriting the frames R44 needs to recapture byte-for-byte. The fix instead draws the
boundary where the property is needed. `canonical_json` uses serde_json's first-party
`Value::sort_all_objects` API to sort every object recursively in place, preserving array order;
the borrowed `canonicalize` wrapper clones only when a caller cannot give up ownership.
`sort_all_objects` landed in serde_json 1.0.129, the exact source minimum `Cargo.toml` now
declares. Redaction and profile fingerprinting own their trees -- the fingerprint's canonical
`Value` is a throwaway serialization, not a stored-profile mutation -- and call the in-place form.
Policy hashing and the raw permission-envelope comparison borrow their trees and use the wrapper.
The old vendor-frame path does neither, so capture keeps its original order.

Sorting only the sanitized side of `permission_envelope_contains_secret_shape` initially exposed a
collateral false rejection: an otherwise harmless envelope with unsorted keys no longer textually
matched its sorted sanitized form and looked secret-shaped. Both sides are now canonical before
comparison, leaving redaction as the only possible source of a difference. The profile's final
sort is deliberately defense in depth, not evidence that a current bug was otherwise observable:
the struct declaration already fixes the top-level field order and its sanitized
`permissionEnvelope` is already canonical. Removing that final sort did not fail by the current
construction, but it protects any future free-form field from reviving the dependency on insertion
order.

The adversarial review found a separate redaction edge while checking this boundary. Sorting the
finished redacted object makes its output order stable, but cannot choose between two different
source keys that both redact to the same key: insertion-order-dependent last-wins behavior had
already chosen the value before the final sort. `redact_json_value` now sorts source keys *before*
redaction, so a collision deterministically selects the lexicographically greatest source key.
The collision regression test supplies the same two secret-shaped keys in both insertion orders
and observes the same surviving value.

Falsifiability was mechanical, not a claim that green tests were enough:

| Temporary removal or regression | Test that failed |
|---|---|
| Redaction's final canonical sort | `sanitize_json_is_byte_identical_for_two_differently_ordered_equal_objects` |
| Policy fingerprint canonicalization | `key_order_in_the_yaml_layers_does_not_change_the_fingerprint` |
| Canonicalizing the raw permission-envelope side | `a_permission_envelope_with_unsorted_keys_is_not_mistaken_for_a_secret` |
| Sorting source keys before redaction | `sanitize_json_resolves_redacted_key_collisions_independently_of_input_order` |

Each edit was restored after its named test failed. `cargo test sanitize_json --lib` returned 5/5,
and `cargo test --test adapter_contract --test config` returned 23/23. No committed fixture or
test pins a computed digest, so the one-time change to canonical digest bytes is safe: it changes
no externally committed value while ensuring equal future content receives one address.

## Part XXII — The capture pipeline that graded its own homework

R44 found two failures in the same tool, one hiding the other. `crates/runtime/src/conformance/
capture.rs` writes a fixture with `fs::write`, then decided whether it had changed by re-reading
that same just-written file and comparing it to the bytes it had just written — `unchanged` was
`true` on every real capture and `false` on every dry run, regardless of what had been committed
before. Its own doc comment claimed the flag meant "identical to what was already committed" —
true only if the comparison happens before any write — but the code computed it *after* `fs::write`
had already replaced the file, so the comparison was against itself. Underneath that, the
scrubber that must turn a live vendor turn into fixture bytes reduced "already canonical" to one
specific literal: `stable_session_id`/`stable_uuid` special-cased the exact `11111111-…`/`a0000000-…`
prefixes `claude/initialize.jsonl` happens to use and passed them through unchanged; everything else
fell through to a renumbering path. The one round-trip test, `scrubbing_scrubbed_fixture_is_identity`,
only ever exercised that one fixture, so nothing proved the other ten fixtures — each in a different
vendor's own ID shape — would survive a scrub, let alone a fresh capture.

`aaef59e` fixed the write side first: `persist_fixture_content` now reads the existing file *before*
deciding anything (`capture.rs:218-248`), returns that comparison as `unchanged` honestly, and a
dry run never calls `fs::write` at all — `persist_fixture_content_dry_run_reports_differences_
without_mutating` and its two siblings pin all three outcomes (equal, different, missing) under
both real and dry-run modes.

The scrubber side took longer because the one hardcoded special case was masking two separate
defects. `stable_uuid` generated placeholders from a bare monotonic counter, not a value → placeholder
map, so the *same* raw `uuid` appearing twice in one capture got two different stable ids — the
correlation a fixture-based conformance suite depends on was never actually preserved for anything
but session ids. `45934b0` and `c72fcdd` replaced both the passthrough and the counter with one rule:
every session id and every raw `uuid` is canonicalized through a `HashMap<String, String>` keyed by
first encounter, with no exception for values that already look canonical. A fixture already in
canonical form re-derives the same numbering from its own encounter order; a fresh capture derives
it for the first time — both paths are now the same code.

That still left the values the original scrubber never touched at all: its own module comment
said it preserved "the correlation ids that conformance suites assert on," which in practice meant
`tool_call_id`, `messageId`, `hook_id`, `itemId`, and `agentId` were written into fixtures verbatim
— real, one-time identifiers a live vendor CLI generates fresh on every turn. `d362a07` pinned the
gap with a failing test, `correlation_ids_are_renumbered_by_family_and_encounter_order`, expecting a
`msg-`/`tool-`/`hook-`/`item-`/`agent-` family placeholder that didn't exist yet; `08144b5` built it:
`correlation_family` classifies a value by its key (or, for a bare `id`, by its grandparent object —
`message`, `tool_use`, `item`) and `stable_correlation_id` renumbers within that family by first
encounter, mirroring the session/uuid mechanism exactly. Three fixture-consuming tests had been
asserting against the raw vendor values that no longer existed once fixtures were migrated —
Copilot's `result_usage_artifacts_scenario` matched a literal `"call-2"` (`d9d5be1`, now
`tool-000000000002`), and an OMP-RPC usage test asserted a live-captured cost of `0.0007` instead of
the canonical `0.0142` (`5cb7b82`) — both were the correlation fix working as intended, not new bugs.

`correlation_family`'s fallback — recognizing a family from a raw value's own prefix, for captures
that reuse a vendor's id shape without a recognizable key — was scoped too broadly on first landing:
it matched the prefix against *any* key's value, not just `id` fields, so a frame carrying
`"subtype":"hook_started"` had its subtype discriminator silently rewritten into `hook-000000000001`
because the string happened to start with `hook_`. `fe02a28` restricted the fallback to `key == "id"`;
`a6d2b9c`'s `correlation_prefixes_are_only_normalized_in_id_fields` feeds exactly that frame through
and asserts `subtype` and a `text` field reading `"msg-not-an-id"` both survive untouched while
`hook_id: "hook-real"` still canonicalizes.

`08144b5` closed two more gaps in the same pass: `command` values — an absolute path to whichever
vendor binary happened to be installed locally — collapse to their basename under a fixed prefix
(`normalize_command_path`), and the RFC 3339 timestamp heuristic, previously "contains `T`, ends in
`Z`" (loose enough to misfire on ordinary text), now requires a real date-shaped prefix. Both are
defended by one test, `normalizes_command_paths_without_misclassifying_prose_or_nested_turns`, whose
`prose: "meetingTendsAtZ"` field is the adversarial input the old heuristic would have corrupted.
The same commit also reverted `scrub_line` to JSON-only — non-JSON captured lines (vendor banners,
log noise) are dropped instead of redacted-and-kept, since a capture-managed fixture must be JSON on
every line (`drops_non_json_lines`).

Manifest honesty came next: `capture-manifest.yml` had listed a fixture, `claude/result.jsonl`, that
was never actually capturable — it covers an `error_max_turns` edge case no successful prompt can
reproduce, so it had always been synthetic content masquerading as a captured one. `af3f4c3` removed
it from the manifest and documented it, alongside `codex/schema-version.json`, as one of exactly two
fixtures capture deliberately does not manage, leaving eleven real manifest entries.

`d4f08a4` and `3ee9de4` then migrated all eleven to their canonical, encounter-order form, and
`61598c7`/`bf7e32f` rewired the proof itself: `manifest_fixtures_are_scrub_render_fixed_points`
(`capture.rs:499-594`) runs every manifest fixture through `scrub_captured_frame` and
`render_fixture_content` — the exact functions a real capture calls — and asserts the output is
byte-identical to what's committed, closing sub-claim 1 directly for all eleven instead of the one
fixture the old test covered. A second half of the same test walks each fixture directory
(ignoring dotfiles, `1ba7cf8`) and asserts its contents are exactly the manifest set plus the two
named exclusions — a stray file can no longer hide from either the manifest or this test.

Reviewing the consolidated diff caught one regression the tests hadn't: `08144b5` touched both
`scrub.rs` and `capture.rs` in the same sweep, and while restructuring `capture_one`'s frame
collection it silently dropped the `adapter.dispose().await.ok()` call between collecting frames and
scrubbing them. Nothing failed loudly — a leaked vendor CLI process doesn't fail the capture that
leaked it — but every subsequent manifest entry in the same run would have started a new adapter
without the previous one ever being torn down. `4b6935b` restored the call.

No falsifiability table accompanies this Part the way Part XIX–XXI's do: R44's proof is the fixed-
point test itself, run against real committed content rather than a temporarily-broken copy —
`manifest_fixtures_are_scrub_render_fixed_points` fails the moment any of the eleven fixtures stops
being reproducible by the exact pipeline a live capture runs, which is the property sub-claim 1
asked for. Combined with `persist_fixture_content`'s six pre/dry-run cases, the thirteen scrubber
tests in `scrub.rs`, and `capture_status_distinguishes_unchanged_rewritten_and_would_rewrite`
(`cli.rs`) pinning the three states a capture can report, the tool that produces every committed
conformance fixture is now proven correct against all eleven of them, not the one it happened to be
built against.

## Part XXIII — The same guarded write, one interleaving further: the decider that no longer owned the task

Part XX's own adversarial review found this one and, following the same discipline that had
produced R70 in the first place, registered it rather than fixing it in the same pass:
`decide_approval`'s guarded transaction (hardened for R70) checked `decision IS NULL` and the
run's terminal state, but never re-read `tasks.owner_client_instance_id`. `ApprovalService::decide`
still authorized ownership the old way — a single `load_snapshot` round trip, compared to the
caller's `principal_instance_id` entirely in memory, before the guarded write was ever reached.
`reconcile/omp`'s ownership rebind (`DomainRepository::reconcile_ownership`, an unguarded `UPDATE
tasks SET owner_client_instance_id = ...`) is a separate, single round trip that can commit in the
window between that snapshot read and `decide_approval`'s write. A caller who owned the task at
snapshot time, then lost it to a rebind before its write landed, still reached and won the guarded
write — deciding an approval for a task it no longer owned. R70's guard closed the
decide-vs-decide race; this is decide-vs-rebind, a different pair of operations racing the same
guarded write, and it was Part XX's text itself that named the gap: `tasks.owner_client_instance_id`
"is separately mutated by the reconcile path's ownership rebind, a real, reachable interleaving
between reconcile and decide, but not R70's mechanism and out of this fix's scope."

`crates/runtime/tests/approval_owner_race.rs` (03ac6e2, 351 lines) made that interleaving
deterministic the same way `approval_decide_race.rs` made R70's: `tokio::join!(biased; ...)`
against the single-owner DB actor's strictly FIFO command processing. `decide` is two round trips
(`load_snapshot`, then `decide_approval`); the rebind is one. `biased` polling `decide` first on
every poll guarantees `load_snapshot`'s command is enqueued — and thus processed — before the
rebind's `UPDATE`, and guarantees the rebind commits before `decide`'s second command can possibly
be sent, since that send cannot happen before `load_snapshot`'s reply wakes `decide` for another
poll, which cannot happen before the rebind's own synchronous first-poll send. Run against the
pre-fix code, `a_stale_owner_that_passed_the_pre_check_is_refused_by_the_guarded_write` failed RED
exactly as designed: `decide` returned `Ok(Decided)` for a caller the rebind had already dispossessed,
because the caller-side pre-check read the original owner and passed, and the unguarded write had no
ownership check left to refuse it with. A second test, `the_new_owner_can_decide_after_a_rebind`,
guarded the eventual fix against over-rejection — a legitimate new owner, deciding sequentially
after the rebind, must still succeed — and passed unmodified throughout, since nothing about a
correct decide-after-rebind was ever broken.

`31cb763` moved ownership out of the caller-side pre-check and into the guarded transaction itself,
mirroring R70's own move of the conflict and terminal-run checks one commit earlier: inside
`append_and_apply`'s closure, before the `UPDATE approvals ... WHERE decision IS NULL` guard,
`decide_approval` now re-reads `tasks.owner_client_instance_id` for the task the approval belongs
to and compares it against a newly threaded `principal_instance_id` parameter. A mismatch returns
the new `DomainError::NotOwner { task_id, instance_id }`; a missing task row returns the existing
`DomainError::NotFound { kind: "task", .. }`. `ApprovalService::decide` deleted its caller-side
ownership check entirely and instead maps `DomainError::NotOwner` to the `ApprovalError::Forbidden`
variant the deleted pre-check used to return directly — the error a caller sees is unchanged, only
where it's decided moves. `run_id` and `human_required` stay caller-side pre-checks from the
snapshot, since a decision write never mutates either field, so neither can go stale in the window
this fix closes. With ownership no longer part of `ApprovalSnapshot`'s job, the now-unused
`owner_client_instance_id` field and the `JOIN tasks` that populated it were dropped from the
snapshot and its query.

Because the ownership check runs before the decision `UPDATE`, not after, it outranks idempotent
replay: a former owner replaying its own identical decision after losing ownership gets
`ApprovalError::Forbidden`, not `alreadyDecided` — the caller-side ownership fact wins over the
row's decision history, the same precedence Part XX gave the decision-vs-terminal-run ordering
question. An unauthorized decide still costs a rolled-back write transaction, not a free rejection,
but that costs nothing durable: `events.sequence` is a plain `INTEGER PRIMARY KEY`, not
`AUTOINCREMENT`, so a transaction that returns `Err` from inside `append_and_apply` burns no
sequence number and leaves no gap in the journal. Verification ran the RED test against the fix and
the R70 suite together — `approval_owner_race` (2 tests) plus `approval_decide_race` (4 tests),
6 of 6 passing — confirming the new ownership arbitration didn't disturb the decision-vs-decision
guard it now shares a transaction with.

The adversarial review this fix demanded of itself found two real defects, both fixed in follow-up
commits rather than folded into the mechanism commit. First: `DomainError::NotOwner` fell through
to `From<DomainError> for ServiceError`'s catch-all `internal(...)` arm for any call site that
converts a raw `DomainError` without going through `ApprovalError` first — one such call site away
from a stale-owner rejection surfacing as `-32603` instead of a caller error, the exact
misclassification class R54's review was built to catch. `dd7804d` added the explicit
`NotOwner` → `error_code::INVALID_PARAMS` arm alongside the sibling `NotFound`/`AlreadyResolved`/
`RunSettled` arms. Second: the test file's own module doc, and `violation.rs`'s cross-reference to
it, had gone stale mid-review — both still narrated the race in the present tense, as still open,
and `violation.rs` claimed `ViolationService::decide` shared `ApprovalService::decide`'s
caller-side ownership pattern, which this fix had just made false for the approval side.
`b68324b` rewrote `approval_owner_race.rs`'s header as resolved-contract narration (the pinned
`biased` interleaving is no longer load-bearing for correctness, only for reproducibility, now that
the guarded write arbitrates ownership under every interleaving, not just the one `biased` pins),
renamed its first test to `a_stale_owner_is_refused_by_the_guarded_write_after_a_rebind`, corrected
`approval_decide_race.rs`'s header to note only `humanRequired` remains a caller-side pre-check
post-R71, and corrected `violation.rs`'s doc comment to say ownership checking has diverged: the
violation side still pre-checks it caller-side, unchanged, while the approval side now arbitrates
it inside the guarded write.

The same review surfaced two more residuals outside this fix's mechanism, registered rather than
fixed in this pass — the same discipline that produced R71 itself out of Part XX's review of R70:
**R72**, the identical reconcile-vs-decide race one service over, in `ViolationService::decide` and
`resolve_policy_violation`, which never adopted R71's guarded re-read; and **R73**, a lost update on
`RunFlags` across `ApprovalService::decide`'s awaited vendor callback — `set_run_flags` is a blind
whole-struct write with no compare-and-swap, so a `policy_quarantined` flag set concurrently by
`ViolationService::apply_action` during that await can be silently reverted by the callback-failure
branch's stale copy. Both are open in `REVIEW.md` as of 2026-08-18.

## Part XXIV — The same guarded write, one service over: the violation that no longer had an owner

R71's own adversarial review found this one and, following the same discipline that had produced
R71 out of Part XX's review of R70, registered it rather than fixing it in the same pass:
`ViolationService::decide` still checked task ownership as a caller-side pre-check, unchanged by
anything R71 did to the sibling approval path. `decide` read a `PolicyViolationSnapshot` via one
`policy_violation_snapshot` round trip — a query that joined `tasks` to include
`owner_client_instance_id` alongside the violation's `run_id`/`task_id`/`worker_id` — then compared
that snapshot's owner to the caller's `principal_instance_id` entirely in memory, before the guarded
write was ever reached. `resolve_policy_violation`'s transaction guarded only `resolution IS NULL`
and the run's terminal state; it never re-read `tasks.owner_client_instance_id`.
`reconcile/omp`'s ownership rebind (`DomainRepository::reconcile_ownership`, the same unguarded
`UPDATE tasks SET owner_client_instance_id = ...` Part XXIII cited for the approval side) is a
separate, single round trip that could commit in the window between that snapshot read and the
write. A caller that owned the task at snapshot time, then lost it to a rebind before its write
landed, still reached and won the guarded write — resolving a policy violation for a task it no
longer owned. Identical mechanism to R71, one service over.

`crates/runtime/tests/violation_owner_race.rs` (7076ebd, mirroring `approval_owner_race.rs`) made
the interleaving deterministic the same way Part XXIII's test had: `tokio::join!(biased; ...)`
against the single-owner DB actor's strictly FIFO command processing, `decide`'s two round trips
(`policy_violation_snapshot`, then `resolve_policy_violation`) declared first so `biased` polls it
before a direct `reconcile_ownership` rebind on every poll, guaranteeing the snapshot read enqueues
before the rebind and the rebind commits before `decide`'s second command can possibly be sent. Run
against the pre-fix code, the first test failed RED exactly as designed: `decide` returned
`Ok(DecideOutcome::Decided)` for a caller the rebind had already dispossessed, because the
caller-side pre-check read the original owner and passed, and the unguarded write had no ownership
check left to refuse it with. A second test, `the_new_owner_can_resolve_after_a_rebind`, guarded the
eventual fix against over-rejection — a legitimate new owner deciding sequentially after the rebind
must still succeed — and passed unmodified throughout.

`c02f56a` moved ownership out of the caller-side pre-check and into the guarded transaction,
mirroring R71's own move one commit earlier: inside `resolve_policy_violation`'s `append_and_apply`
closure, before the `UPDATE policy_violations ... WHERE resolution IS NULL` guard, the transaction
now re-reads `tasks.owner_client_instance_id` for the violation's task and compares it against a
newly threaded `principal_instance_id` parameter. A mismatch returns the existing
`DomainError::NotOwner { task_id, instance_id }` — the same variant R71 added for
`decide_approval`, reused rather than duplicated; a missing task row returns
`DomainError::NotFound { kind: "task", .. }`. `ViolationService::decide` deleted its caller-side
ownership check entirely and instead maps `DomainError::NotOwner` to `ViolationError::Forbidden`
from inside the match on the guarded write's result, ahead of the `AlreadyResolved`/`RunSettled`
arms — the same error a caller saw before, only where it's decided moves. `run_id`/`task_id`/
`worker_id` stay caller-side reads from the snapshot, since a decision write never mutates any of
them; only ownership needed to move. With ownership no longer part of the snapshot's job,
`PolicyViolationSnapshot` dropped its now-unused `owner_client_instance_id` field and the
`JOIN tasks` that populated it.

Because the ownership check runs before the `resolution IS NULL` guard, not after, it outranks
idempotent replay on the violation side exactly as Part XXIII established for the approval side: a
former owner replaying its own identical resolution after losing ownership gets
`ViolationError::Forbidden`, not `alreadyDecided`. That precedence held from `c02f56a` on, but
nothing pinned it as a test on either service until `cbf21c9` added
`a_former_owner_replaying_its_identical_resolution_is_refused` to `violation_owner_race.rs` and
`a_former_owner_replaying_its_identical_decision_is_refused` to `approval_owner_race.rs` in the same
commit — decide/resolve as the original owner, rebind to a new owner, replay the identical
decision/resolution as the original owner, assert `Err(Forbidden)` and that the original decision,
event count, and (for approval) callback count are unchanged.

The adversarial review this fix demanded of itself returned NEEDS CHANGES on its first pass:
`violation_owner_race.rs`'s own module doc, written before the fix landed, still narrated the race
in the present tense as though the violation side had not yet been fixed — the same kind of
stale-narration defect Part XXIII's review had found in the approval-side test file. `2fed760`
rewrote the header as resolved-contract narration, mirroring `approval_owner_race.rs`'s post-R71
shape: the pre-fix caller-side pre-check against a stale snapshot, the `biased` interleaving that
produced the committed RED failure, and that ownership is now arbitrated inside the guarded write
itself, so the `biased` enqueue ordering is no longer load-bearing for correctness — only for
keeping the interleaving reproducible. It also corrected the second test's stale comment, which
still called the landed fix "the eventual fix."

The same review surfaced one more residual outside this fix's mechanism, registered rather than
fixed in this pass via `e3e9dc7`: **R74**, task revision monotonicity — `task_upsert` and
`reconcile_omp` both read the stored `tasks.revision` via one round trip, compare it to the
caller-supplied revision in memory, and write in a second round trip, while neither
`upsert_task`'s `ON CONFLICT` write nor `reconcile_ownership`'s `UPDATE` carries a revision
predicate. Since R71 and this fix made `tasks.owner_client_instance_id` the in-transaction authority
both `decide_approval` and `resolve_policy_violation` now trust, that write's correctness is
load-bearing for both of them — a lost update there could silently hand a stale client the ownership
those guarded writes are built to defend. The pattern by now has its own shape: each guarded-write
fix's adversarial review has found the same check-then-act-across-two-round-trips defect one layer
down — R70's review found R71, R71's review found R72 and R73, and R72's review found R74.
`REVIEW.md` is updated in the same pass this Part records to move R72 into resolution history.

## Part XXV — Not a conflict either side detects: the flag write that clobbered its neighbor

`ApprovalService::decide`'s callback-failure branch (`service.rs:248-267`) read
`snapshot.run_flags` once, inside `load_snapshot`, before the decision write and before awaiting
`self.callback.acknowledge` — then, on a callback failure, set only `flags.protocol_unhealthy =
true` on that now-stale in-memory copy and wrote the *entire* six-field struct back via
`set_run_flags(run_id, &RunFlags)`, a blind whole-struct `UPDATE` with no compare-and-swap and no
in-transaction re-read. Any other flag mutated on the same run during the awaited callback window —
most plausibly `policy_quarantined`, set by a concurrent `ViolationService::apply_action` — was
silently reverted to whatever value the pre-callback snapshot happened to hold. `ViolationService`'s
own `set_quarantined` had the identical shape one layer over: read a flags snapshot in one round
trip, mutate it in memory, write the whole struct back in a second. Neither side detects the loss;
it is not a conflict either write refuses, just a value that quietly reverts.

`crates/runtime/tests/run_flags_lost_update.rs` (4c51026) made the interleaving deterministic
without the `biased`-`join!` machinery Parts XX/XXIII/XXIV needed for their ownership races, because
the concurrent mutation here doesn't need to race anything — it can simply run *inside* the callback
`decide` is already blocked on. `QuarantineDuringCallback::acknowledge` performs
`ViolationService::set_quarantined`'s exact read-modify-write shape (read flags, flip
`policy_quarantined`, write the whole struct back) and then fails, guaranteeing the competing write
lands strictly between `decide`'s pre-callback snapshot read and its post-failure write-back. Run
against the pre-fix tree, `a_flag_set_during_the_callback_window_survives_a_callback_failure` failed
RED exactly as designed: `policy_quarantined` came back `false` even though `protocol_unhealthy` was
correctly `true`. A second test, `the_unhealthy_flag_is_applied_when_no_concurrent_mutation_happens`,
pinned the ordinary case — a plain failing callback with nothing else mutating the run — so the
eventual fix couldn't overcorrect by dropping the `protocol_unhealthy` write it exists to make.

`a2c07c2` closed both call sites at once with a single new primitive: `RunFlag`, a closed six-variant
enum naming one boolean field on `RunFlags`, and `DomainRepository::set_run_flag(run_id, RunFlag,
bool)`, which reads the run's *current* row, flips exactly one flag via a total match with no `_`
arm, and writes the whole row back — all inside one call, with nothing else able to observe or
mutate the row in between. The `RuntimeEvent::RunFlagsEvent` it journals carries the post-flip
struct it just built, not the caller's stale copy, so the wire shape is unchanged — still the full
`RunFlags`, not a delta — but its contents can no longer be wrong. `set_run_flags`, the whole-struct
API, was deleted outright rather than kept alongside the guarded one: a repo-wide grep found no
legitimate caller left, only the two lost-update-shaped ones this fix migrated and two test seeders
that needed the same guarded call instead of reaching around the domain layer with raw SQL. Exactly
one function in the workspace now writes the `flags_*` columns — a sole-writer property strictly
easier to defend than the pre-fix scatter. `ApprovalSnapshot` lost its `run_flags` field entirely and
`load_snapshot` dropped the `JOIN runs` and six flag columns that populated it, rather than leaving
either as unused dead weight.

`set_run_flag`'s read runs on `self.conn`, *before* `append_and_apply` opens its SQL transaction —
not inside it, unlike `resolve_policy_violation`'s in-tx re-read from Part XIX. It has to: the event
`set_run_flag` journals carries the post-flip `RunFlags` struct by value, and `append_and_apply`
takes a fully-built `RuntimeEvent` as an argument, so that struct must exist before the closure
handed to it does. What actually closes the gap between this read and its write is not a
transaction, then — it's `DatabaseHandle`'s single-owner actor thread, which runs one
`run_domain_op` closure to completion before starting the next. That makes `set_run_flag` atomic at
*closure* granularity, a stronger boundary than `resolve_policy_violation`'s transaction granularity
but a different one, and the first version of the doc comment describing it didn't say so — it
invoked "R70-R72's guarded-write doctrine" without naming which boundary was doing the guarding, or
that append_and_apply's own signature is why the read couldn't move inside the tx even if a future
reader wanted it to. The adversarial review this fix demanded of itself (`agent://R73Adversary`,
PASS WITH WARNINGS) caught exactly that gap as W1, plus a second doc defect as S1 — `RunFlag`'s
comment called it "internal to this crate" when it's `pub`, re-exported from `domain`, and
constructed directly by integration tests as `batman_runtime::domain::RunFlag`; the true, narrower
claim is only that it isn't a protocol type, so `RunFlagsChanged`'s wire shape is unaffected by it.
`778b644` rewrote both: the doc block now names the actual serializer and the structural constraint
forcing the read before the transaction, and `RunFlag`'s comment states the narrower, true claim.

The review's third warning, W3, found a real coverage gap rather than a doc one: nothing anywhere
asserted the `RunFlagsChanged` *event* `set_run_flag` emits, only the database row its `UPDATE`
leaves behind — and reordering the event's construction ahead of `flag.apply` would leave that row
correct while broadcasting the pre-change struct, with every existing assertion staying green. The
replay contract is exactly the thing this project exists to guarantee, and this was the one mutation
whose event content is computed rather than passed in by a caller. `e5b03bf` closed it: the
callback-window test now takes an explicit broadcast sender, keeps its own receiver, and after
`decide` completes, asserts the received `RunFlagsChanged` envelope carries both
`protocol_unhealthy: true` and `policy_quarantined: true` — pinning the broadcast payload, not just
the row. Verified by a temporary falsifiability mutation rather than a revert (reverting `a2c07c2`
wouldn't even compile, since the test file imports the post-fix `RunFlag` API): reordering
`set_run_flag`'s event construction ahead of `flag.apply` made only the new broadcast assertion fail
(`protocol_unhealthy: false` in the envelope), while both database-row assertions and the sibling
test stayed green; reverted, `git diff` on `repository.rs` came back empty and both tests passed.

The same review surfaced one more residual outside this fix's mechanism, registered rather than
fixed in this pass: **R75**, quarantine state still decided from a caller-side snapshot read one
round trip before the write that acts on it — `record_nested_worker`/`record_cost_ceiling` read
`already_actioned = flags.policy_quarantined || state.is_terminal()` before `record_policy_violation`
commits, and `apply_action` trusts that stale read to short-circuit; `decide`'s release path commits
`resolve_policy_violation` and `set_quarantined(run_id, false)` as two *separate* commits, so a new
violation quarantining the run in between is immediately un-quarantined by the release's second
commit. Both interleavings end with a journaled, unresolved violation on a run whose
`policy_quarantined` is false — the exact silently-un-quarantining harm R73's own priority line
cited, one layer further down. The pattern now runs five deep: R70's review found R71, R71's review
found R72 and R73, R72's review found R74, and R73's review found R75. `REVIEW.md` is updated in the
same pass this Part records to move R73 into resolution history.

## Part XXVI — A guard that overreached: the rebind that couldn't be resumed

R74, registered during Part XXIV's review of R72, was the same check-then-act shape one write
further down the stack: `task/upsert` and `reconcile/omp` each read `tasks.revision` via one
`run_domain_op` round trip, compared it to the caller-supplied revision entirely in memory, and
only then issued a second round trip to write — while neither `upsert_task`'s `ON CONFLICT` arm nor
`reconcile_ownership`'s `UPDATE` carried a revision predicate of its own. Since R71 and R72 made
`tasks.owner_client_instance_id` the in-transaction authority `decide_approval` and
`resolve_policy_violation` now trust, a lost update on *this* write could hand a stale client
ownership those guarded reads would then treat as legitimate.

`crates/runtime/tests/task_revision_race.rs` (`d745477`, 305 lines) made the interleaving
deterministic the same way `approval_owner_race.rs` and `violation_owner_race.rs` had: since
`service::query` is `pub(crate)` and `task_upsert`/`reconcile_omp` are private, the test drives the
repo/db layers directly, with `tokio::join!(biased; ...)` pinning the actor-FIFO two-round-trips-
per-caller ordering. Run against the pre-fix tree, `concurrent_upserts_cannot_move_a_revision_backwards`
failed RED: two callers whose pre-checks both read stored revision 3 both landed their unconditional
writes, revision 4 last, leaving `(4, "omp-3")` even though revision 5 had already been presented and
wrongly overwritten. `concurrent_reconciles_with_the_same_revision_admit_exactly_one_rebind` failed
RED the same way. A third test, `a_stale_upsert_arriving_after_a_newer_one_is_refused_sequentially`,
already passed and had to keep passing post-fix — it isn't a concurrency test at all, just the
ordinary sequential case with no caller-side pre-check left to catch it once the fix removed one.

`9a78e74` moved both guards into their writes: `upsert_task`'s `ON CONFLICT` arm gained
`WHERE excluded.revision >= tasks.revision`, refusing a lower revision inside its own transaction as
`DomainError::RevisionTooLow`; `reconcile_ownership`'s `UPDATE` gained `AND revision = ?`, refusing a
mismatch as `DomainError::RevisionMismatch`. Both caller-side pre-checks were deleted. But the fix
went one step further than the guard required: a successful rebind also *consumed* the presented
revision, advancing the stored value to `revision + 1` and returning it from the RPC, specifically so
that two reconciles racing at the same revision would admit exactly one winner — the loser's
predicate would no longer match. To keep a twice-restarted OMP able to reclaim its own tasks after
that consumption, the extension side (`index.ts`, `reconcile.ts`, `tasks.ts`) grew a persisted
"advanced correlation," written to the session log after every successful rebind. (An unrelated
`b70a197` rustfmt-only pass for R73's flag work landed in between, touching `approval/service.rs`
and `run_flags_lost_update.rs` — no part of this mechanism.)

The adversarial review this fix demanded of itself returned **NEEDS CHANGES**, and the two errors it
found were a real functional regression, not a doc defect: **E1**, `task/upsert`'s resume path always
presents `revision: 0` (`INITIAL_TASK_REVISION`) — once a reconcile had consumed the revision and
advanced the stored value past 0, every subsequent resume upsert would present a revision the write
now correctly, and permanently, refused as too low. **E2**, reclaim itself had become single-use:
idempotent replay depended entirely on the extension's best-effort session-log append landing and
surviving, with no guard against a lost or never-written entry leaving a client unable to reclaim a
task it had every right to. The exactly-one-rebind property the consumption existed to buy wasn't
even necessary — it duplicated a guarantee R71 and R72 already provide one layer up.

`7b70875` kept the `AND revision = ?` guard and dropped the consumption: `reconcile_ownership`'s
`UPDATE` no longer touches the `revision` column, so reclaim is idempotent again — a repeated or
retried reconcile at the same revision simply succeeds, last reconciler wins — and the
exactly-one-*owner* property lives where it actually belongs: a usurped owner is refused not at
rebind time but at decision time, by the same R71/R72 in-transaction re-read of
`tasks.owner_client_instance_id` inside `decide_approval`/`resolve_policy_violation`. The RPC's
`revision` result field and the extension's advanced-correlation machinery were reverted outright,
not deprecated alongside a replacement — `index.ts`, `reconcile.ts`, and `tasks.ts` went back to
their pre-`9a78e74` shape. `concurrent_reconciles_with_the_same_revision_admit_exactly_one_rebind`
was replaced with `a_reconcile_presenting_a_stale_revision_is_refused`, which pins the corrected
contract directly: a reconcile at a now-stale revision is refused with the actual stored revision
classified in the same transaction and nothing journaled, a reconcile at the current revision
rebinds, and a second reconcile presenting that same unconsumed revision *also* rebinds — final state
`(5, "omp-4")`, `reconcile_event_count() == 2`, one event per admitted rebind. `orchestration_rpc.rs`
gained `task_upsert_at_the_same_revision_still_succeeds_after_a_reconcile`, the first test to cover
`task/upsert` after `reconcile/omp` at the RPC boundary at all: upsert at revision 7, reconcile to a
new owner at 7, `task/get` still reports revision 7, a resume upsert at 7 from the new owner succeeds,
and an upsert at 6 is refused with `-32602` and the byte-pinned legacy message `"revision 6 is lower
than stored revision 7"`.

The scoped re-review that followed (`agent://R74Adversary`, range `b70a197..7b70875`) returned
**PASS WITH WARNINGS**: E1 and E2 both confirmed addressed — a repo-wide grep found
`advancedCorrelation` gone entirely, and the new RPC test pins exactly the interaction E1 broke. Most
of the first pass's warnings turned out moot once re-verified against the repaired code (the RPC
result shape, the event's `revision` field, and the tool description had all reverted to a
consistent, correct shape along with everything else), but one, **W4**, was a real narration defect
the repair had introduced: the sequential test's comment called it a "GREEN guard" when the guarded
*repo-layer* write this file drives was RED even in the pre-fix tree (only the since-deleted
service-layer pre-check had caught it) — the same false-tense mistake Parts XXIII and XXIV's own
reviews had already found once each in the sibling owner-race files. `7b70875` corrected it in the
same commit. That correction, though, left one instance of the identical defect one helper over:
`reconcile_omp_round_trips`'s doc comment still claimed a successful rebind "consumes the presented
revision," flatly contradicting both the module header eight lines above it and the test directly
below it. The review flagged it as **W7**; `d26a9dd` fixed it as its own commit, closing the loop on
the third occurrence of a defect class this project now recognizes on sight.

The same review also confirmed a residual the first-pass review had already spotted but left
unregistered: **R76**, `task/upsert` takes no `principal` and threads the caller-supplied
`ownerClientInstanceId` straight into the guarded write, which predicates only on revision — any
connected `ompExtension` client can seize another instance's task outright by presenting its stored
revision, bypassing `reconcile/omp`'s arbitration and the ownership authority R71/R72 built entirely.
`c336505` registered it the same day. The finding chain now runs two directions from Part XX's
original review of R70: R70 → R71 → R72 → R74 → R76 down one branch, R73 → R75 down the other.
`REVIEW.md` is updated in the same pass this Part records to move R74 into resolution history.

## Part XXVII — Whoever committed first: the ownership guard that arrived in someone else's commit

R76, found during Part XXVI's review of R74's fix and registered the same day, was the residual
that review's own W4 already anticipated: `task/upsert` threaded no `principal` through `dispatch`
at all, so the caller's own `ownerClientInstanceId` was read straight off the wire and passed into
`upsert_task` unchanged. The guarded write R74 had just finished hardening
(`ON CONFLICT ... WHERE excluded.revision >= tasks.revision`) arbitrated revision monotonicity and
nothing else -- no predicate anywhere checked who currently owned the row. Any connected
`ompExtension` client could call `task/upsert { taskId, ownerClientInstanceId: "me", revision:
<stored> }` for a task it had never reconciled and seize it outright, bypassing `reconcile/omp`'s
arbitration entirely.

`22458e6` made that concrete. `task_upsert_cannot_seize_ownership_from_another_instance` has
`omp-1` create a task at revision 7, then has `omp-2` -- without ever calling `reconcile/omp` --
upsert the same task at the *stored* revision with its own instance id. RED: the upsert succeeded
and rewrote `ownerClientInstanceId` to `"omp-2"`. The companion half of the same test pins the
legitimate route that must keep working once the guard lands: reconcile first (which itself assigns
the new owner under revision arbitration), then an upsert by the new owner at the stored revision.

The fix landed in two pieces, and the attribution across them is worth recording honestly, because
it happened by accident of timing rather than design. `b1d469f`, committed as R75's own fix for an
unrelated finding (quarantine consistency inside `ViolationService`), touched `repository.rs`
broadly enough that a concurrently-edited, not-yet-committed change to the same function --
the R76 owner clause on `upsert_task`'s `ON CONFLICT` arm -- rode along inside it. The arm now
additionally requires `excluded.owner_client_instance_id = tasks.owner_client_instance_id`, and a
declined write is classified inside the same transaction with deliberate precedence: a stored
revision higher than the one presented is `RevisionTooLow` regardless of ownership -- an owner is
entitled to know its own upsert is stale, and it keeps R74's byte-pinned message stable -- otherwise
the revision would have been accepted and the owner didn't match, so it's `DomainError::NotOwner`.
Neither agent hid the accident. `b1d469f`'s own doc comment on `upsert_task` names R76 directly
("transferring ownership goes through `reconcile/omp`, never through `task/upsert` (R76)"), and
`a508667` -- R76's own commit, landed eighty-six seconds later at 02:35:29 -- says so in its
message: "The repository-layer guard against ownership seizure itself landed already in `b1d469f`
(R75, committed concurrently by a peer agent editing the same file)." Neither commit claims credit
for the other's work.

`a508667` added the half `b1d469f` couldn't have: `dispatch` now threads `principal` into
`task_upsert`, which refuses `ownerClientInstanceId != principal.instance_id` as `INVALID_PARAMS`
before the guarded write is ever reached. The doc comment is explicit that this is *not* the R76
race guard -- it's param validation against an identity the connection layer already authenticated,
the same class of check `reconcile/omp` already performs against its own `new_owner`. Task creation
(no existing row) still binds ownership to the presented id unconditionally, since there is no prior
owner to protect. `task_revision_race.rs`'s three R74 tests, which had contended revisions from
*different* owners against one seeded task, were updated to contend from the *same* owner instead,
so they keep isolating revision monotonicity from R76's now-orthogonal ownership guard.

The adversarial review (`agent://ReviewR76`) returned NEEDS CHANGES on one real regression and one
new finding, not on the mechanism itself -- its acceptance-question answers confirmed the predicate
placement, the classification precedence, and the creation carve-out were all correct. **E1**:
`packages/extension/src/tools/tools.test.ts`'s hand-rolled harness connected as
`instanceId: "omp-tools-test"` while the tool under test sent
`extCtx.sessionManager.getSessionId()` (`"test-session-id-12345"`) as `ownerClientInstanceId` -- a
divergence the pre-fix binary never noticed and the post-fix one refused outright with `-32602`.
`ec68e66` fixed the harness, not the tool: production already wires
`sessionId -> instanceId -> ownerClientInstanceId` through one value end to end (`runtime.ts:262`,
`tasks.ts:45`), so the harness now hoists that value into a single `FAKE_SESSION_ID` const used by
both the fake connect and the fake session manager, mirroring the chain instead of hand-picking two
different strings for what production treats as one. **W1**: three of the four branches the fix
introduced had no test -- higher-revision-plus-non-owner (the variant that also clears R74's `>=`
guard, so the owner clause alone must catch it), lower-revision-plus-non-owner (pinning that
`RevisionTooLow` wins the precedence even when the owner is also wrong), and the param-validation
branch itself (a spoofed `ownerClientInstanceId` refused before the guarded write is ever reached).
`ec68e66` added all three to `task_upsert_cannot_seize_ownership_from_another_instance`.

The review's **W4** is the sentence this Part inherits rather than softens: R76 makes ownership
*transfer* auditable and revision-matched, not *authorized*. `instance_id` is self-declared within
ADR-0004's same-machine trust boundary -- nothing about R76 makes it unforgeable, only consistent.
`task/get` remains un-gated by ownership at all (any `ompExtension` client, including `display`, can
read a task's current revision), and `reconcile/omp`'s only arbitration is equality against that
publicly-readable revision -- deliberately "last reconciler wins," per Part XXVI. Two calls still
move ownership to any client that wants it; R76's guard only means the move is now journaled as a
proper `ReconcileOwnershipChanged` event and can no longer happen silently through `task/upsert`
itself. The review's other new finding, **W2**, is the gap that same framing exposes: task
ownership now gates *decisions* -- `approval/decide`, `policy/violation/decide`, and as of this Part
`task/upsert` itself -- but nothing gates the run *lifecycle*: `run/submit`, `run/retry`,
`run/cancel`, `message/send`, `workspace/acquire`, and `coordination/child/decide` are all still
dispatched with `params` only, no `principal`, so any connected client can submit, retry, or cancel
a run on a task it doesn't own without ever needing to seize the task first. Registered the same day
as **R77**, still in flight as this Part is written.

## Part XXVIII — Two clocks, one flag: the quarantine race that closed into three more findings

R75, found during Part XXV's review of R73, was the same check-then-act shape one service over,
but split across *two* clocks on the same flag rather than one. `ViolationService`'s
`record_nested_worker`/`record_cost_ceiling` computed `already_actioned = flags.policy_quarantined
|| state.is_terminal()` in their own round trip -- via the now-deleted `load_run_state_and_flags`
-- a whole commit before `record_policy_violation`'s journal `INSERT` landed, and `apply_action`'s
`if already_actioned { return Ok(()); }` trusted that stale read. `decide`'s `"release"` path had
the mirror shape one level up: `resolve_policy_violation` and an unconditional
`set_quarantined(run_id, false)` were two separate commits, so a violation that quarantined the run
*between* them was immediately wiped by the release's second write, with no way for that write to
know a fresh violation existed. `06c6522`'s two RED tests pinned both directions with the same
`tokio::join!(biased; ...)` technique Part XIX introduced: the DB actor is a single-threaded, strictly
FIFO consumer of whole closures, so declaring the racing future first fixes its round-trip-`k`
ahead of the other's round-trip-`k` at every step, with no sleeps or spawns needed.
`a_release_landing_mid_record_does_not_suppress_the_fresh_quarantine` RED'd the record-side hole:
the fresh violation's own stale `already_actioned = true`, borrowed from a run an *older* violation
had already quarantined, made `apply_action` skip re-quarantining while a concurrent release of
that older violation cleared the flag underneath it. `a_release_does_not_unquarantine_a_violation_
recorded_after_its_resolve` RED'd the release-side hole from the other direction: the fresh
violation's `set_quarantined(true)` landed between the release's resolve commit and its own
unconditional clear, which then clobbered it. A third, non-racing test,
`a_plain_release_with_no_concurrent_violation_clears_quarantine`, stayed green throughout as the
control. Five consecutive runs showed no flakes in either direction.

`b1d469f`'s fix moved both clocks into the writes they were supposed to guard.
`record_policy_violation` now re-reads `flags_policy_quarantined` and run state on its own
connection immediately before the same closure's journal `INSERT` -- closure-granularity atomicity,
the same boundary R73 established for `set_run_flag` -- and returns a `PolicyViolationRecordOutcome`
carrying that `already_actioned` discriminator alongside the commit, instead of the service reading
it a whole round trip earlier from the deleted helper. The release path no longer performs an
independent second commit at all: the new `DomainRepository::release_quarantine` reads the current
flags plus a live `COUNT(*) ... WHERE resolution IS NULL` over the run's remaining policy violations
-- `resolve_policy_violation`'s own resolution has already committed by this point in the same
transaction sequence, so it is never counted -- and refuses to clear the flag while another
violation is still open. A release targeting one violation can therefore never silently
un-quarantine a run for a different, still-open one. `set_run_flag` and `release_quarantine` now
share extracted `read_run_flags`/`write_run_flags` helpers, so both flag-mutating paths build their
`UPDATE`/`RunFlagsChanged` event from one place -- R73's sole-writer property is structural, not
re-implemented. `ViolationService::set_quarantined` (called with `true` on every remaining path)
was renamed `quarantine` and lost its now-dead bool parameter; the release call site became
`release_quarantine`. `quarantine_race.rs` (3/3), `policy_violation.rs`, `violation_owner_race.rs`,
`run_flags_lost_update.rs`, and `orchestration_rpc.rs` all passed, alongside the full runtime `lib`
suite (208 tests).

The adversarial review (`agent://ReviewR75`) returned **NEEDS CHANGES** -- three errors, five
warnings, three suggestions -- though its own acceptance-question analysis (**A1**) could not
construct a harmful ordering for the flag itself: enumerating every interleaving of the record and
release atomic units against both starting seeds, the *only* ordering that ends with a fresh
unresolved violation and `policy_quarantined = false` is a run going terminal between the release's
checks and the record call's read -- and that is deliberate and harmless, since `already_actioned`
counts a terminal run as actioned by design, a terminal run makes no further progress regardless,
and a release landing on an already-terminal run would itself have been refused as `RunSettled`.
Every other ordering the review enumerated ends `true`, including the ordering the record-side
read exists specifically to protect. What blocked the verdict was closure integrity, not the
mechanism. **E1**: R75's own registered secondary location, `orchestration.rs`'s
`ensure_not_quarantined`, sat byte-identical to its pre-fix state, and pruning R75 without
mentioning it would have silently dropped a location `REVIEW.md` itself named. The review found the
same unregistered shape in `coordination/broker.rs`'s `require_not_quarantined`, and noted the fold
is only mechanically possible for `message/send` (foldable into `record_message`'s own transaction)
-- `workspace/apply`/`workspace/inspect` and `coordination/publishArtifact` gate a working-tree or
broker-side mutation with no SQL write to fold the check into, so they need a different mechanism
entirely. **E2**: `quarantine_race.rs`'s header and inline comments still narrated the defect as
open, citing the deleted `load_run_state_and_flags`/`set_quarantined` symbols and line numbers the
rename had already moved -- the fourth time this project's own review process has caught test
narration lagging its own fix, after Parts XXIII, XXIV, and XXVI. **E3**: `docs/plugin-usage.md`
still promised `"release"` unconditionally lifts quarantine, a promise the COUNT guard had just made
conditional. **W1**: half the fix had no falsifying test -- all three original RED tests happened to
falsify the *same* release-side COUNT guard hunk (per the review's own ordering walk, **A6**); the
record-side read, load-bearing for the ordering where a release clears the flag and a fresh
violation must re-quarantine underneath it, had nothing exercising it. **W2**:
`run_flags_lost_update.rs`'s doc comments still named the deleted `set_quarantined` method. **W3**:
`record_nested_worker`'s idempotency doc over-claimed -- true for the flag (A1 proved it), false for
the `Cancel`/`QuarantineAndCancel` side effects, which are still decided from the same pre-effect
`already_actioned` value one to two round trips before the terminal transition it gates actually
commits. **W4**: a held quarantine was invisible where the operator was looking -- `decide` returned
`"decided"` whether or not the flag actually cleared, there is deliberately no violation-listing op,
and the monitor models the derived flag but never violations themselves. **W5**: the extension's
own copy (`violations.ts`, `SKILL.md`) repeated E3's now-false promise.

`REVIEW.md` gained three findings the same day, alongside R77 from Part XXVII's review: **R78**
(Medium) is E1's carve-out -- the quarantine RPC gates, registered rather than folded, because two
of the three gated operations have no write to fold the check into. **R79** (Medium) is W3's
residue -- the cancel-side discriminator still races, so two concurrent cancelling violations can
both journal an audited intent and both attempt the terminal transition, with the loser's failure
logged rather than classified as idempotent success. **R80** (Low) is W4's gap -- no signal, and no
query surface, for a quarantine a release failed to clear.

Five polish commits closed the NEEDS CHANGES list. `6b43e4c` rewrote `quarantine_race.rs`'s header
and inline comments in past tense with current symbol names, dropping every brittle absolute line
number, and added the section E2 asked for stating which single hunk each of the three original
tests falsifies (closing E2 and, by making the gap visible in the file itself, motivating W1's
fix in the same commit). It also added the fourth test W1's concrete design called for --
`a_record_landing_after_a_release_reapplies_quarantine_from_its_own_read` -- by declaring `decide`
first and wrapping the record future in one leading no-op round trip, so biased-FIFO enqueue order
forces `decide`'s release to clear the flag before `record_policy_violation`'s own read runs, which
must then observe the cleared flag and re-quarantine. It was verified RED by temporarily hoisting
that read back out into a separate round trip ahead of `load_policy_fingerprint` -- reproducing the
exact pre-`b1d469f` shape -- then reverted, with the suite green and five consecutive clean runs.
The same commit fixed `run_flags_lost_update.rs`'s three stale `set_quarantined` references,
closing W2. `aee8e82` closed W4's core ask: `ViolationService::release_quarantine` now returns
whether it actually cleared the flag, `decide`'s body moved into a new `decide_and_release_status`
returning `(DecideOutcome, Option<bool>)` -- `Some(cleared)` only for a newly decided release,
`None` for a cancel or an idempotent replay, since neither computes a clearing decision -- and
`decide()` itself became a thin wrapper discarding the bool, so every one of its thirteen existing
call sites (all in test code by this point) kept compiling unchanged.
`policy_violation_decide`'s handler adds `"quarantineCleared": bool` to the result, present only
for a newly decided release and absent for `"cancel"` and for an `alreadyDecided` replay, since
neither computes a value the field could honestly report. `880bf0a` closed W3 by scoping the
idempotency claim to the flag alone and adding an explicit paragraph naming the cancellation
residue's mechanism and stating it is pre-existing, outside this fix, and tracked separately --
the honest version of the claim rather than a quiet weakening. `73a9e69` closed W5's source half:
`SKILL.md` and `violations.ts` now say quarantine lifts only when every violation on the run is
decided, and name `quarantineCleared`'s three states; the shipped `dist` bundle needed no separate
rebuild commit here since it ships on release, though the parallel R76 range's `61d2fbd` rebuilt it
anyway for unrelated changes landing in the same window.

A scoped re-review of `ec68e66..73a9e69` returned **PASS WITH WARNINGS**: seven of eight
dispositions **ADDRESSED** (E1/R78, E2, W1, W2, W3, W4, S1); W5 only **PARTIALLY ADDRESSED**, since
the tracked `dist` bundle still carried the pre-`73a9e69` text at the moment of that re-review --
closed by the same `61d2fbd` rebuild noted above before this docs pass began. It re-verified A1's
enumeration and A2's atomicity argument held unchanged across the whole polish range, and raised
four new items, none blocking the mechanism: **N1**, that R80 was stale on arrival -- registered by
`f144feb` citing a response shape `aee8e82` had already fixed in the same range, since
`quarantineCleared` now exists at `orchestration.rs:1874` -- narrowed in place in this pass to the
discovery-surface gap alone. **N2**, the `dist` rebuild, already resolved by `61d2fbd`. **N3**, that
R79's and R77's line citations had already drifted -- `880bf0a` shifted `apply_action`,
`create_cancellation_intent`, and `cancel_and_transition` down to `violation.rs:355`, `:433`, and
`:469`, and `aee8e82` shifted `coordination_child_decide` to `orchestration.rs:1971` -- both
refreshed in this pass. **N4**, that `ViolationService::decide` now has zero production callers:
`orchestration.rs` calls `decide_and_release_status` exclusively, and every remaining `svc.decide(
...)` -- across `policy_violation.rs`, `violation_owner_race.rs`, and `quarantine_race.rs` -- is a
test. It stays as the convenience wrapper its own doc comment already names it: migrating thirteen
call sites to a lossier win of symmetry with `ApprovalService::decide` was judged not worth the
churn, and `decide_and_release_status` remains the one production entry point with the lossless
return type.

Measured for this fix and its polish together: `crates/runtime/tests/quarantine_race.rs` (4/4 after
`6b43e4c`), `policy_violation.rs`, `violation_owner_race.rs`, `run_flags_lost_update.rs`, and
`orchestration_rpc.rs` all green, plus the full runtime `lib` suite. `REVIEW.md` is updated in the
same pass this Part records to move R75 into resolution history, narrow R80 to the surviving
discovery-surface gap, and refresh R79's and R77's citations for the line drift this Part's own
commits caused.

## Part XXIX — Six doors, one owner: the run lifecycle gets the same lock as task upsert

Part XXVII's own adversarial review (`agent://ReviewR76`, W2) named this gap the day it closed R76:
task ownership by then gated *decisions* -- `approval/decide`, `policy/violation/decide`,
`task/upsert` itself -- but nothing gated the run *lifecycle*. `OrchestrationService::dispatch`
still routed `run/submit`, `run/retry`, `run/cancel`, `message/send`, `workspace/acquire`, and
`coordination/child/decide` with `params` only, no `principal`, so any connected `ompExtension`
instance could submit, retry, cancel, message, lease, or answer a child-spawn request against a
task it had never reconciled -- without ever needing to seize the task first through `task/upsert`.
`faac2ee`'s six RED integration tests made that concrete one method at a time: a second instance
(`omp-2`) submitting a run against `omp-1`'s task, cancelling `omp-1`'s in-flight run, retrying
`omp-1`'s terminal run under its own worker, injecting a message into `omp-1`'s run, acquiring a
workspace lease scoped to `omp-1`'s run, and answering a pending child-spawn request on `omp-1`'s
run -- all six observed *succeeding* against the pre-fix binary, each asserted instead to refuse
`-32602` and leave both run state and the event journal (`events/replay`'s count) untouched. A
seventh test, `owner_can_perform_every_guarded_run_lifecycle_mutation_on_its_own_task`, chained all
six as the genuine owner and stayed GREEN throughout, so the eventual fix could not pass by
universally refusing.

`8bb17b7` threaded ownership into the four repository methods those six RPCs bottom out in --
`submit_run`, `transition_run`, `record_message`, `decide_child` -- each gaining an
`Option<&str> principal_instance_id` re-read from `tasks.owner_client_instance_id` from *inside*
its own guarded write, immediately before the mutating `INSERT`/`UPDATE`: the same in-tx re-read
pattern `decide_approval` established for R70, because the database actor interleaves whole
`run_domain_op` closures, so only a read from inside that same closure can observe a
`reconcile/omp` rebind landing between a caller's snapshot and this write. `record_message` derives
the run's *actual* owning task from `runs.task_id`, never from the caller-supplied `taskId` field,
so a caller cannot dodge the check by asserting a task it does own for a run it does not. `None` is
passed at exactly seven production call sites, and the fix's own doc comments justify each one by
name rather than leaving it to be inferred: crash recovery and driver-observed lifecycle
transitions (`recovery.rs`, `adapter/run_lifecycle.rs`, `service/run_driver.rs`) act on the
runtime's own authority with no connected caller to arbitrate against; approval/violation
resolution returning a run to `working` is already owner-arbitrated one call up, in
`decide_approval`/`resolve_policy_violation`; and `coordination/send`/`coordination/publishArtifact`
(`coordination/broker.rs`, twice) pass `None` because a `workerMcp` principal's authority *is* its
scope token, already verified against the run at connection time -- it is never the task-owning
`ompExtension` instance, so an ownership check there would refuse every legitimate worker message.
`workspace/acquire` is the one guarded operation that isn't a runs-DB write at all -- its lease
lives in `LeaseService`'s own database file -- so `8bb17b7` added `service::query::run_owner_op`, a
dedicated read-only round trip, for `e416dd7` to call as close to `LeaseService::acquire` as
possible in the next commit.

`e416dd7` threaded `principal` through `dispatch` into all six handlers. `run_retry`'s task to
arbitrate is read from the *prior run's own stored row* (`query::run_get_op(prior_run_id)` →
`prior["taskId"]`), never from a client-supplied field -- `run_retry`'s body parses exactly
`priorRunId`, `workerId`, `prompt`, `workspaceMode`, `displayPreference` and no `taskId` at all, so
a caller cannot launder ownership by asserting a task it happens to own for a different run's
retry. `workspace_acquire` calls `run_owner_op` as the first statement after parsing its request,
with `lease_service.acquire` the very next call and no I/O between them; its doc comment names the
resulting residual honestly rather than claiming atomicity the two separate SQLite files cannot
deliver: a `reconcile/omp` rebind landing in that one gap is not observed, though a caller refused
there allocates nothing, since the check runs before any lease exists to unwind.

`agent://ReviewR77` returned **PASS WITH WARNINGS** -- no Errors. Its **q2** answer audited all
seven `None` call sites individually and confirmed each is genuine runtime authority with no
external caller able to reach it, and its **splice-through-dispatch** check (part of the same
answer) confirmed `dispatch` has exactly two callers, both passing the principal `authenticate`
constructed at connect time; no code builds a `ClientPrincipal` any other way; `ClientRole::Display`'s
method table contains zero mutating methods; and `workerMcp`'s scoped run id comes from
`principal.scoped_run_id`, minted only by `worker_verifier.verify`, never from client params -- so
`None` could not have silently become reachable from an unauthorized branch. Three warnings
followed: **W1**, the `coordination/child/decide` RED test drove only the deny arm, leaving the
accept arm's ownership check independently deletable; **W2**, the `workspace/acquire` RED test
proved no workspace *event* was journaled but could not distinguish "checked before acquiring" from
"checked after acquiring, with the leaked lease never journaled" (leases live in a separate
database and emit nothing until `record_workspace_event`); and **W3**, `transition_run` and
`decide_child` ran `check_transition` -- a plain pre-transaction read -- *before* the owner re-read,
so a non-owner's illegal-edge attempt classified as `ILLEGAL_TRANSITION` instead of `NotOwner`,
inconsistent with `submit_run`/`record_message`, which have no pre-write validity check to outrank
ownership. Two suggestions: **S1**, that `Option<&str>` makes "trusted, no principal" the shape a
new call site falls into by writing `None`, with the compiler unable to distinguish a deliberate
runtime-authority `None` from a forgotten one; **S2**, a stale "cross-project rejection" comment.
And one new High finding, registered the same day as **R81**: `workspace/get`, `workspace/release`,
`workspace/inspect`, and `workspace/apply` take no principal at all and resolve their target purely
from a caller-supplied `leaseId` -- gating `acquire` makes lease *creation* owner-safe, not the rest
of the workspace surface, and the `leaseId` needed is disclosed by the already-ungated `run/get`'s
`workspacePath` and by `events/replay`'s `LeaseAcquired` payloads.

`1ce50c9` closed W3 with an explicit precedence decision rather than a silent reordering:
authorization-first for `transition_run`/`decide_child`, the opposite of `upsert_task`'s
revision-before-ownership precedence from R76 -- and it states why, in both doc comments, instead
of leaving the divergence for a future reader to reconcile. R76's ordering reads: "a stored revision
higher than the one presented is `RevisionTooLow` regardless of ownership -- an owner is entitled to
know its own upsert is stale, and it keeps R74's byte-pinned message stable." R77's reads the
opposite way on purpose: "This is the opposite precedence from `Self::upsert_task`'s
revision-before-ownership check (R76): there, the only way to present a stale revision is to already
be the task's actual owner racing itself, so disclosing staleness first tells a legitimate caller
something it is entitled to know. Here the caller is, by construction, not yet known to have any
standing over the run at all, so it must clear ownership before this method will say anything about
the run's current state." The fix adds a second, plain `self.conn` ownership read immediately before
`check_transition`, purely to fix error-code precedence for the ordinary case; the authoritative,
race-safe re-read inside `append_and_apply`'s closure is unchanged and is still the only one that
actually protects the mutation, so a race between the two reads can only ever make the early one
agree or disagree with the later one, never let a mutation through without the later read's
independent approval. `b72c975` then pinned all three review gaps against the existing RED tests
rather than adding new ones: an accept-arm case for `coordination/child/decide` with the attacker's
own fabricated child ids (closing W1); a `run/get` assertion that `workspacePath` stays `null` after
a refused `workspace/acquire`, since `run/get` reads the lease database directly (closing W2); and a
terminal-run cancel from a non-owner now asserted to classify as `NotOwner` (`-32602`), not
`ILLEGAL_TRANSITION` (`-32100`) -- the observable contract authorization-first actually promises,
pinned rather than merely stated (closing W3).

S1's `Authority` enum was read, weighed, and deliberately not built: the seven current `None` sites
are all correct and doc-justified today, and the review's own words on it are the record --
"this bites on the next adapter or recovery path added," not on anything shipped in this fix. Adding
a two-variant type purely to future-proof a call-site shape that is currently unambiguous was judged
not worth the churn this pass; the next site that needs `None` is where that trade re-opens.

Measured for the fix and its polish together: `cargo test --test orchestration_rpc --test approval
--test policy_violation --test coordination --test run_lifecycle --test recovery --test monitor_cli
--test adapter_contract` -- 44/44 in `orchestration_rpc` (the six formerly-RED tests, the GREEN
owner guard, and every pre-existing case) plus every other named suite green; `cargo test --lib` --
208 passed (`batman-runtime`), 105 passed (`batman-protocol`); `cargo fmt --all -- --check` clean.
`REVIEW.md` is updated in the same pass this Part records: R77 moves into resolution history, and
the review's own new finding becomes **R81** (High) -- `workspace/get`/`release`/`inspect`/`apply`
still take no principal, the identical defect class one method over, registered 2026-08-19 and still
in flight as this Part is written. The chain of findings this doctrine has now produced runs
R70 → R71 → R72 → R74 → R76 → R77 → R81, each one the same guarded-write ownership check, found one
guarded write -- or one sibling of a just-fixed one -- further down the stack than the review before
it looked.

## Part XXX — Four gates, one helper: the chain that stops here

`753c99b`'s RED integration tests made R81 concrete the same way R77's had one review earlier: run
the exploit as if it were the happy path, and read the assertions it fails. Its own commit message
states what each of the three exercised handlers actually did against the pre-fix binary: "today a
non-owner's release tears down the lease"; "today a non-owner fully inspects another instance's real
git workspace (patch artifact, commit history)"; and, for apply, that a bogus, never-seeded
`artifactId` still means "today the handler journals `ApplyStarted` and fails with an
artifact-not-found message instead of refusing at all" -- proof that artifact resolution, not
ownership, was the first check apply ever ran. `workspace_get` was deliberately left untested at
this commit: read-only, and every field it discloses was already reproducible from the pre-existing,
unrelated ungated `events/replay` stream carrying `LeaseRequested`/`LeaseAcquired` payloads.

`e7195fe` threaded `principal` into all four handlers and ran the same `query::run_owner_op`
arbitration `workspace_acquire` (R77) already uses, immediately after `lease_service.get` resolves
the lease and before any mutation -- before release/teardown, before
`ensure_not_quarantined`/materialization for inspect, and before
`ensure_not_quarantined`/artifact resolution/`ApplyStarted` for apply. It gated `workspace_get`
anyway, over its own documented exception, in its own words: "workspace_get is read-only and every
field it discloses was already reproducible from the pre-existing, ungated events/replay stream, so
R81RedTests judged it harmless and left it untested. Gated it anyway for a uniform ownership surface
across all four lease-scoped methods rather than carrying a documented exception a future response
field could silently invalidate -- the gate is one call to the existing run_owner_op helper." 62/62
passed, including the three formerly-RED tests and the owner-success guard.

`agent://ReviewR81` returned **PASS WITH WARNINGS** -- zero Errors, the first review in this chain
to land there. Its residual-window question (**q6**) found the disclosure honest in kind but
understated in extent: `workspace_release`'s "single gap" claim is literally true, but inspect's and
apply's docs claimed "the same bounded residual" as acquire/release while each interposes more
machinery before the mutation it's supposed to be gating -- `ensure_not_quarantined` for inspect,
that plus the `ApplyStarted` append for apply -- so a rebind racing the check on either of those two
can leave a journaled event and a real working-tree mutation attributed to a caller that is no
longer the owner, a consequence the doc text never named.

`cd2b44a` and `2357484` closed all four warnings the same day. `cd2b44a` extracted the four
byte-identical gate blocks into a private `require_lease_owner` helper, which does two things at
once: it makes "all four lease-scoped methods are gated identically" a `grep` away instead of an
eyeball diff across four call sites (closing **W1**'s stale dispatch doc as a side effect -- there
is now one place, not four, for the doc to be wrong), and it corrects the two residual-window doc
comments to state inspect's and apply's actual spans instead of borrowing acquire's (closing **W3**).
`2357484` closed the remaining two by testing what the refactor didn't touch:
`workspace_get_against_another_instances_lease_is_refused` (**W4**, the one gate the RED suite
never exercised in either direction) and
`owner_accepting_a_child_request_journals_the_child_ids_and_returns_the_parent_to_working` -- the
first test anywhere in the suite to drive `coordination/child/decide`'s accept arm to a real
acceptance rather than a refusal or a deny, pinning `kind == childWorkerRequested`, all three child
ids round-tripping, and the parent run returning to `working` (**W2**).

A fourth, unrelated commit rides along in the same window: `8cc0bcd` refactored `decide_child`'s
accept/deny arms, which had encoded a sum type across four separate `Option` parameters, into a
single `ChildDecision` enum, because -- in its own words -- that shape "tripped clippy's argument
ceiling once R77 threaded the principal." `ChildDecision::Accept` binds the child ids; `Deny` carries
the reason; an acceptance without ids or a denial with them is now unrepresentable at the type level
rather than by convention. The wire shape of the journaled `ChildEvent` is unchanged, and
`ReviewR81`'s own **q4** confirmed the two call sites still pass exactly the combinations the old
four-`Option` signature accepted, byte-identically.

This is the first link in the chain -- R70 → R71 → R72 → R74 → R76 → R77 → R81 -- to close without
spawning a successor High. `ReviewR81` did not stop at the four handlers it was asked to review: its
**q7** swept `artifact/list`/`artifact/fetch` (owner-filtered via `owned_run_ids_op`),
`events/replay` and every other project-scoped read (`task/get`, `worker/list`/`get`,
`run/list`/`get`, `message/list`, `approval/list`), the `workerMcp` coordination surface
(scope-token- and same-task-filtered), and `profile/register` (mints its own id server-side, never
overwrites) for the same unarbitrated-mutation class R70 through R81 kept finding one door over --
and concluded, on that evidence, "no unarbitrated task-scoped mutation remains." What the sweep did
surface -- `runtime/shutdown`'s total absence of arbitration, an accepted child request sharing its
event kind with a mere request, a `leaseId` existence oracle sitting in front of the ownership gate,
an undocumented asymmetry between the one gated read and the many open ones, and a lease-cleanup
path with no remedy once its correlation was never persisted -- was registered the same day as
**R82-R86** (Medium/Low), not fixed in this pass; a sixth, the missing child-accept test, was closed
in place by `2357484` instead of registered, since it was a test gap rather than a functional
defect. None of the five registered findings is the same class as R70-R81: each is a genuinely
different defect (missing arbitration on a different kind of surface, an ambiguous discriminator, a
misclassified error code, an undocumented policy asymmetry, an unreachable cleanup path) that the
R70-R81 doctrine's repeated sweep happened to be the thing that finally looked in that direction.

Measured at the fix (`e7195fe`): `cargo test --test orchestration_rpc --test workspace_materialize`
-- 62/62 passing, including the three formerly-RED R81 tests and the owner-success guard. The polish
commits added four more tests to the same two suites -- the `workspace_get` refusal, two
owner-success assertions folded into the existing release test, the apply-reaches-artifact-resolution
owner case, and the first successful child-accept -- without changing production code beyond what
`ReviewR81` had already passed. `REVIEW.md` prunes R81 into resolution history in the same pass this
Part records; with it gone, **High: 0** for the first time since this doctrine started finding
successors to close.

## Part XXXI — The map corrects itself: six documentation lies and one new Medium

The close-out pass opened with the batch that costs nothing to run and everything to skip: making
the project's description of itself true before touching code. `63ef990` fixed the loudest lie
first — `README.md:7` still claimed delivery "as an external npm package (`@nikolasd/batman`),"
a sentence ADR 0022 had invalidated when the leaf packages died: nothing is published to npm at
all; the extension git-clones through the OMP marketplace and `batcave` arrives as a SHA-256
verified GitHub Release asset. `75bc183` committed the matching mechanical residue — `bun.lock`
still carried all four `@nikolasd/batman-*` leaf workspace entries and an extension version of
0.1.0 against a real 0.4.0. The rest of the batch was the same class at smaller scale: the
`audit export` example in both quick-reference files pointed `--state-dir` at `~/.batman/state`,
a directory containing no `runtime.db`, so the documented command silently produced an empty
export (`3295453`, R58); every operator document invoked a bare `batcave` that nothing ever puts
on `PATH` (`c628306`, R20); `CONTRIBUTING.md` demonstrated `cargo test --features` against a
workspace with zero `[features]` tables (`1c5d0c7`, R31); the extension entry point's header
enumerated six of eleven registered tools (`d81f3f5`, R32); and `batman_artifact`'s model-facing
description claimed task scoping the handlers never implemented — the real filter is session
ownership through `owned_run_ids_op`, with `taskId` only narrowing within it (`edf629d`, R43).

R46 closed by deletion, and its recorded evidence deserves a correction here, because the review
(`agent://ReviewBatch0`) caught the batch about to transcribe a stale claim into permanent
history. REVIEW.md's R46 text said the fixture-mode baseline records `"ompRpc": []` — that was
true when written on 2026-08-12 and false six days later, after R44's fix re-recorded the whole
baseline under `BATMAN_DISABLE_VENDOR_CLI=1` with five ompRpc entries. The deletion of
`docs/manual-testing.md`'s `ompRpc/approval` exception row (`878d99e`) is still correct on three
independent grounds: `approval` is absent from `expectedFailures.ompRpc`, so the release gate at
`cli.rs:796` would already fail on an approval regression the row claimed to expect; the
capability is genuinely implemented (`extension_ui_request_to_pending_approval`, exercised by the
real approval scenario in `omp_rpc/conformance.rs`); and the exception table describes the
switch-unset run — a different scope from the baseline file's switch-set recording, so "the
baseline says zero" was never the right comparison in the first place.

The sweep also registered one genuinely new finding rather than fixing it out of order:
**R87** (`998c14d`). `Shared::active_runs` is documented as "a placeholder at foundation scope
(always `0`), but wired into the idle decision so a live run suppresses shutdown" — and nothing
ever increments it, so `batcave status` reports zero active runs forever and the idle timer can
self-terminate a detached daemon mid-run. It lands in Batch 8 with R82, whose shutdown
arbitration is worthless against a counter that is always zero. The review's sweep added a
pointer worth keeping: `PolicyEvaluator::active_runs` already exists as an honestly-maintained
counter (incremented on authorization, decremented on release, defended by six tests), so the fix
should wire the IPC layer to that rather than invent a third bookkeeping path.

`agent://ReviewBatch0` returned one Error and three Warnings, all closed in place the same day
(`084230e`): the Error is the R46 evidence correction recorded above; the Warnings were R20's own
defect class one door over (`docs/getting-started.md` — the developer manual, fifteen bare
`batcave` invocations and the only quick-reference file with no `OMP_BATMAN_BINARY` mention),
`artifacts.ts`'s module header still asserting the exact claim R43 removed 26 lines above the
corrected description string, and the entry-point header (fixed for R32) still omitting the three
non-orchestration registrations. Baseline for the pass, re-measured before any change: 813 Rust
tests passing across 57 suites with the one environment-specific Copilot failure (local CLI
1.0.80 not yet in `COPILOT_KNOWN_CLI_VERSIONS` — it now fails rather than skips, same cause as
recorded on 2026-08-19), and 139 TS tests passing.

## Part XXXII — strict: true was a decoration: wiring the compiler gate

The workspace declared `"strict": true` from day one and never ran the compiler. No local script
and no CI job invoked `tsc`; `bun build` transpiles without type-checking, so two real compile
errors shipped in tracked source and sat there — `model.test.ts` reading a `pendingApprovalCount`
field `MonitorState` never had and omitting the required `decidedBy` from two approval-event
fixtures (R37), and `shared.ts` typing its load-bearing `OrchestrationToolContext` interface
against an `ExtensionContext` name the file never imported (R61). The config that would have
caught both couldn't even parse: `"module": "Bundler"` is not a value any TypeScript release has
accepted (`Bundler` is a `moduleResolution` value; the legal bundler-style `module` is
`"Preserve"`), and `packages/extension`'s `"typescript": "latest"` resolved to the 7.x Corsa port
this repo has a recorded tooling incompatibility with — against `bunfig.toml`'s own
`exact = true` convention.

`a202cca` wired the gate: typescript pinned to 5.9.3 (root and, initially, the extension),
`module: "Preserve"`, an `exclude` for the committed bundle, a root `typecheck` script folded
into `check`, and an independent `typecheck` CI job whose comment names the two findings it
exists to prevent. `skipLibCheck` went in exactly as far as the plan's contingency allowed:
`tsc --noEmit --skipLibCheck false` shows exactly three errors, all in third-party `.d.ts` files
(pi-catalog's `models.json` import, pi-coding-agent's `wrapper.d.ts` variance, pi-mnemopi's
optional `fastembed` peer), zero first-party. `5e336a4` fixed R37 by moving the test to the
contract — the generated union was not touched — and `b3ccbff` fixed R61 on the existing
`import type` line. `3198849` corrected four more first-party test typings the new gate exposed
the moment it could run (`Bun.spawn` subprocess signatures, an untyped fake-tool return shape, a
sync `getClient` where the interface demands a promise). `4d8401a` closed R30 by making local
`test`/`check` run bare `bun test`, what CI runs — which added the conformance assert-report
suite to every local run (149 tests, up from 139).

`agent://ReviewBatch1` returned one Error and three Warnings, all closed in place (`85c7e45`,
`55ae4c0`): the Error was `docs/code-walkthrough.md` still teaching the typescript@7 workaround
this batch obsoleted — steering a contributor away from the new gate at exactly the moment they'd
hit it; the Warnings were the two agent-facing command blocks omitting `bun run typecheck`, the
workspace's only `as any` (in a test fake, doubly erased by an adjacent cast), and three
different fake `execute` return typings for one concept. The review also proved the gate
load-bearing rather than decorative: Batch 3's `satisfies` drift assertions in `workspaces.ts`
already name `bun run typecheck` as the thing that breaks when a wire union drifts.

## Part XXXIII — Tool contracts that lied about themselves

Seven findings, all in `packages/extension/src/`, all the same species: the surface the model or
the operator sees promising something the code doesn't do. Two schema contracts accepted what the
runtime rejects — `batman_violation.resolution` was an open string against a runtime that accepts
exactly `release`|`cancel` (R16, `e6bba53`), and `batman_run.workspaceMode` was an open string
against `shared`|`isolated`|`copy` (R29, `3ce9bee`); both now fail at the schema with the token
list in the description, saving the model a doomed round trip. One contract advertised what the
wire discards: `batman_task.description` never left the extension — `task/upsert`'s payload has
no field for it — so `ec360e8` deleted the parameter rather than plumbing it end-to-end
(a protocol field, a `tasks` column, a migration, and binding churn for a value nothing reads,
against invariant 6's placement of task-text authority in OMP), and the tool description now says
where task text actually goes: `batman_run.prompt`. R40 (`3d9d56e`) removed a `revision` argument
a test passed to a schema that never defined it.

The monitor pair shipped RED-first (`c3be76c`): `session_start` connected but never rendered, so
a healthy runtime showed nothing until the first event fired (R56), and `session_shutdown`
stopped the subscription without clearing `subscribedClient`, leaving `connect()` to early-return
into a dead monitor (R39). `8b86080` fixed both — render-once-after-connect, guarded so an
unreachable runtime stays silent rather than lying "No BATMAN runs yet.", and the shutdown
handler now pairs `stop()` with the clear exactly as the repair path does. `da48a67` closed R18:
the detached daemon spawn now attaches an `error` listener before `unref()`, so an async
`EAGAIN`/`EMFILE`/vanished-binary failure logs instead of being silently lost (`console.error`
deliberate — the file has no logger seam).

`agent://ReviewBatch2` returned one Error and five Warnings. The Error was this batch's own
defect class reflected back: two `docs/manual-testing.md` prompts still told the model to pass
`description` to `batman_task` (`3c0e749`). Warnings closed in place: the stale `description` in
`tools.test.ts` (`55ae4c0`), a spawn-failure regression test for R18 (a shebang pointing at a
nonexistent interpreter — passes every `selectBinary` check, fails async at exec; `eed1b91`), a
faithful-fake test pinning the closed-client repair path (`eed1b91`), and a comment recording the
deliberate `/batman`-versus-`session_start` render asymmetry (`25da1a0`). The review also
corrected R39's severity: in production, `index.ts` closes the cached client at shutdown, so
`connect()`'s pre-existing `isClosed` repair branch already saved the monitor — the real defect
was the monitor's correctness depending on an unrelated handler's cleanup, not a reachable dead
monitor. Two new findings were registered rather than folded in (`8d19ad7`): **R88**
(`batman_message.kind` is R16's class one door over — the only remaining open-string enum among
the eleven tools) and **R89** (`run/submit` echoes `workspaceMode: "isolated"` for a `copy`
workspace — the request vocabulary closed while the response side still collapses two kinds).

## Part XXXIV — The generator that only generates what it's told

R60's root cause was a belief this journal itself once stated: that `bun run generate` "walks
every `#[ts(export)]` type". It never has. `export_bindings` is an explicit allowlist, and a type
carrying the derive but absent from the list — and unreferenced by anything in it — emits
nothing, silently, forever: `generate --check` passes because the generator's steady state *is*
the omission. That was exactly `Artifact`/`ArtifactKind` and the four canonical result types:
`artifacts.ts`'s hand-rolled kind enum had no generated source of truth to drift against at all.
`4c424cd` added the artifact and workspace result types to the allowlist and to
`ProtocolDocument`, so their bindings and schema `$defs` now exist — which is also what Batch 4
builds its validators on. The follow-up (`4c7540f`) went further on the review's push: the
workspace *request* types joined the list (the sharp edge was exporting `InspectResult` while
`InspectRequest` stayed invisible), and `export_bindings`' doc comment now states the honest
contract — the list, not the derive, decides.

`56465fb` closed R17's two halves. The barrel now re-exports all 56 (now 60) generated files
instead of 35 — twelve wire types, `RunFlags` and `RuntimeEventKind` among them, were generated
but unimportable. And the three hand-copied enum tuples in `workspaces.ts` plus the one in
`artifacts.ts` are now tied to their generated unions with `as const satisfies readonly X[]`
plus a consumed exhaustiveness constant, so drift in either direction breaks
`bun run typecheck`: a variant removed in Rust fails the `satisfies`, a variant added makes the
conditional alias `never` and the `= true` assignment fail. Both directions were proven live
against a scratch-edited generated file (TS2322 at the exact expected lines), which is why
Batch 1's compiler gate had to land first. `cfec902`/`4c424cd` closed R64: `generate --check`
now fails on any drift between `.claude-plugin/marketplace.json`'s two version fields and the
extension version, with error messages naming the file and both values — proven live in both
drift directions — and the manual release-checklist bullet it replaces now points at the gate.

`agent://ReviewBatch3` returned **PASS WITH WARNINGS** — zero Errors, and its warnings all became
in-place hardening (`4c7540f`): the barrel's completeness is no longer a convention but a
`generate --check` failure (`check_barrel_completeness` — proven by the four new request bindings
failing the gate until the barrel learned them), and `check_version_coherence`'s marketplace
branches gained fixture tests for both drift directions plus the coherent case. One warning was
already closed by the time the review landed (the stale `code-walkthrough.md` typescript@7
paragraph — Batch 1's review had caught the same lines hours earlier), and one is deferred to
this Part's own correction: the Part-3 claim that the generator walks every `#[ts(export)]` type
was aspirational, not descriptive — the generator has always been an allowlist, and that mistaken
belief is precisely how R60 lived this long. The `MonitorFlags`-mirrors-`RunFlags` suggestion is
left for Batch 9, which touches the monitor model anyway.

## Part XXXV — Making invariant 2 true instead of aspirational

`client.ts`'s header and invariant 2 both claimed every inbound daemon message was Ajv-validated
before reaching caller code. The envelope half was true; the result half validated exactly one
method (`runtime/status`) because `JsonRpcResponse.result`'s generated schema is the JSON-Schema
`true` node — every other orchestration result reached tool logic unchecked (R55). The fix
(`fe331a8` RED, `b51f953`) declined to type all 25 results — the six pass-through reads would
have dragged the domain-query row shapes with them, and the tool layer never destructures
results — and instead did three things: the four handlers that were hand-duplicating canonical
protocol types (`workspace/inspect`, `workspace/apply`, `artifact/list`, `artifact/fetch`) now
serialize `InspectResult`/`ApplyResult`/`ArtifactListResult`/`ArtifactFetchResult` directly,
byte-identically (verified field-for-field against the pre-image, nulls included); `request()`
consults a `RESULT_VALIDATORS` map and Ajv-validates those methods' results; and every
validator-less method's result must at least be a JSON object, so a `null`/scalar/array can
never reach tool logic. The invariant's wording in `CONTRIBUTING.md`, `AGENTS.md`, `CLAUDE.md`,
and the header itself now states exactly that split.

`agent://ReviewBatch4` returned one Error — and it was this batch's own thesis reflected back:
the reworded invariant ("Ajv-validated for every method with a canonical protocol result type")
was false the moment it was written, because `workspace/get` *has* a canonical type.
`require_lease_owner` literally returns `batman_protocol::WorkspaceInfo`, whose doc comment says
"returned by `get`" — and the handler discarded the type and hand-rolled the same JSON with
per-enum `match` arms, the exact duplication the batch had just removed from its four
neighbours, in the adjacent function of the same file. `3196b04` closed it: `workspace_get`
serializes the type, `WorkspaceInfo` joined the schema root, the export list, the barrel, and
`RESULT_VALIDATORS`. The review's warnings landed in the same commit — the elided `artifact`
object in `plugin-usage.md`'s fetch example completed, and exact wire key-set assertions for
`workspace/get`/`workspace/inspect`/`artifact/list` in `orchestration_rpc.rs`, so a future
`skip_serializing_if` or rename breaks CI here instead of failing Ajv at the far end. One
finding was registered rather than fixed (`e821a8f`): **R90**, the ts-rs/schemars disagreement
on numeric width (`bigint` vs number) and Option presence — a static-type trap the new
validators are the first code to stand in front of.

## Part XXXVI — Three adapters, three honesty fixes

The smallest first: Copilot's unknown-stop-reason health detail interpolated the lowercased,
separator-stripped match binding instead of the vendor's token — `unknownStopReason:
somebizarrereason` for a vendor that said `Some_Bizarre-Reason` — while the failure string two
lines below already used the raw value (R42, `ed45a1d` RED / `985f7a6`). The pre-existing test
was blind by construction: its input was already lowercase with underscores. The RED test feeds
a mixed-case, mixed-separator token and asserts the detail carries it verbatim.

R57 (`c7adc2c` RED / `27f152e`) closed a gate that its own doc comment claimed was
unconditional: `ensure_client` refused a *known-bad* Copilot CLI version but let a response that
omitted `agentInfo.version` proceed entirely unverified, while `probe()` correctly treated
`None` as unknown. The two now share one decision function —
`copilot_negotiated_version_verified`, where a missing version is unknown, not implicitly
trusted — and the refusal message names the missing-version case. Deliberately **not** done:
adding the local machine's CLI 1.0.80 to `COPILOT_KNOWN_CLI_VERSIONS`. That table means
"empirically conformance-verified", and adding an unverified version to silence the one
environment-failing real-binary test would be R57's defect one layer up. The review's W-1
pushed the evidence further than the plan had: `d96b3c5` drives `ensure_client` itself, through
`resume()` against a fake ACP agent (a shell script speaking real NDJSON frames) whose
initialize response omits the version, and asserts the typed refusal end-to-end — so reverting
the wiring, not just the predicate, now fails a test.

R12 (`432151a` RED / `b4ef3f2`) stopped Claude's error results from being silently reduced to
usage reports. The committed `error_max_turns` fixture carries `is_error: true` and a subtype;
`RawResult` didn't model either field, so the run's failure never surfaced as an event.
`RawResult` now carries both (optional — a success arm may omit them), and `normalize_result`
emits `ProtocolHealthChanged { healthy: false }` naming the subtype after the usage report —
the same shape Copilot, Codex, and OMP-RPC already use for a vendor-reported failure, which is
exactly why no new event kind was invented. The fixture-mode conformance baseline did not shift:
the review confirmed the conformance scenarios never feed the error fixture. What the review
did surface became **R91** (`d57f83c`): all this precision stops one layer short of the
operator — the `batcave status` row mapping collapses the event to the constant string
"protocol health changed" and the `/batman` monitor has no handler for it at all. Batch 9's
monitor work is where that lands. Full suite after the batch: 818 passed, one known-environment
Copilot real-binary failure, clippy and fmt clean.

## Part XXXVII — The audit trail that threw away its own rationale

Two data-loss bugs shared one `UPDATE` in `decide_approval`, so they shared one RED test and one
migration. R34: `decided_by` was written via `serde_json::to_string`, persisting the JSON-quoted
token — `"human"` with quotes — so `WHERE decided_by = 'human'` matched zero rows, permanently,
and the column that exists to make `human_required` decisions auditable was unqueryable by the
token it documented. R59: `decide_approval` contained `let _ = reason;` — the rationale was
threaded from the RPC boundary through the service layer and thrown away, on every decision,
with no column to land in. The RED test (`a5b63c2`) decides an approval with a reason and reads
the row back with raw SQL — the only formulation that could fail, since no RPC read surface
returns either column (that gap is now **R92**).

`9e82c28` fixed both in the one guarded write: `DecidedBy::as_str` mirrors the serde rename
exactly (now pinned to it by a `crates/protocol` contract test, so a future `#[serde(rename)]`
cannot silently re-create R34), the UPDATE persists the bare token and the reason together, and
`MIGRATION_9` adds the column plus a data repair that strips the quotes from rows the bug left
behind — exhaustively analyzed by the review as safe for every possible stored value, idempotent,
and defended by a migration test that seeds a quoted row at v8 and asserts the bare token at v9.
The decision's rationale also rides the wire now: `ApprovalDecided` events carry an optional
`reason` (`serde(default, skip_serializing_if)` + `ts(optional)`, so every pre-R59 journal
replays and the field never appears on request events). One deliberate residue, recorded here as
the review asked: the change is downgrade-unsafe — a pre-R59 binary replaying a post-R59 journal
fails on the unknown field, the same forward-only property every additive event change in this
codebase has.

`agent://ReviewBatch6` returned one Error and three Warnings. The Error was not in either fix:
the committed extension bundle still embedded the pre-R59 schema, under which an
`approvalDecided` event carrying `reason` matches zero arms of the `RuntimeEvent` `oneOf` — the
shipped monitor would validate-reject every such event and, once one existed in the journal,
never subscribe at all. `e21b151` rebuilt and committed the bundle; the lesson is that a schema
change makes the bundle refresh load-bearing, not cosmetic. Warnings closed in place: the
token-pinning contract test (W-1, `3ec2b7a`), an empty `reason` now refused at the RPC boundary
rather than persisted as a rationale that does not exist (W-2, `ecdc580`), and the monitor's
decided row now renders the rationale (S-1). The hand-rolled `decidedBy` parser collapsed into
serde's canonical deserialization (S-3). W-3 became **R92**: decision provenance — both
`decided_by` and the new `reason` — is persisted and journaled but `approval/list` returns
neither, the same read-surface class R80 registered for policy violations.

## Part XXXVIII — Seven kinds of dishonest error, classified honestly

Batch 7 was error hygiene: seven independent findings, each a place where the runtime told a
caller (or an operator, or itself) something untrue about a failure. `45f227f` (R66) made
`DatabaseHandle::shutdown` join the actor thread even after an abnormal death — the old
`rx.await.map_err(...)?` short-circuited past the join, leaking the `JoinHandle` and losing the
panic; a regression test now panics the actor with a poisoned domain op and proves shutdown
still reaps it. `a0ed385` (R14) removed the redactor's fail-open fallback: a sink whose org
security patterns do not compile can no longer exist, because invariant 4 says content is
redacted before it becomes durable — unreachable today (startup validates and refuses), but one
config-reload away from journaling text the org's rules were meant to remove. `c463d21` (R13)
gave `RunDriver::cancel_run` a typed success — `CancelOutcome::NoRunningAdapter` is a clean
outcome, not an error string indistinguishable from a failed kill — and a genuine kill failure
after a policy cancellation now raises `flags.degradedControl` through the same guarded
`set_run_flag` write quarantine uses, so `run/get` and the monitor see "the control plane could
not act on this run" instead of only the log. `fad8dc5`/`ecdc580` closed the lease-layer trio:
an unknown `leaseId` is the caller's `-32602` (R84), `active_for_run` propagates real database
errors instead of reading them as "no lease" (R62 — no fault-injection seam exists, so the
defense is the type change plus the review's trace of both callers, stated plainly here), and
`LeaseError::Conflict`'s doc stopped promising a same-run guard `acquire` never had (R63 — doc
fixed, not code: a run holding a read-only view alongside an isolated write worktree is
legitimate by design). R35 reordered `artifact/fetch`: authorization now runs against metadata
only, before content is read and hashed, closing the latency oracle between "exists but not
yours" and "does not exist".

`agent://ReviewBatch7` returned one Error and six Warnings — and the Error was the batch's own
lesson applied to itself: the comment justifying R84's fix claimed the unknown-lease refusal
"matches the ownership refusal", while ten lines below the ownership arm still surfaced
`task <id> is not owned by <instance>` — a complete existence oracle by message text instead of
error code, plus a free task-id leak. `5d7bec2` hoisted one refusal string over every
caller-distinguishable failure of `require_lease_owner` and made both `workspace/get` tests
assert the two messages byte-identical — the assertion is what makes the comment true. The
warnings closed in the same commit: the actor-panic regression test (W1), `run/get` no longer
re-collapsing the very error R62 just made propagate (W2 — the fix's own defect class, one
door over), `AlreadyReleased` post-gate reclassified as the caller's error (W4), the `Conflict`
doc naming its exact raising arm (W5), and a post-authorization digest mismatch reported as the
internal fault it is rather than an ownership refusal that would hide on-disk tampering from
the artifact's own owner (W6). W3 became **R93**: `run/cancel` still reports unqualified
success after what is now an unambiguous kill failure — the same ten-line treatment, one door
over, registered rather than folded in.

## Part XXXIX — The counter that was always zero, and the gates that moved inside

Batch 8 was arbitration: four findings about decisions made from numbers or flags that were not
real. `058174d` (R87) replaced `Shared::active_runs` — a hardcoded `0` the idle-shutdown timer
and `runtime/status` both believed — with `RunDriver::active_run_count()`, the adapter
registry's live map length, wired through the same seam `run/submit` already used. The fake
test drivers report a fixed count, so `runtime/status` tests now assert the driver's number and
the refusal message renders the real counts. `5aef049` (R82) gave the in-band
`runtime/shutdown` its missing arbitration: refused with `-32602` while any run is live or
another connection is being served, unless `params.force == true` — the deliberate, logged
operator escape hatch. The out-of-band `batcave stop`/SIGTERM path stays unarbitrated on
purpose; whoever can signal the process can stop it, and both docs now say so. `0ccdb3d` (R78)
moved the quarantine gates inside the writes they guard: `record_message` and
`record_workspace_event` take `enforce_quarantine` and re-read the flag inside the same guarded
transaction, so a quarantine landing between a caller's pre-check and its write now refuses
instead of journaling; the dead caller-side pre-check and its `run_flags_op` query were
deleted. `574a00c` (R79) made concurrent policy cancellations honest: the loser of the
`cancel_and_transition` race acknowledges `superseded` in the audited operations table instead
of surfacing a transition error, and a deterministic `current_thread` race test proves both
interleavings land in the operations table. The adversarial review returned one Error — a stale
dist bundle, already healed by the next batch's refresh — and nine warnings, all applied in
`ced828a`: `active_run_count` became a required trait method (a silently-zero driver cannot
reintroduce R87), the unforced accept leg and the quarantined `workspace/apply`/`inspect`
refusals gained tests, and the race test's expects now name their real proof, since the sink
swallows violation errors into a warn log. One review finding became R94: `require_live_run`
is itself an advisory pre-check outside the writes it guards — R78's class, one door over.
A replay nuance worth recording: an accepted-shutdown journal write races nothing, but a
pre-R83 binary replaying a post-R83 journal fails the *whole* `events/replay` call on the
unknown variant, not just the one event — downgrades break replay entirely.

## Part XL — The event that lied about itself, and the violation you could not find

Batch 9 was observability. `c0dfd75`/`d65c542` (R83) stopped `decide_child`'s accept arm from
journaling `ChildWorkerRequested` — the request's own kind — for an acceptance: the accept now
emits `RuntimeEventKind::ChildWorkerAccepted`, a new additive wire variant, and the status row,
monitor label, and TS bindings all render "child worker accepted". The RED test pins the LAST
`childEvent` for the parent run, so the seeded request row cannot satisfy it. `57faa77` (R80)
built the discovery surface the quarantine loop was missing: `policy/violation/list` — protocol
type (`PolicyViolationListResult`/`PolicyViolationSummary`), query op, dispatch, `ompExtension`
and `display` roles, the extension's `batman_violation { op: "list" }`, the monitor's
`Open violations:` row naming each undecided violation id, and SKILL.md's recovery loop
(`quarantineCleared: false` → list → decide the one with `resolution: null`). `2e4f63e` closed
the R55-class gap in the same motion: the new method's result is Ajv-validated in the client's
`RESULT_VALIDATORS`, and the Rust test pins the exact wire key set against the
`additionalProperties: false` schema — then, after review, round-trips the result through the
canonical `deny_unknown_fields` type so a renamed field fails at `cargo test` time
(`bd0acf9`, which also let the `display` role read the new list, corrected architecture.md's
role counts, and taught the docs the third child label). The review's verdict: the hand-rolled
`json!` projection in `query.rs` deviates from Batch 4's `serde_json::to_value(&canonical)`
convention but is now pinned from both directions.

## Part XLI — Tests for the promises everyone already believed

Batch 10 closed the two standing test gaps. `cca7810` (R36) added producer-side stamping tests:
the isolation tests always hand-seeded `run_id` on their input fixtures, so nothing proved
`WorkspaceInspector` and `WorkspaceApplier` actually stamp the producing run's id on the patch
and conflict artifacts they store — reverting the production stamping to `run_id: None` left
the whole suite green. Both new tests were watched failing against exactly that scratch revert
(one stale-mtime lesson later: `cargo` trusts timestamps, `touch` after an `os.replace`
restore). `ef53e9a` (R67) drove a synthetic `ProcessExited` through a real `SettlementSink`
into `watch_settlement` with a real ceiling-1 `PolicyEvaluator` behind the trait object —
booked slot, exhausted ceiling, settle, freed ceiling. Scope honesty, per the review (W2): R67
closed at the seam its own Fix field prescribed — `SettlementSink → watch_settlement →
PolicyEvaluator::release` — and the vendor-adapter (Claude/Codex) end-to-end settlement path
remains untested; a live Claude run's slot release is still proven only by composition, not by
one test. The review's W1 was real: the sink half of the test originally discriminated by
*hanging* (the live sender pends the receiver forever), fixed in `9e54b1f` by dropping the sink
after the emit so a non-firing sink errors the receiver and fails the final authorize cleanly.
The review also caught the watcher doc claiming the terminal-adapter settlement gap was
"tracked separately" when no tracker existed — now it is, as R95: a terminal-adapter run that
settles without `ProcessExited` pins its slot, the idle timer, and unforced shutdown for the
life of the process.

## Part XLII — The lease nobody could release, and the release that had to be earned

Batch 11 was lifecycle hygiene. `3c5d9ac` (R38) narrowed `install_frame_tap` to `pub(crate)` —
the frame tap is a process-global, single-slot side channel for conformance capture, and a
public export would let an embedder silently siphon every supervised worker's stdout; the
reason now lives at the declaration. `1318f0c`/`ed28ab2` (R65) closed the rate limiter's
retired-sender leak with a lazy full-map sweep under the existing lock — deliberately not the
plan's `forget_run` hook, because a sweep cannot be forgotten at a new retirement site — and a
test that watches `tracked_senders()` drop. `6d43eab` (R85) recorded ADR-0024: project-scoped
reads are open to any same-user client by design; ownership gates *mutation*; `workspace/get`'s
gate is uniform-refusal hygiene, not confidentiality. `fbc319a` (R86) added `batcave lease
release`, the operator remedy for a lease whose owning session correlation was never persisted
— and the adversarial review rejected it with three Errors, all real: the forced release
journaled nothing (a permanent `LeaseAcquired` with no terminating event), guarded nothing (an
active lease of a live run released cleanly, and a live daemon's monitors could never see the
out-of-band write), and turned the doctor's `cleanupFailed` report into a way to *lose* a
leaked directory. The rework (`2a1f715`) made the command earn its power: refused while the
runtime's socket exists, `--yes` required for an `active` lease, intent persisted to the
audited `operations` table before the release, `LeaseReleased`/`CleanupFailed` journaled after,
the worktree torn down exactly as `abandon_lease` does with teardown failure keeping the row in
`cleanupFailed`, distinct exits (1 unknown, 2 already-released), both refusals on stderr — all
proven against the compiled binary, including the journal row and the acknowledged intent. The
review also found R65's class one door over (expired scope-token records, registered and fixed
as R96 in `db6eee9`) and required ADR-0009's back-reference (`33e5fc3`).

## Part XLIII — Nine findings the reviews themselves had found

Batch 12 emptied the review-found backlog, R88 through R96. `e08a0fd` (R88) tied
`batman_message.kind` to the generated `MessageKind` union — R16's class, one door over, the
last open-string enum among the tool schemas — with the same `satisfies` + exhaustiveness
pattern, exporting `MessageKind` through xtask rather than hand-writing it. `1dd19af`/`7dcc805`
(R89) made `copy` read back as `copy`: the RED test caught `run/submit` echoing `"isolated"`
verbatim, and after review (S2) all three echoes derive from one helper over the *resolved*
`IsolationKind`, so a future resolution fallback cannot make the echo lie again. `e086f9c`
(R90) ended the bigint/number schism: every `u64`/`i64` wire field carries
`#[ts(type = "number")]`, zero `bigint` remains in the generated bindings, and the monitor
model dropped its bigint state — the values are sequences, counters, and byte lengths, all far
below 2^53. `5fe1210` (R91) put `ProtocolHealthChanged`'s detail — the vendor error subtype
R12/R42/R57 worked to preserve — on both operator surfaces, with the status-row half gaining
its own unit test after the review noted (W4) it could be reverted green. `d02ae4f` (R92) gave
approval provenance a read surface: `approval/list` projects `decidedBy` and `reason`
(present-and-null while pending, a shape the review caught the doc misdescribing, W3).
`defc38d`/`48f6de2` (R93) made `run/cancel` honest about a genuine kill failure — the same
journaled, broadcast `degradedControl` treatment R13 gave the policy path. `60d74cd` (R94)
moved the broker's liveness promise inside the writes it guards: `record_message` re-reads the
run's state in the same transaction as its INSERT, `request_child` re-runs its transition
check inside its guarded write, and `message/send`'s deliberate exemption is pinned by a test.
`61b6a50` (R95) taught the terminal adapter to settle: `cancel` emits one visibly-synthetic
`ProcessExited` (exit code null, signal "cancelled") through the sink captured at `start`, so a
cancelled terminal run no longer pins its slot, the idle timer, and unforced shutdown forever —
and after review (W2), a second cancel is a no-op success, because an `Err` there would read as
a real kill failure and raise a false `degradedControl`. `db6eee9` (R96) swept expired
scope-token records in `verify` — and, after review (S1), in `bind` too, so a bind-only
workload is bounded. The R86 rework (`2a1f715`, Part XLII) was re-reviewed in the same pass:
all three prior Errors confirmed closed, with one new Warning (W1) — the liveness guard used
the socket file as its proof, which an unclean crash leaves behind, reinstating R86's
no-remedy condition for exactly the crash case; `0dab89b` switched it to the advisory-flock
probe `batcave stop` already trusts and pinned the stale-socket case with its own test.
One watch item remains: R97, a single unreproduced flake of R79's race test under
full-workspace load, registered rather than speculatively patched.

## Reading order, if you're new here

If you're going to *use* BATMAN, not build or maintain it, skip this journal entirely and start
with [`plugin-usage.md`](plugin-usage.md) — the user manual. Everything below is for someone
contributing to or maintaining the codebase itself.

1. **README.md** — what this is, in two paragraphs.
2. **This journal** — how it got to be that, commit by commit.
3. **`docs/adr/`** — the decisions that outlived their commit, in a form built to survive being
   read out of context.
4. **architecture.md** — the finished design, with no history in it at all.
5. **[`getting-started.md`](getting-started.md)** — the developer manual: build, configure, test.
6. **code-walkthrough.md** — how to find anything, trace a request, and debug it.
7. **rust-primer.md** — if Rust itself is still new, read this alongside the journal; every "Day"
   in the primer is the concept behind one of the commits above.
8. **manual-testing.md** — every live/manual verification step this journal references by name,
   runnable, including the environment variables each worker adapter's live suite gates on.
9. **engineering-lessons.md** — the specific bugs this journal narrates as history, indexed by
   file/ADR instead of by commit, for when you're debugging something that feels familiar.
10. **operations.md** / **cli-reference.md** / **compatibility.md** — day-to-day references once
    you're past onboarding: running `batcave` by hand, its full flag set, and what's actually
    proven to work against which platform/adapter version.

## Part XLIV — run/result: the first run method with a canonical result type

Gap 2 of the multiagent-cooperation design (spec: docs/superpowers/specs/
2026-08-21-multiagent-cooperation-gaps-design.md). Before this, a worker's final answer was
journaled but unreachable from the tool surface — `run/get` returns state and flags only, so
OMP could not chain one worker's output into another's prompt. `run/result` reads the journal
for a terminal run: last `adapterMessageFinal` with non-null text (role-agnostic — Claude
finals are `role: "result"`, OMP-RPC is `role: "system"`; a role filter was the design's
original, wrong, selection rule), plus an adapter-aware usage fold (Claude sums per-invocation
deltas; Codex/OMP-RPC cumulative totals are last-wins; Copilot honestly `null`). Guarded by
crates/runtime/tests/run_result.rs (refusal for non-terminal/unknown runs, chunk fallback,
redaction, per-adapter fold) and a client-side ValidationError test; manual scenario:
docs/manual-testing.md §6.

## Part XLV — WP29: live TUI smokes, keep/drop, and the build that wasn't deterministic

Final gap-closure work (phase 10, PR #12, merged to main as 0.5.0-prep; tag held). Three threads:

**Two-phase prompt delivery.** The live TUI smoke failed because the adapter wrote prompt text and
its Enter as one atomic `text\r` at a fixed delay after spawn. Whether a vendor TUI processes that
Enter depends on where its render loop is when bytes land — too early (stdin unwired) or mid-layout
(the CR is swallowed) and the prompt renders but never submits, so no nonce transcript appears.
Machine load shifts the timeline, which is why byte-identical injections sometimes submitted and
sometimes didn't. Fix (`crates/runtime/src/adapter/tui/adapter.rs`): type TEXT at
`max(first-output, INJECT_MIN_DELAY=500ms)` after spawn, then send the single Enter only after
`ENTER_IDLE_MIN` (10s, configurable as `TuiTimings::submit_idle`) of PTY output silence. No submit
byte exists before that silence, so no live turn is interrupted, and an idle TUI processes it like a
human keystroke regardless of render speed. Queue-style `send()` is likewise idle-gated and splits
text/Enter with a 150ms gap (codex swallows an atomic `text\r` whole). `Steer` stays exempt.

**Per-vendor keep/drop (spec §4.6, user decision).** All four headless adapters KEPT; the built-in
adapters' configured default mode is now TUI (`default_adapters()` in config/crew.rs sets
`AdapterMode::Tui` for claude/codex/copilot/omp — `AdapterMode::default()` itself stays `Headless`
for empty/legacy profiles) because all four TuiVendors pass fixture-mode conformance. The harness
preserves `--mode headless` for the non-interactive live path.

**Live results (raw reports in `release/live-conformance/`, with an erratum).** claude-tui and
omp-rpc-tui: all runnable scenarios pass (probe, read_only, follow_up, cancellation). codex-tui:
read_only passes but follow_up fails on `usageLimitExceeded` (out of credits). copilot-tui:
read_only + follow_up both fail on the same out-of-credits error; only probe + cancellation pass.
None of the four passes `session_resume` — it is *skipped* by design, because a single-process
resume is not a daemon restart; the transcript-recovery-across-restart e2e is a separate, not-yet-run
follow-up (the report's "proven by serve→stop→serve smoke" detail is overstated — that smoke was
vendor-free).

**Bundle determinism.** `bundle-check` failed because the committed `dist/index.js` embeds Bun's
platform-specific module shim, and a darwin-arm64 rebuild diverges from CI's linux-x64 rebuild even
at the same Bun 1.3.14. The fix: rebuild the committed bundle in CI's exact environment (the
`refresh-bundle` workflow, or a linux-x64 container) — verified byte-identical to CI's own rebuild
(git blob `ee775fd`). The per-vendor live reports were written verbatim with an adjacent erratum;
they are not regenerated from edited JSON.

**Flake, not regression.** `ownership.test.ts` failed once on macos with `SQLITE_BUSY`: it opens a
second connection to the daemon-owned WAL `runtime.db` and writes directly. Added
`PRAGMA busy_timeout = 5000` to match the daemon's own connection policy; the daemon sets the same
pragma on that database. Verified locally (2 pass) and green on both CI OSes.

## Part XLVI — The endgame: interim findings closed, three work packages, the headless drop

**Interim review, closed (#17).** crew-reviewer's deep-track pass (HEAD `64c70d6`) found C1
CRITICAL: `registry.rs`'s `tui_transcript_path_for_session` hardcoded `"claude"` as the vendor
regardless of which TUI adapter a run actually used, and `recovery.rs`'s restart-recovery path
called it for every TUI run — a non-Claude TUI run silently terminalized on restart with a
misleading Claude-shaped transcript-path reason. Alongside it: I1 (`record_escalation_raised`
wrote its projection row outside the event transaction, violating invariant 1), I2 (the
repeated-failure escalation query counted *any* prior failure rather than a consecutive streak),
and I3 (migrations 11–14 shipped with no version-step tests). All four closed in #17;
crew-reviewer's own follow-up verification pass (main@`2cde61e`) confirmed C1/I1/I2/I3 CONFIRMED
CLOSED.

**Two blockers, found during that same verification pass.** `recovery.rs:510`'s idempotent-
terminalize branch queried `SELECT status FROM runs` — the column is `state` — so the `d637033`
fix for the already-terminal re-read path had never actually worked; every re-read silently fell
through to the broken path it was meant to guard. Separately, `packages/extension/dist/index.js`
was found absent from git at HEAD entirely (deleted by #17's own M5 cleanup, its refresh never
re-run) — a fresh clone had no extension entry point, and CI's `bundle-check` was vacuously
passing against a file that didn't exist. Both fixed in WP-A (#19), alongside a Copilot `1.0.81`
version-pin fix and the doc-honesty sweep the phase's name promised.

**Three work packages, sequenced and landed.** WP-A (#19, blockers + doc honesty) → WP-B (#20,
authorization re-basing: `gate_profile` now gates on the run's own requested `(kind, mode)`, not
`kind` alone, so a TUI run is authorized against its own TUI-suite effective capabilities instead
of its vendor's headless ones; CI's vendor-independent conformance signal deliberately substituted
the four `*-tui` fixture suites for the headless ones) → WP-C (#21, the headless drop proper:
`AdapterMode::Headless` kept deserializable but typed-rejected everywhere it's requested, per the
user's 2026-08-27 decision reversing WP29's recorded "KEEP ALL FOUR" stance (spec §4.6); the four
headless adapter implementations, their fixtures, and their conformance suites deleted outright —
not kept dark behind a flag). All three landed on `main` (`d27cee4`) after two full rounds of
crew-reviewer review plus a final polish pass — including one genuine near-miss (a mechanical
`AdapterMode::Headless` → `Tui` enum swap in a kill-switch test that compiled clean while its
assertions had gone stale) and a whole-engagement lesson about deletion sweeps needing to check
surviving *prose claims*, not just surviving *references* — both written up in
`docs/engineering-lessons.md`.

**Pre-tag bundle gate, closed with positive evidence.** The `refresh-bundle` workflow run on
post-merge `main` produced a bundle byte-identical to the one already committed — proving the
WP-C extension-side comment edits are inert through the bundler, not merely assumed to be. `v0.5.0`'s
tag is still held pending the repo owner's explicit `ship`.
