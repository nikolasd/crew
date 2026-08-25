# `claude-tui` fixture: provenance

`session.jsonl` is a **recorded** capture, not hand-authored: `provenance: recorded-headless`
(never `approximated`, never `interactive`) -- real content, but from `-p` (headless) mode rather
than the actual interactive TUI, per the one caveat below.

## How it was recorded

The executing agent's harness had no real PTY to drive an interactive `claude` session, so the
capture was taken via the documented fallback: `claude -p "<prompt>"` (headless, one-shot) run in a
scratch git repo, against `claude --version` `2.1.241 (Claude Code)`. The exact prompt:

```
Say hi, run 'echo crew-fixture', then ask me exactly one clarifying question. [crew:fixture1]
```

The newest session file under `~/.claude/projects/<slug>/` was copied, then processed two ways
before being committed:

1. `crates/runtime/src/conformance/scrub::Scrubber` (the existing fixture scrubber): every
   timestamp, session id, and other `uuid` rewritten to stable placeholders, secret-shaped strings
   redacted, `cwd` rewritten to `/workspace/crew`.
2. A further, one-off manual pass over every `type: "attachment"` entry's own nested payload
   (`content`/`stdout`/`addedNames`/`addedBlocks`/... fields), replacing bulky text with a short
   placeholder string. These fields carried the *recording agent's own* personal hook/skill/MCP
   configuration verbatim (tool names, skill descriptions) -- real content, but specific to that
   one development environment rather than anything the Claude Code wire format itself guarantees,
   and not something this adapter's parser ever reads (every `attachment` entry degrades to
   `TuiEvent::Raw` regardless of its nested shape). The scrubber's own secret/timestamp/id rewriting
   already ran first; this pass only trims bulk and environment-identifying noise it does not
   otherwise touch.

## A recording-environment gotcha that became a real code fix

The scratch repo was created under `/tmp/crew-fixture-scratch`, but the recorded session file
lives under `~/.claude/projects/-private-tmp-crew-fixture-scratch/` -- `-private-tmp-...`, not
`-tmp-...`. macOS resolves `/tmp` to `/private/tmp` via a symlink, and the real CLI slugs the
*canonicalized* cwd, not the literal path string it was launched with. `ClaudeTuiVendor::transcript_root`
canonicalizes its own `cwd` before slugging for exactly this reason (with a unit test using a real
temp directory and a real symlink to it, not just `/tmp` itself).

## The one caveat: headless vs. interactive transcript format

This capture is from `-p` (headless) mode, not a real interactive TUI session -- `ClaudeTuiVendor`
launches `claude` *without* `-p`. The working assumption, unverified by this capture, is that the
on-disk session JSONL transcript format (`~/.claude/projects/<slug>/<session-id>.jsonl`) is written
by the same session-transcript writer regardless of invocation mode, so headless and interactive
sessions produce the same entry shapes. Real captured evidence for `user`/`assistant`/`tool_use`/
`tool_result` content and for the exact question-detection heuristic; **not** independently
confirmed for the interactive PTY path specifically. WP29's live TUI smoke test is what closes this
gap for real -- if it finds the interactive transcript format differs, this fixture (and
`ClaudeTuiVendor`'s format mapping) gets revisited then, not assumed correct forever.

## What the recording surfaced, beyond the v1 plan's format sketch

- Real entry `type` values seen: `queue-operation`, `attachment` (9, several distinct subtypes),
  `user`, `last-prompt`, `atis-latch`, `assistant`, in addition to the `user`/`assistant`/`summary`
  the plan named. None of the extras carry a `message` field at all -- exercised by this adapter's
  unknown-entry tolerance (`TuiEvent::Raw`), not a parser assumption that every entry has one.
- Every entry, of every type, carries top-level `sessionId` and `timestamp` -- consistent with the
  vendor spec's own assumption.
- Real `assistant` entries in this capture carry exactly **one** content block each (chained by
  `parentUuid`/`uuid` into a turn, not one entry with a multi-block `content` array) -- `thinking`,
  then `text`, then `tool_use` as three *separate* JSONL lines for one logical turn. The question-
  detection heuristic's "no subsequent tool_use in the same entry" qualifier is therefore normally
  vacuous against a real capture (a real entry's `content` array is never longer than one block);
  `ClaudeTuiVendor`'s parser still implements the multi-block case per the vendor spec as written
  (and it is exercised by a synthetic unit test), since nothing observed here rules out a vendor
  build emitting a multi-block entry, and degrading gracefully either way costs nothing.
- The final assistant entry's question ("what would you like me to actually work on next?") is
  plain `type: "text"` with no structural marker -- confirms the heuristic must be textual
  (ends in `?`), never protocol-flagged.
