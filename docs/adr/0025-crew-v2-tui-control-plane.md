# Crew v2 TUI control plane

* Status: Accepted
* Date: 2026-08-25

## Context and Problem Statement

Crew v2 introduces durable plans, multi-worker leader tools, TUI-backed adapters, panes, milestone digests, budgets, and timeout facts. A visible vendor terminal is useful evidence, but it must not become an unjournaled control plane that competes with OMP's task graph or Crew's durable message semantics.

A durable ownership boundary is therefore needed: who decides, who executes, and which channel carries an instruction.

> **Reconstructed reference — added 2026-09-08.** This section originally cited
> `docs/superpowers/specs/2026-08-22-crew-v2-design.md` §2.2/§2.3 for that requirement. That
> specification was an ephemeral working document under a gitignored path and no longer exists, so
> the citation could not be followed. What the requirement was is reconstructed here from this ADR's
> own text and from the shipped implementation, so that the decision stands without it. This is not a
> quotation: the specification's wording and section structure are not recoverable, and no attempt is
> made to reproduce them.
>
> The requirement it carried was that every surface crew v2 adds must resolve to one owner and one
> instruction channel. Those surfaces, each verifiable in the shipped code rather than in the lost
> document: durable plans (`plan/propose`, `plan/decide`, `plan/get`); leader-facing tools for
> multi-worker work (`crew_plan`, `crew_spawn`, `crew_send`, `crew_status`, `crew_transcript`,
> `crew_stop`, `crew_finish`); TUI-backed adapters driving a real vendor CLI on a PTY; visible attach
> panes with `pane/reopen`; milestone digests delivered to the leader; per-subtask turn budgets; and
> `WorkerTimeout` as a reported fact. Each of those could plausibly have been given its own control
> path — a pane that steers, a budget that resends, a timeout that kills — and the boundary below is
> what refuses that, once, for all of them.

## Decision Drivers

* OMP owns the task graph, scheduling, approval policy, and merge/finish decisions.
* Crew must persist intent before side effects, redact before durability, and broadcast the same committed envelope to live projections.
* A vendor TUI may be unavailable, restarted, or manually closed while the run and journal remain live.
* Two control channels for one worker create duplicated instructions, divergent histories, and budget accounting gaps.

## Considered Options

* Make panes the primary worker-control interface.
* Let OMP decide and Crew execute/replay; panes remain attach views and Crew messages remain the sole worker-control channel.
* Keep all worker state in OMP and use Crew only as a best-effort process launcher.

## Decision Outcome

Chosen option: OMP is the leader control plane; Crew is the durable execution and replay plane; a pane is an attach view only.

The leader proposes a plan, passes its approval gate, spawns approved subtasks, consumes milestone digests, then sends, stops, or finishes through Crew's RPC/tools. `WorkerTimeout` is a reported fact: the leader explicitly extends, nudges through one Crew message, or aborts. Budget exhaustion is a durable limit, never an excuse to resend through a vendor terminal.

`pane/reopen` may create another visible attach pane for a live socket, but it cannot create or steer a worker. Its `DisplayPaneAttached` journal write is ownership-guarded in the same transaction as the mutation.

### Positive Consequences

* Every leader decision and worker-visible instruction has one durable, replayable path.
* Monitor, dashboard, audit export, and crash recovery observe the same event stream as the leader.
* A closed or missing pane degrades observability, not control or run correctness.
* TUI and headless adapters share lifecycle, budget, timeout, and ownership semantics.

### Negative Consequences

* Operators cannot treat a vendor pane as an ad-hoc steering console; they must use `crew_send`.
* Reopening a pane requires a live attach socket and can honestly refuse for a settled run.
* The extension carries more leader-facing tools instead of hiding state behind vendor-specific terminal behavior.

## Links

* ADR-0011 — OMP retains task graph authority
* ADR-0020 — every durable mutation broadcasts its committed event
* ADR-0024 — project-scoped reads are open; ownership gates writes
* `docs/superpowers/specs/2026-08-22-crew-v2-design.md` §2.2, §2.3 — **no longer exists** (an
  ephemeral working document under a gitignored path). Its requirement is reconstructed in "Context
  and Problem Statement" above; this entry is left in place rather than deleted, because an ADR
  records what it cited when it was written.
