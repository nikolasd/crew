//! One tiny helper for a renamed `CREW_*`/`OMP_CREW_*` environment
//! variable that used to be `BATMAN_*`/`OMP_BATMAN_*`: read the new name,
//! falling back to the old one, so an existing shell, CI job, or `.env`
//! file that still sets the pre-rename name keeps working unchanged during
//! the migration. Mirrors TypeScript's `envFlag`
//! (`packages/extension/src/env-flag.ts`).
//!
//! Only `BATMAN_STATE_DIR` still needs this. The vendor-CLI kill switch
//! used to, and no longer does: `.cargo/config.toml` sets
//! `CREW_DISABLE_VENDOR_CLI` for every cargo-launched process, which means
//! the new name is never absent under cargo and the fallback to the old one
//! could never fire. A switch that cannot be reached is worse than a
//! retired one -- someone setting the old name for a live run would have
//! got fixture mode and no indication why -- so it was retired outright.

/// Reads `new` from an explicit environment map, falling back to `old` when
/// `new` is absent -- for call sites that already thread a map through for
/// testability (e.g. [`crate::security::StateRoot::resolve`]).
#[must_use]
pub fn env_flag_from(
    env: &std::collections::HashMap<String, String>,
    new: &str,
    old: &str,
) -> Option<String> {
    env.get(new).or_else(|| env.get(old)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_map_prefers_new_over_old() {
        let mut env = std::collections::HashMap::new();
        env.insert("NEW".to_string(), "new-value".to_string());
        env.insert("OLD".to_string(), "old-value".to_string());
        assert_eq!(
            env_flag_from(&env, "NEW", "OLD"),
            Some("new-value".to_string())
        );
    }

    #[test]
    fn from_map_falls_back_to_old_when_new_is_absent() {
        let mut env = std::collections::HashMap::new();
        env.insert("OLD".to_string(), "old-value".to_string());
        assert_eq!(
            env_flag_from(&env, "NEW", "OLD"),
            Some("old-value".to_string())
        );
    }

    #[test]
    fn from_map_is_none_when_neither_is_set() {
        let env = std::collections::HashMap::new();
        assert_eq!(env_flag_from(&env, "NEW", "OLD"), None);
    }
}
