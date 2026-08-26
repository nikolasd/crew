//! `crewd config` CLI behaviour: scaffolding that must never destroy an
//! operator's own configuration, and printing that must reflect the layers
//! actually in force.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const CREWD: &str = env!("CARGO_BIN_EXE_crewd");

fn repo() -> TempDir {
    tempfile::Builder::new()
        .prefix("crew-cfg-cli-")
        .tempdir_in("/tmp")
        .unwrap()
}

fn config(repo: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(CREWD);
    cmd.arg("config");
    cmd.args(args);
    // Keep the user layer out: these assertions are about this repository.
    cmd.env("HOME", repo.join("no-such-home"));
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.output().expect("crewd config runs")
}

#[test]
fn init_writes_a_loadable_snapshot_and_its_schema_side_by_side() {
    let repo = repo();

    let out = config(
        repo.path(),
        &["init", "--repo", repo.path().to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let config_path = repo.path().join(".omp/crew.json");
    let schema_path = repo.path().join(".omp/crew-config.schema.json");
    assert!(config_path.exists(), "crew.json must be written");
    assert!(
        schema_path.exists(),
        "the schema must land beside it or the relative $schema reference dangles"
    );

    let loaded = crew_runtime::config::crew::load_layers(&[config_path.as_path()], None)
        .expect("what init writes must be a file the daemon accepts");
    assert_eq!(loaded, crew_runtime::config::crew::CrewConfig::default());
}

/// The file belongs to the operator. Silently replacing hand-tuned
/// configuration would be the worst possible failure for a convenience
/// command, so an existing file is left byte-for-byte untouched.
#[test]
fn init_refuses_to_clobber_an_existing_config_and_leaves_it_intact() {
    let repo = repo();
    let dir = repo.path().join(".omp");
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("crew.json");
    let original = "{ \"approval\": \"never\" }";
    std::fs::write(&config_path, original).unwrap();

    let out = config(
        repo.path(),
        &["init", "--repo", repo.path().to_str().unwrap()],
    );

    assert!(
        !out.status.success(),
        "an existing file must fail the command"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--force"),
        "the refusal must name the escape hatch: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        original,
        "the operator's file must be untouched"
    );
}

#[test]
fn init_force_overwrites_an_existing_config() {
    let repo = repo();
    let dir = repo.path().join(".omp");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("crew.json"), "{ \"approval\": \"never\" }").unwrap();

    let out = config(
        repo.path(),
        &["init", "--repo", repo.path().to_str().unwrap(), "--force"],
    );

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let written = std::fs::read_to_string(dir.join("crew.json")).unwrap();
    assert!(
        written.contains("\"$schema\""),
        "the snapshot replaces the old file"
    );
}

/// The operator is told, at the moment of writing, that a full snapshot
/// turns every key into an override. Writing that file silently is what
/// creates a stale pin nobody remembers choosing.
#[test]
fn init_warns_that_a_full_snapshot_pins_every_key() {
    let repo = repo();

    let out = config(
        repo.path(),
        &["init", "--repo", repo.path().to_str().unwrap()],
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("override"),
        "init must say the snapshot overrides rather than tracks defaults: {stdout}"
    );
}

#[test]
fn print_effective_reflects_the_project_layer() {
    let repo = repo();
    let dir = repo.path().join(".omp");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("crew.json"), "{ \"approval\": \"never\" }").unwrap();

    let out = config(
        repo.path(),
        &[
            "print",
            "--effective",
            "--repo",
            repo.path().to_str().unwrap(),
        ],
    );

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("effective config is JSON");
    assert_eq!(
        value.get("approval").and_then(|v| v.as_str()),
        Some("never")
    );
}

/// A malformed layer must fail loudly with the file named, not print a
/// silently-defaulted config that misrepresents what is in force.
#[test]
fn print_effective_fails_loudly_on_an_unknown_key() {
    let repo = repo();
    let dir = repo.path().join(".omp");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("crew.json"), "{ \"limits\": { \"nope\": 1 } }").unwrap();

    let out = config(
        repo.path(),
        &[
            "print",
            "--effective",
            "--repo",
            repo.path().to_str().unwrap(),
        ],
    );

    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("limits.nope"),
        "the exact JSON path must be named: {stderr}"
    );
}

#[test]
fn path_reports_both_layers_in_precedence_order() {
    let repo = repo();

    let out = config(
        repo.path(),
        &["path", "--repo", repo.path().to_str().unwrap()],
    );

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let user_at = stdout.find(".omp/crew.json").expect("a user layer line");
    let project_at = stdout
        .rfind(".omp/crew.json")
        .expect("a project layer line");
    assert!(
        user_at < project_at,
        "user layer must be listed first: {stdout}"
    );
    assert!(
        stdout.contains("built-in defaults"),
        "an empty set must say so: {stdout}"
    );
}
