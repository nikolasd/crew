// `crew_violation`: lists and decides recorded policy violations. A
// winning "release" resolves that specific violation but lifts quarantine
// only if it was the *last* unresolved violation on the run -- a
// different, still-open violation on the same run keeps it quarantined
// even though this one was decided. Use op: 'list' to find which
// violation still holds a quarantine (R80). A "cancel" ends the run
// outright. `decide` is tier `exec` -- a decision resumes or kills real
// work; `list` is a read.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

export const CREW_VIOLATION_TOOL_NAME = "crew_violation";

export function registerViolationTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["decide", "list"]).describe("Which violation operation to perform."),
    violationId: pi.zod.string().optional().describe("Required for decide: the recorded violation to decide."),
    resolution: pi.zod.enum(["release", "cancel"]).optional().describe("Required for decide: 'release' resumes the quarantined run (if this was its last unresolved violation), 'cancel' ends the run outright."),
    runId: pi.zod.string().optional().describe("Optional for list: narrow to one run's violations."),
  });

  pi.registerTool({
    name: CREW_VIOLATION_TOOL_NAME,
    label: "Crew Violation",
    description:
      "Use to find and resolve policy violations. Use op: 'list' (optionally with runId) to see every recorded violation and its decision state -- an entry with resolution: null on a quarantined run is the one holding the quarantine. Use op: 'decide' with the violationId and a resolution to resolve one. The deciding identity is taken from your session automatically. A \"release\" only lifts quarantine if this was the last unresolved violation on the run -- check the result's quarantineCleared field (true/false/absent) to tell whether it did; if false, use op: 'list' to find the still-open violation. Until every violation on a run is decided, the run makes no further progress.",
    parameters: params,
    // Deciding resumes or kills real work; listing is pure evidence.
    approval: (args) => (typeof args === "object" && args !== null && "op" in args && args.op === "list" ? "read" : "exec"),
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "list":
          return callOrchestration(client, "policy/violation/list", { runId: input.runId });
        case "decide":
          // The runtime takes the deciding identity from the connection
          // principal, so no owner field is sent: an OMP-supplied identity
          // would be unverified and could impersonate another instance.
          return callOrchestration(client, "policy/violation/decide", {
            violationId: input.violationId,
            resolution: input.resolution,
          });
      }
    },
  });
}
