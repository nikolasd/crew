# Learning Rust with this codebase as the textbook

**Audience & purpose:** anyone learning Rust with this repository as the textbook — most often a
contributor with a TypeScript background, but the material stands on its own for any newcomer. A
companion to [getting-started.md](getting-started.md), the developer manual, and to
[`docs/adr/`](adr/) and [engineering-lessons.md](engineering-lessons.md).

You know TypeScript. You don't know Rust. This guide gets you productive in the Crew Rust crates in
about a week by teaching each concept with code that is already in this repository. Every section
names real files — open them next to this document.

**How to use it.** Read a day, then open the files it names and read the surrounding code, not just
the excerpt. Each day ends with **Exercises**: do them in the repo, with `cargo check` running. The
excerpts here are real and current, but code moves — if an excerpt and the file disagree, the file
is right, and finding that is itself the skill this repo cares about most. Which brings us to the
one habit worth forming before any syntax:

> **Verify the claim against the code.** Every assertion in a comment, a doc, or a review is a
> testable statement. "This module handles config merging" is answered by `grep -rn "mod merge"` —
> and in this repo, that grep once revealed a component the architecture document described as live
> which was not even compiled. `cargo check` and `grep` are the two most useful tools in this
> tutorial. Reach for them before you reach for belief.

Seven days is the spine, not a limit — each day is a topic with real code, and the later ones carry
**Exercises** you should actually run. Treat a day as done when its exercises compile and you can
explain why they failed before they passed.

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
| a package | a **crate** (this repo has four: `crew-protocol`, `crew-runtime`, `crew-xtask`, `fake-worker`) |
| `bun test` | `cargo test` |
| `bun run build` | `cargo build` (`target/debug/crewd` is the output binary) |
| eslint / prettier | `cargo clippy` / `cargo fmt` |

`cargo test -p crew-protocol` = "run tests for that one workspace package".

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

`crates/protocol/src/lib.rs` is the crate root. It declares 15 child modules (`approval`, `artifact`, `coordination`, `display`, `event`, `ids`, `message`, `method`, `rpc`, `run`, `schema`, `task`, `version`, `worker`, `workspace`) and re-exports their public items so users write `crew_protocol::Timestamp` instead of `crew_protocol::event::Timestamp`.

This is the same pattern as a TypeScript barrel `index.ts`, except visibility is enforced: without `pub`, an item is private to its module — a fact Day 5 turns into a security mechanism.

**Do now:** run `cargo test -p crew-protocol`, then read all the files in `crates/protocol/src/` top to bottom. They're short, and they're 80% struct/enum declarations — ideal first Rust.

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

### Shared ownership, and why this codebase is full of `Arc`

`Arc<T>` is shared ownership with a thread-safe reference count: every clone is a new owner, and the
value drops when the last one does. In TypeScript every object reference behaves this way and you
never say so; in Rust you say so, and the type records it.

Read this signature from `crates/runtime/src/adapter/run_lifecycle.rs`:

```rust
pub fn wrap(
    inner: Arc<dyn AdapterEventSink>,
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    events_tx: broadcast::Sender<EventEnvelope>,
    run_id: RunId,
    activity: Arc<ActivityClock>,
) -> Arc<dyn AdapterEventSink>
```

Four things are being said at once, and each is a design decision rather than syntax noise:

* **`Arc<dyn AdapterEventSink>` in *and* out.** This function wraps a sink in another sink and
  returns something of the same shape, so wrappers compose: the production stack is a redacting sink
  wrapped by a lifecycle sink wrapped by a settlement sink. `dyn` means "some type implementing this
  trait, decided at runtime" — the TypeScript instinct here is an interface-typed variable.
* **`Arc<DatabaseHandle>`, not `&DatabaseHandle`.** A borrow would need a lifetime that outlives the
  wrapper, and the wrapper outlives this function call — it is stored and used later, from other
  tasks. When a value must be *kept*, shared ownership is the honest answer; a borrow is for
  *looking*.
* **`ProjectId` and `RunId` by value.** They are small `Copy`-ish identifier newtypes. Borrowing
  something smaller than a pointer buys nothing.
* **`broadcast::Sender` by value.** Senders are cheap to clone and are *meant* to be handed out;
  cloning one is how you get another producer.

The rule of thumb this codebase follows: **borrow to look, `Arc` to keep, clone when the cost is
smaller than the argument about it.** You will see `Arc::clone(&x)` written explicitly rather than
`x.clone()` in most places here — same effect, but it tells a reader "this is a refcount bump, not a
deep copy" at a glance.

### `Mutex`, and why `&self` methods can still mutate

Rust normally requires `&mut self` to change something. But `Arc<T>` hands out shared references, so
how does anything shared ever change? Through *interior mutability*: a `Mutex<T>` inside the struct
lets a `&self` method take the lock and mutate what is inside.

`crates/runtime/src/display/coordinator.rs` holds `live_panes: Arc<Mutex<HashSet<RunId>>>` and
mutates it from `&self` methods. This repo uses `parking_lot::Mutex`, whose `.lock()` returns the
guard directly rather than a `Result` — there is no lock poisoning to handle, which is why you will
not see `.unwrap()` after locks here.

For a single flag, a whole mutex is overkill. `crates/runtime/src/adapter/run_lifecycle.rs` uses an
`AtomicBool`:

```rust
working_observed: AtomicBool,
```

It is a latch: once a run has been observed working, stop paying for a state read on every
subsequent event. Two lessons hide in it. First, `Ordering::Relaxed` is chosen deliberately — the
flag guards an optimisation, not a correctness invariant, so no memory ordering is needed beyond
atomicity. Second, a latch that is never *reset* is a bug waiting to happen: this one has to be
reopened when a turn boundary parks the run, or the next turn's output is silently ignored. That
exact bug shipped and was caught by a test. Interior mutability is easy; deciding when state must be
cleared is the hard part.

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

The CLI layer (`crates/runtime/src/cli.rs`) defines each subcommand — `serve`, `status`, `stop`,
`monitor`, `schema`, `audit export`, and others (`doctor`, `config`, `conformance`, `adapters`,
`lease`, `attach`, `coordination-mcp`, `display` — the full, current list is `Command`'s own
definition, not repeated here since it keeps growing) — as a variant of a `Command` enum, and
dispatches to a typed handler function per variant. A representative slice:

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

### Exhaustive matching is a refactoring tool, not a chore

This is the single biggest practical difference from TypeScript, and it is worth internalising early.

When you add a variant to an enum, every `match` that lacks a `_ =>` arm stops compiling. That feels
like friction until the first time it saves you. A real instance from this repo: adding a `TurnEnded`
variant to `TuiEvent` — a vendor's end-of-turn boundary — broke compilation in about a dozen places
across parsers, the adapter shell, the event sink and several test fixtures. Every one of those was a
site that genuinely had to decide what the new variant meant. Nothing was forgotten, because nothing
*could* be forgotten.

The corollary is a style rule you will see enforced in review here: **avoid `_ =>` in matches over
your own enums.** A wildcard converts that compiler-guided refactor into a silent default. Where a
catch-all is genuinely right, this codebase names the variants anyway:

```rust
TuiEvent::AssistantText { .. }
| TuiEvent::ToolActivity { .. }
| TuiEvent::SessionMeta { .. }
| TuiEvent::TurnEnded { .. } => true,
TuiEvent::Raw { .. } => false,
```

That is from `emits_a_payload` in `crates/runtime/src/adapter/tui/mod.rs`. Adding a variant breaks
it, which is the point — whether a new event carries a payload is exactly the kind of question a
human should answer once, explicitly.

### `match` on the *shape* of data, not just its tag

Patterns destructure. You can match on nested structure, bind parts, and add conditions:

```rust
let exit = match &event.payload {
    AdapterEventPayload::ProcessExited { exit_code, signal } => {
        Some((*exit_code, signal.clone()))
    }
    _ => None,
};
```

Two details from `run_lifecycle.rs` worth copying. It matches on `&event.payload` — a *reference* —
because `event` is moved into the inner sink immediately afterwards; matching by value would consume
the payload before the code that needs it runs. And the `_ => None` here is over a foreign-ish
payload enum where only one variant is relevant, with the exhaustive decision made elsewhere.

You will also meet guards and `if let` chains, which this codebase uses heavily:

```rust
if let Some(injection) = inject
    && let Err(err) = write_paste(pty, kind, "start", injection.text, injection.write_timeout).await
{
    // both conditions held; `injection` and `err` are in scope
}
```

Chained `let` conditions like this are a recent-edition feature and appear throughout the runtime.
Read them as "if all of these bind, then".

### Exercises — Day 3

1. **Feel the compiler work for you.** Add a variant `Paused` to `TuiEvent` in
   `crates/runtime/src/adapter/tui/mod.rs`, run `cargo check -p crew-runtime`, and read every error.
   Count the sites. Now delete the variant and confirm the errors go away. You have just performed
   the refactor Rust is best at.
2. **Break a wildcard.** In `emits_a_payload`, replace the named `Raw` arm with `_ => false`, add
   your `Paused` variant again, and observe that it now compiles *silently* with a default you never
   chose. Revert both. This is the argument against `_` in one experiment.
3. **Read an error type.** Open `crates/runtime/src/adapter/tui/mod.rs` and find a
   `#[derive(thiserror::Error)]` enum. For each variant, decide whether a caller could sensibly
   *recover* from it. That question is what separates `thiserror` (typed, caller might branch) from
   `anyhow` (opaque, just report it).

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

The runtime crate's `lib.rs` exposes over 20 public modules — `adapter`, `approval`, `audit`,
`canonical_json`, `config`, `conformance`, `coordination`, `dashboard`, `db`, `display`, `doctor`,
`domain`, `env_flag`, `ipc`, `lifecycle`, `paths`, `policy`, `recovery`, `security`, `service`,
`supervisor`, `timeout_sweep`, `workspace` — each re-exporting its key types. When you're lost
about where something lives, start from `lib.rs`.

**Do now:** pick `RuntimeStatus` in `rpc.rs`, follow it to
`packages/protocol-ts/src/generated/RuntimeStatus.ts` and to its entry in
`packages/protocol-ts/schema/crew.schema.json`. Change nothing; just see that the Rust struct is
the one place that shape is defined, and that both other files are downstream of it.

### Default methods: how a trait grows without breaking its implementors

A trait method can have a body. Implementors that say nothing get that body; implementors that care
override it. This is how you extend a trait that already has several implementations without touching
any of them — and the *choice of default* is a safety decision.

From `crates/runtime/src/adapter/tui/mod.rs`:

```rust
fn recorded_prompt(&self, entry: &serde_json::Value) -> Option<String> {
    let _ = entry;
    None
}
```

`TranscriptFormat` gained this method so the adapter could verify that a vendor recorded the whole
prompt it was sent. Four vendor formats implement the trait; three now answer this question, and the
default means the fourth — and any future one — is not *forced* to. Returning `None` disables the
verification rather than failing it, so a format whose shape is unknown cannot fail runs it has no
opinion about. A default of "assume intact" would have been the dangerous choice: silently claiming
success. **A check that cannot judge must abstain, not guess.**

Note `let _ = entry;` — it silences the unused-parameter warning while keeping the parameter named
for documentation. You will see this idiom wherever a default ignores its arguments.

### `dyn Trait`, and the bounds that make it usable across threads

`Arc<dyn AdapterEventSink>` is a *trait object*: the concrete type is erased and calls dispatch
through a vtable, exactly like an interface-typed variable in TypeScript. The Rust-specific part is
that sharing one across threads requires the trait to promise thread-safety:

```rust
pub trait AdapterEventSink: Send + Sync { /* ... */ }
```

`Send` means the value can move between threads; `Sync` means `&T` can be shared between them. Tokio
tasks may run on any worker thread, so anything held across an `await` needs these. When you get a
"future is not `Send`" error, the usual cause is holding a non-`Send` guard — like a `MutexGuard` —
across an `await`. The fix is almost always to narrow the lock's scope so it is dropped before the
await, not to reach for a different lock type.

Traits also make testing cheap. Because the runtime depends on `Arc<dyn AdapterEventSink>` rather
than a concrete sink, tests substitute a recording fake in one line. That is dependency injection
with no framework — the trait *is* the seam.

### Generics vs. trait objects: which one and why

Both appear here, and the choice is not arbitrary:

| | Generic (`fn f<T: Trait>(x: T)`) | Trait object (`Arc<dyn Trait>`) |
|---|---|---|
| Dispatch | static, monomorphised per type | dynamic, one vtable call |
| Cost | zero at runtime, larger binary | one indirection |
| Needs | type known at compile time | type chosen at runtime |
| Used here for | `Classified<T>`, `parse_jsonl_chunk<F>` | sinks, adapters, display backends |

The rule: if the set of types is fixed and known where you write the code, use a generic. If callers
pick at runtime — which adapter, which display backend, which sink stack — use a trait object.

---

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

### The channel taxonomy: pick by how many, how often

Tokio gives you several channel types and the choice encodes intent. All four appear in this
codebase, each for a reason you can read off the problem:

| Channel | Shape | Used here for |
|---|---|---|
| `oneshot` | one value, one time | "this run has settled" / "its slot is free" |
| `mpsc` | many senders, one receiver | PTY write jobs queued to one writer thread |
| `broadcast` | one sender, many receivers, each gets a copy | journal events fanned out to every subscriber |
| `watch` | latest value only, many receivers | current-state style updates |

The `oneshot` pair is the most instructive. From `crates/runtime/src/adapter/event_sink.rs`:

```rust
let (settled_tx, settled_rx) = oneshot::channel();
let (slot_tx, slot_rx) = oneshot::channel();
```

Two separate one-shot signals from one sink, because two different things now happen at two different
moments: a *turn* ending frees the concurrency slot, while the *process* exiting tears the session
down. They used to be one signal, and splitting them was the whole point of a design change. When you
find yourself wanting to fire one signal for two purposes, that is usually two signals.

`broadcast` has a property worth knowing before it surprises you: a slow receiver can *lag* and miss
messages, receiving `RecvError::Lagged(n)` instead. Code here handles that explicitly rather than
treating it as fatal — a viewer that fell behind skips ahead rather than killing the worker. And a
receiver only sees messages sent *after* it subscribed, which is why subscription order matters and
why one call site in the TUI adapter carries a comment explaining that it subscribes before spawning
anything, or it would miss the first output.

### Blocking work in an async world

`await` yields; blocking does not. A blocking call on an async worker thread stalls every other task
on that thread. This codebase keeps the boundary explicit in two ways.

Filesystem scanning goes to a blocking pool:

```rust
let scan_result = tokio::task::spawn_blocking(move || {
    scan(&root_for_scan, started_at, &nonce_for_scan, MAX_DEPTH)
}).await;
```

And PTY writes get a dedicated OS thread with an `mpsc` queue, because `write_all` on a pty master
genuinely blocks until the kernel accepts every byte. The async side sends a job and awaits an
acknowledgement; the thread does the blocking work. That shape — *own thread, queue in, ack out* — is
the standard answer for "this API is blocking and I cannot change it".

The database is the same idea taken further: one thread owns the single SQLite connection, and callers
send closures to it. That is Day 6's actor pattern, and it is why "atomicity" in this codebase means
"inside one closure" — see [engineering-lessons.md](engineering-lessons.md) for the bugs that taught
everyone that boundary.

### Exercises — Day 6

1. **Watch a lag happen.** Write a test that subscribes to a `broadcast` channel with a tiny
   capacity, sends more messages than it holds without receiving, then receives. Observe
   `RecvError::Lagged`. Decide what the right behaviour would be for a pane viewer versus for the
   journal.
2. **Break the blocking boundary.** In a scratch test, call `std::thread::sleep` inside a
   `#[tokio::test]` (single-threaded by default) alongside a spawned task and watch the task starve.
   Then swap to `tokio::time::sleep` and see it interleave.
3. **Find the subscribe-before-spawn comment.** Search `crates/runtime/src/adapter/tui/adapter.rs`
   for the `subscribe_output` call and read why it happens where it does. Move it later in the
   function, run the TUI adapter tests, and see what fails.

---

## Day 7 — Testing, tooling, macros, and fluency drills

### Tests

- **Unit tests** live inside source files in a `#[cfg(test)] mod tests { ... }` block (= "compile
  only when testing"). See the bottom of `security/redaction.rs`, `version.rs`, `ids.rs`, etc.
- **Integration tests** are files in `crates/runtime/tests/` compiled as separate crates using
  only the public API — 50+ test files covering `paths`, `database`, `redaction_boundary`,
  `ipc`, `lifecycle`, `domain_repository`, `orchestration_rpc`, `coordination`, `approval`, the
  TUI vendor adapters (Claude/Codex/Copilot/OMP-RPC, e.g. `tui_adapter.rs`,
  `tui_claude_registry.rs`), every display (terminal, tmux, herdr), audit, conformance,
  supervisor, workspace (apply, lease, materialize), config, and monitor. The lifecycle suite
  runs the actual compiled binary via `env!("CARGO_BIN_EXE_crewd")` as real child processes.
- Protocol integration tests live in `crates/protocol/tests/`: `wire_contract.rs`,
  `workspace_contract.rs`, `domain_contract.rs`, `coordination_contract.rs`, `fixtures.rs`.
- `#[test]` marks a test; `#[tokio::test]` gives it an async runtime; `assert!`, `assert_eq!`
  are the assertion macros. `tempfile::TempDir` is the throwaway-directory helper you'll see in
  nearly every runtime test.

Run one test with output: `cargo test -p crew-runtime --test ipc -- --nocapture <name_substring>`.

### The tools that keep you honest

```bash
cargo clippy --workspace --all-targets   # the linter; this repo keeps it warning-clean
cargo fmt --all                          # the formatter; --check in CI
cargo doc -p crew-runtime --open       # rendered API docs from the /// doc comments
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

### A test you have not watched fail is decoration

This is the house standard, and it is worth more than any syntax on this page.

A test written after the code it tests passes on the first run — which proves nothing. It may assert
the wrong thing, assert the implementation rather than the behaviour, or never execute the branch you
care about. The discipline:

1. Write the assertion first and run it. Confirm it fails **for the reason you expect** — feature
   missing, not a typo or a compile error.
2. Make it pass.
3. If you wrote the code first, *mutate* the code: break the thing deliberately and confirm the test
   notices. Then revert.

Step 3 has caught real problems in this repo. A framing test looked green but passed on an empty
result — vacuously true. A guard test only proved anything once its guard was deleted and the test
was watched failing. If you cannot make a test fail on demand, you do not yet know what it tests.

```rust
// A vacuous assertion: passes even if `paste_chunks` returns nothing at all.
for chunk in paste_chunks(&prompt, 512) {
    assert!(chunk.len() <= 512);
}

// Non-vacuous: the loop cannot be trivially satisfied.
let chunks = paste_chunks(&prompt, 512);
assert!(chunks.len() > 1, "this input must actually be split");
assert_eq!(delivered_payload(&chunks), prompt);
for chunk in &chunks { assert!(chunk.len() <= 512); }
```

Any `for`/`any`/`all` assertion over a collection you did not first assert is non-empty has this
failure mode. Check the collection's size before checking its contents.

### `#[tokio::test]` has flavours, and the default will deadlock you

```rust
#[tokio::test]                                                  // current-thread
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]      // real threads
```

The default is a **single-threaded** runtime. If your test blocks that thread — a `std::sync::mpsc`
receive, a lock held across a spawn, anything genuinely blocking — while waiting for a spawned task
to make progress, nothing can progress. It looks exactly like a hang, and in CI it looks exactly like
a slow build.

That is not hypothetical: a registry test in this repo deadlocked precisely this way, because the
fixture it borrowed blocks a whole thread inside an authorization call. Its neighbours carry
`flavor = "multi_thread"` for the same reason. **When a test that spawns something hangs, check the
flavour before you suspect your logic.**

### Where the tests live, and why in two places

| Location | For | Sees |
|---|---|---|
| `#[cfg(test)] mod tests` in the source file | unit tests | private items — `pub(crate)`, private fns |
| `crates/*/tests/*.rs` | integration tests | only the public API, like a real consumer |

The split is not stylistic. A private helper can only be tested from inside its module; a public
contract is better tested from outside, because that is where a bug in the *shape* of the API shows
up. This repo has hit bugs that only an integration test could see — a fixture that compiled fine but
made a promise the public path did not keep.

One consequence worth planning for: an integration test cannot see `pub(crate)` items. If you find
yourself wanting to widen visibility purely for a test, that is usually a signal to test through the
public seam instead — or to move the test into the module.

### Run the whole suite, not the part you touched

`cargo test -p crew-runtime --lib adapter::tui` is the fast loop. It is not the gate.

Three classes of failure only appear in a full `cargo test --workspace` run:

* **Compile-only fixtures.** A struct literal in another crate's test breaks when you add a field.
  Nothing in your crate notices.
* **Contract tests.** This repo asserts the exact list of methods each client role may call. Adding a
  method fails that test *by design* — a role's surface must never widen unnoticed.
* **Committed-artifact tests.** Generated files — a JSON schema, a default config snapshot — are
  compared byte-for-byte against what the code would produce now.

All three have caught real breakage here. The habit: iterate narrow, gate wide.

### Exercises — Day 7

1. **Earn one test.** Pick any test in `crates/runtime/src/adapter/tui/input.rs`, break the
   production code it covers, and confirm it fails with a message that tells you what went wrong.
   If the message is unhelpful, improve it — a failure message is documentation read at the worst
   possible moment.
2. **Cause the deadlock on purpose.** Take a `multi_thread` test in
   `crates/runtime/tests/adapter_registry.rs`, drop it to plain `#[tokio::test]`, and watch it hang.
   Restore it. You now recognise the symptom for life.
3. **Find a vacuous assertion.** Search the test suites for a `for` loop or `.all(` over a collection
   whose length is never asserted. Either prove it cannot be empty or add the assertion.
4. **Trip a contract test.** Add a variant to `CrewMethod` in `crates/protocol/src/method.rs`, run
   `cargo test --workspace`, and read which tests fail and why. Revert. You have just met the repo's
   immune system.

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
