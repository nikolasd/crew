# Vendor first-run gates — direct measurement, 2026-09-09

**What this establishes: on a first launch in a directory the vendor has never seen, crew's
prompt-delivery sequence answers the vendor's own security dialog. On claude it selects "No,
exit" and the worker dies. On codex it selects "Yes, continue" and directory trust is granted.**

Vendors under test: claude 2.1.265, codex-cli 0.153.4. Host: macOS arm64.

The 2026-09-08 attempt-3 record named this as open and asked for it to be settled deliberately:
*"what Claude's dialog does with that Enter … That is the difference between 'a wasted run' and
'crew can auto-accept a filesystem-trust prompt'."* This record answers it by measurement. The
answer is neither of those two candidates for claude, and is the second one for codex.

This record reports what was observed. It makes no claim about vendor versions other than the two
named, and none about copilot or omp-rpc, which were not exercised.

---

## Method

A harness reproduced the adapter's own sequence byte for byte: spawn on a PTY, wait for first
output, hold until the spawn-anchored injection floor, deliver the prompt as one bracketed paste
in chunked writes, wait for the output-idle window, then write the single submit byte. Each probe
ran in its own brand-new directory under a scratch path. The vendor argv was the one the adapter
builds for the default permission mode.

The harness carried a safety interlock: the submit byte was written only when a first-run dialog
was confirmed present in the captured screen, so it could never submit a prompt into a live
composer. No prompt was submitted in any probe and no turn was started.

The sign-in probes used a throwaway vendor config directory holding no credentials
(`CLAUDE_CONFIG_DIR`, `CODEX_HOME`); real account state was neither read nor written.

## claude 2.1.265 — the submit byte selects "No, exit"

1. **The default permission-mode flag does not skip the trust dialog.** It appears regardless.
2. **The dialog paints once and then emits nothing.** ~1.3 KB at ~0.2 s, then silence. This is why
   both of the adapter's readiness tests pass on it: readiness accepts any bytes, and the
   output-idle wait reaches its threshold immediately, because a static modal is silent.
3. **The bracketed paste produces no output at all.** The dialog consumes and discards it; the
   selection does not move.
4. **The submit byte is accepted and selects the focused option, which is "No, exit".** The worker
   process exits with code 1. Trust is not granted, and no entry is written to the vendor's
   configuration for the directory.

So the failure is not a wasted run and not an auto-accept: on a first run in any new repository,
crew terminates its own worker, and the only durable trace is a process exit.

### The framing is a security control

The same probe, the same text, the same single submit byte, with the paste written **unframed**
instead of as a bracketed paste:

* the bytes are interpreted as keystrokes;
* a cursor-movement escape occurring inside the prompt text moved the selection off "No, exit"
  and onto "Yes, I trust this folder";
* the submit byte then **granted filesystem trust**, recorded in the vendor's configuration;
* the vendor proceeded into its composer under the permissive permission mode.

Bracketed-paste framing was introduced to stop multi-line prompts being submitted line by line.
This measurement shows it is also the control that prevents crew from answering a vendor security
dialog, and it was neither documented nor tested as one.

## codex-cli 0.153.4 — the submit byte grants directory trust

The ordering is inverted relative to claude:

1. codex paints its UI first, under the permissive permission mode;
2. it **accepts the bracketed paste into its composer** — the prompt text is in the input box;
3. and only then raises its directory-trust gate on top;
4. the gate's default selection is the accepting option.

Verified with an empty paste, so that nothing could be submitted: the submit byte **accepts**, and
the vendor's configuration gains a trusted-project entry for the directory.

On codex, therefore, crew currently grants directory trust to whatever directory it is pointed at,
while running under the permissive permission mode. A second submit byte — a queued send, or a
retry — would submit the prompt already sitting in the composer.

## The property common to both, which is why the current readiness test cannot work

Both vendors' first-run gates are **output-silent after their initial paint**. Any readiness
predicate built on quantity of output, or on output going quiet, classifies a modal as ready. The
predicate has to be about screen content.

## Full gate inventory

Reaching the composer on a machine that has never run the vendor requires answering more than one
gate. Both vendors were walked through their sequence with a throwaway, credential-free config.

| Vendor | Gates, in order |
|---|---|
| claude 2.1.265 | theme selection → login-method selection → browser OAuth wait → workspace trust → composer |
| codex-cli 0.153.4 | sign-in method selection → directory trust → composer |

Two consequences for any readiness predicate:

* The claude OAuth step presents a **text input** ("paste code here if prompted"). A predicate
  that treats "an input line is present" as readiness would classify it as ready and paste the
  task prompt into it. Readiness must require a positive, vendor-specific composer marker, not the
  presence of an input.
* A gate that is not recognised must fail closed. A theme picker is harmless to answer and a trust
  dialog is not, and nothing in the byte stream distinguishes their danger.

## Volume, which constrains how a screen is held in memory

codex emits roughly 780 KB of animated banner within ten seconds of launch. Its sign-in prompt
occupies the first ~1.6 KB of that stream. A byte cap on retained output implemented as a plain
tail window would discard exactly the gate it exists to find.

## Evidence

The captures are committed as `fixtures/adapters/tui-screens/`, with their provenance and the two
deliberate exclusions recorded in that directory's `README.md`. They are the golden inputs for
`crates/runtime/src/adapter/tui/screen.rs`.

## What was not established

Neither vendor's behaviour was measured on a machine where the vendor was already authenticated
*and* the directory was already trusted, because that combination has no gate to observe. Nothing
here describes copilot or omp-rpc. The claude OAuth screen was reached but is not committed as a
fixture; see the fixtures README.
