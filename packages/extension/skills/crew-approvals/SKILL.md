---
name: crew-approvals
description: >-
  Use when a run is pending approval, quarantined by a policy violation,
  or blocked by a nested-child spawn request.
  Fires on "approve that", "deny it", "why is this run blocked",
  "a run needs approval", "a child wants to spawn".
---

## How Crew tools work

Two facts are invisible from the tool schemas but essential for correct use:

- **Crew stores no task text of its own.** The `prompt` argument must be supplied on every `run/submit` **and every `run/retry`**. Retry does not remember the prior prompt — you must pass it again.
- **Every Crew tool returns the daemon's JSON result verbatim under `details`.** Read ids (`taskId`, `workerId`, `runId`, `leaseId`, `approvalId`, `violationId`) from there. Never invent or guess them.

## Approvals

Call `crew_approval { op: "list" }` to see pending approvals. Each approval has a `humanRequired` flag:

- When `humanRequired` is `true`, the runtime enforces this server-side and **rejects any model-supplied decision**. With no interactive UI present, the honest action is to leave the approval pending and tell the user it needs their attention. Do not fabricate a decision.
- When `humanRequired` is `false`, a model-supplied decision is allowed and you may resolve it.

## Violations

A policy violation quarantines a run — it makes no further progress until every unresolved violation on it is decided. A run can have more than one open violation at once.

- Call `crew_violation { op: "list" }` (optionally with `runId`) to see every recorded violation and its decision state. An entry with `resolution: null` on a quarantined run is the one holding the quarantine.
- Call `crew_violation { op: "decide", violationId, resolution }` to decide one violation. The `resolution` is exactly `"release"` (resume the quarantined run) or `"cancel"` (end it). A "release" only lifts quarantine if this was the *last* unresolved violation on the run — the result's `quarantineCleared` field (`true`/`false`, absent for `cancel` or an already-decided replay) says whether it did; `false` means a different violation is still open — find it with `op: "list"`.

## Child spawn requests

When a worker wants to spawn a nested child, it records the intent — nothing happens until you decide.

- `crew_child { op: "list" }` shows pending child requests (optionally filtered by `runId`).
- `crew_child { op: "decide", parentRunId, decision }` resolves a request:
  - **Accept** requires `childTaskId`, `childWorkerId`, and `childRunId` — these provision the child run.
  - **Deny** requires a `reason` explaining the refusal.

A request is only an intent — accepting is what actually creates the child run.
