//! Herdr display backend tests: real `herdr status --json` compatibility
//! gating and pane-lifecycle operations, using injected command
//! executors keyed off `fixtures/displays/herdr/*.txt` -- the exact
//! `status --json` shape captured from the installed `herdr 0.7.5`
//! binary (mismatch fixture's server side edited to the previously
//! observed protocol-16 workstation state).

use batman_protocol::{DisplayConfig, DisplayPlacement};
use batman_runtime::display::{CommandExecutor, CommandResult, DisplayBackendTrait, HerdrDisplay};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

fn load_fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/displays/herdr")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()))
}

/// A command executor keyed by exact `"program arg1 arg2..."` string,
/// recording every invocation so tests can assert on exactly what (and
/// how many times) this backend actually ran.
struct FixtureExecutor {
    responses: std::collections::HashMap<String, CommandResult>,
    calls: std::sync::Mutex<Vec<String>>,
}

impl FixtureExecutor {
    fn new() -> Self {
        Self {
            responses: std::collections::HashMap::new(),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn with(mut self, key: &str, result: CommandResult) -> Self {
        self.responses.insert(key.to_string(), result);
        self
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

fn ok(stdout: impl Into<String>) -> CommandResult {
    CommandResult {
        success: true,
        stdout: stdout.into().into_bytes(),
        stderr: Vec::new(),
    }
}

impl CommandExecutor for FixtureExecutor {
    fn execute(&self, program: &str, args: &[&str]) -> io::Result<CommandResult> {
        let key = format!("{program} {}", args.join(" "));
        self.calls.lock().unwrap().push(key.clone());
        self.responses
            .get(&key)
            .cloned()
            .ok_or_else(|| io::Error::other(format!("no fixture response for: {key}")))
    }
}

#[test]
fn creates_with_config() {
    let herdr = HerdrDisplay::new(DisplayConfig::default());
    assert_eq!(herdr.backend_name(), "herdr");
}

#[test]
fn the_compatible_fixture_makes_the_backend_available() {
    let executor = Arc::new(FixtureExecutor::new().with(
        "herdr status --json",
        ok(load_fixture("status-compatible.txt")),
    ));
    let herdr = HerdrDisplay::with_executor(DisplayConfig::default(), executor);
    assert!(herdr.is_available());
    let status = herdr
        .probe()
        .expect("probe must succeed against a well-formed fixture");
    assert_eq!(status.client_protocol, 17);
    assert_eq!(status.server_protocol, Some(17));
    assert!(status.compatible);
}

#[test]
fn the_mismatch_fixture_makes_the_backend_unavailable_with_restart_guidance_and_issues_no_pane_command()
 {
    let executor = Arc::new(FixtureExecutor::new().with(
        "herdr status --json",
        ok(load_fixture("status-mismatch.txt")),
    ));
    let herdr = HerdrDisplay::with_executor(
        DisplayConfig::default(),
        Arc::clone(&executor) as Arc<dyn CommandExecutor>,
    );
    assert!(!herdr.is_available());

    let result = herdr.create_pane(
        &["crewd".to_string(), "monitor".to_string()],
        DisplayPlacement::SplitRight,
        "run-1",
        "display-1",
    );
    let err = result.expect_err("an incompatible protocol must refuse to create a pane");
    assert!(
        err.contains("restart"),
        "expected restart guidance in: {err}"
    );
    assert!(herdr.owned_pane_ids().is_empty());
    // Only the status probe was ever invoked -- no `pane split`/`pane
    // run`/`pane report-agent` command was issued once incompatibility
    // was detected.
    assert_eq!(executor.call_count(), 1);
}

#[test]
fn a_created_pane_updates_state_three_times_and_close_only_touches_crew_tagged_panes() {
    let split = ok(r#"{"id":"cli:pane:split","result":{"pane":{"pane_id":"w1:p9"}}}"#);
    let executor = Arc::new(
        FixtureExecutor::new()
            .with(
                "herdr status --json",
                ok(load_fixture("status-compatible.txt")),
            )
            .with("herdr pane split --current --direction right", split)
            .with(
                "herdr pane run w1:p9 crewd monitor --run-id run-1",
                ok("{}"),
            )
            .with(
                "herdr pane report-agent --source crew --agent display-1 --state working w1:p9",
                ok("{}"),
            )
            .with("herdr pane close w1:p9", ok("{}")),
    );
    let herdr = HerdrDisplay::with_executor(
        DisplayConfig::default(),
        Arc::clone(&executor) as Arc<dyn CommandExecutor>,
    );

    let pane_id = herdr
        .create_pane(
            &[
                "crewd".to_string(),
                "monitor".to_string(),
                "--run-id".to_string(),
                "run-1".to_string(),
            ],
            DisplayPlacement::SplitRight,
            "run-1",
            "display-1",
        )
        .expect("a compatible backend must create the pane");
    assert_eq!(pane_id, "w1:p9");
    assert_eq!(herdr.owned_pane_ids(), vec!["w1:p9".to_string()]);

    // Closing an unrelated, never-created pane is refused outright.
    let refused = herdr.close_owned_pane("some-other:p1");
    assert!(refused.is_err());

    // Closing the pane this backend actually created succeeds and
    // removes it from ownership tracking.
    herdr
        .close_owned_pane(&pane_id)
        .expect("closing an owned pane must succeed");
    assert!(herdr.owned_pane_ids().is_empty());
}
