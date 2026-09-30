# A citation names its symbol, and a checker asserts the symbol is declared at the cited line

* Status: Proposed
* Date: 2026-09-30
* Supersedes: *(none — no existing record states a citation policy; this adds one beside the two checks already in the repository and changes no existing rule)*
* Amends: *(none)*
* Numbering: `0055`. `docs/adr/` holds `0001`–`0032` and `0034`–`0038`; `0033` is reserved and deliberately unused (`docs/adr/README.md:13-17`); `0044` is deliberately skipped; `0039`–`0043`, `0045` and `0046` are claimed by other records. `0055` is the next number claimed by nothing. `0052` and `0054` are free and are deliberately left as gaps: the four records being written together were numbered as a block, and two of them were moved onto numbers their subjects had already reserved. **⚠ The owner's planning notes once claimed `0049` for a draft of this exact subject** — see "Why this is not `0049`".

> **⚠ THIS RECORD IS A DRAFT AND AWAITS THE OWNER'S RATIFICATION.** The owner's ruling is settled;
> the syntax, the checker, and the rot policy are **not**, and are marked `[AGENT-PROPOSED]`
> throughout. **The checker this record describes does not exist**, and the owner's own requirement —
> that the checker be shown capable of failing — cannot be met until it is built. Ratifying it means
> resolving the named open questions, changing this record's own `Status:` line to `Accepted`, and
> letting the index row follow.

## Context and Problem Statement

A citation in this repository can be wrong in two ways, and **a check that only asks whether the
cited line exists catches neither.**

**The fictional symbol.** A citation names an identifier that was never declared. There is no line
to check, so a line-existence check has nothing to fail on — the number resolves, the file exists,
and the reader who follows it finds nothing.

**The real line that declares something else.** A citation names a line that genuinely exists and
genuinely declares a real thing, which is not the thing the sentence claimed. This is the more
dangerous of the two, because the citation *looks* perfect: it opens, it points at code, and the code
is real.

**This project has already made and refuted one of each.** One recorded citation named a run state
that does not exist — there is no such declaration anywhere in this repository, so no line-number
check could ever have caught it. Another pointed at a **real** line that declared something
unrelated, and that is the one worth generalising from: it passed every check a line-existence gate
can make, and the mistake was only findable by a reader who opened the file and read it.

**The wrong citation is not always a number.** Both real defects this record generalises from were
citations that landed *inside* something rather than *at* a declaration — one into a test body where
the claim belonged to the function under test, one at a neighbouring arm of a `match` where the claim
belonged to the enclosing `fn`. A citation that names the symbol and asserts the declaration is at
that line refuses both, because a body line does not declare the symbol the sentence is about.

**What the repository already has, and it is the right shape.** `check-adr-status` reads a structured
source — the index table, via `parseIndex`
(`scripts/check-adr-status.ts:140-166#parseIndex`) — and a target, the record's own status, via
`parseRecordStatus` (`scripts/check-adr-status.ts:168-185#parseRecordStatus`) — and then asserts that
the thing it found **agrees with the thing it expected**, per row, in `checkRow`
(`scripts/check-adr-status.ts:190-209#checkRow`). That is the shape a citation checker takes: parse a
structured thing, resolve a named target, compare, and exit non-zero on any disagreement. It exits
non-zero in the `import.meta.main` block
(`scripts/check-adr-status.ts:228-238`, the `process.exit(1)` at line 232 of that block). It is
recorded here as the **existing precedent for the form**, not as something it already does.

**The repository's own rule about trusting a checker, which this record inherits.** A checker that
reads nothing reports success in exactly the way a clean repository does. `checkStatus`
(`scripts/check-adr-status.ts:211-220#checkStatus`) therefore *throws* when the index yields no rows
at all — the `if (rows.length === 0) throw new Error(...)` at line 218 of that declaration, whose
message asks whether the table is still a table. The header comment says why at lines 37-43 of that
file (a comment block, cited by path and line and quoted, because it has no declaration to anchor
to). `scripts/check-markers.ts` states the same principle at lines 31-50 of its own header, and both
checks carry a **positive control** in their test files: `check-markers.test.ts` holds text every
rule must catch (the `mustCatch` table at
`scripts/check-markers.test.ts:21-30#describe("positive control: every rule catches its own marker")`),
and `check-adr-status.test.ts` holds both catch-these and leave-these-alone controls.

**Today's cost is small.** A search of tracked Markdown for `path:line` references finds a
handful outside `release/` evidence files, and **not one of them is symbol-anchored**. `[VERIFIED]`
by a pattern search at `2962c80`; references written some other way were not counted, and the count
is not a claim about how many citations exist by any other spelling.

## Decision Drivers

* **The owner's requirement, stated as what the check must catch**: a fictional symbol, and a real
  line that declares something else. `[USER-STATED]`
* **The owner chose the tool over continued manual fixing**, and said the change is a citation-format
  change across all documents *plus* a checker, not a checker alone. `[USER-STATED]`
* **A checker that finds nothing must not read as a clean pass.** This repository has already been
  burned by that, four separate times by its own account, and states the lesson in two file headers.
* **A check that fails on correct content gets deleted rather than fixed.** That is why
  `check-adr-status` compares only the leading status word and its own header says so — demanding a
  whole-cell match would fail on every amended and superseded record.
* **Records are write-once**, so a new rule may not require editing an accepted record to comply.

## Considered Options

* **Symbol-anchored citations, with a checker that resolves each one.** Chosen.
* **Line-existence checking only.** **Rejected by the owner** `[USER-STATED]`: it catches neither of
  the two recorded failures.
* **Continue fixing citations by hand.** **Rejected by the owner** `[USER-STATED]`.
* **Citation by symbol with no line** (`path#Symbol`), which cannot rot. Not chosen, because the
  owner's wording is that the symbol is "declared at that line" — the line is part of the claim.
* **Citation pinned to a commit** (`path@sha:line#Symbol`), resolved with `git show`. `[AGENT-PROPOSED]`,
  raised here and not in the owner's ruling.

## Decision Outcome

### Decision 1 — a citation names its symbol, and the checker asserts the symbol is declared at the cited line

**A citation carries the symbol, not only the position.** A single-line declaration is cited as
`path:line#Symbol`; a multi-line one as `path:start-end#Symbol`, where `Symbol` is the identifier as
it appears in the declaration. **The symbol must be *declared* at the cited line, or somewhere inside
the cited range.** A *use* is not a declaration, and a line inside a body is not a declaration of the
thing a sentence is about — which is exactly how the two defects already made in this project got
made. `[USER-STATED]` for "names the symbol, and the checker asserts it is declared at that line".

**This is the requirement, and it is the owner's.** Everything below is `[AGENT-PROPOSED]`.

### Decision 2 — the checker follows `check-adr-status`'s shape

Parse, resolve, compare, exit non-zero on any disagreement, and **refuse to report success on an empty
read**. The precedent is cited above and is a real, working check in this repository, not a shape
someone imagined.

**Its own file's stated caution carries over and needs an answer.** `checkStatus` throws on zero
rows because the index table structurally has rows. **A citation checker has no such structural
guarantee**: today not one citation in tracked Markdown is anchored, so a checker that threw on zero
citations would fail on day one, and one that did not would report success on a scan that read
nothing. It has to throw on zero *documents in scope* and be given a fixture citation as its positive
control instead. `[AGENT-PROPOSED]` — the difference is real and is not addressed by "follow the
identical shape".

### Decision 3 — the checker is shown capable of failing

The check is run against a deliberately wrong symbol and must exit non-zero. **A checker that passes
everything is a failed check**, and this repository already holds that principle in executable form in
both of its existing tests. `[USER-STATED]` for the requirement; the checker does not exist, so the
requirement is recorded and undischarged.

### Decision 4 — the existing marker guard is unaffected

The "bare pull-request number" rule requires a digit immediately after the `#`
(`scripts/check-markers.ts:64-96#RULES`, that rule at line 82 of the array), so `path:line#Symbol`
does not match it. `[VERIFIED]` by running the rule's own pattern against the proposed syntax: it
matches a hash followed by one to four digits, and it does **not** match
`registry.rs:516-521#resume_run`, does not match `path:1#Symbol`, does not match a hash followed by
five digits, and does not match the same citation at all once the digit is replaced by an
identifier. **The proposed form introduces no new marker.** (The two literal cases this record
exercises are written out here in words rather than reproduced, because reproducing them would be
the marker the rule forbids — which is the rule working, not the record failing.)

### The gap found today: this gate does not check titles

**`check-adr-status` compares Status cells only, never Title cells — so a title drift between the
index and the record is ungated.** `[VERIFIED]`, and this is a gap **in the gate, not a defect in
it**: the check does precisely what its own header says it does — the leading lifecycle word — and
comparing more would be the "fails on correct content" case its header argues against. Title drift is
outside its stated scope.

**It is nonetheless ungated, and this is how a title row went unnoticed.** The row for `0038` at
`docs/adr/README.md:82` carries a Title cell that does **not** match that record's own first line
("A one-turn worker's exit is the run's terminal event; the lifecycle must model exit, not hold",
`docs/adr/0038-one-turn-worker-lifecycle.md:1`). Both lines are prose in Markdown files and are cited
by path and line, because neither has a declaration to anchor to. **The gate passes on this** — and
does so for the right reason: `checkStatus` returns zero findings on the real index today, verified by
running it at this commit. A separate uncommitted worktree exists whose only change is repairing that
title cell, which is how the drift was found: **by a reader, not by a check.**

**The mechanism, precisely.** `parseIndex` reads a row's status from `parsed[2]`
(`scripts/check-adr-status.ts:140-166#parseIndex`, that assignment at line 162 of the declaration) and
its `Row` type carries five fields — `line`, `id`, `raw`, `target`, `status`
(`scripts/check-adr-status.ts:104-116#Row`) — of which **none is a title.** Reading a parsed row back
at this commit confirms the shape exactly: the title cell is parsed and discarded. `[VERIFIED]`
both by reading the source and by observing the parsed value.

**So the two gates cover two different cells, and neither covers the pair together.** Status drift is
gated; title drift is not; and the two are read by the same reader scanning the same table. Closing
this is filed as work below, and **is not part of this decision** — it is a separate change to a
separate check, and folding it in would make this record's ratification carry an unrelated
modification to a working gate.

### Positive Consequences

* **Both recorded failures become mechanically impossible to commit.** A fictional symbol has no
  declaration to match; a real line declaring something else fails the comparison; and a citation into
  a body rather than at a declaration fails for the same reason the second one did.
* The check reuses a shape this repository already trusts, including its refusal to report success on
  an empty read and its positive-control discipline.
* The cost in this repository today is small — a handful of citations outside `release/`.
* A citation carries what a reader needs in order to verify it, without opening the file: the
  declaration itself, at the line.

### Negative Consequences

* **Line numbers rot.** Any edit above a cited declaration moves it, and the check then fails on a
  change that never touched the document. That is the price of the owner's "at that line"; the
  alternatives each give something up (open question 2).
* **The zero-match rule does not carry over unchanged**, and the difference is a day-one failure or a
  day-one false all-clear, depending on which way it is resolved. See Decision 2.
* **Accepted records cannot be migrated.** An accepted ADR is never edited
  (`docs/adr/README.md:3-6`), and `docs/adr/0038-one-turn-worker-lifecycle.md:23` carries one bare
  `file:line` citation whose symbol is named in the surrounding prose. The checker must grandfather
  existing records explicitly, or it fails on a record nobody may fix. The index already treats
  citations left as written as a deliberate precedent.
* **Some targets have no declaration to anchor to** — a Markdown record's sentence, a file-header
  comment, an SQL `CREATE TABLE` inside a Rust string constant. The form has to say what such a
  citation looks like, or they stay outside the check while being exactly the citations most likely
  to drift.
* **A declaration matcher is real code to maintain**, and a pattern-based one can be fooled by a
  comment or a string containing a declaration-shaped line. The choice between a syntax-aware matcher
  and patterns is open.
* **The checker does not exist**, so this record describes intent and the requirement that it be
  proved capable of failing cannot yet be discharged.

## Pros and Cons of the Options (rejected alternatives, with reasons)

* **Line-existence only.** **Rejected by the owner** `[USER-STATED]`: it catches neither of the two
  recorded failures — nothing to fail on for a fictional symbol, and a real wrong line is real.
* **Continue fixing citations by hand.** **Rejected by the owner** `[USER-STATED]`: the tool was
  chosen over continued manual fixing, and manual fixing is what produced both defects.
* **Symbol only, no line.** **Not chosen**, because it departs from the ruling: the owner said the
  symbol is declared "at that line", so the line is part of the claim. It would also remove the rot
  problem entirely, which is a reason the owner may want to reconsider the ruling — and that is the
  owner's call, not this record's. `[AGENT-PROPOSED]` observation.
* **Pin the citation to a commit.** **Not rejected and not weighed** `[AGENT-PROPOSED]`. It removes
  rot and matches how the owner's own notes state their basis ("verified against commit `2962c80`").
  Against it: it cannot verify a citation against the tree the reader is actually looking at, and it
  puts a commit id in every citation.
* **The symbol named in nearby prose, as most citations are today.** **Rejected by the owner**
  `[USER-STATED]` as the failure itself: many citations are bare `path:line` with the symbol left to
  the surrounding prose rather than anchored in the same clause. That is why the format change and
  the checker travel together — a checker cannot resolve an anchor the format does not carry.

## The open questions this record leaves visible

1. **Which syntax?** The owner's working form is `path:line` followed by the declaration text
   verbatim (`registry.rs:877 async fn watch_settlement`), which verifies the whole declaration; the
   design's form is `path:line#Symbol`, which verifies one identifier against it. **The owner's form
   and the proposed form differ and the owner has not chosen between them.** What would settle it:
   the owner choosing, or accepting the proposed one.
2. **What is the rot policy?** Check against the current tree, which fails when an unrelated edit
   moves a declaration; pin to a commit; or let the symbol be authoritative and the line advisory —
   which departs from "at that line". Three options, none chosen.
3. **What does the check cover, and where does it live?** The owner's ruling was about the planning
   notes' own citations, and those are **outside this repository**: a script here cannot scan them
   and CI cannot see them. Inside the repository the check would cover Markdown and possibly code
   comments, with accepted records grandfathered and `release/` evidence files presumably exempt.
   **If the owner's aim was the planning notes, a record in this repository is the wrong home for
   that half of the decision.** What would settle it: the owner stating the scope.
4. **How is "declared" decided for a target that is not Rust?** SQL inside a Rust string constant
   (`MIGRATION_2`, `crates/runtime/src/db/migrations.rs:42-107#MIGRATION_2`, whose `tasks` table is
   declared at line 50 of that constant and whose `runs` table at line 65), TypeScript, Markdown
   prose, and comment blocks. The proposed data shape for a neighbouring record cites the SQL one,
   so this is a live question, not a hypothetical. `[AGENT-PROPOSED]`
5. **Is the record written before the checker exists?** Promoting it now records intent. The
   requirement that the checker be proved able to fail cannot be met until it is built, so a promoted
   record carries an undischarged requirement — stated here rather than left for a reader to
   discover.

## Work filed by this record

1. **The checker itself**: resolve every `path:line#Symbol` in the documents in scope, fail when the
   named symbol is not declared at the cited line or range, throw on zero documents in scope, and
   carry a positive control. **It must be proved capable of failing** before this record's
   requirement is met.
2. **The title-cell check, filed as separate work and deliberately not part of this decision.**
   `check-adr-status` reads no title cell (`Row` carries none —
   `scripts/check-adr-status.ts:104-116#Row`), so index/record title drift is ungated, and the `0038`
   row is drifted today (`docs/adr/README.md:82` against
   `docs/adr/0038-one-turn-worker-lifecycle.md:1`). **A new check, not a widening of the existing
   one**: `check-adr-status` compares the leading status word by design, and widening it would make
   it fail on the amended and superseded rows its own header defends. Whether the fix is a second
   checker or a third compared cell is itself undecided.

### Why this is not `0049`

The owner's planning notes claim `0049` for a draft of this exact subject, and that draft's own
numbering section records the numbers as provisional and states that the sequence must be decided
before any one draft is promoted. `0049` is therefore claimed. This record does not take it, and the
owner should decide the sequence across all four of these drafts before any lands.

## Links

* `scripts/check-adr-status.ts` — the precedent shape, cited above with each declaration anchored.
* `scripts/check-markers.ts` — the sibling check whose zero-match rule and positive control are
  inherited.
* `AGENTS.md:259` — the existing citation rule ("A citation must point at something that survives"),
  and `AGENTS.md:151-154`, which states the same constraint for comments. **This record adds a form
  to that rule; it does not change what may be cited.**
* `docs/adr/README.md:3-6` — the write-once rule that forces accepted records to be grandfathered
  rather than migrated.
* Verification basis: every `path:line#Symbol` above was opened at commit `2962c80` and the named
  symbol was seen declared at that line or inside that range. A line inside a cited declaration is
  named in prose as "line N of" that declaration; a line in a Markdown document or a comment block is
  cited by path and line with the sentence quoted, because neither has a declaration to anchor to.
  The marker-guard behaviour in Decision 4 was observed by running that rule's own pattern against
  the proposed syntax.
