# No version gate decides whether Crew may drive a harness: an unrecognised entry degrades to `Raw`, and the vendor's self-declared version is recorded rather than enforced

* Status: Proposed
* Date: 2026-09-30
* Supersedes: *(none — this does not reverse a decision; the gate it removes was inherited and never decided)*
* Amends: *(none — no existing record states the gate policy; `docs/compatibility.md` is a documentation table, not a decision record)*

> **This record is Proposed and nothing in it is yet a decision.** It awaits the owner's
> ratification. The owner's ruling of 2026-09-29 that fixes its central proposal is quoted verbatim
> below; the mechanism the ruling directs is `[AGENT-PROPOSED]` and is what ratification would
> accept or reject. **Its title states the policy for all four harnesses, and the four are not
> equally evidenced:** Copilot's rests on verified evidence in this repository, and Claude's,
> Codex's and OMP's are reasoned from the ruling and proposed rather than evidenced — every place
> that difference matters is marked, and the marking is the point. Ratifying it means changing this
> header's `Status:` to `Accepted` and following the index in [README.md](README.md) — the
> repository's write-once rule means a later ratification lands in this file's own text rather than
> as an edit elsewhere.

**A note on this record's own title and filename.** The H1 above states the mechanism this record
proposes. The filename is `0051-copilot-version-gate-is-a-range-and-unknown-entry-types-degrade-to-raw.md`,
which names the **withdrawn** earlier proposal — a range gate — and the half of this decision that
does survive. The filename is deliberately **not** changed: it is cited by the row for this record
in [README.md](README.md) at line 87, and from the planning vault, and this record is not permitted
to break either reference. The withdrawn title and the reason it was withdrawn are recorded here
rather than performed on the filename.

**The title this record was drafted under is withdrawn, 2026-09-30.** The first title was "The
Copilot TUI version gate is a range like the other three, and an unrecognised entry type degrades
to `Raw` before that range is widened", which proposed replacing Copilot's exact-match list with a
parsed inclusive range. That proposal is withdrawn rather than silently edited away, and the reason
is the owner's ruling below. A range is still a hand-maintained version bound, and the bound is the
part that goes stale.

**A scope correction made in this file, 2026-09-30, and named here rather than performed
silently.** An earlier draft of this record carried the H1 above while deciding Copilot alone:
Decision 3 said the gate stopped deciding *"whether a Copilot session starts"*, and every
`[VERIFIED]` citation, every rejected alternative and both consequence sections were Copilot's. **The
title and the body disagreed about scope, and the body was the one that had to change**, because
the owner's ruling speaks of harnesses in the plural and names none — so the record could not keep a
general title over a single-harness decision. The ruling was not re-read to justify the general
title, and it was not narrowed either: what changed is that the body now states the policy four
times, once per harness, and **marks which three of those four are proposed rather than evidenced.**
The evidence was not strengthened to match the title, because the evidence for three of the four
cannot be. What *is* verified for each of them is verified here and cited here.

## How to read the labels

Four provenance labels are used, and the point of the scheme is that a reader can tell an owner
ruling from an agent's reading of one:

* `[USER-STATED]` — the owner said it. Quoted verbatim where the wording matters; never paraphrased
  into something firmer than what was said.
* `[VERIFIED]` — opened and read in this repository for this record, or against a document named as
  such. Every `file:line#Symbol` below was opened and the named symbol was seen at that line or
  inside that range; a line inside a cited declaration is written "line N of" the anchored range.
  A line in prose (a Markdown record, a comment block, a JSONL fixture line) is cited by path and
  line with the sentence quoted, because it has no declaration to anchor to. `UNVERIFIED` marks
  anything not resolved.
* `[AGENT-PROPOSED]` — a design choice or inference put forward for the owner to accept or reject.
* `[AGENT-DECIDED]` — a choice made where the owner left the mechanism open. It is still not the
  owner's ruling, and is marked so it can be reversed.

**An inference is never presented as a ruling.** Where the owner ruled a consequence and the design
supplied the object, both are said.

## The owner's ruling of 2026-09-29

`[USER-STATED]` 2026-09-29, verbatim:

> "Harness are updated almost daily. This is not a bug. We are making it a bug, because for a
> reason that is not well established you check the version of each harness. We need to identify why
> this is happening now and if we still need to keep that version guard on harnesses. I say, we do
> not! At least not up to the patch."

The ruling's own diagnosis — *"for a reason that is not well established"* — is the question this
record answers, and its answer is the reason the guard goes: the reason was never established, and
what the guard's own comment asserts in its place is not a reason at all. The ruling's scope is
*"up to the patch"*: a major or minor boundary is not ruled on here and is not claimed to be settled.

## The scope of this record: four harnesses, decided separately

`[USER-STATED]` the ruling speaks of *"the version of each harness"* and of *"that version guard
on harnesses"* — plural, and naming none of them. It does not name Copilot. **This record
therefore states the policy four times, once per harness, and the four statements are not
equally evidenced.**

| Harness | Policy this record states | What is underneath it |
|---|---|---|
| Claude | No version gate decides whether Crew will drive it | `[AGENT-PROPOSED]` — reasoned from the ruling; see below |
| Codex | No version gate decides whether Crew will drive it | `[AGENT-PROPOSED]` — reasoned from the ruling; see below |
| OMP | No version gate decides whether Crew will drive it | `[AGENT-PROPOSED]` — reasoned from the ruling; see below |
| Copilot | No version gate decides whether Crew will drive it | `[VERIFIED]` end to end, by file and line; see below |

Read that table as the honest distribution of what carries each cell. **Copilot's is carried by
verified evidence. The other three are carried by the owner's own plural ruling plus one verified
fact they share, and by nothing else.** Where this record says something about Claude, Codex or
OMP that it does not say about Copilot, the asymmetry is deliberate and is stated where it occurs
rather than smoothed over. `[AGENT-PROPOSED]` is what marks the difference between *the owner
ruled this* and *an agent read the ruling as reaching this too*.

### What is verified about the other three: ranges, and each one's own admission

`[VERIFIED]` each of the three is a hand-maintained **range**, declared as a constant pair in the
adapter's own module. The bounds below were re-read from those three files for this record and
are **not** carried forward from `docs/compatibility.md:232-234`, which states the same three in
prose; the source is cited because the source is what an implementer edits.

| Harness | `MIN_TESTED_VERSION` | `MAX_TESTED_VERSION` | Declaration |
|---|---|---|---|
| Claude | `(1, 0, 0)` | `(2, 99, 99)` | `crates/runtime/src/adapter/tui/claude.rs:38-39` |
| Codex | `(0, 100, 0)` | `(0, 199, 99)` | `crates/runtime/src/adapter/tui/codex.rs:35-36` |
| OMP | `(18, 0, 0)` | `(18, 99, 99)` | `crates/runtime/src/adapter/tui/omp.rs:52-53` |

`[VERIFIED]` **each adapter's own comment says most of its range is untested**, and each says so
in its own words rather than in one shared phrase:

* Claude — `crates/runtime/src/adapter/tui/claude.rs:36`: *"rest of the range is an untested
  extrapolation, not a second data point"*, against the one version *"actually validated"*,
  `2.1.241`, named at `:34`.
* Codex — `crates/runtime/src/adapter/tui/codex.rs:34`: *"the range an untested extrapolation
  until a live smoke run widens it"*, against *"one validated point (`0.149.1`)"* at `:33`.
* OMP — `crates/runtime/src/adapter/tui/omp.rs:44`: *"an untested extrapolation until a live smoke
  run widens it"*, against *"one validated point (`18.0.5`, an installed CLI probed for this
  module)"* at `:42`.

**That is the disease the ruling strikes, in a different form from Copilot's.** Copilot's gate has
a *falsified* policy: its comment asserts exact match and its code is a substring search. The
other three's gates are *honest* policies that are wrong for the same underlying reason — a
version bound a human must edit by hand against daily vendor releases. An accurate comment does
not make the bound correct. `[USER-STATED]` *"Harness are updated almost daily."* A bound with one
validated point in it, maintained by hand, is what that ruling strikes, and whether its shape is a
list or a range does not change what it is.

**How far each of the three's one validated point is actually evidenced differs, and the record
does not flatten it.** `[VERIFIED]` Claude's `2.1.241` and Codex's `0.149.1` each have a committed
fixture behind the comment: `fixtures/adapters/claude-tui/README.md:11` records the capture against
`claude --version` `2.1.241 (Claude Code)`, and `fixtures/adapters/codex-tui/session.jsonl:1`
carries `"cli_version":"0.149.1"`. `[VERIFIED]` OMP's `18.0.5` has **no fixture behind it**: a
search for `18.0.5` across `crates/`, `fixtures/` and `docs/` returns only this adapter's own
comments and one unit-test literal at `crates/runtime/src/adapter/tui/omp.rs:724`, and the
comment's own phrase is *"an installed CLI probed for this module"* — a claim about a past action,
carried here as the claim it makes and not as evidence. So OMP's range has one validated point
claimed and **zero** artefact behind it, where Claude's and Codex's have one each with an
artefact. **None of the three has two data points**, which is precisely what their own comments
say.

### One fact all four genuinely share: no recorded run was ever refused

`[VERIFIED]` this is the single evidentiary item common to all four, and it is checked against
the code that would have recorded a refusal. All four TUI conformance probe scenarios call their
own vendor's gate on the real `--version` stdout and fail the scenario on an incompatible verdict:
`crates/runtime/src/adapter/tui/claude_conformance.rs:130`, `codex_conformance.rs:103`,
`omp_conformance.rs:102` and `copilot_conformance.rs:103` each hold
`match vendor.version_gate(&version)`. `[VERIFIED]` the production probe path reaches the same gate
at `crates/runtime/src/adapter/tui/adapter.rs:2631`, where an incompatible verdict becomes
`AdapterError::incompatible_version`.

`[VERIFIED]` all six committed live conformance reports in `release/live-conformance/` — which
between them cover all four vendors — record `"version": null` at line 5 of each and record their
`probe` scenario as `"outcome": "pass"`: `claude-tui.json:32-33`, `codex-tui.json:32-33`,
codex-tui-post-quota.json:32-33`, `omp-rpc-tui.json:32-33`, `copilot-tui.json:32-33` and
copilot-tui-2026-08-26-transcript-capture.json:32-33. `[VERIFIED]` `docs/compatibility.md:207-209`
records why the version is absent from them: the reports *"deliberately carry no vendor version ...
so a report is evidence about the adapter injection path, not about a specific CLI release."*

**Zero recorded refusals across all four harnesses is a fact about the record, not about the
world.** It says Crew holds no evidence that any of these gates ever stopped anything. It does not
say one would not have.

### What is *not* verified about the other three, and is not manufactured here

Stated plainly, so that a later reader does not credit this record with evidence it does not have:

* **No falsified policy is shown for Claude, Codex or OMP.** `[VERIFIED]` each one's `version_gate`
  — declared at `crates/runtime/src/adapter/tui/claude.rs:209`, `crates/runtime/src/adapter/tui/codex.rs:227`
  and `crates/runtime/src/adapter/tui/omp.rs:285` — compares a parsed `(u32, u32, u32)` against its
  own declared pair, which is exactly what its comment says it does. Copilot's defect is that its
  code does not do what its comment says. These three are not defective in that way and this record
  does not pretend they are.
* **No over-admitting comparison is shown for them.** Copilot's substring search at
  `crates/runtime/src/adapter/tui/copilot.rs:188` admits `1.0.800` as `1.0.80`. The three ranges
  parse before they compare — `parse_leading_version` is declared at
  `crates/runtime/src/adapter/tui/claude.rs:46`, `crates/runtime/src/adapter/tui/codex.rs:42` and
  `crates/runtime/src/adapter/tui/omp.rs:60` — so that class of overreach does not apply to them.
  `[VERIFIED]` this was read, not assumed.
* **No uncalled correct predicate exists for them.** Copilot's
  `copilot_cli_version_known` at `crates/runtime/src/adapter/tui/copilot_compatibility.rs:68` is a
  real exact-match predicate that nothing in this repository calls. Claude, Codex and OMP have no
  equivalent dead-but-correct alternative standing beside their gates; the gate *is* the
  comparison.
* **No ACP justification is inherited by any of them.** Copilot's gate arrived with the deleted
  headless ACP adapter, below. `[VERIFIED]` the other three's ranges carry no protocol
  justification in their comments: Claude's and Codex's cite the same honesty about which part of
  the range is real, and OMP's cites only its own file format's self-description at
  `crates/runtime/src/adapter/tui/omp.rs:47`.

**So the reach to the other three rests on the ruling's plural words plus the shared facts above —
not on transplanting Copilot's case onto them.** `[USER-STATED]` the ruling asks *"if we still
need to keep that version guard on harnesses"* and answers *"I say, we do not!"* Extending that to
a gate of a different shape is an inference, and it is marked `[AGENT-PROPOSED]` at every point
where it appears.

## Why the guard was there, and why it is gone

**This section is Copilot's case only.** The other three harnesses did not inherit a gate from a
deleted protocol and are not argued here; their ground is the shared facts and the ranges under
*The scope of this record* above, and the reach to them is `[AGENT-PROPOSED]`.
### It was written for a protocol this repository no longer speaks

The "why it happened" answer, `[VERIFIED]`:

The guard came in with the headless ACP adapter, in commit `1ad4e44` ("feat(adapter): add gated
Copilot ACP worker", 2026-07-25), which created
`crates/runtime/src/adapter/copilot/compatibility.rs`. `[VERIFIED]` the headless adapter is gone:
`grep -rn -- '--acp' crates/` returns zero hits, no `.rs` file contains `CopilotAcpClient`, and the
only `acp` match in the TUI adapter outside `copilot_compatibility.rs` is the word *ACP* inside a
comment at `crates/runtime/src/adapter/tui/copilot.rs:178`. `[VERIFIED]`
`docs/adr/0026-headless-retirement.md:75-78` records the deletion and preserved this one artefact:

> "The one exception is Copilot's CLI/ACP-protocol-version compatibility table
> (`adapter/copilot/compatibility.rs`), which encodes empirically-verified facts about the vendor
> CLI independent of headless vs. TUI dispatch — moved to `adapter/tui/copilot_compatibility.rs`
> rather than deleted."

That record preserves the artefact and says nothing about the policy it encodes. **The gate's
policy was documented but never decided**, and it was inheriting its justification from a protocol
this repository no longer speaks.

### The rationale survives only as prose, and was never ratified

`[VERIFIED]` `docs/compatibility.md:226-228` is the only place in the repository that states the
policy's reason:

> "Copilot's is a discrete, exact-match list instead (ACP is a wire protocol, not a self-describing
> file format, so an untested build gets no benefit of the doubt):"

and `docs/compatibility.md:230-235` is the table it heads, which records the Copilot row as
`Exact match` with the five versions `1.0.73`, `1.0.75`, `1.0.78`, `1.0.80`, `1.0.81`. The rationale
is sound *for ACP*, and the ACP adapter that needed it is deleted. It is a rationale, not a record,
and no ADR ratifies it.

### The policy is falsified by the implementation that claims to enforce it

`[VERIFIED]` the stated policy is exact match. `crates/runtime/src/adapter/tui/copilot.rs:175-183`
is the gate's own doc comment, which asserts at lines 179-180 of that range that this vendor
*"ships breaking protocol changes between patch releases"* and so must never extrapolate a range.
`[VERIFIED]` the implementation is an **unanchored substring search over the vendor's entire
`--version` prose**: `crates/runtime/src/adapter/tui/copilot.rs:184` declares
`fn version_gate(&self, probed: &str) -> VersionVerdict`, and line 188 of that range is
`.find(|version| probed.contains(version))` over the five entries declared at
`crates/runtime/src/adapter/tui/copilot_compatibility.rs:36`
(`COPILOT_KNOWN_CLI_VERSIONS`).

`[VERIFIED]` the comparison is unanchored, so it is not a version comparison at all — it is a
search for a literal anywhere in the string. Simulating that exact expression against the five
declared entries: `1.0.800` is admitted by `1.0.80`, `0.1.0.8000` is admitted by `1.0.80`,
`1.0.81-beta.1` is admitted by `1.0.81`, and `1.0.750` is admitted by `1.0.75`. **A patch release
nobody verified passes the gate the comment calls an exact-match gate.** The genuine exact-match
predicate exists and is declared at
`crates/runtime/src/adapter/tui/copilot_compatibility.rs:68` (`copilot_cli_version_known`), whose
comparison at line 71 is `entry.cli_version == cli_version`; it rejects all four of the above.

`[VERIFIED]` the single gate test, `version_gate_accepts_only_empirically_verified_versions` at
`crates/runtime/src/adapter/tui/copilot.rs:472`, exercises only `GitHub Copilot CLI 1.0.80.`,
`1.0.73`, `GitHub Copilot CLI 9.9.99.` and the empty string. None of those four contains a listed
substring *plus* an extra character, so the test is consistent with both a correct exact-match gate
and the substring search, and proves nothing about the overreach. The comment at
`crates/runtime/src/adapter/tui/copilot.rs:480-481` states the policy the test does not check.

### How many times the gate has ever prevented anything: none that this repository records

`[VERIFIED]` the refusal count is **not** Copilot-specific: it holds for all four harnesses, and
is stated once for all of them under *One fact all four genuinely share* above. What follows here
is Copilot's case only — the parts of the count argument that have no counterpart for the other
three.

* `[VERIFIED]` the Copilot half of that shared count is exactly as thin as the other three's: the
  only test that touches the gate's refusal path is Copilot's, at
  `version_gate_on_unmatched_output_never_echoes_the_probed_junk`
  `crates/runtime/src/adapter/tui/copilot.rs:500`, and it asserts a property of the error *message*
  (it must not echo the vendor's raw output), not that a refusal prevented anything. Copilot gains
  no evidential ground over Claude, Codex or OMP here.
* `[VERIFIED]` a prior claim circulated that the five admissions were "all identical" — the doc
  comment at `crates/runtime/src/adapter/tui/copilot_compatibility.rs:12-16` says each was
  *"empirically reprobed with a real `initialize` handshake and confirmed to negotiate
  `protocolVersion: 1` with identical ACP v1 `agentCapabilities`/`agentInfo` field names"*. That
  sentence is **a code comment with no surviving artefact behind it**: the fixture it names,
  `fixtures/adapters/copilot/initialize-v1.json`, was deleted with the headless fixtures, and
  `docs/compatibility.md:207-209` records that the live reports deliberately carry no vendor
  version, so *"a report is evidence about the adapter injection path, not about a specific CLI
  release."* The comment is carried here as the claim it makes, not as evidence.
* `[VERIFIED]` the three `pub fn`s in `copilot_compatibility.rs` — `copilot_cli_version_known`
  (`:68`), `copilot_negotiated_version_verified` (`:80`), `copilot_acp_protocol_version_supported`
  (`:87`) — and the two ACP protocol constants (`:62-63`) have **no caller anywhere outside that
  module's own test block**. The exact-match predicate is not merely uncalled by the gate; nothing
  in the repository calls it. Only `COPILOT_KNOWN_CLI_VERSIONS` reaches production code, and only
  via the substring search.

### Does any vendor's transcript format actually change between patch releases: no evidence, either way

`[VERIFIED]` this was searched for directly across conformance fixtures, committed golden
transcripts, changelog references and code comments. **No evidence of a format change between patch
releases was found for any vendor, and no evidence of stability was found either.** Specifically:

* The only assertion in the repository that Copilot breaks between patches is the comment quoted
  above at `crates/runtime/src/adapter/tui/copilot.rs:179-180` of the range `:175-183`. It is
  unsupported by any artefact, fixture, or recorded observation.
* `[VERIFIED]` the committed Copilot transcript is a single capture at one version.
  `fixtures/adapters/copilot-tui/session.jsonl:1` carries `"copilotVersion":"1.0.80"`, and
  `docs/compatibility.md:237-241` names the capture versions for all four vendors
  (claude-tui `2.1.241`, codex-tui `0.149.1`, copilot-tui `1.0.80`). One capture per vendor is not
  a version series; nothing in the repository compares two Copilot captures.
* `[VERIFIED]` no golden-file or multi-version fixture set exists for any TUI vendor.
  `fixtures/adapters/` holds exactly one `session.jsonl` per TUI vendor, plus a `tui-screens/`
  directory of raw screen captures.

**Absence of evidence is not evidence of stability, and this record does not claim stability it has
not verified.** The honest formulation, and the one this record relies on: **a format change is
possible and unhandled.** That is a weaker claim than the comment at
`crates/runtime/src/adapter/tui/copilot.rs:179-180` asserts, and it is the only one the evidence
supports. It is also sufficient, because the mechanism below is chosen to bound the consequence of
a format change rather than to prevent one by guessing which versions changed.

## The replacement mechanism

### Decision 1 — an unrecognised entry type degrades, and is never silently dropped

`[AGENT-PROPOSED]`. `[VERIFIED]` `map_entry` at `crates/runtime/src/adapter/tui/copilot.rs:251`
dispatches on the entry type, and its catch-all arm at `:291` is `_ => (Vec::new(), None)`,
commented at `:286-290` as *"Never surfaced, never an error -- mirroring codex's
token_count/exec_* handling."* Copilot is the only vendor that discards it:

* `[VERIFIED]` Claude, `crates/runtime/src/adapter/tui/claude.rs:338` — `other => events.push(TuiEvent::Raw { … })`
* `[VERIFIED]` Codex, `crates/runtime/src/adapter/tui/codex.rs:341-346` — `other => (vec![TuiEvent::Raw { … }], None)`
* `[VERIFIED]` OMP, `crates/runtime/src/adapter/tui/omp.rs:497-502` — `_ => (vec![TuiEvent::Raw { … }], entry_id)`

Decision 1 replaces the arm at `crates/runtime/src/adapter/tui/copilot.rs:291` with Copilot's
counterpart. The arms that are deliberately empty stay empty and are enumerated by name so a later
reader does not read this as reversing them.

**What `Raw` actually is, stated at the confidence earned.** `[VERIFIED]`
`crates/runtime/src/adapter/tui/mod.rs:188` declares the variant as `Raw { entry_type: String }` —
it carries the vendor's own type tag and **nothing else**; it has no entry-id field. `[VERIFIED]`
line 336 of the same file, inside `fn emits_a_payload` (`:328`), matches
`TuiEvent::Raw { .. } => false`, and `[VERIFIED]` the dispatch at
`crates/runtime/src/adapter/tui/adapter.rs:1692-1694` handles the variant by emitting a
`tracing::debug!` with the entry type and nothing else. **A `Raw` event does not reach the durable
journal.** It is a typed, named, observable record in the trace, not a journal row.

This corrects an overclaim in the withdrawn draft, which asserted that an unrecognised entry
*"reaches the journal as `Raw` instead of vanishing"*. It does not reach the journal; it reaches
the trace. The decision this record makes does not depend on the difference, and the difference is
stated because the record must not overclaim what the degradation is worth: **the entry is not
lost silently, and it is not preserved in the journal either.**

Note also `[VERIFIED]` that Copilot already degrades in one direction today: the test
`malformed_lines_degrade_to_raw_not_errors` at `crates/runtime/src/adapter/tui/copilot.rs:631`
proves a line that is not valid JSON reaches `Raw`, per the shared parser contract documented at
`crates/runtime/src/adapter/tui/mod.rs:253` (*"a line that is not valid JSON degrades to
[`TuiEvent::Raw`] with `entry_type: "parse_error"`"*). What is discarded today is a **valid JSON
line carrying an entry type this adapter has not been taught** — the case a vendor's own new
release introduces.

### Decision 2 — the value from the out-of-band `--version` probe is recorded per run, as an inference about the harness rather than a self-declaration, and is not a gate

`[USER-STATED]` **2026-09-29, verbatim: "probe per run"** — which is the whole of the owner's answer,
given against the question of *how* Crew should record a harness version. It is labelled
`[USER-STATED]` **for this decision only**. The rest of this record is `Proposed` and
`[AGENT-PROPOSED]` where marked, and is unchanged. **What has been ruled is what the record *is*
and when it is taken; what Crew must build to make the recording true has not been ruled, and is
`[AGENT-PROPOSED]`** — see the closing paragraph of this section.

**The decision, stated in full.** Crew records the probe value **per run**, for **all four
harnesses**, through **one mechanism**. The value is the out-of-band `--version` probe's, which all
four adapters already perform identically. It is **recording, not gating**: it decides nothing about
whether Crew will drive a harness. That is the same distinction Decision 3 draws, and recording a
probe's output does not reintroduce the hand-maintained bound Decision 3 removes — a value written
down is not a list consulted.

**The mechanism, and the evidence that it is one mechanism for all four rather than four.**
`[VERIFIED]` `ProbeResult` is declared at `crates/runtime/src/adapter/trait.rs:26` and its version
field at `:27-28`, documented as *"The installed vendor CLI/tool version, if determinable."*
`[VERIFIED]` it is filled from trimmed `--version` stdout at
`crates/runtime/src/adapter/tui/adapter.rs:2638-2639`, from the value trimmed at `:2630` out of the
child process spawned at `:2619-2621`. **That is one function serving four vendors, not four
functions**: `TuiAdapter::probe` is declared at
`crates/runtime/src/adapter/tui/adapter.rs:2609` and reaches whichever vendor it holds through
`self.vendor.launch` at `:2618`, so every vendor gets the same out-of-band probe. The four
per-vendor conformance probes are the same shape, written out independently — `.arg("--version")` at
`crates/runtime/src/adapter/tui/claude_conformance.rs:124`,
`crates/runtime/src/adapter/tui/codex_conformance.rs:98`,
`crates/runtime/src/adapter/tui/copilot_conformance.rs:98` and
`crates/runtime/src/adapter/tui/omp_conformance.rs:97`, each trimming stdout at `:128`, `:102`,
`:102` and `:101` of its own file.

**And it is discarded at both ends of its path.** `[VERIFIED]` the conformance path's value is used
as a cache-invalidation stamp and dropped at `crates/runtime/src/adapter/registry.rs:1226`, where
`(*stamped_version == probed_version).then_some(*cached)` validates a memoized conformance suite and
then goes out of scope. `[VERIFIED]` `ProbeResult` itself is dropped whole at
`crates/runtime/src/adapter/tui/mod.rs:502-503`, where the live scenario matches
`adapter.probe().await` against `Ok(_)` and keeps only the pass/fail verdict. **The value is
therefore already produced, identically, for all four harnesses, and already thrown away at both
points that receive it. Recording it is a change of destination, not a new probe.**

**Why this record may no longer call the value self-declared — the honest limit of the evidence.**
The out-of-band probe is a **separate process invocation** of the vendor binary, so what it returns
is **an inference Crew makes about the harness**, not a statement the harness makes about itself.
That is why the heading above reads *inference* where this section previously read *self-declared*,
and it is a genuine weakening of the claim rather than a change of vocabulary: a probe reads
whatever the binary prints, while a session that reports its own version is evidence the vendor
chose to emit. `[VERIFIED]` **no adapter reads an in-band version today.** `copilotVersion` appears
nowhere under `crates/`. The `session_meta` arm at `crates/runtime/src/adapter/tui/codex.rs:323-335`
reads `session_id`/`id` and nothing else. What the vendors do state in-band, recorded here so the
change of mechanism is not mistaken for a claim that no in-band field exists: Copilot
`copilotVersion` in `session.start`'s `data`
(`fixtures/adapters/copilot-tui/session.jsonl:1` — `"copilotVersion":"1.0.80"`); Codex
`cli_version` in `session_meta` (`fixtures/adapters/codex-tui/session.jsonl:1` —
`"cli_version":"0.149.1"`); Claude `version` on each entry
(`fixtures/adapters/claude-tui/session.jsonl:3` — `"version": "2.1.241"`). `[VERIFIED]` **OMP states
no harness version at all**: its `"version": 3` at `fixtures/adapters/omp-tui/session.jsonl:2` is a
**transcript schema** version, as the adapter's own comment at
`crates/runtime/src/adapter/tui/omp.rs:46-51` says in its own words — the transcript
*"self-describes its own schema (`"version": 3` on its `session` line)"*.

**The OMP gap this decision closes, and the one it leaves standing.** The question this answers was
framed around a vendor that does not self-report a harness version. **Recording the probe value per
run answers it for OMP on the same terms as for the other three, which is the whole point of the
mechanism being one** — the probe exists for all four, so OMP stops being a case that needs its own
answer. What remains open, and this decision **does not** reach, is whether Crew should
*additionally* record the structured **in-band** field for the three vendors that have one. That was
one of the options put to the owner, and `"probe per run"` neither accepts nor rejects it. It is
carried as an open question in *What is not decided here*.

**`[AGENT-PROPOSED]` — what the ruling does not settle, and what no answer to this question could.**
The decision fixes *what* is recorded and *when*. It does not fix **where**. `[VERIFIED]`
`ProbeResult` carries no `run_id` — its four fields are declared at
`crates/runtime/src/adapter/trait.rs:26-40` — the `events` table created in `MIGRATION_1` at
`crates/runtime/src/db/migrations.rs:20-26` has no version column, and no variant of
`AdapterEventPayload`, declared at `crates/runtime/src/adapter/event_sink.rs:63`, carries one. So
*"per run"* is not yet true of anything: it needs a binding to a run, a destination that survives
process exit, and a migration, and none of those three is supplied by answering this question. They
are filed as build work rather than designed here. **A note on this record's own title:** its H1 at
line 1 carries the same "self-declared" wording this decision no longer uses, and it is **left
unchanged** — `docs/adr/README.md:87` still cites the withdrawn earlier title, so editing the H1
here would substitute one title inconsistency for another, and reconciling the two is filed as its
own work rather than done inside a decision edit.

### Decision 3 — no version gate decides whether Crew may drive the harness

**Stated once per harness, because the four gates are four different pieces of code.**

* **Copilot.** `[AGENT-PROPOSED]`, following the ruling and carried by the `[VERIFIED]` case
  above. The substring search at `crates/runtime/src/adapter/tui/copilot.rs:188` and the list at
  `crates/runtime/src/adapter/tui/copilot_compatibility.rs:36` stop being the mechanism that decides
  whether a Copilot session starts.
* **Claude.** `[AGENT-PROPOSED]`, **proposed, not evidenced as Copilot's is.** The range check in
  `version_gate` at `crates/runtime/src/adapter/tui/claude.rs:209`, against
  `MIN_TESTED_VERSION`/`MAX_TESTED_VERSION` declared at `:38-39`, stops deciding whether a Claude
  session starts. Grounded in the plural words of the ruling and in that gate's own admission at
  `:36` that the range beyond `2.1.241` is *"an untested extrapolation, not a second data point"*.
* **Codex.** `[AGENT-PROPOSED]`, proposed on the same footing. The range check in `version_gate` at
  `crates/runtime/src/adapter/tui/codex.rs:227`, against the constants at `:35-36`, stops deciding
  whether a Codex session starts; that adapter's own comment at `:34` calls everything outside
  `0.149.1` *"an untested extrapolation until a live smoke run widens it"*.
* **OMP.** `[AGENT-PROPOSED]`, proposed on the same footing. The range check in `version_gate` at
  `crates/runtime/src/adapter/tui/omp.rs:285`, against the constants at `:52-53`, stops deciding
  whether an OMP session starts; that adapter's own comment at `:44` uses the same words about
  everything outside `18.0.5`.

**What these four statements do not have in common is the evidence, and the record marks it that
way throughout.** Copilot's is `[VERIFIED]` — a falsified policy, an uncalled correct predicate,
and a claim with no artefact behind it. Claude's, Codex's and OMP's are `[AGENT-PROPOSED]` and rest
on two grounds only: the ruling names harnesses in the plural, and each gate's own comment says
almost all of its range is untested. **They have no falsified policy, no over-admitting
comparison, and no dead-but-correct predicate** — those three are Copilot-only findings, listed as
such under *What is not verified about the other three*. Extending a ruling whose subject the
owner did not itemise is a reading, and it is offered as one.

**A note on Claude's protocol mode, which has a second gate of its own.** `[VERIFIED]`
`crates/runtime/src/adapter/claude_protocol/posture.rs:57` declares
`pub(crate) fn version_gate(reported: &str) -> Result<(), String>` over its own range —
`MIN_TESTED_VERSION`/`MAX_TESTED_VERSION` declared at `:34-35` — called on `system/init`'s own
`claude_code_version` at `crates/runtime/src/adapter/claude_protocol/reader.rs:497`, where a
failure aborts the turn. `[AGENT-PROPOSED]` **the policy above reaches it for the same reason it
reaches Claude's TUI gate**: it is another hand-maintained version bound, and its own comment at
`posture.rs:32-33` says the same thing Claude's TUI comment says — *"the rest of the range is an
untested extrapolation, not a second data point"*, against one captured version, `2.1.268`. It is
named here rather than left for an implementer to trip over. Whether the protocol-mode adapter
adopts Decision 2's recording arm, and how it reports a missing `claude_code_version` at all, is
**not decided here** — see *What is not decided here*.

**The order is part of the decision, and it binds Copilot specifically.** Decision 1 is not a
tidy-up for Decision 3. Removing Copilot's gate while its catch-all arm still discards converts a
clean refusal into a session that starts and loses every entry type it has not been taught.
Decision 1 lands before Decision 3 for Copilot, and that ordering is explicit so an implementer
following this record cannot take Decision 3 first. **For Claude, Codex and OMP the ordering is not
load-bearing in the same way**, and the record does not claim it is: `[VERIFIED]` all three already
degrade — their catch-all arms are at `crates/runtime/src/adapter/tui/claude.rs:338`,
`crates/runtime/src/adapter/tui/codex.rs:341-346` and `crates/runtime/src/adapter/tui/omp.rs:497-502`
— so removing their gates removes a refusal without opening a discard path. That asymmetry is a
fact about the current code, not a licence to reorder Copilot's two decisions. The trade Copilot's
ordering makes is stated plainly under Negative Consequences: a session starts on a build whose
format may have changed, and the degradation bounds the damage without preventing it.

### What Copilot is not missing, and must not be "fixed"

`[VERIFIED]` all four vendors drop unknown *content block* types deliberately:
`crates/runtime/src/adapter/tui/claude.rs:536` and `crates/runtime/src/adapter/tui/omp.rs:565` each
drop the catch-all in a bare `_ => {}` arm, and `crates/runtime/src/adapter/tui/codex.rs:397`
returns `_ => (Vec::new(), entry_id)` with the comment at `:392-396` that reasoning is
*"the model's hidden thinking -- never surfaced"*. Copilot has no equivalent arm because its
content is a bare string with no block type. Those arms are correct as written, and this record
does not touch them.

## Decision Drivers

* **All four.** A version bound maintained by hand goes stale against daily vendor updates. A
  range is a wider hand-maintained bound, not a different kind of one. This is the one driver that
  reaches every harness on its own evidence, and it is the driver Decision 3's reach to the other
  three rests on.
* **Copilot only.** A gate's stated policy and its implementation must not disagree. The comment at
  `crates/runtime/src/adapter/tui/copilot.rs:180` of the range `:175-183` is falsified by `:188`.
  No equivalent disagreement is shown for the other three, and this record does not assert one.
* **Copilot only.** An entry type this adapter has not been taught must not be discarded with no
  record anywhere. `[VERIFIED]` the other three already satisfy this today, which is why Decision 1
  is a Copilot change.
* **Copilot only.** The justification for a guard must name a protocol this repository actually
  speaks. `[VERIFIED]` the other three's range comments name no protocol.
* **Copilot, Codex and Claude.** The vendor already states its own version in-band; a second copy
  in Crew's source is a snapshot that can only go stale. **Not available for OMP** — see Decision 2.

**Drivers 2, 3 and 4 are Copilot-only findings, and the record marks them so rather than letting
them read as the argument for all four.** Driver 1 is the only one carrying the general policy.

## Considered Options

1. Keep the exact-match list, unchanged. *(Copilot-shaped; "the exact-match list" exists only
   there.)*
2. Keep a list but fix only the substring overreach, by calling the existing exact predicate.
   *(Copilot only — no other harness has a substring search or a dead predicate.)*
3. Widen the gate to a range like the other three.
4. Remove the gate without adding degradation.
5. Remove the gate, add degradation, and record the vendor's version instead of gating on it.
6. Gate on something other than the CLI version string.
7. Also add `Raw` to content-block handling, for symmetry with the other three vendors.
8. Keep Claude's, Codex's and OMP's ranges exactly as they are. *(The general alternative, and the
   only option here that addresses all three of them.)*

**Options 1, 2, 4 and 7 are Copilot's alternatives and have no counterpart for the other three**,
whose gates are already ranges with no substring overreach, no dead predicate and no discarding arm,
so no per-harness version of them exists to weigh. `[AGENT-PROPOSED]` **option 5's removal arm is
what the policy proposes for all four; its two companions — degradation and recording — are not
equally available**, degradation already being the case for Claude, Codex and OMP and recording
being *unavailable* for OMP. Option 8 is the alternative the owner would be choosing against if
the ruling reached only Copilot, and it is stated here so that choice is visible rather than
implied.

## Decision Outcome

**Chosen: option 5 — degrade, then record, and let no version bound decide.** For Copilot that is
all three arms of option 5. For Claude, Codex and OMP the **removal** arm is what this record
proposes, on `[AGENT-PROPOSED]` grounds rather than `[VERIFIED]` ones: the other three already
degrade, and for OMP there is nothing to record.

Decision 1. An unrecognised Copilot entry type degrades to `TuiEvent::Raw` carrying the entry type,
mirroring the arms at `crates/runtime/src/adapter/tui/codex.rs:341-346` and
`crates/runtime/src/adapter/tui/omp.rs:497-502`, and replacing the discarding arm at
`crates/runtime/src/adapter/tui/copilot.rs:291`.

Decision 2. `[USER-STATED]` **The value from the out-of-band `--version` probe is journalled per
run**, for all four harnesses and through one mechanism, **as an inference Crew makes about the
harness rather than a self-declaration by the session**. Crew keeps no list of known versions, and
a build is not refused because its version is absent from one. **What and when are ruled; the
binding, the destination and the migration are not, and are filed as build work.**

Decision 3. **No version gate decides whether Crew will drive any of the four harnesses**, stated
once per harness above and labelled there: Copilot's is `[VERIFIED]`-grounded, and Claude's,
Codex's and OMP's are `[AGENT-PROPOSED]` — reasoned from the ruling's plural wording and from each
gate's own admission that its range is untested, and **not** carrying Copilot's falsified-policy or
uncalled-predicate findings. For Copilot specifically, `version_gate` at
`crates/runtime/src/adapter/tui/copilot.rs:184` no longer decides whether a session starts; what
the guard's dead predicates — `copilot_cli_version_known` and its two siblings at
`crates/runtime/src/adapter/tui/copilot_compatibility.rs:68`, `:80`, `:87` — are replaced with, or
whether they survive, is an implementation question this record does not settle. What is settled
for all four is that no version bound governs the decision.

Decision 4. `[VERIFIED]` the rationale at `docs/compatibility.md:226-228` and the table at
`docs/compatibility.md:230-235` state a gate policy for all four vendors that would no longer exist
— the whole table, not only Copilot's row. **That edit has not been made** — it is a documentation
change outside this record, and until it lands the documentation contradicts this record. This
record is the newer statement of the same fact.

**This record is a proposal, not a decision.** Until the owner ratifies it, Decisions 1, 3 and 4
are `[AGENT-PROPOSED]`, the code they describe is not built, and the facts in "What would reverse
this" are why. **Decision 2 is the one exception and the exception is narrow:** its substance is
`[USER-STATED]` — the owner ruled *"probe per run"* on 2026-09-29 — while the record carrying it
remains `Proposed`, and answering that question is not ratifying this file.

### Positive Consequences

* **All four.** A hand-maintained version bound — a list or a range alike — stops being something
  Crew has to keep current against daily vendor updates. For Copilot that is the five-entry list at
  `crates/runtime/src/adapter/tui/copilot_compatibility.rs:36`; for the other three it is the
  `MIN_TESTED_VERSION`/`MAX_TESTED_VERSION` pairs at
  `crates/runtime/src/adapter/tui/claude.rs:38-39`, `crates/runtime/src/adapter/tui/codex.rs:35-36`
  and `crates/runtime/src/adapter/tui/omp.rs:52-53`.
* **Copilot only.** The gate's documented policy and its implementation stop disagreeing, because
  the policy is gone rather than restated. The other three's policies are not falsified, so there
  is nothing of this kind to stop.
* **Copilot only.** An entry type a future Copilot release introduces produces a named trace
  record instead of vanishing. `[VERIFIED]` Claude, Codex and OMP already produce that record
  today, so for them this is already true rather than newly true.
* **Copilot, Codex and Claude.** Crew's own source stops carrying a second copy of a version the
  vendor already states in-band. **OMP gains nothing here**, and the record does not pretend
  otherwise.
* **Copilot only.** The guard's justification stops inheriting from a deleted protocol: ACP is
  gone, and so is the guard it justified. The other three never inherited one.

### Negative Consequences

* **All four.** A protocol-level or format-level break is no longer refused at startup. **A format
  change is possible and unhandled**, stated at the confidence earned above: this record verified
  no evidence of a change between patch releases for any vendor, and none of stability either.
  `[AGENT-PROPOSED]` **for Copilot the damage is bounded** — it surfaces as entries the adapter
  cannot map, which Decision 1 turns into named `Raw` trace records. `[AGENT-PROPOSED]` **for
  Claude, Codex and OMP the existing degradation is what bounds it**, and that is verified in the
  code rather than proposed; but the bound is the same trace-not-journal bound, so it is a weaker
  guarantee than a refusal, not a different one.
* **Copilot only, and new.** The degradation is a trace, not a journal row. An unmapped entry is
  recorded in the log and absent from the journal. This is better than silence and worse than
  preservation, and the record says so rather than rounding it up. `[VERIFIED]` the other three
  already had this bound before this record; Copilot would acquire it.
* **OMP, and narrowed by the owner's ruling of 2026-09-29.** OMP has no in-band harness version in
  this repository, so the *in-band* half of Decision 2 is still not uniformly available across the
  four. **The recording question is no longer open**: the owner ruled *"probe per run"*, which
  answers it for OMP on the same terms as for the other three, because the out-of-band probe exists
  for all four. **What remains is whether Crew *additionally* records the in-band field for the
  three vendors that have one** — not reached by the ruling, and listed below.
* **Claude's protocol mode.** `[AGENT-PROPOSED]` removing that gate leaves it with no version
  assertion of its own, and this record does not say what should replace it.
* **The reach to three of the four is an inference.** `[AGENT-PROPOSED]` Claude's, Codex's and
  OMP's inclusion rests on the ruling's plural wording and on their own comments' admission that
  most of each range is untested. It does **not** rest on Copilot's evidence, and if the owner
  intended the ruling to name only Copilot, then this record's title overreaches and the three
  per-harness statements fall with it. That is a question for the owner, and it is the reason the
  record is `Proposed` and not `Accepted`.

### Pros and Cons of the Options

**Options 1, 2, 4 and 7 are Copilot's alternatives and are argued from Copilot's evidence.** None
of them has a per-harness equivalent for Claude, Codex or OMP, because their gates are already
ranges with no substring overreach, no dead predicate and no discarding arm. `[AGENT-PROPOSED]`
**They are therefore proposed as the treatment for the other three rather than evidenced as it** —
the only option below that speaks to all four is option 8, and it is the one the ruling's own words
carry. Option 6 is Copilot's as well, with one addition: it also names a gate outside the TUI set,
which the decision now reaches.

**Option 1 — keep the exact-match list, unchanged.** Rejected. It is what the ruling rejects. The
list is a snapshot of five builds taken against an auto-updater, and `[VERIFIED]` the repository
records no instance of it ever preventing anything: all six live conformance reports pass the
probe, and the five-entry "identical handshake" claim has no surviving artefact.

**Option 2 — keep the list, fix only the substring overreach.** Rejected as an answer, and it was
the proposal this record first carried. `[VERIFIED]` it is a real improvement on its own — calling
the predicate at `crates/runtime/src/adapter/tui/copilot_compatibility.rs:68` would make the
implementation match the comment, and it would refuse `1.0.800` and `0.1.0.8000`. But it makes a
never-decided policy correct instead of asking whether the policy is right, and it leaves Crew
refusing builds against a hand-maintained list, which is the thing the ruling rejects.

**Option 3 — widen the gate to a range.** Rejected by the ruling, and the reason generalises.
`[USER-STATED]` *"I say, we do not! At least not up to the patch."* **The phrase *"up to the patch"*
is what rejects this option**, because a range is still a hand-maintained version bound, and it is
the bound, not the mechanism, that goes stale. `[VERIFIED]` the three existing ranges are exactly
that: `crates/runtime/src/adapter/tui/claude.rs:38-39`, `crates/runtime/src/adapter/tui/codex.rs:35-36`
and `crates/runtime/src/adapter/tui/omp.rs:52-53` each declare a `MIN_TESTED_VERSION` and
`MAX_TESTED_VERSION` pair that someone must edit when a vendor moves, and each adapter's own comment
says so — `claude.rs:36` *"an untested extrapolation, not a second data point"*,
`codex.rs:34` and `omp.rs:44` both *"an untested extrapolation until a live smoke run widens it"*.
Against daily updates that is a larger window to go stale, not a different kind of window. It would
also have made Copilot's gate *look* like the other three without the other three being a reason.

**Option 4 — remove the gate without adding degradation.** Rejected. This converts a visible,
recoverable refusal into silent loss. With the arm at `crates/runtime/src/adapter/tui/copilot.rs:291`
still discarding, a user debugging it sees a journal missing entries with no error, no event and no
trace anywhere — harder to diagnose than a refusal, and it loses data. The ordering is why Decision 1
is named first.

**Option 5 — the chosen one.** Degradation bounds the consequence of a format change without
requiring Crew to predict which versions changed; recording the version keeps the evidence without
making it a gate. The ordering is explicit, so an implementer following this record cannot take
Decision 3 first.

**Option 6 — gate on something else entirely.** Rejected, and no better instrument is available
here. `[VERIFIED]` the nearest candidate is the structured in-band field rather than `--version`
stdout: `crates/runtime/src/adapter/claude_protocol/posture.rs:57` declares
`pub(crate) fn version_gate(reported: &str) -> Result<(), String>` over `system/init`'s own
`claude_code_version`, and its doc comment at `:50-55` explains why that field is safe to trust
where probed `--version` stdout is not. **That is the one option whose rejection reaches past the
TUI set**: it is a fifth version bound, over `MIN_TESTED_VERSION`/`MAX_TESTED_VERSION` declared at
`posture.rs:34-35` and called at
`crates/runtime/src/adapter/claude_protocol/reader.rs:497`, and its own comment at `:32-33` says
the same *"untested extrapolation, not a second data point"* Claude's TUI comment says. Structuring
the input more carefully does not change what a bound is. `[AGENT-PROPOSED]` **what replaces it,
if anything, is not decided here** — see *What is not decided here*.

**Option 7 — also add `Raw` to content-block handling.** Rejected on the evidence, and doing it
would be a defect. The dropping arms at `crates/runtime/src/adapter/tui/claude.rs:536`,
`crates/runtime/src/adapter/tui/codex.rs:397` and `crates/runtime/src/adapter/tui/omp.rs:565` are
deliberate: reasoning is the model's hidden thinking and must never surface. Copilot's content is
a bare string with no block type, so it has no such arm to change. Symmetry here is a leak of model
reasoning, not parity.

**Option 8 — keep Claude's, Codex's and OMP's ranges as they are.** Rejected, `[AGENT-PROPOSED]`
and on the thinnest ground in this record: the ruling's plural wording plus each gate's own
admission that almost all of its range is untested. There is **no** falsified policy, no
over-admitting comparison and no dead-but-correct predicate behind this rejection, unlike the
rejection of options 1 and 2 — this option is declined because a hand-maintained bound goes stale
against daily updates, which is the same disease Copilot's gate has and the same reason the ruling
reaches it, but it is **not** declined on evidence of a defect. The three adapters' comments are
truthful about their own limits, which is to their credit and is not a reason to keep the bounds.

## What is not decided here

Five questions are open and this record answers none of them. **A sixth was answered by the owner
on 2026-09-29 and is struck here rather than deleted, so the history stays legible.**

* ~~**What Crew records for a vendor that does not self-report a harness version — OMP.**~~
  **ANSWERED 2026-09-29.** `[USER-STATED]`, verbatim: **"probe per run"** — recorded as Decision 2
  above. Crew records the out-of-band `--version` probe value per run, for all four harnesses
  through one mechanism, as an inference about the harness rather than a self-declaration. This
  closes the question **for OMP on the same terms as for the other three**, which is what makes the
  single mechanism the answer rather than a workaround. The premise the question rested on is
  unchanged and still verified: `[VERIFIED]` OMP's transcript states no harness version, the
  `"version": 3` on its session line (`fixtures/adapters/omp-tui/session.jsonl:2`) being a schema
  version as `crates/runtime/src/adapter/tui/omp.rs:47` says. **What the ruling did not settle is
  recorded as still open below, and must not be read as answered by association.**
* **Whether Crew should *additionally* record the in-band version field for Claude, Codex and
  Copilot.** **OPEN — NOT REACHED BY THE RULING.** `[USER-STATED]` *"probe per run"* answers how
  the value is obtained; it neither accepts nor rejects recording the vendors' own structured
  declaration as well, which was one of the options put to the owner. `[VERIFIED]` the three fields
  exist and are unread: `fixtures/adapters/copilot-tui/session.jsonl:1` carries
  `"copilotVersion":"1.0.80"`, `fixtures/adapters/codex-tui/session.jsonl:1` carries
  `"cli_version":"0.149.1"`, `fixtures/adapters/claude-tui/session.jsonl:3` carries
  `"version": "2.1.241"`, and `copilotVersion` appears nowhere under `crates/`. **The two are not
  interchangeable** — an in-band value is the vendor declaring itself, and the probe is Crew asking
  a process it spawned — and the owner has not said which, or both, Crew records. OMP has no in-band
  field at all, so any such decision is three-of-four and would leave the asymmetry standing.
* **What replaces Claude's protocol-mode version gate.** `[VERIFIED]`
  `crates/runtime/src/adapter/claude_protocol/posture.rs:57` gates a protocol-mode Claude turn on
  the range at `:34-35`, called at
  `crates/runtime/src/adapter/claude_protocol/reader.rs:497`. `[AGENT-PROPOSED]` the policy above
  reaches that gate, and `[AGENT-PROPOSED]` nothing in this record replaces it — including whether
  the protocol-mode adapter should adopt Decision 2's recording arm, given
  `[VERIFIED]` `system/init` already carries a structured `claude_code_version` that this record has
  not proposed journalling. Left open.
* **Whether the ruling's plural reaches three of the four.** `[USER-STATED]` the ruling says *"the
  version of each harness"* and *"that version guard on harnesses"*, naming none. This record reads
  that as four. **If the owner meant only Copilot, then this record's title overstates it**, and the
  correct outcome is option 8 — keep the three ranges — for which this record's own evidence is
  sufficient and sufficient only. The question is stated here rather than assumed away, and it is
  the main thing ratification would settle.
* **The `ADR-0039` backstop size.** `[VERIFIED]` `docs/adr/0039-instruct-then-kill-on-timeout.md:12-13`
  carries it as an open item in its own right — the owner was offered a number for that backstop and
  declined it. Whether removing a version gate changes what a backstop has to cover is not settled
  by this record and the size is not chosen here.
* **ADR ratification generally.** This record is `Proposed`. Ratifying it, and the order the other
  Proposed records are ratified in, are separate questions.
* **Whether Claude's protocol mode can deliver a death notice.** `[VERIFIED]`
  `crates/runtime/src/adapter/claude_protocol/adapter.rs:470-474` escalates SIGINT → SIGTERM →
  SIGKILL on the child's own exit. Whether a protocol-mode run can announce its own end through
  the protocol, rather than only being reaped by signal, is **UNVERIFIED** in this record and is
  left open.

## What would reverse this

* Evidence that a release inside any plausible bound changed the transcript field names this
  adapter reads, in a way the `Raw` degradation does not bound. That would be the first evidence
  bearing on this decision from the other direction, and it would make a narrower instrument — a
  minor-version bound, or a per-release verified list — the right one again. **No such evidence
  exists in this repository today, in either direction, for any of the four vendors.**
* The vendor shipping a change Crew genuinely cannot degrade from, at which point refusing to start
  is the honest behaviour and a gate is the instrument.
* A recorded run in which the substring search at `crates/runtime/src/adapter/tui/copilot.rs:188`
  admits a build that then misbehaves, which would make the overreach a live defect rather than a
  documentation inconsistency.
* **For the other three, the reversing evidence is weaker than Copilot's and would have to be
  stronger.** Their gates admit only what is inside a broad range and refuse only what is outside
  it, so a recorded failure would most likely be a build outside the range — which is the bound
  working as written. `[VERIFIED]` no such recorded failure exists in this repository for any of
  the four.

## Links

* [0026](0026-headless-retirement.md) — deletes the headless adapters and moves this table; states
  no gate policy.
* [0037](0037-protocol-first-control-plane.md) — drives workers over the vendors' own protocols, so
  a version gate is part of the transport assumption this record removes.
* [0039](0039-instruct-then-kill-on-timeout.md) — the backstop size it leaves open.
* [0014](0014-flat-op-discriminator-over-zod-discriminated-unions.md) — a discriminator's unknown
  arm, and why this repository's bias is to preserve what it does not recognise.
* [README.md](README.md) — the index row at line 87 that cites this record's unchanged filename.
