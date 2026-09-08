# Display placement is the backend's natural form, and `Embedded` is deleted rather than deprecated

* Status: Accepted
* Date: 2026-08-31

## Context and Problem Statement

A run whose display preference omitted `placement` got `DisplayPlacement::Embedded`, and the default
was written down twice, independently, in two languages:

* `crates/runtime/src/service/orchestration.rs` built
  `DisplayPreference { ordered: vec![], placement: Embedded, launch_program: None }` at two call
  sites.
* `packages/extension/src/tools/shared.ts`'s `displayPreferenceFragment()` sent
  `{ ordered: [], placement: "embedded", launchProgram: hint }`, with a comment asserting these were
  "exactly the daemon's own defaults".

They agreed because the comment said so. Nothing checked it — the unguarded-duplicate class this
repository has been closing elsewhere, in the form where a comment stands in for a test.

The default was also wrong on its own terms. The intended semantics were already settled: panes
follow the host, so a tmux session gets splits, herdr gets panes, and a windowed host gets a tab.
`Embedded` implements none of those, and two backends refuse it outright —
`crates/runtime/src/display/herdr.rs` and `crates/runtime/src/display/tmux.rs` both reject it.

**That asymmetry is why the defect shipped and looked healthy.** Only those two refuse. The windowed
backend ignores the requested placement entirely and reports the placement it actually used, so a
run there behaved correctly whatever was asked for. A bare-terminal session got a pane and nothing
complained. Any test covering only the windowed backend passes straight through this bug, which is
why the guard tests target herdr and tmux specifically.

When the refusal did fire, the run silently fell back to a hidden display. The condition was
surfaced — a diagnostic reached the monitor — and then lost: `packages/extension/src/monitor/model.ts`
set `latestActivity`, and the next patch overwrote it, so the warning existed for milliseconds. It
could not be recovered afterwards either, because the monitor records only the outcome, and
`backend === "hidden"` is equally the legitimate deliberate choice for a headless run.
Requested-versus-actual was not representable.

So there were three questions: what the default should be and who resolves it, what to do about the
`Embedded` value already written into journals, and how a downgrade reaches someone who can act on
it.

## Decision Drivers

* The intended semantics were already decided; the default contradicted them. This is an
  implementation catching up with a decision, not a new policy.
* A default defined in two languages must be single-sourced or guarded. Re-aligning the two values
  would leave the defect class intact.
* `placement` is journaled — `crates/protocol/src/event.rs` carries it inside the display pane event
  — so any change to the type is a wire-compatibility question, not a local refactor.
* A durable condition on an ephemeral channel is not surfaced. Whatever channel is chosen has to
  outlive the next event.
* A downgrade is only actionable while something can still be done about it.

## Considered Options

For the default:

* Resolve it caller-side: the client asks for a backend-appropriate placement.
* Resolve it backend-side: each backend supplies the placement it would naturally use.

For the journaled `Embedded` value:

* Deprecate in place — keep the variant, remove every construction site, reject it as input, and
  accept it on replay.
* Delete the variant and add a deserialize-only tolerance mapping the legacy string to `Unknown`.
* Delete it outright and document a breaking change.

For surfacing the downgrade:

* A warning field on the `run/submit` result.
* A sticky flag on the monitor row.
* Extend `DisplaySelection.attempts` to record post-selection failures.
* A milestone digest line.

## Decision Outcome

**The default is the backend's natural form, resolved backend-side.** herdr supplies a pane, tmux a
split, the windowed backend a tab — which it already did, and it keeps reporting its *actual*
placement rather than the requested one. Backend-side resolution keeps the knowledge where the
constraint lives and makes the default unfalsifiable by a stale client, which is the property the
duplicated constant lacked.

The duplication itself was resolved by **deletion rather than by a guard**, which is better than what
was decided. The decision's structural rider was to fixture-guard the two values against each other,
on the pattern used for state-root parity between `resolveStateRoot` and `StateRoot::resolve`. What
shipped instead removes the client-side value: `displayPreferenceFragment` in
`packages/extension/src/tools/shared.ts` no longer sends `placement` at all, only `ordered` and
`launchProgram`. There is nothing left to keep in sync, so there is nothing to guard — which is the
stronger form of the same intent, and the one that follows from resolving backend-side rather than
merely agreeing across the boundary.

**`Embedded` is deleted outright, and the deletion is a breaking change.** The remedy is documented
rather than engineered: a journal containing the value cannot be read by the new binary, and the fix
is to clear the repository's SQLite state under the crew state directory.

**Three channels carry a downgrade, and the `run/submit` result is not one of them.**

* A sticky `paneDowngraded` flag on the monitor row, modelled on `openViolations` in the same file —
  which is already sticky and cleared only by an explicit decision. Not `latestActivity`.
* `DisplaySelection.attempts` extended to record pane-creation failures that happen *after* backend
  selection succeeded. Its doc already promised "every backend tried, in order, so an operator can
  see why the preferred one lost"; it was empty in this failure precisely because selection worked
  and creation did not. Extending one already-plumbed field beats adding a channel.
* One currency-checked milestone digest line, which becomes the leader's notification path.

### Implementation status of the three channels

Recorded because this ADR states a decision and two thirds of it is in the code, which a reader will
otherwise assume means all of it is.

* The `PaneDowngraded` event kind exists (`crates/protocol/src/event.rs`), carrying requested and
  actual backend and placement plus a redacted reason.
* The sticky monitor flag shipped and is explicitly modelled on `openViolations`, with a doc comment
  in `packages/extension/src/monitor/model.ts` saying so and a test asserting it survives subsequent
  unrelated events — the precise behaviour whose absence hid the original failure.
* The digest line shipped (`packages/extension/src/milestones.ts`).
* **The `attempts` extension has not shipped.** `crates/runtime/src/display/mod.rs` still populates
  `attempts` only inside the backend-selection loop, and the field is still a list of backends
  rather than of outcomes, so a pane-creation failure after successful selection is still absent
  from it. The decision stands; the work does not exist yet. Anyone reading `attempts` today is
  reading selection history only. Tracked as CREW-73, to land before the next live exercise.

> **Amendment, 2026-09-08.** Every claim in the section above was true when written and three of
> them are no longer the whole picture. Recorded as an addition rather than by editing the text,
> because the original was a decision's state at a moment and the changes since are what happened
> to it.
>
> **The `attempts` extension was never built, and the intent landed elsewhere.**
> `DisplaySelection` is not reachable from any wire message — it appears nowhere in
> `crew.schema.json`, has no generated binding, and nothing consumed its `attempts` field — so
> extending it as this decision specified would have built a channel with no far end. The sequence
> instead became `attempted` on the `PaneDowngraded` event, which is journaled, monitor-visible and
> already fires exactly when the preferred backend lost. It carries the backends **walked or
> tried**, in order, without duplicates: each retry re-resolves over a strict suffix of the
> previous candidate list, so entries cannot repeat — but an entry is not proof that
> `create_pane` was called on that backend, only that resolution reached it. Payloads written
> before the field existed replay as an empty sequence.
>
> **Attach now retries the remaining candidates before falling back to hidden**, where this
> decision described a single attempt. Two consequences for the surfacing half: `actualBackend` is
> no longer always `hidden` — a pane can land in a non-preferred backend — and a downgrade is
> journaled on **any** requested-versus-actual divergence, including when a later candidate
> *succeeded*. Emitting only on total failure would have made "landed in tmux when herdr was
> preferred" silent, which is the class this decision exists to close. Each retry re-derives its
> placement from the candidate's own natural form rather than carrying the first backend's, or the
> retry would reintroduce the mismatch the default half of this decision fixed.
>
> **Reopen is deliberately not symmetric with attach, and that is a decision rather than an
> omission.** `pane/reopen` remains single-attempt: on pane-creation failure it returns an error
> to the caller rather than degrading to hidden, so there is nothing to downgrade and no event to
> journal — for a fresh attach a pane is optional and the run must proceed, while for a reopen the
> pane is the entire request. Retrying there would also scatter one run's panes across hosts over
> its lifetime. One inconsistency inside that: reopen's two early returns (no backend selected, or
> the registry cannot find the selected one) journal a hidden attach and report success, so a
> reopen with nothing available is visible only as "pane attached: hidden" rather than as an error
> or a downgrade.
>
> **And a citation in this ADR's own Links was evidence of nothing.**
> `a_failed_pane_creation_does_not_hold_a_slot` was listed here as pinning the resource
> consequence. It was vacuous until the retry work fixed it: it asserted the cap was free by
> attaching through a **second, independently-constructed coordinator** whose `live_panes` set was
> fresh regardless of what the first released, so it passed whether or not the release happened.
> The comment above the assertion described the right property while the code measured a different
> object. Both slot tests now share one coordinator and both fail when the release is removed.
> Left in the Links list rather than removed, with this note, because a reader who checked the
> citation before that fix would have been misled and should be able to see why.

### Why the result field was dropped, and what it would have reversed

The first shape of this decision put a warning on the `run/submit` response, on the reasoning that
this is the only moment the leader can react. Scoping falsified the premise: **pane attach happens
after `run/submit`'s response is sent**, so the failure does not exist when the response is built.
There was nothing to put in the field.

`crates/runtime/src/service/orchestration.rs` also carries an explicit earlier decision keeping
display information out of that result, because "the real attach, if any, happens later and is only
ever reported through the journaled `DisplayPaneAttached` event". Adding the field would have
reversed that decision silently, as a side effect of a surfacing change.

### The reasoning this decision reverses, and why the reversal is not a correction

`Embedded` was first ruled **deprecated in place** rather than deleted, and the argument was sound:
the value is journaled, replayed events must keep validating against the canonical Rust type, and
the append-only journal means old rows cannot be rewritten to fix them. Deleting the variant breaks
replay, recovery and audit for any repository whose journal contains it.

That argument was not wrong. It was *premised* on needing old-journal replay, and the premise was
falsified as a product fact: the extension has no external users yet, so no journal exists that
anyone needs to keep readable. Legacy-journal compatibility became a non-goal, and with the premise
gone the conclusion went with it.

Recorded this way deliberately, because the two readings are different and only one is honest. This
was not a technical argument overturned by a better technical argument; it was a correct technical
argument resting on a fact about the product that only the product's owner could settle. A reader
who assumes the invariants reasoning was mistaken will draw the wrong lesson from it — the reasoning
holds, and becomes binding again the moment an external user has a journal.

The same ruling **cancelled** a bundled fix for an older instance of the identical hazard:
`DisplayBackend::Terminal` was retired by removing the variant with no legacy acceptance
(`crates/protocol/src/display.rs`), so a sufficiently old journal already fails to deserialize. That
finding is closed as accepted-pre-release on the same reasoning. It is worth naming rather than
quietly dropping, because the hazard is real and the acceptance is conditional on the same product
fact.

### Positive Consequences

* The default finally implements the intended semantics, in the one place that owns the constraint.
* The cross-language duplication is guarded rather than re-aligned, so the two values cannot drift
  apart silently again.
* A downgrade survives subsequent events, and the requested-versus-actual distinction becomes
  representable instead of being inferred from an outcome that has two possible causes.
* `attempts` becomes the whole story of a display resolution rather than only its first half.

### Negative Consequences

* **Deleting `Embedded` is a breaking change with a data-loss remedy.** Anyone with a journal
  containing the value clears their state to upgrade, losing that history. The decision accepts this
  on the strength of a product fact that will stop being true.
* A deserialization failure is a poor way to discover this. The courtesy owed with it is a legible
  typed error at startup and replay naming the remedy and the directory to clear — never a bare
  serde panic. That is an implementation obligation this decision creates, not a property it
  provides.
* Backend-side resolution means the placement a client asked for and the placement it gets can
  differ without the client being told at request time. That is why the surfacing half of this
  decision is not separable from the default half.
* The digest becoming the leader's notification path makes this decision depend on the digest's own
  currency guard. A digest that fires on a decided fact would surface a downgrade that has already
  been handled.

## Pros and Cons of the Options

### Backend-side natural form (chosen)

* Good, because the knowledge lives where the constraint is enforced, and a stale client cannot
  contradict it.
* Good, because it deletes a duplicated constant instead of synchronising one.
* Bad, because the resolved placement is not known to the caller at request time.

### Caller-side natural form

* Good, because the request is self-describing — what was asked for is what was resolved.
* Bad, because every client needs a table of backend capabilities, which is the duplication this
  decision exists to remove, in a new place.

### Deprecate `Embedded` in place

* Good, because it preserves replay, recovery and audit for existing journals, and keeps the wire
  history honest and self-documenting.
* Good, because the diff is small: the variant stays, its construction sites go, and a typed
  rejection guards any producing boundary.
* Bad, because it carries a value nobody may produce, forever, for a compatibility guarantee that no
  current user needs.

### Delete with deserialize-only tolerance

* Good, because it shrinks the enum while keeping old journals readable.
* Bad, because it replaces a deprecated variant with a custom deserializer — more code in the one
  place that must never be wrong, to avoid a variant that documents itself.

### Delete outright (chosen)

* Good, because the type ends up describing exactly what the system produces, with no legacy surface
  to maintain or explain.
* Bad, because it is a breaking change whose remedy is discarding data, justified by the absence of
  users rather than by anything about the design.

### A warning on the `run/submit` result

* Good, because it reaches the leader at the only moment a resubmission is possible.
* Bad, because the failure has not happened yet when that response is built, so the field would
  always be empty — and adding it would have reversed a prior decision about what that result
  carries.

## Links

* Implements the settled "panes follow the host" semantics that the `Embedded` default never did.
* Constrained by [ADR-0020](0020-per-mutation-event-broadcast-is-not-optional.md) — the diagnostic
  and the pane events commit and broadcast in one call like every other mutation.
* Bounded by invariants 1 and 2 (canonical Rust protocol types; every daemon message validated
  before extension logic touches it), which are what made the journaled-value question load-bearing
  rather than cosmetic.
* The resume-cause event kind (PR #89) rode this decision's protocol-change window so that there
  would be one schema regeneration rather than two. That is scheduling rather than design, and is
  noted only so a reader does not look for a connection between the two subjects.
* The placement half shipped in PR #87 ("delete Embedded placement, backend-natural-form default");
  the surfacing half in PR #88 ("typed PaneDowngraded event, sticky monitor flag, digest"). The
  digest's currency guard, which the third channel depends on, shipped separately in PR #84.
* Guard tests, by their real names on main rather than by the names the design note proposed:
  `attach_reports_the_backends_actual_placement_not_the_requested_one` pins that a backend reports
  what it did rather than what was asked;
  `a_create_pane_failure_journals_a_typed_pane_downgraded_event_and_falls_back_to_hidden` pins the
  downgrade path, with `a_create_pane_failures_secret_shaped_stderr_is_actually_redacted_before_journaling`
  covering the reason's redaction and `a_failed_pane_creation_does_not_hold_a_slot` the resource
  consequence. On the extension side, `packages/extension/src/monitor/model.test.ts` asserts the
  sticky flag survives subsequent unrelated events — the precise behaviour whose absence hid the
  original failure — and `packages/extension/src/milestones.test.ts` that a downgrade is always a
  milestone. **There is no test that a post-selection failure appears in `attempts`, because that
  channel does not exist yet** (CREW-73). The two legacy-replay guards written for the
  deprecate-in-place shape were dropped with it.
