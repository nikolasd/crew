//! Superseded by `crates/runtime/tests/crew_config.rs` (crew-v2 gap-closure
//! WP4/WP5): this file tested the YAML org/repo/user layering system that
//! WP5 removed (`crates/runtime/src/config/mod.rs`'s `LayeredConfig`,
//! `merge.rs`, org locks). Several of its tests asserted the opposite of
//! their own stated behavior -- see the WP5 report for the audit -- and
//! the two that were genuinely valid (fingerprint stability, key-order
//! invariance) are pinned instead by `crew_config.rs`'s
//! `fingerprint_is_stable_under_key_order` and
//! `fingerprint_differs_for_different_configs`.
//!
//! Left in place, emptied, rather than deleted outright: this session's
//! sandbox denies `rm`. The exact command to finish the removal:
//! `rm crates/runtime/tests/config.rs`.
