//! Drift gate for the two committed config artifacts at the repository
//! root: `crew.default.json` (the readable reference snapshot users copy
//! from) and `crew-config.schema.json` (what a `$schema` annotation points
//! at so editors autocomplete and validate).
//!
//! Both are generated from `CrewConfig` -- `crewd config print --defaults`
//! and `crewd config print --schema` emit exactly these bytes. This test
//! is the gate that keeps them from going stale: it runs under the
//! `cargo test --workspace` CI already performs, so a default changed in
//! Rust without regenerating the artifacts fails the build with the
//! command to fix it.

use std::path::{Path, PathBuf};

use crew_runtime::config::crew::{self, CrewConfig};

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root resolves from the runtime crate manifest")
}

fn read_artifact(name: &str) -> String {
    let path = repo_root().join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} must be committed at the repo root: {e}", path.display()))
}

/// The committed snapshot must be byte-identical to what
/// `crewd config print --defaults` emits.
#[test]
fn committed_default_snapshot_matches_generated_bytes() {
    let generated = String::from_utf8(crew::render_default_document()).unwrap();

    assert_eq!(
        read_artifact("crew.default.json"),
        generated,
        "crew.default.json is stale; regenerate with \
         `cargo run -p crew-runtime --bin crewd -- config print --defaults > crew.default.json`"
    );
}

/// The committed schema must be byte-identical to what
/// `crewd config print --schema` emits.
#[test]
fn committed_config_schema_matches_generated_bytes() {
    let generated = String::from_utf8(crew::render_config_schema()).unwrap();

    assert_eq!(
        read_artifact("crew-config.schema.json"),
        generated,
        "crew-config.schema.json is stale; regenerate with \
         `cargo run -p crew-runtime --bin crewd -- config print --schema > crew-config.schema.json`"
    );
}

/// The snapshot is not just readable reference material -- it has to be a
/// file the daemon actually accepts, `$schema` annotation and all.
#[test]
fn committed_default_snapshot_loads_and_equals_the_builtin_defaults() {
    let path = repo_root().join("crew.default.json");

    let loaded = crew::load_layers(&[path.as_path()], None)
        .expect("the committed snapshot must load through the daemon's own loader");

    assert_eq!(loaded, CrewConfig::default());
}

/// The snapshot points editors at the committed schema, or the `$schema`
/// support buys users nothing.
#[test]
fn committed_default_snapshot_points_at_the_committed_schema() {
    let raw = read_artifact("crew.default.json");
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();

    assert_eq!(
        value
            .get(crew::SCHEMA_ANNOTATION_KEY)
            .and_then(|v| v.as_str()),
        Some("./crew-config.schema.json")
    );
}

/// CREW-8: `packages/extension/src/crew-config.ts`'s TS-side model
/// resolution reads `crew.json`'s `adapters.*` section keys directly, and
/// hardcodes the one place they diverge from `AdapterKind::wire_name`
/// (`"ompRpc"` -> `"omp"`, per `RESERVED_ADAPTER_CONFIG_KEYS`'s own doc
/// comment). That mapping silently goes stale if this set ever changes
/// shape -- pinned here against the actual generated document, not just
/// hand-copied, so a future rename fails this test with a clear diff
/// instead of silently breaking the TS side's lookup.
#[test]
fn committed_default_snapshot_declares_exactly_the_reserved_adapter_config_keys() {
    let raw = read_artifact("crew.default.json");
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();

    let mut keys: Vec<&str> = value["adapters"]
        .as_object()
        .expect("crew.default.json must have an adapters object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();

    let mut expected = ["claude", "codex", "copilot", "omp"];
    expected.sort_unstable();

    assert_eq!(
        keys, expected,
        "crew.default.json's adapters keys drifted from the reserved set -- update \
         packages/extension/src/crew-config.ts's CONFIG_KEY_FOR_ADAPTER mapping to match"
    );
}
