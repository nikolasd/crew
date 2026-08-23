//! Runtime health checking and rollout gate management.
//!
//! The [`Doctor`] provides a comprehensive health check of the Crew runtime,
//! including:
//! - Database connectivity
//! - State directory accessibility
//! - Rollout gate status
//! - Adapter availability
//! - Configuration validity
//!
//! This is used by the `status` CLI command and can also be triggered manually
//! for diagnostics.

use std::sync::Arc;

use batman_protocol::ProjectId;
use serde::Serialize;
use thiserror::Error;

use crate::adapter::AdapterKind;
use crate::config::RuntimePolicy;
use crate::db::DatabaseHandle;

/// Errors that can occur during a doctor check.
#[derive(Debug, Error)]
pub enum DoctorError {
    /// The database is not accessible.
    #[error("database is not accessible: {0}")]
    DatabaseError(String),

    /// The state directory is not accessible.
    #[error("state directory is not accessible: {0}")]
    StateDirError(String),

    /// A rollout gate is unresolved.
    #[error("rollout gate '{gate}' is unresolved")]
    RolloutGateUnresolved { gate: String },

    /// A configuration error was detected.
    #[error("configuration error: {0}")]
    ConfigError(String),

    /// An adapter is not available.
    #[error("adapter '{adapter}' is not available: {reason}")]
    AdapterUnavailable { adapter: String, reason: String },
}

/// Result of a doctor check.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorResult {
    /// Whether the runtime is healthy.
    pub healthy: bool,

    /// The set of checks that passed.
    pub passed_checks: Vec<String>,

    /// The set of checks that failed, with error messages.
    pub failed_checks: Vec<FailedCheck>,

    /// The set of unresolved rollout gates.
    pub unresolved_gates: Vec<String>,
}

/// A single failed check.
#[derive(Debug, Clone, Serialize)]
pub struct FailedCheck {
    /// The name of the check.
    pub check_name: String,

    /// The error message.
    pub error: String,
}

/// Performs health checks on the Crew runtime.
///
/// Every check reports either a pass or a [`FailedCheck`]. A check that
/// cannot run reports a [`FailedCheck`] whose error starts with `skipped:`
/// -- reporting an unrun check as passing would make `healthy` a lie.
pub struct Doctor {
    db: Option<Arc<DatabaseHandle>>,
    state_dir: Option<std::path::PathBuf>,
    policy: Option<RuntimePolicy>,
    /// The runtime socket, checked for ownership and mode when present.
    socket_path: Option<std::path::PathBuf>,
    /// The repository root, used to locate the committed schema document.
    repo_root: Option<std::path::PathBuf>,
    /// The project whose leases and runs are inspected.
    project_id: Option<ProjectId>,
}

/// The target triples the foundation ships prebuilt `crewd` leaves for.
/// Mirrors `crates/xtask/src/main.rs`'s `SUPPORTED_TARGETS`; the two must
/// be changed together.
const SUPPORTED_TARGETS: &[(&str, &str)] = &[
    ("macos", "aarch64"),
    ("macos", "x86_64"),
    ("linux", "aarch64"),
    ("linux", "x86_64"),
];

/// The free space `state_dir`'s filesystem must have for the runtime to be
/// considered healthy: enough for a worktree plus a journal's growth.
const MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;

impl Doctor {
    /// Creates a [`Doctor`] with the given database handle, state
    /// directory, and runtime policy. Checks needing more context are
    /// enabled by [`Self::with_runtime_context`]; without it they report
    /// `skipped:` rather than silently passing.
    #[must_use]
    pub fn new(
        db: Option<Arc<DatabaseHandle>>,
        state_dir: Option<std::path::PathBuf>,
        policy: Option<RuntimePolicy>,
    ) -> Self {
        Self {
            db,
            state_dir,
            policy,
            socket_path: None,
            repo_root: None,
            project_id: None,
        }
    }

    /// Supplies the socket path, repository root, and project id that the
    /// socket, schema, lease, and run checks need.
    #[must_use]
    pub fn with_runtime_context(
        mut self,
        socket_path: std::path::PathBuf,
        repo_root: std::path::PathBuf,
        project_id: ProjectId,
    ) -> Self {
        self.socket_path = Some(socket_path);
        self.repo_root = Some(repo_root);
        self.project_id = Some(project_id);
        self
    }

    /// Creates a [`Doctor`] with no inputs at all. Every check then
    /// reports `skipped:`, so the result is unhealthy -- which is correct:
    /// nothing was verified.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(None, None, None)
    }

    /// Runs the full check catalog.
    ///
    /// # Errors
    /// Returns a [`DoctorError`] only if the catalog itself cannot be run.
    /// An individual check's failure lands in
    /// [`DoctorResult::failed_checks`], not here.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use batman_runtime::db::DatabaseHandle;
    /// # use batman_runtime::doctor::Doctor;
    /// # async fn example(db: Arc<DatabaseHandle>) -> Result<(), Box<dyn std::error::Error>> {
    /// let doctor = Doctor::new(Some(db), None, None);
    /// let result = doctor.check().await?;
    /// if !result.healthy {
    ///     println!("Runtime has issues: {:?}", result.failed_checks);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn check(&self) -> Result<DoctorResult, DoctorError> {
        let mut report = Report::default();

        report.record("database_connectivity", self.check_database().await);
        report.record("configuration_valid", self.check_configuration());
        report.record("state_dir_writable", self.check_state_dir());
        report.record("platform_supported", Self::check_platform());
        report.record("binary_integrity", Self::check_binary_integrity());
        report.record("socket_permissions", self.check_socket_permissions());
        report.record("schema_compatibility", self.check_schema_compatibility());
        for kind in [
            AdapterKind::Claude,
            AdapterKind::Codex,
            AdapterKind::Copilot,
            AdapterKind::OmpRpc,
        ] {
            let name = format!("adapter_{}_available", kind.wire_name());
            report.record(&name, Self::check_adapter(kind).await);
        }
        report.record("display_available", self.check_display());
        report.record("disk_space", self.check_disk_space());
        report.record("stale_workspaces", self.check_stale_workspaces());
        report.record("stale_runs", self.check_stale_runs().await);

        // Advisory: unresolved gates are listed, and each is also a failed
        // check so `healthy` reflects them.
        let unresolved_gates = match &self.policy {
            Some(policy) => policy
                .unresolved_gates()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            None => Vec::new(),
        };
        match &self.policy {
            None => report.record(
                "rollout_gates_resolved",
                Err(DoctorError::ConfigError(
                    "skipped: no runtime policy was supplied".to_string(),
                )),
            ),
            Some(_) if unresolved_gates.is_empty() => {
                report.record("rollout_gates_resolved", Ok(()));
            }
            Some(_) => {
                for gate in &unresolved_gates {
                    // `native_discovery_reviewed` is enforced at
                    // authorization time, so an unresolved value denies
                    // runs rather than merely warning.
                    let consequence = if gate == "native_discovery_reviewed" {
                        "blocks authorization until resolved"
                    } else {
                        "advisory"
                    };
                    report.record(
                        &format!("rollout_gate_{gate}"),
                        Err(DoctorError::RolloutGateUnresolved {
                            gate: format!("{gate} ({consequence})"),
                        }),
                    );
                }
            }
        }

        let Report {
            passed_checks,
            failed_checks,
        } = report;

        Ok(DoctorResult {
            healthy: failed_checks.is_empty(),
            passed_checks,
            failed_checks,
            unresolved_gates,
        })
    }

    /// Executes a trivial query against the journal. A handle that opened
    /// but cannot answer a query is not connectivity.
    async fn check_database(&self) -> Result<(), DoctorError> {
        let Some(db) = &self.db else {
            return Err(DoctorError::DatabaseError(
                "skipped: no database handle was supplied".to_string(),
            ));
        };
        let op: crate::db::DomainClosure = Box::new(move |conn| {
            let count: i64 = conn.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?;
            Ok(serde_json::json!({ "events": count }))
        });
        db.run_domain_op(op)
            .await
            .map(|_| ())
            .map_err(|e| DoctorError::DatabaseError(e.to_string()))
    }

    /// Asserts the merged policy's numeric bounds, retention period, and
    /// org redaction patterns are all usable.
    fn check_configuration(&self) -> Result<(), DoctorError> {
        let Some(policy) = &self.policy else {
            return Err(DoctorError::ConfigError(
                "skipped: no runtime policy was supplied".to_string(),
            ));
        };
        if policy.max_workers == 0 {
            return Err(DoctorError::ConfigError("max_workers is 0".to_string()));
        }
        if policy.concurrency_ceiling == 0 {
            return Err(DoctorError::ConfigError(
                "concurrency_ceiling is 0".to_string(),
            ));
        }
        crate::audit::retention::parse_period(&policy.retention)
            .map_err(|e| DoctorError::ConfigError(format!("retention: {e}")))?;
        for pattern in &policy.org_security_patterns {
            regex::Regex::new(pattern).map_err(|e| {
                DoctorError::ConfigError(format!("org_security_patterns entry {pattern:?}: {e}"))
            })?;
        }
        Ok(())
    }

    /// Asserts the state directory exists, is private (`0700`, owned by
    /// this uid), and accepts a write.
    fn check_state_dir(&self) -> Result<(), DoctorError> {
        let Some(state_dir) = &self.state_dir else {
            return Err(DoctorError::StateDirError(
                "skipped: no state directory was supplied".to_string(),
            ));
        };
        if !state_dir.exists() {
            return Err(DoctorError::StateDirError(format!(
                "state directory does not exist: {}",
                state_dir.display()
            )));
        }
        // `ensure_private_dir` creates-or-validates: on an existing
        // directory it is exactly the ownership and mode assertion.
        crate::security::ensure_private_dir(state_dir)
            .map_err(|e| DoctorError::StateDirError(e.to_string()))?;

        let probe = state_dir.join(".doctor-write-probe");
        std::fs::write(&probe, b"probe").map_err(|e| {
            DoctorError::StateDirError(format!("state directory is not writable: {e}"))
        })?;
        std::fs::remove_file(&probe).map_err(|e| {
            DoctorError::StateDirError(format!("write probe could not be removed: {e}"))
        })?;
        Ok(())
    }

    /// Asserts this host is one of the four supported target platforms.
    fn check_platform() -> Result<(), DoctorError> {
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        if !SUPPORTED_TARGETS.contains(&(os, arch)) {
            return Err(DoctorError::ConfigError(format!(
                "unsupported platform {os}/{arch}"
            )));
        }
        // Windows is excluded above; musl is the remaining unsupported
        // libc, and only a glibc build resolves this loader path.
        if os == "linux" && !std::path::Path::new("/lib64/ld-linux-x86-64.so.2").exists() {
            let alt = std::path::Path::new("/lib/ld-linux-aarch64.so.1").exists();
            if !alt {
                return Err(DoctorError::ConfigError(
                    "glibc loader not found; musl is unsupported".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Asserts this process's own executable resolves, and reports how the
    /// launcher said it was obtained. The path is deliberately never
    /// printed -- an override's location is not diagnostic information the
    /// caller is owed.
    fn check_binary_integrity() -> Result<(), DoctorError> {
        std::env::current_exe()
            .map(|_| ())
            .map_err(|e| DoctorError::ConfigError(format!("current_exe is unresolvable: {e}")))
    }

    /// Asserts the runtime socket, when it exists, is a socket owned by
    /// this uid with mode `0600`. A world-writable socket is a takeover.
    fn check_socket_permissions(&self) -> Result<(), DoctorError> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

        let Some(socket_path) = &self.socket_path else {
            return Err(DoctorError::ConfigError(
                "skipped: no socket path was supplied".to_string(),
            ));
        };
        if !socket_path.exists() {
            // No daemon is running. That is a legitimate state, and the
            // socket's absence is not a permission fault.
            return Ok(());
        }
        let metadata = std::fs::metadata(socket_path)
            .map_err(|e| DoctorError::ConfigError(format!("socket is unreadable: {e}")))?;
        if !metadata.file_type().is_socket() {
            return Err(DoctorError::ConfigError(format!(
                "{} exists but is not a socket",
                socket_path.display()
            )));
        }
        let uid = nix::unistd::Uid::current().as_raw();
        if metadata.uid() != uid {
            return Err(DoctorError::ConfigError(format!(
                "socket is owned by uid {}, expected {uid}",
                metadata.uid()
            )));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(DoctorError::ConfigError(format!(
                "socket mode is {mode:o}, expected 600"
            )));
        }
        Ok(())
    }

    /// Asserts the committed schema document matches what this binary's
    /// linked `batman-protocol` generates -- the same comparison
    /// `xtask generate --check` performs. This only applies when `--repo`
    /// happens to be a checkout of the Crew source tree itself (the only
    /// place this file is ever committed); `--repo` is ordinarily an
    /// unrelated project Crew is running against, so a missing schema
    /// document there means "not applicable", not "broken" -- unlike a
    /// present-but-mismatched document, which is always a real drift.
    fn check_schema_compatibility(&self) -> Result<(), DoctorError> {
        let Some(repo_root) = &self.repo_root else {
            return Err(DoctorError::ConfigError(
                "skipped: no repository root was supplied".to_string(),
            ));
        };
        let schema_path = repo_root.join("packages/protocol-ts/schema/crew.schema.json");
        let committed = match std::fs::read(&schema_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(DoctorError::ConfigError(format!(
                    "{}: {e}",
                    schema_path.display()
                )));
            }
        };
        let generated = batman_protocol::render_schema()
            .map_err(|e| DoctorError::ConfigError(format!("rendering schema: {e}")))?;
        if committed != generated {
            return Err(DoctorError::ConfigError(format!(
                "{} is stale relative to this binary; run `bun run generate`",
                schema_path.display()
            )));
        }
        Ok(())
    }

    /// Probes the vendor CLI for `kind`. A missing or unauthenticated CLI
    /// fails: a run submitted against it would fail too. A *skipped* probe
    /// (the kill switch) is reported as usable because it was never
    /// attempted -- the switch is a development convenience, not evidence
    /// the CLI is broken.
    async fn check_adapter(kind: AdapterKind) -> Result<(), DoctorError> {
        let result = crate::conformance::probe_availability(kind).await;
        if result.disproved() {
            Err(DoctorError::AdapterUnavailable {
                adapter: kind.wire_name().to_string(),
                reason: result.detail,
            })
        } else {
            Ok(())
        }
    }

    /// Reports per-backend display availability. When the merged policy
    /// forces a specific backend (`display.backend`, anything other than
    /// `"auto"`), that backend must itself be available -- an operator who
    /// pinned `tmux` gets told `tmux` is unusable, not a vacuous pass. With
    /// no forced backend, a *real* backend (Herdr or tmux) must be
    /// available: `TerminalDisplay` is unconditionally available (it is
    /// deleted outright in a later milestone) and must never satisfy this
    /// check on its own, or the check could never fail.
    fn check_display(&self) -> Result<(), DoctorError> {
        use crate::display::{DisplayBackendTrait, HerdrDisplay, TerminalDisplay, TmuxDisplay};
        use batman_protocol::{DisplayBackend, DisplayConfig};

        let availability: [(DisplayBackend, bool); 3] = [
            (
                DisplayBackend::Herdr,
                HerdrDisplay::new(DisplayConfig::default()).is_available(),
            ),
            (
                DisplayBackend::Tmux,
                TmuxDisplay::new(DisplayConfig::default()).is_available(),
            ),
            (
                DisplayBackend::Terminal,
                TerminalDisplay::new(DisplayConfig::default()).is_available(),
            ),
        ];

        let forced = self
            .policy
            .as_ref()
            .and_then(|policy| policy.display_backend.parse::<DisplayBackend>().ok());

        if let Some(forced) = forced {
            let available = availability
                .iter()
                .find(|(backend, _)| *backend == forced)
                .map(|(_, available)| *available)
                .unwrap_or(false);
            return if available {
                Ok(())
            } else {
                Err(DoctorError::ConfigError(format!(
                    "configured display backend '{forced}' is not available"
                )))
            };
        }

        let real_backend_available = availability
            .iter()
            .any(|(backend, available)| *backend != DisplayBackend::Terminal && *available);
        if real_backend_available {
            Ok(())
        } else {
            let detail = availability
                .iter()
                .map(|(backend, available)| format!("{backend}: {available}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(DoctorError::ConfigError(format!(
                "no real display backend is available ({detail}); the always-available terminal \
                 fallback does not satisfy this check"
            )))
        }
    }

    /// Asserts the filesystem holding the state directory has enough room
    /// for a worktree and the journal's growth.
    fn check_disk_space(&self) -> Result<(), DoctorError> {
        let Some(state_dir) = &self.state_dir else {
            return Err(DoctorError::StateDirError(
                "skipped: no state directory was supplied".to_string(),
            ));
        };
        let stat = nix::sys::statvfs::statvfs(state_dir.as_path())
            .map_err(|e| DoctorError::StateDirError(format!("statvfs failed: {e}")))?;
        // `blocks_available()` returns `fsblkcnt_t`, which is `u32` on macOS/Darwin
        // (x86_64 and aarch64) but already `u64` on glibc Linux (x86_64 and aarch64) —
        // the only platforms this workspace targets. `fragment_size()` returns
        // `c_ulong`, which is `u64` on all four. The widening below is required on
        // macOS and a no-op on Linux; the allow covers that Linux no-op.
        #[allow(clippy::useless_conversion)]
        let free = u64::from(stat.blocks_available()) * stat.fragment_size();
        if free < MIN_FREE_BYTES {
            return Err(DoctorError::StateDirError(format!(
                "only {} MiB free, need at least {} MiB",
                free / (1024 * 1024),
                MIN_FREE_BYTES / (1024 * 1024)
            )));
        }
        Ok(())
    }

    /// Counts leases the runtime should not still be holding: a live lease
    /// whose worktree vanished, or one whose cleanup failed.
    fn check_stale_workspaces(&self) -> Result<(), DoctorError> {
        let (Some(state_dir), Some(project_id)) = (&self.state_dir, self.project_id) else {
            return Err(DoctorError::StateDirError(
                "skipped: no state directory or project was supplied".to_string(),
            ));
        };
        let leases = crate::workspace::LeaseService::open(
            project_id,
            &state_dir.join("workspace-leases.db"),
        )
        .map_err(|e| DoctorError::StateDirError(e.to_string()))?;
        let stale = leases
            .stale()
            .map_err(|e| DoctorError::StateDirError(e.to_string()))?;
        if stale.is_empty() {
            return Ok(());
        }
        let detail = stale
            .iter()
            .map(|(id, state)| format!("{id} ({state})"))
            .collect::<Vec<_>>()
            .join(", ");
        Err(DoctorError::StateDirError(format!(
            "{} stale workspace lease(s): {detail} -- release one with \
             `crewd lease release --repo <repo> --lease-id <id>`",
            stale.len()
        )))
    }

    /// Counts runs left in a non-terminal state whose last journaled event
    /// is older than [`crate::recovery::DEFAULT_STALE_RUN_THRESHOLD`],
    /// reusing the recovery coordinator's own stuck-run query. Read-only: it
    /// never transitions anything, and it never claims a silent run is dead
    /// -- a running daemon can legitimately supervise a run that emits
    /// nothing for minutes.
    async fn check_stale_runs(&self) -> Result<(), DoctorError> {
        let (Some(db), Some(project_id)) = (&self.db, self.project_id) else {
            return Err(DoctorError::DatabaseError(
                "skipped: no database handle or project was supplied".to_string(),
            ));
        };
        let coordinator =
            crate::recovery::RecoveryCoordinator::with_defaults(Arc::clone(db), project_id);
        let stuck = coordinator
            .find_stuck_runs(crate::recovery::SweepScope::StaleBeyond(
                crate::recovery::DEFAULT_STALE_RUN_THRESHOLD,
            ))
            .await
            .map_err(|e| DoctorError::DatabaseError(e.to_string()))?;
        if stuck.is_empty() {
            return Ok(());
        }
        let detail = stuck
            .iter()
            .map(|run| run.run_id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(DoctorError::DatabaseError(format!(
            "{} stuck run(s): {detail}",
            stuck.len()
        )))
    }
}

/// Accumulates check outcomes. Exists so no call site can push a name onto
/// `passed_checks` without having produced an `Ok`.
#[derive(Default)]
struct Report {
    passed_checks: Vec<String>,
    failed_checks: Vec<FailedCheck>,
}

impl Report {
    fn record(&mut self, check_name: &str, outcome: Result<(), DoctorError>) {
        match outcome {
            Ok(()) => self.passed_checks.push(check_name.to_string()),
            Err(error) => self.failed_checks.push(FailedCheck {
                check_name: check_name.to_string(),
                error: error.to_string(),
            }),
        }
    }
}
