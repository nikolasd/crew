---
name: crew-orchestration
description: >-
  Use when a user asks Crew to delegate work to Claude, Codex, Copilot, or an OMP-RPC worker;
  plan multi-worker work; triage a worker timeout; or inspect, message, stop, or finish Crew runs.
---

# Crew orchestration

Crew executes durable worker runs. OMP owns the task graph and the leader's decisions; Crew persists, supervises, and replays the work.

## Leader flow

1. `crew_plan { op: "propose", runId, subtasks }` proposes a decomposition. The configured approval gate decides whether it may proceed; a write-capable plan can remain proposed for a human decision.
2. `crew_spawn { planId, subtaskId, prompt }` starts one approved subtask. It reuses a compatible idle worker or creates one, then links the submitted run to the plan.
3. Read milestone digests from `/crew`. The monitor is the live projection and reports questions, timeouts, budgets, escalations, and terminal edges; request `result` only after its terminal milestone.
4. `crew_send { runId, text, kind }` steers, answers, or follows up. Use one communication channel: Crew messages for worker coordination, never the same instruction through a vendor CLI, pane, and another agent channel.
5. `crew_stop { runId, outcome: "done" | "abort" }` ends one run. `crew_finish { runIds }` closes only the named plan runs; it never releases a workspace lease behind the leader's back.

## Routing profiles

`adapters.*.profile` is the routing profile the model sees: adapter, model, permission envelope, and mode. Reuse a worker only when its resolved profile matches the requested adapter/model. A vendor name never implies a model or permission level.

Crew stores task identity, not task prose. Pass the full instruction as `prompt` on every submit and retry; read every returned id from `details`, never invent one.

## Budgets and timeouts

- Each linked subtask snapshots its `turnBudget` or the configured default. Leader-originated messages consume it; at the limit `crew_send` returns `BUDGET_EXCEEDED` and journals the fact.
- A `WorkerTimeout` is a fact, not an automatic kill. Decide once: `crew_run { op: "timeoutAck", runId, decision: "extend" }` grants a fresh window; nudge with `crew_send`; or abort with `crew_run { op: "timeoutAck", runId, decision: "abort" }`.
- Worker questions and policy violations remain explicit leader/human decisions. Use `crew-approvals`; do not guess a human-required approval.

## Workspace modes

| Situation | `workspaceMode` |
|---|---|
| Default; one writer or read-only work | `shared` |
| Parallel writers in a Git repository | `isolated` (git worktree) |
| Work outside Git or a disposable filesystem copy | `copy` |

Shared writes serialize. Use a worktree when parallel writers need independent files, not merely to avoid reading the plan.

## Read surfaces

- `/crew` — live monitor; `runs`, `run <runId>`, `export [runId]`, `clean`, and `reopen <runId>`.
- `crew_status` — leader snapshot for tools.
- `crew_transcript` — bounded digest of replayed events.
- `crew_run { op: "result" }` — terminal result only; a non-terminal result is refused.

## Recovery

Retry creates a new run: `crew_run { op: "retry", priorRunId, workerId, prompt }`. Cancel or retry only with an observed run id and state. This skill never decides approvals or violations; use `crew-approvals`.
