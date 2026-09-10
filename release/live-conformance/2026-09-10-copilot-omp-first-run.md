# Copilot and omp first-run surfaces — direct measurement, 2026-09-10

**What this establishes: copilot raises a folder-trust dialog whose focused default is "Yes", so
an unattended Enter grants filesystem trust — the same hazard already measured on codex. omp raises
no trust dialog at all; it raises a five-step first-run setup wizard that blocks the composer until
it is completed or skipped, and whose first step confirms a provider sign-in on Enter.**

Vendors under test: GitHub Copilot CLI 1.0.81 and 1.0.83; omp 18.1.15 and 18.1.16. Host: macOS
arm64.

**Both vendors self-updated during the probe window**, which is a fact about this kind of record
rather than an aside: copilot moved 1.0.81 → 1.0.83 and omp 18.1.15 → 18.1.16 between the first
observation and the committed captures, without being asked. Every committed capture was therefore
re-taken with the version checked immediately before and after, and each one now carries its own
version string in-band — `1.0.83` in the copilot dialog, `omp v18.1.16` in the composer frame — so
it can be pinned from the bytes rather than from this sentence. The wizard capture carries no
version string and is 18.1.15, verified at capture time; that is the one claim here that rests on
the record rather than on the file.

The 2026-09-09 record closed the equivalent question for claude and codex and stated its own gap:
*"none about copilot or omp-rpc, which were not exercised."* This record closes that gap by
measurement. Neither vendor behaves the way the claude/codex pattern predicts.

This record reports what was observed. It makes no claim about vendor versions other than the two
named, and none about either vendor's screens after setup completes, which were not reached — see
*Limits*.

---

## Context from vendor documentation (not measured here)

The following was established by reading vendor documentation, changelogs and shipped source. It
is recorded because it changes what the measurements above mean, and it is separated from them
deliberately: **nothing in this section was observed on a PTY.**

**Trust is keyed on the repository, and a linked worktree inherits it.** Claude, codex and copilot
all key folder trust on the git main-checkout root and treat a `git worktree add` workspace as the
same repository. A crew worker running in a worktree of an already-trusted repository therefore
sees no dialog. Claude and codex document this. Copilot's changelog claims it from 1.0.60, but the
resolution lives in a compiled module — so it was unverifiable by reading, and **was measured
instead**; see *Copilot worktree trust inheritance* below.

This reframes the dialog seen during the third live end-to-end attempt: it appeared because the
target repository had never been trusted, not because worktrees defeat trust inheritance.

**The documented hand-written trust entries**, for an operator trusting a repository once per
vendor:

| Vendor | File | Entry |
|---|---|---|
| claude | `~/.claude.json` | `projects["<repo root>"].hasTrustDialogAccepted = true` |
| codex | `~/.codex/config.toml` | `[projects."<repo root>"] trust_level = "trusted"` |
| copilot | `~/.copilot/config.json` | `"trustedFolders": [ "<abs path>" ]` — seed both the path and its realpath |

Crew does not write these and will not: detect-and-escalate stays, and the escalation question
tells the operator to trust the repository once per vendor.

**No permission-bypass flag suppresses the dialog.** Not claude's
`--dangerously-skip-permissions`, not codex's `--dangerously-bypass-approvals-and-sandbox`, not
copilot's `--allow-all-tools` or `--yolo`. Copilot's `COPILOT_ALLOW_ALL=true` does bypass trust in
the shipped bundle, but it is a blanket tool auto-approval and is ruled out for that reason — the
trust dialog is the smaller problem.

**omp has no per-directory trust at all**, anywhere in its source. Its wizard is global, and is
skipped by `OMP_SKIP_SETUP=1` or by a `setupVersion` entry in `config.yml`. That is the
documentary half of what the captures above show, and it is why omp's share of this work is
minimal.

---

## Method

Each vendor was launched on a PTY with the argv crew's own adapter builds for the default
permission mode (`copilot`; `omp --allow-home`), in a brand-new directory the vendor had never
seen, with `HOME` pointed at a throwaway profile per vendor and the XDG variables cleared. The
terminal was sized 40×120.

**No prompt was ever written to the PTY, no trust was granted, and no provider was selected.** The
harness owned each child for its whole life and terminated it itself; no vendor process outlived a
capture.

Two capture shapes were used, and the difference matters when reading the fixtures:

- **From spawn** (copilot): every byte from launch onward, preserving the *transition* into the
  dialog the way `codex-composer-then-trust.raw` does. Taken against an already-warm profile, so
  the recording is the folder gate rather than the vendor's own first-run splash — the trust gate
  fires per unseen directory regardless of profile age, so nothing about the gate is lost and
  69 KB of unrelated startup is.
- **Settled repaint** (omp): wait for the PTY to fall quiet, resize by one column, `SIGWINCH`, and
  record only the resulting redraw. omp animates its initial paint — 2,862 reads and **2.85 MB** in
  about three seconds — so a from-spawn recording is roughly a hundred times the entire existing
  fixture corpus for one screen. The repaint discards 2,852,666 bytes of animation and captures
  **7,685**, containing the identical settled grid a predicate classifies against.

`SIGWINCH` is a signal to the process, not terminal input; it advances no dialog.

Wizard steps 2–5 were reached by sending one `Esc` per step, which the wizard's own legend
documents as *skip*. `Esc` grants no trust and signs nothing in. This was authorised separately
from the initial probe. A sixth `Esc`, past the last step, is what exits the wizard and reaches the
composer — five are not enough, and a profile left mid-wizard raises it again on the next launch.

`OMP_SKIP_SETUP=1` would have reached the composer in one launch with no keystrokes at all. It was
found afterwards, in omp's source, while establishing that omp has no per-directory trust. Anyone
repeating this should use it; the captures here are what they are because it was not known at the
time.

### Two method failures worth recording

**`startup.showSplash` is not the cause and does not help.** omp exposes that config key; it was set
to `false` in a fresh throwaway profile and verified by reading it back. The capture came out at
2,871,191 bytes — marginally *larger* than with the splash enabled. Whatever animates omp's first
paint is not the splash, and the flag that looks like the answer is not one.

**A quiet PTY means two different things.** The first settled-repaint attempt reported
`discarded=0` and captured 2.84 MB — the whole animated startup — because the settle detector
treated "silent because the vendor has not begun painting" as "silent because it has finished".
Both are silence. The detector now requires output before quiet counts. The defect was visible only
because the discarded byte count was printed alongside the result; the capture itself looked
plausible.

---

## Copilot — folder trust, focused on "Yes"

Final screen (`copilot-folder-trust.raw`, 4,517 bytes, from spawn, Copilot CLI 1.0.83):

```
Confirm folder trust
  <the directory path>
  Copilot can read files in this folder and, with your permission, edit them or run code and
  shell commands. It will remember your permissions for the rest of this session.

  Do you trust the files in this folder?
  ❯ 1. Yes
    2. Yes, and remember this folder for future sessions
    3. No (Esc)
  ↑/↓ to navigate · enter to select · esc to cancel
```

**The selection marker sits on "Yes".** Enter, arriving unattended, grants the vendor read access
and permission-gated write and shell execution in whatever directory the worker was pointed at.
This is the codex hazard, in a vendor that is outside the park-and-escalate path as of this record.

Output ceased 7.3 s after launch and the PTY stayed silent for the remaining ~18 s of the capture
window, so the surface is stable once painted.

## omp 18.1.15 — a five-step setup wizard, no trust gate

The string `trust` appears **zero** times across all five captured frames. omp raises no
directory- or workspace-trust dialog.

It raises a wizard that blocks the composer. Each frame is a settled repaint:

| Step | What it asks | Focused default |
|---|---|---|
| `Setup step 1 of 5` | provider sign-in | `❯ ChatGPT Plus/Pro (Codex Subscription)` |
| `Setup step 2 of 5` | model selection | `❯ lm-studio/laguna-2.1-xs` |
| `Setup step 3 of 5` | glyph level | `❯ 2 Unicode` |
| `Setup step 4 of 5` | prompt layout | `❯ 1 Status Band (Default)` |
| `Setup step 5 of 5` | theme | `❯ Titanium, Default dark theme` |

Only step 1 is committed as a fixture (`omp-setup-step1.raw`), and as a **negative** sample. Steps
2–5 were captured and are reported here as observations; they are not fixtures because nothing
classifies against them — see *What this means for crew* below.

All five carry the identical legend `↑/↓ select · enter confirm · esc skip · ctrl+c exit setup`.

**Only step 1 is credential-consequential** — Enter there confirms the highlighted provider and
begins a sign-in. Steps 2–5 set cosmetic preferences. But every step blocks the composer, and Enter
on any of them commits a choice nobody made, which is why crew must not send one. Under the ruling
below, a wizard screen is `Undecided` and fails the start closed with a typed error naming what was
seen — not a hang, and not a keystroke.

Output ceased 6.3 s after launch on the from-spawn capture and the PTY stayed silent for the
remainder, so the surface is stable once painted.

### Phrases present in every frame, verified as literal bytes

Checked as contiguous byte sequences in each file, never read out of escape-stripped text:

| Phrase | s1 | s2 | s3 | s4 | s5 |
|---|---|---|---|---|---|
| `Setup step` | ✔ | ✔ | ✔ | ✔ | ✔ |
| `of 5` | ✔ | ✔ | ✔ | ✔ | ✔ |
| `ctrl+c exit setup` | ✔ | ✔ | ✔ | ✔ | ✔ |
| `enter confirm` | ✔ | ✔ | ✔ | ✔ | ✔ |
| `esc skip` | ✔ | ✔ | ✔ | ✔ | ✔ |
| `Setup step N of 5` | only its own frame | | | | |
| `trust` | — | — | — | — | — |

`ctrl+c exit setup` is the strongest single discriminator: one contiguous phrase, present in all
five, and bound to setup rather than to any screen that happens to be numbered. `enter confirm` and
`esc skip` are generic list-navigation legend text and are the wrong choice for that reason.

---

## What this means for crew

**Copilot's trust gate is a gate**, in the sense the park-and-escalate path already means: an
unattended Enter has a real and irreversible effect, so the adapter must recognise it, refuse to
write, and ask a person.

**omp's setup wizard is not**, and the difference is not about how it looks. The maintainer's
ruling: crew never launches omp under a fresh home, so a machine showing this wizard is one where
omp was never configured — an operator error, not a decision waiting for a human to make through
crew. Escalating it would ask someone to complete a five-step personalisation flow through a
worker pane, which is the wrong place for it.

So omp's predicate recognises omp's **normal prompt** and returns `PromptReady`. Everything else,
the wizard included, is `Undecided`, and an `Undecided` surface fails the start closed with a typed
error. A never-configured machine therefore gets an honest failure naming what crew saw, and never
an Enter into a provider sign-in. The documented remedy is to run omp once by hand before using it
as a worker.

That is why step 1 is committed as a negative sample: the predicate must be shown returning
`Undecided` on a real capture of the screen it is most likely to be wrong about. Steps 2–5 add
nothing to that proof, so they stay here as prose.

## Copilot worktree trust inheritance — measured

**A copilot worker in a linked worktree inherits the main checkout's trust and sees no dialog. The
stored path is the repository root, not the worktree.**

This one *was* observed on a PTY, unlike the section above. A real git repository was created under
a neutral scratch path with a `git worktree add` worktree, all under a throwaway `HOME`:

| Step | Result |
|---|---|
| grant trust in the main checkout | dialog shown; "Yes, and remember this folder" selected |
| relaunch in the main checkout | **no dialog** — the control: trust persisted |
| launch in the worktree | **no dialog** — the worktree inherits |

`~/.copilot/config.json` gained exactly one entry:

```json
"trustedFolders": [ "<repo root>" ]
```

stored as the **realpath** of the repository root. Nothing was written naming the worktree. That
confirms the 1.0.60 changelog claim, which until now rested on a compiled module, and it is why an
operator seeding this file by hand should write both the path and its realpath.

### The control is the only reason this is not the opposite finding

The first attempt sent Down then Enter on a timer to select "Yes, and remember". **The Down never
registered.** Enter took option 1 — session-only trust — nothing persisted, and the worktree then
showed the dialog. That reads exactly like "a worktree does not inherit trust", with a clean
capture behind it. Only re-launching in the *main checkout* first revealed that there was no
persisted trust to inherit.

The fix was to interlock on the intermediate state rather than the outcome: after the dialog is
confirmed present, wait for the paint to settle, send Down, and **refuse to press Enter until the
selection marker is verifiably on option 2**. The committed reasoning is that a keystroke is not an
event you can assume landed — the capture must show the screen responding to it before the next key
is sent. The final capture shows the marker on option 1 and then on option 2.

This is the same failure as the settle detector in *Method* above: a step that silently did nothing,
leaving an artefact that looked exactly like a successful run.

## Neither vendor positions words individually

Both write prose contiguously with real spaces: `Do you trust the files in this folder?` survives
escape-stripping intact, as does `Select provider to login`. This is the opposite of claude and
codex, whose per-word cursor-column escapes are the reason the screen primitive compares
whitespace-stripped text at all.

The primitive needs no change — stripping whitespace from both sides is a superset, so it still
matches — but the reason it exists does not apply to these two, and a reader of the copilot or omp
predicates should not infer that it does.

## A phrase read from stripped text is a hypothesis

An early reading of omp reported a legend `press enter to skip`, which would have inverted the
security conclusion for step 1. **It does not exist**: searched as literal bytes it returns no
match. The phrase was manufactured by escape-stripping, which concatenated characters from the
animated splash that were never adjacent on screen.

This is the mirror image of the per-word trap. There, stripping *loses* the spaces and hides a
phrase that is really on screen. Here, stripping *joins* fragments and invents one that is not.
Every phrase in this record was therefore re-checked as a contiguous byte sequence in the capture
before being written down, and any predicate built from these fixtures should be checked the same
way.

## Limits

- **Neither vendor's post-setup screen was reached.** omp's composer is blocked by the wizard on a
  fresh profile, and copilot's was not exercised. So no claim is made that these phrases are absent
  from either vendor's normal operating screens. A false positive there would park a ready run
  rather than write into a gate — it fails safe — but it is unmeasured.
- **Copilot showed no sign-in gate, and this record does not claim it has none.** Isolation covered
  `HOME` and the XDG variables but *not* the macOS Keychain, so existing operator credentials may
  have satisfied it. The status is the same as claude's unobserved sign-in gate: not observed, not
  ruled out.
- **Only these two versions were exercised**, on one host, at one terminal size.

## Fixture sizes

`copilot-folder-trust.raw` is 4,517 bytes, `omp-setup-step1.raw` 7,685, `omp-composer.raw` 17,488.
All sit inside the range the existing corpus already occupied (1,289–11,012 bytes) except the
composer, which is larger because it paints a full welcome panel. Every file is well under the 2 MB
threshold at which `scripts/check-markers.ts` skips a file — a 2.85 MB from-spawn omp recording
would have sat above it and been silently unscanned. The `fixtures/` directory is excluded from
that guard regardless, so this is a property worth keeping rather than one being relied on.

**No committed capture contains an absolute home path.** The first copilot recording did: its
dialog displays the directory it is asking about, so the scratch path went into the bytes, and
every pre-existing fixture contains zero such paths. All probes were re-run from a neutral
`/tmp` scratch directory. A byte-exact recording of a dialog that displays its own working
directory will bake whatever path it ran in into a public repository, and no text rule catches it
because `fixtures/` is exempt from the marker guard by design.
