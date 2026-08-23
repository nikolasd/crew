//! One tiny helper shared by every renamed `CREW_*`/`OMP_CREW_*`
//! environment variable that used to be `BATMAN_*`/`OMP_BATMAN_*`: read the
//! new name, falling back to the old one, so an existing shell, CI job, or
//! `.env` file that still sets the pre-rename name keeps working unchanged
//! during the migration. Mirrors TypeScript's `envFlag`
//! (`packages/extension/src/env-flag.ts`).

use std::env;

/// Reads `new` from the process environment, falling back to `old` when
/// `new` is unset.
#[must_use]
pub fn env_flag(new: &str, old: &str) -> Option<String> {
    env::var(new).ok().or_else(|| env::var(old).ok())
}

/// Same as [`env_flag`], but reads from an explicit map instead of the
/// process environment -- for call sites that already thread a map through
/// for testability (e.g. [`crate::security::StateRoot::resolve`]).
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
