# A worker's model name is resolved against the host's catalogue, and persisted only once confirmed

* Status: Accepted
* Date: 2026-09-07

## Context and Problem Statement

A leader invented a plausible-looking dated model identifier, `crew_profile` accepted it because
nothing validated model names, and the persistence rule wrote it into the repository's
`.omp/crew.json` — where it became the project's answer for every later session. Observed during a
live exercise.

Two separate problems sit behind that, and separating them is most of this decision:

* **Validation.** Nothing established that a model name existed before a worker was launched with
  it.
* **Resolution.** A person naturally writes a shorthand. "Use codex with sol" names a real model,
  and `sol` is not a string any vendor accepts.

An earlier decision had settled the surrounding policy: ask the user which model to use on first
use, persist the answer, and stay silent about it afterwards. That is what made the failure durable
rather than momentary — the invented name was persisted by design, and the design had no way to tell
an invented name from a real one.

The investigation that preceded this decision also produced a correction worth recording, because it
changed the option space. An initial note concluded that no vendor CLI offers a queryable model
list, and therefore that live validation was impossible. That was true of the three vendor CLIs and
beside the point: crew runs *inside* omp, and omp ships a model catalogue that reads from a local
cache — 725 models across seven providers, three of which are crew's vendor adapters. Validation was
available for free and nobody had looked at the host.

## Decision Drivers

* Persisting a wrong name is worse than rejecting a right one. A rejected name costs one exchange; a
  persisted wrong name costs every future session until someone edits a file by hand.
* A shorthand a person would naturally type should work, and should work *visibly* — resolving it
  silently means the model that ran is not the string anyone wrote down.
* Validation cannot be promised uniformly. Whatever ships must not claim in a tool description or a
  skill that model names are checked, because that claim cannot hold for every adapter.
* The catalogue is a cache. A genuinely new model can be absent from it while being perfectly valid,
  so an unrecognised name cannot be treated as an error.

## Considered Options

* Pass every name through to the vendor and let it fail; validate nothing.
* Validate against the host catalogue; refuse anything it does not know.
* Validate against the host catalogue, resolve shorthands, pass unknown names through with a visible
  note — and separate the persistence decision from the launch decision.

## Decision Outcome

**Resolve against the host catalogue and crew's own alias tables, pass unknown names through
visibly, and persist only what was confirmed.**

Resolution order, in `packages/extension/src/models.ts`: an exact catalogue identifier; then a
vendor-defined alias; then a name matching exactly one identifier within the adapter's own provider;
then ambiguity, refused by name; then unknown, passed through.

**Alias before substring is load-bearing.** Against the twenty-four real `anthropic` identifiers,
`opus` matches ten, `sonnet` eight, `haiku` three and `fable` two — so a substring-first resolver
would refuse all four of the vendor's own shorthands as ambiguous. Conversely `sol` needs no table
entry at all: scoped to its provider it matches exactly one identifier, which is how the shorthand
that motivated the work resolves without crew owning a mapping for it.

**Nothing resolves silently.** A resolution is echoed back — the caller is told what their input
became — and an unrecognised name is reported as unverified rather than accepted quietly. That is
the difference between crew resolving a shorthand and crew rewriting a request behind someone's
back.

**Persistence is gated on confirmation, not on success.** A name the catalogue confirms, or that
crew's own reviewed alias table maps, is persisted. A name nothing could confirm runs — the vendor
owns its namespace and is the second line of defence — but is not written down, and the caller is
told it was not.

### The persistence rule this supersedes, and why the premise changed

The earlier ruling specified **evidence-based persistence**: a model string would be persisted only
after a run using it had produced a real vendor turn. That was the right choice *given what was
believed at the time* — that registration-time validation existed only where a documented surface
happened to exist, which covered one adapter of four. With no way to check a name up front,
waiting for a run to prove it was the only mechanism that covered the other three.

The catalogue falsified that premise. It provides a registration-time check for three of the four
adapters, which is strictly earlier than waiting for a turn: the invented name never reaches a
vendor process, never consumes a lease, a slot or a pane, and never becomes durable. So the weaker
but earlier check replaced the stronger but later one.

**The cost of the swap, stated plainly:** a genuinely new model, valid but not yet in the local
cache, is now never persisted — where the superseded rule would have persisted it after one
successful run. That case is real and the remedy is manual. It is accepted because the failure it
trades against is silent and permanent, while this one is visible and self-correcting: the caller is
told the name was not confirmed and not recorded, every time.

### What is deliberately not claimed

Validation is not uniform and no tool description or skill says it is. Two of the four adapters have
no alias source crew can cite, and `ompRpc` has no single catalogue provider bounding it at all — so
for those adapters behaviour is exactly what it was before this decision: the name passes through
untouched and is persisted as given. Promising uniform validation would have been a claim two
adapters could not keep.

The alias table is small on purpose, and every entry's *target* is checkable against the catalogue,
which is what stops it rotting silently. Its authority is the vendor's binary rather than the
vendor's documentation: one vendor's `--help` offers three aliases as examples while the binary's own
configuration carries a fourth, so a table built from the documented surface would have silently
omitted a working shorthand.

### Positive Consequences

* The failure that prompted this cannot recur through the same path: an unconfirmed name is never
  written to `.omp/crew.json`.
* A shorthand a person would naturally type resolves, and the resolution is visible in the result.
* An ambiguous name is refused with every candidate named, rather than resolved to whichever the
  implementation happened to reach first.
* Validation happens before a vendor process is spawned, so a wrong name costs nothing but the
  exchange that reports it.

### Negative Consequences

* **A valid model absent from the local cache is never persisted**, and must be passed on every
  session or written into the config by hand. This is the accepted cost of the supersession above.
* The catalogue is read, never refreshed, so its staleness is inherited. That is precisely why an
  unrecognised name passes through rather than being refused, but it also means the confirmed set
  lags reality.
* Reading the catalogue is a subprocess. The host does expose a model facade to extensions, and it
  is deliberately not used here — its resolver breaks ambiguity by preference (most-recently-used,
  provider precedence), which would resolve an ambiguous shorthand to a silently chosen winner. That
  is the failure mode this decision exists to prevent, so the host's own resolver is the wrong tool
  despite being the more obvious one.
* Two of four adapters gain nothing, and a reader of the tool description has to notice which.

## Pros and Cons of the Options

### Resolve, pass through visibly, persist on confirmation (chosen)

* Good, because it separates "may this run" from "may this be remembered", which is the distinction
  the original failure collapsed.
* Good, because it makes the vendor the second line of defence rather than the only one, without
  claiming a guarantee it cannot give for every adapter.
* Bad, because a valid-but-uncatalogued model is never remembered, and because the confirmed set is
  only as current as a local cache.

### Pass everything through, validate nothing

* Good, because the vendor owns its namespace and will reject what it does not recognise, and
  because it adds no mechanism.
* Bad, because rejection happens after a lease, a slot and a process have been spent, and because it
  leaves the persistence path — the part that made the failure durable — completely unguarded.

### Validate and refuse anything unknown

* Good, because nothing unverified ever runs, which is the strongest possible guarantee about what
  reaches a vendor.
* Bad, because the catalogue is a cache: it would refuse a genuinely new model on the strength of a
  stale local file, and the person who typed a correct name would have no recourse.

## Links

* Supersedes the evidence-based persistence rule described above; the resolution and echo behaviour
  it also specified is kept and implemented.
* Builds on the earlier ask-on-first-use-and-persist policy, which this decision leaves intact and
  gates.
* Resolution logic in `packages/extension/src/models.ts`, applied in
  `packages/extension/src/tools/profiles.ts` before the stored-model comparison — the ordering
  matters, because comparing a shorthand against a stored canonical identifier reads as a conflict
  when it is the same model.
* Proven by `packages/extension/src/models.test.ts` and the profile tests beside it, whose catalogue
  fixtures are the **complete** provider lists rather than a selection: an early draft used a subset
  in which one alias matched a single identifier and therefore resolved by unique match, which made
  the alias table look optional. A subset fixture can make an ambiguous input look unique.
