---
name: crew-orchestration
description: >-
  Use when a user asks Crew to delegate work to Claude, Codex, Copilot, or an OMP-RPC worker;
  plan multi-worker work; triage a worker timeout; or inspect, message, stop, or finish Crew runs.
---

# Crew orchestration

Crew executes durable worker runs. OMP owns the task graph and the leader's decisions; Crew persists, supervises, and replays the work.

## Worker provisioning flow (profile-first)

To run any task via Claude, Codex, Copilot, or OMP-RPC:

1. **Register a profile** — `crew_profile { adapter: "claude", model: "sonnet", startupOptions: { claude: { mode: "tui" } } }`. The profile captures adapter, model, startup options, and environment allowlist. Receives a `profileId`. `model` is optional-with-ask-once: if omitted and none is configured yet for that adapter, you get a typed `model-not-configured` error instead of a guess — ask the user which model to use by offering the vendor's stable aliases (e.g., "sonnet", "opus", "haiku" for Claude), never dated model IDs, then call `crew_profile` again with it; the answer is persisted into the repo's `.omp/crew.json` and reused silently on every later call, for that adapter, for good. Passing a model that *conflicts* with one already configured is refused too, with a typed `model-conflict` error naming the stored value — `crew_profile` never overwrites it and never silently ignores the conflict; the fix is editing the repository's `.omp/crew.json` directly (`/crew config path` locates it — `/crew config` has no set/edit subcommand). `mode: "tui"` is filled in automatically for a reserved adapter (claude, codex, copilot, ompRpc) when omitted — headless is retired, but you never have to remember to spell out the replacement.
2. **Create a worker** — `crew_worker { op: "create", profileId }`. The worker identity binds to that profile permanently. Receives a `workerId`.
3. **Submit a run** — `crew_run { op: "submit", taskId, workerId, prompt }` to execute a task. The prompt is the full instruction; Crew stores identity, not prose.

**Important:** The legacy flow (fingerprint, adapter, model on every worker create) is rejected. Always use profiles.

## Run lifecycle: a run settles, it doesn't just exit

A TUI vendor process never exits between turns — it sits there waiting for the next instruction. So a run's answer becomes readable, and the leader's turn to act begins, the moment the vendor finishes a turn, not only when the whole run reaches a terminal state (`succeeded`/`failed`/`cancelled`/`lost`):

1. A finished turn parks the run at `waitingUser` and journals the turn-end. The `/crew` monitor now emits a milestone for this the same way it does for terminal states, worker questions, and timeouts — you do not have to poll for it.
2. `crew_run { op: "result", runId }` is readable as soon as that happens (a settled `waitingUser`, not only a terminal run) — no need to cancel the run first to read its answer.
3. From there you have three real choices, none of which requires a terminal state first:
   - `crew_send { runId, text, kind }` to follow up — a follow-up on an existing run is never refused, so there is no need to cancel-and-resubmit just to keep steering the same conversation.
   - `crew_run { op: "finish", runId, outcome: "succeeded" | "failed" }` — the leader's own decision that this conversation is done. States the outcome explicitly (default `"succeeded"`); never inferred from the vendor's own turn markers, because a finished turn only means "the vendor stopped talking," not "the task went well." Works on any non-terminal run, not only one that has settled a turn.
   - `crew_stop { runId, outcome: "done" | "abort" }` to end it by killing the worker instead (see below) — reach for `finish` when the conversation is genuinely complete, `stop` when you need the process gone regardless.

`crew_finish { runIds }` is a different, narrower tool: it **cancels** the named plan runs in a loop (the same effect as `crew_stop { outcome: "abort" }`, batched) — it does not settle anything and never states an outcome. Use `crew_run { op: "finish" }` when you want to record how one run's conversation actually went; use `crew_finish` only to bulk-cancel a plan's remaining live runs once you're done with the plan as a whole. Neither ever releases a workspace lease behind your back.

`crew_stop { runId, outcome: "done" | "abort" }`: both outcomes kill the worker process immediately — there is no server-side graceful/soft stop today, regardless of which outcome you pick. `"done"` additionally sends a wrap-up follow-up message right before the same kill, so the worker's transcript records why it stopped; `"abort"` kills with no message. Pick `"done"` for a courtesy note, not for a gentler shutdown — there isn't one.

## Leader flow

1. `crew_plan { op: "propose", runId, subtasks }` proposes a decomposition. The configured approval gate decides whether it may proceed; a write-capable plan can remain proposed for a human decision.
2. `crew_spawn { planId, subtaskId, prompt }` starts one approved subtask. It reuses a compatible idle worker or creates one (profile-first via profileId from the plan), then links the submitted run to the plan.
3. Read milestone digests from `/crew`. The monitor is the live projection and reports questions, timeouts, budgets, escalations, settled turns, and terminal edges as they happen — you never need to poll for any of these.
4. `crew_send { runId, text, kind }` steers, answers, or follows up, on a run in any state. Use one communication channel: Crew messages for worker coordination, never the same instruction through a vendor CLI, pane, and another agent channel.
5. `crew_stop` kills a run; `crew_run { op: "finish" }` settles one with a stated outcome; `crew_finish { runIds }` bulk-cancels a plan's remaining runs. See "Run lifecycle" above for which one to reach for.

## Routing profiles

`adapters.*.profile` is the routing profile the model sees: adapter, model, permission envelope, and mode. Reuse a worker only when its resolved profile matches the requested adapter/model. A vendor name never implies a model or permission level.

Crew stores task identity, not task prose. Pass the full instruction as `prompt` on every submit and retry; read every returned id from `details`, never invent one.

## Budgets, timeouts, and concurrency

- Each linked subtask snapshots its `turnBudget` or the configured default. Leader-originated messages consume it; at the limit `crew_send` returns `BUDGET_EXCEEDED` and journals the fact.
- A `WorkerTimeout` is a fact, not an automatic kill. Decide once: `crew_run { op: "timeoutAck", runId, decision: "extend" }` grants a fresh window; nudge with `crew_send`; or abort with `crew_run { op: "timeoutAck", runId, decision: "abort" }`. `extend` can be refused (`-32602`, no tracked timeout to extend) if the run settled between the timeout being journaled and this call arriving — an **expected, benign race** (you acted correctly, just slightly late), not a fault: don't retry or escalate it, just check the run's current state with `op: "get"` if you're unsure what happened.
- Worker questions and policy violations remain explicit leader/human decisions. Use `crew-approvals`; do not guess a human-required approval.
- Two caps bound concurrency (ADR-0027): `limits.maxConcurrentWorkers` (default 4) bounds runs actively taking a turn at once; `limits.maxLiveSessions` (default 16, always at least the ceiling) separately bounds *every* session that exists, including ones parked at a settled `waitingUser` — a settled run you haven't finished or stopped yet still occupies a live-session slot even though it isn't actively working. The refusal for hitting this cap lands on `crew_run { op: "submit" }` (a typed refusal, the same shape as the concurrency ceiling's own), never on `crew_send` — an existing run can always be steered regardless of the cap. Don't leave settled runs open indefinitely if you're running many concurrent subtasks and expect to submit more.

## Workspace modes

| Situation | `workspaceMode` |
|---|---|
| Default; one writer or read-only work | `shared` |
| Parallel writers in a Git repository | `isolated` (git worktree) |
| Work outside Git or a disposable filesystem copy | `copy` |

Shared writes serialize. Use a worktree when parallel writers need independent files, not merely to avoid reading the plan.

## Read surfaces

- `/crew` — live monitor; `runs`, `run <runId>`, `export [runId]`, `clean`, `reopen <runId>`, `health`, `doctor`, and `config` (`path | print [effective|defaults|schema] | init [global] [force]` — inspecting or scaffolding `crew.json`, no set/edit subcommand).
- `crew_status` — leader snapshot for tools.
- `crew_transcript` — bounded digest of replayed events.
- `crew_run { op: "result" }` — readable once the run is terminal *or* has settled a turn without exiting (see "Run lifecycle" above); a run that hasn't reached either is refused.

## Recovery

Retry creates a new run: `crew_run { op: "retry", priorRunId, workerId, prompt }`. Cancel or retry only with an observed run id and state. This skill never decides approvals or violations; use `crew-approvals`. For daemon-crash recovery, the `lost`/`failed` distinction, and reattaching after your own session dropped, use `crew-recovery`.
