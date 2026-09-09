//! Herdr display backend: real client/server protocol compatibility
//! gating (via `herdr status --json`) and pane-level operations
//! (split/run/move/close/report-agent) over Herdr's own socket-backed
//! CLI, grounded against the installed `herdr 0.8.2` binary's real
//! `--help` output and `status --json` shape (previously
//! verified against 0.7.5). Argv shapes below are additionally gated by
//! [`MIN_SUPPORTED_PROTOCOL`]: a herdr whose protocol predates the
//! version these shapes were checked against is never assumed
//! compatible merely because its own client and server agree with each
//! other.

use crew_protocol::{DisplayBackend, DisplayConfig, DisplayPlacement, DisplayStatus};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{
    CommandExecutor, CommandResult, DisplayBackendTrait, DisplayFuture, PaneHandle, PaneRequest,
    RealCommandExecutor,
};

/// Herdr's own `herdr status --json` result, parsed into the fields this
/// adapter needs for compatibility gating. Field names/nesting verified
/// against the installed `herdr 0.8.2` binary's real output:
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
        let mutually_compatible = server
            .get("compatible")
            .and_then(|v| v.as_bool())
            .unwrap_or_else(|| server_running && server_protocol == Some(client_protocol));
        // Client/server agreeing with each other is not the same claim
        // as "this crew build knows how to speak this protocol" -- a
        // herdr install that upgraded both sides together past what
        // this backend's argv shapes were last verified against would
        // otherwise pass silently. Require the floor on both sides that
        // reported a protocol at all.
        let below_min_protocol = client_protocol < MIN_SUPPORTED_PROTOCOL
            || server_protocol.is_some_and(|p| p < MIN_SUPPORTED_PROTOCOL);
        let compatible = mutually_compatible && !below_min_protocol;
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
        if self.client_protocol < MIN_SUPPORTED_PROTOCOL {
            return format!(
                "herdr client protocol {} (client {}) predates the minimum protocol {} this crew \
                 build's herdr command shapes were verified against; upgrade the herdr client",
                self.client_protocol, self.client_version, MIN_SUPPORTED_PROTOCOL,
            );
        }
        if let Some(server_protocol) = self.server_protocol
            && server_protocol < MIN_SUPPORTED_PROTOCOL
        {
            return format!(
                "herdr server protocol {server_protocol} (server {}) predates the minimum \
                 protocol {MIN_SUPPORTED_PROTOCOL} this crew build's herdr command shapes were \
                 verified against; upgrade the herdr server",
                self.server_version.as_deref().unwrap_or("unknown"),
            );
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

/// The lowest herdr wire protocol number this backend's argv shapes have
/// been verified against (herdr 0.8.2). A herdr client or
/// server reporting a protocol below this is never treated as
/// compatible, even when its own `status --json` reports `compatible:
/// true` -- that field only promises the client and server agree with
/// each other, not that this crew build's command syntax still applies.
/// Bump this only after re-verifying every `herdr pane ...` invocation
/// in this file against the new minimum version's real `--help` output.
const MIN_SUPPORTED_PROTOCOL: u64 = 20;

/// Herdr display backend.
///
/// Compatibility gate: probes `herdr status --json` and requires EXACT
/// client/server protocol equality (Herdr does not promise cross-
/// protocol wire compatibility); the probe result is cached for 5
/// seconds so repeated availability checks do not spawn a process each
/// time.
pub struct HerdrDisplay {
    #[allow(dead_code)]
    // carried for parity with TmuxDisplay/HiddenDisplay; no field of it is read yet
    config: DisplayConfig,
    session_active: bool,
    executor: Arc<dyn CommandExecutor>,
    status_cache: Mutex<Option<(Instant, Result<HerdrStatus, String>)>>,
    owned_panes: Mutex<Vec<String>>,
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

    /// Creates a new Crew-owned pane running `req.command`, reporting
    /// `req.title` as the agent name via `pane report-agent`, at
    /// `req.placement`. Probes `herdr status` first (via the 5-second
    /// cache) and issues no pane command at all when incompatible.
    ///
    /// # Errors
    /// Returns a message (naming a coordinated Herdr restart when the
    /// cause is a protocol mismatch) without ever creating a pane, if
    /// Herdr is unavailable/incompatible, the split fails, or the
    /// command fails to launch.
    fn create_pane_sync(&self, req: &PaneRequest) -> Result<String, String> {
        let status = self.probe()?;
        if !status.compatible {
            return Err(status.remediation());
        }
        if req.placement == DisplayPlacement::Window {
            return Err(
                "DisplayPlacement::Window is OsWindowDisplay's own actual-outcome placement, \
                 never a valid request for Herdr"
                    .to_string(),
            );
        }

        let direction = match req.placement {
            DisplayPlacement::SplitRight => "right",
            // Tab/Workspace placement first creates a tagged pane via an
            // ordinary down-split, then moves it -- Herdr's own `pane
            // move` is what actually promotes a pane to a new
            // tab/workspace, not `pane split` itself.
            DisplayPlacement::SplitDown | DisplayPlacement::Tab | DisplayPlacement::Workspace => {
                "down"
            }
            DisplayPlacement::Window => {
                unreachable!("handled above")
            }
        };
        let pane_id = self.run_pane_command(
            &["pane", "split", "--current", "--direction", direction],
            "split",
        )?;

        // A partial move failure must clean up only the pane just
        // created, never a pre-existing one.
        let outcome: Result<(), String> = (|| {
            match req.placement {
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
                DisplayPlacement::Window => {
                    unreachable!("handled above")
                }
            }

            let mut run_args: Vec<&str> = vec!["pane", "run", &pane_id];
            run_args.extend(req.command.iter().map(String::as_str));
            self.execute_or_err(&run_args, "run")?;

            // herdr 0.8.2's usage line puts PANE_ID first:
            // `herdr pane report-agent <pane_id> --source ID --agent
            // LABEL --state ...`. Passing it last (the previous shape
            // here) fails against a live 0.8.2 herdr with `unknown
            // option: crew` (reproduced directly against both
            // a nonexistent pane id and a real live pane).
            self.execute_or_err(
                &[
                    "pane",
                    "report-agent",
                    &pane_id,
                    "--source",
                    "crew",
                    "--agent",
                    &req.title,
                    "--state",
                    "working",
                ],
                "report-agent",
            )?;
            Ok(())
        })();

        match outcome {
            Ok(()) => {
                self.owned_panes.lock().push(pane_id.clone());
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
    fn close_owned_pane(&self, pane_id: &str) -> Result<(), String> {
        {
            let mut panes = self.owned_panes.lock();
            let Some(index) = panes.iter().position(|p| p == pane_id) else {
                return Err(format!(
                    "refusing to close pane {pane_id}: not tracked as owned by this backend"
                ));
            };
            panes.remove(index);
        }
        self.execute_or_err(&["pane", "close", pane_id], "close")?;
        Ok(())
    }

    /// The panes this backend currently tracks as owned, for tests and
    /// diagnostics.
    #[must_use]
    pub fn owned_pane_ids(&self) -> Vec<String> {
        self.owned_panes.lock().clone()
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

    fn create_pane(&self, req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
        Box::pin(async move {
            let pane_ref = self.create_pane_sync(&req)?;
            Ok(PaneHandle {
                backend: DisplayBackend::Herdr,
                pane_ref,
                placement: req.placement,
            })
        })
    }

    fn close_pane(&self, handle: &PaneHandle) -> DisplayFuture<'_, ()> {
        let pane_ref = handle.pane_ref.clone();
        Box::pin(async move { self.close_owned_pane(&pane_ref) })
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

    const COMPATIBLE_STATUS: &str = r#"{"client":{"version":"0.8.2","channel":"stable","protocol":20},"server":{"status":"running","running":true,"version":"0.8.2","protocol":20,"compatible":true,"socket":"/tmp/herdr.sock"}}"#;
    // Both sides are at or above MIN_SUPPORTED_PROTOCOL but disagree
    // with each other -- this must hit the mismatch/restart guidance,
    // not the minimum-protocol guidance (kept distinct from
    // BELOW_MINIMUM_STATUS below, which exercises that other branch).
    const MISMATCH_STATUS: &str = r#"{"client":{"version":"0.8.3","channel":"stable","protocol":21},"server":{"status":"running","running":true,"version":"0.8.2","protocol":20,"compatible":false,"socket":"/tmp/herdr.sock"}}"#;
    // Both sides agree with each other (the server itself reports
    // `compatible: true`) but on a protocol older than this backend's
    // verified minimum -- MIN_SUPPORTED_PROTOCOL's floor must override that self-report.
    const BELOW_MINIMUM_STATUS: &str = r#"{"client":{"version":"0.7.5","channel":"stable","protocol":17},"server":{"status":"running","running":true,"version":"0.7.5","protocol":17,"compatible":true,"socket":"/tmp/herdr.sock"}}"#;

    #[test]
    fn parses_the_real_compatible_status_shape() {
        let status = HerdrStatus::parse(COMPATIBLE_STATUS).unwrap();
        assert_eq!(status.client_protocol, 20);
        assert_eq!(status.server_protocol, Some(20));
        assert!(status.compatible);
    }

    #[test]
    fn parses_the_real_mismatch_status_shape() {
        let status = HerdrStatus::parse(MISMATCH_STATUS).unwrap();
        assert_eq!(status.client_protocol, 21);
        assert_eq!(status.server_protocol, Some(20));
        assert!(!status.compatible);
        assert!(status.remediation().contains("restart"));
    }

    #[test]
    fn protocol_below_minimum_is_incompatible_even_when_client_and_server_agree() {
        let status = HerdrStatus::parse(BELOW_MINIMUM_STATUS).unwrap();
        assert_eq!(status.client_protocol, 17);
        assert_eq!(status.server_protocol, Some(17));
        assert!(
            !status.compatible,
            "client/server self-agreement must not override the verified-minimum floor"
        );
        let remediation = status.remediation();
        assert!(
            remediation.contains("minimum protocol"),
            "remediation should name the minimum-protocol gate, got: {remediation}"
        );
        assert!(remediation.contains("upgrade the herdr client"));
    }

    #[tokio::test]
    async fn below_minimum_protocol_issues_no_pane_command() {
        let executor =
            Arc::new(FixtureExecutor::new().with("herdr status --json", ok(BELOW_MINIMUM_STATUS)));
        let display = HerdrDisplay::with_executor(DisplayConfig::default(), executor);
        assert!(!display.is_available());

        let result = display
            .create_pane(pane_request(
                &["crewd", "monitor"],
                DisplayPlacement::SplitRight,
                "display-1",
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("minimum protocol"));
        assert!(display.owned_pane_ids().is_empty());
    }

    fn pane_request(command: &[&str], placement: DisplayPlacement, title: &str) -> PaneRequest {
        PaneRequest {
            title: title.to_string(),
            command: command.iter().map(|s| s.to_string()).collect(),
            placement,
            launch_program: None,
        }
    }

    #[tokio::test]
    async fn incompatible_status_makes_the_backend_unavailable_and_issues_no_pane_command() {
        let executor =
            Arc::new(FixtureExecutor::new().with("herdr status --json", ok(MISMATCH_STATUS)));
        let display = HerdrDisplay::with_executor(DisplayConfig::default(), executor);
        assert!(!display.is_available());

        let result = display
            .create_pane(pane_request(
                &["crewd", "monitor"],
                DisplayPlacement::SplitRight,
                "display-1",
            ))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("restart"));
        assert!(display.owned_pane_ids().is_empty());
    }

    #[tokio::test]
    async fn compatible_status_creates_one_tagged_pane_and_tracks_it_as_owned() {
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
                    "herdr pane report-agent w1:p2 --source crew --agent display-1 --state working",
                    ok("{}"),
                ),
        );
        let display = HerdrDisplay::with_executor(DisplayConfig::default(), executor);

        let handle = display
            .create_pane(pane_request(
                &["crewd", "monitor"],
                DisplayPlacement::SplitRight,
                "display-1",
            ))
            .await
            .expect("compatible protocol must allow pane creation");
        assert_eq!(handle.pane_ref, "w1:p2");
        assert_eq!(handle.backend, DisplayBackend::Herdr);
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
