// `crew_artifact`: lists and fetches artifacts published by runs this
// session owns (patches, commit lists, conflict reports, workspace
// manifests). Both ops are tier `read` -- neither mutates anything; fetching
// an artifact only streams bytes the runtime already stored.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";
import type { ArtifactKind } from "@nikolasd/batman-protocol";

import type { OrchestrationToolContext } from "./shared";
import { callOrchestration } from "./shared";

// Tied to the generated `ArtifactKind` wire union (R17/R60): `satisfies`
// fails the compile when a Rust variant is removed, and the `_Exhaustive`
// check fails it when one is added.
const ARTIFACT_KINDS = ["patch", "commitList", "conflictReport", "workspaceManifest"] as const satisfies readonly ArtifactKind[];
type _ArtifactKindExhaustive = Exclude<ArtifactKind, (typeof ARTIFACT_KINDS)[number]> extends never ? true : never;
// The assignment is what makes the check bite: when a variant is added in
// Rust, the alias resolves to `never` and `true` no longer assigns.
const _artifactKindExhaustive: _ArtifactKindExhaustive = true;
void _artifactKindExhaustive;

export const CREW_ARTIFACT_TOOL_NAME = "crew_artifact";

export function registerArtifactTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["list", "fetch"]).describe("Which artifact operation to perform."),
    kind: pi.zod.enum(ARTIFACT_KINDS).optional().describe("Optional filter for list: only return artifacts of this kind. Omit to list every kind."),
    taskId: pi.zod.string().optional().describe("Optional for list: narrow to artifacts from a specific task. Defaults to all tasks owned by the current session."),
    artifactId: pi.zod.string().optional().describe("Required for fetch: the artifact id to read."),
    offset: pi.zod.number().int().optional().describe("Optional for fetch: byte offset to start from. Defaults to 0."),
    length: pi.zod.number().int().optional().describe("Optional for fetch: how many bytes to read. The runtime caps this; the response's nextOffset says where to resume."),
  });

  pi.registerTool({
    name: CREW_ARTIFACT_TOOL_NAME,
    label: "Crew Artifact",
    description:
      "Use to read the evidence a worker produced: patches, commit lists, conflict reports, and workspace manifests. Use op: 'list' to see what a run published (optionally filtered by kind), then op: 'fetch' with an artifactId to read its bytes. Fetches are chunked -- the response carries nextOffset, so pass it back as offset to continue reading a large artifact. Artifacts are scoped to runs this session owns; taskId only narrows further within them.",
    parameters: params,
    approval: "read",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      switch (input.op) {
        case "list":
          return callOrchestration(client, "artifact/list", { kind: input.kind, taskId: input.taskId });
        case "fetch":
          return callOrchestration(client, "artifact/fetch", {
            artifactId: input.artifactId,
            offset: input.offset,
            length: input.length,
          });
      }
    },
  });
}
