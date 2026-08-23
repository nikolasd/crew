// Pure normalizers from the installed OMP `task:subagent:lifecycle`,
// `task:subagent:progress`, and `task:subagent:event` payloads to Crew
// `OmpNativeAgentFact` values. Never reads or mutates OMP's own state --
// each function is a total map from one payload to one fact (or, for the
// raw event channel, no fact at all).

import type { SubagentEventPayload, SubagentLifecyclePayload, SubagentProgressPayload } from "@oh-my-pi/pi-coding-agent/task";

import type { OmpNativeAgentFact, OmpNativeStatus } from "./types";

function mapLifecycleStatus(status: SubagentLifecyclePayload["status"]): OmpNativeStatus {
  switch (status) {
    case "started":
      return "working";
    case "completed":
      return "succeeded";
    case "failed":
    case "aborted":
      return "failed";
  }
}

function mapProgressStatus(status: SubagentProgressPayload["progress"]["status"]): OmpNativeStatus {
  switch (status) {
    case "pending":
    case "running":
      return "working";
    case "completed":
      return "succeeded";
    case "failed":
    case "aborted":
      return "failed";
  }
}

/** Normalizes a `task:subagent:lifecycle` payload (start/end transitions). */
export function normalizeLifecyclePayload(payload: SubagentLifecyclePayload, ompProcessEpoch: string, observedAtMs: number): OmpNativeAgentFact {
  return {
    ompAgentId: payload.id,
    status: mapLifecycleStatus(payload.status),
    description: payload.description,
    sessionFile: payload.sessionFile,
    artifactRefs: [],
    ompProcessEpoch,
    observedAtMs,
  };
}

/** Normalizes a `task:subagent:progress` payload (in-flight updates). */
export function normalizeProgressPayload(payload: SubagentProgressPayload, ompProcessEpoch: string, observedAtMs: number): OmpNativeAgentFact {
  return {
    ompAgentId: payload.progress.id,
    status: mapProgressStatus(payload.progress.status),
    description: payload.progress.description ?? payload.progress.assignment,
    sessionFile: payload.sessionFile,
    artifactRefs: [],
    ompProcessEpoch,
    observedAtMs,
  };
}

/**
 * Normalizes a `task:subagent:event` payload. Raw session events carry no
 * lifecycle status of their own; this channel only correlates an id to its
 * originating session, so there is no fact to derive here. Returns
 * `undefined` -- callers keep whatever fact they already recorded for this
 * agent id.
 */
export function normalizeEventPayload(_payload: SubagentEventPayload): OmpNativeAgentFact | undefined {
  return undefined;
}
