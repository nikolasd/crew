---
name: crew-recovery
description: >-
  Use when a daemon restart left runs in an unexpected state, a run shows state `lost`,
  crew_stop's outcome didn't do what you expected, a workspace lease seems stuck, or your
  own OMP session dropped and reconnected with active tasks outstanding.
---

# Crew recovery

Covers what happens around the edges of the normal run lifecycle: daemon crashes, killed workers, stuck leases, and your own session dropping. For the normal lifecycle (submit, settle, finish, stop), see crew-orchestration.

## Daemon crash recovery

If `crewd` crashes or is killed, runs can be left in a non-terminal state (`queued`, `starting`, `working`, `waitingUser`, `waitingPeer`, `paused`). The next daemon start runs a recovery sweep, once, synchronously, before it accepts any connection:

1. **Resume first.** For each stuck run, the daemon tries to continue it on the *same* vendor session its previous incarnation established (never a retry, never a new run — a genuine continuation with the same run id). Eligibility requires a real vendor session id, a resolvable adapter, TUI mode (headless can't resume), and its deterministic transcript file actually existing on disk.
2. **Fallback if resume isn't eligible or fails:** `queued`/`starting`/`working` become `failed`; `waitingUser`/`waitingPeer`/`paused` become `cancelled` — but only if the daemon's configuration opts into recovering those. If it doesn't, or the run is otherwise ineligible, it's **left exactly as it was, untouched** — not terminalized. A run in a stuck-looking state after a restart is not necessarily abandoned; check with `crew_run { op: "get" }` before assuming it needs a retry.
3. There is no periodic re-sweep. A quiet run is not automatically presumed dead while the daemon is alive — nothing else ever declares a live run stuck based on age alone.

## What `lost` means, and how it differs from `failed`

Terminal states are `succeeded`, `failed`, `cancelled`, and `lost`. `lost` is not a synonym for `failed` — it specifically means the supervisor could not observe how the vendor process actually exited (no exit code, no signal — the information genuinely isn't there), as opposed to `failed`, which means an exit was observed and it was a real failure (nonzero code, or a signal). Don't treat `lost` as "it failed, retry it" without first considering that the daemon simply doesn't know what happened — a `lost` run's own transcript is still the best source of what state it was actually left in.

## `crew_stop`'s real behavior

`crew_stop { runId, outcome: "done" | "abort" }`: **both outcomes kill the worker process immediately** — there is no graceful/soft stop today, regardless of outcome. `"done"` additionally sends a wrap-up follow-up message right before the same kill (so the transcript records why); `"abort"` kills with no message. Don't expect `"done"` to let a worker finish its current turn before stopping — it doesn't wait for anything.

Neither `crew_stop` nor `crew_finish` releases a workspace lease. If a run acquired an isolated workspace (a worktree or copy), stopping or finishing the run leaves that lease held — release it explicitly with `crew_workspace { op: "release", leaseId }` once you're done with it.

## Stuck or stale workspace leases

`/crew doctor` reports stale leases (one stuck `allocating` past its grace period, or one whose backing worktree/copy vanished from disk) by run id and state. The fix is the daemon CLI, not a tool call: `crewd lease release --repo <repo> --lease-id <id>`.

A shared write lease is exclusive project-wide — a second concurrent write request is refused, naming isolation (`gitWorktree` or `copy`) as the way to get a lease that won't conflict. Isolated leases never conflict with each other or with a shared lease, and a single run may legitimately hold more than one lease at once (e.g. a read-only shared view alongside its own isolated write worktree).

## Your own session dropped and reconnected

If your OMP session was interrupted and restarted, and you had active tasks running under the prior session, use `crew_reconcile { taskId, revision }` to rebind ownership of those tasks to the current session. It requires the task's monotonic OMP revision to match what the daemon has stored — a mismatch is refused (not silently accepted) to prevent two sessions racing to claim the same task. Do this before trying to act on a task from a session you suspect was already interrupted once.
