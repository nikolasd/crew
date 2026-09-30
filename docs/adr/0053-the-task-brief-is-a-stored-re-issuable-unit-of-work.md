# The task brief is a stored, re-issuable unit of work: a restart re-issues it into a new session under a different model, with no `--resume`

* Status: Proposed
* Date: 2026-09-30
* Supersedes: *(none — nothing here reverses a decision)*
* Amends: *(none — [0028](0028-submit-prompt-is-journaled-redacted-run-intent.md) is read and left as written; whether the brief's lifetime and 0028's retention bound conflict is one of the open questions below, and resolving it would amend 0028)*
* Numbering: `0053`. `docs/adr/` holds `0001`–`0032` and `0034`–`0038`; `0033` is reserved and deliberately unused (`docs/adr/README.md:13-17`); `0044` is deliberately skipped; `0039`–`0043`, `0045` and `0046` are claimed by other records. `0053` is the next number claimed by nothing. `0052` and `0054` are free and are deliberately left as gaps: the four records being written together were numbered as a block, and two of them were moved onto numbers their subjects had already reserved. **⚠ The owner's planning notes once carried a task-brief proposal at `0048`** — see "Why this is not `0048`".

> **⚠ THIS RECORD IS A DRAFT AND AWAITS THE OWNER'S RATIFICATION.** The owner's rulings it
> records are settled; the object's data shape and storage are **not**, and are marked
> `[AGENT-PROPOSED]` throughout. Ratifying it means resolving the named open questions, changing
> this record's own `Status:` line to `Accepted`, and letting the index row follow. Until then this
> file confers nothing.

## Context and Problem Statement

The owner's restart flow is the whole of this decision, and it is stated as two sentences
`[USER-STATED]`, 2026-09-29:

> "Assign this task to Claude with Sonnet"
>
> "RESTART claude with Opus and assign this task"

The first starts a session fed an initial prompt stating the task. The second starts **another new
session, on the same task, under a different model, with no `--resume`** — the owner is explicit on
the last point, and states it again: *"The new session does NOT use `--resume`. It receives the task
fresh."*

Read as one action rather than two, that flow says something about the *task* that neither sentence
says on its own: the task has content that outlives any one session's outcome, and that content is
**the ask**, not the conversation. It was needed again, unchanged, by a session that never saw the
first one. What makes the second sentence cheap is therefore not that restart is clever. It is that
something about the task was **stored in a form that could be issued again**, and issued into a
session that did not inherit it.

That is what makes the brief a first-class object rather than a convenient naming for a run's
prompt. `[AGENT-PROPOSED]` — the owner stated the flow and the consequence (*the artifact that makes
restart cheap is the task brief*, a first-class object the design will need); the framing as an
object is drawn from the flow, not uttered in it.

**What exists today, and why it is not yet this.** The durable prompt is real and it is
already stored. `record_run_prompt`
(`crates/runtime/src/domain/repository.rs:1045-1080#record_run_prompt`) journals the submit prompt as
the run's durable intent, carrying it as a `Redacted` value so it cannot be constructed un-redacted:
the `RunPromptEvent` variant it constructs is declared at
`crates/protocol/src/event.rs:789-794#RunPromptEvent`, its `prompt` field at line 793 of that
declaration, and `Redacted` itself is a newtype whose only constructor asserts authorship
(`crates/protocol/src/event.rs:203#Redacted`).

Two facts about that stored prompt stop it from being a brief. **It is scoped to a run.** ADR-0028
records it as *that run's* intent, and its doc says why it lives in the journal rather than a column:
the retention sweep then bounds it "like every other event, instead of one row pinning a prompt
forever" (those words are at lines 1049-1051 of `record_run_prompt`'s declaration). A brief has to
outlive the runs that consume it. **And nothing reads it back.** The mechanism that could re-issue it
exists and is nearly the right shape — `run_retry`
(`crates/runtime/src/service/orchestration.rs:1285-1404#run_retry`) takes a prior run id, mints a
distinct new one (line 1349 of that declaration), and already accepts a `WorkerId` distinct from the
prior run's (line 1302). But its prompt parameter is optional and, when omitted, falls back to none
supplied (lines 1307-1310 of that declaration) rather than to the stored ask. The only production
caller that journals a prompt is `run_submit`
(`crates/runtime/src/service/orchestration.rs:969#run_submit`, calling `record_run_prompt` at line
1165 of that declaration), so a task submitted more than once has several prompts and no way to name
which is *the* brief.

## Decision Drivers

* **The owner's flow, verbatim, is the specification.** Two sentences, and the second must produce
  the first's task in a session that never had it.
* **"Same task" has to mean something stored, not something retyped.** If the caller retypes the
  prompt on restart, the two runs' asks can diverge silently and nothing records that they were
  meant to be one.
* **The model is not part of the task.** Restarting under a different model is the owner's stated
  purpose; anything that made the model part of the brief would make the brief un-reissuable exactly
  when it is needed. `[AGENT-PROPOSED]`
* **A re-issued brief is a fresh instruction, not a history.** It arrives as a new prompt in a new
  session. That is the same property that disqualifies record-replay from being a restore (see
  "Related"), and it is the reason this record is not a restore mechanism.
* **Storage is a real choice with a real cost**, and the owner has not made it. Two of the three
  options below conflict with a property an accepted record already claims.

## Considered Options

* **The task brief is a stored, re-issuable unit of work, and a restart re-issues it into a new
  session with no `--resume`.** Chosen — see below.
* **Restart replays the dead run's transcript into the new session.** **Rejected by the owner**
  `[USER-STATED]`. The owner named the "re-fed everything so far" framing as wrong and said restart
  is "not a replay".
* **Restart through the vendor's `--resume`.** **Rejected by the owner** `[USER-STATED]`: "The new
  session does NOT use `--resume`. It receives the task fresh." Restore and restart are different
  operations and are kept apart by a neighbouring record.
* **Status quo: the caller retypes the prompt on every restart.** **Rejected.** It is what makes two
  runs' asks diverge silently, and it makes "restart under a different model" a manual transcription
  rather than an operation.
* **Project the brief from the journal** — no new storage; the most recent `RunPromptEvent` for the
  task *is* the brief. `[AGENT-PROPOSED]`, **not decided.**
* **Give the brief its own storage, scoped to the task.** `[AGENT-PROPOSED]`, **not decided.**
* **Let the driver supply the brief with the task.** `[AGENT-PROPOSED]`, **not decided.**

## Decision Outcome

### What the owner decided

**Decision 1 — a restart re-issues the same task into a new session, under a different model, with
no `--resume`.** `[USER-STATED]` 2026-09-29, from the flow quoted above. The new session receives
the task fresh; it does not inherit the prior session's context and it does not continue the prior
run. A restart is a **fresh assignment**, which is why it is not a restore.

**Decision 2 — what is re-issued is the task, not the transcript and not the history.** The owner's
rejection of replay is what establishes this. `[USER-STATED]`

**Decision 3 — the brief is the thing that makes this possible, and it is a stored, re-issuable unit
of work rather than a property of one run's row.** `[AGENT-PROPOSED]`, drawn from Decisions 1 and 2.
This is the sentence the owner's flow implies and does not say. It is the one inference in this
record that a reader rejecting the rest would still be left holding, so it is stated separately and
labelled.

**Decision 4 — the model is not part of the brief.** It is the parameter that varies across
re-issues, and `run_retry`'s existing `WorkerId` parameter is already the place a different model
would be named. `[AGENT-PROPOSED]`

### What is design inference, and is therefore not decided by this record

Everything in this subsection is `[AGENT-PROPOSED]`. It is recorded because the owner asked for the
reasoning to be written down, and because an implementation cannot start without it — but none of it
is a ruling, and all of it is reversible without touching anything above.

**The proposed data shape.** A brief belonging to a *task*, carrying: the task it belongs to (the
existing `tasks.task_id`, declared inside `MIGRATION_2` at
`crates/runtime/src/db/migrations.rs:42-107#MIGRATION_2`, the `tasks` table's first column at line
51 of that declaration); the prompt text, `Redacted`, read back rather than duplicated; the resolved
profile the session is to run under (`WorkerProfileRef`, whose `model` field is declared at
`crates/protocol/src/worker.rs:22-35#WorkerProfileRef`, line 30 of that declaration, and which
`worker_profiles` persists in the same migration constant); and the display preference and workspace
mode, which have no durable source today. **The shape is a guess about the right set of fields, and
the guess is not evidence.** `[AGENT-PROPOSED]`

**The proposed storage.** The most economical option is that `tasks` gains no prompt column at all:
the brief's text is the most recent `RunPromptEvent` for that task, resolved through the run's own
`task_id`, read back rather than stored twice. That keeps ADR-0028's retention bound intact and adds
no table. **It also gives the brief exactly that bound, and a brief whose lifetime is the journal's
retention window is not obviously the lifetime the owner's flow needs** — a restart after the
retention sweep has run would have no brief to re-issue. That conflict is unresolved and is open
question 1 below; it is the single decision that most changes what gets built. `[AGENT-PROPOSED]`

### Positive Consequences

* "Restart under a different model" becomes an operation rather than a transcription, which is the
  owner's stated purpose for having it.
* The two runs' asks cannot silently diverge, because there is one stored ask and both runs issue
  it.
* The mechanism is nearly present already: `run_retry` mints a distinct run id and already takes a
  distinct `WorkerId`, so the missing piece is the prompt fallback rather than a new lifecycle.
* It keeps the journal's role where the owner put it — recording what was asked, for presentation and
  audit — and out of the path that reconstructs an agent's own memory.

### Negative Consequences

* **A restart is a new run, so the prior run's context is gone.** That is the owner's ruling, and
  it is a real loss: a restarted session knows the task and nothing of what the first session learned
  doing it. `[USER-STATED]`
* **The brief's lifetime is unresolved and the options disagree.** Bounded retention and a brief
  that outlives its runs cannot both hold for the same bytes. Whichever wins, the other is given up.
* **The re-issued text is the redacted text.** The prompt is journalled after redaction, so
  "unchanged" means unchanged from the redacted form; anything the redactor masked is masked in the
  restart. `[VERIFIED]` for the mechanism (the field is a `Redacted` and the redactor's own drop and
  mask rules are at `crates/runtime/src/security/redaction.rs:310-325#sanitize_fragment`); the
  equivalence consequence is `[AGENT-PROPOSED]` analysis.
* **"The brief" is ambiguous when a task has several prompts.** Only `run_submit` journals a prompt
  today, so a corrected prompt carried by a retry is not journalled at all, and a task submitted
  twice has two prompts with nothing to choose between them.
* `run_retry` requires a terminal prior run, so restarting a hung session needs that run cancelled or
  finished first. That is today's method's property; this record does not change it.
* Nothing here is built. A record of this decision describes an object that does not exist, and the
  storage question has to be answered before construction can start.

## Pros and Cons of the Options (rejected alternatives, with reasons)

* **Replay the dead run's transcript into the new session.** **Rejected by the owner** `[USER-STATED]`.
  The owner named the "re-fed everything so far" framing as wrong and said restart is not a replay.
  It would also arrive as new user turns rather than as the agent's own history, which is the same
  defect that disqualifies record-replay from being a restore.
* **Restart through `--resume`.** **Rejected by the owner** `[USER-STATED]`: "The new session does
  NOT use `--resume`." A resume continues a vendor session's own context; the owner's flow asks for
  the same *task* in a *new* session, and those are different operations.
* **Status quo, the caller retypes.** **Rejected** `[AGENT-PROPOSED]`. Two runs' prompts diverge
  silently, and the divergence is invisible because nothing records that the two were meant to be
  the same ask.
* **A bare model string inside the brief.** **Rejected** `[AGENT-PROPOSED]`. `worker_profiles.model`
  is already the durable home of "which model" and is what `evaluate_resume_eligibility` reads back
  when it rebuilds a run's adapter; a second copy would drift from it.
* **A brief with its own storage and its own lifetime.** **Not rejected.** This is the unresolved
  fork of open question 1, and it is recorded as unresolved rather than argued either way.
* **The driver supplies the brief with the task.** **Not rejected, and not evaluated.**
  `[AGENT-PROPOSED]` ADR-0011 has this repository persisting driver-supplied intent verbatim and
  never authoring task content, so a brief carried on `task/upsert` would sit on that side of the
  line. Against it: a driver with no durable store of its own — which the harness-agnostic driver
  model may produce — would have nowhere to keep it. The design did not weigh this.

## The open questions this record leaves visible

1. **Where does the brief live, given retention?** Journal projection (bounded lifetime, no new
   storage, ADR-0028's rationale preserved), storage of its own (survives retention, and 0028's
   rationale for putting the prompt in the journal is thereby weakened), or supplied by the driver
   with the task (survives retention, needs the driver to persist it). **This decides whether this
   record needs an `Amends:` line against 0028, and it is the answer construction waits on.** What
   would settle it: the owner saying which of the two requirements wins when they conflict — a brief
   that outlives its runs, or a prompt whose lifetime the retention sweep bounds.
2. **Which prompt is the brief when a task has more than one?** The first submit's, the latest
   submit's, or one set explicitly. Not stated by the owner, and unanswerable from source today,
   because a retry's prompt is not journalled at all.
3. **How is a model change expressed on re-issue?** As a field of the brief that the re-issue may
   override, or purely as a parameter of the restart request read alongside it. `[AGENT-PROPOSED]`
   for both options; the owner stated only that the model changes and the task does not.
4. **Can a brief be re-issued with modifications?** The owner's phrasing is "assign this task",
   which reads as the same ask; whether a corrected prompt becomes the new brief, or is a separate
   thing, is not stated.
5. **Which fields does the brief carry beyond the prompt?** Model, workspace mode, and display
   preference have no durable source today. Whether they belong to the brief at all is `[AGENT-PROPOSED]`.
6. **Is redacted-only acceptable as "the task fresh"?** The mechanism guarantees it and nothing says
   otherwise, but the owner has not been recorded confirming it, and it means a restart is not always
   equivalent to the original ask.

### Why this is not `0048`

The owner's planning notes once carried a task-brief proposal at `0048`, and their planning log has
since withdrawn that draft from its promotion queue — on the grounds that the record their next pass
will write is about the driver's authority gate, and the task brief is a different question. **That
withdrawal is a statement about which record gets written next, not a decision that the task brief
is unrecorded**, and the vault's own requirements carry the brief as a first-class object in their
own right. `0048` is nonetheless claimed, so this record does not take it: the owner must either
reclaim `0048` for this record or release it, and the two must not both land.

## Links

* Builds on [0028](0028-submit-prompt-is-journaled-redacted-run-intent.md) — the journaled, redacted
  submit prompt this record's stored text already is. 0028 is read here and left as written;
  whether the brief's required lifetime conflicts with 0028's retention rationale is open question 1
  and, if it resolves that way, would need an `Amends:` line against 0028 in a *later* record.
* [0006](0006-type-enforced-redaction-boundary.md) — the type boundary the brief's text carries.
* Related: the separate record on restore, which is the vendor's own `--resume` and which this
  record must not be read as touching; and the driver's authority record, which owns the question of
  who may cause a brief to be re-issued.
* Verification basis: every `path:line#Symbol` above was opened at commit `2962c80` and the named
  symbol was seen declared at that line or inside that range. A line inside a cited declaration is
  named in prose as "line N of" that declaration rather than cited on its own, because a line inside a
  body is not a declaration. Claims resting on the owner's planning notes carry no `file:line`,
  because those notes are not in this repository.
