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

The last one is not a duplicate. Codex paints its composer first, accepts a
pasted prompt into it, and only then raises its trust gate, so a single
capture holds both surfaces. Any predicate of the form "the composer is up and
no gate is up" has to be correct against it.

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
