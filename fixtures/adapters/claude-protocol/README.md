# `claude-protocol` discriminating-experiment fixtures: provenance

Three files, from a live-capture sequence that answered whether
`can_use_tool` approval requests travel over claude's `stream-json` stdout
the way `claude_protocol::approval_bridge` assumes. The first two are from
an early round that turned out to be missing the flag actually needed to
enable that control channel at all; the third is from the corrected call
that supplied it and got a real, answered `can_use_tool` round trip. All
three are kept for the real wire shapes they captured. None is a raw,
byte-for-byte scrub of the real capture the way `claude-tui/session.jsonl`
is -- all are **condensed and scrubbed**: every identifying field
(username, home path, session id, message id, request id, tool-use id,
timestamp, the actual echoed test string) was replaced with a stable
placeholder, and for `ask-rule-no-control-frame.jsonl` specifically, the
surrounding SessionStart hook lines and the real
`tools`/`mcp_servers`/`agents`/`skills`/`plugins` arrays (which in the raw
capture ran to several hundred real, environment-specific entries) were
trimmed to a representative few. The real captures underneath (nine live
`claude -p` calls against `claude_code_version` `2.1.268`, in a real trusted
repository, real billed API calls) are not committed anywhere and exist only
in this session's own local scratch.

## `permission-denied-frame.jsonl`

One line: the `type: "system", subtype: "permission_denied"` shape a real
`rm` call produced when denied by a hard `deny` rule. NOT evidence about
`can_use_tool`/the control channel -- the underlying live call was later
found to be confounded (the `rm` command matched the operator's own personal
deny rule, so no approval request was ever generated to observe; a hard deny
never asks anyone).

**No longer a curiosity -- this is load-bearing test input.**
`claude_protocol::reader::classify_line` originally fell through to
`Other` for this shape, and this fixture documented that as a confirmed
gap. It now parses it (`StreamLine::PermissionDeniedNotice`), because
`can-use-tool-round-trip.jsonl`'s own three `tool_use_id` correlations
(assistant `tool_use` -> this notice -> the closing `result`'s own
`permission_denials` entry) showed a hard deny announces itself on the
stream and correlates to its own denial entry by that id -- the second
leg of `drive_turn`'s ledger-reconciliation check, alongside what it
bridged into `ApprovalService`. `crates/runtime/src/adapter/claude_protocol/reader.rs`'s
own test suite reads this exact file as its fixture for that
classification, not a hand-typed lookalike.

## `ask-rule-no-control-frame.jsonl`

The full turn from a calibration call: `--permission-mode manual`
(reports back as `permissionMode: "default"`), a custom `ask` rule for
one specific `Bash` command via `--settings`, and a prompt invoking
exactly that command. `permissionMode` was reported back and matched
the mode this call actually passed (`manual` was passed, `"default"`
came back, consistent with what was already measured) -- this fixture
does NOT show the production `auto` pin; a reader reaching for it to
exercise the posture assertion against the pinned production value
will find `default` here and should not conclude the assertion is
broken.

**What this fixture does NOT establish, and how that was resolved.** The
tool ran immediately with `permission_denials: []`, and no
`control_request`/`control_response`/`can_use_tool`-shaped line appears
anywhere in the sequence. Two things came to light after this capture
that both bore on what that null result meant -- both are now resolved,
not open:

1. An empty `permission_denials` on an `ask`-rule call is *consistent
   with the rule never having matched at all* -- the documented
   hostless behaviour is deny-and-report, not silent allow, so a
   genuine ask-match with no host present should show up as a denial,
   not run clean; on its own, this capture cannot distinguish "the rule
   matched and was allowed" from "the rule never matched". **Eliminated**
   by a positive control, not left open: the corrected call that produced
   `can-use-tool-round-trip.jsonl` ran a hard `deny` rule alongside the
   `ask` rule in the same turn, and the hard-denied command came back
   denied and not run -- proof that rule matching itself was live in
   that call, the exact discriminator this capture lacked.
2. Neither this call nor any of the seven in this round passed
   `--permission-prompt-tool stdio` -- the flag believed (from SDK
   source, not CLI docs) to be the actual enabler of `can_use_tool`
   control frames at all. Absent that flag, a null result here says
   nothing about whether the control channel exists; it may only mean
   the mechanism was never turned on. **Confirmed as the actual cause**:
   adding only that flag, with everything else unchanged, is what
   produced `can-use-tool-round-trip.jsonl`'s own real `can_use_tool`
   request.

Kept as the recorded shape of a `manual`-mode, ask-configured,
allowed-outcome turn -- not as evidence for or against the approval
bridge's own premise; see `can-use-tool-round-trip.jsonl` below for that.

## `can-use-tool-round-trip.jsonl`

The corrected call: the same two rules (`ask` on one `Bash` command,
hard `deny` on another) plus `--permission-prompt-tool stdio`. This is
the only one of the three fixtures here that proves the reply side,
live, against the real CLI, rather than against this codebase's own
fake stream: the assistant tries the denied command first (denied and
not run, the positive control described above), then tries the
ask-configured command, which produces a real `control_request`
(`can_use_tool`); a reply built from
`claude_protocol::approval_bridge::build_permission_response`'s own
unmodified shape -- `request_id` only, never `tool_use_id` -- was sent
back, and the command then actually ran, with its real stdout appearing
in both the `tool_result` and the assistant's own final summary. The
closing `result` message's `permission_denials` names only the
hard-denied command, not the ask-configured one that ran -- direct
confirmation that a request `approval_bridge` answers is not also
reported as a denial. Settles, live, that `build_permission_response`
needs no code changes and that `request_id` alone is sufficient for the
CLI's own correlation.
