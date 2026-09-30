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

## The owner's ruling of 2026-09-29 that fixes the three terms

The record below was drafted on 2026-09-29 before the owner fixed the vocabulary the rule is
written in. The three terms it uses are load-bearing, and the ruling that fixes them is quoted
first, verbatim `[USER-STATED]` 2026-09-29:

> "Sub-agent is the agent an existing running harness spawns internally. Agent is a new harness
> spawned by the driver, using crew."

> "A crew-spawned agent, can do whatever he needs. Spawn its own sub-agents, edit, write, ask. It
> cannot though act like a driver! There is ONE driver, in the crew team."

> "user is the handler of driver agent. driver is the steering wheel and user is the car driver."

The three quotations are this record's foundation and are not paraphrased into something firmer
anywhere below. What they fix:

* **`driver` — the harness the user is conversing with.** Claude Code, Codex, Copilot, opencode, or
  omp. It is the user's instrument. The third quotation is the reason the user and the driver are
  not separated by this record: the user is the handler and the driver is the instrument, and a
  steering wheel and the hand on it are two descriptions of one arrangement, not two parties to be
  told apart on the wire.
* **`agent` — a new harness the driver spawned, using Crew.** Crew-spawned, and therefore
  Crew-known: Crew created it, Crew observes it, and Crew's obligation ends at having handed the
  task to it.
* **`sub-agent` — spawned internally by a harness.** Crew neither creates one, nor mirrors one, nor
  observes one. **This is a domain boundary, not a permission level**, and the distinction is
  load-bearing: a permission level is something Crew could in principle check and chose not to; a
  boundary is a fact about what is inside Crew's domain at all. *There is no route to a sub-agent,
  because there is nothing at the far end of one to route to.*

**`WorkerMcp` is the agent's channel back to Crew, and it is not a sub-agent's.** The role named
`ClientRole::WorkerMcp` (`crates/protocol/src/rpc.rs:38-42#ClientRole`, the variant declared at
line 39) belongs to a **Crew-spawned agent** — the middle term above. A sub-agent has no Crew
connection of any kind, so it holds no role here, authenticates to nothing, and has no method table
at all. Every sentence in this record that says "a worker" means the agent, and none of them
reaches a sub-agent.

**An agent's autonomy is total within its own scope, and this record states that as a capability
rather than as a restriction.** The second quotation is the owner's, and it is positive: a
Crew-spawned agent may edit, may write, may ask, and may spawn its own sub-agents natively — and
doing the last of those is expressly permitted. What it may not do is act like a driver, and the
reason is not a narrower grant of authority but that **there is one driver and it is not the
agent**. Nothing in this record reduces what an agent may do to its own task; what the record
governs is the narrower question of who may cause a *new* Crew agent to exist.

**"There is ONE driver, in the crew team" means one driver per team, at a time — and it is not a
statement about what happens after a team ends.** `[USER-STATED]` 2026-09-29, the ruling
[0039](0039-instruct-then-kill-on-timeout.md) records: *"driver dies, agents stop and save."* The
fuller form of the same ruling is the owner's own question and its own answer `[USER-STATED]`,
2026-09-29: *"If the driver dies unexpectedly, or even if the user just /exit driver, how crew will
ask?! Unless what you mean, is that crew understands that driver is dead, and instructs the agent
to stop, save their work and exit gracefully."*

**Closing the driver ends the team.** It is not a slot that transfers to whoever is holding the
wheel next. A crash and a deliberate `/exit` are the same event, Crew detects the departure on the
connection actually being gone and never on silence, and every agent in that team is instructed to
stop, save its work, and exit gracefully, with the kill following the instruction's own window.
**A later driver is a new team with nothing inherited** — no agents, no tasks, no run state, and
nothing from the previous team, because ADR-0039's entire subject is that a team's agents do not
survive its driver. This record therefore **makes no claim, and must not be read as making one,
about handover, transfer, or succession between drivers across sessions.** What the one-driver rule
forbids is narrower and is what the two arms below exist to enforce: **an agent taking over as
driver mid-team.** ADR-0039 remains `Proposed`, and the one item it leaves open — the size of the
backstop timeout — is not resolved here and is not reopened by this record.

**A note on this record's own title and filename.** The H1 and the filename say "the user and the
driver may start agents", and the arm as restated below is now "is the sender the driver?" — the
third quotation above is why the two collapse. The filename is deliberately **not** changed: it is
cited by the row for this record in [README.md](README.md) and from the planning vault, and this
record is not permitted to break either reference. The correction is recorded here rather than
performed on the filename.

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
governs. Only an agent **Crew** spawns is in scope, and only the driver may cause one.
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

### The defect the gate must close: `OmpExtension` is self-assertable

The defect above is the one the predicate was drafted against. The owner's ruling of 2026-09-29
adds a second, and this one sits on the *subject* of the predicate rather than on the step it
gates. Arm 1 as restated asks **"is the sender the driver?"** — and today the party being asked
that question supplies the answer. This is `[VERIFIED]` in the code as of `2962c80`, and it is the
defect the gate must close, because a predicate whose subject the untrusted party names is not a
predicate.

**An agent runs under the owner's uid, so it can present the driver's own role.** Admission as
`OmpExtension` is checked by `validate_agent_directory` and nothing else: the arm at
`crates/runtime/src/ipc/connection.rs:330-334` destructures `ClientAuth::OmpExtension` and, at
line 334, calls `validate_agent_directory(agent_directory, shared.config.euid)` — so the *only*
admission condition is that the directory exists, canonicalizes, and is owned by a uid. That check
is declared at `crates/runtime/src/ipc/connection.rs:372` and its ownership test is at
`crates/runtime/src/ipc/connection.rs:381-386` (`if metadata.uid() != euid`). **The uid compared
against is the daemon's own effective uid, taken from `shared.config.euid` — not the peer's.**
An agent, a worker, or any process the owner runs, is running under that same uid, so it presents
an arbitrary `instanceId` together with an owner-owned `agentDirectory`, passes both checks, and
receives the driver's spawn-capable method table.

**The second half is that nothing binds the connection to the repository it claims.**
`InitializeParams::repository` is declared at `crates/protocol/src/rpc.rs:84` and is never compared
against `ServerConfig::repository`, declared at `crates/runtime/src/ipc/mod.rs:174-175`. So the
peer also names its own repository, and the server serves whichever one the peer asked for.

**The method table makes the consequence concrete.** `ClientRole::OmpExtension` is the arm at
`crates/runtime/src/ipc/mod.rs:286` of `allowed_methods`, declared at
`crates/runtime/src/ipc/mod.rs:271`, and it includes `WorkerCreate` at
`crates/runtime/src/ipc/mod.rs:293`. The `ClientRole::WorkerMcp` arm at
`crates/runtime/src/ipc/mod.rs:344` **contains no method that creates a worker** — it is the
agent's table, and it already omits the spawn capability. The asymmetry the ruling requires is
therefore already half-present in the code: the agent's table is correct, and the privileged table
is the one that must stop being self-assertable.

**Therefore: the driver must prove it is the driver, and an agent can only ever reach arm 2.**
This is recorded as a requirement of this record rather than as an implementation, because this
record is `Proposed` and the code it describes is not built. The combined nature of the fix is
stated under "What is not built yet" and is not two pieces of work.

### Why arm 2 is currently unreachable, not merely unenforced

The mirror image of the defect above is closed — and vacuously. The scope token an agent would have
to present in order to authenticate as `WorkerMcp` **is never minted in production.** `AdapterMcpConfig::reserve`
is declared at `crates/runtime/src/adapter/mcp_config.rs:90` and `AdapterMcpConfig::activate` at
`crates/runtime/src/adapter/mcp_config.rs:108`; `ScopeTokenStore::revoke_for_run` is declared at
`crates/runtime/src/coordination/scope_token.rs:233`. `#[cfg(test)]` begins at
`crates/runtime/src/adapter/mcp_config.rs:256`, and **the only invocations of `reserve` and
`activate` anywhere in the repository are inside that test module** — at
`crates/runtime/src/adapter/mcp_config.rs:381`, `:393`, `:415`, `:417` and `:427`. There is no
call site outside `#[cfg(test)]`.

**So arm 2 is not "enforced but leaky"; it is unreachable.** An agent has no issued credential to
present, so it cannot open the channel the second arm governs, and the honest path — the agent
asking the driver — does not work today. That is a different defect from the self-assertion
above, and it has a different fix; both are named in "What is not built yet" below.

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
  identity is read; a step is permitted when that identity is the driver's, or when
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

**One rule, two arms.** `(1)` **The sender is the driver** → Crew may spawn an agent on its
authority. `(2)` **The counterparty is the driver** → an agent may *ask* the driver to spawn one.
**No third arm, no takeover, no second driver.** `[USER-STATED]` 2026-09-29.

A Crew-mediated step is permitted when **the sender is the driver**, or when **the step's
counterparty is the driver**. Every edge Crew mediates is therefore incident to the driver, and
never between two agents, which is what keeps the topology from becoming a mesh.

**The first arm as it was previously written could not be evaluated, and is restated here.** It
read "the connection's bound identity is the user's **or the driver's**", and a predicate with an
"or" in it has no subject: a caller cannot be checked against a disjunction of two parties when
the protocol has no way to tell them apart, and — as the finding recorded in "The defect this
closes" below establishes — the one role that carries the spawn-capable method table is one any
process running under the owner's uid can present. The arm is now a single question with a single
subject: **is the sender the driver?** The user is not a second value of that question, and this
record does not maintain the "user or the driver" phrasing as though it were a rule.

The disjunction is withdrawn rather than silently edited away, for the reason the record's own
earlier title withdrawal states. `[USER-STATED]` 2026-09-29, the owner:

> "user is the handler of driver agent. driver is the steering wheel and user is the car driver."

Both arms ask who a step belongs to, of a fact rather than of a claim: the first arm asks whether
the bound sender is the driver, the second whether the counterparty is the driver. The first arm
is the operative one for the spawn path, which has no counterparty — only the driver can cause an
agent to exist — and it carries forward the earlier same-day ruling "me and the driver are both
allowed to start agents" read under the later correction above, in which the user *is* the driver's
handler rather than a second authority. The second arm exists because the owner's ruling requires
an agent to be able to ask for another agent, and an ask is a message whose sender is by definition
not the driver.

**Why arm 2 is an ask and not a spawn.** The second arm does not let an agent cause an agent to
exist; it lets an agent *reach* the driver, and the driver then decides under arm 1. That is what
"There is ONE driver, in the crew team" requires: an agent is never the driver, so an agent can
never be the thing that authorises the next agent. The two arms are the whole of the rule, and a
third arm would be a takeover.

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

Only the driver may cause Crew to spawn an agent. The check is the first arm of the predicate
alone, because a spawn has no counterparty: there is no second arm to satisfy. Under the owner's
ruling, a vendor's own subagent mechanism is not this path and is not gated by it; the gate governs
an agent **Crew** spawns.

**The gate must also stop being self-assertable.** Under the arm as restated, "the sender is the
driver" is a question about an authenticated fact, and today the party being asked it supplies the
answer: `role: "ompExtension"` is a value the peer states in its own `InitializeParams`. The
finding is recorded in "The defect this closes" below with its citations, and the requirement that
follows from it is in "What is not built yet": **the driver proves it is the driver, and an agent
can only ever reach arm 2.**

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
* **`OmpExtension` is self-assertable, and the agent's own credential is never issued.** The two
  findings are recorded with their citations under "The defect the gate must close" above, and they
  are stated here as one requirement rather than two, because they are one fix:
  **issuing the agent's credential and closing the privileged role are the same change.** Close
  `OmpExtension` to anything that has not proved it is the driver — a proof that does not exist
  today, because admission checks only that an owner-owned directory exists
  (`crates/runtime/src/ipc/connection.rs:330-334`, `:372`, `:381-386`) and never compares
  `InitializeParams::repository` (`crates/protocol/src/rpc.rs:84`) against
  `ServerConfig::repository` (`crates/runtime/src/ipc/mod.rs:174-175`) — and, in the same change,
  actually issue the scope token an agent presents as `WorkerMcp`, which today is minted nowhere
  outside `#[cfg(test)]` (`crates/runtime/src/adapter/mcp_config.rs:90`, `:108`, `:256`, `:381`,
  `:393`, `:415`, `:417`, `:427`; `crates/runtime/src/coordination/scope_token.rs:233`). **Doing
  only the first leaves arm 2 unreachable; doing only the second leaves the privileged role
  forgeable.** The two halves are not separable work items and are not filed as two.
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
