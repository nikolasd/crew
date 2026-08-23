---
description: >-
  Stops an internal `task` subagent call from silently substituting for Crew
  when the call names an external AI vendor or model.
condition: "(?i)\\b(claude|codex|copilot|sonnet|opus|haiku|anthropic|openai|gpt-?\\d)\\b"
scope: "tool:task"
interruptMode: always
---

This `task` call names an external AI vendor or model (Claude, Codex, Copilot, Sonnet, Opus,
Haiku, GPT, or similar). Internal `task` subagents run in-process on the current session's
model — they cannot honor a named external vendor or model, and substituting one for the
other silently is a routing failure, not a valid substitute.

Stop this call. Use the `crew-orchestration` skill and Crew's own tools
(`crew_worker`, `crew_task`, `crew_run`) to satisfy this request instead of `task`.

If the `task` call does not actually delegate to a named external worker — the vendor/model
name only appears incidentally (e.g. it's quoted from the user's message but the work itself
stays in-process, or it's discussing Claude/Codex/etc. rather than asking to run on it) —
proceed with `task` as originally intended.
