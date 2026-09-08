# Paste delivery is bounded on progress, not on elapsed time

* Status: Accepted
* Date: 2026-09-08

## Context and Problem Statement

A prompt is written into a vendor's PTY as one bracketed paste, in paced chunks of about a kilobyte
(`crates/runtime/src/adapter/tui/input.rs`). Each chunk was bounded by a flat ten-second timeout: if
`PtyProcess::write_input` had not resolved by then, the prompt was declared undeliverable and the
start failed.

Measured under CPU load, that bound failed 3 of 14 runs, reporting that a chunk of a 4533-byte
prompt "was not acknowledged within 10s". About a kilobyte to a PTY master is microseconds of real
work, so ten seconds does not describe a slow vendor. It describes the writer thread not being
scheduled for ten seconds — starvation of crew's own thread, not of the vendor's read.

Three mechanical facts shape what can be done about it, all in
`crates/runtime/src/supervisor/pty.rs`:

* **The awaited window is wider than the write.** `write_input` sends a job into a bounded channel
  and awaits a oneshot acknowledgement, so the bound spans queueing, the writer thread being
  scheduled, the blocking write, the flush, and the acknowledgement's return trip. Attributing all
  of that to the vendor is what the previous error message did.
* **`write_all` is all-or-nothing to any outside observer.** It iterates over `write()` internally,
  so the count of bytes the far side has accepted existed inside the standard library and nowhere
  else. **No progress signal was available, and none could be added without replacing `write_all`.**
* **Chunking gives pacing, not progress.** Chunks already existed, but each was one atomic job, so
  "chunk smaller" — the obvious cheap idea — buys nothing: a smaller job still reports only
  completion.

The requirement that follows is not a larger number. It is that *a write which is advancing must not
be failed for being slow, and a write which is not advancing must still be failed promptly.* Those
two were indistinguishable, which is the whole defect.

## Decision Drivers

* The bound exists to catch a vendor that has stopped reading its stdin. Every second added to a
  flat timeout is a second such a vendor holds a start open, so raising the number trades directly
  against the thing the bound is for.
* Host load is not a vendor fault, and a failed start blamed on a vendor sends whoever reads it to
  the wrong place.
* Whatever ships must keep the guarantee that a prompt is never *silently* truncated — the property
  the flat bound was originally introduced to provide.
* The distinction between a starved writer thread and an unreading vendor may not be inferable at
  all. If it is not, the error must say so rather than pick one.

## Considered Options

* Raise the flat per-chunk timeout.
* Bound the time the far side may accept *nothing*, keeping an absolute per-chunk timeout as a
  backstop behind it.
* Add a heartbeat the writer thread bumps each loop iteration, so "running but bytes static" can be
  separated from "not running at all".

## Decision Outcome

**Bound the stall, not the total.** The writer thread runs an explicit `write()` loop and publishes
cumulative accepted bytes to an `AtomicU64`, exposed as `PtyProcess::bytes_accepted`. Each expiry of
a two-second stall window re-reads that counter and keeps waiting if it moved.
`PASTE_CHUNK_WRITE_TIMEOUT` survives only as an absolute per-chunk backstop, so a vendor accepting
one byte per window cannot hold a start open forever. The decision logic lives in
`bound_on_progress` in `crates/runtime/src/adapter/tui/adapter.rs`.

Two seconds because a far side that has accepted nothing at all for two full seconds is meaningfully
stuck, while one that is merely slow — or a host too loaded to run the writer thread — keeps the
write alive by accepting anything whatsoever.

### The arithmetic that halted the first shape of this decision

This is the part worth recording, because the design was approved once in a form that would have
made the problem worse.

The first shape kept the per-chunk backstop at the ten seconds it had when it was the *only* bound,
on the reasonable-sounding ground that leaving it alone kept the change to one mechanism. With the
backstop unchanged:

* before — a chunk fails if its duration exceeds ten seconds;
* after — a chunk fails if any two-second window passes with no byte accepted, **or** its duration
  exceeds ten seconds.

The second set strictly contains the first. Every write the flat bound failed, the new bound also
fails, plus every write that paused for two seconds. The change would have made the failure it was
written to fix *strictly more likely*, and applied to the original measurement it fails at two
seconds instead of ten — sooner, not never.

**A backstop must be far above the primary signal, or it is the primary signal.** So the ceiling
moved to ninety seconds, matching `ENTER_IDLE_CAP` in the same file rather than being chosen as a
round number: both are the same decision — the point at which the runtime stops waiting on a vendor
regardless of what it appears to be doing — and one figure for that in one file is worth more than
two tuned separately. `the_ceiling_is_a_backstop_not_the_primary_bound` asserts the ratio, because a
sentence explains and an assertion holds.

### The limitation this decision accepts, and does not paper over

**A progress bound cannot distinguish a starved writer thread from a vendor that has stopped
reading.** The counter is incremented by the very thread that is not being scheduled, so under both
conditions the observable is identical: it stops moving.

The error therefore names both causes. That is not hedging; it is the limit of what was observed,
and the limitation is documented at the stall check itself so that a later edit does not "improve"
the message into a claim the mechanism cannot support.

What the bound buys is narrower than telling them apart, and sufficient: the distinction stops
mattering for the failure that motivated it. A starved thread needs to accept one byte per window to
stay alive, where the flat bound required it to finish a whole chunk. Separating the two would take
the heartbeat option, and nothing currently behaves differently depending on the answer — so it is
deliberately not built, and this ADR is the record that it was considered rather than missed.

### The truncation guarantee, restated

"Never a silent fragment" never meant the write is atomic. A chunked write can fail after earlier
chunks have landed, so bytes may already have reached the vendor — that was true before this
decision and remains true. It means no fragment is ever *silent*, and that now holds at two distinct
points:

* A vendor that accepts every byte and then truncates inside its own composer is invisible at the
  PTY boundary, and is caught by comparing the recorded prompt. Untouched by this decision — that
  write succeeds and never reaches the new bound.
* A write that does not complete now reports how many bytes *of this paste* the vendor accepted
  before it stopped, so a partial delivery is stated rather than merely failed. That count is what
  the explicit write loop exists to make knowable.

### Positive Consequences

* A slow-but-working host stops being a failure, which is the user-visible point.
* A genuinely unreading vendor now fails in two seconds rather than ten — five times faster, which
  is the other half of the trade.
* The error reports evidence instead of a diagnosis: how many bytes were accepted, and how long was
  actually waited.

### Negative Consequences

* **A dribbling vendor holds a start open for up to ninety seconds** where the flat bound capped it
  at ten. That is the accepted cost of making the backstop a backstop, and it applies only to a
  vendor accepting something in every window while never finishing.
* The starvation-versus-deaf-vendor ambiguity is permanent under this design, so any future work
  that needs the distinction pays for the heartbeat then.
* Replacing `write_all` with an explicit loop moves a correctness-critical loop out of the standard
  library and into this repository — the interrupted-write retry and the zero-length-write case are
  now ours to get right.

### Two defects found in this change's own output

Recorded because both were found by printing the error the change produces rather than by reading
its code, and because both are instances of what the change is about.

* The stall message interpolated the window *constant* rather than the window actually waited, so it
  claimed two seconds after waiting five hundred milliseconds whenever a caller's ceiling clamped
  the window — a false claim in an error message, inside the change whose subject is false claims in
  error messages.
* `bytes_accepted` is cumulative for the process, so "N bytes of the prompt had been accepted" was
  true only for the first write of a process; a follow-up would have reported the original prompt's
  bytes as progress on the new one. Progress is now baselined per paste.

## Pros and Cons of the Options

### Bound the stall, with a distant backstop (chosen)

* Good, because it separates "not progressing" from "not finished", which is the distinction the
  flat bound conflated and the only one that matters here.
* Good, because it makes a genuinely stuck far side fail faster while making a merely slow one
  succeed.
* Bad, because it requires owning an explicit write loop, and because the backstop's new value lets
  a dribbling vendor hold a start much longer.

### Raise the flat timeout

* Good, because it is a one-character change with no new mechanism and no new failure mode.
* Bad, because it trades directly against the bound's purpose: every second added is a second an
  unreading vendor hangs a start, and no value both survives a loaded host and catches a dead vendor
  promptly. It also leaves the error message attributing host load to the vendor.

### Add a heartbeat to separate starvation from an unreading vendor

* Good, because it is the only option that makes the error's two causes distinguishable rather than
  jointly reported.
* Bad, because nothing currently acts differently on the answer, so it is machinery bought for a
  distinction no code consumes. Deferred rather than rejected: it is what a future need for that
  distinction should build.

## Links

* Follows the measurement in PR #103, which fixed a test-side accelerated bound and left this
  product question open deliberately.
* Shipped in PR #109.
* Constrained by the truncation guarantee that introduced chunked bracketed-paste delivery in the
  first place; that guarantee is restated above rather than changed.
* Proven by five tests over `bound_on_progress` driven directly, with the write future and the
  progress source as parameters — a mock vendor dribbling bytes at a chosen rate would measure the
  tty buffer as much as the bound, since a small prompt never blocks at all and the rate window
  separating old behaviour from new depends on a buffer size that is not portably knowable. The
  end-to-end proof that the bound is wired to a real PTY stays with the unreading-vendor test in
  `crates/runtime/tests/tui_adapter.rs`.
* Every claim above about which bound fires when is mutation-verified: reverting the ceiling,
  stalling regardless of progress, reporting the constant instead of the measured window, and
  dropping the per-paste baseline are each caught by exactly one test.
