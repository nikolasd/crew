# Using the Crew OMP Extension

**This is the Crew user manual.** Audience: anyone using Crew through OMP — you drive it with
plain language, and the model calls the tools on your behalf. You never need to touch the source,
build anything, or write raw tool-call JSON yourself — that detail lives in Appendix A below, for
advanced users and for the model's own reference.

For installing the extension, see the [README](../README.md#installation). For running/
troubleshooting the daemon directly, see [`operations.md`](operations.md) and
[`cli-reference.md`](cli-reference.md). For whether your platform/adapter is supported, see
[`compatibility.md`](compatibility.md). For the wire protocol and internal architecture (a
contributor concern, not a usage one), see [`architecture.md`](architecture.md).

## 1. Install

```
/marketplace add nikolasd/batman
/marketplace install crew@crew
```

**Exit and start a new `omp` session** — `/reload-plugins` only refreshes skills and slash
commands, not extension modules or tools, so `/crew-runtime-install` (and every `crew_*`
tool) only exists once a fresh session has loaded the installed module. Then:

```
/crew-runtime-install
/crew-status
```

**This repository is private** — the marketplace step git-clones it, so it needs your own GitHub
read access to `nikolasd/batman` (an SSH key registered with GitHub, or a `gh auth login` session
backed by a git credential helper). `/crew-runtime-install` additionally needs a `GITHUB_TOKEN` or
`GH_TOKEN` environment variable, or that same `gh auth login` session, to download and verify the
release asset.

**Skills and rules load automatically — no extra manifest entry needed.** `packages/extension/`
ships `skills/` (three skills, §3) and `rules/` (`crew-delegation-guard.md`) as plain sibling
directories next to `dist/`, and `packages/extension/package.json` declares only `omp.extensions`
— nothing skill- or rule-specific. That's sufficient: OMP's `omp-plugins` provider (priority 90)
discovers `skills/` and `rules/*.{md,mdc}` by directory convention for any extension package it
loads, whether that's via `extensions:`/`--extension`/`-e` or an installed plugin under
`~/.omp/plugins/node_modules` (where a marketplace install like this one is symlinked — see
`docs/marketplace.md`). Source: `oh-my-pi` docs
[`docs/skills.md`](https://github.com/can1357/oh-my-pi/blob/main/docs/skills.md) ("`omp-plugins`
(priority 90) — `skills/` bundled next to extension packages loaded through `extensions:`,
`--extension`/`-e`, or installed plugins under `~/.omp/plugins/node_modules`") and
[`docs/rulebook-matching-pipeline.md`](https://github.com/can1357/oh-my-pi/blob/main/docs/rulebook-matching-pipeline.md)
("`omp-plugins` (priority `90`) — `rules/*.{md,mdc}` inside configured extension package roots").
No `package.json` change is required.

## 2. Confirm it works

Run `/crew-status`. A healthy runtime answers with exactly this shape (`formatStatus` in
`status.ts`):

```
Crew runtime: running
Protocol: 1.0 (healthy: true)
Project: 0f4c1d9a8b7e6f50
Active runs: 0
Schema version: 7
Uptime: 3s
Binary source: package
```

`Binary source: package` means the verified, downloaded-and-cached binary is running. `override`
means `OMP_CREW_BINARY` was set and is running instead — the local-development path, described in
[`getting-started.md`](getting-started.md#how-the-extension-finds-and-starts-crewd).

If this fails instead, skip to [When something breaks](#6-when-something-breaks).

## 3. Just ask

Once installed, you drive Crew with plain language — the model calls the tools. The three
installed skills (`crew-orchestration`, `crew-approvals`, `crew-troubleshooting`, under
`packages/extension/skills/`) already carry these workflows, so the model doesn't need a tool-call
hint from you. Some examples of what to say, and what happens:

| You say | What Crew does |
|---|---|
| "run the auth refactor on Claude" | Looks up (or creates) a worker for that adapter, upserts a task, submits a run |
| "...in its own worktree" / "don't touch my files" | Same, plus `workspaceMode: "isolated"` — the run gets its own git worktree |
| "...on a copy instead" | Same, plus `workspaceMode: "copy"` — a per-run copy of the repository |
| "run these three on separate workers" | Creates (or reuses) three workers, then submits three runs — each with its own `workspaceMode: "isolated"` so they don't collide on the same files |
| "how's that run doing?" | Polls `run/get`, or points you at `/crew` to watch it live |
| "what did that run say?" | Reads the finished run's final output with crew_run op "result" |
| "that failed, try again" | Retries with the original prompt restated (Crew doesn't remember it for you) |
| "stop it" | Cancels the run |

## 4. Watching runs

`/crew` opens (or refreshes) a live widget above the editor showing every active run: state icon,
adapter/model, workspace mode, pending approvals, and latest activity — up to 7 rows. It subscribes
to the daemon's live event stream, so it updates itself with zero further input as runs progress,
and it replays from the daemon's journal across OMP restarts, so nothing is lost or duplicated. When
there's nothing running yet, it prints:

```
No Crew runs yet.
```

`/crew status <runId>` prints the full detail block for one run: task, worker, state, harness/
model, flags, pending approvals, workspace mode, latest activity, first-seen and last-event
timestamps.

On session start the monitor connects on its own: if the daemon is unreachable it degrades to
inactive rather than blocking session startup — running `/crew` again retries the connection.
The widget itself appears only when the journal has runs, so a session with nothing to show stays
widget-free until the first run event arrives.

## 5. When Crew needs you

Some actions require an explicit human decision, and Crew never fabricates one on your behalf:

- **Approvals.** A worker's escalated action may require `humanRequired: true`. The runtime
  enforces this server-side and rejects a model-supplied decision for it. With an interactive UI
  present, you'll see a dialog (secrets redacted); without one, the approval simply stays pending
  until you decide — this is a fail-closed rule, not a bug.
- **Policy violations.** If policy quarantines a run (for example, a worker tries to spawn a nested
  child when policy forbids it), the run makes no further progress until all violations are resolved.
  These surface through the event stream and the `/crew` monitor, not a query you poll.
- **Nested-child requests.** A worker that wants to spawn a child records only an intent — nothing
  happens until it's accepted or denied.

## 6. When something breaks

Work through this ladder:

1. **`/crew-status`** — connects to (or spawns) the daemon and reports whether it's healthy.
2. **`/crew-runtime-install`** — downloads and verifies the `crewd` binary, if it's missing.
3. **`/crew-doctor`** — works even with no live daemon; runs the full check catalog (database,
   state directory permissions, platform support, schema compatibility, adapter availability, disk
   space, unresolved rollout gates, and more — see
   [`cli-reference.md`](cli-reference.md#crewd-doctor) for the complete list).

Every Crew tool failure has the same shape: text `"<method> failed: <message>"`,
`details: { code, message, data }`, `isError: true`. The `details.code` field maps to a fix:

| `details.code` | Fix |
|---|---|
| `runtime-not-installed` | Run `/crew-runtime-install` to download the binary. |
| `checksum-mismatch` | Re-run `/crew-runtime-install`. The cached binary doesn't match its manifest. |
| `version-mismatch` | Re-run `/crew-runtime-install`. The cached binary is for a different extension version. |
| `manifest-invalid` | Re-run `/crew-runtime-install`. The cached manifest is corrupt or for another platform. |
| `unsupported-platform` | Crew only supports macOS and glibc Linux, arm64/x64. |
| `connection-failed` | Run `/crew-doctor` for a detailed check without needing a live daemon. |
| `http-error` (from `/crew-runtime-install`) | **This repository is private.** Set `GITHUB_TOKEN`/`GH_TOKEN`, or run `gh auth login`, then retry the install. |

## 7. Run three workers end to end

The §3 row above — "run these three on separate workers" — is the whole story in one line. Here's
what's underneath it, from three workers to the work merged back into `main`.

### Kick it off

You say something like:

> Run the auth refactor, the billing fix, and the infra hardening in parallel — three workers, each
> in its own worktree.

Crew, per piece of work:

1. `worker/create` (or reuses an existing worker for that adapter/model).
2. `task/upsert` — one durable task per piece of work.
3. `run/submit` with `workspaceMode: "isolated"`. Each submission gets its own detached `git
   worktree`, checked out from the same base commit (see `workspacePath` in the `run/submit`
   example in Appendix A) — three workers, three real working directories, one shared `.git`
   object database. The isolation isn't faked: it's a worktree any ordinary `git` command works
   against.

Because all three worktrees start from the same commit and, in this example, touch different
files, none of them can collide with each other or with your own working tree — that's what
`workspaceMode: "isolated"` buys you.

### Watching them

`/crew` now shows three rows, one per run, each with its own state, adapter/model, and
`workspaceMode: isolated` (§4). If one worker's action needs a decision, that row surfaces a
pending approval or a quarantine independently of the other two (§5); deciding it only affects that
run.

### Collecting the work

A `gitWorktree` isolation is a real git worktree, not a copy — commits made inside it are already
reachable from your main checkout, because they share the same object database. Once a run's done
(or partway through, if you just want to check in), there are two ways to get at the work:

- **Directly, with git.** `run/get` (or the `run/submit` response) carries `workspacePath` — the
  worktree's directory. There's nothing stopping the model from running `git -C <workspacePath>
  log`/`diff` there, or `git cherry-pick <sha>` those commits straight into `main`, exactly as it
  would for any other worktree of the same repository. This is the simplest path, and it needs no
  lease id.
- **Through Crew, audited.** `crew_workspace { op: "inspect" }` turns a worktree's dirty/
  untracked state and diverged commits into a durable `patch` artifact (Appendix A,
  `workspace/inspect`) that `workspace/apply` can then land with a revision check — useful when you
  want the merge itself recorded, and a conflict to come back as evidence instead of a live git
  conflict. It needs the run's own `leaseId`, though, and `run/submit` doesn't hand that back by
  design (see the workspace-lease note under `task/upsert` in Appendix A) — the model would need it
  from you or from the event journal (`crewd audit export`, see
  [`cli-reference.md`](cli-reference.md)).

`workspaceMode: "copy"` is the exception: a copy excludes `.git` entirely, so neither of the above
applies — there's no repository in it for either plain git or Crew's own (git-backed) `inspect`/
`apply` to work against. Reach for `copy` only when you want a worker fully outside git's reach;
reach for `isolated` whenever you'll want the work back.

### Merging back into `main`

For a `gitWorktree` run, the direct-git route above is usually all you need. If you want Crew to
mediate the merge instead:

1. `crew_workspace { op: "acquire", runId: <any of the three>, mode: "write" }` — a shared-mode
   write lease on the repository root itself. Unlike a run's own isolated lease, this call returns
   its `leaseId` directly (Appendix A, `workspace/acquire`).
2. For each worker's patch artifact: `crew_workspace { op: "apply", leaseId: <the one from step
   1>, strategy: "applyPatch", artifactId: <its patchArtifactId>, expectedTargetRevision: <main's
   current HEAD> }`. `applyPatch` lands the diff as uncommitted changes in `main`'s working tree,
   ready to review and commit; `cherryPick` (when the artifact is a commit list) creates real
   commits instead.
3. A conflict is never an RPC error — `workspace/apply` answers `success: false` with a
   `conflictArtifactId` (fetch it with `crew_artifact { op: "fetch" }`) and an `errorCode` of
   `"CONFLICT"` (the patch didn't apply cleanly) or `"STALE_REVISION"` (`main` moved since you read
   its head). Either way: look at the conflict, fix it, and retry `apply` with a fresh
   `expectedTargetRevision` — Crew never guesses a resolution for you.
4. Repeat for the next worker's patch, re-reading `main`'s head each time.

### Cleaning up

A run finishing — even successfully — doesn't release its workspace lease or remove its worktree;
Crew never deletes work you haven't collected yet. Once you're done with one,
`crew_workspace { op: "release", leaseId: ... }` tears it down — and because that removal is
forced (`git worktree remove --force`, needed since a worked-in worktree is never clean), it also
deletes any uncommitted changes still sitting in it. Collect what you need first. If the daemon
can't start and a lease is orphaned, `crewd lease release <lease-id>` (see
[`cli-reference.md`](cli-reference.md)) does the same release from a terminal and prints the leaked
path if teardown itself fails, so you can clean it up by hand.

All of that — three workers, three isolated runs, approvals and violations handled independently,
three patches merged back with conflicts surfaced instead of hidden — is still just "run these
three on separate workers" from §3. Crew does the bookkeeping; you only ever decide the things a
git merge would ask you to decide anyway.

## Appendix A — tool reference

For advanced users, and for the model's own use: the extension registers **11 orchestration
tools** (the deterministic `crew_*` tools below), plus three health/install helpers —
`crew_health`, `crew_doctor`, and `crew_runtime_install` — each also available as a slash
command (`/crew-status`, `/crew-doctor`, `/crew-runtime-install`). Every tool shares one
runtime connection per OMP session — the first call connects to (or spawns) the repository's
`crewd` daemon; every later call in the same session reuses that connection.

**Shared contract** (`packages/extension/src/tools/shared.ts`, `callOrchestration`): a successful
call's text content is `"<method>: <JSON.stringify(result)>"`, and `details` is the daemon's JSON
result **verbatim** — no wrapping, no renaming. A failed call's text is `"<method> failed: <message>"`,
with `details: { code, message, data }` and `isError: true`.

Approval tiers (`read` / `write` / `exec`) gate whether OMP prompts before running the operation.

| Tool | Ops | Tier | Purpose |
|---|---|---|---|
| `crew_profile` | register | `exec` | Register a reusable (adapter, model, startup options) profile |
| `crew_worker` | create, list, get | `exec`/`read` | Provision or look up a worker identity for a harness/model |
| `crew_task` | upsert, get | `write`/`read` | Create or read the durable, cross-session unit of work |
| `crew_run` | submit, list, get, retry, cancel, result | `exec`/`read` | Execute, monitor, retry, or cancel a task on a worker |
| `crew_workspace` | acquire, get, inspect, apply, release | `exec`/`read` | Manage the git worktree/copy a run executes in |
| `crew_artifact` | list, fetch | `read` | Read patches, commit lists, conflict reports a run published |
| `crew_child` | list, decide | `read`/`exec` | Approve or deny a worker's request to spawn a nested child |
| `crew_violation` | list, decide | `read`/`exec` | Find which violation still holds a run's quarantine, and resolve it |
| `crew_message` | send, list | `write` | Send/read coordination messages between workers in a run |
| `crew_approval` | list, decide | `exec` (always) | List and decide a worker's escalated approval request |
| `crew_reconcile` | (single-purpose) | `write` | Rebind task ownership after a dropped/reconnected session |

### `task/upsert`

```json
{ "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f", "sequence": 42 }
```

`ownerClientInstanceId` must equal the connected principal's own instance id -- the daemon binds
every upsert to the identity the connection already authenticated, never to whatever id the caller
presents. Three conditions refuse with `-32602`: the presented `ownerClientInstanceId` doesn't
match the connected instance; the presented `revision` is lower than the task's stored revision
(staleness wins the classification even when the owner also doesn't match, since an owner is
entitled to know its own upsert is stale); or an *existing* task is owned by a different instance (a
non-lower revision alone is not enough to re-upsert someone else's task). Creating a new task (no
existing row) binds ownership to the presented id unconditionally. The only way to move ownership
from one instance to another is `reconcile/omp`, which arbitrates the rebind by revision match.

The same connection-bound identity check -- never a caller-presented value -- also gates six
run-lifecycle methods once a task has an owner: `run/submit`, `run/retry`, `run/cancel`,
`message/send`, `workspace/acquire`, and `child/decide` (R77). Each is refused `-32602` if the
connected instance does not own the run's task; `run/retry` and `message/send` derive the task to
check from the *target run's own stored row* (the prior run's `taskId`, or the message's `runId`'s
owning task), never from a client-supplied field, so a caller cannot launder ownership by asserting
a task it does happen to own. Because ownership is bound to the connection's own authenticated
identity rather than to anything the caller asserts, a stale or rotated `ownerClientInstanceId` --
a session that reconnects under a new `instanceId` without a matching `reconcile/omp` call -- makes
every one of these six methods, and `task/upsert` itself, refuse `-32602` against a task that same
session originally created, until `reconcile/omp` rebinds ownership to the new instance id.

R81 extended the same connection-bound-identity gate to the workspace-lease surface itself:
`workspace/get`, `workspace/release`, `workspace/inspect`, and `workspace/apply` all resolve their
target purely from a caller-supplied `leaseId`, so each now re-derives the lease's owning task and
refuses `-32602` unless the connected instance owns it -- before any teardown, materialization, or
artifact resolution runs. Only `workspace/acquire` (R77) and these four (R81) are lease-scoped this
way; `run/get`'s `workspacePath` field and `events/replay`'s `LeaseAcquired` payload remain open to
any same-user client by design, not by oversight -- they're the discovery route a legitimate owner
uses to find its own lease id in the first place, and gating them would just move the disclosure
problem rather than remove it (R85).

### `task/get`

```json
{
  "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f",
  "projectId": "0f4c1d9a8b7e6f50",
  "ownerClientInstanceId": "client-a1b2c3d4",
  "revision": 3,
  "createdAt": "2026-08-10T14:02:11Z",
  "updatedAt": "2026-08-10T14:05:47Z"
}
```

### `profile/register`

```json
{ "profileId": "3c9e2f1a-7d4b-4e8a-9c1d-2b3a4c5d6e7f", "fingerprint": "sha256:a1b2c3d4e5f6" }
```

### `worker/create`

```json
{ "workerId": "7a8b9c0d-1e2f-4a3b-8c4d-5e6f7a8b9c0d", "sequence": 43 }
```

### `worker/list`

```json
{
  "workers": [
    {
      "workerId": "7a8b9c0d-1e2f-4a3b-8c4d-5e6f7a8b9c0d",
      "parentWorkerId": null,
      "createdAt": "2026-08-10T14:00:00Z",
      "profileRef": { "id": "3c9e2f1a-7d4b-4e8a-9c1d-2b3a4c5d6e7f", "fingerprint": "sha256:a1b2c3d4e5f6", "adapter": "claude", "model": "claude-opus-4" }
    }
  ]
}
```

### `worker/get`

Same as one `worker/list` entry, plus `projectId` and `profileRef.permissionEnvelope`:

```json
{
  "workerId": "7a8b9c0d-1e2f-4a3b-8c4d-5e6f7a8b9c0d",
  "projectId": "0f4c1d9a8b7e6f50",
  "parentWorkerId": null,
  "createdAt": "2026-08-10T14:00:00Z",
  "profileRef": {
    "id": "3c9e2f1a-7d4b-4e8a-9c1d-2b3a4c5d6e7f",
    "fingerprint": "sha256:a1b2c3d4e5f6",
    "adapter": "claude",
    "model": "claude-opus-4",
    "permissionEnvelope": { "allow": ["read", "write"] }
  }
}
```

### `run/submit`

```json
{ "runId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e", "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f", "sequence": 44 }
```

Plus `workspacePath` and `workspaceMode` (`"isolated"` or `"copy"`, matching the mode submitted) when a workspace was materialized, plus
`display` when a monitor pane was selected:

```json
{
  "runId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e",
  "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f",
  "sequence": 44,
  "workspacePath": "/Users/you/.omp/batman/repos/<repository-id>/worktrees/b2c3d4e5",
  "workspaceMode": "isolated"
}
```

### `run/retry`

Same as `run/submit`, plus `priorRunId`:

```json
{ "runId": "c3d4e5f6-a7b8-4c9d-0e1f-2a3b4c5d6e7f", "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f", "sequence": 45, "priorRunId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e" }
```

### `run/get` (and each entry of `run/list`)

```json
{
  "runId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e",
  "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f",
  "workerId": "7a8b9c0d-1e2f-4a3b-8c4d-5e6f7a8b9c0d",
  "state": "working",
  "flags": {
    "degradedControl": false,
    "needsReconciliation": false,
    "protocolUnhealthy": false,
    "policyQuarantined": false,
    "workspaceDirty": true,
    "childrenActive": false
  },
  "vendorSessionId": "vendor-session-9f8e7d6c",
  "createdAt": "2026-08-10T14:02:20Z",
  "startedAt": "2026-08-10T14:02:21Z",
  "completedAt": null,
  "policyFingerprint": "sha256:f1e2d3c4b5a6"
}
```

### `run/list`

```json
{ "runs": [ /* … one object shaped like run/get above, per run … */ ] }
```

### `run/cancel`

```json
{ "sequence": 46 }
```

### `run/result`

Only answers for a **terminal** run — a run still in flight is refused with `-32602`
(`run <id> is not finished`), never given a partial answer. `resultText` is the run's final
journaled message (redacted; `null` when the run produced no visible final message). `usage`
is `null` when the adapter reports none (Copilot under ACP v1); Codex reports tokens but never
cost.

```json
{
  "runId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e",
  "state": "succeeded",
  "resultText": "all done: pomegranate",
  "usage": { "inputTokens": 1000, "outputTokens": 2000, "costUsd": 2.5 },
  "completedAt": "2026-08-10T14:09:03Z"
}
```

### `workspace/acquire`

```json
{
  "leaseId": "d4e5f6a7-b8c9-4d0e-1f2a-3b4c5d6e7f8a",
  "runId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e",
  "mode": "write",
  "isolationKind": "gitWorktree",
  "path": "/Users/you/.omp/batman/repos/<repository-id>/worktrees/b2c3d4e5",
  "state": "active",
  "baseRevision": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
  "acquisitionSequence": 47
}
```

### `workspace/get`

```json
{
  "leaseId": "d4e5f6a7-b8c9-4d0e-1f2a-3b4c5d6e7f8a",
  "runId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e",
  "mode": "write",
  "isolationKind": "gitWorktree",
  "path": "/Users/you/.omp/batman/repos/<repository-id>/worktrees/b2c3d4e5",
  "state": "active",
  "baseRevision": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0"
}
```

### `workspace/inspect`

```json
{
  "leaseId": "d4e5f6a7-b8c9-4d0e-1f2a-3b4c5d6e7f8a",
  "patchArtifactId": "e5f6a7b8-c9d0-4e1f-2a3b-4c5d6e7f8a9b",
  "commitCount": 3,
  "commitIds": ["a1b2c3d", "b2c3d4e", "c3d4e5f"],
  "dirtyFileCount": 2,
  "untrackedFileCount": 1,
  "baseRevision": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
  "currentRevision": "b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1"
}
```

### `workspace/apply`

```json
{ "leaseId": "d4e5f6a7-b8c9-4d0e-1f2a-3b4c5d6e7f8a", "success": true, "conflictArtifactId": null, "targetRevisionAfter": "c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2", "errorCode": null }
```

### `workspace/release`

```json
{ "released": true, "cleanupFailed": false }
```

### `artifact/list`

```json
{ "artifacts": [ /* … artifact entries, filterable by kind on the request … */ ] }
```

### `artifact/fetch`

```json
{ "artifact": { "artifactId": "e5f6a7b8-c9d0-4e1f-2a3b-4c5d6e7f8a9b", "kind": "patch", "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08", "byteLength": 4096, "mediaType": "text/x-patch", "storagePath": "sha256/9f/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08", "runId": "c3d4e5f6-a7b8-4c9d-0e1f-2a3b4c5d6e7f" }, "contentBase64": "ZGlmZiAtLWdpdCBhL2ZvbyBiL2Zvbwo=", "nextOffset": 4096, "complete": false }
```

### `message/send`

```json
{ "messageId": "f6a7b8c9-d0e1-4f2a-3b4c-5d6e7f8a9b0c", "sequence": 48 }
```

### `message/list`

```json
{
  "messages": [
    {
      "messageId": "f6a7b8c9-d0e1-4f2a-3b4c-5d6e7f8a9b0c",
      "runId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e",
      "senderWorkerId": "7a8b9c0d-1e2f-4a3b-8c4d-5e6f7a8b9c0d",
      "recipientWorkerId": null,
      "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f",
      "kind": "steer",
      "payload": "focus on the error path first",
      "deliveryState": "acknowledged",
      "createdAt": "2026-08-10T14:03:00Z",
      "sentAt": "2026-08-10T14:03:00Z",
      "acknowledgedAt": "2026-08-10T14:03:02Z",
      "replyTo": null
    }
  ]
}
```

### `approval/list`

```json
{
  "approvals": [
    {
      "approvalId": "a7b8c9d0-e1f2-4a3b-4c5d-6e7f8a9b0c1d",
      "runId": "b2c3d4e5-f6a7-4b8c-9d0e-1f2a3b4c5d6e",
      "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f",
      "action": "runShellCommand",
      "arguments": { "command": "rm -rf /tmp/scratch" },
      "humanRequired": true,
      "policyReason": "destructive command outside allowlist",
      "createdAt": "2026-08-10T14:04:00Z",
      "decidedAt": null,
      "decision": null
    }
  ]
}
```

A decided approval also carries its provenance — `decidedBy` (`"human"` or `"model"`) and the
`reason` supplied with the decision; both keys are present and `null` while the approval is
pending (like `decidedAt`/`decision` above).

### `approval/decide`

Request takes `decision: "approve" | "deny"`; the response's `outcome` reports what happened to
that decision, not the decision itself:

```json
{ "approvalId": "a7b8c9d0-e1f2-4a3b-4c5d-6e7f8a9b0c1d", "outcome": "decided" }
```

`outcome` is `"decided"`, `"decidedCallbackFailed"` (decided, but notifying the waiting worker
failed — the decision still stands), or `"alreadyDecided"` (a no-op repeat of an identical prior
decision).

An identical repeat of an already-decided approval returns `outcome: "alreadyDecided"` and
re-applies nothing — no second callback, no second `approvalDecided` event. A *different*
decision submitted for an already-decided approval is refused with `-32602`, and the first
decision stands — exactly one decision is ever journaled per approval, even if two clients
submit concurrently. A *new* decision is refused with `-32602` once the run has reached a
terminal state, because a settled run is never revived; an identical repeat of a decision already
on record still returns `"alreadyDecided"`, since the already-decided check is evaluated first.
Task ownership is checked before any of that, though: a caller that no longer owns the approval's
task is refused with `-32602` (`Forbidden`), even when replaying a decision already on record —
ownership outranks idempotent replay.

### `violation/decide`

Request takes `resolution: "release" | "cancel"`. `release` resolves *that* violation and lifts
quarantine only if it was the last unresolved violation on the run -- a different, still-open
violation on the same run keeps `flags.policyQuarantined` set even though this one is decided.
`cancel` cancels the run outright. The response's `outcome` is `"decided"` or `"alreadyDecided"`; a
newly decided `release` additionally carries `quarantineCleared: bool` -- `true` if this call
actually cleared the run's quarantine flag, `false` if a different, still-unresolved violation kept
it held. `quarantineCleared` is absent for `cancel` and for an `alreadyDecided` replay, since
neither computes a clearing decision:

```json
{ "violationId": "b8c9d0e1-f2a3-4b4c-5d6e-7f8a9b0c1d2e", "outcome": "decided", "quarantineCleared": true }
```

An identical repeat of an already-decided violation returns `outcome: "alreadyDecided"` and
re-applies nothing. A *different* resolution submitted for an already-decided violation is
refused with `-32602`, and the first decision stands -- exactly one decision is ever journaled
per violation, even if two clients submit concurrently. `release` is refused with `-32602` once
the run has already reached a terminal state, because a settled run is never revived.
Task ownership is checked before any of that, though: a caller that no longer owns the violation's
task is refused with `-32602` (`Forbidden`), even when replaying a resolution already on record --
ownership outranks idempotent replay.

### `violation/list`

```json
{ "violations": [ { "violationId": "…", "runId": "…", "taskId": "…", "workerId": "…", "action": "quarantine", "createdAt": "…", "resolvedAt": null, "resolution": null, "resolvedBy": null, "vendorChildId": null, "vendorParentRef": null } ] }
```

Newest first. An entry with `resolution: null` on a quarantined run is the one still holding
the quarantine -- decide exactly that `violationId` to lift it.

### `child/list`

```json
{ "requests": [ /* … pending child-spawn requests, one JSON object per request … */ ] }
```

### `child/decide`

```json
{ "sequence": 49 }
```

### `reconcile/omp`

```json
{ "taskId": "5f0b6b3e-6b1a-4b8e-9c2d-1a2b3c4d5e6f", "newOwnerClientInstanceId": "client-b2c3d4e5", "sequence": 50 }
```

### `crew_runtime_install`

Success (text, then `details`):

```
Crew runtime installed: crewd 0.3.0 (darwin-arm64)
Path: /Users/you/.omp/batman/bin/0.3.0/crewd
```

```json
{ "version": "0.3.0", "target": "darwin-arm64", "path": "/Users/you/.omp/batman/bin/0.3.0/crewd", "sizeBytes": 41211752 }
```

Failure (private-repo case):

```
Runtime install failed: failed to fetch release https://api.github.com/repos/nikolasd/batman/releases/tags/v0.3.0: HTTP 404
```

```json
{ "code": "http-error", "message": "failed to fetch release https://api.github.com/repos/nikolasd/batman/releases/tags/v0.3.0: HTTP 404" }
```

That `404` on a private repo almost always means no `GITHUB_TOKEN`/`GH_TOKEN` was set and no
`gh auth login` session exists — see the [code table above](#6-when-something-breaks).

## Appendix B — how the runtime binary is resolved

Moved to [`getting-started.md` § How the extension finds and starts `crewd`](getting-started.md#how-the-extension-finds-and-starts-crewd)
— you don't need it to use the tools above. Short version: an existing daemon is reused if its
socket answers; otherwise `OMP_CREW_BINARY` (developer override) or the checksum-verified
`<state root>/bin/<version>/crewd` cache is spawned, and `crew_health` reports which one as
"Binary source".
