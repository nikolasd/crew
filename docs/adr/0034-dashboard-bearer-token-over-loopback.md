# A daemon-hosted web dashboard, authenticated by a per-run bearer token over loopback

* Status: Accepted
* Date: 2026-08-30

## Context and Problem Statement

The daemon's primary control surface is a Unix domain socket, which the IPC layer secures twice —
`check_owner_only` on the socket's directory, and `admit_same_uid` on every connecting peer, using
kernel-reported credentials with no TCP equivalent (ADR-0009). A web dashboard needs a browser to
reach it, and a browser cannot open a Unix socket. Once the daemon binds a TCP listener for that
purpose, the same-user guarantee the socket relies on disappears: loopback binding keeps other
*hosts* off the port, but says nothing about other *local users or processes* on the same machine.
How does a read-only web view get the daemon's data to a browser without becoming a second,
weaker-authenticated control surface?

## Decision Drivers

* The dashboard is read-only by design (a projection over the same domain reads the RPC layer
  uses); it must not become a way to bypass IPC's role-based authorization.
* No credential prompt, password, or account system exists anywhere else in the daemon, and adding
  one would be disproportionate to what a local, single-operator status view needs.
* A leaked URL (screen share, shell history, a copied link) must not grant standing access forever.
* The secret must not linger somewhere a browser habitually persists it — the address bar, browser
  history, or an outbound `Referer` header.

## Considered Options

* No web dashboard — status stays terminal-only (the existing `/crew` monitor and `crewd status`).
* A TUI-only view, extending the terminal monitor rather than adding an HTTP surface at all.
* An unauthenticated loopback listener, relying on loopback binding alone to keep it local.
* A per-daemon-run bearer token, presented via the URL and exchanged for a cookie on first load.

## Decision Outcome

Chosen option: a per-daemon-run bearer token. The daemon binds `127.0.0.1:<port>` only — never a
routable interface — and generates one token per run, printed once at startup and surfaced again in
`/crew health`. Every route requires it: access control runs *before* routing, so an unauthenticated
request reaches no handler at all, not even a 404 that would otherwise confirm which paths exist. A
valid token presented as `?token=` on `GET /` is exchanged for the `crew_dashboard` cookie
(`HttpOnly`, `SameSite=Strict`) via a 303 redirect, so the secret leaves the address bar, browser
history, and any `Referer` after the first load — deliberately, and printing the token-bearing URL
in `/crew health` is the same deliberate choice from the other side: the daemon hands the operator
the one thing that gets them in, on the assumption that whoever can already run `/crew health` for
this repository is already trusted. Token comparison is constant-time (an XOR-fold over
equal-length byte slices, not a short-circuiting equality check) — overkill for a loopback-only
listener, but cheap enough not to be worth arguing about.

### Positive Consequences

* No new account or credential system: the token is generated, not chosen, and needs no storage
  beyond the daemon's own memory for the run's lifetime.
* The redirect-and-cookie exchange moves the secret out of the browser's own persisted state — the
  address bar, history, and any `Referer` — after first load, but the token itself is not
  single-use: a `?token=` URL keeps authenticating for as long as the daemon that issued it keeps
  running, since the token is generated once per daemon run and nothing invalidates or rotates it
  on exchange. What the exchange buys is that the *browser* stops carrying the secret around, not
  that the secret expires. It does stop working the next time the daemon restarts (a fresh token
  invalidates the old cookie and the old URL alike).
* Access control ahead of routing means the unauthenticated surface reveals nothing — not even
  which paths exist — to a request that doesn't already have the token.

### Negative Consequences

* A browser tab left open across a daemon restart shows a stale, uninformative error until the
  operator reloads with a fresh link — an intentional trade (see the runbook's dashboard phase) but
  a real rough edge.
* Loopback binding alone does not distinguish between local users on a genuinely shared machine;
  the token is what does that job, and losing the token (e.g., a `/crew health` transcript pasted
  somewhere) is equivalent to losing dashboard access for that daemon run.

## Pros and Cons of the Options

### Per-run bearer token over loopback (chosen)

* Good, because it closes the same-user gap TCP reopens, without inventing an account system.
* Good, because the redirect-and-cookie exchange keeps the long-lived secret out of every place a
  browser habitually persists a URL.
* Bad, because it is still weaker than the IPC socket's kernel-enforced same-uid check — the token
  is a bearer secret, not an identity proof, and anyone who obtains it before it rotates is
  authenticated as the operator.

### No web dashboard

* Good, because it adds no new surface, no new secret, and no new failure mode to reason about.
* Bad, because a live, browsable view of active runs, cost, and transcripts has real value a
  terminal monitor can't fully replace (screen sharing, a second monitor, no terminal access).

### TUI-only view

* Good, because it reuses the existing monitor's trust model exactly — no new authentication
  surface at all.
* Bad, because it inherits the terminal's own constraints (one session, no easy sharing, no
  browser-based access from another device on the same machine).

### Unauthenticated loopback listener

* Good, because it is the simplest possible implementation.
* Bad, because loopback binding was never a same-user guarantee — any other local process or user
  account on the machine could reach it, which is exactly the gap this decision exists to close.

## Links

* Access-control rationale: `crates/runtime/src/dashboard/mod.rs` module documentation
* Sibling design note: `docs/operations.md` § Dashboard, `docs/user-guide.md` § Watching Runs on the
  Dashboard
* Related: [ADR-0006](0006-type-enforced-redaction-boundary.md) (the dashboard reads only through
  the same redacted domain ops the RPC layer uses; it is never a second path to the journal), and
  [ADR-0009](0009-role-based-authorization-from-the-connection-not-per-call.md) (the same-user
  guarantee this decision has to replace for a TCP listener)
