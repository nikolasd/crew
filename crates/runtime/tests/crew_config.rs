//! Integration tests for the crew JSON config module (spec §10).
//!
//! Layers: built-in defaults → arbitrary ordered file paths → per-run
//! overrides. Deep merge, later layers win, `security.patterns` is
//! additive, and an unknown key at any depth is a hard error naming the
//! JSON path.

use std::collections::BTreeMap;
use std::path::Path;

use crew_runtime::config::crew::{
    self, AdapterConfig, AdapterMode, ApprovalMode, CloseOnExit, ConfigError, CrewConfig,
    DashboardConfig, DisplayBackend, DisplayConfig, Limits, PermissionMode, RetentionConfig,
    SecurityConfig, WorkspaceConfig, WorkspaceMode,
};
use serde_json::json;
use tempfile::tempdir;

fn write_layer(dir: &Path, name: &str, value: &serde_json::Value) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    path
}

/// Defaults match spec §10, with the controller override that every
/// adapter's `mode` defaults to `headless` except `claude`, whose TUI
/// adapter has landed (WP13) and so defaults to `tui`.
#[test]
fn defaults_match_spec_with_headless_mode_override() {
    let cfg = crew::load_layers(&[], None).expect("defaults load with no layers");

    assert_eq!(cfg.approval, ApprovalMode::Always);

    assert_eq!(cfg.limits.max_concurrent_workers, 4);
    assert_eq!(cfg.limits.inactivity_timeout_sec, 300);
    assert_eq!(cfg.limits.total_timeout_sec, 1800);
    assert_eq!(cfg.limits.turn_budget_per_subtask, 10);

    assert_eq!(cfg.display.backend, crew::DisplayBackend::Auto);
    assert_eq!(cfg.display.close_on_exit, crew::CloseOnExit::OnSuccess);

    assert_eq!(cfg.adapters.len(), 4);
    for name in ["claude", "codex", "copilot", "omp"] {
        let adapter = cfg.adapters.get(name).unwrap_or_else(|| {
            panic!("expected default adapter '{name}'");
        });
        assert!(adapter.enabled);
        // Every built-in adapter defaults to `mode: tui` since WP28
        // (all four TuiVendor impls pass fixture-mode conformance).
        assert_eq!(adapter.mode, AdapterMode::Tui, "adapter '{name}' mode");
        assert_eq!(adapter.permission_mode, PermissionMode::Max);
    }
    assert_eq!(cfg.adapters["claude"].bin, "claude");
    assert_eq!(
        cfg.adapters["claude"].profile,
        "complex analysis, investigation, deep debugging"
    );
    assert_eq!(
        cfg.adapters["codex"].profile,
        "code review, finding defects"
    );
    assert_eq!(
        cfg.adapters["copilot"].profile,
        "documentation, explanations"
    );
    assert_eq!(cfg.adapters["omp"].profile, "implementation, coding tasks");
    assert_eq!(cfg.adapters["omp"].model, Some("qwen".to_string()));
    assert_eq!(cfg.adapters["claude"].model, None);

    assert_eq!(cfg.workspace.default_mode, crew::WorkspaceMode::Shared);
    assert_eq!(cfg.workspace.copy_max_bytes, None);
    assert_eq!(cfg.workspace.copy_max_files, None);

    assert!(!cfg.dashboard.enabled);
    assert_eq!(cfg.dashboard.port, 4747);

    assert_eq!(cfg.retention.max_runs, 20);
    assert_eq!(cfg.retention.period, "30d");

    assert!(cfg.security.patterns.is_empty());
}

/// A layer that overrides one field in `limits` must leave its siblings
/// at the built-in defaults (deep merge, not layer-replaces-object).
#[test]
fn deep_merge_overriding_one_limit_leaves_siblings_at_defaults() {
    let dir = tempdir().unwrap();
    let user = write_layer(
        dir.path(),
        "user.json",
        &json!({ "limits": { "maxConcurrentWorkers": 8 } }),
    );

    let cfg = crew::load_layers(&[user.as_path()], None).expect("layer merges");

    assert_eq!(cfg.limits.max_concurrent_workers, 8);
    // Siblings survive from defaults.
    assert_eq!(cfg.limits.inactivity_timeout_sec, 300);
    assert_eq!(cfg.limits.total_timeout_sec, 1800);
    assert_eq!(cfg.limits.turn_budget_per_subtask, 10);
}

/// `security.patterns` concatenates across layers instead of the later
/// layer replacing the earlier one's list.
#[test]
fn security_patterns_are_additive_across_layers() {
    let dir = tempdir().unwrap();
    let user = write_layer(
        dir.path(),
        "user.json",
        &json!({ "security": { "patterns": ["user-pattern"] } }),
    );
    let project = write_layer(
        dir.path(),
        "project.json",
        &json!({ "security": { "patterns": ["project-pattern"] } }),
    );

    let cfg = crew::load_layers(&[user.as_path(), project.as_path()], None)
        .expect("layers merge additively");

    assert_eq!(
        cfg.security.patterns,
        vec!["user-pattern", "project-pattern"]
    );
}

/// An unknown key at any depth fails closed, naming the JSON path.
#[test]
fn unknown_key_at_depth_errors_with_json_path() {
    let dir = tempdir().unwrap();
    let user = write_layer(
        dir.path(),
        "user.json",
        &json!({ "limits": { "maxConcurrentWorkers": 8, "bogusField": true } }),
    );

    let err = crew::load_layers(&[user.as_path()], None).expect_err("unknown key must fail");

    match err {
        ConfigError::UnknownKey { path } => assert_eq!(path, "limits.bogusField"),
        other => panic!("expected UnknownKey, got: {other}"),
    }
}

/// An unknown top-level key also fails, naming just that key as the path.
#[test]
fn unknown_top_level_key_errors_with_bare_path() {
    let dir = tempdir().unwrap();
    let user = write_layer(dir.path(), "user.json", &json!({ "totallyBogus": 1 }));

    let err = crew::load_layers(&[user.as_path()], None).expect_err("unknown key must fail");

    match err {
        ConfigError::UnknownKey { path } => assert_eq!(path, "totallyBogus"),
        other => panic!("expected UnknownKey, got: {other}"),
    }
}

/// The per-run override layer applies on top of file layers.
#[test]
fn per_run_layer_applies_on_top_of_file_layers() {
    let dir = tempdir().unwrap();
    let user = write_layer(
        dir.path(),
        "user.json",
        &json!({ "limits": { "maxConcurrentWorkers": 8 } }),
    );
    let per_run = json!({ "limits": { "maxConcurrentWorkers": 2 } });

    let cfg = crew::load_layers(&[user.as_path()], Some(&per_run)).expect("per-run layer applies");

    assert_eq!(cfg.limits.max_concurrent_workers, 2);
    assert_eq!(cfg.limits.inactivity_timeout_sec, 300);
}

/// An arbitrary adapter name is accepted, but its inner shape is still
/// strict — an unknown field inside it still errors with the full path.
#[test]
fn unknown_adapter_name_accepted_with_strict_inner_shape() {
    let dir = tempdir().unwrap();
    let user = write_layer(
        dir.path(),
        "user.json",
        &json!({
            "adapters": {
                "gemini": {
                    "enabled": true,
                    "bin": "gemini",
                    "mode": "headless",
                    "permissionMode": "max",
                    "profile": "custom vendor"
                }
            }
        }),
    );

    let cfg = crew::load_layers(&[user.as_path()], None).expect("unknown adapter name accepted");
    let gemini = cfg.adapters.get("gemini").expect("gemini adapter present");
    assert_eq!(gemini.bin, "gemini");
    assert_eq!(gemini.profile, "custom vendor");
    // Built-in adapters are untouched siblings.
    assert_eq!(cfg.adapters.len(), 5);

    let bad = write_layer(
        dir.path(),
        "bad.json",
        &json!({
            "adapters": {
                "gemini": {
                    "enabled": true,
                    "bin": "gemini",
                    "mode": "headless",
                    "permissionMode": "max",
                    "profile": "custom vendor",
                    "notAField": true
                }
            }
        }),
    );

    let err = crew::load_layers(&[bad.as_path()], None).expect_err("unknown inner field fails");
    match err {
        ConfigError::UnknownKey { path } => assert_eq!(path, "adapters.gemini.notAField"),
        other => panic!("expected UnknownKey, got: {other}"),
    }
}

/// The fingerprint is stable across layer files whose JSON keys are
/// written in different orders, since it hashes canonical bytes of the
/// deserialized (already order-independent) config.
#[test]
fn fingerprint_is_stable_under_key_order() {
    let dir = tempdir().unwrap();
    let a = write_layer(
        dir.path(),
        "a.json",
        &json!({ "limits": { "maxConcurrentWorkers": 6, "inactivityTimeoutSec": 120 } }),
    );
    // Same content, different key order within the object.
    let b_path = dir.path().join("b.json");
    std::fs::write(
        &b_path,
        r#"{ "limits": { "inactivityTimeoutSec": 120, "maxConcurrentWorkers": 6 } }"#,
    )
    .unwrap();

    let cfg_a = crew::load_layers(&[a.as_path()], None).expect("layer a loads");
    let cfg_b = crew::load_layers(&[b_path.as_path()], None).expect("layer b loads");

    assert_eq!(crew::fingerprint(&cfg_a), crew::fingerprint(&cfg_b));
}

/// Two structurally-equal configs fingerprint identically, and a changed
/// config fingerprints differently (sanity on the hash itself).
#[test]
fn fingerprint_differs_for_different_configs() {
    let default_cfg = crew::load_layers(&[], None).expect("defaults load");

    let dir = tempdir().unwrap();
    let user = write_layer(
        dir.path(),
        "user.json",
        &json!({ "limits": { "maxConcurrentWorkers": 99 } }),
    );
    let changed_cfg = crew::load_layers(&[user.as_path()], None).expect("layer loads");

    assert_ne!(
        crew::fingerprint(&default_cfg),
        crew::fingerprint(&changed_cfg)
    );
}

/// A full-field round trip: every field of `CrewConfig` set to a distinct
/// non-default value, serialized to a single layer file, then re-loaded
/// through `load_layers` (which walks `validate_shape`'s hand-maintained
/// `*_KEYS` tables before deserializing). Guards the tables staying in
/// sync with the struct field lists -- a field present in the struct but
/// missing from its `*_KEYS` entry would make this test fail with an
/// `UnknownKey` naming exactly that field, since every field here is
/// deliberately populated and none left at its default.
#[test]
fn full_field_round_trip_through_json_survives_validate_shape() {
    // `load_layers` deep-merges this layer onto `CrewConfig::default()`,
    // which already carries the four built-in adapter names -- so every
    // one of them must be listed here too (each overridden to a distinct
    // non-default value), or the round trip would spuriously "pass" with
    // leftover defaults for whichever built-in name was omitted.
    let mut adapters = BTreeMap::new();
    for name in ["claude", "codex", "copilot", "omp"] {
        adapters.insert(
            name.to_string(),
            AdapterConfig {
                enabled: false,
                bin: format!("{name}-custom-bin"),
                // Headless is a NON-default value since WP28 flipped all
                // four built-ins to tui -- this round trip must prove the
                // mode key survives, so it overrides away from the default.
                mode: AdapterMode::Headless,
                permission_mode: PermissionMode::Readonly,
                model: Some(format!("{name}-custom-model")),
                profile: format!("{name} custom profile"),
                session_dir: Some(format!("/tmp/{name}-sessions")),
                extra_args: vec![format!("--{name}-flag")],
            },
        );
    }
    adapters.insert(
        "vertex".to_string(),
        AdapterConfig {
            enabled: false,
            bin: "vertex-cli".to_string(),
            mode: AdapterMode::Headless,
            permission_mode: PermissionMode::Readonly,
            model: Some("gemini-custom".to_string()),
            profile: "a distinct custom profile".to_string(),
            session_dir: Some("/tmp/vertex-sessions".to_string()),
            extra_args: vec!["--flag-a".to_string(), "--flag-b".to_string()],
        },
    );

    let original = CrewConfig {
        approval: ApprovalMode::Never,
        limits: Limits {
            max_concurrent_workers: 7,
            inactivity_timeout_sec: 111,
            total_timeout_sec: 222,
            turn_budget_per_subtask: 3,
        },
        display: DisplayConfig {
            backend: DisplayBackend::Hidden,
            close_on_exit: CloseOnExit::Always,
        },
        adapters,
        workspace: WorkspaceConfig {
            default_mode: WorkspaceMode::Copy,
            copy_max_bytes: Some(123),
            copy_max_files: Some(45),
            artifact_max_bytes: Some(6789),
        },
        dashboard: DashboardConfig {
            enabled: true,
            port: 9999,
        },
        retention: RetentionConfig {
            max_runs: 99,
            period: "7d".to_string(),
        },
        security: SecurityConfig {
            patterns: vec!["a-pattern".to_string(), "b-pattern".to_string()],
        },
    };

    let dir = tempdir().unwrap();
    let layer = write_layer(
        dir.path(),
        "full.json",
        &serde_json::to_value(&original).expect("CrewConfig serializes"),
    );

    let round_tripped =
        crew::load_layers(&[layer.as_path()], None).expect("every field is a known key");

    assert_eq!(round_tripped, original);
}

/// A missing layer file path is treated as an absent layer, not an error
/// -- real deployments have optional user/project config files.
#[test]
fn missing_layer_file_is_treated_as_absent() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("does-not-exist.json");

    let cfg = crew::load_layers(&[missing.as_path()], None).expect("missing layer is skipped");

    assert_eq!(cfg, CrewConfig::default());
}

// ---------------------------------------------------------------- $schema

/// A `$schema` key is what makes editors autocomplete and validate
/// crew.json. `validate_shape` walks the merged value against the known
/// top-level keys, so `$schema` has to be an accepted key or every
/// schema-annotated config fails the launch.
#[test]
fn schema_key_is_accepted_at_the_top_level() {
    let dir = tempdir().unwrap();
    let layer = write_layer(
        dir.path(),
        "crew.json",
        &json!({ "$schema": "https://example.invalid/crew-config.schema.json" }),
    );

    let cfg = crew::load_layers(&[layer.as_path()], None)
        .expect("a $schema annotation must not fail the launch");

    assert_eq!(cfg, CrewConfig::default());
}

/// `$schema` is an editor annotation, not configuration: it must never
/// change the resolved config, and two configs differing only by it must
/// fingerprint identically.
#[test]
fn schema_key_does_not_affect_the_resolved_config() {
    let dir = tempdir().unwrap();
    let annotated = write_layer(
        dir.path(),
        "annotated.json",
        &json!({ "$schema": "https://example.invalid/s.json", "approval": "never" }),
    );
    let plain = write_layer(dir.path(), "plain.json", &json!({ "approval": "never" }));

    let with_schema = crew::load_layers(&[annotated.as_path()], None).unwrap();
    let without_schema = crew::load_layers(&[plain.as_path()], None).unwrap();

    assert_eq!(with_schema, without_schema);
    assert_eq!(
        crew::fingerprint(&with_schema),
        crew::fingerprint(&without_schema)
    );
}

/// A misspelled schema key must still be rejected -- accepting `$schema`
/// is a single-key allowance, not a hole in unknown-key rejection.
#[test]
fn a_misspelled_schema_key_is_still_rejected() {
    let dir = tempdir().unwrap();
    let layer = write_layer(dir.path(), "crew.json", &json!({ "$shema": "x" }));

    let err = crew::load_layers(&[layer.as_path()], None)
        .expect_err("only the exact key $schema is allowed");

    assert!(
        matches!(err, ConfigError::UnknownKey { ref path } if path == "$shema"),
        "got {err:?}"
    );
}

// ------------------------------------------------- drift against defaults

/// A layer that sets nothing overrides nothing.
#[test]
fn an_empty_layer_reports_no_overrides() {
    assert!(crew::diff_against_defaults(&json!({})).is_empty());
}

/// A key written at exactly the current built-in default is not an
/// override -- reporting it would drown the real signal.
#[test]
fn a_key_matching_the_current_default_is_not_reported() {
    let layer = json!({ "limits": { "totalTimeoutSec": 1800 } });

    assert!(crew::diff_against_defaults(&layer).is_empty());
}

/// The signal the `config_drift` doctor check reports: a key whose value
/// differs from the current built-in default, named by full JSON path,
/// carrying both values so the operator can judge it.
#[test]
fn a_key_differing_from_the_default_is_reported_with_both_values() {
    let layer = json!({ "limits": { "totalTimeoutSec": 900 } });

    let drift = crew::diff_against_defaults(&layer);

    assert_eq!(drift.len(), 1, "got {drift:?}");
    assert_eq!(drift[0].path, "limits.totalTimeoutSec");
    assert_eq!(drift[0].configured, json!(900));
    assert_eq!(drift[0].default, json!(1800));
}

/// Overrides are reported in a stable path order, so the doctor's output
/// does not reshuffle between runs over the same file.
#[test]
fn overrides_are_reported_in_stable_path_order() {
    let layer = json!({
        "retention": { "maxRuns": 5 },
        "approval": "never",
        "limits": { "maxConcurrentWorkers": 9 },
    });

    let paths: Vec<String> = crew::diff_against_defaults(&layer)
        .into_iter()
        .map(|d| d.path)
        .collect();

    assert_eq!(
        paths,
        vec![
            "approval".to_string(),
            "limits.maxConcurrentWorkers".to_string(),
            "retention.maxRuns".to_string(),
        ]
    );
}

/// `$schema` is an editor annotation with no default counterpart; it must
/// never be reported as configuration drift.
#[test]
fn the_schema_annotation_is_never_reported_as_drift() {
    let layer = json!({ "$schema": "https://example.invalid/s.json" });

    assert!(crew::diff_against_defaults(&layer).is_empty());
}

/// A custom adapter has no built-in default to drift from -- it is a
/// deliberate addition, not a stale pin, so it is not reported.
#[test]
fn a_custom_adapter_without_a_builtin_default_is_not_reported() {
    let layer = json!({ "adapters": { "mistral": { "bin": "mistral" } } });

    assert!(crew::diff_against_defaults(&layer).is_empty());
}

/// The whole point of the check: a full snapshot written by
/// `crewd config init` reports nothing today, because every value in it
/// still equals the built-in default it was generated from.
#[test]
fn a_full_default_snapshot_reports_no_drift_when_freshly_generated() {
    let snapshot = serde_json::to_value(CrewConfig::default()).unwrap();

    assert!(crew::diff_against_defaults(&snapshot).is_empty());
}
