//! `crewd doctor`'s two informational config checks.
//!
//! Neither is a failure: running on built-in defaults is a valid setup,
//! and deliberately overriding a default is the whole point of a config
//! file. They are reported as *notes* so the operator can see what their
//! configuration is actually doing without the runtime declaring itself
//! unhealthy over it.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

const CREWD: &str = env!("CARGO_BIN_EXE_crewd");

struct Fixture {
    state: TempDir,
    repo: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let state = tempfile::Builder::new()
            .prefix("crew-doc-cfg-s-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = tempfile::Builder::new()
            .prefix("crew-doc-cfg-r-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        Self { state, repo }
    }

    fn repo_dir(&self) -> &Path {
        self.repo.path()
    }

    /// Writes `<repo>/.omp/crew.json` and returns its path.
    fn write_project_layer(&self, value: &Value) {
        let dir = self.repo.path().join(".omp");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("crew.json"),
            serde_json::to_string_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn doctor_json(&self) -> Value {
        let mut cmd = Command::new(CREWD);
        cmd.arg("doctor")
            .arg("--state-dir")
            .arg(self.state.path())
            .arg("--repo")
            .arg(self.repo_dir())
            .arg("--json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Keep the user layer out of the picture: the check must report on
        // this repository, not on whoever's home directory runs the test.
        cmd.env("HOME", self.repo.path().join("no-such-home"));
        let output = cmd.output().expect("doctor runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("JSON output: {e}\n{stdout}"))
    }
}

fn note_detail(result: &Value, check_name: &str) -> Option<String> {
    result
        .get("notes")?
        .as_array()?
        .iter()
        .find(|n| n.get("check_name").and_then(Value::as_str) == Some(check_name))
        .and_then(|n| n.get("detail").and_then(Value::as_str))
        .map(str::to_string)
}

/// With no layer file anywhere, the operator is told they are on built-in
/// defaults and how to change that -- and the runtime stays healthy,
/// because running on defaults is a valid configuration.
#[test]
fn config_present_notes_the_absence_of_any_layer_without_failing() {
    let fixture = Fixture::new();

    let result = fixture.doctor_json();

    let detail = note_detail(&result, "config_present")
        .expect("a config_present note when no layer file exists");
    assert!(
        detail.contains("built-in defaults"),
        "note should say defaults are in use, got: {detail}"
    );
    assert!(
        detail.contains("config init"),
        "note should name the command that creates one, got: {detail}"
    );

    let failed = result
        .get("failed_checks")
        .and_then(Value::as_array)
        .unwrap();
    assert!(
        !failed
            .iter()
            .any(|c| c.get("check_name").and_then(Value::as_str) == Some("config_present")),
        "an absent config layer must never be a failed check: {failed:?}"
    );
}

/// A layer that exists is reported by path, so the operator knows which
/// file is actually in play.
#[test]
fn config_present_names_the_layer_file_that_exists() {
    let fixture = Fixture::new();
    fixture.write_project_layer(&serde_json::json!({}));

    let result = fixture.doctor_json();

    let detail = note_detail(&result, "config_present").expect("a config_present note");
    assert!(
        detail.contains(".omp/crew.json"),
        "note should name the layer path, got: {detail}"
    );
}

/// A layer whose values all still match the built-in defaults has no
/// drift, so there is nothing to report.
#[test]
fn config_drift_is_silent_when_no_key_diverges() {
    let fixture = Fixture::new();
    fixture.write_project_layer(&serde_json::json!({ "limits": { "totalTimeoutSec": 1800 } }));

    let result = fixture.doctor_json();

    assert_eq!(note_detail(&result, "config_drift"), None);
}

/// The signal that makes a stale pin visible: a key set to something other
/// than the current built-in default is named, with both values.
#[test]
fn config_drift_names_each_diverging_key_with_both_values() {
    let fixture = Fixture::new();
    fixture.write_project_layer(&serde_json::json!({ "limits": { "totalTimeoutSec": 900 } }));

    let result = fixture.doctor_json();

    let detail = note_detail(&result, "config_drift").expect("a config_drift note");
    assert!(
        detail.contains("limits.totalTimeoutSec"),
        "note should name the path, got: {detail}"
    );
    assert!(
        detail.contains("900") && detail.contains("1800"),
        "note should carry configured and default values, got: {detail}"
    );
}

/// Drift is information, not a fault: overriding a default on purpose is
/// the entire reason crew.json exists.
#[test]
fn config_drift_never_makes_the_runtime_unhealthy() {
    let fixture = Fixture::new();
    fixture.write_project_layer(&serde_json::json!({ "approval": "never" }));

    let result = fixture.doctor_json();

    let failed = result
        .get("failed_checks")
        .and_then(Value::as_array)
        .unwrap();
    assert!(
        !failed
            .iter()
            .any(|c| c.get("check_name").and_then(Value::as_str) == Some("config_drift")),
        "drift must never be a failed check: {failed:?}"
    );
}
