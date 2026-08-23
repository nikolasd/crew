// Normalized OMP-native subagent facts and reconciliation types, mapped
// from the installed OMP `task:subagent:*` event-bus payloads without
// mutating OMP's own state.

/**
 * The Crew-facing status bucket an OMP-native mirror can occupy. Distinct
 * from the runtime's authoritative `RunState`: these are parent-scoped
 * facts the extension observes, never a `Run` row the extension itself
 * transitions.
 */
export type OmpNativeStatus = "working" | "succeeded" | "failed" | "lost";

/** A normalized fact about one OMP-native subagent. */
export interface OmpNativeAgentFact {
  /** The OMP subagent id. */
  readonly ompAgentId: string;
  /** The mapped Crew-facing status. */
  readonly status: OmpNativeStatus;
  /** The agent/assignment description, when known. */
  readonly description?: string;
  /** The subagent's session file path, when known. */
  readonly sessionFile?: string;
  /** Artifact references extracted from tool output, when known. */
  readonly artifactRefs: readonly string[];
  /** The UUID identifying the OMP process that observed this fact. */
  readonly ompProcessEpoch: string;
  /** When this fact was recorded (ms since epoch), for coalescing. */
  readonly observedAtMs: number;
}

/** Identifies the Crew task a prior `task/upsert` correlated with an
 * OMP-native subagent, so `reconcile/omp` can rebind its ownership. */
export interface OmpNativeTaskCorrelation {
  readonly taskId: string;
  readonly revision: number;
}
