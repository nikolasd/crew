//! Integration tests for `crewd doctor` CLI command.
//!
//! These tests verify the doctor command's behavior with various inputs:
//! - Valid repository with proper state directory
//! - Invalid/missing repository
//! - Missing state directory
//! - JSON output mode
//! - Error handling

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
            .prefix("bat-doc-s-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = tempfile::Builder::new()
            .prefix("bat-doc-r-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        Self { state, repo }
    }

    fn state_dir(&self) -> &Path {
        self.state.path()
    }

    fn repo_dir(&self) -> &Path {
        self.repo.path()
    }

    fn doctor(&self, json: bool) -> Command {
        let mut cmd = Command::new(CREWD);
        cmd.arg("doctor")
            .arg("--state-dir")
            .arg(self.state_dir())
            .arg("--repo")
            .arg(self.repo_dir());
        if json {
            cmd.arg("--json");
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd
    }
}

#[test]
fn doctor_with_missing_db_returns_failure() {
    let fixture = Fixture::new();
    let mut cmd = fixture.doctor(false);
    let output = cmd.output().expect("failed to execute doctor");

    // Should fail because no database exists yet
    assert!(!output.status.success());
}

#[test]
fn doctor_json_mode_with_missing_db() {
    let fixture = Fixture::new();
    let mut cmd = fixture.doctor(true);
    let output = cmd.output().expect("failed to execute doctor");

    // Should fail and output JSON
    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should be valid JSON even on failure
    let parsed: Result<Value, _> = serde_json::from_str(&stdout);
    assert!(parsed.is_ok(), "JSON output should be parseable: {stdout}");

    let json = parsed.unwrap();
    assert_eq!(json.get("healthy").and_then(|v| v.as_bool()), Some(false));
    assert!(json.get("error").is_some() || json.get("failed_checks").is_some());
}

#[test]
fn doctor_with_nonexistent_state_dir() {
    // A `--state-dir` that doesn't exist yet is not an error condition:
    // `RuntimePaths::resolve` provisions it (mode 0700) before `Doctor` ever
    // runs a check, the same way `serve` would. This pins that provisioning
    // behavior rather than treating "didn't exist at the command line" as
    // a failure the way a genuinely unwritable path would be.
    let fixture = Fixture::new();
    let state_dir = fixture.state_dir().join("does/not/exist/yet");
    assert!(
        !state_dir.exists(),
        "fixture precondition: must start absent"
    );

    let mut cmd = Command::new(CREWD);
    cmd.arg("doctor")
        .arg("--json")
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--repo")
        .arg(fixture.repo_dir())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output().expect("failed to execute doctor");

    assert!(
        state_dir.exists(),
        "doctor should have provisioned the missing state dir, same as `serve`"
    );

    // Still unhealthy overall (no database, no rollout-gate config were
    // supplied), but never because the state dir itself was missing.
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).expect("doctor --json prints valid JSON");
    let failed_checks = json["failed_checks"]
        .as_array()
        .expect("failed_checks is an array");
    assert!(
        failed_checks
            .iter()
            .all(|check| check["check_name"] != "state_dir_writable"),
        "a freshly-provisioned state dir must not fail state_dir_writable: {failed_checks:?}"
    );
}

#[test]
fn doctor_with_nonexistent_repo() {
    let fixture = Fixture::new();
    let mut cmd = Command::new(CREWD);
    cmd.arg("doctor")
        .arg("--state-dir")
        .arg(fixture.state_dir())
        .arg("--repo")
        .arg("/tmp/does/not/exist/repo")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output().expect("failed to execute doctor");

    // Should fail
    assert!(!output.status.success());
}

// --- Check-catalog tests -------------------------------------------------
//
// These drive `Doctor` directly rather than the CLI: a failing condition
// for one check has to be forced in isolation, and the CLI only exposes
// the aggregate.

use batman_protocol::ProjectId;
use batman_runtime::config::{LayeredConfig, RuntimePolicy};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::doctor::{Doctor, DoctorResult};
use std::sync::Arc;

/// The default merged policy: no layers, so `merge` yields the built-in
/// defaults. This is the common case the doctor must never treat as an
/// error.
fn default_policy() -> RuntimePolicy {
    LayeredConfig::load(None, None, None)
        .expect("no layers always loads")
        .merge(None)
        .expect("the default policy always merges")
}

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/runtime is nested two levels below the workspace root")
        .to_path_buf()
}

fn error_for<'a>(result: &'a DoctorResult, check_name: &str) -> Option<&'a str> {
    result
        .failed_checks
        .iter()
        .find(|c| c.check_name == check_name)
        .map(|c| c.error.as_str())
}

/// Builds a doctor over a fresh temp state dir with full runtime context.
async fn doctor_over(state: &Path, policy: RuntimePolicy) -> (Doctor, Arc<DatabaseHandle>) {
    let db = Arc::new(
        DatabaseHandle::start(state.join("runtime.db"))
            .await
            .expect("database opens"),
    );
    let doctor = Doctor::new(
        Some(Arc::clone(&db)),
        Some(state.to_path_buf()),
        Some(policy),
    )
    .with_runtime_context(state.join("runtime.sock"), repo_root(), ProjectId::new());
    (doctor, db)
}

#[tokio::test]
async fn a_check_that_cannot_run_is_never_reported_as_passed() {
    let result = Doctor::empty().check().await.expect("catalog runs");

    assert!(
        result
            .passed_checks
            .iter()
            .all(|name| !name.contains("skipped")),
        "a skipped check must not land in passed_checks: {:?}",
        result.passed_checks
    );
    assert!(
        error_for(&result, "database_connectivity").is_some_and(|e| e.contains("skipped:")),
        "an unrunnable check must be a failure carrying its reason: {:?}",
        result.failed_checks
    );
    assert!(
        !result.healthy,
        "nothing was verified, so nothing is healthy"
    );
}

#[tokio::test]
async fn configuration_valid_fails_on_a_zero_worker_ceiling() {
    let state = tempfile::tempdir().unwrap();
    let mut policy = default_policy();
    policy.max_workers = 0;
    let (doctor, _db) = doctor_over(state.path(), policy).await;

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "configuration_valid").is_some_and(|e| e.contains("max_workers")),
        "expected max_workers to be named: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn stale_workspaces_fails_when_an_active_lease_path_is_gone() {
    use batman_protocol::{IsolationKind, LeaseMode, RunId};
    use batman_runtime::workspace::LeaseService;

    let state = tempfile::tempdir().unwrap();
    let leases = LeaseService::open(ProjectId::new(), &state.path().join("workspace-leases.db"))
        .expect("lease database opens");
    let created = leases
        .acquire(
            RunId::new(),
            LeaseMode::Write,
            Some(IsolationKind::GitWorktree),
        )
        .expect("lease is allocated");
    // Activate against a worktree that was never materialized: exactly the
    // shape a crash between `activate` and cleanup leaves behind.
    leases
        .activate(
            created.lease_id.clone(),
            state.path().join("vanished-worktree").display().to_string(),
        )
        .expect("lease activates");

    let (doctor, _db) = doctor_over(state.path(), default_policy()).await;
    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "stale_workspaces").is_some_and(|e| e.contains(&created.lease_id)),
        "expected the offending lease id to be named: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn stale_workspaces_fails_when_an_allocating_lease_outlives_the_grace_period() {
    use batman_protocol::{IsolationKind, LeaseMode, RunId};
    use batman_runtime::workspace::{ALLOCATING_LEASE_GRACE, LeaseService};

    let state = tempfile::tempdir().unwrap();
    let db_path = state.path().join("workspace-leases.db");
    let leases = LeaseService::open(ProjectId::new(), &db_path).expect("lease database opens");
    let created = leases
        .acquire(
            RunId::new(),
            LeaseMode::Write,
            Some(IsolationKind::GitWorktree),
        )
        .expect("lease is allocated");

    // Exactly the shape a crash between `acquire` and `materialize` leaves
    // behind: the row never reached `activate`, so its path is still empty
    // and the missing-path check above can never see it. Back-date
    // `acquired_at` directly -- `LeaseService` has no API for it on purpose.
    let old =
        (time::OffsetDateTime::now_utc() - ALLOCATING_LEASE_GRACE - time::Duration::minutes(1))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE workspace_leases SET acquired_at = ?1 WHERE lease_id = ?2",
        rusqlite::params![old, created.lease_id],
    )
    .unwrap();
    drop(conn);

    let (doctor, _db) = doctor_over(state.path(), default_policy()).await;
    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "stale_workspaces").is_some_and(|e| e.contains(&created.lease_id)),
        "an allocating lease abandoned before materialization has no path to check, \
         so only the grace-period rule can surface it: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn configuration_valid_fails_on_an_unparseable_retention_period() {
    let state = tempfile::tempdir().unwrap();
    let mut policy = default_policy();
    policy.retention = "forever".to_string();
    let (doctor, _db) = doctor_over(state.path(), policy).await;

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "configuration_valid").is_some_and(|e| e.contains("retention")),
        "expected retention to be named: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn configuration_valid_fails_on_an_uncompilable_org_pattern() {
    let state = tempfile::tempdir().unwrap();
    let mut policy = default_policy();
    policy.org_security_patterns = vec!["([unterminated".to_string()];
    let (doctor, _db) = doctor_over(state.path(), policy).await;

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "configuration_valid")
            .is_some_and(|e| e.contains("org_security_patterns")),
        "expected the offending field to be named: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn state_dir_writable_fails_when_the_directory_is_absent() {
    let state = tempfile::tempdir().unwrap();
    let db = Arc::new(
        DatabaseHandle::start(state.path().join("runtime.db"))
            .await
            .expect("database opens"),
    );
    let doctor = Doctor::new(
        Some(db),
        Some(state.path().join("no-such-subdirectory")),
        Some(default_policy()),
    );

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "state_dir_writable").is_some_and(|e| e.contains("does not exist")),
        "expected a missing-directory failure: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn socket_permissions_fails_when_the_socket_path_is_not_a_socket() {
    let state = tempfile::tempdir().unwrap();
    // A regular file where the socket belongs: something else owns the
    // path, which is exactly the takeover this check exists to catch.
    std::fs::write(state.path().join("runtime.sock"), b"not a socket").unwrap();
    let (doctor, _db) = doctor_over(state.path(), default_policy()).await;

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "socket_permissions").is_some_and(|e| e.contains("not a socket")),
        "expected a non-socket failure: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn schema_compatibility_fails_when_the_committed_schema_drifts() {
    let state = tempfile::tempdir().unwrap();
    // A repo root whose schema file is deliberately wrong.
    let fake_repo = tempfile::tempdir().unwrap();
    let schema_dir = fake_repo.path().join("packages/protocol-ts/schema");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(schema_dir.join("crew.schema.json"), b"{}\n").unwrap();

    let db = Arc::new(
        DatabaseHandle::start(state.path().join("runtime.db"))
            .await
            .expect("database opens"),
    );
    let doctor = Doctor::new(
        Some(db),
        Some(state.path().to_path_buf()),
        Some(default_policy()),
    )
    .with_runtime_context(
        state.path().join("runtime.sock"),
        fake_repo.path().to_path_buf(),
        ProjectId::new(),
    );

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "schema_compatibility").is_some_and(|e| e.contains("stale")),
        "expected a staleness failure: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn schema_compatibility_passes_when_repo_has_no_schema_file() {
    let state = tempfile::tempdir().unwrap();
    // `--repo` is an ordinary project with no `packages/protocol-ts/schema/`
    // at all -- the common case, since `--repo` is the project Crew runs
    // against, not a checkout of Crew's own source.
    let ordinary_repo = tempfile::tempdir().unwrap();

    let db = Arc::new(
        DatabaseHandle::start(state.path().join("runtime.db"))
            .await
            .expect("database opens"),
    );
    let doctor = Doctor::new(
        Some(db),
        Some(state.path().to_path_buf()),
        Some(default_policy()),
    )
    .with_runtime_context(
        state.path().join("runtime.sock"),
        ordinary_repo.path().to_path_buf(),
        ProjectId::new(),
    );

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        result
            .passed_checks
            .iter()
            .any(|name| name == "schema_compatibility"),
        "a repo with no schema document is not applicable, not broken: {:?}",
        result.failed_checks
    );
}

#[tokio::test]
async fn schema_compatibility_passes_against_the_committed_schema() {
    let state = tempfile::tempdir().unwrap();
    let (doctor, _db) = doctor_over(state.path(), default_policy()).await;

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        result
            .passed_checks
            .iter()
            .any(|name| name == "schema_compatibility"),
        "the committed schema must match this binary: {:?}",
        result.failed_checks
    );
}

/// The catalog's `display_available` check used to construct
/// `TerminalDisplay`, which hardcodes `is_available() -> true`, into the
/// same "any backend available" `.any()` -- so the check could never fail.
/// Forcing a specific backend that isn't actually usable (no tmux, no
/// session) must surface as a failure naming that backend, not be masked
/// by the terminal fallback.
#[tokio::test]
async fn display_available_fails_when_the_forced_backend_is_unavailable() {
    let state = tempfile::tempdir().unwrap();
    let mut policy = default_policy();
    policy.display_backend = "tmux".to_string();
    let (doctor, _db) = doctor_over(state.path(), policy).await;

    let result = doctor.check().await.expect("catalog runs");

    assert!(
        error_for(&result, "display_available").is_some_and(|e| e.contains("tmux")),
        "expected the forced-but-unavailable backend to be named: {:?}",
        result.failed_checks
    );
}

/// 14b's regression: `doctor` used to hand `--repo` to the config loader,
/// which read a directory as a file and failed before running any check.
/// With no config flags the default policy must merge and the catalog must
/// run.
#[test]
fn doctor_runs_the_catalog_without_any_config_flags() {
    let fixture = Fixture::new();
    let output = fixture.doctor(true).output().expect("doctor runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout).expect("JSON output");

    assert!(
        json.get("error").is_none(),
        "no config flags must not be a load error: {stdout}"
    );
    let passed = json
        .get("passed_checks")
        .and_then(Value::as_array)
        .expect("a full result, not an abort envelope");
    assert!(
        passed
            .iter()
            .any(|v| v.as_str() == Some("configuration_valid")),
        "the default policy is valid: {stdout}"
    );
}
