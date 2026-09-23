# A one-turn worker's exit is the run's terminal event; the lifecycle must model exit, not hold

* Status: Proposed
* Date: 2026-09-22
* Supersedes: *(none — this does not reverse a decision; it resolves one that was held)*
* Amends: [0037](0037-protocol-first-control-plane.md) (decision 8, the deferred question), [0027](0027-turn-end-settles-a-run.md) (the meaning of the turn-boundary event), [0036](0036-leader-disconnect-grace-window.md) (lease release on worker exit)

## Context and Problem Statement

The run lifecycle was built around a worker that **finishes a turn and holds**. A TUI vendor never
exits, so `ProcessExited` alone would leave such a run non-terminal forever, and the vendor's own
end-of-turn boundary (`observe_turn_ended`, ADR-0027) is what settles it. ADR-0037 decision 8
reversed that for the protocol path — a protocol worker finishes its turn and **exits**, so its exit
and its turn boundary coincide — but deliberately left open the larger question:

> whether a run lifecycle built around a persistent worker is the right model for a worker whose
> normal life is one turn.

That question is now settled. The lifecycle code already encodes the correct rule; nothing was
omitted, and no code change is required to make it right — only to stop treating the rule as
deferred.

The relevant code is `crates/runtime/src/adapter/run_lifecycle.rs:694-731`, `terminal_state_for`. It
is exact about the condition, not just the exit code:

```rust
fn terminal_state_for(
    exit_code: Option<i32>,
    signal: Option<&str>,
    turn_settled: bool,
) -> RunState {
    if signal.is_some() {
        return state("failed");
    }
    match exit_code {
        Some(0) if turn_settled => RunState::unrendered_verdict(),
        Some(_) => state("failed"),
        None => state("lost"),
    }
}
```

The decision is `turn_settled` — whether this run's turn had *already settled* (ADR-0027's
`observe_turn_ended`) with no `run/finish` verdict since. The four cases:

* a signalled death is `failed` (the code is not trustworthy once a signal is);
* a non-zero exit is `failed`;
* a zero exit with **no settled turn** is `failed` — the run did no work, whether it never got past
  `starting` (a start failure) or parked at `waitingUser` (ADR-0012's approval flow) without ever
  finishing a turn;
* a zero exit **after** a settled turn is `unrendered_verdict()` — the run did real work and produced
  a result, but nothing (no `run/finish` call) rendered a verdict before the process went away.
  **Never a guessed `succeeded`** — that judgment belongs solely to the leader's `run/finish`
  (ADR-0027).

That is exactly the model a one-turn worker needs. A one-turn worker (claude/codex/copilot driven
over its protocol) finishes its turn and exits; the exit and the turn boundary arrive together. The
`turn_settled` flag is the discriminator: if the vendor's own turn-end was observed before the exit,
the exit is `unrendered_verdict` (real work, no verdict rendered — the leader decides); if no turn was
settled, the exit is `failed`. The lifecycle already distinguishes these. The only thing that was
wrong was the *documented belief* that this was an open question.

The proof-run rows corroborate the code. As recorded in the consolidated backup (and `PROOF.md`),
claude driven over the protocol path exited cleanly after a settled turn at rows 1 (argv
`--model claude-sonnet-5` read from the live process, contradicting the operator's `opus` default),
4 and 5 (one approval round trip produced a durable ledger entry; the three-way reconciliation's
second leg), and 9 (posture observed at `system/init`). Each was a one-turn worker; the lifecycle
handled them correctly because `turn_settled` was true. Nothing in the run contradicts
`terminal_state_for`; the states are consistent with the code.

The in-repo evidence that closes the one-turn-worker case is the unit test
`an_exit_after_a_settled_turn_with_no_run_finish_settles_the_run_as_the_unrendered_verdict_state`
(`run_lifecycle.rs:2156`): it emits a real `TurnEnded` (setting `turn_settled`), then a
`ProcessExited { exit_code: Some(0), signal: None }`, and asserts the run lands in
`unrendered_verdict`, not `failed` and not `succeeded`. That is exactly the one-turn-worker case the
ADR describes, exercised against the claude protocol adapter.

## Decision

**A one-turn worker's clean exit, after its turn has settled, is terminal — the run is
`unrendered_verdict`, and the leader's `run/finish` is the only thing that may render a verdict on
it.** The lifecycle models exit as the terminal event; it was never wrong, only described as if it
were provisional.

Concretely:

1. The lifecycle is adopted as the model for **all** protocol-driven workers (claude, codex,
   copilot, omp), not only the claude adapter. A cross-adapter conformance check — asserting
   `terminal_state_for` returns the same verdict for a one-turn worker driven by each adapter — is
   owed and is the gate that closes this ADR. It was "enforced today for the claude protocol adapter
   only" (ADR-0037 decision 8); the outstanding clause is the cross-adapter check, not the rule.

   **Owed as a test, not a code change** — consistent with the ADR's central claim that no code
   change is required to make the rule right: add a test mirroring
   `an_exit_after_a_settled_turn_with_no_run_finish_settles_the_run_as_the_unrendered_verdict_state`
   (`run_lifecycle.rs:2156`) but driven through each adapter's `terminal_state_for` path (claude,
   codex, copilot, omp) and asserting the verdict is identical. One test; no new code.

2. `turn_settled` remains the sole discriminator. The lifecycle does **not** gain a `succeeded`
   state derived from exit status alone. A one-turn worker that exits cleanly with no settled turn
   is `failed` (it did no work); a one-turn worker that exits cleanly with a settled turn is
   `unrendered_verdict` (the leader decides). Nothing reads exit status as success.

3. The turn-boundary event keeps its name and its corrected meaning — *the vendor's turn reached its
   end* (ADR-0037 decision 8) — and is recorded as observable for a one-turn worker on the protocol
   path. The earlier definition (the vendor *holding at its prompt*, a living terminal session) half-
   described a worker that leaves. It is not reopened.

## What the code and the run establish

The code (`terminal_state_for`, `run_lifecycle.rs:694-731`) and the proof run agree. The run's
claude half, driven over the protocol path, produced four one-turn workers that exited cleanly after a
settled turn (rows 1, 4, 5, 9) and the lifecycle handled each correctly. The run closed deliberately
after row 5; rows 6 and 7 never ran, and codex/copilot/omp have no live evidence — but the lifecycle
does not depend on that evidence existing. It depends on `turn_settled`, which the claude half
exercised.

The one finding that would falsify this ADR is a one-turn worker that exits cleanly **after a settled
turn** and is recorded as `failed` (or `succeeded`). No such case appears in the run; the run's
claude half is consistent with `terminal_state_for`. The absence of evidence for the other three
vendors is not evidence against this ADR — it is the cross-adapter conformance check that is owed.

## Consequences

**Carried over unchanged:** the journal and its invariants, the approval *service*, the workspace
lease (decision 7 of ADR-0037), and the leader's `run/finish` as the sole verdict-renderer (ADR-0027).

**Changed by this decision:**

- The lifecycle is no longer described as built around a persistent worker with an open question. It
  is described as built around a worker whose turn settles and then exits, with `turn_settled` as
  the discriminator. The `ProcessExited` rows in `docs/architecture.md` (findings 7 and 8 of the
  2026-09-13 docs audit) already encode this; this ADR makes them primary rather than provisional.

- The claude-only conformance check (ADR-0037 decision 8, "enforced today for the claude protocol
  adapter only") becomes a cross-adapter check. Until it exists, ADR-0037's codex/copilot/omp halves
  remain inferred, unverified — not because the lifecycle is wrong, but because the lifecycle has
  only been exercised against one adapter.

- The meaning of `unrendered_verdict` widens from "a long-lived worker exited after its turn settled"
  to "any worker, including a one-turn worker, exited after its turn settled." The leader still
  decides; the lifecycle still never guesses `succeeded`.

**Lost or newly owed:**

- **A cross-adapter conformance check** — the single outstanding clause. It asserts that
  `terminal_state_for` returns the same verdict for a one-turn worker driven by each adapter.
  Without it, this ADR is proven for one vendor and inferred for three.

- **Live evidence for codex/copilot/omp** — the lifecycle does not need it, but ADR-0037's claim that
  those three are "inferred, unverified" remains true until they are driven. This ADR does not close
  that; it closes the lifecycle question.

## What would reverse this

Stated so this can be wrong in a detectable way: if a one-turn worker exits cleanly after its turn has
settled and the lifecycle records it as `succeeded` (a guess) or `failed` (a wrong verdict), then
`turn_settled` is not the correct discriminator and this ADR is wrong. The evidence — the run's claude
half and the code — both say the lifecycle does neither. If a future run shows it doing one, the
lifecycle must be re-examined, not the meaning of the exit.

## Open, not settled

- The cross-adapter conformance check (owed; closes this ADR).
- Live evidence for codex, copilot, and omp (not closed here; remains ADR-0037's "inferred, unverified").
- Whether the leader's `run/finish` should ever render `succeeded` from a one-turn worker's clean exit,
  or whether `unrendered_verdict` is terminal and the leader must be explicit — ADR-0027's rule that
  the leader decides stands; this ADR does not change it.
