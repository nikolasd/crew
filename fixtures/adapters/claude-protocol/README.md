# `claude-protocol` discriminating-experiment fixtures: provenance

Two files, both from a live-capture round aimed at whether `can_use_tool`
approval requests travel over claude's `stream-json` stdout the way
`claude_protocol::approval_bridge` assumes. This round later turned out to
be missing the flag believed to actually enable that control channel at
all (see `ask-rule-no-control-frame.jsonl`'s own note below) -- both
fixtures are kept for the real wire shapes they captured, not as a settled
answer to that question. Neither is a raw, byte-for-byte
scrub of the real capture the way `claude-tui/session.jsonl` is -- both are
**condensed and scrubbed**: every identifying field (username, home path,
session id, message id, request id, timestamp, the actual echoed test string)
was replaced with a stable placeholder, and for `ask-rule-no-control-frame.jsonl`
specifically, the surrounding SessionStart hook lines and the real
`tools`/`mcp_servers`/`agents`/`skills`/`plugins` arrays (which in the raw
capture ran to several hundred real, environment-specific entries) were
trimmed to a representative few. The real captures underneath (seven live
`claude -p` calls against `claude_code_version` `2.1.268`, in a real trusted
repository, real billed API calls) are not committed anywhere and exist only
in this session's own local scratch.

## `permission-denied-frame.jsonl`

One line: the `type: "system", subtype: "permission_denied"` shape a real
`rm` call produced when denied by a hard `deny` rule. Worth keeping because
**nothing in this codebase parses or reacts to this shape today** --
`claude_protocol::reader::classify_line` falls through to `Other` for it, and
that is a confirmed gap this fixture documents rather than papers over: a
real, wire-confirmed frame no code path reads. NOT evidence about
`can_use_tool`/the control channel -- the underlying live call was later
found to be confounded (the `rm` command matched the operator's own personal
deny rule, so no approval request was ever generated to observe; a hard deny
never asks anyone). Kept for the frame shape alone, not the experiment it
came from.

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

**What this fixture does NOT establish, corrected after it was
captured.** The tool ran immediately with `permission_denials: []`,
and no `control_request`/`control_response`/`can_use_tool`-shaped line
appears anywhere in the sequence. Two things came to light after this
capture that both weaken any conclusion drawn from that alone: (1) an
empty `permission_denials` on an `ask`-rule call is *consistent with
the rule never having matched at all* -- the documented hostless
behaviour is deny-and-report, not silent allow, so a genuine
ask-match with no host present should show up as a denial, not run
clean; this capture cannot distinguish "the rule matched and was
allowed" from "the rule never matched"; and (2) neither this call nor
any of the seven in this round passed `--permission-prompt-tool
stdio` -- the flag believed (from SDK source, not CLI docs) to be the
actual enabler of `can_use_tool` control frames at all. Absent that
flag, a null result here says nothing about whether the control
channel exists; it may only mean the mechanism was never turned on.
Kept as the recorded shape of a `manual`-mode, ask-configured,
allowed-outcome turn -- not as evidence for or against the approval
bridge's own premise. A corrected experiment (verified forcing
condition, `--permission-prompt-tool stdio` present) is pending
separately; this file predates it and should not be read as its
answer.
