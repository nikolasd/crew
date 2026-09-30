# A leader-gone run is instructed to stop and save, and the kill follows the instruction's own window

* Status: Proposed
* Date: 2026-09-29
* Supersedes: *(none — this extends a decision, it does not reverse one)*
* Amends: [0036](0036-leader-disconnect-grace-window.md) (the teardown mechanism; detection is unchanged)

> **This record is Proposed and nothing in it is yet a decision.** It awaits the owner's
> ratification. **The two questions it had to answer before ratification were both answered by the
> owner on 2026-09-29, by two separate rulings**, and each is named under "The two questions, and
> where each now stands" below with its own status there. What remains open there is not either
> question, but one named consequence of question 2 — the size of a backstop the owner ruled into
> existence and did not size, and which a third ruling of the same date has since **gated on a
> named prerequisite** rather than left merely unset. Ratifying it means: landing that prerequisite
> and resolving that open item, then changing this header's `Status:` to `Accepted` and following
> the index in [README.md](README.md) — not editing
> [0036](0036-leader-disconnect-grace-window.md), which records what was decided on 2026-09-09 and
> is not edited to read as though it always agreed.
>
> **Amended 2026-09-29 (uncommitted).** A third owner ruling, the same date, settled the *ordering*
> of the backstop's sizing: *"I agree c and then e"*. **The settle loop is to be made concurrent
> before any value is chosen, and the value is then to be short, with the documented manual escape
> as the sanctioned way to grant a wedged agent more time.** Consequences now carry that ruling in
> full; item 3 below is updated to state the value as **deliberately unset and gated**. **The
> header's `Status:` is unchanged and the record remains Proposed** — nothing here is ratified, and
> ADR ratification remains the owner's and is not conferred here. The backstop's value, the
> in-band version-recording question, and whether Claude's protocol mode can receive a death notice
> all remain open and are not resolved by this amendment.

## How to read the labels

Four provenance labels are used, and the point of the scheme is that a reader can tell an owner
ruling from an agent's reading of one:

* `[USER-STATED]` — the owner said it. Quoted verbatim where the wording matters; never paraphrased
  into something firmer than what was said.
* `[VERIFIED]` — opened and read in this repository at commit `2962c80`, or against a document named
  as such. Every `file:line#Symbol` below was opened and the named symbol was seen at that line or
  inside that range; a line *inside* a cited declaration is written "line N of" the anchored range.
  A line in prose (a Markdown record, a comment block) is cited by path and line with the sentence
  quoted, because it has no declaration to anchor to. `UNVERIFIED` marks anything not resolved.
* `[AGENT-PROPOSED]` — a design choice or inference put forward for the owner to accept or reject.
* `[AGENT-DECIDED]` — a choice made where the owner left the mechanism open. It is still not the
  owner's ruling, and is marked so it can be reversed.

**An inference is never presented as a ruling.** Where the owner ruled a consequence and the design
supplied the object, both are said.

## What the owner ruled

`[USER-STATED]` 2026-09-29, verbatim:

> "driver dies, agents stop and save. If ADR-0036 describes a different ruling, then ADR-0036 should
> be replaced. If not, then it should be ammended."

> "Instruct-then-kill-on-timeout"

> "yes"

The third quotation is the answer to the question "does the graceful stop reuse the existing window
or carry its own?" — **it carries its own.** It is quoted bare because it is a bare answer; what it
answers is stated here so the record reads on its own.

> "What crew can observe is whether the agent is idle, waiting an answer or still working. If it is
> idle, not waiting nor working, then the agent is considered that all his work is finished."
>
> "a paused agent is an idle agent. There is not other state for paused. So the answer is told to
> stop."
>
**The fourth quotation is dated 2026-09-29 and is the answer to question 1 below** — whether a
`paused` run counts as "finished", the one value the question turned on. It is quoted bare because
it is a bare answer; what it answers is stated here so the record reads on its own. **It is a
definition of a word, not a preference between two readings**, and that is what makes it an answer
rather than a tie-break. It is recorded with the mapping it produces under question 1.
**A fifth ruling, the same date and later the same day, settled question 2.** Offered the choice of
a fixed interval against waiting on the agent's own report, the owner chose the second: the
graceful stop is **not timed**, and Crew waits until the agent **reports itself idle and not
waiting**, on the owner's own finished-state definition. A timeout is retained **only as a
backstop**, so that a wedged or unresponsive agent cannot hang the settle forever. **The owner was
offered a number for that backstop and declined it.** **No verbatim of that exchange is carried
here, and none is invented** — what is recorded is its substance, in full, at question 2 below,
where the backstop's **unset** size is carried as an open item in its own right.
>
> "I agree c and then e"
>
**A sixth ruling, the same date and last of the day, decided the *ordering* of the backstop's
sizing, and it is quoted verbatim because the order is the ruling.** Offered five ways to reach that
value, the owner chose **(c) then (e)**: **parallelise the settle loop first, and only then pick a
number; and set it short, with the manual escape documented.** **What it did is not supply a
number** — it ruled that no number is to be chosen until the loop is concurrent, which is a stronger
statement than declining one. It is recorded in full under "The backstop's sizing, ruled on
2026-09-29 and ordered" in Consequences below, with its effect on item 3, where the value is now
carried as **open, deliberately unset, and gated on a named prerequisite**. **The fifth ruling's
substance above is unchanged by it**; what changed is that the unset value now has a reason and an
order, and (d) is on the record as considered and not chosen.

**The branch the ruling itself names is settled and is not re-opened here.** ADR-0036 does not
describe a different ruling — it says nothing about instructing a worker at all — so it is **amended,
not replaced**, and only its teardown is touched. An earlier investigation reached the same
conclusion, that detection is reused unchanged and only the teardown conflicts; that conclusion is
carried into this record as given and is not re-derived below.

## Context and Problem Statement

ADR-0036 chose how a daemon decides a run's leader is gone, and what it then does to the run. The
first half is detection; the second half is teardown. **This record touches the second and not the
first.**

Detection is a refcounted registry of leader connections, each entry carrying the instant of its last
true 1→0 transition
(`crates/runtime/src/ipc/leader_registry.rs:45-57#Entry`), armed from a drop guard
(`crates/runtime/src/ipc/leader_registry.rs:89-107#LeaderRegistry::register`) through the
zero-transition discriminator
(`crates/runtime/src/ipc/leader_registry.rs:112-127#LeaderRegistry::deregister_reporting_zero_transition`)
and asked through
`crates/runtime/src/ipc/leader_registry.rs:164-198#LeaderRegistry::gone_for_at_least`. The wiring is
one span, `crates/runtime/src/ipc/connection.rs:107-126` — `register` called at line 111 with the
`on_gone` closure opening on that same line, `sleep(grace)` at line 115, the
`gone_for_at_least` question at line 118, and `settle_leader_gone` at line 122. `[VERIFIED]`
Every one of those is unchanged by this decision.

**What the teardown does today has no interval in it.** `settle_leader_gone`
(`crates/runtime/src/service/orchestration.rs:1656-1751#Orchestrator::settle_leader_gone`) walks the
leader's non-terminal runs, journals a `WorkerTimeout { kind: LeaderGone }` fact through
`crates/runtime/src/domain/repository.rs:1790-1795#DomainRepository::record_worker_timeout_if_live`
(called at `crates/runtime/src/service/orchestration.rs:1716`), broadcasts it (line 1736), and then
calls
`cancel_and_settle_run(run_id, RunState::unrendered_verdict(), None)`
(`crates/runtime/src/service/orchestration.rs:1740`, the awaited call closing on 1741).
`[VERIFIED]`

Inside that helper — `crates/runtime/src/service/orchestration.rs:1570-1618#Orchestrator::cancel_and_settle_run` —
the order is: commit the transition (line 1581), broadcast it (line 1586), then
`driver.cancel_run(run_id, CancelScope::Worker)` (line 1590). `[VERIFIED]` The kill is
**unconditional and immediate**. Between the journaled fact and the kill there is no step at which a
worker is told that it is about to be stopped, and no interval in which it could save state and exit.
The report-before-decide half already holds: ADR-0036 states the fact is journaled first "so the
journal always states the reason before the state changes because of it"
(`docs/adr/0036-leader-disconnect-grace-window.md:88-90`).

**The cost of that was already named, in a record this one amends.** ADR-0037 listed, among the
failures that were "crew's own model of what a worker is", "a teardown window that reaps a worker the
vendor may legitimately still be using"
(`docs/adr/0037-protocol-first-control-plane.md:26-27`), and separately observed that "both
first-party SDKs bound teardown themselves — one with the comment that a signal before the flush
loses the last assistant message", so that "a hard kill at the deadline can truncate the very
transcript decision 2 reconciles against"
(`docs/adr/0037-protocol-first-control-plane.md:203-205`). `[VERIFIED]`

**The substrate for an instruction exists and has never been used.** `MessageKind::Shutdown` is
declared at `crates/protocol/src/message.rs:61#MessageKind::Shutdown` and is mapped to live adapter
delivery by `crates/runtime/src/adapter/registry.rs:783-825#AdapterRegistry::send_follow_up`, where
it takes the same `AdapterMessage::FollowUp` arm as assign, follow-up, question,
approval-decision and cancel (that arm is line 820 of the cited range). `[VERIFIED]`

**Nothing in `crates/` ever constructs one.** Every occurrence of `MessageKind::Shutdown` in this
repository, and what each one is:

| Site | What it is |
|---|---|
| `crates/protocol/src/message.rs:61#MessageKind::Shutdown` | the declaration |
| `crates/runtime/src/ipc/connection.rs:831-850#parse_message_kind_field` (the arm is line 847) | a **parser** — string to kind |
| `crates/runtime/src/service/orchestration.rs:3714-3732#parse_message_kind` (the arm is line 3727) | a **parser** — string to kind |
| `crates/runtime/src/domain/repository.rs:3183-3195#message_kind_str` (the arm is line 3194) | a **projection** — kind to stored string |
| `crates/runtime/src/adapter/registry.rs:783-825#AdapterRegistry::send_follow_up` (the arm is line 820) | a **match arm** — kind to adapter message |
| `crates/protocol/tests/coordination_contract.rs:93-110#coordination_send_params_accept_every_message_kind` (the item is line 103) | a test enumerating every kind the schema accepts |

`[VERIFIED]`, by reading every occurrence. There is no seventh site and no construction: the kind is
accepted on the way in, stored on the way through, and mapped on the way to a live adapter, and
nothing anywhere asks to send one. **The wire format, the parsers, the projection and the delivery
mapping are all already in place; only a sender is missing.**

## Decision Drivers

* The owner's ruling: when the driver dies, agents stop and save.
* **The trigger is the driver's death, not the agent's silence.** This is ADR-0036's central ruling
  and it is not reopened: silence alone is never evidence a leader gave up. The worker's own state
  bears on what the instruction *means*; it never becomes the reason to tear down.
* A worker that is mid-turn when it is signalled may lose the work it had already done. ADR-0037
  recorded both vendors' SDKs bounding teardown for that reason, and that a hard kill "can truncate
  the very transcript decision 2 reconciles against".
* Reuse what exists. ADR-0036's stated reason for reusing `run_cancel`'s settle path was that "a
  second, parallel implementation is a second place they could quietly diverge"
  (`docs/adr/0036-leader-disconnect-grace-window.md:31-33`); the same reasoning applies to the
  instruction, which already has a type, parsers, a storage projection and a delivery mapping.
* The concurrency-slot bound and the graceful-stop bound are different quantities and must not be
  sized by each other. `[AGENT-PROPOSED]`

## Considered Options

* **Instruct, wait for the agent to report itself finished, then kill — bounded by a backstop timeout whose size is not set.** Chosen — see below.
* **How to arrive at the backstop's size, as a separate question put on 2026-09-29.** Five options
  were offered and the owner chose **(c) then (e)**. Each is recorded so a reader can see what was on
  the table, and the chosen pair is recorded twice — once here, in full under "The backstop's
  sizing, ruled on 2026-09-29 and ordered" in Consequences.
  - **(a) A short fixed number.** Settles fast, but an agent mid-save is killed while saving, and
    the cost is **N × that number** for a multi-run driver. Not chosen.
  - **(b) A long fixed number.** Every agent gets room, but the cost is **N × that number** in slot
    holds and live processes. Not chosen.
  - **(c) Parallelise the loop first, then pick a number. CHOSEN, and chosen first.** The settle
    loop is sequential and shares nothing, so making it concurrent makes every option N times
    cheaper. `[USER-STATED]` — *"I agree c and then e"*.
  - **(d) Silence-based rather than absolute.** The backstop extends while the agent still emits
    output and fires only when it goes quiet. **Offered and NOT chosen; recorded so a future reader
    knows it was not overlooked. It remains available and nothing in this record forecloses it.**
  - **(e) Short backstop plus the manual escape. CHOSEN, and chosen second, after (c).** Set it
    short, and document `task/get` → `reconcile/omp` → `run/cancel` as the way to grant an agent
    more time when someone is watching.
  - **The order is the ruling.** (c) precedes (e), and **the value remains unset until (c) lands**,
    because the loop is serial today and multiplies any value by the number of non-terminal runs
    the lost leader owned. `[VERIFIED]` at `2c812cb`.
* **Kill with no instruction, which is what ADR-0036 chose.** Rejected; see below.
* **A second settle path, parallel to `run_cancel`.** Rejected; see below.
* **Reuse the 60s window for the graceful stop.** Rejected; see below.
* **Instruct and never kill.** Rejected; see below.
* **Wait on worker silence instead of leader-connection-gone.** Rejected; see below.

## Decision Outcome

Chosen option, subject to the owner: **instruct the run, wait for it to report itself finished,
then settle it through the path ADR-0036 already chose.**

Concretely, four things:

1. **Detection is reused unchanged, and a second window does not disturb it.**
   `gone_for_at_least` already takes its grace as a **parameter**
   (`crates/runtime/src/ipc/leader_registry.rs:164#LeaderRegistry::gone_for_at_least`; the parameter
   is in the signature, the comparison is at line 174). `[VERIFIED]` A second window is therefore a
   second *call* with a different argument, not a second mechanism, and the refcount, the
   `zero_since` instant, the drop guard, the restart seeding and the per-run re-check are all
   untouched. `LEADER_DISCONNECT_GRACE_WINDOW` is 60 seconds and stays 60 seconds
   (`crates/runtime/src/ipc/leader_registry.rs:41#LEADER_DISCONNECT_GRACE_WINDOW`). `[VERIFIED]`

2. **The instruction goes in the gap the owner's ruling names**, which is between the journaled
   `LeaderGone` fact and the settle call. Today the sequence inside the per-run loop of
   `crates/runtime/src/service/orchestration.rs:1656-1751#Orchestrator::settle_leader_gone` is:
   re-check (line 1703), journal (line 1716), broadcast (line 1736), settle (line 1740). The
  instruction is delivered after the journaled fact and before the settle, and the graceful stop's
  wait is made there.

3. **The instruction rides the existing substrate.** A `MessageKind::Shutdown` is delivered to the
   run's live adapter through the same path every other in-run message takes, and the wait is made
   before `cancel_and_settle_run` is called — so the report-before-decide ordering ADR-0036
   established is preserved exactly, with one step inserted into it. `[AGENT-DECIDED]` **The owner
   ruled the shape — instruct, then kill on timeout — and did not rule the mechanism.** Where the
   instruction is delivered from, in what order relative to the broadcast, and what it says are
   this agent's choices and are reversible.

4. **The wait is state-driven, not timed, and a timeout is retained only as a backstop.**
   `[USER-STATED]` 2026-09-29, a ruling later the same day than the shape recorded above and the
   answer to question 2. Crew waits until the agent **reports itself idle and not waiting**, on
   the owner's own finished-state definition, rather than for a fixed interval; the backstop exists
   so that a wedged or unresponsive agent cannot hang the settle forever, and **its size is not
   decided here.** See question 2 under "The two questions, and where each now stands" below.

`[AGENT-PROPOSED]` in points 2 and 3, `[AGENT-DECIDED]` as marked in point 3. The owner has ruled
points 1 and 4's *existence* and neither of their *contents*, and has since ruled the *shape* of
point 4 — state-driven, not timed — **without ruling the size of the backstop that bounds it.**

### What the repository already has that bears on this, and what it does not

`[VERIFIED]` One precedent for the shape exists inside the runtime already.
`crates/runtime/src/adapter/claude_protocol/adapter.rs:370#SELF_EXIT_GRACE` is five seconds, and its
own doc says it is "how long a completed turn's own claude process gets to exit on its own before
`settle_after_turn` starts signaling it", existing "so that reasoning is never load-bearing". That is
the same shape — give the process a window to leave under its own power, then signal — on a
**different trigger** (a completed turn) and a **different scope** (one adapter's own child, not a
run whose leader has gone). It is cited as precedent for the shape, not as a source for the number.

`[VERIFIED]` The owner also stated what crew can observe: idle, waiting an answer, or still working.
The repository has machinery for the first of those on the TUI path only:
`crates/runtime/src/adapter/tui/adapter.rs:430-435#ENTER_IDLE_MIN` is ten seconds of PTY
output-silence, argued there to mean "exactly 'idle TUI holding our text'". **That is an inference
from worker silence, and ADR-0036 is the record that rejected silence as evidence about a *leader*.**
It is not revived here, and the distinction is the whole reason this record amends ADR-0036's
teardown rather than its detection: under this decision the leader-connection evidence is what
starts the teardown, and the worker's own state only says what the instruction means.

`[VERIFIED]` **The number the graceful stop needs is not measured anywhere in this repository.** The
sixty seconds bounds a different quantity — the constant's own doc says it is sized so "an
abandoned run does not tie up a concurrency slot indefinitely"
(`crates/runtime/src/ipc/leader_registry.rs:30-41#LEADER_DISCONNECT_GRACE_WINDOW`). Nothing in
`crates/` times "notice an instruction, save, and exit". The only two windows anywhere near it are
`SELF_EXIT_GRACE` above and `crates/runtime/src/supervisor/process.rs:92#SETTLE_GRACE`, a one-second
settle bound in the supervisor; neither measures the quantity this decision needs.

## Rejected alternatives, with reasons

* **Kill with no instruction — the status quo ADR-0036 chose. Rejected.**
  `[USER-STATED]` the ruling: "driver dies, agents stop and save". The status quo does the second
  clause and skips the first. It is also the failure ADR-0037 already named: a hard kill at the
  deadline can truncate the very transcript the audit reconciles against
  (`docs/adr/0037-protocol-first-control-plane.md:203-205`). `[VERIFIED]` This is the one option
  being replaced, and only in this respect; ADR-0036's detection is carried forward whole.

* **A second settle path, parallel to `run_cancel`. Rejected.** `[VERIFIED]` against ADR-0036's own
  stated reason: "Reuse `run_cancel`'s existing settle mechanism rather than invent a second one:
  the ordering and broadcast invariants ADR-0020 requires were already solved once, and a second,
  parallel implementation is a second place they could quietly diverge"
  (`docs/adr/0036-leader-disconnect-grace-window.md:31-33`). A graceful stop that settled through
  its own path would be exactly that second place. The chosen option inserts a step *into* the
  existing path (`cancel_and_settle_run`,
  `crates/runtime/src/service/orchestration.rs:1570-1618#Orchestrator::cancel_and_settle_run`) and
  does not re-implement any part of it.

* **Reuse the 60s window. Rejected.** `[AGENT-PROPOSED]`, on the owner's answer to the question
  itself: the graceful stop carries **its own** window. Independently, the two windows size different
  quantities — the sixty seconds bounds a concurrency slot
  (`crates/runtime/src/ipc/leader_registry.rs:30-41#LEADER_DISCONNECT_GRACE_WINDOW`) and the new one
  would bound an agent's time to notice, save and exit, which is unmeasured
  (see question 2 under "The two questions, and where each now stands" below). Reusing the number
  would have been a claim that the two quantities are equal, and nothing in the repository says
  they are. `[VERIFIED]`

* **Instruct and never kill. Rejected.** `[AGENT-PROPOSED]`. It is the one alternative that does not
  satisfy the ruling as given — the owner said *instruct-then-kill-on-timeout*, and "agents stop and
  save" is a statement about the *agent's* behaviour, not permission for the daemon to leave a
  vendor process and a concurrency slot live indefinitely. It would also reintroduce, for a second
  reason, the stranding ADR-0036 exists to end: a run whose worker ignored the instruction would
  never settle, and the leader that would have cancelled it by hand is by definition gone.

* **Wait on worker silence rather than leader-connection-gone. Rejected.** `[VERIFIED]` This is
  precisely the decision ADR-0036 reversed, and its own words: it "conflated two genuinely
  different situations under one signal"
  (`docs/adr/0036-leader-disconnect-grace-window.md:11-12`), and that "silence alone is never
  evidence a leader gave up" (lines 17-18 of the same file). The trigger here is
  unchanged — a leader's connection actually gone for its window. The owner's remark about what crew
  can observe bears on what the instruction means to the agent, not on what may start the teardown,
  and reading it as a new trigger would make this record a supersession of 0036's central ruling
  rather than an amendment of its teardown, which is not what the ruling asked for.

## Consequences

### Positive Consequences

* A worker that is mid-turn when its leader dies is told to stop and save before it is signalled,
  which is the interval ADR-0037 recorded both first-party SDKs providing for themselves and which
  crew did not. `[VERIFIED]` as a claim about the SDKs; the crew-side interval is what this record
  adds.
* A worker that has already finished leaves under its own power, and the transcript it already wrote
  survives a leader that disconnected during the turn.
* Detection is untouched, and the change is additive: one inserted step between two lines that
  already exist (`crates/runtime/src/service/orchestration.rs:1736` and line 1740).
* The settle still goes through the one path ADR-0036 chose, so the ordering and broadcast
  invariants ADR-0020 requires are not re-solved here.
* The instruction needs no new wire type, no new parser and no new delivery path; the sender is the
  only missing piece.
* The per-run re-check that protects a leader reconnecting mid-loop (lines 1703-1710 of
  `crates/runtime/src/service/orchestration.rs:1656-1751#Orchestrator::settle_leader_gone`) is
  re-evaluated naturally if it is re-read at the settle rather than only before the instruction.
  `[AGENT-PROPOSED]`

### Negative Consequences

* **A leader that is genuinely gone now takes longer to be cleaned up.** The 60s plus a wait whose
  length is the agent's to decide is the floor for a slot's release, and a leader with several runs
  pays the graceful stop per run unless the implementation shares one. ADR-0036's own negative
  consequence about holding a slot for a minute is made longer by an amount that is **now bounded
  only by a backstop nobody has sized yet.** `[VERIFIED]` as a consequence of the shape; the
  backstop's size is the open item below.
* **`MessageKind::Shutdown` is currently a label on a text follow-up.** The delivery mapping in
  `crates/runtime/src/adapter/registry.rs:783-825#AdapterRegistry::send_follow_up` sends it as
  `AdapterMessage::FollowUp { text }`, so what the instruction actually says — and whether a worker
  distinguishes it from an ordinary prompt — is not settled by the type alone. `[VERIFIED]` A
  worker that treats it as another prompt has been *given more time* and then killed, which is the
  current behaviour with extra latency, not a new failure.
* **A worker that exits on its own during the wait leaves the run's settlement to evidence.** The
  kill once the agent reports itself finished — or once the backstop expires — is written as
  unconditional, and this record does not change that;
  if a run is already terminal when the settle is reached,
  `transition_run`'s own guarded write refuses the illegal edge and the run is skipped
  (lines 1651-1655 of
  `crates/runtime/src/service/orchestration.rs:1656-1751#Orchestrator::settle_leader_gone`, which
  documents the branch). That is correct and already the behaviour; it is listed
  because a wait makes "already terminal" much more likely than it was, and the branch is now
  load-bearing in a way it was not.
* **The detection semantics of ADR-0036 get harder to state in one sentence.** "Settled after the
  leader is gone for 60 seconds" becomes "settled after the leader is gone for 60 seconds, plus a
  wait in which it was told to stop" — **a wait whose end the agent decides, and one number that
  is unset.**
* **A paused run's treatment is now load-bearing** in a way it was not, because "finished" has to be
  given a concrete meaning. **That meaning is settled** by the owner's ruling of 2026-09-29 — a
  `paused` agent *is* an idle agent, and is told to stop — so the question is closed and the mapping
  that follows from it is stated under question 1. **The record is still not implementable as
  written, and the reason is now one named item alone:** the size of the backstop timeout, which
  exists by the owner's ruling and has never been set.
* Nothing here makes the concurrency-slot bound tighter. If a leader leaves with many live runs,
  every one of them now pays the graceful stop.
* **The settle loop is serial, so every backstop this record contemplates is paid N times over.**
  `settle_leader_gone` (`crates/runtime/src/service/orchestration.rs:1656-1751#Orchestrator::settle_leader_gone`)
  iterates the run ids returned by `owned_nonterminal_run_ids_op` in a plain `for` loop at line
  1686, awaiting each run's own journal write (`:1712-1734`), its own broadcast (`:1736`) and its
  own settle (`:1739-1741`) before moving to the next. **There is no `join_all`, no
  `FuturesUnordered` and no per-run spawn anywhere in the function.** `[VERIFIED]` at `2c812cb` by
  reading the whole range and searching the file. **A leader owning N non-terminal runs pays the
  backstop N times, in sequence** — which is why the backstop's value cannot be chosen until the
  loop is made concurrent. **The owner's ruling of 2026-09-29 orders exactly that, and it is
  recorded under "The backstop's sizing, ruled on 2026-09-29 and ordered" below.**

### The backstop's sizing, ruled on 2026-09-29 and ordered

`[USER-STATED]` 2026-09-29, verbatim, answering a question about this record's unset backstop:

> "I agree c and then e"

**The ruling is ordered, and the order is the whole of it.** Offered five ways to size the backstop,
the owner chose **(c) then (e)**: **make the settle loop concurrent first, and only then pick a
number; and set that number short, with the manual escape documented as the way to grant a wedged
agent more time while someone is watching.** **A reader must not collapse the two halves.** (c) is
work to be done before a number means anything; (e) is the shape of the number once (c) has landed.

**What (c) was, as it was put to the owner.** Parallelise the settle loop before choosing a value —
*"The settle loop is sequential and shares nothing; make it concurrent and every option becomes N
times cheaper."* **What (e) was.** A short backstop plus the manual escape — *"set it short, and
document `task/get` → `reconcile/omp` → `run/cancel` as the way to grant an agent more time when
someone is watching."*

**The consequence this record carries forward, and it is a new negative one.** **The backstop's
value remains unset, and it cannot sensibly be set until (c) lands** — because the loop is serial
today, so any value is multiplied by the number of non-terminal runs the lost leader owned, and a
value chosen against a serial loop is a value chosen against a loop that will not be the one in
production. **This is a stronger statement than "the owner has not chosen a number":** it is that
**choosing one now would be choosing it against the wrong loop.** Item 3 below is updated
accordingly, and it stays OPEN and UNSET.

**Option (d) was offered and not chosen, and is recorded so a future reader knows it was not
overlooked.** The silence-based variant — the backstop extends while the agent still emits output
and fires only when it goes quiet — was one of the five options put to the owner. **It is available
and was not selected**, in favour of (c) then (e). **Nothing in this record forecloses it**, and no
argument against it is made here; the fact that it was on the table and not taken is the fact
worth keeping.

**What (e) adds to this record, in the record's own terms.** The sanctioned way to give a wedged
agent more time is **not** a longer default, and this record should name it as such. The two-call
path exists today and is documented as intended rather than as a workaround:
`reconcile_omp` (`crates/runtime/src/service/orchestration.rs:3504#OrchestrationService::reconcile_omp`),
whose ownership rebind is revision-guarded in
`crates/runtime/src/domain/repository.rs:2715#DomainRepository::reconcile_ownership` (the `WHERE
task_id = ?4 AND revision = ?2` predicate is at `:2748-2750`), taking a task id and a revision that
`task/get` supplies (`crates/runtime/src/service/query.rs:11#task_get_op`); then `run_cancel`
(`crates/runtime/src/service/orchestration.rs:1542#OrchestrationService::run_cancel`), which is
owner-gated by the `if let Some(principal_instance_id)` block at
`crates/runtime/src/domain/repository.rs:895-919` inside `transition_run`. `[VERIFIED]` at
`2c812cb`. **This is how work is reclaimed from a disconnected client**, and (e) makes it also the
documented way to extend a single wedged agent's life. **Lengthening the default is not the
sanctioned answer to a wedged agent, and this record should not be read as proposing one.**

**And a fact that makes the ordering load-bearing rather than tidy.** **The settle path never
releases the run's workspace lease, today and under this record alike.** `cancel_and_settle_run`
(`crates/runtime/src/service/orchestration.rs:1570-1618#Orchestrator::cancel_and_settle_run`) performs
three operations — the domain transition (`:1577-1585`), the broadcast (`:1586`) and
`driver.cancel_run` (`:1589-1590`) — and its own doc comment at `:1566-1569` says lease and slot
release is not its job and happens downstream, evidence-driven, from the adapter's own exit event.
**The function contains no lease call of any kind.** `[VERIFIED]` at `2c812cb` by reading the whole
range. **A concurrency slot, by contrast, is released by that adapter event** —
`crates/runtime/src/adapter/event_sink.rs:852-853` frees on `ProcessExited` or `TurnEnded`
(`is_exit` on 852, `frees_slot` on 853), and the kill produces it — so **a longer backstop holds a
slot, a live vendor process and a PTY for proportionally longer, per run.** A run left non-terminal
by a long backstop is, on every observable this record has, **indistinguishable from a healthy
working run**: a live row state, a live lease, a held slot, a live PTY. And **when the backstop
expires the journal is byte-identical to today's immediate kill** — `WorkerTimeout { kind:
LeaderGone }` (`crates/protocol/src/event.rs:354#TimeoutKind::LeaderGone`, recorded at
`crates/runtime/src/domain/repository.rs:1823-1829` from
`crates/runtime/src/service/orchestration.rs:1716`) and then `RunCancelled`. **There is no field
anywhere that distinguishes an agent that ignored the instruction from one that saved and exited.**
**A backstop long enough to be generous is therefore indistinguishable, to every consumer, from a
backstop that failed to work** — which is the cost (c) is paid to reduce and the reason (e) is
recorded as the sanctioned alternative.

**Provenance note on this subsection.** Everything marked `[VERIFIED]` in it was opened and read in
this repository at `2c812cb`, not at the `2962c80` named in "How to read the labels" above; the
amendment's own verification is at the current anchor and the labels above are unchanged. **Nothing
in this subsection is implemented.** `[VERIFIED]`

**A reader who has reached the end of this subsection should hold five separate facts, and only
one of them is a decision about a number.** The *shape* of the wait is decided. The *direction* of
the backstop is decided — short. The *order* is decided — (c) before (e), and no value at all
until (c) lands. The *manual escape* is now the sanctioned way to grant a wedged agent more time,
in place of a longer default. And the *settle loop is still serial*, which is the fifth fact and
the one the whole ordering turns on. **None of the four decisions supplies the value, and this
record still does not contain one.**

## The two questions, and where each now stands

**Both are the owner's, and both are now answered — by two separate owner rulings on 2026-09-29,
with a third ruling of the same date bearing on the consequence of the second.** They are kept under
their original numbers so that no reference to either breaks. **Neither closure closes everything it
sits over, and the statuses below must not be read as one.** Question 1 is answered in the owner's
own words, quoted below. Question 2's answer is a *shape*, and it left behind one named item that is
**not answered**: the size of the backstop. **The third ruling does not answer that item either — it
orders the work that must precede answering it**, and it is recorded in full under "The backstop's
sizing, ruled on 2026-09-29 and ordered" in Consequences, with its effect on item 3 below. **A reader
who takes the closure of question 1, of question 2's shape, or of the third ruling as the closure of
the backstop's value is misreading this record**, and an implementation that needs that value
before the owner has set it has a blocker, not a default.

### 1. Which `RunState` values count as "finished" — **CLOSED, answered by the owner 2026-09-29**

The owner defined finished as an *observability* judgement: "What crew can observe is whether the
agent is idle, waiting an answer or still working. If it is idle, not waiting nor working, then the
agent is considered that all his work is finished." `[USER-STATED]`

**The ruling that closes this question.** `[USER-STATED]` 2026-09-29, verbatim:

> "a paused agent is an idle agent. There is not other state for paused. So the answer is told to
> stop."

**This is settled by definition, and not by choosing between two readings.** The only reading
available to `paused` besides "idle" was that it is a run frozen precisely so that a decision can
still be made about it — waiting, not idle — and the owner has foreclosed it in the same breath:
"There is not other state for paused." A paused agent is an idle agent; it is told to stop, and it
saves. **There is nothing left here to choose, and this record states no preference.**

**The mapping.** The set of run states this decision treats as finished is
**{`queued`, `starting`, `paused`}** — three members. All ten valid `RunState` values are
accounted for below, and **the exclusions are of two kinds, which a reader is entitled to be able
to tell apart.**

| Value | Finished? | Basis |
|---|---|---|
| `queued` | **yes** | none of the owner's three: not `working`, not waiting an answer, not terminal |
| `starting` | **yes** | as `queued` |
| `paused` | **yes** | as `queued`, and named outright by the ruling above — a paused agent *is* an idle agent, and it is told to stop |
| `working` | no | "still working" — the one of the owner's own three the definition excludes by name |
| `waitingUser` | no | "waiting an answer" |
| `waitingPeer` | no | "waiting an answer" |
| `succeeded` | **excluded mechanically** | never reaches this code — see the next paragraph |
| `failed` | **excluded mechanically** | never reaches this code — see the next paragraph |
| `cancelled` | **excluded mechanically** | never reaches this code — see the next paragraph |
| `lost` | **excluded mechanically** | never reaches this code — see the next paragraph |

**What the three members share, because it is the property and not the values that matter.** None
of them is `working`; none is waiting on a person or a peer; none is terminal. **That is the whole
test the owner stated, and all three values satisfy it.** Each is declared among the ten validated
states at `crates/protocol/src/run.rs:102-103#TryFrom<&str> for RunState` (the `impl` block is
`crates/protocol/src/run.rs:97-107`) `[VERIFIED]`.

**Which exclusions were chosen and which were inevitable.** Three of the seven are judgements:
`working`, `waitingUser` and `waitingPeer` are outside the set because the owner's own three-way
test names each of them, and a different ruling could move any of them. **The other four are not
judgements at all.**
`succeeded`, `failed`, `cancelled` and `lost` are terminal, and **this code path never sees them**:
the leader's run query filters terminal states in SQL, at
`crates/runtime/src/service/query.rs:413#TERMINAL_STATES_SQL` — the constant, whose own value is
those four literal state names — used by
`crates/runtime/src/service/query.rs:422-442#owned_nonterminal_run_ids_op` (the function; the
`NOT IN` filter is line 430 of that range) `[VERIFIED]`. The same four are the ones
`crates/protocol/src/run.rs:30-35#RunState::is_terminal` names, the four literals being lines
32-33 `[VERIFIED]`. **No judgement was exercised about them and none can be** — a run that is
already terminal has nothing left to instruct. A reader asking whether this record decided to
exclude them is asking a question with no answer behind it: they are not candidates.

**What the ruling changed, stated so the change is visible rather than silent.** Before it, this
record carried two readings of the set, differing on `paused` alone: *literal* gave
`{queued, starting, paused}`, and *recovery-gated* gave `{queued, starting}`. **The ruling settles
`paused` in favour of the literal reading, and the set it produces is the same set that reading
gave before the ruling existed.** What the ruling removes is therefore not a member of the set but
the choice itself — this record no longer carries a `paused` arm that an implementer might build
the other way.

**The `paused -> working` edge is unchanged and is not a counter-argument.**
`crates/protocol/src/run.rs:89`, inside
`crates/protocol/src/run.rs:66-94#RunState::can_transition_to`, still carries an outgoing edge from
`paused` to `working`, and `is_terminal` still excludes `paused`
(`crates/protocol/src/run.rs:30-35#RunState::is_terminal`) `[VERIFIED]`. **A paused run is by
construction resumable, and the owner has said that is not a reason to withhold the instruction.**
That edge governs what a run may become; this decision governs what an agent is told while it is
paused. The owner's ruling is about the second, and it is the later word on the subject.

### 2. The size of the graceful stop's window — **CLOSED as to the shape, 2026-09-29; the backstop's size is unset and is now gated on a named prerequisite, see item 3**

**This is a separate owner ruling from the one recorded under question 1, and a reader must be able
to tell which ruling set what.** Question 1 settled which `RunState` values count as finished.
**This one settles the shape of the wait, and no part of its length.**

**The ruling.** `[USER-STATED]` 2026-09-29, later the same day. Offered the choice of a fixed
interval against waiting on the agent's own report, the owner chose the second, and the terms the
ruling turns on are these — **the substance of the ruling as recorded, not a verbatim transcript
of the exchange, and none is invented here:**

* the graceful stop is **not timed**;
* Crew waits until the agent **reports itself idle and not waiting**, on the owner's own
  finished-state definition;
* a timeout is retained **only as a backstop**, so that a wedged or unresponsive agent cannot hang
  the settle forever;
* **the owner was offered a number for that backstop and declined it**, in favour of the
  state-driven wait.

**The wait is therefore not timed, and "carries its own window" now names its own backstop.** The
earlier answer the same day — that the graceful stop carries its own window rather than reusing
`LEADER_DISCONNECT_GRACE_WINDOW` — still stands, and the thing it names is that backstop, which is
distinct from the constant and has never been set equal to it.

**Why the existing 60 seconds is not available to the backstop.** `[VERIFIED]` That constant is
sized to bound a **concurrency-slot hold** — its own doc says it exists so that "a slot is held for
at most a minute after a leader genuinely leaves, which is short enough that an abandoned run does
not tie up a concurrency slot indefinitely, and long enough to cover a quick OMP restart or
reconnect"
(`crates/runtime/src/ipc/leader_registry.rs:30-41#LEADER_DISCONNECT_GRACE_WINDOW`, the declaration
is line 41 of that range). **A worker's time to notice an instruction, save, and exit is a
different quantity, and nothing in this repository measures it.** The closest existing numbers,
`SELF_EXIT_GRACE` at
`crates/runtime/src/adapter/claude_protocol/adapter.rs:370#SELF_EXIT_GRACE` and `SETTLE_GRACE` at
`crates/runtime/src/supervisor/process.rs:92#SETTLE_GRACE`, bound different triggers and are cited
for the shape only, never as a source for a number. Reusing sixty seconds would assert that the two
quantities are equal, and nothing in the repository says they are.

**What this closes and what it does not are two different things, and this record must not let
them run together.** The *shape* is closed: state-driven, untimed, ending on the agent's own
report. The *backstop* is a separate quantity, it exists by the owner's ruling, and **its size is
not set** — carried as item 3 below.

### 3. The backstop's size — **OPEN, DELIBERATELY UNSET, AND NOW GATED ON A NAMED PREREQUISITE**

**The status line above changed on 2026-09-29 and the change is the substance of the ruling, not a
housekeeping edit.** The item was OPEN because the owner had been offered a number and declined it.
**It is now OPEN because the owner has ruled that the number cannot be chosen yet** — the settle
loop must be made concurrent first. `[USER-STATED]` 2026-09-29, verbatim: *"I agree c and then e"*.
**These are different states and a reader entitled to tell them apart will find different work in
them:** the first said the owner had no number in mind, the second says the owner has decided what
kind of number to want and has decided the codebase is not yet in the state where one can be
picked. The full ruling is under "The backstop's sizing, ruled on 2026-09-29 and ordered" in
Consequences above.

**The prerequisite, named so it can be checked rather than assumed.** **(c) is to make the settle
loop concurrent**, and it is a prerequisite to the value, not merely adjacent to it. The loop is
serial today (`crates/runtime/src/service/orchestration.rs:1656-1751#Orchestrator::settle_leader_gone`,
the plain `for` at line 1686, with no `join_all`, no `FuturesUnordered` and no per-run spawn), and
**a backstop is paid once per non-terminal run the lost leader owned.** Choosing a value against
that loop would be choosing it against a loop that is not the one that will run in production.
`[VERIFIED]` at `2c812cb`.

**What is decided about the value, so the "deliberately unset" is not read as "unexamined".** The
owner has ruled that the number is to be **short** — (e), *"set it short, and document
`task/get` → `reconcile/omp` → `run/cancel` as the way to grant an agent more time when someone is
watching."* **So the direction is decided and the magnitude is not**: short is a property the
value will have, and it is not itself a value. **The sanctioned response to a wedged agent is the
documented manual escape, not a longer default**, and that is now part of this record rather than
only a practice.

**Option (d), the silence-based variant, was offered and not chosen.** It remains available and
nothing here forecloses it. **It is recorded because a future reader must be able to see that it
was considered.**

**Stated as two separate facts, because they are two separate states and a reader must be able to
tell them apart.**

* **That a backstop exists — CLOSED.** `[USER-STATED]` 2026-09-29, recorded in item 2 above: a
  timeout is retained **only as a backstop**, so that a wedged or unresponsive agent cannot hang
  the settle indefinitely. This much is the owner's ruling and it is not open.
* **How long that backstop is — OPEN, unset, and now gated.** The owner **was offered a number and
  declined it**, in favour of the state-driven wait, and has since ruled that no number is to be
  chosen until the settle loop is concurrent. **Declining a number is not a decision that the value
  is anything in particular**, and it is not permission to pick one. **This record proposes no
  number**; an implementation that needs one before the prerequisite above has landed has a
  blocker, not a default, and a plausible-looking value supplied by an implementer would be a
  number no owner ever chose against a loop that does not yet exist.

**What would settle it, in the new order.** Two things, and the first now gates the second:
**(1) the settle loop is made concurrent** — the work the owner's ruling puts first, and the
prerequisite for any number meaning what it says; **then (2) the owner's view of how long a wedged
or unresponsive agent is owed before the kill**, or a measurement of how long a graceful exit
takes, which the repository does not hold and which would have to be taken before any number could
be defended rather than guessed. The 60-second constant is **not** an available answer; item 2
above is why it bounds a different quantity. **Until (1) lands, writing a number into this record
would be a number chosen against the wrong loop**, and an implementation that needs the value
before (1) has landed has a blocker, not a default.

## What would reverse this

Stated so it can be wrong in a detectable way. This record would be wrong if the graceful stop
inverted the ruling — if the instruction turned out to be what a worker needed in order to *save*,
and the run's real work was still lost when it exited. The finding that would falsify it is a run
whose worker, having been given the wait to stop and save, produced no durable artefact that
the un-instructed kill would not have produced either. Second, and independently: if a run that
exits cleanly during the graceful wait is recorded in a way that loses the result — the
`unrendered_verdict` settlement
(`crates/protocol/src/run.rs:59#RunState::unrendered_verdict`) is deliberately not `succeeded`, and
only the leader's `run/finish` may say that (ADR-0027) — then the worker saved, the run was settled,
and the verdict is still lost, which is a different defect from this one and is not fixed here.

## Links

* Amends [0036](0036-leader-disconnect-grace-window.md), and only its teardown. Its detection — the
  refcount, the `zero_since` instant, the 60-second window, the restart seeding and the per-run
  re-check — is carried forward unamended, which is why the header carries no `Supersedes:` line.
  [0036](0036-leader-disconnect-grace-window.md) is **not edited**; the amendment lives here, per the
  write-once rule in [README.md](README.md:3-6).
* Related: [0037](0037-protocol-first-control-plane.md) (the teardown-bound observation this record
  acts on, and the run whose decision 2 reconciles the transcript a hard kill truncates),
  [0027](0027-turn-end-settles-a-run.md) (`unrendered_verdict` and the leader's exclusive right to
  render a verdict), [0020](0020-per-mutation-event-broadcast-is-not-optional.md) (the
  broadcast-in-the-same-call invariant this path inherits from `cancel_and_settle_run`),
  [0038](0038-one-turn-worker-lifecycle.md) (the most recent record that also amends 0036 without
  superseding it, and the shape of the header this one follows).
* Numbering: this record takes `0039`. There is no ADR-0033, reserved and deliberately left unused
  because its decision was already recorded in
  [0027](0027-turn-end-settles-a-run.md) ([README.md](README.md:13-17)); that gap is left alone.
* Implementation, as it stands today: `crates/runtime/src/ipc/leader_registry.rs` (the registry, the
  constant, and the parameterised `gone_for_at_least` this decision reuses),
  `crates/runtime/src/ipc/connection.rs` (the wiring), and
  `crates/runtime/src/service/orchestration.rs` (`settle_leader_gone`, `cancel_and_settle_run`).
* **This record has no implementation yet.** The three paths above are cited for what they do today,
  not for what they will do.
