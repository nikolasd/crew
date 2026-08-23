// `crew_workspace`: acquires, inspects, and releases isolated (or shared)
// working directories for a run. `acquire` is tier `exec` -- it materializes
// a git worktree or copy on disk (or grants shared access to the repository
// root) and activates the lease. `release` is tier `exec` -- it tears down
// the lease's exclusivity so another run may acquire the same isolation.
// `get` and `inspect` are read-only lookups against an already-acquired
// lease.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import type { ApplyStrategy, IsolationKind, LeaseMode } from "@nikolasd/batman-protocol";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

// Hand-written token lists tied to the generated wire unions (R17):
// `satisfies` fails the compile when a Rust variant is removed, and the
// `_Exhaustive` checks fail it when one is added, so drift in either
// direction breaks `bun run typecheck` instead of silently shipping.
const LEASE_MODES = ["readOnly", "write"] as const satisfies readonly LeaseMode[];
const ISOLATION_KINDS = ["shared", "gitWorktree", "copy"] as const satisfies readonly IsolationKind[];
const APPLY_STRATEGIES = ["applyPatch", "cherryPick"] as const satisfies readonly ApplyStrategy[];
type _LeaseModeExhaustive = Exclude<LeaseMode, (typeof LEASE_MODES)[number]> extends never ? true : never;
type _IsolationKindExhaustive = Exclude<IsolationKind, (typeof ISOLATION_KINDS)[number]> extends never ? true : never;
type _ApplyStrategyExhaustive = Exclude<ApplyStrategy, (typeof APPLY_STRATEGIES)[number]> extends never ? true : never;
// The assignments are what make the checks bite: when a variant is added in
// Rust, the alias resolves to `never` and `true` no longer assigns.
const _leaseModeExhaustive: _LeaseModeExhaustive = true;
const _isolationKindExhaustive: _IsolationKindExhaustive = true;
const _applyStrategyExhaustive: _ApplyStrategyExhaustive = true;
void _leaseModeExhaustive;
void _isolationKindExhaustive;
void _applyStrategyExhaustive;

export const CREW_WORKSPACE_TOOL_NAME = "crew_workspace";

export function registerWorkspaceTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["acquire", "get", "release", "inspect", "apply"]).describe("Which workspace operation to perform."),
    runId: pi.zod.string().optional().describe("Required for acquire: the run this workspace lease belongs to."),
    mode: pi.zod.enum(LEASE_MODES).optional().describe("Required for acquire: readOnly allows sharing with other readers, write requires isolation."),
    requestedIsolation: pi.zod.enum(ISOLATION_KINDS).optional().describe("Optional for acquire: the isolation strategy to materialize. Defaults to shared. Use gitWorktree or copy when a peer agent will work on the same task concurrently."),
    leaseId: pi.zod.string().optional().describe("Required for get, release, inspect, and apply: the lease id."),
    strategy: pi.zod.enum(APPLY_STRATEGIES).optional().describe("Required for apply: applyPatch applies a patch artifact, cherryPick replays commits."),
    artifactId: pi.zod.string().optional().describe("Required for apply: the artifact to apply (from crew_artifact { op: 'list' })."),
    expectedTargetRevision: pi.zod.string().optional().describe("Required for apply: the revision the workspace must currently be at. A mismatch is refused as STALE_REVISION rather than applied to the wrong base."),
    approvalCorrelationId: pi.zod.string().optional().describe("Optional for apply: correlates this application with an approval decision."),
  });

  pi.registerTool({
    name: CREW_WORKSPACE_TOOL_NAME,
    label: "Crew Workspace",
    description:
      "Use to acquire, inspect, apply changes to, or release an isolated (or shared) working directory for a run. Use op: 'acquire' before submitting a run that needs its own git worktree or copy (requires runId and mode; pass requestedIsolation: 'gitWorktree' for concurrent agents working on the same task in isolation), op: 'get' to fetch a lease's current path and state, op: 'inspect' to read the workspace's dirty/untracked file counts and diverged commits, op: 'apply' to land a patch or cherry-pick an artifact into the workspace (requires strategy, artifactId, and expectedTargetRevision), or op: 'release' to tear down the lease once the run is done with it. A shared-mode write lease is exclusive across the whole project; isolated (gitWorktree or copy) leases never conflict with each other or with shared leases.",
    parameters: params,
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && (args.op === "acquire" || args.op === "release" || args.op === "apply") ? "exec" : "read"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "acquire":
          return callOrchestration(client, "workspace/acquire", {
            runId: input.runId,
            mode: input.mode,
            requestedIsolation: input.requestedIsolation,
          });
        case "get":
          return callOrchestration(client, "workspace/get", { leaseId: input.leaseId });
        case "release":
          return callOrchestration(client, "workspace/release", { leaseId: input.leaseId });
        case "inspect":
          return callOrchestration(client, "workspace/inspect", { leaseId: input.leaseId });
        case "apply":
          // `ApplyRequest` is `deny_unknown_fields`, so exactly these five
          // keys may be sent -- an extra key fails the whole call.
          return callOrchestration(client, "workspace/apply", {
            leaseId: input.leaseId,
            strategy: input.strategy,
            artifactId: input.artifactId,
            expectedTargetRevision: input.expectedTargetRevision,
            approvalCorrelationId: input.approvalCorrelationId,
          });
      }
    },
  });
}
