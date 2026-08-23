// Cross-restart persistence for OMP-native facts and task correlations.
//
// `reconcileAcrossRestart` needs facts observed by a *prior* OMP process,
// and `reconcileWithRuntime` needs the `{ taskId, revision }` of a task a
// prior process owned. Neither survives in memory, and the runtime has no
// `task/list` RPC to rediscover owned tasks from, so both have to be
// remembered locally.
//
// This reuses the persistence seam the embedded monitor already uses
// (`packages/extension/src/monitor/controller.ts`): `pi.appendEntry` writes
// a `custom` entry into OMP's own session log, and a later process reads it
// back by scanning `sessionManager.getEntries()`. No new state file, no new
// format to migrate.

import type { OmpNativeAgentFact, OmpNativeTaskCorrelation } from "./types";

/** The session-entry `customType` carrying one OMP-native fact. */
export const OMP_NATIVE_FACT_ENTRY_TYPE = "crew-omp-native-fact";

/** The session-entry `customType` carrying one task correlation. */
export const OMP_NATIVE_CORRELATION_ENTRY_TYPE = "crew-omp-native-correlation";

/**
 * The subset of `pi.appendEntry`'s session-entry log this module reads.
 * Mirrors `monitor/controller.ts`'s `SessionEntryLike` rather than
 * importing it, so the monitor's shape and this one can diverge without
 * one silently constraining the other.
 */
export interface SessionEntryLike {
  readonly type?: string;
  readonly customType?: string;
  readonly data?: unknown;
}

/** Narrows a persisted entry payload to a fact. */
function asFact(data: unknown): OmpNativeAgentFact | undefined {
  if (data === null || typeof data !== "object") return undefined;
  const { ompAgentId, status, ompProcessEpoch, observedAtMs, artifactRefs } = data as Record<string, unknown>;
  if (typeof ompAgentId !== "string" || typeof status !== "string") return undefined;
  if (typeof ompProcessEpoch !== "string" || typeof observedAtMs !== "number") return undefined;
  if (!Array.isArray(artifactRefs)) return undefined;
  // `status` is validated as one of the four buckets rather than cast: a
  // persisted entry is external input, and an unknown status would make
  // `reconcileAcrossRestart`'s terminal-status check silently wrong.
  if (status !== "working" && status !== "succeeded" && status !== "failed" && status !== "lost") {
    return undefined;
  }
  const { description, sessionFile } = data as Record<string, unknown>;
  return {
    ompAgentId,
    status,
    ompProcessEpoch,
    observedAtMs,
    artifactRefs: artifactRefs.filter((ref): ref is string => typeof ref === "string"),
    ...(typeof description === "string" ? { description } : {}),
    ...(typeof sessionFile === "string" ? { sessionFile } : {}),
  };
}

/** Narrows a persisted entry payload to a task correlation. */
function asCorrelation(data: unknown): OmpNativeTaskCorrelation | undefined {
  if (data === null || typeof data !== "object") return undefined;
  const { taskId, revision } = data as Record<string, unknown>;
  if (typeof taskId !== "string" || typeof revision !== "number") return undefined;
  return { taskId, revision };
}

/**
 * Every persisted fact, latest-per-agent, in entry order. A later entry for
 * the same `ompAgentId` supersedes an earlier one: the log is append-only,
 * so the newest entry is the one that survived.
 */
export function persistedFacts(entries: readonly SessionEntryLike[]): OmpNativeAgentFact[] {
  const latest = new Map<string, OmpNativeAgentFact>();
  for (const entry of entries) {
    if (entry?.type !== "custom" || entry.customType !== OMP_NATIVE_FACT_ENTRY_TYPE) continue;
    const fact = asFact(entry.data);
    if (fact !== undefined) {
      latest.set(fact.ompAgentId, fact);
    }
  }
  return [...latest.values()];
}

/**
 * Every persisted task correlation, latest-per-task. The newest `revision`
 * for a task wins, because `reconcile/omp` rejects any revision that is not
 * the stored one.
 */
export function persistedCorrelations(entries: readonly SessionEntryLike[]): OmpNativeTaskCorrelation[] {
  const latest = new Map<string, OmpNativeTaskCorrelation>();
  for (const entry of entries) {
    if (entry?.type !== "custom" || entry.customType !== OMP_NATIVE_CORRELATION_ENTRY_TYPE) {
      continue;
    }
    const correlation = asCorrelation(entry.data);
    if (correlation !== undefined) {
      const prior = latest.get(correlation.taskId);
      if (prior === undefined || correlation.revision >= prior.revision) {
        latest.set(correlation.taskId, correlation);
      }
    }
  }
  return [...latest.values()];
}
