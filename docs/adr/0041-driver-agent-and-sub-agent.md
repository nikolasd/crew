# `driver`, `agent`, and `sub-agent`: Crew knows only the agent it spawned, and the third term is a domain boundary rather than a permission level

* Status: Proposed
* Date: 2026-09-30
* Supersedes: *(none — nothing here reverses a decision)*
* Amends: [0011](0011-omp-retains-task-graph-authority.md) (the *subject* of its authority line only; its substance is untouched — see "What this does and does not change about ADR-0011" below)
* Numbering: `0041`. The owner's planning notes reserved this number for this subject — "The driving harness retains task-graph authority, whichever harness that is" — and **this record is that decision, so it carries the reserved number.** `docs/adr/` holds `0001`–`0032` and `0034`–`0038`; `0033` is reserved and deliberately unused (`docs/adr/README.md:13-17`); `0044` is deliberately skipped. **⚠ The reserved title asserted a subject this record deliberately leaves open** — see "The open question this record does not answer", which is the one thing the owner must read before ratifying.

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

**Decision 1 — `driver`.** The agent the user started. It is the sole interlocutor on the user's
side: the user speaks to the driver, and Crew never speaks to the user. There is exactly one
`[USER-STATED]` — *"there is only one driver agent"*. A driver is not a harness and not a product; it
is a role a session may hold. Crew does not address it as an agent and does not track it as one.

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
beginning at line 286 of that declaration, `WorkerCreate` at line 293). So *"the user or the driver"*
reduces on the wire to *"a connection that authenticated as `OmpExtension`"*, and the `WorkerMcp`
arm of the same table (line 344) contains no method that creates a worker. That is a fact about the
authentication this repository has today, **not** a claim that the user and the driver are the same
party. `[VERIFIED]` for the tables; `[AGENT-PROPOSED]` for the reduction.

### The open question this record does not answer

**The subject of ADR-0011's authority line is still open, and this record does not settle it.**
There are two candidate subjects, and they are not variants of one another:

* **"The driving harness."** The authority is a property of *software*. The driving harness retains
  task-graph authority, whichever harness it is.
* **"The driver."** The authority is a property of a *role*. Whoever holds the driver's role in this
  session retains task-graph authority, and a harness is not thereby a driver — a harness is capable
  of hosting a driver, and one session in it may hold the role while the next does not.

These differ in a way that has consequences. Under the first, a harness that is not currently
driving is out of scope by construction, and the line reads as a product statement. Under the
second, the line reads as a role statement, and it becomes necessary to say what happens when a
session stops being the driver — which is a question about session identity this repository has not
been asked and has not answered.

**This record deliberately uses the word "driver" throughout and does not assert that the authority
subject is the driver rather than the harness.** It settles the vocabulary and the boundary, and it
notes that the subject is open. `[AGENT-PROPOSED]` — the owner has not been asked, and this record
must not supply the answer by writing it down.

**⚠ The title this number was reserved under asserts the first candidate as the settled answer.**
When the owner's planning notes reserved `0041` for this subject, the title they chose was "The
driving harness retains task-graph authority, whichever harness that is" — and that wording **decides
the question above by assertion**. The reservation identified the subject; the title that came with
it went further and picked a side.

**This record keeps the number and does not keep the assertion.** Its own title names the
vocabulary and the boundary — the things the owner ruled — and leaves the authority subject stated
as open, because it is open. Ratifying this record therefore means **correcting that earlier
title**, and the correction is substantive rather than cosmetic: the record lands saying "the
subject of ADR-0011's authority line is unsettled, and here are the two candidates and what turns on
the choice", where the reserved wording would have landed saying the first candidate was the answer.
**[AGENT-PROPOSED]** — the owner has not been asked which candidate is correct, and choosing one here
is the one thing this section exists to prevent.

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
* **The user and the driver remain one wire identity**, so any mechanism that needs to tell them
  apart cannot be built on the current authentication. This record makes no claim about whether
  they must be.
* **The record is silent about what happens to the driver's authority when the driver exits.** The
  open question above is the reason, and it is not a gap this record may paper over.
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
