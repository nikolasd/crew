# Rust in a week — a fast track grounded in this codebase

**Audience & purpose:** contributors with a TypeScript background who are new to Rust — a
companion to [getting-started.md](getting-started.md), the developer manual, and to
[journal.md](journal.md) (each "Day" below is the concept behind one of that document's commits).

You know TypeScript. You don't know Rust. This guide gets you productive in the Crew Rust crates
in about a week by teaching each concept with the code that's already in this repository. Every
section names real files — open them next to this document.

The plan:

| Day | Topic | Home base in this repo |
|---|---|---|
| 1 | Toolchain, syntax anatomy, modules | `crates/protocol/` |
| 2 | Ownership, borrowing, moves | `crates/runtime/src/paths.rs` |
| 3 | Enums, `Option`/`Result`, `?`, errors | `crates/runtime/src/lifecycle.rs` |
| 4 | Structs, traits, derives, generics, serde | `crates/protocol/src/rpc.rs`, `event.rs` |
| 5 | Visibility as a security tool, newtypes | `crates/runtime/src/security/redaction.rs` |
| 6 | Threads, channels, async/Tokio | `crates/runtime/src/db/actor.rs`, `ipc/` |
| 7 | Testing, tooling, macros, fluency drills | `crates/runtime/tests/` |

---

## Day 1 — Toolchain, syntax anatomy, modules

### Cargo is npm + tsc + bun test in one

| You know | Rust equivalent |
|---|---|
| `package.json` | `Cargo.toml` (root one defines the *workspace*; each crate has its own) |
| `bun.lock` | `Cargo.lock` |
| a package | a **crate** (this repo has four: `batman-protocol`, `batman-runtime`, `batman-xtask`, `fake-worker`) |
| `bun test` | `cargo test` |
| `bun run build` | `cargo build` (`target/debug/crewd` is the output binary) |
| eslint / prettier | `cargo clippy` / `cargo fmt` |

`cargo test -p batman-protocol` = "run tests for that one workspace package".

The workspace uses Rust edition 2024 and pins `rust-version = "1.97.1"`. The resolver is `"3"`.

### Anatomy of a Rust file

Open `crates/protocol/src/version.rs`. Almost everything you'll ever read is one of these forms:

```rust
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema, TS,
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }
}
```

Note the derives: `Copy, PartialEq, Eq, PartialOrd, Ord, Hash` were added so this type can be used
as a map key, sorted, and compared — protocol versions are cheap value types. The `#[ts(export)]`
attribute hooks into `ts-rs` to generate TypeScript bindings automatically.

Decoder ring for the sigils you'll meet constantly:

| Symbol | Meaning | TS analogy |
|---|---|---|
| `::` | path separator / static access | `.` for namespaces: `ProtocolVersion::new(1, 0)` ≈ `ProtocolVersion.new(1, 0)` |
| `let x = …;` | immutable binding (default!) | `const x` |
| `let mut x = …;` | mutable binding | `let x` |
| `&x` / `&mut x` | borrow (read-only / writable) — Day 2 | no analogy; the big new idea |
| `fn f(x: u64) -> String` | typed function | `function f(x: number): string` |
| `Self` / `self` | the type / the instance | the class / `this` |
| `|x| x + 1` | closure | `(x) => x + 1` |
| `foo!(…)` | **macro** call (code generated at compile time) | no analogy; `println!`, `format!`, `assert_eq!` are macros |
| `#[derive(…)]`, `#[serde(…)]` | attributes annotating the next item | decorators, roughly |
| `#[must_use]` | compiler warns if the return value is discarded | no direct TS analogue |
| `const fn` | a function callable at compile time (const evaluation) | no direct TS analogue |
| last expression, no semicolon | the return value | explicit `return` (allowed too, but idiomatic Rust omits it) |

That last one trips everyone: in `fn f() -> u32 { 41 + 1 }` the `41 + 1` *is* the return because it
has no trailing `;`. Add a semicolon and it becomes a statement returning `()` — and the compiler
will complain about the type mismatch.

### Modules

`crates/protocol/src/lib.rs` is the crate root. It declares 15 child modules (`approval`, `artifact`, `coordination`, `display`, `event`, `ids`, `message`, `method`, `rpc`, `run`, `schema`, `task`, `version`, `worker`, `workspace`) and re-exports their public items so users write `batman_protocol::Timestamp` instead of `batman_protocol::event::Timestamp`.

This is the same pattern as a TypeScript barrel `index.ts`, except visibility is enforced: without `pub`, an item is private to its module — a fact Day 5 turns into a security mechanism.

**Do now:** run `cargo test -p batman-protocol`, then read all the files in `crates/protocol/src/` top to bottom. They're short, and they're 80% struct/enum declarations — ideal first Rust.

## Day 2 — Ownership and borrowing (the one genuinely new idea)

Rust has no garbage collector. Instead, every value has exactly **one owner**, and the compiler
tracks it. Three rules cover most of what you'll read:

1. **Assignment moves.** `let b = a;` for a heap value (e.g. `String`, `PathBuf`, `Vec`) makes `b`
   the owner; using `a` afterwards is a compile error. (Cheap `Copy` types — integers, `bool`,
   `ProtocolVersion` — are copied instead, like JS primitives.)
2. **`&T` borrows read-only, `&mut T` borrows writably.** Many `&T` borrows may coexist; a
   `&mut T` must be exclusive. The compiler enforces this — data races become compile errors.
3. **Owner goes out of scope → value is freed** (its `Drop` runs). Deterministic, no GC pauses.

Read a real signature with these glasses on (`crates/runtime/src/paths.rs`):

```rust
pub fn resolve(state_root: &Path, repository: &Path) -> Result<Self, PathError>
```

- `&Path` — "lend me a path to look at; I won't keep it or mutate it". The caller keeps ownership.
- The returned `RuntimePaths` is a brand-new owned value; the caller now owns it.

And the pairs you'll see everywhere:

| Borrowed (a view) | Owned (the data) | TS mental model |
|---|---|---|
| `&str` | `String` | both are "string"; `&str` is "someone else's string, read-only" |
| `&Path` | `PathBuf` | ditto for filesystem paths |
| `&[T]` | `Vec<T>` | readonly array view vs. the array |

`.clone()` makes an independent owned copy when you genuinely need one; `.to_owned()` /
`.to_string()` convert borrowed → owned. When you fight the borrow checker in week one, cloning is
an acceptable escape hatch — correctness first, elegance later.

One more owner shape you'll meet on Day 6: `Arc<T>` (atomic reference count) = shared ownership
across threads, like every JS object reference, but explicit. `crates/runtime/src/ipc/server.rs`
shares its state between connection tasks with `Arc<Shared>`.

**Do now:** in `paths.rs`, find every `&` in the function signatures and say out loud who owns
what. Then deliberately break something: change `state_root: &Path` to `state_root: PathBuf` and
read the compiler errors at the call sites — Rust's errors are unusually good teachers. Revert.

---

## Day 3 — Enums, pattern matching, `Option`, `Result`, `?`

### Enums carry data

Rust enums are tagged unions — TypeScript discriminated unions, but first-class
(`crates/runtime/src/lifecycle.rs`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// No live runtime was found to stop.
    NotRunning,
    /// A live runtime was signalled and its socket was removed.
    Stopped,
}
```

and with payloads (`crates/protocol/src/rpc.rs`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(tag = "role", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub enum ClientAuth {
    OmpExtension { instance_id: String, agent_directory: String },
    WorkerMcp { instance_id: String, scope_token: String },
    Display { instance_id: String },
}
```

`match` consumes them and the compiler forces you to handle **every** variant — add a variant
later and every non-exhaustive `match` in the codebase becomes a compile error pointing you at
what to update. That is why this codebase leans so hard on enums.

The CLI layer (`crates/runtime/src/cli.rs`) defines six subcommands — `serve`, `status`, `stop`,
`monitor`, `schema`, and `audit export` — each as a variant of a `Command` enum, and dispatches
to a typed handler function per variant:

```rust
#[derive(Subcommand)]
enum Command {
    Serve { … },
    Status { … },
    Stop { … },
    Monitor { … },
    Schema,
    Audit { command: AuditCommand },
}
```

### No `null`, no exceptions

- `Option<T>` = `Some(value)` or `None`. This is `T | undefined` made honest — you *cannot* forget
  the `None` case. Example: `ServeOptions.idle_seconds: Option<u64>` (an omitted `--idle-seconds`).
- `Result<T, E>` = `Ok(value)` or `Err(error)`. This replaces `throw`/`try`/`catch` for expected
  failures. Every fallible function in this repo says so in its type:
  `Result<RuntimePaths, PathError>`, `Result<Self, DbError>`, `Result<(), ServeError>`.

The `?` operator is the ergonomics that make this bearable:

```rust
let paths = RuntimePaths::resolve(&state_dir, &repo)?;   // Err? return it to my caller. Ok? unwrap it.
```

`?` ≈ "await-and-rethrow" for `Result`s. Chains of `?` read like straight-line happy-path code
while still propagating every failure — `lifecycle::serve` is a good long example.

`.unwrap()` / `.expect("msg")` extract the `Ok`/`Some` and **crash** otherwise. In this codebase
they're allowed in tests and for provably-infallible cases (always with `expect` and a message
saying *why* it can't fail — see `paths.rs` building a `ProjectId` from a hash it just produced).
Never reachable from user or network input.

### Error types with `thiserror`

Each module defines an error enum; `#[derive(thiserror::Error)]` writes the boilerplate
(`crates/runtime/src/paths.rs`):

```rust
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("path is not valid UTF-8: {path:?}")]
    NonUtf8 { path: PathBuf },
    // ...
}
```

The `#[error(...)]` string is the human-readable message. `#[from]` variants (see `ServeError`)
let `?` auto-convert a lower layer's error into this layer's — that's how a `DbError` deep inside
`serve` surfaces as a `ServeError` at the CLI.

**Do now:** read `cli.rs` end to end (it's ~330 lines across six subcommands), then trace one
`?` in `lifecycle::serve` down to the error enum variant it produces.

---

## Day 4 — Structs, traits, derives, generics, serde

### Traits ≈ interfaces, derives ≈ free implementations

A trait declares capability (`Serialize`, `Debug`, `Clone`). `#[derive(...)]` asks the compiler
(or a library macro) to implement it for you. Now this line — one of the most common lines in
`crates/protocol` — reads fully:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct ClientInfo { pub name: String, pub version: String }
```

- `Debug` — printable with `{:?}` (Day 5 shows a hand-written one used as a security control).
- `Clone` — `.clone()` works.
- `Serialize`/`Deserialize` — serde JSON encode/decode.
- `JsonSchema`/`TS` — schemars + ts-rs codegen hooks; **this is how the TypeScript bindings and
  the JSON schema fall out of the Rust types**.
- The `#[serde(...)]` attribute is configuration: camelCase field names on the wire, and reject
  unknown fields on input (that's Ajv's `additionalProperties: false`, but on the Rust side).
- `#[ts(export)]` — `ts-rs` will generate a TypeScript type for this struct at build time.

Enum representation attributes are worth 5 minutes because the wire shape depends on them:
`ClientAuth` uses `#[serde(tag = "role", ...)]` (internal tag → `{"role":"ompExtension", ...}`),
`RuntimeEvent` uses `#[serde(tag = "type", content = "payload")]` (adjacent tag →
`{"type":"diagnostic","payload":{...}}`). Compare with the JSON in
`fixtures/protocol/initialize.request.json`.

### Generics

`crates/protocol/src/rpc.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export)]
pub struct JsonRpcRequest<P> { /* ... params: P ... */ }
```

Exactly TypeScript's `JsonRpcRequest<P>`, except resolved at compile time (no erasure).
`Classified<T>` in `event.rs` is the same idea. Trait bounds like `impl Into<PathBuf>` in
`DatabaseHandle::start(path: impl Into<PathBuf>)` mean "anything convertible into a `PathBuf`" —
that's why call sites can pass a `&Path`, `PathBuf`, or `&str`.

Traits also give you *seams* for testing without a mocking framework: `ipc/mod.rs` defines the
`PeerCredentialReader` and `WorkerCredentialVerifier` traits; production wires
`SystemPeerCredentialReader` / `RejectAllWorkerVerifier`, tests inject fakes. That's dependency
injection, Rust-style: a trait object or generic parameter instead of a DI container.

The runtime crate's `lib.rs` exposes 19 public modules — `adapter`, `approval`, `audit`, `config`,
`conformance`, `coordination`, `db`, `display`, `doctor`, `domain`, `ipc`, `lifecycle`, `paths`,
`policy`, `recovery`, `security`, `service`, `supervisor`, `workspace` — each re-exporting its
key types. When you're lost about where something lives, start from `lib.rs`.

**Do now:** pick `RuntimeStatus` in `rpc.rs`, follow it to
`packages/protocol-ts/src/generated/RuntimeStatus.ts` and to its entry in
`packages/protocol-ts/schema/crew.schema.json`. Change nothing; just see that the Rust struct is
the one place that shape is defined, and that both other files are downstream of it.

## Day 5 — Visibility as a security boundary ("make illegal states unrepresentable")

This is the most instructive Rust lesson in the repo. Requirement: *nothing reaches the database
unless it went through redaction.* In TypeScript you'd write that in a comment and hope. In Rust
it's enforced by the module system (`crates/runtime/src/security/redaction.rs`):

```rust
/// A sanitized event, the only type the database actor's journal accepts.
/// Fields are private; there is no public constructor.
#[derive(Debug, Clone)]
pub struct PersistableEvent {
    timestamp: Timestamp,      // <- fields are NOT pub
    project_id: ProjectId,
    // ...
    event_json: String,
}
```

Private fields + no public constructor ⇒ code outside this module **cannot create one**. The only
producer is `Redactor::sanitize(raw: RawRuntimeEvent) -> PersistableEvent`, which drops
`Thinking`/`Secret` content and masks secret-shaped strings. And the database actor's append API
accepts *only* `PersistableEvent` (`db/actor.rs::append_event`). The type system now proves the
security property: unredacted data has no route into SQLite. `SanitizedJson` repeats the pattern
for operation payloads.

Two supporting ideas in the same file/area:

- **Newtypes.** `pub struct SanitizedJson(String);` wraps a plain `String` in a distinct type so
  it can't be confused with an arbitrary string. All 9 id types (`ProjectId`, `TaskId`, `RunId`,
  `OperationId`, `MessageId`, `ArtifactId`, `ApprovalId`, `WorkerId`, `PolicyViolationId` — in
  `crates/protocol/src/ids.rs`) are newtypes over UUIDv7 strings — you cannot pass a `TaskId`
  where a `RunId` is expected, even though both are "just strings" on the wire. The `uuid_id!`
  macro generates them (macros: Day 7).
- **A hand-written trait impl as a control.** `Classified<T>` implements `Debug` manually so that
  `{:?}` prints `<redacted>` for non-visible content — even accidental debug logging can't leak a
  secret (`crates/protocol/src/event.rs`).

The redaction module actually owns two types for this boundary:

- `RawRuntimeEvent` — the unsafe, pre-redaction form that exists only in process memory.
- `PersistableEvent` — the safe, post-redaction form that can be persisted.

The `Redactor` struct is the sole bridge between them.

**Do now:** try to defeat it. In any runtime file, attempt to construct a
`PersistableEvent { ... }` literal or call a constructor. Read the compiler's refusal. That error
message *is* the security review.

---

## Day 6 — Concurrency: threads, channels, async/Tokio

### The actor pattern (threads + channels)

SQLite wants one writer from one thread. This codebase gives the `rusqlite::Connection` to a
single OS thread and lets everyone else talk to it via messages
(`crates/runtime/src/db/actor.rs`):

```text
async caller ──(bounded mpsc: Command + oneshot reply-sender)──▶ actor thread (owns Connection)
      ◀──────────────(oneshot: Result<T, DbError>)──────────────┘
```

- `tokio::sync::mpsc::channel(32)` — a bounded multi-producer single-consumer queue. Capacity is
  32 (defined as `COMMAND_CHANNEL_CAPACITY`). Bounded = backpressure: producers wait instead of
  ballooning memory.
- Each `Command` variant (an enum, of course) carries a `oneshot::Sender` — a one-shot reply
  envelope. The caller `await`s the matching `oneshot::Receiver`.
- Write commands reply only **after** `tx.commit()` succeeds, which is how "the call returned"
  comes to mean "it's durable".

The `DatabaseHandle` is cheap to clone (it's an `Arc`), and `shutdown()` takes `&self` so the
clean drain-and-join runs even while other clones of the handle are still live. A
`DomainClosure` type alias (`Box<dyn FnOnce(&mut Connection) -> Result<Value, DomainError> + Send + 'static>`)
is the unit of work dispatched to the actor thread.

This is Go-style "share memory by communicating", and it's the standard Rust answer to "one
resource, many users" — no mutex spaghetti, and ownership rules mean the compiler *verifies* that
only the actor thread touches the connection.

### IPC: per-connection reader + serialized writer

Each accepted socket connection is handled by `ipc/connection.rs::handle()`: one reader loop
parses incoming JSON-RPC frames, and a single serialized writer task serializes outgoing frames.
A `WriterMsg` enum and an `mpsc::Sender<WriterMsg>` ensure that subscription event notifications
never interleave mid-frame with a request response. The writer loop enforces a maximum frame size
(4 MiB default, 64 KiB minimum).

### async/await and Tokio

Rust's `async fn` ≈ TypeScript's `async function`, with two differences that matter for reading
this code:

1. **Futures are lazy** — nothing runs until `.await`ed (JS promises are eager).
2. **There's no built-in event loop** — Tokio provides it. `#[tokio::main]` on `main` starts the
   runtime; `tokio::spawn(async { ... })` ≈ launching a background task (used per-connection in
   `ipc/server.rs`); `tokio::select!` races several futures (used for shutdown-vs-accept).

The cardinal sin is **blocking inside async** (freezes a worker thread the whole runtime shares).
When this codebase must block — joining the DB actor's OS thread in `DatabaseHandle::shutdown` — it
wraps the call in `tokio::task::spawn_blocking`, which shunts it to a dedicated blocking pool. If
you see `spawn_blocking` in review comments, this is why.

Sharing state across tasks combines Day 2 tools: `Arc<Shared>` (shared ownership) in
`ipc/server.rs`, and channels rather than locks wherever possible. The per-connection design —
one reader task, one writer task fed by a bounded channel — is in `ipc/connection.rs`.

### Broadcast channels for event fan-out

The runtime uses `tokio::sync::broadcast::channel(64)` to fan events to every live
`events/subscribe` connection. The sender lives in `Shared.events_tx` and is cloned to
`OrchestrationService`, `CoordinationBroker`, `ApprovalService`, and `RunDriverContext`. Each
live subscription calls `.subscribe()` for its own receiver; every mutation calls
`domain::broadcast_committed(&events_tx, &mut result)` after its transaction commits. If you add
a mutation and forget to call `broadcast_committed`, nothing errors — the monitor just never
updates for that one case; see [`docs/engineering-lessons.md`](engineering-lessons.md#durable-mutations-must-broadcast-the-same-event-they-just-committed) for exactly this bug.

**Do now:** read `db/actor.rs` top to bottom (it is the best-commented file in the repo), then
find where `lifecycle::serve` calls `db.shutdown()` and confirm the ordering guarantee the
architecture doc promises: stopping event committed → actor closed → socket removed.

---

## Day 7 — Testing, tooling, macros, and fluency drills

### Tests

- **Unit tests** live inside source files in a `#[cfg(test)] mod tests { ... }` block (= "compile
  only when testing"). See the bottom of `security/redaction.rs`, `version.rs`, `ids.rs`, etc.
- **Integration tests** are files in `crates/runtime/tests/` compiled as separate crates using
  only the public API — there are 30+ test files covering `paths`, `database`, `redaction_boundary`,
  `ipc`, `lifecycle`, `domain_repository`, `orchestration_rpc`, `coordination`, `approval`, every
  adapter (claude, codex, copilot, omp_rpc), every display (terminal, tmux, herdr), audit,
  conformance, supervisor, workspace (apply, lease, materialize), config, and monitor. The
  lifecycle suite runs the actual compiled binary via `env!("CARGO_BIN_EXE_crewd")` as real
  child processes.
- Protocol integration tests live in `crates/protocol/tests/`: `wire_contract.rs`,
  `workspace_contract.rs`, `domain_contract.rs`, `coordination_contract.rs`, `fixtures.rs`.
- `#[test]` marks a test; `#[tokio::test]` gives it an async runtime; `assert!`, `assert_eq!`
  are the assertion macros. `tempfile::TempDir` is the throwaway-directory helper you'll see in
  nearly every runtime test.

Run one test with output: `cargo test -p batman-runtime --test ipc -- --nocapture <name_substring>`.

### The tools that keep you honest

```bash
cargo clippy --workspace --all-targets   # the linter; this repo keeps it warning-clean
cargo fmt --all                          # the formatter; --check in CI
cargo doc -p batman-runtime --open       # rendered API docs from the /// doc comments
```

Treat clippy as a tutor: it usually names the exact idiomatic replacement.

### Macros, just enough

You don't need to *write* macros for a long time, but you'll read three kinds here:

1. Utility macros: `println!`, `format!`, `vec![]`, `assert_eq!` — function-like, the `!` is the
   tell.
2. Derive macros: `#[derive(Serialize, ...)]` — Day 4.
3. One local declarative macro: `uuid_id!` in `crates/protocol/src/ids.rs`, which stamps out the
   nine id newtypes from one template. Read it once to demystify `macro_rules!`; it's
   find-and-replace with hygiene.

### Fluency drills (in increasing order of ambition)

1. Add a `RuntimeEvent::Heartbeat` variant, run `cargo build`, and fix every place the compiler
   points at. Then `bun run generate` and watch the TS type update itself. Revert.
2. Write a unit test in `security/redaction.rs` proving a new secret-shaped pattern of your
   choosing is masked (then add the regex rule to make it pass — TDD, as this repo practices it).
3. Add a `crewd paths --repo <dir>` debug subcommand to `cli.rs` that prints the resolved
   `RuntimePaths` as JSON. Touches clap, `Result`, serde — Days 1–4 in one exercise. (Don't ship
   it; it's a kata.)
4. Read `lifecycle.rs` start to finish. When the flock/`LockGuard`/`Drop` interplay makes sense —
   why crash-safety needs *no code at all* here, because the kernel releases the lock when the
   process dies and `Drop` handles the graceful path — you're no longer a beginner.

### Where to go deeper

- [The Rust Book](https://doc.rust-lang.org/book/) — chapters 4 (ownership), 6 (enums), 9
  (errors), 10 (generics/traits), 16 (concurrency) map directly onto Days 2–6.
- [Tokio tutorial](https://tokio.rs/tokio/tutorial) — its channels + actor chapters describe
  exactly the pattern in `db/actor.rs`.
- `docs.rs` for any dependency (`serde`, `rusqlite`, `nix`, `tokio-util`) — hover-quality docs for
  the whole ecosystem.

The unifying theme you should leave with: **this codebase uses Rust's compiler as its enforcement
mechanism** — exhaustive `match` for protocol evolution, ownership for connection/lock lifetimes,
module privacy for the redaction boundary, and traits as injectable seams for testing. When you
review or write Rust here, the question is rarely "does it work" and usually "does the type
system *prove* it works".
