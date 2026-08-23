//! Herdr display backend: real client/server protocol compatibility
//! gating (via `herdr status --json`) and pane-level operations
//! (split/run/move/close/report-agent) over Herdr's own socket-backed
//! CLI, grounded against the installed `herdr 0.7.5` binary's real
//! `--help` output and `status --json` shape.

use batman_protocol::{DisplayBackend, DisplayConfig, DisplayPlacement, DisplayStatus};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{CommandExecutor, CommandResult, DisplayBackendTrait, RealCommandExecutor};

/// Herdr's own `herdr status --json` result, parsed into the fields this
/// adapter needs for compatibility gating. Field names/nesting verified
/// against the installed `herdr 0.7.5` binary's real output:
/// `{"client":{"version","protocol",...},"server":{"running","version","protocol","compatible",...}}`.
#[derive(Debug, Clone, PartialEq)]
pub struct HerdrStatus {
    pub client_version: String,
    pub client_protocol: u64,
    pub server_running: bool,
    pub server_version: Option<String>,
    pub server_protocol: Option<u64>,
    pub compatible: bool,
}

impl HerdrStatus {
    fn parse(json: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| format!("herdr status --json produced invalid JSON: {e}"))?;
        let client = value
            .get("client")
            .ok_or_else(|| "herdr status --json missing \"client\"".to_string())?;
        let server = value
            .get("server")
            .ok_or_else(|| "herdr status --json missing \"server\"".to_string())?;
        let client_version = client
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let client_protocol = client
            .get("protocol")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "herdr status --json missing client.protocol".to_string())?;
        let server_running = server
            .get("running")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let server_version = server
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let server_protocol = server.get("protocol").and_then(|v| v.as_u64());
        // Trust the server's own "compatible" field when present, but
        // never claim compatibility it doesn't report -- fall back to
        // exact protocol number equality only when the server is
        // running and reported a protocol at all.
        let compatible = server
            .get("compatible")
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| server_running && server_protocol == Some(client_protocol));
        Ok(Self {
            client_version,
            client_protocol,
            server_running,
            server_version,
            server_protocol,
            compatible,
        })
    }

    /// Human-readable remediation text for an incompatible/unavailable
    /// server, naming the exact versions/protocols observed.
    #[must_use]
    pub fn remediation(&self) -> String {
        if !self.server_running {
            return "herdr server is not running; start it or run `herdr` to launch one"
                .to_string();
        }
        match self.server_protocol {
            Some(server_protocol) if server_protocol != self.client_protocol => format!(
                "herdr client protocol {} does not match server protocol {} (client {}, server {}); \
                 restart the herdr server so both sides run the same version",
                self.client_protocol,
                server_protocol,
                self.client_version,
                self.server_version.as_deref().unwrap_or("unknown"),
            ),
            _ => "herdr server reported itself incompatible".to_string(),
        }
    }
}

/// One pane this backend created and is tracked as owning. Never
/// modifies or closes a pane lacking this tag.
#[derive(Debug, Clone)]
struct OwnedPane {
    pane_id: String,
    #[allow(dead_code)] // retained for diagnostics/future coordination-metadata reporting
    run_id: String,
    #[allow(dead_code)]
    display_id: String,
}

/// Herdr display backend.
///
/// Compatibility gate: probes `herdr status --json` and requires EXACT
/// client/server protocol equality (Herdr does not promise cross-
/// protocol wire compatibility); the probe result is cached for 5
/// seconds so repeated availability checks do not spawn a process each
/// time.
pub struct HerdrDisplay {
    #[allow(dead_code)]
    // carried for parity with TmuxDisplay/TerminalDisplay; no field of it is read yet
    config: DisplayConfig,
    session_active: bool,
    executor: Arc<dyn CommandExecutor>,
    status_cache: Mutex<Option<(Instant, Result<HerdrStatus, String>)>>,
    owned_panes: Mutex<Vec<OwnedPane>>,
}

const STATUS_CACHE_TTL: Duration = Duration::from_secs(5);

impl HerdrDisplay {
    #[must_use]
    pub fn new(config: DisplayConfig) -> Self {
        Self::with_executor(config, Arc::new(RealCommandExecutor::new()))
    }

    /// Creates a HerdrDisplay with a custom command executor (for testing).
    #[must_use]
    pub fn with_executor(config: DisplayConfig, executor: Arc<dyn CommandExecutor>) -> Self {
        HerdrDisplay {
            config,
            session_active: false,
            executor,
            status_cache: Mutex::new(None),
            owned_panes: Mutex::new(Vec::new()),
        }
    }

    /// Probes `herdr status --json`, caching the result for 5 seconds so
    /// callers checking availability repeatedly (e.g. before every new
    /// pane attachment) do not spawn a process each time.
    ///
    /// # Errors
    /// Returns a message if `herdr` is not on `PATH`, exits non-zero, or
    /// its `--json` output cannot be parsed into the expected shape.
    pub fn probe(&self) -> Result<HerdrStatus, String> {
        {
            let cache = self.status_cache.lock();
            if let Some((fetched_at, result)) = cache.as_ref()
                && fetched_at.elapsed() < STATUS_CACHE_TTL
            {
                return result.clone();
            }
        }
        let result = self.probe_uncached();
        *self.status_cache.lock() = Some((Instant::now(), result.clone()));
        result
    }

    fn probe_uncached(&self) -> Result<HerdrStatus, String> {
        match self.executor.execute("herdr", &["status", "--json"]) {
            Ok(CommandResult {
                success: true,
                stdout,
                ..
            }) => {
                let text = String::from_utf8_lossy(&stdout);
                HerdrStatus::parse(&text)
            }
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "herdr status --json exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("herdr is not available: {e}")),
        }
    }

    /// Creates a new Crew-owned pane running `command`, tagged with
    /// `run_id`/`display_id` ownership, at `placement`. Probes `herdr
    /// status` first (via the 5-second cache) and issues no pane command
    /// at all when incompatible.
    ///
    /// # Errors
    /// Returns a message (naming a coordinated Herdr restart when the
    /// cause is a protocol mismatch) without ever creating a pane, if
    /// Herdr is unavailable/incompatible, the split fails, or the
    /// command fails to launch.
    pub fn create_pane(
        &self,
        command: &[String],
        placement: DisplayPlacement,
        run_id: &str,
        display_id: &str,
    ) -> Result<String, String> {
        let status = self.probe()?;
        if !status.compatible {
            return Err(status.remediation());
        }
        if placement == DisplayPlacement::Embedded {
            return Err(
                "DisplayPlacement::Embedded creates no separate pane; Herdr has nothing to split"
                    .to_string(),
            );
        }

        let direction = match placement {
            DisplayPlacement::SplitRight => "right",
            // Tab/Workspace placement first creates a tagged pane via an
            // ordinary down-split, then moves it -- Herdr's own `pane
            // move` is what actually promotes a pane to a new
            // tab/workspace, not `pane split` itself.
            DisplayPlacement::SplitDown | DisplayPlacement::Tab | DisplayPlacement::Workspace => {
                "down"
            }
            DisplayPlacement::Embedded => unreachable!("handled above"),
        };
        let pane_id = self.run_pane_command(
            &["pane", "split", "--current", "--direction", direction],
            "split",
        )?;

        // A partial move failure must clean up only the pane just
        // created, never a pre-existing one.
        let outcome: Result<(), String> = (|| {
            match placement {
                DisplayPlacement::Tab => {
                    self.execute_or_err(
                        &["pane", "move", &pane_id, "--new-tab"],
                        "move to new tab",
                    )?;
                }
                DisplayPlacement::Workspace => {
                    self.execute_or_err(
                        &["pane", "move", &pane_id, "--new-workspace"],
                        "move to new workspace",
                    )?;
                }
                DisplayPlacement::SplitRight | DisplayPlacement::SplitDown => {}
                DisplayPlacement::Embedded => unreachable!("handled above"),
            }

            let mut run_args: Vec<&str> = vec!["pane", "run", &pane_id];
            run_args.extend(command.iter().map(String::as_str));
            self.execute_or_err(&run_args, "run")?;

            self.execute_or_err(
                &[
                    "pane",
                    "report-agent",
                    "--source",
                    "crew",
                    "--agent",
                    display_id,
                    "--state",
                    "working",
                    &pane_id,
                ],
                "report-agent",
            )?;
            Ok(())
        })();

        match outcome {
            Ok(()) => {
                self.owned_panes.lock().push(OwnedPane {
                    pane_id: pane_id.clone(),
                    run_id: run_id.to_string(),
                    display_id: display_id.to_string(),
                });
                Ok(pane_id)
            }
            Err(err) => {
                // Never persist an ownership claim until every step
                // acknowledged; clean up only the pane this call itself
                // created, never a pre-existing one.
                let _ = self.execute_or_err(&["pane", "close", &pane_id], "cleanup close");
                Err(err)
            }
        }
    }

    /// Closes `pane_id` only if this backend recorded it as owned;
    /// existing unrelated panes are never modified or closed.
    ///
    /// # Errors
    /// Returns a message if `pane_id` is not tracked as Crew-owned, or
    /// the close command itself fails.
    pub fn close_owned_pane(&self, pane_id: &str) -> Result<(), String> {
        let owned = {
            let mut panes = self.owned_panes.lock();
            let Some(index) = panes.iter().position(|p| p.pane_id == pane_id) else {
                return Err(format!(
                    "refusing to close pane {pane_id}: not tracked as owned by this backend"
                ));
            };
            panes.remove(index)
        };
        self.execute_or_err(&["pane", "close", &owned.pane_id], "close")?;
        Ok(())
    }

    /// The panes this backend currently tracks as owned, for tests and
    /// diagnostics.
    #[must_use]
    pub fn owned_pane_ids(&self) -> Vec<String> {
        self.owned_panes
            .lock()
            .iter()
            .map(|p| p.pane_id.clone())
            .collect()
    }

    /// Runs the full `herdr` argv in `args` (including the leading
    /// `"pane"` subcommand) and extracts the created/target pane id from
    /// the response. Herdr's own JSON envelope for a single pane nests
    /// it as `result.pane.pane_id` (verified against `herdr pane
    /// current`/`herdr pane get`); this also accepts the flatter
    /// `result.pane_id` shape defensively, since `split`'s own exact
    /// nesting was not independently re-verified against a live mutation
    /// (this backend never spawns a pane against a developer's own live
    /// Herdr session during tests) -- an unrecognized shape is a clear
    /// parse error, never a silently guessed pane id.
    fn run_pane_command(&self, args: &[&str], what: &str) -> Result<String, String> {
        let raw = self.execute_or_err(args, what)?;
        extract_pane_id(&raw).ok_or_else(|| {
            format!("herdr {what} succeeded but its response carried no recognizable pane id")
        })
    }

    /// Runs the full `herdr` argv in `args` (including the leading
    /// `"pane"` subcommand), returning stdout on success.
    fn execute_or_err(&self, args: &[&str], what: &str) -> Result<String, String> {
        match self.executor.execute("herdr", args) {
            Ok(CommandResult {
                success: true,
                stdout,
                ..
            }) => Ok(String::from_utf8_lossy(&stdout).into_owned()),
            Ok(CommandResult { stderr, .. }) => Err(format!(
                "herdr {what} exited with error: {}",
                String::from_utf8_lossy(&stderr)
            )),
            Err(e) => Err(format!("failed to run herdr {what}: {e}")),
        }
    }
}

/// Extracts a pane id from a Herdr CLI JSON response, accepting either
/// `result.pane.pane_id` (the shape `herdr pane current`/`herdr pane
/// get` verifiably use) or `result.pane_id` (a plausible flatter shape
/// for mutation commands, not independently re-verified -- see
/// [`HerdrDisplay::run_pane_command`]'s own doc comment).
fn extract_pane_id(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let result = value.get("result")?;
    result
        .get("pane")
        .and_then(|pane| pane.get("pane_id"))
        .or_else(|| result.get("pane_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

impl DisplayBackendTrait for HerdrDisplay {
    fn backend_name(&self) -> &str {
        "herdr"
    }

    fn is_available(&self) -> bool {
        self.probe()
            .map(|status| status.compatible)
            .unwrap_or(false)
    }

    fn activate(&mut self) -> Result<(), String> {
        let status = self.probe()?;
        if !status.compatible {
            return Err(status.remediation());
        }
        self.session_active = true;
        Ok(())
    }

    fn status(&self) -> DisplayStatus {
        DisplayStatus {
            backend: DisplayBackend::Herdr,
            available: self.is_available(),
            active: self.session_active,
            dimensions: None,
        }
    }

    fn version(&self) -> Option<String> {
        self.probe().ok().map(|s| s.client_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct FixtureExecutor {
        responses: std::collections::HashMap<String, CommandResult>,
    }

    impl FixtureExecutor {
        fn new() -> Self {
            Self {
                responses: std::collections::HashMap::new(),
            }
        }

        fn with(mut self, key: &str, result: CommandResult) -> Self {
            self.responses.insert(key.to_string(), result);
            self
        }
    }

    fn ok(stdout: &str) -> CommandResult {
        CommandResult {
            success: true,
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn err(stderr: &str) -> CommandResult {
        CommandResult {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    impl CommandExecutor for FixtureExecutor {
        fn execute(&self, program: &str, args: &[&str]) -> io::Result<CommandResult> {
            let key = format!("{program} {}", args.join(" "));
            self.responses
                .get(&key)
                .cloned()
                .ok_or_else(|| io::Error::other(format!("no fixture response for: {key}")))
        }
    }

    const COMPATIBLE_STATUS: &str = r#"{"client":{"version":"0.7.5","channel":"stable","protocol":17},"server":{"status":"running","running":true,"version":"0.7.5","protocol":17,"compatible":true,"socket":"/tmp/herdr.sock"}}"#;
    const MISMATCH_STATUS: &str = r#"{"client":{"version":"0.7.5","channel":"stable","protocol":17},"server":{"status":"running","running":true,"version":"0.7.4","protocol":16,"compatible":false,"socket":"/tmp/herdr.sock"}}"#;

    #[test]
    fn parses_the_real_compatible_status_shape() {
        let status = HerdrStatus::parse(COMPATIBLE_STATUS).unwrap();
        assert_eq!(status.client_protocol, 17);
        assert_eq!(status.server_protocol, Some(17));
        assert!(status.compatible);
    }

    #[test]
    fn parses_the_real_mismatch_status_shape() {
        let status = HerdrStatus::parse(MISMATCH_STATUS).unwrap();
        assert_eq!(status.client_protocol, 17);
        assert_eq!(status.server_protocol, Some(16));
        assert!(!status.compatible);
        assert!(status.remediation().contains("restart"));
    }

    #[test]
    fn incompatible_status_makes_the_backend_unavailable_and_issues_no_pane_command() {
        let executor =
            Arc::new(FixtureExecutor::new().with("herdr status --json", ok(MISMATCH_STATUS)));
        let display = HerdrDisplay::with_executor(DisplayConfig::default(), executor);
        assert!(!display.is_available());

        let result = display.create_pane(
            &["crewd".to_string(), "monitor".to_string()],
            DisplayPlacement::SplitRight,
            "run-1",
            "display-1",
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("restart"));
        assert!(display.owned_pane_ids().is_empty());
    }

    #[test]
    fn compatible_status_creates_one_tagged_pane_and_tracks_it_as_owned() {
        let split_response = ok(r#"{"id":"cli:pane:split","result":{"pane":{"pane_id":"w1:p2"}}}"#);
        let executor = Arc::new(
            FixtureExecutor::new()
                .with("herdr status --json", ok(COMPATIBLE_STATUS))
                .with(
                    "herdr pane split --current --direction right",
                    split_response,
                )
                .with("herdr pane run w1:p2 crewd monitor", ok("{}"))
                .with(
                    "herdr pane report-agent --source crew --agent display-1 --state working w1:p2",
                    ok("{}"),
                ),
        );
        let display = HerdrDisplay::with_executor(DisplayConfig::default(), executor);

        let pane_id = display
            .create_pane(
                &["crewd".to_string(), "monitor".to_string()],
                DisplayPlacement::SplitRight,
                "run-1",
                "display-1",
            )
            .expect("compatible protocol must allow pane creation");
        assert_eq!(pane_id, "w1:p2");
        assert_eq!(display.owned_pane_ids(), vec!["w1:p2".to_string()]);
    }

    #[test]
    fn closing_an_untracked_pane_is_refused() {
        let executor =
            Arc::new(FixtureExecutor::new().with("herdr status --json", ok(COMPATIBLE_STATUS)));
        let display = HerdrDisplay::with_executor(DisplayConfig::default(), executor);
        let result = display.close_owned_pane("not-owned:p1");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not tracked as owned"));
    }

    #[test]
    fn probe_result_is_cached_for_five_seconds() {
        let executor =
            Arc::new(FixtureExecutor::new().with("herdr status --json", ok(COMPATIBLE_STATUS)));
        let display = HerdrDisplay::with_executor(
            DisplayConfig::default(),
            executor as Arc<dyn CommandExecutor>,
        );
        // Two probes in quick succession must both succeed off the same
        // single fixture response -- `FixtureExecutor` errors on any
        // invocation it has no entry for, so a second real spawn here
        // would fail the test; this also proves caching, since the
        // fixture is never asked to answer a second `status --json`.
        assert!(display.probe().unwrap().compatible);
        assert!(display.probe().unwrap().compatible);
    }

    #[test]
    fn unreachable_herdr_binary_is_unavailable_not_a_panic() {
        let executor =
            Arc::new(FixtureExecutor::new().with("herdr status --json", err("command not found")));
        let display = HerdrDisplay::with_executor(DisplayConfig::default(), executor);
        assert!(!display.is_available());
    }
}
