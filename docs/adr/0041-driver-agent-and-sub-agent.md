# `driver`, `agent`, and `sub-agent`: Crew knows only the agent it spawned, and the third term is a domain boundary rather than a permission level

* Status: Proposed
* Date: 2026-09-30
* Supersedes: *(none — nothing here reverses a decision)*
* Amends: [0011](0011-omp-retains-task-graph-authority.md) (the *subject* of its authority line only; its substance is untouched — see "What this does and does not change about ADR-0011" below)
* Numbering: `0041`. The owner's planning notes reserved this number for this subject — "The driving harness retains task-graph authority, whichever harness that is" — and **this record is that decision, so it carries the reserved number.** `docs/adr/` holds `0001`–`0032` and `0034`–`0038`; `0033` is reserved and deliberately unused (`docs/adr/README.md:13-17`); `0044` is deliberately skipped. **The reserved title asserted a subject this record originally left open** — the question is now **settled by the owner's ruling of 2026-09-29**, quoted and dated in "The authority subject is settled: the driver, not the driving harness" below, and the reserved wording is corrected here rather than inherited.

## The authority subject is settled: the driver, not the driving harness

**The reserved title of this number asserted the wrong subject, and the owner has now ruled on
it.** `[USER-STATED]` 2026-09-29, verbatim:

> "user is the handler of driver agent. driver is the steering wheel and user is the car driver."

**The subject of ADR-0011's authority line is the driver.** Not the driving harness. The distinction
is not a wording preference and this record does not treat it as one:

* **"The driving harness"** would make the authority a property of *software* — a product
  statement, in which any harness not currently driving is out of scope by construction, and in
  which the authority is asserted by the fact of the software rather than proved by the connection.
* **"The driver"** makes the authority a property of a *role* — a statement about whoever holds the
  driver's role in this session. **A harness is never the authority.** A harness is capable of
  hosting a driver; the session in it may hold the role, and a harness that is not hosting a driver
  holds nothing. The owner's own third term carries this: the user is the handler and the driver
  is the steering wheel — one arrangement described two ways, not a product that owns a capability.

**The consequence, stated so it cannot be read the other way.** Nothing in the protocol may
establish the driver by naming a product. `role: "ompExtension"` is a string a peer supplies, and
this repository has verified that supplying it is currently sufficient to receive the driver's
spawn-capable method table — the finding, with its citations, is in
[0050](0050-one-rule-two-arms-the-user-and-the-driver-may-start-agents.md) under "The defect the
gate must close: `OmpExtension` is self-assertable". **The driver must prove it is the driver.**
That proof is not built, and this record does not build it; `0050` is `Proposed` and records the
requirement.

**Exactly one driver, per team, at a time.** `[USER-STATED]` 2026-09-29: *"There is ONE driver, in
the crew team."* The rule's whole purpose is to prevent **an agent from taking over as driver
mid-team** — an agent is never the driver, and a sub-agent is outside Crew's domain entirely. It is
not a statement about what happens across sessions, and this record makes **no claim about
handover, transfer, or succession between drivers**: closing the driver **ends the team**, per
[0039](0039-instruct-then-kill-on-timeout.md) — *"driver dies, agents stop and save"* — where every
agent is instructed to stop, save its work and exit gracefully. A later driver begins a **new
team** and inherits nothing from the previous one: not its agents, not its tasks, not its run
state. `0039` remains `Proposed` and the one item it leaves open, the size of the backstop
timeout, is not resolved here.

> **⚠ THIS RECORD IS A DRAFT AND AWAITS THE OWNER'S RATIFICATION.** Nothing in it is a
> decision yet. It records rulings the owner has already given, the reasoning those rulings
> imply, and the questions they leave open — and it is written here so the owner can read all
> three at once. Ratifying it means resolving the named open question, changing this record's own
> `Status:` line to `Accepted`, and letting the index row follow. Until then this file confers
> nothing, and no other document may cite it as though it did.

## Context and Problem Statement

Crew is the machinery the user and the driver's session use, and it is not an agent. It knows the
first point of contact and nothing past it. The owner, 2026-09-29 `[USER-STATED]`, verbatim:

> "Since crew is the machinery, the machinery only needs to know the first point of contact. This
> means that Crew only needs to now that a task, has been assigned to an agent. How an agent handles
> this task, spawning sugagents, or handling it on its own, is a detail that does not need to be know
> to Crew. What Crew needs to know is that the agent is handling the task."

That sentence is a scope boundary, and it is the reason this record exists. It draws a line where
the previous vocabulary drew a *hierarchy*: it says there are things Crew does not know, not that
there are agents below agents. The owner's second ruling of the same day names the two mechanisms
that must be kept apart `[USER-STATED]`, verbatim:

> "You have to understand the difference between a subagent spawned by a harness and an agent
> spawned by Crew. We can define our internal naming so we separate these, if you like."

The second clause of the ruling that follows — "if I ask Codex to spawn a copilot agent, this is
not permitted" `[USER-STATED]` — is a statement about who may cause a new *agent* to exist. The
clause after it — "But if Codex, needs a subagent, he can spawn as many subagents he wants"
`[USER-STATED]` — is a statement about what Crew does not observe. Those are different kinds of
claim, and collapsing them into one permission ladder is the specific confusion this record removes.

**What the repository's own names currently say, and why they are the problem.** The vocabulary
this record settles is already partly present, and where it is present it means something else.
`Worker.parent_worker_id`
(`crates/protocol/src/worker.rs:44-56#Worker`, the field declared at line 52 of that declaration)
is a nullable self-reference whose own doc says only "The parent worker ID, if this worker was
spawned as a child" — a fact about the database, not about authority. `CoordinationRequestChildParams`
(`crates/protocol/src/coordination.rs:50-59#CoordinationRequestChildParams`) is documented as
"asks OMP to authorize a child worker" — a request for *an agent*, raised by a *worker*, and named
after the harness that will decide it. `ClientRole::OmpExtension`
(`crates/protocol/src/rpc.rs:38-42#ClientRole`, the variant declared at line 39) is the role whose
method table contains `WorkerCreate`, so it is the role that can cause a new agent to exist.

The same shape appears in prose, and the prose is where a reader meets it first. `crew_send`'s
description ("Send a correlated, journaled message to a peer worker or to OMP") is written by
`tool_specs` (`crates/runtime/src/coordination/mcp_protocol.rs:63#tool_specs`, the entry at line
210 of that declaration), and `crew_request_child`'s ("Ask OMP to authorize a child worker. Never
creates a task or worker itself", same declaration, line 209 of the name and line 210 of the
description). `decide_child`
(`crates/runtime/src/domain/repository.rs:2859-2889#decide_child`) is documented as recording
"OMP's decision on a prior child-worker request", and the same doc block records — at lines
2880-2882 of that declaration — that `coordination/child/decide` is `ompExtension`-only.

None of that is wrong today. It is named after OMP because OMP was the harness that drove Crew
when those names were written. The owner's own instruction is to stop locking on OMP
`[USER-STATED]`, 2026-09-28: *"I might direct from omp, or claude, or codex or any other of the
supported harnesses. Do not lock on OMP only."* A vocabulary that names the third party in the
authority chain as a sub-agent, and that names the user's own driving session as the *only*
authority, cannot survive the generalisation: once the driver may be any harness, "the sub-agent"
and "the agent" stop being distinguishable by who spawned them, and the record needs to say which
is which on its own terms.

## Decision Drivers

* **The owner's boundary, exactly as drawn.** Crew knows a task has been assigned to an agent. How
  that agent handles the task — alone, or by spawning helpers of its own — is a fact Crew neither
  requires nor observes. `[USER-STATED]`
* **A term that names an invisible thing must not look like a permission level.** If `sub-agent`
  reads as "an agent with fewer rights", every future reader will try to enforce a tier that does
  not exist, and will look for the gate that does not exist, and find none.
* **Two mechanisms, one word.** A harness spawning a helper and Crew spawning an agent are
  recorded by different systems, in different places, with different lifecycles. A single term for
  both makes at least one of them a lie.
* **The driving session is the user's own agent, not the user.** The owner defines it as the agent
  the user actually started `[USER-STATED]`: *"I start claude. Claude is my driver."* It is
  therefore something Crew may have to be driven by, and never something Crew speaks to.
* **The authority line must name a subject that can be every harness.** ADR-0011's line names OMP.
  The owner has instructed that OMP must not be the locked-in answer `[USER-STATED]`.
* **Write-once.** A vocabulary change is a change to what every later record may assume, which is
  the definition of hard to reverse.

## Considered Options

* **Keep one word, "agent", for everything Crew spawns, and say nothing about helpers.** Rejected.
  It satisfies the boundary by not mentioning it, and leaves the existing "child worker" and
  "sub-agent" prose to keep implying a tier.
* **Keep the two-tier permission model** — the driver controls agents, an agent controls its own
  sub-agents. **Rejected by the owner** `[USER-STATED]`: the second tier is withdrawn, because a
  sub-agent is not a thing Crew can see and therefore cannot be a thing Crew can police. This is the
  option this record replaces.
* **Treat `sub-agent` as a Crew entity** — Crew records harness-spawned children as workers.
  **Rejected.** It would make Crew know what the owner's ruling says it does not need to know, and
  it would put a fact Crew cannot observe into a table whose rows are all Crew-observed.
* **Name the three terms as `driver` / `agent` / `sub-agent`, with `sub-agent` declared a domain
  boundary.** Chosen — see below.
* **Drop `driver` as a term and keep only "the user".** Rejected: it would make the user's driving
  session indistinguishable from the user, and the repository's own roles cannot tell them apart
  on the wire (see "What this does not change").

## Decision Outcome

Chosen option: **three terms, each with one meaning, and the third meaning a boundary.**

**Decision 1 — `driver`.** The harness the user is conversing with — Claude Code, Codex, Copilot,
opencode, or omp. It is the sole interlocutor on the user's side: the user speaks to the driver, and
Crew never speaks to the user. **The user is the handler of the driver agent; the driver is the
steering wheel and the user is the car driver** `[USER-STATED]` 2026-09-29, verbatim: *"user is the
handler of driver agent. driver is the steering wheel and user is the car driver."* **A driver is
not a harness and not a product; it is a role a session may hold, and a harness is never the
authority.** There is exactly one per team, at a time: *"There is ONE driver, in the crew team"*
`[USER-STATED]`. Crew does not address it as an agent and does not track it as one.

**Decision 2 — `agent`.** A worker Crew spawned. Crew knows it, Crew created it, Crew observes it,
and it is the *first point of contact*: the unit the owner's boundary is drawn around. When an agent
cannot finish, it asks — `crew_request_child` raises a request for a new agent, and
`decide_child` answers it — and Crew's part ends there.

**Decision 3 — `sub-agent`.** A helper a harness spawned, for a harness. **Crew neither creates one,
nor observes one, nor can address one.** This is stated as a **domain boundary, not a permission
level**, and the distinction is load-bearing: a permission level is something Crew could in
principle check and chose not to enforce, whereas a boundary is a fact about what is inside Crew's
domain at all. *There is no route to a sub-agent, because there is nothing at the far end of one to
route to.* `[AGENT-DECIDED]` — the owner ruled the mechanism Crew does not observe; the framing as a
boundary rather than an unenforced permission is the design's reading of that ruling, stated so it
can be rejected.

**Decision 4 — Crew's required knowledge stops at the assignment.** What Crew knows is that a task
has been assigned to an agent and that the agent is handling it. How it is handled is not Crew's
business and is not in Crew's data. No record, table, or interface may be introduced whose purpose
is to hold a fact about a sub-agent. `[USER-STATED]` for the boundary; `[AGENT-DECIDED]` for the
"no route exists" phrasing, which follows from it.

**Decision 5 — the relay is addressed to the driver, and the driver decides who sees what.** The
owner's words `[USER-STATED]`: *"A tells the driver and the driver passes it on"*, and *"Driver and
the driver decides whether to show it to me, or another agent."* An agent's output therefore reaches
the user only through the driver, and Crew's obligation ends at delivering it there.

### What this does and does not change about ADR-0011

**This is an amendment, not a reversal.** [0011](0011-omp-retains-task-graph-authority.md) is
Accepted and its substance is untouched by this record. What it decided — that Rust persists
driver-supplied intent verbatim and applies only the transitions it alone observed the evidence
for, quoted at lines 39-44 of that record — stands exactly as written, and the scope-creep failure
mode it registered against itself at lines 60-62 is unaffected: generalising the authority's *name*
hands Rust no scheduling decision.

What changes is the **subject** of the authority line, from "OMP" to the driving session. That is
the one edit. It is made here, in this record, and not by editing 0011, because the repository
writes an ADR once and leaves it alone (`docs/adr/README.md:3-6`) — the pattern ADR-0037 already used
against 0025 and 0026.

**Verified consequence, stated so nobody has to derive it.** The user and the driver are not
distinguishable to this repository. They share one client role and one authentication shape:
`ClientAuth::OmpExtension` (`crates/protocol/src/rpc.rs:54-66#ClientAuth`, the variant at lines
55-58) carries only an instance id and an agent directory, and `ClientRole::OmpExtension`
(`crates/protocol/src/rpc.rs:38-42#ClientRole`) is the role whose method table includes
`WorkerCreate` (`crates/runtime/src/ipc/mod.rs:271-359#allowed_methods`, the `OmpExtension` arm
beginning at line 286 of that declaration, `WorkerCreate` at line 293) and the `WorkerMcp` arm at
line 344, which contains no method that creates a worker. That is a fact about the authentication
this repository has today, **and it is the reason the harness can never be the authority**: a role
name a peer supplies is not a proof, and `0050` records the consequence with its citations.
`[VERIFIED]` for the tables.

### The authority subject: answered, quoted and dated

**The subject of ADR-0011's authority line is no longer open. It is the driver, not the driving
harness.** The question was put to the owner and answered on 2026-09-29:

> "user is the handler of driver agent. driver is the steering wheel and user is the car driver."

`[USER-STATED]` 2026-09-29. The two candidates this record previously held open were:

* ~~**"The driving harness."** The authority is a property of *software*. The driving harness
  retains task-graph authority, whichever harness it is.~~ — **NOT THE ANSWER.** Struck and kept,
  because it is the candidate the reserved title of this number asserted by wording, and a reader
  who meets the reserved title needs to see that it was not adopted and why. The owner ruled on
  2026-09-29 that the subject is the **driver**; a harness is capable of hosting a driver and is
  not thereby one.
* **"The driver."** The authority is a property of a *role*. Whoever holds the driver's role in
  this session retains task-graph authority, and a harness is not thereby a driver — a harness is
  capable of hosting a driver, and one session in it may hold the role while the next does not.
  — **CHOSEN**, on the owner's ruling quoted above.

**The consequence that had to be named with the answer: a harness is never the authority.** Nothing
in the protocol may establish the driver by naming a product, and the current code does exactly
that — `role: "ompExtension"` is a string the peer supplies, and supplying it is presently
sufficient to receive the driver's spawn-capable method table. The finding is in
[0050](0050-one-rule-two-arms-the-user-and-the-driver-may-start-agents.md) with its citations;
**the driver must prove it is the driver**, and that proof is not built.

**The question this section previously said was unasked — what happens when a session stops being
the driver — is answered by ADR-0039, and the answer is not a handover.** Closing the driver **ends
the team**: *[USER-STATED]* 2026-09-29, *"driver dies, agents stop and save"*, and in the owner's
fuller form, *"If the driver dies unexpectedly, or even if the user just /exit driver, how crew will
ask?! Unless what you mean, is that crew understands that driver is dead, and instructs the agent to
stop, save their work and exit gracefully."* A crash and a deliberate `/exit` are the same event.
A later driver is a **new team** and inherits nothing — no agents, no tasks, no run state — because
ADR-0039's entire subject is that a team's agents do not survive its driver. **There is no driver
switch and no handover ceremony; there is one driver per team, at a time.** `0039` is itself
`Proposed` and the one item it leaves open, the size of the backstop timeout, is not resolved here
and is not reopened by this record.

**What remains open in this record.** The vocabulary, the boundary, and the authority subject are
all settled by the owner's rulings and quoted above. **The record is still `Proposed`** and still
awaits ratification; and the unbuilt proof — how a driver is proved rather than asserted — is work
recorded in [0050](0050-one-rule-two-arms-the-user-and-the-driver-may-start-agents.md), not here.

### Positive Consequences

* The word `sub-agent` can no longer be read as an authority tier, because the record says in one
  sentence what it is: outside Crew's domain.
* "Ask the driver to spawn an agent" is expressible and checkable, and Crew spawning one on an
  agent's own initiative is not — the boundary is the reason, and it is the owner's.
* The names in `crates/protocol/src/coordination.rs:50-59#CoordinationRequestChildParams` and
  `crates/runtime/src/domain/repository.rs:2859-2889#decide_child` now have a defined referent, so
  renaming them to match is mechanical rather than interpretive.
* A later reader who wants Crew to track sub-agents has to argue with a recorded boundary instead
  of with a naming convention.
* ADR-0011's substance survives untouched, so the "exactly one scheduler" invariant is not reopened.

### Negative Consequences

* Two of the three terms have no representation in this repository today, and building them is
  work this record does not specify. `[AGENT-PROPOSED]`
* **The existing OMP-named surfaces stay OMP-named.** `decide_child`'s doc
  (`crates/runtime/src/domain/repository.rs:2859-2889#decide_child`), `crew_send` and
  `crew_request_child`'s descriptions (both inside
  `crates/runtime/src/coordination/mcp_protocol.rs:63#tool_specs`), and `ClientRole::OmpExtension`
  itself (`crates/protocol/src/rpc.rs:38-42#ClientRole`) all say OMP. Renaming them is a separate
  change, and until it lands a reader of those files alone gets the old vocabulary.
* **The user and the driver remain one wire identity**, and the driver is currently **asserted**
  rather than proved — `role: "ompExtension"` is a value the peer supplies, and supplying it is
  presently enough to receive the driver's spawn-capable method table (`0050`, with citations).
  This is a fact about the authentication this repository has today, and it is work, not a gap this
  record may paper over: **the driver must prove it is the driver.**
* **What happens to the driver's authority when the driver exits is settled elsewhere and is not a
  transfer.** Closing the driver **ends the team**: the agents are instructed to stop, save and exit
  gracefully, per ADR-0039. A later driver is a new team inheriting nothing. This record asserts no
  handover, and no part of a team's agents survives its driver.
* An agent that needs a second *agent* must go through the driver every time. That is the owner's
  ruling and it costs a round trip; nothing here argues otherwise.

## Pros and Cons of the Options (rejected alternatives, with reasons)

* **The two-tier permission model.** **Rejected by the owner** `[USER-STATED]`, 2026-09-29: the
  second tier — that an agent freely controls its own sub-agents *as a distinct tier of the
  permission model* — is withdrawn. Crew cannot police a tier whose members it cannot enumerate,
  and enumerating them is what the first ruling forbids.
* **A Crew-side record of harness-spawned children.** **Rejected on the owner's ruling**, and
  independently on its cost: it would require Crew to observe the thing the first ruling says it
  does not need to know, and there is no mechanism in this repository by which it could.
* **One word for everything.** **Rejected** — it satisfies the boundary by silence, and leaves the
  existing prose implying a tier that does not exist.
* **Dropping `driver`.** **Rejected** — it would make the user's own session and the user the same
  operator on paper while the wire already conflates them for an unrelated reason, compounding two
  separate problems.
* **Editing ADR-0011 in place instead of amending it.** **Rejected** on the repository's own
  write-once rule (`docs/adr/README.md:3-6`), the same reason ADR-0037 was written rather than
  0026 edited.

## Links

* Amends [0011](0011-omp-retains-task-graph-authority.md) — the subject of its authority line only;
  its substance at lines 39-44 and its self-registered scope-creep failure mode at lines 60-62 are
  carried forward unamended. `[VERIFIED]` by reading the record.
* Related: [0009](0009-role-based-authorization-from-the-connection-not-per-call.md) — the
  connection-bound-identity discipline this record's vocabulary serves, and why the user and the
  driver being one wire identity is a finding rather than a ruling;
  [0016](0016-coordination-scope-tokens-bound-to-run-and-pid-ancestry.md) — bound scope as the only
  sender identity, the mechanism that makes "the driver decides who sees it" enforceable rather
  than aspirational.
* **Neighbouring record, not this one.** The driver's authority to create an agent, and the gate
  that authorises a command, are a separate proposed record. This record defines the terms it uses;
  it does not state that gate, and must not be read as stating it.
* Verification basis: every `path:line#Symbol` above was opened at commit `2962c80` and the named
  symbol was seen declared at that line or inside that range. Line numbers are as of that commit.
  Claims resting on the owner's planning notes are labelled `[USER-STATED]` and carry no
  `file:line`, because those notes are not in this repository.
