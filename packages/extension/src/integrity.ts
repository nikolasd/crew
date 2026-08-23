// Small file-integrity helper shared by `platform.ts`. Kept in its own
// module because it has no dependency on platform-selection concerns and is
// trivial to unit test in isolation.

import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

/**
 * Returns the lowercase hex SHA-256 digest of the file at `path`.
 *
 * Synchronous by design: {@link resolveCrewd} in `platform.ts` is called
 * from `ensureRuntime`'s synchronous `packagedBinaryResolver` seam, so every
 * step of package-binary resolution -- including this integrity check --
 * must complete before any process is spawned, without an event-loop turn.
 */
export function sha256File(path: string): string {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}
