# OMP retains task-graph authority; Rust only enforces run lifecycle

* Status: Accepted
* Date: 2026-07-24

## Context and Problem Statement

The orchestration extension gives BATMAN durable task/worker/run records, tools, and an
authorization surface — enough machinery that it *could* start making its own scheduling,
worker-selection, retry, or merge decisions if nobody drew a hard line against it. If both OMP and
the Rust runtime can independently decide "what happens to this task next," the system has two
schedulers that can disagree — a split-brain waiting to happen. Where exactly does BATMAN's
authority end and OMP's begin, and how is that line actually enforced rather than just described
in a comment?

## Decision Drivers

* Explicit project constraint: never create or edit the OMP task graph inside Rust.
* BATMAN must be removable or replaceable without OMP losing its own state — it can never become
  the only place OMP's own scheduling decisions live.
* The runtime *does* see things OMP structurally cannot: raw process exit codes, protocol-level
  adapter acknowledgements, socket-level identity. That evidence needs somewhere authoritative to
  live, or it's wasted.

## Considered Options

* Rust never mutates OMP's own task graph. It persists OMP-supplied intent (`TaskRef`'s
  `ownerClientInstanceId` + monotonic `revision`) verbatim (see Amendment below), and enforces
  *only* the invariants
  that require evidence Rust alone can observe: run-lifecycle transitions (only after
  process/protocol evidence — ADR-0012) and task ownership (only via `reconcile/omp`'s
  revision-matched rebind).
* Rust mirrors OMP's task graph but can independently resolve, merge, or retry tasks, treating
  OMP's decisions as just one input among several.
* Rust owns scheduling outright; OMP becomes a thin UI over Rust's decisions.

## Decision Outcome

Chosen option: strict separation. Every orchestration RPC method that mutates state either
persists OMP-supplied intent unmodified (see Amendment below) (`task/upsert`) or applies a transition Rust alone has
standing to make because only Rust observed the evidence for it (`run/submit`'s driver-reported
`starting`/`working`, `approval/decide`'s callback-derived outcome). No method retries
automatically, selects a worker, merges anything, or edits an OMP-owned field without OMP asking
for it explicitly through the wire protocol.

### Positive Consequences

* No split-brain scheduling is possible, because there is exactly one scheduler: OMP.
* A BATMAN bug can corrupt BATMAN's own mirror of a task; it cannot corrupt OMP's own task-graph
  state, because BATMAN never writes to it.
* The boundary is enforceable in code review with one question per new method: "does this
  require evidence only the runtime can see, or is it a scheduling/policy decision that belongs to
  OMP?" — and it held across eighteen new methods without exception.

### Negative Consequences

* Some duplication is unavoidable: task existence is tracked in both OMP's own graph and BATMAN's
  `tasks` table, and the two must be explicitly reconciled (`reconcile/omp`) rather than trusting
  a single shared source.
* Every future orchestration feature must re-derive which side of this line it falls on — the
  temptation to let Rust "just handle" a convenient-looking decision (a retry, a worker pick)
  never goes away on its own; it has to be checked against this ADR each time.

## Pros and Cons of the Options

### Strict separation (chosen)

* Good, because authority is unambiguous and testable — a mutation either requires runtime
  evidence or it doesn't.
* Bad, because it means some logic OMP could delegate to Rust for convenience must instead be
  round-tripped through OMP explicitly.

### Rust as an independent second scheduler

* Good, because it could resolve some conflicts locally without a round trip to OMP.
* Bad, because "independently resolve" is exactly the split-brain scenario this decision exists to
  prevent — two authorities that can each believe they're right about the same task.

### Rust owns scheduling, OMP is a thin UI

* Good, because it would concentrate scheduling logic in one place (Rust) with strong types.
* Bad, because it directly violates the project's foundational constraint and would make BATMAN
  impossible to remove without OMP losing its own scheduling capability entirely.

## Amendment (2026-08-19, R76)

Both annotated claims above are now partial. `task/upsert`'s guarded write
(`DomainRepository::upsert_task`) still persists whatever revision and owner OMP presents when
*creating* a task -- there is no prior owner to protect -- but an *existing* task's
`ownerClientInstanceId` is no longer accepted verbatim on either side of the boundary:

* At the service layer, `task_upsert` refuses `ownerClientInstanceId != principal.instance_id`
  before the write is ever attempted. This is not new scheduling authority: it validates a
  caller-supplied value against the identity the connection layer already authenticated at connect
  time (ADR-0009), the same kind of check `reconcile/omp` already performed against its own
  `new_owner`.
* At the guarded write itself, `upsert_task`'s `ON CONFLICT` arm now conjoins R74's revision
  predicate with an ownership predicate (`excluded.owner_client_instance_id =
  tasks.owner_client_instance_id`) -- an existing row may only be re-upserted by its current owner.
  A non-owner presenting even the exact stored revision is refused, classified in the same
  transaction as `DomainError::NotOwner`.

Neither change hands Rust a scheduling decision OMP didn't already make. Both close the same gap:
before this fix, `task/upsert` was the one mutating method in this ADR's "persists intent verbatim"
category that took no principal at all, so a second `ompExtension` connection could present someone
else's task id, someone else's stored revision, and its own instance id, and the runtime would apply
it as if OMP itself had asked to transfer ownership -- which OMP had not. This ADR's actual boundary
line is unmoved: Rust still never resolves, merges, or retries anything OMP didn't ask for; it now
also refuses to let one caller impersonate another caller's *identity* when writing a field this ADR
always intended to reflect OMP's own intent. See `docs/journal.md` Part XXVII and `REVIEW.md`'s R76
resolution history for the full mechanism, including the run-lifecycle gap this fix's own review
found (R77) -- since closed by threading the same ownership check into `run/submit`, `run/retry`,
`run/cancel`, `message/send`, `workspace/acquire`, and `coordination/child/decide` (`docs/journal.md`
Part XXIX). The workspace-lease surface (`workspace/get`/`release`/`inspect`/`apply`, R81) closed
the same way one review later, gated by the same `run_owner_op` arbitration (`docs/journal.md` Part
XXX); that review sweep found no further unarbitrated task-scoped mutation, so the remaining
registered successors from this doctrine (R82-R86) are all Medium or Low, not High.

## Links

* Narrated in `../journal.md`, Part II introduction and throughout
* Enforced concretely by [ADR-0012](0012-explicit-run-lifecycle-relation-runtime-evidence-only.md)
* `REVIEW.md`'s R76, R77, R81 and R82-R86 (cited in the sections above) — **cited a register that no
  longer exists.** `REVIEW.md` was a maintainer-local, gitignored findings register; it is gone, so
  those numbers cannot be resolved by anyone. The mechanism each one indexed is described in this
  ADR's own prose above and needs no external source. The citations are left as written, for the same
  reason the `docs/journal.md` citations are (see `README.md` in this directory): an ADR records what
  it cited when it was written.
