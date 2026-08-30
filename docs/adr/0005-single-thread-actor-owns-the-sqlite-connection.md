# A single-thread actor owns the SQLite connection

* Status: Accepted
* Date: 2026-07-23

## Context and Problem Statement

SQLite (ADR-0003) wants one writer at a time, and the daemon is an async Tokio program with many
concurrent connections potentially wanting to read or write at once. Sharing one
`rusqlite::Connection` safely across async tasks, and guaranteeing "the call returned" means "it
committed," needs a concrete concurrency design.

## Decision Drivers

* Only one thread may safely hold and use the SQLite connection at a time.
* Callers must not be able to observe a write as "done" before its transaction has actually
  committed.
* The design should make illegal concurrent access a compile-time impossibility, not a runtime
  bug waiting to be found under load.
* Must not block Tokio's async worker threads (blocking a shared thread pool stalls unrelated
  work).

## Considered Options

* An actor: the connection lives on one dedicated `std::thread`; every other part of the program
  sends a `Command` value over a bounded `tokio::mpsc` channel, each carrying a `oneshot::Sender`
  for the reply; a write command's reply is sent only after `tx.commit()` succeeds.
* `Arc<Mutex<Connection>>` shared directly across async tasks, locked for the duration of each
  operation.
* A connection pool (e.g. `r2d2`), relying on SQLite's WAL mode to allow concurrent readers
  alongside a single writer.

## Decision Outcome

Chosen option: the actor pattern (`crates/runtime/src/db/actor.rs`). Ownership rules make it a
compile error for anything outside the actor thread to touch the connection; the channel's
request/reply shape makes "reply only after commit" a natural default rather than something that
has to be remembered at every call site.

### Positive Consequences

* The compiler enforces single-owner access — there is no `Mutex` to forget to lock, because
  there is nothing to lock.
* "The call returned" is defined, uniformly, to mean "it's durable" — every command handler
  replies after `tx.commit()`, never before.
* Blocking work (opening the connection, running migrations, joining the thread on shutdown) stays
  confined to this one thread and is joined via `spawn_blocking` when the async side needs to wait
  on it, rather than blocking a shared worker thread.

### Negative Consequences

* No read parallelism: even a read-only query queues behind the actor's single command stream.
  Acceptable at this project's per-repository scale; would need revisiting if read volume ever
  became a bottleneck.
* Every database access pays a channel round trip (send + `oneshot` await), which a direct
  in-process call would not.

## Pros and Cons of the Options

### Actor thread + bounded channel (chosen)

* Good, because ownership rules make misuse a compile error, and "reply after commit" falls out
  of the design for free.
* Bad, because it serializes all database access, reads included, through one channel.

### `Arc<Mutex<Connection>>`

* Good, because it needs no separate thread or channel machinery.
* Bad, because holding a `Mutex` across `.await` points is a well-known Tokio footgun (it can
  block the whole runtime), and nothing stops a future caller from holding the lock longer than a
  single logical operation.

### Connection pool with WAL concurrent reads

* Good, because it would allow genuine read parallelism.
* Bad, because it reintroduces exactly the kind of connection-lifetime bookkeeping the actor
  pattern was chosen to avoid, for a performance benefit this project doesn't currently need.

## Links

* Narrated in the engineering journal (since deleted; its durable lessons live in `../engineering-lessons.md`), commit `8cd8ad8`; the Rust tutorial's Day 6 was written to explain
  this exact file
* Reused, unmodified, by every later mutation added through Task 2 onward (see ADR-0011, ADR-0012)
