# The submit prompt is journaled, redacted, as durable run intent

* Status: Accepted
* Date: 2026-08-30

## Context and Problem Statement

A run's prompt — what the leader actually asked the worker to do — was never made durable. It was
read from `run/submit`'s params, passed to the adapter as `StartSpec::prompt`, and existed only in
memory for the life of the process:

* `runs` has no prompt column, and none of the four `ALTER TABLE runs` migrations adds one
  (`policy_fingerprint`, `transcript_cursor`, `plan_ref`, `flags_turn_settled`).
* `submit_run` journals a `Run` record that has no prompt field, so `RunQueued` never carried it.
* The only `INSERT INTO messages` is reached from the coordination broker and `message/send` —
  never from submit.
* `tasks` stores no text at all by design: task id, project, owner, revision, timestamps.

So nothing in the system could answer "what was this run asked to do?" after the fact. Not
`run/result`, not `events/replay`, not the audit export, not the monitor, not a human reading the
journal during an incident. A run's transcript was a set of answers to a question nobody recorded.

The concrete trigger was a dashboard column: showing what each run is executing had no honest
source, and the alternatives were to derive it from the first journaled *assistant* message — which
is the answer, not the task — or to ship a permanently empty column. Both were rejected, and the
absence became a decision to make rather than a gap to paper over.

The question is not whether the intent is worth recording. It is what recording it costs, because a
prompt is user-authored content that can contain secrets, and anything durable here crosses
[ADR-0006](0006-type-enforced-redaction-boundary.md)'s boundary and lands in every surface that
reads the journal.

## Decision Drivers

* Invariant 4 — *intent persisted before side effects, content redacted before durability* — reads
  as a requirement for exactly this. The prompt **is** the intent, and it was the one piece of
  intent the runtime acted on without persisting.
* A run's journal is only interpretable against its question. Every consumer that reads run content
  is degraded without it, and the degradation is invisible: the transcript looks complete.
* Prompts are content, not metadata. Whatever is stored has to pass the classification boundary,
  not merely be trusted because the caller is the leader.
* Durability and *distribution* are separable concerns here, and conflating them would understate
  the cost. The journal is read by the audit export, `events/replay`, and (since the dashboard's
  transcript route) a browser page.

## Considered Options

* Journal the full prompt at submit, classified `Visible` and passed through `Redactor::sanitize`.
* Journal only a truncated summary — the first line, or a fixed prefix.
* Keep it ephemeral, and derive any display text from the vendor's own transcript file or from the
  first journaled assistant message.

## Decision Outcome

Chosen option: **the full prompt, classified `Visible`, through `Redactor::sanitize`, journaled at
`run/submit` before the adapter is spawned.**

Its own event kind rather than a field on `RunQueued`: a run submitted with no prompt then has no
prompt event, instead of a `RunQueued` carrying a null that every reader has to interpret. Like
every other domain mutation it commits and broadcasts in the same call
([ADR-0020](0020-per-mutation-event-broadcast-is-not-optional.md)).

Ordering is the part that matters and is not incidental: the prompt is journaled *before* the
adapter starts, so invariant 4's first clause holds structurally rather than by convention — the
intent is durable before the side effect it authorizes exists.

`Visible` is the correct classification and not a convenience. `Thinking` and `Secret` describe
vendor-produced content; a submit prompt is authored by the leader and is meant to be readable. So
the secret-shaped denylist applies to it and the text itself survives, which is precisely the
intent.

### Positive Consequences

* `run/result`, `events/replay`, the audit export, and the dashboard's per-run transcript all gain
  the run's question. The dashboard's task column becomes possible with a real source rather than
  an inference.
* Invariant 4's first clause is satisfied by construction at the one call site that was violating
  its spirit.
* No truncation, so no lossy projection. A reader who wants one line takes the first line of a
  complete record; a reader given only the first line cannot recover the rest. Choosing the summary
  at write time would decide, permanently and on everyone's behalf, which half of a two-paragraph
  prompt mattered.

### Negative Consequences

* **The distribution surface genuinely widens.** The prompt now appears in the audit export
  (`audit/export.rs` selects `FROM events`), in `events/replay`, and in the dashboard's transcript
  route — which is a web page, even a token-gated localhost one. Before this decision, submit
  prompts appeared in none of them.
* **Content is only as protected as the denylist**, which ADR-0006 deliberately keeps small because
  *classification* is the primary boundary. A prompt is `Visible` by definition, so a secret in a
  shape the denylist does not match becomes durable. This decision therefore leans on a mechanism
  ADR-0006 explicitly described as the weaker of its two, and it does so knowingly.
* Journal size now scales with prompt length, and by choosing not to truncate we accept that a
  pathologically large prompt is stored whole.

### The asymmetry this decision does not resolve

Recorded because it was found while making this decision, and a reader comparing the two paths will
otherwise assume it was overlooked.

`message/send` payloads — steers and answers, also user-authored text — are **already durable and
are not redacted at all.** The payload is read from the request params and reaches `INSERT INTO
messages` verbatim; `service/orchestration.rs` contains no `Redactor`, `sanitize`, or `Classified`
reference of any kind. `crates/protocol/src/message.rs` nonetheless documents the field as "the
message payload (redacted before persistence)", which is false, and false in the most misleading
possible place: a security property asserted next to the field, where whoever adds the next
message-producing path will reasonably trust it.

The two exposure classes are therefore *different rather than nested*, and it is worth being exact:

| | durable | redacted | in `audit export` |
|---|---|---|---|
| steer / answer payload (today) | yes | **no** | no |
| submit prompt (this decision) | yes | yes | **yes** |

So this decision produces a system where the redacted content is the widely distributed one and the
unredacted content is the narrowly held one — defensible, but only while stated.

**The cause is structural, and worth naming because it is larger than one missing call.** There are
two paths by which an event becomes durable, and only one of them is the boundary ADR-0006
describes. `DatabaseHandle::append_event` takes a `PersistableEvent` — a type with no public
constructor, obtainable only from `Redactor::sanitize` — and that is the path ADR-0006's tests scan.
`DomainRepository::append_and_apply` takes a plain `&RuntimeEvent` and inserts its serialization
into `events.event_json` directly, with no boundary type anywhere in the signature. Every domain
event is written that way.

The consequence is that redaction on the domain-event path is *convention*, which is the precise
thing ADR-0006 was written to eliminate — "a convention is only as strong as the discipline of
whoever adds the next call site." The message payload is not an oversight by one author; it is what
that missing boundary produces by default. This decision's own prompt event is redacted by the same
convention: `run/submit` classifies and sanitizes before calling `record_run_prompt`, and nothing in
the type system requires it to. The method's doc comment says so outright rather than implying a
guarantee it cannot make.

Extending the type boundary to the domain-event path is the real fix and is out of scope here — it
touches every domain mutation, not one call site. Stated so that whoever picks up the message-payload
work sizes it correctly, and does not conclude that adding one `sanitize` call closes the class. Closing the
message-path gap is its own decision with its own security reasoning, and is tracked separately; it
is not folded in here, because a decision about run intent should not quietly also change how
message content is persisted.

The false doc comment on `Message::payload` is deliberately **not** edited. Its words — "redacted
before persistence" — are what the code *should* do, so the fix is to make them true rather than to
soften them into an accurate description of a gap. That is the separately tracked follow-up which
routes the message payload through the redactor; once it lands the comment is correct exactly as
written, and this paragraph is the record that for a period it was not.

### A rationale this decision invalidates

`TranscriptFormat::recorded_prompt` is deliberately an accessor and not a `TuiEvent`, and the design
note for that work justified it partly on the grounds that emitting one would duplicate "the
already-journaled prompt". No prompt was journaled at the time, so that leg was never sound.

The conclusion stands on its remaining leg, which is the sound one and now the only one: a
user-entry event must not be emitted because a fresh start would duplicate the prompt this decision
makes genuinely durable, and a resume would replay it as new conversation that already happened.
Cited here so the disproved half is not resurrected once journaling makes it superficially
plausible.

## Pros and Cons of the Options

### Full prompt, redacted, at submit (chosen)

* Good, because the intent behind every run becomes durable and interpretable, and invariant 4 holds
  by construction rather than by reviewer vigilance.
* Good, because no reader is forced to accept somebody else's guess about which part of the prompt
  mattered.
* Bad, because it widens the distribution surface for user content, and it depends on the denylist
  for the residual secret risk rather than on classification.

### Truncated summary only

* Good, because it bounds both the stored size and the exposure, and it satisfies the display case
  that prompted the question.
* Bad, because the truncation point is a guess made once, at write time, for every future reader —
  and the discarded remainder is unrecoverable. It also buys less protection than it appears to: a
  secret pasted at the start of a prompt is inside the first line.

### Keep it ephemeral

* Good, because it changes nothing and adds no durable content.
* Bad, because it leaves every journal consumer reading answers to an unrecorded question, and the
  only available substitutes are dishonest: the vendor's own transcript file is unredacted and must
  never be served ([ADR-0006](0006-type-enforced-redaction-boundary.md)), and the first assistant
  message is the response, not the request.

## Links

* Builds on [ADR-0006](0006-type-enforced-redaction-boundary.md) — the classification boundary this
  content passes through, and the source of the "classification is primary, the denylist is
  secondary" framing above.
* Constrained by [ADR-0020](0020-per-mutation-event-broadcast-is-not-optional.md) — the prompt event
  commits and broadcasts in one call like every other mutation.
* Proven in `crates/runtime/tests/orchestration_rpc.rs` at the RPC boundary: a submit whose prompt
  contains a vendor-key-shaped string journals an event that does not contain it and does contain
  the surrounding text. Mutation-checked — bypassing `sanitize_fragment` fails that test and only
  that test. Deliberately *not* claimed to be proven by `redaction_boundary.rs`'s byte-scan: that
  file is one test in its own binary by design (it installs a process-global `tracing` subscriber),
  and it exercises the `PersistableEvent` path, which — see below — is not the path a domain event
  takes.
