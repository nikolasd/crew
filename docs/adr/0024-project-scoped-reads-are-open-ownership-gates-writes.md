# Project-scoped reads are open; ownership gates writes

* Status: Accepted
* Date: 2026-08-19

## Context and Problem Statement

An ownership gate added earlier (2026-08-18; see [ADR-0011](0011-omp-retains-task-graph-authority.md)'s
amendment) restricted the `workspace/*` surface: `acquire`, `apply`, `release`, and
`get` all refuse a caller whose session does not own the lease's run, with one uniform refusal
message. Its adversarial review then observed an asymmetry: every *other*
project-scoped read — `task/get`, `worker/list`, `worker/get`, `run/list`, `run/get`,
`message/list`, `approval/list`, `policy/violation/list`, `coordination/child/list`, and
`events/replay` — takes no principal at all, and several of them disclose the very facts
`workspace/get` now refuses to serve. `run/get` returns `workspacePath`; `events/replay`
carries every `LeaseAcquired` payload verbatim. Is the read side under-gated, or is
`workspace/get` gated for a different reason than confidentiality?

## Decision Drivers

* The socket is the security boundary: `0700` state directory, `SO_PEERCRED`/`LOCAL_PEERCRED`
  uid verification, and role-scoped method tables (ADR-0009). Every client that can connect at
  all is the same OS user.
* OMP owns the task graph (ADR-0011); BATMAN's journal exists to make the whole project's
  state observable and replayable. Monitors, doctors, and a *new* session recovering from a
  crash all legitimately need to see runs their session did not create.
* Ownership arbitration exists to serialize *mutation* — two sessions must not both
  drive one task — not to hide state from same-user readers.

## Considered Options

* Document the read side as deliberately open: same-user, role-admitted clients may read the
  whole project; ownership gates only mutation. `workspace/get`'s gate is surface uniformity
  with its three sibling mutations, not a confidentiality boundary.
* Gate every read that names another session's run behind the same ownership check the
  `workspace/*` surface uses.
* Gate only the reads that disclose filesystem paths (`run/get`'s `workspacePath`,
  `events/replay`'s lease payloads).

## Decision Outcome

Chosen option: the first. Project-scoped reads are open by design to any same-user client the
role table admits; ownership gates writes. `workspace/get` remains gated solely so the four
`workspace/*` methods answer with one uniform refusal — a caller probing `workspace/get`,
`workspace/apply`, and `workspace/release` for an unowned lease cannot use *which* method
refused as an ownership oracle. No confidentiality claim rides on it: the facts it withholds
are readable one door over via `run/get`, and were never secrets between processes running as
the same uid.

### Positive Consequences

* The monitor, the doctor, `batcave monitor`, and a recovering session all read the full
  project without impersonating the owning session — which is exactly what crash recovery
  requires, since the owning session may no longer exist.
* The rule is statable in one sentence, so every future read handler has a default answer:
  reads take no principal; mutations arbitrate ownership inside the transaction that writes.

### Negative Consequences

* `run/get` discloses `workspacePath` to any same-user client — the entry point that earlier
  ownership-gate review's evidence named. The gate that matters is that *mutating* that workspace requires ownership; the path
  itself is not a capability.
* A future multi-tenant deployment (different OS users proxied through one daemon) would
  invalidate the same-user premise this decision rests on and would have to revisit the whole
  read surface, not just one method.

## Pros and Cons of the Options

### Open reads, ownership-gated writes (chosen)

* Good, because it matches what the system already needs to function (recovery, monitoring)
  and documents the asymmetry found instead of leaving it implicit.
* Bad, because the confidentiality non-claim must stay documented, or the next reviewer
  re-derives the same finding.

### Ownership-gate every run-naming read

* Good, because the surface would be symmetric and `workspacePath` undisclosed.
* Bad, because it breaks the monitor, the doctor, and crash recovery outright — a new session
  owns nothing until `reconcile/omp` runs, and `reconcile/omp` itself needs reads to decide
  what to rebind. Symmetry here costs the system its own observability.

### Gate only the path-disclosing reads

* Good, because it narrows the disclosure without gating all reads.
* Bad, because it protects nothing (a same-user process can enumerate worktrees from the state
  directory it can already open) while adding a second, harder-to-state rule and breaking the
  monitor's degraded-run detail view.

## Links

* Documents the asymmetry found during the workspace-lease ownership gate's adversarial review
  (see [ADR-0011](0011-omp-retains-task-graph-authority.md)'s amendment)
* Read-side surface: `crates/runtime/src/service/orchestration.rs` (`task_get`, `worker_list`,
  `worker_get`, `run_list`, `run_get`, `message_list`, `approval_list`,
  `policy_violation_list`), `crates/runtime/src/ipc/connection.rs` (`events/replay`)
* Relies on the connection-level boundary from
  [ADR-0009](0009-role-based-authorization-from-the-connection-not-per-call.md)
* Mutation-side arbitration precedent:
  [ADR-0018](0018-approval-decided-before-callback-never-re-ask-on-failure.md) and the
  guarded-write doctrine journaled in Parts XXIII–XXVI, XXXIX
