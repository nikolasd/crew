# Drive workers over the vendors' own protocols; the terminal becomes a view

* Status: Accepted
* Date: 2026-09-14
* Supersedes: [0026](0026-headless-retirement.md)
* Amends: [0025](0025-crew-v2-tui-control-plane.md), [0027](0027-turn-end-settles-a-run.md)

## Context and Problem Statement

Four live end-to-end attempts stopped on the terminal-recognition control plane. The pattern across
them: **every failure of the control plane itself was a property of reading a screen** — per-word
cursor positioning that broke phrase matching, a composer placeholder that vanishes once the box is
filled, pane-size garbling, and a "grid untrustworthy" latch checked before gate phrases, so a real
sign-in dialog became invisible and the run died as unreadable. Not one bug; a control plane whose
failure modes belong to its medium.

*Stated at that width deliberately.* Attempts also stopped on things that were **not** screen
reading — a digest arriving after the leader's turn, the model-ask list — both extension-side. The
claim is not that nothing else ever stopped a run; it is that the control plane's own failures were
all of one kind.

**A third kind has since been named, and it is the one this document should be read against.** The
defects found by *running* the protocol path were neither screen drift nor protocol drift but
**crew's own model of what a worker is**: a control plane selected by a default nobody stated, a run
lifecycle counting a turn boundary only a long-lived worker emits, a trust check keyed on the
directory crew leased rather than the one the vendor keys on, and a teardown window that reaps a
worker the vendor may legitimately still be using. Screen drift is visible in a capture and protocol
drift is visible in a schema; an unstated assumption about what a worker is produces artifacts that
only disagree once someone holds the assumption up beside them. The run-state documentation and
the run-state code independently encoded the same wrong premise — that a clean exit means success
— so comparing them showed *agreement* while both were wrong. The document has since been
corrected and the code defect was carded separately; a reader who checks today finds the corrected
text, which is why the example is stated here rather than cited.

**That class is not caught by the detection machinery the rest of this decision builds. It is
caught by execution, and it is named here because naming is the only defence available.**

History, stated plainly: every adapter once had a headless implementation over these same protocols.
ADR-0025 made the TUI path primary because **a visible pane is evidence**. ADR-0026 deleted the
headless adapters. This decision reverses that, so it must say what happens to ADR-0025's reasoning
rather than quietly dropping it (decision 7).

## Decision

**Drive workers over the vendors' own protocols. The terminal becomes a view, not the control
surface.**

Per vendor: claude `-p --input-format stream-json --output-format stream-json` plus its control
channel; codex `app-server` on the shared local daemon; copilot `--headless --stdio` once current
releases admit it; omp RPC as today.

**Evidence is marked per vendor throughout, and only one vendor has any.** A claude adapter has been
built and run live. Codex, copilot and omp are **inferred, unverified**. One vendor's proof is not
three vendors' evidence, and a posture with no demonstrated form is an assumption wearing a
decision's clothes.

## What the proof run established

The run was closed deliberately after row 5 (2026-09-14) once it became clear that each further
launch was returning a finding about crew's orchestration rather than about the thing the row
measured. **Nothing further is coming from launches.** The state below is what this decision rests on.

| Row | Claim | State (last moved) |
|---|---|---|
| 1 | the model ask fires; the chosen model reaches the launch | **Passes** (2026-09-13) — argv read from the live process carried `--model claude-sonnet-5`, *contradicting* the operator's own `opus` default, and `profile.model` is the only source on this path |
| 2 | the result returns to omp under crew's own label | **Send and content proven; render pending** (2026-09-13) — leader-received is not human-saw |
| 3 | reconciliation detects a dropped event | **Clean leg failed; detection leg unrunnable live** (2026-09-13) — the drop-injection seam is absent from a release build by construction, so detection rests on a unit test and was never exercised against a real turn. The clean leg's failure was a wrong baseline, not a wrong mechanism — see decision 2 |
| 4 | one approval round trip produces a durable ledger entry | **Passes, forced** (2026-09-13) — proves the mechanism works; row 10A shows it is not exercised in the shipped posture |
| 5 | the three-way reconciliation's second leg; sentinel detection | **Passes** (2026-09-13); the detector question closed — see decision 8 |
| 6 | the trust pre-check refuses, and proceeds once trusted | **Not run** |
| 7 | the launch line, flag by flag, pinned or inherited | **Not run** (documentation pass) |
| 8 | bounded settle | **Inconclusive** (2026-09-13) — see decision 8's settle note |
| 9 | the posture is observed at `system/init` | **Passes** (2026-09-13) — the gate is proven to have *executed*, not inferred from the absence of an abort |
| 10A | does a natural turn under the pinned posture consult crew at all | **Ran; negative** (2026-09-13) — see decision 3 |
| 10B | an allow rule makes the frame disappear | **Not run** — conditional on 10A firing, which it did not |

**The states are perishable and the column says when each last moved.** Rows 6 and 7 never ran.

## Decision points

### Decision 1 — Control over protocols; the terminal is a view

Crew drives the worker over the vendor's own protocol and renders a pane for a human to watch. The
pane is crew's self-report, not the control surface and not evidence (decision 7).

### Decision 2 — The transcript is an independent audit of the journal's completeness, not a correctness mechanism

**Amended from the drafting assumption, on measurement.** Reconciliation runs *after* the turn has
ended, is best-effort, never propagates a failure, and does not repair a gap it finds — it detects
only. Nothing crew decides is downstream of it. It is **audit**, and the earlier wording
("reconciles against the file for correctness") misled its own authors: during the run, three
readers inferred three different wrong causes from a reconciliation number in minutes.

**And the premise underneath it was wrong.** A clean turn reported two gaps, three times, while the
examined count varied — a constant count against a varying turn size is structural, not loss. Both
were identified: a `system/stop_hook_summary` entry the vendor's hook machinery writes and the
stream never carries, and **crew's own injected prompt**, whose transcript id is structurally
unobservable because crew writes it to stdin and the first user turn is never echoed back.

**So the transcript is a superset of the stream by construction.** "Gap" is reserved for entries the
stream should have carried. Hook output and the injected prompt are transcript-only **by category,
not by count** — the count depends on the operator's hook configuration and must never be subtracted
as a constant. Unrecognised entry kinds are reported as **unclassified**, a third outcome distinct
from both "gap" and "excluded", so a new vendor kind surfaces as *we do not know what this is*
rather than as a phantom gap or a hidden one.

**Detection itself has never been exercised against a real turn.** The drop-injection seam is
`#[cfg(test)]` and absent from a release build by construction — a property worth keeping — so the
live run could only ever provide the clean leg, and detection rests on a unit test. **Repair does
not exist at all** and both are listed under what this decision newly owes.

### Decision 3 — The ledger records every approval claude *raised*, never every action the worker took

**Measured, and this is the decision's most consequential finding.** Under crew's pinned
`--permission-mode auto`, a compound tool call whose mutating half created a file ran with **no
consultation at all** — zero approval rows, no permission payload anywhere in the run's journal — on
a machine whose operator's own configured default is `plan`. Crew's pin is what makes this true: the
operator chose a posture that agrees to nothing, and crew replaced it.

**The maintainer decided, with that measurement in hand, to keep `auto`.** The trade is therefore
knowingly accepted and is recorded as decided, not as a gap:

> **Under the shipped posture, authorization is the posture, not the ledger. The ledger records what
> claude raises.**

**What makes that sound rather than merely accepted:** of the three permission values crew pins,
**exactly one is verifiable at `system/init`** — `permissionMode`, asserted by the posture gate and
verified live — and it is the one now carrying the authorization. The two unverifiable pins govern
the **record**, so their silent failure costs a record and not a control.

**Revisit trigger:** *if the ledger ever becomes
load-bearing for authorization — if crew is ever to deny something, or the ledger is to be the record
of record — the sentinel's unverifiability becomes a first-order problem and this decision must be
reopened.*

Consequently **`permissionMode` is a security control**, and a change to that pin is a security
change, not a configuration tweak.

### Decision 4 — Accept codex's approval fan-out; record who resolved it

*(inferred, unverified — no codex worker has been run)* Crew records the resolution source, and
distinguishes **crew said yes and lost the race** (ordinary) from **crew said no and was overridden**
(crew's policy defeated). The second is its own ledger state; collapsing them merges a normal outcome
with a policy defeat.

### Decision 5 — Codex directory trust: ask through crew's own approval path

*(inferred, unverified.)* Bounded honestly: the fail-closed trust check governs a thread crew starts.
A thread started by an earlier daemon, or by a human at the vendor's own resume, runs under nobody's
pre-check.

### Decision 6 — Plain `-p` with a trust pre-check

Crew reads the vendor's own trust record before spawning and **never writes it**. An untrusted
workspace fails the run outright rather than parking: protocol mode has no pane a human could answer
on, and parking would mean an unbounded poll of a file that may never change.

**Documented by composition, not by a quoted guarantee:** committed allow rules are gated on
workspace trust rather than on `-p`; no single sentence states this for `-p`, and the composition of
two passages does.

**Keying, measured:** the vendor keys trust on the **git repository root**, and from inside a
worktree on the **main checkout's** root. Crew's own check is currently handed the **leased workspace
path** instead, so every `isolated` or `copy` run is refused before spawning with remediation the
operator cannot satisfy. **That is a known defect, carded, and it means this decision is demonstrated
only in `shared` mode.** The mode it breaks is the one that makes parallel agents safe.

### Decision 7 — What "evidence" means now, and what happens to ADR-0025

ADR-0025 made the TUI path primary because a visible pane is evidence. **This decision amends that**:
the pane becomes crew's self-report, and the audit path is the vendor's own durable transcript.

**The record distinguishes crew from not-crew, not who.** A human at the pane, a second daemon and
another tool on the same daemon are one bucket. The earlier drafting claim that "the record always
says who did" over-claims and is withdrawn.

### Decision 8 — Never inherit a vendor default we did not set, bounded to what is load-bearing

Crew passes anything load-bearing explicitly rather than inheriting it. A live capture showed that
without an explicit `--permission-mode` the effective mode is inherited silently from the operator's
settings, including a value that would make approvals never reach crew at all.

**The corollary needs three branches, not two.** Pass it explicitly; or, where no explicit form
exists, assert it at runtime and fail closed with a named reason; **or — a case the first two do not
cover — a value that is passed explicitly and still cannot be asserted.** For that case the rule is
neither *pin* nor *assert* but **declare**: name the value, name that nothing checks it, and name
what its silent failure would look like. A pinned value nobody can verify is safer than an inherited
one and is not the same as a controlled one.

**The claude prompt-tool sentinel is that case, and its silent failure is now measured.**
`system/init` reports `permissionMode`, the tool and server inventory, the model, the version and the
working directory — and reports **neither the permission rules nor the prompt tool**. So of the three
permission values crew pins, exactly one is checkable there. A proposed stream-side detector was
offered and **withdrawn on measurement**: a denial with no matching ledger request is produced
identically with the sentinel present (an operator's own deny rule) and absent (no host to ask), and
the discriminator lives in the operator's rule set rather than on the stream. **Under the shipped
posture a missing sentinel is indistinguishable from normal operation** — because normal operation
has no approvals either. That is tolerable **only** because authorization is the posture and not the
ledger (decision 3).

**Settle bound.** No documentation states that a `-p` worker exits promptly when stdin closes; the
documented waits include background work up to ten minutes and a thirty-second output drain, and both
first-party SDKs bound teardown themselves — one with the comment that a signal before the flush
loses the last assistant message. **So crew's bound is load-bearing by measurement and SDK precedent,
not by documentation**, and a hard kill at the deadline can truncate the very transcript decision 2
reconciles against. Two independent observations show a worker not exiting promptly on stream end.
**And crew's own teardown budget is currently shorter than the vendor's**: crew reaps seconds after
the stream ends while the operator's own hook timeout and the vendor's documented background wait are
far longer, and crew does not set the environment variable that would bound the vendor's wait. That
is two numbers from two places that nobody chose together.

### Decision 9 — `start()` means the worker is up

Two clauses, both binding on every adapter:

1. **`start()` returns once the worker is up** — the process running and ready to be driven — never
   once the run itself ends.
2. **The turn's lifetime, terminal event, failure handling and lease/workspace release are owned by a
   named component after `start()` returns**, not by `start()` or its caller.

**Why this is a decision and not a convention:** `run/submit` awaited `start()` synchronously, for
test determinism, on the unexamined assumption that `start()` meant *up*. For a one-turn worker it
means *finished*, so the submit call did not answer until the worker had lived and died — and because
the connection's dispatch loop is sequential and the extension's event chain awaits an RPC on that
same connection, **every event for that run queued until the worker exited.** Three individually
correct designs composed into a deadlock that belonged to no single file.

**Enforced today for the claude protocol adapter only**, by a red-then-green conformance check; the
terminal adapter's instance is asserted by inspection. A cross-adapter check is owed.

**Revisit condition:** a vendor whose bring-up and turn genuinely cannot be separated cannot satisfy
clause 1 as written and would need its own documented exception here, not a silent divergence.

**And one piece of crew's vocabulary changes meaning.** The turn-boundary event was defined in
ADR-0027 as the vendor having finished and being *held at its prompt* — a description of a living
terminal session. A protocol worker finishes its turn and exits, so the event keeps its name and
loses half its definition. This decision amends that meaning to *the vendor's turn reached its end*,
and records the larger question it does not settle: whether a run lifecycle built around a persistent
worker is the right model for a worker whose normal life is one turn. Deferred deliberately, and
named so the deferral is visible.

## Consequences

**Carried over unchanged:** the model ask (extension-side, before spawn), the herdr display backend
and pane lifecycle, the journal and its invariants, and the approval *service*.

**Changed, though an earlier draft of this decision listed them as untouched — each found by running
the spike rather than by reading it:**

- **workspace leases**, whose trust key is wrong under isolation and whose survival across a daemon
  restart is not what the caller asked for;
- **the run lifecycle itself**, which models a worker that finishes a turn and *holds*, and does not
  model one that finishes a turn and *exits*;
- **the omp-facing surface**, where the adapter mode is currently inherited from a default nobody
  states, so a leader following correct documentation selects the terminal path.

*That the first draft of this list said otherwise is the clearest measure of how much of this was
invisible until a worker was actually run.*

**Leases — a named dependency with no safety net.** No adapter's ordinary completion releases a
lease; release is the leader's explicit act, and that is deliberate, since the leader owns
apply-versus-discard. Leader-owned release is not the problem. What follows from it is.

The party that releases **sits further from the evidence than the daemon does** — it acts on the
state crew reports, not on the worker. **The dependency is therefore a leader that acts correctly on
terminal states, and this run falsified it once**: a leader read a completed run's state as failure,
resubmitted on its own, and never released.

**That dependency is unbacked in both directions.** Leader-gone settlement settles the run *state*
and releases no lease — its path touches no lease, release or workspace call at all. And the
stale-lease sweep cannot see the held lease either: it selects only a lease already in cleanup
failure, one whose path has disappeared, or an allocation older than its grace. **An `active` lease
whose path still exists is never stale, at any age**, so `crewd doctor` does not flag it and no
time-based sweep exists for it. A leader that leaves gets its run settled and its workspace held; a
leader that stays and is wrong gets neither.

**The cost is bounded by isolation kind, and the shipped default is the expensive one.** Only
`shared` isolation is exclusive within a project — worktree and copy workspaces never conflict — so a
leaked worktree or copy lease costs a directory. **A leaked shared write lease blocks every
subsequent shared lease in that repository** until someone runs `crewd lease release` by hand, which
is today's only recovery. The proof run ran in shared mode.

This decision's own up/run split also makes a failing run hold its lease where it previously
abandoned it, deliberately, so the two paths do not diverge.

**Whether that is acceptable is this decision's to weigh, not to assume.** It is recorded here as a
dependency with a known uncovered failure mode and a manual recovery, rather than as a defect,
because the design it depends on is deliberate and the analysis of what should back it is owed.

**Gained:** no screen models, gate classifiers, placeholder phrases or pane-size garbling; typed
approvals where the vendor raises them; first-run gates stop being a control-plane problem; drift
arrives as schema change rather than as pixels.

**Lost or newly owed:**

- crew owns a renderer for three of four vendors;
- independent human-checkable evidence, replaced by a machine-readable file (decision 2, decision 7);
- any external record of an approval *decision* for codex and claude (decision 3);
- **repair** — the audit detects a gap and does not repair it;
- **a shared correlation key** between crew's journal and the vendor's transcript; without one the
  audit can count discrepancies and cannot name them;
- **a reason on every terminal and reconciliation event** — during the run an unexplained failure and
  an unexplained gap count led three readers to three different wrong causes within minutes, which is
  the cost of an absent reason demonstrated on the people writing this document;
- **live delivery that is actually live** — a measured window during which the extension processes
  nothing, because all event processing is gated on a synchronous call to the connection that is
  busy; any slow handler blinds it, and bring-up is merely the slow handler we have;
- a silent trust side effect on codex (decision 5), and a billing decision on claude (decision 6);
- protocols that are experimental, undocumented outside SDK source, or hidden from `--help`.

## What would reverse this

Stated so this can be wrong in a detectable way: if reconciliation cannot be made to distinguish loss
from its own baseline; if the rendered pane proves not to be auditable evidence; or if the protocols'
experimental status produces breakage at a rate worse than screen drift did.

## Open, not settled

Rows 6 and 7 of the proof run never executed. Codex, copilot and omp have no live evidence of any
kind. The copilot questions this outline's drafting raised — whether a shell or write approval is
persisted to its own event file, and whether crew's cross-check reads that file rather than the
stream it already journaled — remain open and belong to a copilot spike. Each of the remaining
unknowns needs a live probe, authorised at source.
