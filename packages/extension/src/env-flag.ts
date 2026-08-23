/**
 * Reads `newName` from `env`, falling back to `oldName` when `newName` is
 * unset -- the migration shim for every renamed `CREW_*`/`OMP_CREW_*`
 * environment variable that used to be `BATMAN_*`/`OMP_BATMAN_*`. Mirrors
 * Rust's `env_flag` (`crates/runtime/src/conformance/mod.rs`) so an existing
 * shell, CI job, or `.env` file that still sets the old name keeps working
 * unchanged during the migration.
 */
export function envFlag(env: Readonly<Record<string, string | undefined>>, newName: string, oldName: string): string | undefined {
  return env[newName] ?? env[oldName];
}
