# Restore is the vendor's own `--resume` and only that; Crew never replays its journal into a vendor session

* Status: Proposed
* Date: 2026-09-30
* Supersedes: *(none — no record-replay path was ever built or proposed in this repository)*
* Amends: *(none)*
* Numbering: `0045`. The owner's planning notes reserved this number for this subject — "Restore is the vendor's own `--resume`, and only that; Crew's journal is never replayed into a vendor session as a second restore path" — and **this record is that decision, so it carries the reserved number.** `docs/adr/` holds `0001`–`0032` and `0034`–`0038`; `0033` is reserved and deliberately unused (`docs/adr/README.md:13-17`); `0044` is deliberately skipped.

> **⚠ THIS RECORD IS A DRAFT AND AWAITS THE OWNER'S RATIFICATION.** Nothing in it is a decision
> yet. **It must not be read as saying restore works.** The evidence caveats in "What restore is
> actually demonstrated to do" are part of the record, not a footnote to it. Ratifying it means
> resolving the named open questions, changing this record's own `Status:` line to `Accepted`, and
> letting the index row follow.

## Context and Problem Statement

Restore already works one way in this repository, and there is no second way.

`resume_run` (`crates/runtime/src/adapter/registry.rs:496-554#resume_run`) resumes a specific run on
a previously established vendor session, and its own doc is explicit about what that means: it
"creates no run row, takes no prompt, and continues the same run" (those words at lines 506-507 of
that declaration). It reaches the vendor through the `resume_launch` method of the `TuiVendor` trait
(`crates/runtime/src/adapter/tui/adapter.rs:113-120#resume_launch`, the trait itself declared at
line 105 of that file) via `resume_from`
(`crates/runtime/src/adapter/tui/adapter.rs:865-889#resume_from`), whose doc records the load-bearing
property in its own words — "no prompt injection" (line 871 of that declaration). For Copilot the
argv is `--resume=<session-id>`
(`crates/runtime/src/adapter/tui/copilot.rs:106-120#resume_launch`, the `impl TuiVendor for
CopilotTuiVendor` block opening at line 79 of that file; the argument is built at line 112 of the
method declaration): the vendor's own flag, against the vendor's own on-disk state.

The only production caller is startup recovery, which is resume-first:
`recover_run_resume_first` (`crates/runtime/src/recovery.rs:573-632#recover_run_resume_first`, the
call at line 607 of that declaration). Eligibility is decided first, and a run with no vendor session
is refused by name — "no vendor session was ever established for this run", inside
`evaluate_resume_eligibility`
(`crates/runtime/src/recovery.rs:681-784#evaluate_resume_eligibility`, lines 694-696 of that
declaration) — after which the fallback is `resume_failed_fallback`
(`crates/runtime/src/recovery.rs:634-679#resume_failed_fallback`), which **settles the run terminal**.
It never reconstructs it.

What the resume path reads from the run row is bookkeeping, not context.
`stored_resume_state` (`crates/runtime/src/adapter/registry.rs:1262-1301#stored_resume_state`)
selects the vendor session id and the transcript cursor, and the cursor's only job is telling the
tailer where to resume *watching*, so a resumed session's output is not journalled twice — the
failure it guards against is stated in this repository's own words as "risking a fresh-tail
duplicate replay" (lines 697-699 of `evaluate_resume_eligibility`'s declaration). It is a position
in a file, not the file's content.

**So the question this record answers is narrow and real:** the journal holds every turn Crew ever
recorded, and nothing reads it back to reconstruct an agent. Why not? Because it could not, and
because building a path that tried would be worse than having none.

## Decision Drivers

* **The owner's ruling.** Restore is the vendor's own mechanism, and there is no Crew-side
  record-replay path. `[USER-STATED]` 2026-09-29.
* **The journal is redacted before durability, so it is not the agent's memory.** This repository's
  fourth invariant is "Intent persisted before side effects; content redacted before durability"
  (`AGENTS.md:336`), and the mechanism is concrete: `sanitize_fragment`
  (`crates/runtime/src/security/redaction.rs:310-325#sanitize_fragment`) drops `Thinking` and
  `Secret` fragments outright and returns them as `None` (lines 322-324 of that declaration). A
  replay would feed back a record with holes where the agent's own reasoning was.
* **A strictly worse mechanism standing beside the real one gets used as though it were equivalent.**
  This is the failure the decision exists to prevent, and it is not hypothetical: it is what the
  one existing statement in this repository about the same boundary is already arguing against —
  [0028](0028-submit-prompt-is-journaled-redacted-run-intent.md) records, at lines 174-176, that a
  resume "would replay it as new conversation that already happened", and cites it there so the
  disproved reasoning is not resurrected once journaling makes it superficially plausible.
* **A record of everything is not a record that can restore anything.** The two properties are
  independent, and conflating them is the error.
* **Restore and restart are different operations** and must not be conflated in any interface,
  document, or report. Restart is a fresh assignment through the task brief; it is a neighbouring
  record and this one does not touch it.

## Considered Options

* **Restore is the vendor's own resume, against the vendor's own on-disk state, and nothing else.**
  Chosen.
* **A Crew-side record-replay path as a second way to restore.**
* **Record-replay only as an automatic fallback when the vendor's resume is unavailable.**
* **Vendor resume first, then replay only the tail the vendor missed.** `[AGENT-PROPOSED]`, raised
  here and not in the owner's ruling.
* **Leave it unrecorded**, on the ground that the code already works this way.

## Decision Outcome

### Decision 1 — restore is the vendor's own `--resume`, and only that

A vendor's own resume — or its SDK equivalent — against the vendor's own on-disk session state is
the **only** restore mechanism this project has or builds. No Crew-owned path replays journal content
into a vendor session to reconstruct context. `[USER-STATED]` for the ruling; `[AGENT-PROPOSED]` for
the scoping below.

**What the journal's role in a restore is: bookkeeping.** The only journal-adjacent value a resume
reads is the transcript cursor, and only to know where to resume tailing. **No code path reads
journal event text and hands it to a vendor adapter as prompt content.** `[VERIFIED]` for the resume
path end to end, as cited above.

**A run with no vendor session is not restored.** It is settled terminal by the existing fallback,
or it is restarted — and a restart is a *fresh assignment through the task brief*, not a replay.
`ResumeCause` (`crates/protocol/src/event.rs:383#ResumeCause`) carries no restart variant, which is
the mechanical form of that separation in this repository's own types today. `[VERIFIED]`

### Decision 2 — restore and restart are different operations

**Restore** continues one existing run on one existing vendor session. **Restart** creates a
*different* run with a *different* session, fed the task fresh, and deliberately **without**
`--resume`. A passing restore says nothing about a restart, and this record may not be cited as
evidence that either one works. `[USER-STATED]` for the distinction; `[VERIFIED]` for the two entry
points being distinct (`resume_run` and `run_retry`).

### Why record-replay is not a restore — three reasons, each verified

The owner's ruling carried three reasons together. Each is checkable, and each is checked.

**(a) The journal is redacted before durability, so a replay loses exactly what a real resume keeps.**
`[VERIFIED]` — `AGENTS.md:336` states the invariant; `sanitize_fragment`
(`crates/runtime/src/security/redaction.rs:310-325#sanitize_fragment`) is its implementation, and it
*discards* `Thinking` and `Secret` fragments rather than masking them (lines 322-324 of that
declaration). The vendor's own resume keeps the model's hidden reasoning in the vendor's own state;
a replay of Crew's journal would present the agent's own interior as absent.

**(b) The journal carries no tool state and no vendor checkpoints.** `[AGENT-PROPOSED]`, resting on
the owner's recorded reasoning. The `runs` row holds a `vendor_session_id` and a
`transcript_cursor` (`crates/runtime/src/db/migrations.rs:42-107#MIGRATION_2`, the `runs` table's two
columns at lines 76-77 of that declaration) — positions, not checkpoints — and the event journal is
an append-only stream of events, not a snapshot of any vendor's internal state. **What would settle
it:** a demonstration that a replay can reconstruct a tool call's result or a vendor checkpoint. No
such demonstration exists, and the shape of what is stored says it could not.

**(c) Replayed text arrives as new user turns, not as the agent's own history.** `[VERIFIED]` for
the mechanism, from the same boundary ADR-0028 states at lines 174-176: text handed to a vendor
arrives as input, and input is not memory. The vendor's model has no way to distinguish "I wrote
this" from "I was told this", because by then the difference is gone.

**Hence: record-replay is a lossy fallback, not a restore.** It is not a slower restore. It is a
different thing wearing restore's name.

### What restore is actually demonstrated to do — carried inside the record

**This record decides *what restore is*, not *that restore works*. Both statements are needed, and
the second is a standing gap.**

**Daemon-restart survival is "Not demonstrated" for all four vendors.** The central evidence row
covers survival of a worker session across a full `crewd` restart, and its status is **SKIPPED** —
"Unit-tested. Never empirically demonstrated" — for Copilot exactly as for Claude, Codex, and OMP.
Nothing in this record changes that, and no document may read this record as though it did.
`[VERIFIED]` against the owner's proof record of 2026-09-28 and `2962c80`, and independently visible
in this repository: `docs/compatibility.md:197` records `session_resume` as **skipped** on every
adapter, with the reason stated — "a single-process resume is not a daemon restart". (That line is
prose in a Markdown document rather than a declaration, and is cited by path and line with the
sentence quoted, per the discipline this repository's citation rule requires.)

**The Copilot cross-process case is PROVEN, on the headless `-p` path only, with three inseparable
caveats.** A genuinely fresh process located a persisted Copilot session and a planted-token control
came back with the token, which is a real result and the first of its kind. It carries, inseparably:

1. **Headless `-p` mode, not the PTY path.** Same argv token, different surface; the TUI resume is
   **not** covered by the result.
2. **`--allow-all-tools` was an uncontrolled variable.** No tools were invoked, so it is probably
   immaterial — it was not controlled, and is said so rather than argued away.
3. **It was not a `crewd` restart.** A fresh process is not a restarted daemon.

**The word "survives" must not appear near this result without these caveats attached.** It is the
single easiest way for this row to be re-reported as a restart demonstration, and this record's own
subject — restore — is exactly the thing that would be misread. `[VERIFIED]` against the owner's
proof record; the caveats are stated there in the same terms and are reproduced here rather than
paraphrased, because a paraphrase is how they get separated.

### Positive Consequences

* One restore path, which is the one the code already has. **The decision costs no construction.**
* No lossy path can be mistaken for a restore, because no such path exists.
* The redaction boundary stays one-way: nothing masked on the way in is ever sent back as history.
* Every future proposal gets one question to answer: *does this feed recorded text back to a vendor as
  that vendor's own past?* A yes is a refusal to build, whatever else it achieves.
* The record cannot be misread as a claim that restore works, because it says so in its own text.

### Negative Consequences

* **Restore is exactly as good as the vendor's own resume, and for daemon-restart survival that is
  undemonstrated for every vendor.** Recording this decision does not move that row.
* **There is no safety net.** A vendor whose resume fails, or a run with no session id, gets no
  restoration; the existing behaviour is to settle the run terminal, which loses the agent's context
  entirely. The owner accepted that by choosing this path.
* **The enforcing check does not exist.** A scan that fails the build when a journal-to-adapter
  content flow is added is the natural enforcement of Decision 1, and it is unbuilt. Until it exists
  the prohibition is held by this record and by review.
* **Whether any such flow exists today was not exhaustively checked here** — the resume path above
  was read end to end and carries none, and that is the claim being made. `[VERIFIED]` for the resume
  path; `UNVERIFIED` for a whole-tree absence.
* A run whose vendor session was never established is unrecoverable by restore, and the owner has
  not said what should happen to it instead. That is open question 2.

## Pros and Cons of the Options (rejected alternatives, with reasons)

* **A Crew-side record-replay path as a second restore.** **Rejected by the owner** `[USER-STATED]`,
  on the three reasons above, verified or labelled above as each was checked. Building it would let
  a strictly worse mechanism stand beside the real one and invite the exact conflation this record
  exists to prevent.
* **Record-replay only as an automatic fallback.** **Rejected** `[AGENT-PROPOSED]`, on the same
  reasons and with one addition. A fallback that activates by itself is the case where the
  difference between a restore and a lossy imitation matters most, because **nobody chose it**. The
  run would silently get a worse answer than the one it would have got otherwise, with nothing to
  mark the substitution.
* **Vendor resume, then replay only the tail the vendor missed.** **Rejected** `[AGENT-PROPOSED]`.
  Knowing where the vendor's own record ended is a fact only the vendor's own transcript holds; the
  journal's cursor says where to *watch*, not what the vendor *remembers*. The replayed tail would be
  masked text arriving as new user turns — reason (c) again, with less of the context.
* **Leave it unrecorded.** **Rejected.** The code already behaves this way, which is the strongest
  argument for writing it down rather than the weakest: a behaviour held only by an implementation
  detail is a behaviour the next reader has to re-derive, and the wrong framing — that the journal
  could re-feed an agent — is one this project has had to correct before.
* **Make the journal complete enough to replay from.** **Rejected** `[AGENT-PROPOSED]`, and it fails
  on this repository's own invariant rather than on taste: the redactor *discards* rather than masks,
  so no amount of journal growth recovers what was dropped before durability.

## The open questions this record leaves visible

1. **How far does "never fed back to a vendor" reach?** Reason (c) is that replayed text arrives as
   new user turns — which is also exactly how a relay hop and a re-issued task brief arrive:
   recorded, redacted text delivered as fresh input. The prohibition can only mean *presenting
   recorded turns as an agent's own history to reconstruct its context*, not *delivering new
   instructions*. **This record scopes it that way, `[AGENT-PROPOSED]`, and the owner has not said
   so.** What would settle it: the owner confirming the scope, so that an enforcing check is not
   written so wide it forbids the relay and the brief re-issue, or so narrow it misses a real replay.
2. **Is settling a run terminal an acceptable answer when it cannot be resumed?** It is what the code
   does. The owner has ruled there is no second restore path; the owner has **not** said what should
   happen instead, and "nothing" is currently the answer.
3. **Is the scan that enforces Decision 1 part of this decision or separate work?** It is unbuilt
   either way, and until it is built the decision is held by review alone.
4. **Does this record carry the evidence caveats in its own text, or leave evidence to the release
   gate?** It carries them. That was the author's choice and it is reversible, and the risk of the
   alternative — a promoted record read as a claim that restore works — is larger than the risk of
   the redundancy.

## Links

* Consistent with, and stating at record level, what
  [0028](0028-submit-prompt-is-journaled-redacted-run-intent.md) says in passing at lines 174-176.
  0028 is read here and **not edited**.
* [0006](0006-type-enforced-redaction-boundary.md) — the redaction boundary this record's reason (a)
  depends on; its invariant is stated at `AGENTS.md:336`.
* **Neighbouring records this one must not be read as touching:** the record on the task brief, under
  which restart is a fresh assignment with no `--resume`; and the Copilot SDK record, whose subject
  is how Copilot is *driven*, not what restore is.
* Verification basis: every `path:line#Symbol` above was opened at commit `2962c80` and the named
  symbol was seen declared at that line or inside that range. A line inside a cited declaration is
  named in prose as "line N of" that declaration, and a line in a Markdown document or a comment
  block is cited by path and line with the sentence quoted, because neither has a declaration to
  anchor to. Claims resting on the owner's proof record carry no `file:line`, because that record is
  not in this repository.
