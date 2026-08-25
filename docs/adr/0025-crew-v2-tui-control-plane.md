# Crew v2 TUI control plane

* Status: Accepted
* Date: 2026-08-25

## Context and Problem Statement

Crew v2 introduces durable plans, multi-worker leader tools, TUI-backed adapters, panes, milestone digests, budgets, and timeout facts. A visible vendor terminal is useful evidence, but it must not become an unjournaled control plane that competes with OMP's task graph or Crew's durable message semantics.

The design specification §2.2/§2.3 therefore needs a durable ownership boundary: who decides, who executes, and which channel carries an instruction.

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
* `docs/superpowers/specs/2026-08-22-crew-v2-design.md` §2.2, §2.3
