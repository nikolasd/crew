# Architecture Decision Records

Every entry here uses the [MADR](https://adr.github.io/madr/) format. An ADR is written once a
decision is made and left alone afterward — if a later decision reverses one, the later ADR says
so explicitly (`Status: superseded by ADR-00NN`) and this one is never edited to pretend it always
agreed. Read [../architecture.md](../architecture.md) for the resulting design with no history attached, and
[../engineering-lessons.md](../engineering-lessons.md) for the bugs and invariants that came out of
building it. Several ADRs below cite `docs/journal.md`, a per-commit development narrative that has
since been removed; the lessons worth keeping from it were absorbed into `engineering-lessons.md`.
Those citations are left as written — an ADR records what was decided when it was decided, and is
never edited to read as though it always agreed with what came later.

| ID | Title | Status |
|---|---|---|
| [0001](0001-omp-extension-with-separate-rust-daemon.md) | External OMP extension with a separate Rust daemon | Accepted |
| [0002](0002-rust-canonical-protocol-with-generated-bindings.md) | Rust as the canonical protocol, with generated bindings | Accepted |
| [0003](0003-sqlite-as-the-sole-persistence-engine.md) | SQLite as the sole persistence engine | Accepted |
| [0004](0004-json-rpc-2-over-bounded-ndjson-on-a-unix-socket.md) | JSON-RPC 2.0 over bounded NDJSON on a Unix socket | Accepted |
| [0005](0005-single-thread-actor-owns-the-sqlite-connection.md) | A single-thread actor owns the SQLite connection | Accepted |
| [0006](0006-type-enforced-redaction-boundary.md) | Type-enforced redaction boundary before persistence | Accepted |
| [0007](0007-repository-scoped-singleton-via-kernel-flock.md) | Repository-scoped daemon singleton via kernel flock | Accepted |
| [0008](0008-connect-or-spawn-with-idle-self-shutdown.md) | Connect-or-spawn daemon lifecycle with idle self-shutdown | Accepted |
| [0009](0009-role-based-authorization-from-the-connection-not-per-call.md) | Role-based authorization from the connection, not per call | Accepted |
| [0010](0010-platform-binaries-as-npm-optional-leaf-packages.md) | Platform binaries as npm optional leaf packages | Superseded by [0022](0022-github-release-download-cache-replaces-npm-leaf-packages.md) |
| [0011](0011-omp-retains-task-graph-authority.md) | OMP retains task-graph authority; Rust only enforces run lifecycle | Accepted |
| [0012](0012-explicit-run-lifecycle-relation-runtime-evidence-only.md) | Explicit run-lifecycle relation, applied only on runtime evidence | Accepted |
| [0013](0013-injectable-run-driver-seam-fake-by-default.md) | Injectable `RunDriver` seam, fake by default | Accepted |
| [0014](0014-flat-op-discriminator-over-zod-discriminated-unions.md) | Flat `op` discriminator over Zod discriminated unions | Accepted |
| [0015](0015-omp-native-facts-as-non-owning-mirror-lost-on-omission.md) | OMP-native facts as a non-owning mirror, `lost` on omission | Accepted |
| [0016](0016-coordination-scope-tokens-bound-to-run-and-pid-ancestry.md) | Coordination scope tokens bound to run identity and PID ancestry | Accepted |
| [0017](0017-record-before-delivery-message-semantics.md) | Record-before-delivery message semantics | Accepted |
| [0018](0018-approval-decided-before-callback-never-re-ask-on-failure.md) | Approval decided before callback; never re-ask on failure | Accepted |
| [0019](0019-monitor-is-one-reducer-over-replay-and-live-no-separate-modes.md) | The monitor is one reducer over replay and live — no separate modes | Accepted |
| [0020](0020-per-mutation-event-broadcast-is-not-optional.md) | Per-mutation event broadcast is not optional | Accepted |
| [0021](0021-shared-client-authenticates-with-the-union-of-required-roles.md) | A shared client authenticates with the union of every caller's role | Accepted |
| [0022](0022-github-release-download-cache-replaces-npm-leaf-packages.md) | GitHub Release download-cache replaces npm optional leaf packages | Accepted |
| [0023](0023-run-state-edges-from-adapter-evidence.md) | Run-state edges derive from adapter evidence; an unobservable exit is `lost` | Accepted, amended by [0027](0027-turn-end-settles-a-run.md) |
| [0024](0024-project-scoped-reads-are-open-ownership-gates-writes.md) | Project-scoped reads are open; ownership gates writes | Accepted |
| [0025](0025-crew-v2-tui-control-plane.md) | Crew v2 TUI control plane | Accepted |
| [0026](0026-headless-retirement.md) | Headless control plane retirement | Accepted |
| [0027](0027-turn-end-settles-a-run.md) | A run is a conversation the leader closes; a vendor's turn-end is durable evidence, not a terminal state | Accepted |
| [0028](0028-submit-prompt-is-journaled-redacted-run-intent.md) | The submit prompt is journaled, redacted, as durable run intent | Accepted |

## When to add one

A new ADR is warranted when a decision is hard to reverse, affects more than one file, and someone
could reasonably have chosen differently. Not every commit needs one — most don't. If you're
unsure, ask: "if I'm wrong about this in six months, will I need to know *why* I thought it was
right?" If yes, write it down here before you forget the alternatives you rejected.
