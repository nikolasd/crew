# Vendor TUI screen captures

Raw PTY output from real vendor CLIs at their first-run gates, captured by
replaying crew's own prompt-delivery sequence against a brand-new directory.
They are the golden inputs for `crates/runtime/src/adapter/tui/screen.rs` and
for the vendor surface predicates built on it.

Each file is the unmodified byte stream the vendor wrote to the PTY, escape
sequences and all. That is the point: the escapes are what the code under test
has to cope with, so a fixture with them stripped would test nothing.

## Why these are captures and not hand-written strings

A vendor TUI positions **each word** with its own cursor-column escape, so the
escape-stripped text of a screen contains no spaces. A predicate written by
reading the dialog off a terminal and pasting the phrase into a test matches
nothing at runtime, and passes review because it looks obviously correct.

That failure happened during the investigation these fixtures come from: a
detector reported "no dialog on screen" against a screen that was displaying
the dialog. Only the raw bytes showed why. A predicate without a real capture
behind it is not reviewable.

## Contents

| File | Vendor | Gate |
|---|---|---|
| `claude-theme-picker.raw` | claude 2.1.265 | First-run theme selection |
| `claude-signin-method.raw` | claude 2.1.265 | Login-method selection |
| `claude-workspace-trust.raw` | claude 2.1.265 | Workspace trust ("Accessing workspace:") |
| `codex-signin.raw` | codex-cli 0.153.4 | Sign-in method selection |
| `codex-directory-trust.raw` | codex-cli 0.153.4 | Directory trust |
| `codex-composer-then-trust.raw` | codex-cli 0.153.4 | Composer painted, prompt accepted, **then** the trust gate |
| `claude-trust-to-composer.raw` | claude 2.1.265 | Trust gate **answered**, then the alternate-screen switch and the composer |
| `copilot-folder-trust.raw` | Copilot CLI 1.0.83 | Folder trust ("Confirm folder trust"), focused on **Yes** |
| `copilot-composer.raw` | Copilot CLI 1.0.83 | The normal prompt after trust is granted — what copilot's predicate must recognise as `PromptReady` |
| `omp-composer.raw` | omp 18.1.16 | The normal prompt — what omp's predicate must recognise as `PromptReady` |
| `omp-setup-step1.raw` | omp 18.1.15 | First-run setup wizard, step 1 of 5 — a **negative** sample: the predicate must return `Undecided` on it |

The codex composer-then-trust capture is not a duplicate. Codex paints its
composer first, accepts a pasted prompt into it, and only then raises its trust
gate, so a single capture holds both surfaces. Any predicate of the form "the
composer is up and no gate is up" has to be correct against it.

The last row is the only capture in which a gate is *answered*. It was taken by
driving the dialog the way a person does — a Down keystroke to move the
selection onto the accepting option, then Enter, delivered as keystrokes rather
than as a paste, so it does not double as a prompt-delivery recording. It holds
the sequence a resume path depends on: gate up, gate answered, the vendor
switching to the alternate screen, and the composer painted in its place. It is
also the only capture containing the alternate-screen switch at all, so a screen
model scoped to the other six will need widening for it.

It is the capture that makes the screen-matching primitive's stated limitation
concrete rather than argued. After the gate has been answered and replaced, the
primitive still reports the gate's phrase as on screen, because it accumulates
rather than models a terminal. Any screen model that replaces it must reverse
that specific assertion — the test says so in as many words, so the acceptance
criterion is a test to flip rather than a description to interpret.

That file is a prefix of its capture, truncated immediately before the first
account-specific content the vendor drew and backed off to the preceding line
boundary. Truncation is safe here in a way that editing would not be: a terminal
processes a prefix of a byte stream correctly, whereas substituting bytes inside
one produces a recording of something that never happened.

## How they were captured

A harness reproducing the adapter's sequence byte for byte: spawn on a PTY,
wait for first output, hold until the spawn-anchored injection floor, deliver
the prompt as a bracketed paste in chunked writes, wait for the output-idle
window, then write the single submit byte. Each capture used its own
brand-new directory. The vendor argv is the one the adapter builds for the
default permission mode.

The sign-in captures used a throwaway vendor config directory holding no
credentials (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), so no real account state was
read or written.

## What is deliberately absent

Two captures from the same investigation are **not** here, and neither is
reproduced in prose anywhere that would defeat the point of excluding them:

* A claude composer-ready screen. It carried an organisation announcement and
  a session name from the capturing machine. Excluded rather than scrubbed:
  the file is dense with absolute cursor-column escapes, so a substitution
  that preserved meaning would not reliably preserve layout, and one that
  preserved layout would not be honest about what the vendor drew.
* The claude OAuth screen that follows the login-method selection. It carries
  a PKCE code challenge and state parameter. They belong to an abandoned flow
  and authenticate nothing, but an auth artifact does not belong in a
  repository, so the capture stops before it.

Every file here has been checked to contain no home paths, usernames,
organisation names, session names, or auth artifacts.

## Adding to this set

Capture from a real CLI, in a directory that vendor has never seen, using the
argv the adapter actually builds. Record the vendor version in the table.
Check the bytes for machine- and account-identifying strings before committing
— these files are keystroke-level recordings of a real terminal, and the
things worth redacting are not visible when the capture is rendered.

Keep the probe's own prompt text neutral. Whatever is pasted into a vendor's
composer is drawn to the screen and lands in the recording, permanently: a
capture cannot be edited afterwards without destroying the property that makes
it evidence. Anything that would be unwelcome in the repository forever —
identifiers tied to a tracker or a person, paths, anything topical that will
read as stale — must not be in the prompt in the first place. Two captures here
carry an internal identifier for exactly this reason, and they are the reason
the repository's identifier rules exempt this directory.

Verify the staged blob's hash against the source capture before committing
(`git cat-file blob :<path> | shasum -a 256` against `shasum -a 256 < <source>`).
The `-text` attribute prevents end-of-line rewriting, but the hash comparison is
what proves it worked; see the fixture-integrity entry in
`docs/engineering-lessons.md`.

## The copilot and omp captures (2026-09-10)

`copilot-folder-trust.raw` is recorded **from spawn**, like the codex composer-then-trust capture,
because the transition into the dialog is worth more than the bytes it costs.

`omp-setup-step1.raw` is a **settled repaint**, not a from-spawn recording. omp animates its first
paint — about 2.85 MB in three seconds, a hundred times this whole directory — so the frame was
taken by letting the PTY fall quiet, resizing one column, sending `SIGWINCH`, and recording only
the redraw. The grid is the one a predicate classifies against; the animation that produced it is
not. `SIGWINCH` is a signal, not terminal input, and advances no dialog.

It is here as a **negative** sample. omp's first-run setup wizard is not a gate crew escalates on:
the maintainer ruled that crew never launches omp under a fresh home, so the wizard means the
machine was never configured. The predicate recognises omp's *normal prompt* and returns
`PromptReady`; anything else, this capture included, stays `Undecided` and fails the start closed
with a typed error rather than pressing Enter into a provider sign-in. Steps 2–5 of the wizard were
observed and are described in the record, but are deliberately not fixtures — nothing classifies
against them.

Unlike claude and codex, **neither vendor positions words individually**: their prose survives
escape-stripping with spaces intact. The whitespace-stripped comparison still matches them, but the
reason it exists does not apply here.

Note the direction the stripping trap runs in these two. Against claude and codex it *loses* spaces
and hides a phrase that is on screen. Against omp's animated paint it *joined* fragments into a
legend (`press enter to skip`) that was never on screen at all and would have inverted the security
reading of step 1. Check a candidate phrase as contiguous bytes in the capture before trusting it.

Full method, per-step focused defaults and the stated limits are in
`release/live-conformance/2026-09-10-copilot-omp-first-run.md`.

Both new vendors' captures were taken from a neutral `/tmp` scratch directory, not from a path
under a home directory. Copilot's dialog prints the directory it is asking about, so the first
recording carried an absolute home path in its bytes; every other fixture here contains none. A
byte-exact recording of a dialog that displays its own working directory will bake that path into
this repository, and the marker guard cannot catch it because `fixtures/` is exempt by design.

Both vendors also self-updated during the session that produced these files (copilot 1.0.81 →
1.0.83, omp 18.1.15 → 18.1.16). The copilot and composer captures each carry their version string
in-band, so they can be pinned from the bytes; `omp-setup-step1.raw` does not, and is 18.1.15 on
the record's word alone.
