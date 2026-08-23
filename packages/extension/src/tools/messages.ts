// `crew_message`: sends and lists correlated run messages. `send` is
// tier `write` -- it records intent for the runtime to deliver, but does
// not itself execute code or spawn a process.
//
// `kind` is typed as a plain string (not a zod enum) to avoid a TypeScript
// type-instantiation-depth error from combining a 9-value enum with the
// other optional fields on this schema; the runtime validates the exact
// semantic-kind set server-side and returns `INVALID_PARAMS` for an unknown
// value.

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

import type { OrchestrationToolContext } from "./shared";
import type { MessageKind } from "@nikolasd/batman-protocol";
import { callOrchestration } from "./shared";

// The nine coordination kinds, tied to the generated MessageKind union so
// either direction of drift breaks `bun run typecheck` instead of burning
// a model round trip on a runtime refusal (R88).
const MESSAGE_KINDS = ["assign", "steer", "followUp", "question", "answer", "peerMessage", "approvalDecision", "cancel", "shutdown"] as const satisfies readonly MessageKind[];
type _MessageKindExhaustive = Exclude<MessageKind, (typeof MESSAGE_KINDS)[number]> extends never ? true : never;
// The assignment is what makes the check bite: when a variant is added in
// Rust, the alias resolves to `never` and `true` no longer assigns.
const _messageKindExhaustive: _MessageKindExhaustive = true;
void _messageKindExhaustive;

export const CREW_MESSAGE_TOOL_NAME = "crew_message";

export function registerMessageTool(pi: ExtensionAPI, ctx: OrchestrationToolContext): void {
  const params = pi.zod.object({
    op: pi.zod.enum(["send", "list"]).describe("Which message operation to perform."),
    runId: pi.zod.string().describe("The run this message belongs to (required for both send and list)."),
    senderWorkerId: pi.zod.string().optional().describe("Required for send: the sending worker id."),
    taskId: pi.zod.string().optional().describe("Required for send: the task this message relates to."),
    kind: pi.zod.enum(MESSAGE_KINDS).optional().describe("Required for send: the coordination message kind."),
    payload: pi.zod.string().optional().describe("Required for send: the message payload."),
    recipientWorkerId: pi.zod.string().optional().describe("Optional recipient worker id for send."),
    replyTo: pi.zod.string().optional().describe("Optional id of a prior message this replies to."),
  });

  pi.registerTool({
    name: CREW_MESSAGE_TOOL_NAME,
    label: "Crew Message",
    description:
      "Use to communicate between workers during an active multi-worker run, or to review message history. Use op: 'send' to send a message to another worker (requires runId, senderWorkerId, kind, payload), or op: 'list' to list messages for a run. Message kinds: assign, steer, followUp, question, answer, peerMessage, approvalDecision, cancel, shutdown. Use when workers need to coordinate or escalate decisions.",
    parameters: params,
    approval: "write",
    async execute(_toolCallId, input, _signal, _onUpdate, extCtx) {
      const client = await ctx.getClient(extCtx);
      if (input.op === "send") {
        return callOrchestration(client, "message/send", {
          runId: input.runId,
          senderWorkerId: input.senderWorkerId,
          taskId: input.taskId,
          kind: input.kind,
          payload: input.payload,
          recipientWorkerId: input.recipientWorkerId,
          replyTo: input.replyTo,
        });
      }
      return callOrchestration(client, "message/list", { runId: input.runId });
    },
  });
}
