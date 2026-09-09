//! Superseded by `crates/runtime/tests/crew_config.rs`: this file tested
//! the YAML org/repo/user layering system, which was later removed
//! (replaced by crew.json in config/mod.rs and config/crew.rs).
//! Several of its tests asserted the opposite of their own stated
//! behavior, and the two that were genuinely
//! valid (fingerprint stability, key-order invariance) are pinned instead
//! by `crew_config.rs`'s `fingerprint_is_stable_under_key_order` and
//! `fingerprint_differs_for_different_configs`.
//!
//! Left in place, emptied, rather than deleted outright: this session's
//! sandbox denies `rm`. The exact command to finish the removal:
//! `rm crates/runtime/tests/config.rs`.
