# One rule, two arms; the user and the driver may start agents, an agent asks the driver, and the sender is read from the connection's bound scope

* Status: Proposed
* Date: 2026-09-29
* Supersedes: *(none — this does not reverse a decision; it records one that has not yet been ratified)*
* Amends: *(none — the tension with [0011](0011-omp-retains-task-graph-authority.md) is named below and discharged by a separate record, not by this one)*

**The title this record was drafted under is withdrawn, 2026-09-29.** The first title was "The
driver is the only party that may spawn an agent or relay a message, and the sender is read from
the connection's bound scope", which recorded the rule with a single arm — the sender is the
driver. That reading is withdrawn rather than silently edited away. `[USER-STATED]` 2026-09-29,
the owner replaced it:

> "me and the driver are both allowed to start agents."

and, asked which of the two forms to record for a step in which an agent asks the driver to
spawn, the owner's whole answer was:

> "a"

which selects the two-arm form over the one-arm form. The title above states the two-arm form and
the file was renamed with it, because a filename that says "the driver is the only party" asserts
the withdrawn reading. The number did not change: 0050 is the number reserved for this subject,
not for one of the readings of it.

**This record is not a decision.** It is written and numbered so that the reasoning and the
rejected alternatives are on the table, and its own status says so. It awaits the owner's
ratification; until the owner ratifies it, the single predicate below is a proposal, the code it
describes is not built, and the three facts in "What is not built yet" are the reason. The
repository's write-once rule means that a later ratification lands in this file's own text rather
than as an edit elsewhere.

## Context and Problem Statement

Crew is not an agent. It is the machinery one driver uses to fulfil what the user asked.

`[USER-STATED]` 2026-09-29, the owner's words:

> "Crew is not an agent. Crew is the machinery that the driver will use to fulfill what the user
> asked."

> "A tells the driver and the driver passes it on."

> "Driver and the driver decides whether to show it to me, or another agent."

> "Codex as a driver, spawning Claude as agent is allowed. Claude spawing its own subagents is
> allowed. Claude spawing Codex, or Claude or whatever as agent is not allowed. Only Codex can
> spawn a new agent. If Claude for some reasons requires a new agent, then this will have to be
> spawned by the driver, meaning that Claude should ask for the driver to spawn a new claude agent."

Two of those sentences need reconciling before they can become code, and the reconciliation is
recorded rather than smoothed over. "Claude spawing its own subagents is allowed" sits beside
"Only Codex can spawn a new agent". `[AGENT-DECIDED]` The distinction is **a subagent spawned by a
harness** and **an agent spawned by Crew**. A harness's own subagent mechanism is the vendor's
business, is not a Crew agent, is not journalled by Crew, and is outside anything this record
governs. Only an agent **Crew** spawns is in scope, and only the user or the driver may cause one.
This is the reading under which both sentences are true at once, and it is marked as a decision
because the owner left the mechanism open.

### The defect this closes

`CoordinationBroker::send` accepts any `recipient_worker_id` that parses, with no check that the
recipient is the sender's child, its sibling, or even on the sender's run.
`[VERIFIED]` at `2962c80` — `crates/runtime/src/coordination/broker.rs:236` declares `send`, and
its `recipient_worker_id: Option<WorkerId>` parameter at
`crates/runtime/src/coordination/broker.rs:243` is passed straight through to the `RunMessage` it
builds. The body checks that the run is live, bounds and redacts the payload, charges the rate
limit, refuses a `task_id` that is not the run's own, and refuses a `reply_to` that does not
reference a visible prior message. **It performs no ancestry predicate of any kind on the
recipient.** A worker that knows another worker's id can address it.

The consequence is concrete rather than theoretical: any worker can address any worker, including a
sibling spawned by the same parent, or a worker belonging to a different task's run entirely. There
is no representation of who spawned whom for a relay to consult, and no predicate to consult it
with.

The sender side is in better shape, and the reason it is in better shape is the reason this record
exists. `crates/runtime/src/ipc/connection.rs:676` compares the request's `senderWorkerId` against
`principal.scoped_worker_id` and refuses a disagreement, so a `workerMcp` caller that names
somebody else is already turned away at the dispatch boundary.

**What that check is, and what it is not.** `[AGENT-DECIDED]` It is a *consistency assertion on a
parameter*, not a *read of the bound identity as the value*. The parameter is still parsed, still
carried, and still the thing being checked. Under this record the sender is not carried at all; it
is read from the connection, and the request has nothing to say about it. The existing check is
the right instinct at the wrong altitude, and the difference is the whole security argument in
"Decision Outcome" below.

## Decision Drivers

* A caller that can name its own identity can forge a chain. This is not a style preference. If the
  sender is a parameter, then a worker that wants to appear to be the driver names the driver, and
  every rule written downstream of that parameter is written against a value the untrusted party
  supplied. `[VERIFIED]` the codebase already reached this conclusion independently: the doc comment
  on `ClientPrincipal::scoped_worker_id`, declared at `crates/runtime/src/ipc/mod.rs:263`, reads
  "the only sender identity `coordination/send` trusts for this connection, regardless of what a
  request's `senderWorkerId` parameter claims." This record makes that sentence the gate's
  mechanism rather than a note about one field.
* ADR-0009 ([0009](0009-role-based-authorization-from-the-connection-not-per-call.md)) already
  settled the general form: authorization is decided from what the connection authenticated as,
  never from what the client claims per call. The coordination plane is the same problem in a place
  ADR-0009 did not reach, and re-deriving the rule here rather than restating that record's keeps
  each record's reasoning intact under the write-once rule.
* The predicate has to be right in every place it is applied. A rule that must be restated at the
  spawn path, and again at the driver-to-agent relay, and again at the agent-to-driver relay, is a
  rule that will be implemented three times and will drift. **One rule, not three**, is a
  correctness requirement and not a compression of prose.
* Every mediated hop is visible to Crew. The gate is the only thing standing between a worker's
  request and another worker's context, so it must be a check and not a convention.
* The driver, and only the driver, decides what the user sees. `[USER-STATED]` "Driver and the
  driver decides whether to show it to me, or another agent." Crew never speaks to the user, which
  is the first quotation in this record.

## Considered Options

* **One predicate over the bound sender, applied at every mediated step.** The connection's bound
  identity is read; a step is permitted when that identity is the user's or the driver's, or when
  the step's counterparty is the driver. Spawn, the driver-to-agent relay and the agent-to-driver
  relay are three call sites of this one predicate.
* **A per-node tree with ancestry predicates.** Model parent/child edges and permit a send when the
  recipient is a descendant of the sender. This is what a relay *chain* needs, and it is what the
  superseded model proposed.
* **A broadcast path.** Let a sender address a set of peers, so one message reaches several
  agents without N sends.
* **Separate rules per path.** One rule for spawn, one for the forward relay, one for the reverse
  relay, each written where it is implemented.
* **A wildcard or group recipient.** A recipient that is a set, a role, or `*`, resolved at
  delivery time.
* **Accept the sender identity as a request parameter.** Keep today's shape and add an authorization
  check that compares the parameter against a role table.

### One predicate over the bound sender

One definition, three call sites, and the two directions are mirrors of each other rather than two
rules. The chain-top and sibling questions it forecloses are foreclosed by the predicate rather
than by extra checks: an agent addressing another agent satisfies neither arm, because the sender
is not the driver and the counterparty is not the driver either.

Against it: the predicate's second arm is not obvious from the driver's own vocabulary, and a
reader who expects "is the sender the driver?" to be the whole rule will read the second arm as a
second rule bolted on. The record therefore states both arms together and says explicitly that they
are one predicate.

### A per-node tree with ancestry predicates

This is the strongest rejected option and the one a future reader is most likely to propose again,
because the "chain" it serves was a real design two rulings ago.

Against it: it is superseded by the model this record records. `[USER-STATED]` "Crew is not an
agent." A tree of agents with ancestry is a second hierarchy sitting beside the driver, and the
owner's sentence "Only Codex can spawn a new agent" removes the multiplicity that made the tree
worth modelling. It is also strictly more machinery for the same outcome: with exactly one driver,
"is the recipient my descendant" is answered by "is the sender the driver", and the ancestor walk,
the parent column, and the cycle question that a tree raises and this model does not, all disappear.
And it would put the trust boundary back in the wrong place, because an ancestry graph is only as
good as the edges in it, and the edges are exactly the thing a forgeable sender parameter would
supply.

### A broadcast path

Against it: a broadcast has no sender-to-recipient relation to check, so it cannot be gated by a
predicate about the sender at all — the gate would have to become a predicate about the recipient
set, which is a different rule with a different failure mode. It also contradicts the star directly:
"Driver and the driver decides whether to show it to me, or another agent" means the driver chooses
the audience, and a broadcast hands the audience to a worker.

### Separate rules per path

Against it: this is the drift the record exists to prevent, stated as an alternative so the reason
is on the table. Three rules means three places to update when the model changes, and the
repository has a record of that cost: this very decision arrived as a relay-chain gate and had to be
re-argued as a spawn-and-relay gate once the model changed, because the chain framing had
hard-coded the relay into the rule's shape. One predicate, defined once, is what makes the two
directions provably mirrors.

### A wildcard or group recipient

Against it: resolution happens at delivery time, which is after the gate has already run, so the
gate would be authorising an intent rather than a step. It also reintroduces the broadcast's
audience problem with an extra indirection, and it makes "which agents did this reach?" a question
answered at replay rather than at send.

### Accept the sender identity as a request parameter

Against it: the one the codebase has already answered. A parameter is a claim; a bound scope is an
authenticated fact. Keeping the parameter and adding a role check leaves the untrusted party
supplying the value the check reads, which is a check that can be satisfied by naming the right
person.

## Decision Outcome

Chosen option, subject to the owner: **one predicate, with the sender read from the connection's
bound scope, applied at every step Crew mediates.**

### The predicate, stated once

A Crew-mediated step is permitted when **the connection's bound identity is the user's or the
driver's**, or when **the step's counterparty is the driver**. Every edge Crew mediates is
therefore incident to the user or the driver, and never between two agents, which is what keeps
the topology from becoming a mesh.

Both arms ask who a step belongs to, of a fact rather than of a claim: the first arm asks whether
the bound sender is the user or the driver, the second whether the counterparty is the driver. The
first arm is the operative one for the spawn path, which has no counterparty: only the user or the
driver can cause an agent to exist, which is the owner's ruling "me and the driver are both allowed
to start agents" in executable form, and which carries forward the earlier sentence "Only Codex can
spawn a new agent" with the user named alongside the driver. The second arm exists because the
owner's ruling requires an agent to be able to ask for another agent, and an ask is a message
whose sender is by definition neither the user nor the driver.

**This is one rule and not three.** The spawn path, the driver-to-agent relay and the
agent-to-driver relay are three call sites of the predicate defined in this section. They are not
three rules, they are not independently specifiable, and a change to any one of them is a change to
this predicate. A future reader who wants to know whether the forward relay and the reverse relay
can drift apart should read the two call sites and find that neither of them contains a rule — they
contain a call.

### Why the bound scope, and not a parameter, is load-bearing

If the sender arrives as a request parameter, a caller that wants to be the driver names the
driver, and the gate passes. The check is then satisfied by the untrusted party supplying the value
it is checked against, which is not a check at all.

This is not hypothetical here. `crates/runtime/src/ipc/connection.rs:676` already compares the
request's `senderWorkerId` to the bound scope, which stops a *dishonest* caller. It does not stop a
caller who is being asked a question it can answer: the comparison proves the parameter agrees with
the scope, and the code then continues to use the parameter. Under this record the sender is not a
parameter, so there is nothing to agree.

`[VERIFIED]` The field that carries the fact is `ClientPrincipal::scoped_worker_id`, declared at
`crates/runtime/src/ipc/mod.rs:263`, whose own doc comment already states the rule this record
adopts. The gate reads that field. It does not read, compare, or fall back to any `senderWorkerId`
in the request.

### The spawn path

Only the user or the driver may cause Crew to spawn an agent. The check is the first arm of the
predicate alone, because a spawn has no counterparty: there is no second arm to satisfy. Under the
owner's ruling, a vendor's own subagent mechanism is not this path and is not gated by it; the
gate governs an agent **Crew** spawns.

### Relay, in both directions

* **Driver to agent.** Permitted by the first arm. The driver may address any agent, whichever
  vendor that agent runs, and that is the whole of the driver's authority over the agent
  population.
* **Agent to driver.** Permitted by the second arm. This is the "A tells the driver and the driver
  passes it on" direction, and it is the only Crew-mediated step available to an agent.
* **Agent to agent.** Satisfies neither arm and is refused. It does not need a rule of its own; the
  predicate already answers it.

### The star, not a mesh

The relay is a star. Agent-to-agent never happens directly; it is two edges through the driver, and
the driver's decision is the only thing that turns one agent's message into another agent's input.
That is the owner's sentence "Driver and the driver decides whether to show it to me, or another
agent" read as topology rather than as narration.

### An agent that needs another agent asks the driver

This is the two-hop path, and it is two hops rather than one because the first hop is a message to
the driver under the second arm and the second hop is a spawn under the first. Both are visible to
Crew, and both are admitted by this one predicate. An agent never spawns a Crew agent; it asks, and
the driver decides whether to spawn, which vendor, and whether to pass the answer on.

## What is not built yet

`[VERIFIED]` at `2962c80`. Stated plainly, because a record that implied otherwise would be
describing a system that does not exist:

* **There is no driver role.** `ClientRole` is declared at `crates/protocol/src/rpc.rs:38` and has
  exactly three variants: `OmpExtension`, `WorkerMcp` and `Display`. The predicate's subject does
  not exist in the protocol yet, so "is the bound sender the driver?" cannot be evaluated against
  any role as the code stands.
* **There is no spawn call.** No `crew_spawn_agent`, and no equivalent, exists anywhere in the
  repository. The spawn path this record governs is unbuilt.
* **The relay and the driver are two different paths today, and only one of them binds the
  sender.** `CoordinationBroker::send` is declared at
  `crates/runtime/src/coordination/broker.rs:236` and is reached from
  `crates/runtime/src/ipc/connection.rs:673`; the `workerMcp` table that carries it is the arm at
  `crates/runtime/src/ipc/mod.rs:344`. Separately, `OrchestrationService::message_send` is declared
  at `crates/runtime/src/service/orchestration.rs:2628` and reads its sender from the request at
  `crates/runtime/src/service/orchestration.rs:2634` with no scope check at all. **These are two
  paths, not one.** They differ in role, in whether the sender is bound, and in whether delivery is
  attempted: the broker's own doc comment records that it has no `RunDriver` and therefore settles
  the message at `recorded` rather than advancing it to `sent`. A gate that lands on one of them
  and not the other has not closed the defect, and which of them the gate governs is a question this
  record does not decide.
* **`coordination/requestChild` and `ChildDecision` still name OMP as the arbiter.** The
  `crew_request_child` tool is declared at
  `crates/runtime/src/coordination/mcp_protocol.rs:209` and its description at
  `crates/runtime/src/coordination/mcp_protocol.rs:210` reads "Ask OMP to authorize a child worker.
  Never creates a task or worker itself." `CoordinationBroker::request_child` is declared at
  `crates/runtime/src/coordination/broker.rs:573` and reaches every `workerMcp` caller, while the
  decision half is `ompExtension`-only — the note at
  `crates/runtime/src/domain/repository.rs:2880` says so in its own words, and
  `ChildDecision` is declared at `crates/runtime/src/domain/repository.rs:195`. **Under the model
  this record states, the arbiter is the driver, not OMP, and that is a live-code contradiction this
  record does not resolve by editing code.** It is named here so that nobody reads this record as
  though the current wording already agrees with it. The distinction the current shape actually
  draws is "a worker may ask, and only OMP may decide"; the model this record records is "only the
  driver may decide, and a worker may only ask the driver".

## Positive Consequences

* A worker can no longer reach a sibling, an ancestor, or an unrelated worker on another run, and it
  cannot forge its way past the check by naming somebody else, because it does not name anybody.
* The rule has one definition. Changing the model changes one predicate, not three call sites that
  each carry a copy of the reasoning.
* The two relay directions are provably mirrors rather than two decisions that happen to agree.
* Every mediated hop stays visible: the gate is a check on a step, so an unauthorised step is
  refused rather than silently dropped, and the refusal is a typed error at the dispatch boundary
  where ADR-0009 already put it.
* An agent that genuinely needs a second agent has a path, and the path is the two-hop ask, which
  means the driver sees the request and can decline it.

## Negative Consequences

* An agent cannot talk to a peer. Under the star, two agents working the same problem must route
  through the driver, which costs a hop and gives the driver a decision it did not ask for. This is
  the price of the model and it is paid deliberately.
* The predicate's two arms read as two rules to a reader who arrives expecting one, which is why
  this section states them together and says why the second exists. The mitigation is prose, and
  prose is weaker than a type; `[AGENT-PROPOSED]` a single function whose name is the predicate, and
  whose only callers are the three sites, would make the rule structural rather than written down.
  That is construction, not a decision, and it is not built.
* **The tension with ADR-0011 is real and is not discharged here.** ADR-0011 and AGENTS.md
  invariant 6 both say that OMP owns the task graph and that Rust never creates or edits it. This
  record makes the **driver** the party that spawns, and the driver's ruling explicitly allows Codex
  to be that driver. Those two statements cannot both stand unchanged. This record amends nothing
  and supersedes nothing; the amendment is a separate record whose subject is still contested, and
  until it is written, ADR-0011 stands as written and this record is a proposal. A reader who
  promotes this record without the amendment is knowingly leaving the repository inconsistent, and
  the `Amends:` header above says so rather than implying otherwise.
* Because the predicate's subject does not exist in the protocol yet, nothing here can be
  implemented as written without first answering what a driver *is* on the wire.

## Pros and Cons of the Options (rejected alternatives, with reasons)

1. **A per-node tree with ancestry predicates.** **Rejected**, and this is the option the superseded
   model wanted. `[USER-STATED]` "Crew is not an agent." With one driver, "is the recipient my
   descendant" collapses to "is the sender the driver". A tree would keep a second hierarchy alive
   beside the driver, and its edges would be exactly the values a forgeable parameter supplies.
2. **A broadcast path.** **Rejected.** There is no sender-to-recipient relation to check, so the
   gate would have to become a predicate about the recipient set — a different rule, at a different
   altitude, with a different failure mode. It also hands the audience to a worker, which is the
   driver's decision to make.
3. **Separate rules for spawn and for each relay direction.** **Rejected.** Three rules is three
   places to update, and the cost is already banked: this decision had to be re-argued once because
   the relay-chain framing had put the relay into the rule's shape. One predicate, three call sites,
   no rule in any of them.
4. **A wildcard or group recipient.** **Rejected.** It resolves after the gate has run, so the gate
   authorises an intent rather than a step, and it makes the audience of a message a fact recovered
   at delivery rather than a fact checked at send.
5. **Accepting a sender identity as a request parameter.** **Rejected.** A parameter is a claim and
   a bound scope is an authenticated fact; a check that reads the value the untrusted party supplied
   is satisfied by supplying the right value. `[VERIFIED]` the repository already reached this
   independently at `crates/runtime/src/ipc/mod.rs:263`, whose doc comment states that the bound
   worker is the only sender identity `coordination/send` trusts "regardless of what a request's
   `senderWorkerId` parameter claims".

## Links

* [0009](0009-role-based-authorization-from-the-connection-not-per-call.md) — authorization from the
  connection, not per call. The general form this record applies to the coordination plane.
* [0011](0011-omp-retains-task-graph-authority.md) — OMP retains task-graph authority. Named here
  as the record this one is in tension with, not amended by it.
* [0016](0016-coordination-scope-tokens-bound-to-run-and-pid-ancestry.md) — coordination scope
  tokens bound to run identity and PID ancestry. The existing binding discipline this gate reads
  from.
* [0017](0017-record-before-delivery-message-semantics.md) — record-before-delivery message
  semantics. Why a refused step is refused before anything is journaled.
