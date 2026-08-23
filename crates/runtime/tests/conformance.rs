//! Integration tests for the `crewd adapters`/`crewd conformance`
//! CLI subcommands (`crates/runtime/src/cli.rs`), driven against the
//! real compiled binary as a genuine subprocess -- not the library's
//! own unit-level `conformance::run_fixture_conformance` dispatcher
//! (that seam is covered by `crates/runtime/src/conformance/mod.rs`'s
//! own `#[cfg(test)]` module instead).
//!
//! Never invokes a model: `--fixture` is this milestone's own zero-
//! model-call design invariant, and every `--live` case here sets
//! `CREW_DISABLE_VENDOR_CLI=1`, proving the CLI degrades to an honest
//! per-adapter error rather than ever making a real call.

use std::process::Command;

fn crewd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_crewd"))
}

#[test]
fn adapters_json_reports_all_four_adapters_with_effective_capabilities() {
    let output = crewd()
        .arg("adapters")
        .arg("--json")
        .output()
        .expect("crewd adapters --json must be runnable");
    assert!(
        output.status.success(),
        "crewd adapters --json exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reports: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    let reports = reports
        .as_array()
        .expect("adapters --json prints a JSON array");
    assert_eq!(
        reports.len(),
        4,
        "expected exactly one entry per reserved adapter kind"
    );

    let mut seen_kinds: Vec<&str> = Vec::new();
    for report in reports {
        let adapter = report["adapter"]
            .as_str()
            .expect("every entry names its adapter");
        seen_kinds.push(adapter);
        assert_eq!(report["mode"], "fixture");
        assert!(
            report["declaredCapabilities"].is_object(),
            "{adapter}: declaredCapabilities must be present"
        );
        assert!(
            report["effectiveCapabilities"].is_object(),
            "{adapter}: effectiveCapabilities must be present"
        );
        let scenarios = report["scenarios"]
            .as_array()
            .unwrap_or_else(|| panic!("{adapter}: scenarios must be a JSON array"));
        assert_eq!(
            scenarios.len(),
            14,
            "{adapter}: every effective capability must point to a passing fixture scenario, \
             which requires every one of the 14 canonical scenarios to have actually run: {scenarios:?}"
        );
    }
    for expected in ["claude", "codex", "copilot", "ompRpc"] {
        assert!(
            seen_kinds.contains(&expected),
            "adapters --json is missing the {expected} entry: {seen_kinds:?}"
        );
    }
}

#[test]
fn conformance_fixture_all_writes_four_reports_matching_stdout() {
    let output_path =
        std::env::temp_dir().join(format!("crew-conformance-test-{}.json", std::process::id()));
    let output = crewd()
        .args(["conformance", "--adapter", "all", "--fixture", "--output"])
        .arg(&output_path)
        // The committed baseline declares the switch-set posture, and the
        // baseline gate is bidirectional, so setting the switch here rather
        // than inheriting it keeps this deterministic on a machine that does
        // have the vendor CLIs installed.
        .env(batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV, "1")
        .output()
        .expect("crewd conformance --adapter all --fixture must be runnable");
    assert!(
        output.status.success(),
        "conformance --adapter all --fixture exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout_json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    let file_contents =
        std::fs::read_to_string(&output_path).expect("--output file must have been written");
    let file_json: serde_json::Value =
        serde_json::from_str(&file_contents).expect("--output file must be valid JSON");
    assert_eq!(
        stdout_json, file_json,
        "stdout and the --output file must carry the exact same report"
    );
    assert_eq!(file_json.as_array().expect("a JSON array").len(), 4);

    let _ = std::fs::remove_file(&output_path);
}

#[test]
fn conformance_fixture_one_adapter_writes_a_single_element_array() {
    let output_path = std::env::temp_dir().join(format!(
        "crew-conformance-test-single-{}.json",
        std::process::id()
    ));
    let output = crewd()
        .args(["conformance", "--adapter", "codex", "--fixture", "--output"])
        .arg(&output_path)
        // Same reason as the `--adapter all` case above: the baseline gate is
        // only deterministic with the switch explicitly set.
        .env(batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV, "1")
        .output()
        .expect("must be runnable");
    assert!(output.status.success());
    let file_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&output_path).unwrap()).unwrap();
    let reports = file_json.as_array().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["adapter"], "codex");
    let _ = std::fs::remove_file(&output_path);
}

#[test]
fn conformance_rejects_an_unknown_adapter_kind() {
    let output_path = std::env::temp_dir().join("crew-conformance-test-unknown.json");
    let output = crewd()
        .args(["conformance", "--adapter", "bogus", "--fixture", "--output"])
        .arg(&output_path)
        .output()
        .expect("must be runnable");
    assert!(
        !output.status.success(),
        "an unknown --adapter value must be rejected, not silently accepted"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("bogus"),
        "the rejection message should name the bad value"
    );
    assert!(
        !output_path.exists(),
        "no report file should be written on rejection"
    );
}

#[test]
fn conformance_requires_exactly_one_of_fixture_or_live() {
    let output_path = std::env::temp_dir().join("crew-conformance-test-both.json");

    // Neither flag.
    let neither = crewd()
        .args(["conformance", "--adapter", "claude", "--output"])
        .arg(&output_path)
        .output()
        .expect("must be runnable");
    assert!(
        !neither.status.success(),
        "requiring neither --fixture nor --live must be rejected"
    );

    // Both flags.
    let both = crewd()
        .args([
            "conformance",
            "--adapter",
            "claude",
            "--fixture",
            "--live",
            "--output",
        ])
        .arg(&output_path)
        .output()
        .expect("must be runnable");
    assert!(
        !both.status.success(),
        "supplying both --fixture and --live must be rejected"
    );

    assert!(!output_path.exists());
}

/// R52: fixture mode used to reach a real vendor-CLI spawn on every
/// adapter regardless of the kill switch -- `probe_scenario`, Claude's
/// `live_process_scenarios`/`cancellation_scope_scenario`, Codex's
/// `spawn_raw_client`, Copilot's `real_client`, and OMP-RPC's
/// `resolve_conformance_selector`/`resume_flag_probe` all spawned before
/// anything consulted `CREW_DISABLE_VENDOR_CLI`.
///
/// `PATH` is scrubbed to `/usr/bin:/bin` so the assertion is meaningful on
/// a developer machine that does have the vendor CLIs installed: with the
/// switch honored nothing is spawned at all, and with it ignored the spawn
/// fails loudly with an `ENOENT`-shaped detail. Asserting on the absence of
/// that detail -- rather than on the switch merely being read somewhere --
/// is what actually proves no spawn was attempted.
#[test]
fn conformance_fixture_with_the_kill_switch_never_spawns_a_vendor_cli() {
    for adapter in ["claude", "codex", "copilot", "ompRpc"] {
        let output_path = std::env::temp_dir().join(format!(
            "crew-conformance-test-no-spawn-{adapter}-{}.json",
            std::process::id()
        ));
        let output = crewd()
            .args(["conformance", "--adapter", adapter, "--fixture", "--output"])
            .arg(&output_path)
            .env(batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV, "1")
            .env("PATH", "/usr/bin:/bin")
            .output()
            .expect("must be runnable");
        assert!(
            output.status.success(),
            "{adapter}: fixture mode with the kill switch set must still satisfy the committed \
             baseline: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let reports: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|err| panic!("{adapter}: stdout must be valid JSON: {err}"));
        let scenarios = reports[0]["scenarios"]
            .as_array()
            .unwrap_or_else(|| panic!("{adapter}: scenarios must be a JSON array"))
            .clone();
        assert_eq!(scenarios.len(), 14, "{adapter}: every scenario must be run");
        for scenario in &scenarios {
            let name = scenario["name"].as_str().expect("a scenario names itself");
            let detail = scenario["detail"]
                .as_str()
                .expect("a scenario carries a detail");
            // The first two catch the `ENOENT` shape claude, codex and
            // omp_rpc's `resume_flag_probe` would produce; the last two are
            // copilot's `real_client` and omp_rpc's `resume_flag_probe` own
            // pre-guard failure signatures, which are not `ENOENT`-shaped and
            // would otherwise only be caught indirectly by
            // `crates/runtime/tests/copilot_adapter.rs`.
            for spawn_marker in [
                "No such file or directory",
                "failed to spawn",
                "copilot CLI not found on PATH",
                "the omp binary is unavailable to run",
            ] {
                assert!(
                    !detail.contains(spawn_marker),
                    "{adapter}/{name}: fixture mode attempted a real vendor-CLI spawn despite \
                     {}=1 -- detail was {detail:?}",
                    batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV
                );
            }
        }

        // PROBE is the one scenario that must degrade to a *skip*: a pass
        // would fabricate proof the probe never produced (R52), and a denial
        // would make every run in CI unauthorized -- exactly as
        // `probe_availability`'s own doc explains.
        let probe = scenarios
            .iter()
            .find(|s| s["name"] == "probe")
            .unwrap_or_else(|| panic!("{adapter}: probe must be among the scenarios"));
        assert_eq!(
            probe["outcome"], "skipped",
            "{adapter}: a kill-switched probe must be skipped, not pass or fail: {probe:?}"
        );

        let _ = std::fs::remove_file(&output_path);
    }
}

#[test]
fn conformance_live_with_the_kill_switch_reports_an_honest_error_not_a_hard_failure() {
    let output_path = std::env::temp_dir().join(format!(
        "crew-conformance-test-live-{}.json",
        std::process::id()
    ));
    let output = crewd()
        .args(["conformance", "--adapter", "claude", "--live", "--output"])
        .arg(&output_path)
        // Setting the kill switch is what makes this deterministic: it
        // forbids the vendor process regardless of whether `claude` happens
        // to be installed on the machine running the suite.
        .env(batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV, "1")
        .output()
        .expect("must be runnable");
    assert!(
        output.status.success(),
        "a set kill switch must not hard-fail the whole command: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reports: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let reports = reports.as_array().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["adapter"], "claude");
    assert_eq!(reports[0]["mode"], "live");
    assert_eq!(reports[0]["passed"], false);
    assert!(
        reports[0]["error"]
            .as_str()
            .expect("a disabled-CLI report carries an error string")
            .contains(batman_runtime::conformance::DISABLE_VENDOR_CLI_ENV),
        "the error must name the switch that forbade the invocation: {reports:?}"
    );
    let _ = std::fs::remove_file(&output_path);
}
