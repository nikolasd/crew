//! Reconnect-capable worker-MCP scope tokens.
//!
//! A token is minted immediately before the supervised vendor process
//! launches and bound to `{ projectId, taskId, workerId, runId,
//! vendorProcessIdentity, expiresAt }`. Verification on every MCP socket
//! initialization checks the token is live (not expired, its run still
//! live) and that the connecting peer's process is a descendant of the
//! recorded vendor process -- a restarted MCP subprocess within that same
//! process tree may reinitialize with the same token; a peer outside the
//! ancestry, after vendor exit, or after expiry is rejected.
//!
//! Token bytes are the `HashMap` key only: never journaled, logged, or
//! echoed back in any diagnostic -- only the bound fields (never the token
//! string itself) are visible outside this module.

use std::collections::HashMap;
use std::sync::Mutex;

use crew_protocol::{ProjectId, RunId, TaskId, Timestamp, WorkerId};

use crate::ipc::{ScopedRun, VerifyError, WorkerCredentialVerifier};

/// The vendor process a scope token is bound to: its PID at mint time. Used
/// only to walk the connecting peer's ancestry; never persisted.
///
/// # Residual risk: PID reuse
/// Ancestry is checked by numeric pid alone (see [`PidAncestryChecker`]),
/// not a non-reusable process identity (e.g. start time) -- after this
/// pid is recycled by the OS, an unrelated later process could in
/// principle satisfy the same ancestry check. The primary defense is
/// promptness, not the ancestry check itself: every adapter that binds a
/// token here must call [`ScopeTokenStore::revoke_for_run`] as soon as it
/// observes its supervised vendor process exit (its background session
/// task's `wait()` completing), collapsing the exploitable window to the
/// gap between that exit and the adapter noticing it -- not the token's
/// full `expires_at` lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VendorProcessIdentity {
    pub pid: i32,
}

/// The scope a token binds to: everything [`ScopeTokenStore::mint`]/
/// [`ScopeTokenStore::bind`] need to activate one, bundled so neither
/// exceeds a reasonable argument count.
#[derive(Debug, Clone)]
pub struct ScopeBinding {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub worker_id: WorkerId,
    pub run_id: RunId,
    pub vendor_process: VendorProcessIdentity,
    pub expires_at: Timestamp,
}

/// The record a live scope token is bound to.
#[derive(Debug, Clone)]
struct ScopeTokenRecord {
    project_id: ProjectId,
    task_id: TaskId,
    worker_id: WorkerId,
    run_id: RunId,
    vendor_process: VendorProcessIdentity,
    expires_at: Timestamp,
}

/// Checks whether one process is a descendant of another by walking parent
/// PIDs. Injectable so tests can simulate ancestry without real processes;
/// the [`SystemPidAncestryChecker`] default walks the real process tree.
pub trait PidAncestryChecker: Send + Sync {
    /// Returns `Ok(true)` if `candidate` is `ancestor` or a descendant of
    /// it, `Ok(false)` if the walk reaches the process tree root without
    /// finding `ancestor`, or `Err` if this platform cannot report
    /// trustworthy process ancestry.
    fn is_descendant(&self, candidate: i32, ancestor: i32) -> Result<bool, AncestryError>;
}

/// Why [`ScopeTokenStore::bind`] could not activate a reserved token.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// The token is already bound to a live record. [`ScopeTokenStore::bind`]
    /// never overwrites an existing binding -- doing so could silently
    /// rebind (and thus hijack) another live run's credential.
    #[error("token is already bound to a live scope")]
    AlreadyBound,
}

/// Why a process-ancestry check could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum AncestryError {
    /// This platform has no supported mechanism to walk process ancestry.
    #[error("process ancestry is not supported on this platform")]
    Unsupported,
}

/// The real ancestry checker: walks parent PIDs via `ps -o ppid=`, portable
/// across macOS and Linux without a platform-specific `/proc` dependency.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub struct SystemPidAncestryChecker;

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl PidAncestryChecker for SystemPidAncestryChecker {
    fn is_descendant(&self, candidate: i32, ancestor: i32) -> Result<bool, AncestryError> {
        let mut pid = candidate;
        // Bound the walk: a real process tree is never this deep, and this
        // guards against a parent-pid cycle reported by a hostile/broken ps.
        for _ in 0..4096 {
            if pid == ancestor {
                return Ok(true);
            }
            if pid <= 1 {
                return Ok(false);
            }
            match parent_pid(pid) {
                Some(parent) if parent != pid => pid = parent,
                _ => return Ok(false),
            }
        }
        Ok(false)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parent_pid(pid: i32) -> Option<i32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// The foundation-style default on platforms without a supported ancestry
/// mechanism: reports worker coordination as unsupported rather than
/// accepting an unverifiable reconnect.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub struct SystemPidAncestryChecker;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl PidAncestryChecker for SystemPidAncestryChecker {
    fn is_descendant(&self, _candidate: i32, _ancestor: i32) -> Result<bool, AncestryError> {
        Err(AncestryError::Unsupported)
    }
}

/// An in-memory store of live scope tokens, backing a
/// [`WorkerCredentialVerifier`]. One store per runtime process.
pub struct ScopeTokenStore {
    tokens: Mutex<HashMap<String, ScopeTokenRecord>>,
    ancestry: Box<dyn PidAncestryChecker>,
}

impl ScopeTokenStore {
    /// Creates an empty store using the real system ancestry checker.
    #[must_use]
    pub fn new() -> Self {
        Self::with_ancestry_checker(Box::new(SystemPidAncestryChecker))
    }

    /// Creates an empty store using an injected ancestry checker (tests).
    #[must_use]
    pub fn with_ancestry_checker(ancestry: Box<dyn PidAncestryChecker>) -> Self {
        Self {
            tokens: Mutex::new(HashMap::new()),
            ancestry,
        }
    }

    /// Mints a fresh token bound to the given scope, returning its bearer
    /// string. Convenience for callers that already know the vendor
    /// process's pid before minting (chiefly tests); a real adapter spawn
    /// cannot know that pid until after the process is already running,
    /// so it uses [`Self::reserve_token`] then [`Self::bind`] instead --
    /// see their docs.
    pub fn mint(&self, binding: ScopeBinding) -> String {
        let token = self.reserve_token();
        self.bind(token.clone(), binding)
            .expect("a freshly reserved token is never already bound");
        token
    }

    /// Generates a fresh bearer token string, deliberately *not* inserted
    /// into the live table yet -- [`Self::verify`] rejects it as
    /// [`VerifyError::InvalidToken`] (unknown) until [`Self::bind`] makes
    /// it live. Callers put the reserved value into the vendor process's
    /// environment before spawning it (the only way it can be present at
    /// `execve` time), then bind it to that process's real pid once
    /// spawn returns one -- never the reverse, and never a guessed pid.
    #[must_use]
    pub fn reserve_token(&self) -> String {
        uuid::Uuid::now_v7().to_string()
    }

    /// Activates a token already handed to [`Self::reserve_token`]'s
    /// caller, binding it to the real, now-known vendor process pid. Call
    /// immediately after the supervised vendor process's spawn returns a
    /// pid, before any interaction with it.
    ///
    /// # Errors
    /// Returns [`BindError::AlreadyBound`] if `token` is already bound to
    /// a live record -- never overwrites one, since that would silently
    /// rebind (and thus hijack) whatever run currently owns it.
    pub fn bind(&self, token: String, binding: ScopeBinding) -> Result<(), BindError> {
        let now = Timestamp::now();
        let mut tokens = self
            .tokens
            .lock()
            .expect("scope token mutex is never poisoned");
        // Sweep here too (R96): `verify` sweeps, but a workload that
        // binds tokens for runs whose MCP client never calls verify
        // would still grow monotonically.
        tokens.retain(|_, record| now <= record.expires_at);
        if tokens.contains_key(&token) {
            return Err(BindError::AlreadyBound);
        }
        tokens.insert(
            token,
            ScopeTokenRecord {
                project_id: binding.project_id,
                task_id: binding.task_id,
                worker_id: binding.worker_id,
                run_id: binding.run_id,
                vendor_process: binding.vendor_process,
                expires_at: binding.expires_at,
            },
        );
        Ok(())
    }

    /// Revokes the token bound to `run_id`, if any (e.g. when the run
    /// settles). Idempotent.
    pub fn revoke_for_run(&self, run_id: RunId) {
        let mut tokens = self
            .tokens
            .lock()
            .expect("scope token mutex is never poisoned");
        tokens.retain(|_, record| record.run_id != run_id);
    }

    /// Verifies `token` against a live record, then checks `peer_pid` is
    /// the recorded vendor process or one of its descendants.
    ///
    /// # Errors
    /// Returns [`VerifyError::InvalidToken`] if the token is unknown or
    /// expired, and [`VerifyError::OutsideAncestry`] if `peer_pid` is not
    /// the vendor process or a descendant of it (including when this
    /// platform cannot report trustworthy ancestry at all).
    pub fn verify(&self, token: &str, peer_pid: Option<i32>) -> Result<ScopedRun, VerifyError> {
        let now = Timestamp::now();
        let record = {
            let mut tokens = self
                .tokens
                .lock()
                .expect("scope token mutex is never poisoned");
            // Sweep every expired record while we hold the lock: a run
            // whose adapter died before its settlement hook would
            // otherwise leak its record for the process lifetime (R96) --
            // `revoke_for_run` only fires from settlement paths. Mirrors
            // `RateLimiter::check`'s sweep; self-limiting the same way.
            tokens.retain(|_, record| now <= record.expires_at);
            tokens.get(token).cloned()
        };
        let record = record.ok_or(VerifyError::InvalidToken)?;

        let Some(peer_pid) = peer_pid else {
            return Err(VerifyError::OutsideAncestry);
        };
        let is_descendant = self
            .ancestry
            .is_descendant(peer_pid, record.vendor_process.pid)
            .map_err(|_| VerifyError::OutsideAncestry)?;
        if !is_descendant {
            return Err(VerifyError::OutsideAncestry);
        }

        Ok(ScopedRun {
            run_id: record.run_id,
            task_id: record.task_id,
            worker_id: record.worker_id,
        })
    }

    /// Returns the full scope (project/task/worker/run) bound to a live
    /// token, without re-verifying ancestry. Used by the coordination
    /// broker after the connection has already been admitted.
    #[must_use]
    pub fn scope_for_run(&self, run_id: RunId) -> Option<(ProjectId, TaskId, WorkerId)> {
        let tokens = self
            .tokens
            .lock()
            .expect("scope token mutex is never poisoned");
        tokens
            .values()
            .find(|record| record.run_id == run_id)
            .map(|record| (record.project_id, record.task_id, record.worker_id))
    }
}

impl Default for ScopeTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Adapts [`ScopeTokenStore`] to [`WorkerCredentialVerifier`] for wiring
/// into [`crate::ipc::ServerConfig`].
pub struct ScopeTokenVerifier {
    store: std::sync::Arc<ScopeTokenStore>,
}

impl ScopeTokenVerifier {
    #[must_use]
    pub fn new(store: std::sync::Arc<ScopeTokenStore>) -> Self {
        Self { store }
    }
}

impl WorkerCredentialVerifier for ScopeTokenVerifier {
    fn verify(&self, scope_token: &str, peer_pid: Option<i32>) -> Result<ScopedRun, VerifyError> {
        self.store.verify(scope_token, peer_pid)
    }
}

impl ScopeTokenStore {
    /// The number of live records. Test-only: exists so the expiry sweep
    /// is observable.
    #[cfg(test)]
    pub(crate) fn tracked_records(&self) -> usize {
        self.tokens
            .lock()
            .expect("scope token mutex is never poisoned")
            .len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAncestry {
        descendant_of: Vec<(i32, i32)>,
    }

    impl PidAncestryChecker for FakeAncestry {
        fn is_descendant(&self, candidate: i32, ancestor: i32) -> Result<bool, AncestryError> {
            Ok(self.descendant_of.contains(&(candidate, ancestor)) || candidate == ancestor)
        }
    }

    fn store_with(pairs: Vec<(i32, i32)>) -> ScopeTokenStore {
        ScopeTokenStore::with_ancestry_checker(Box::new(FakeAncestry {
            descendant_of: pairs,
        }))
    }

    fn binding(run_id: RunId, pid: i32, expires_at: Timestamp) -> ScopeBinding {
        ScopeBinding {
            project_id: ProjectId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            run_id,
            vendor_process: VendorProcessIdentity { pid },
            expires_at,
        }
    }

    #[test]
    fn verifies_a_descendant_of_the_vendor_process() {
        let store = store_with(vec![(200, 100)]);
        let run_id = RunId::new();
        let token = store.mint(binding(
            run_id,
            100,
            Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
        ));

        let scoped = store
            .verify(&token, Some(200))
            .expect("descendant verifies");
        assert_eq!(scoped.run_id, run_id);
    }

    #[test]
    fn rejects_a_peer_outside_ancestry() {
        let store = store_with(vec![]);
        let token = store.mint(binding(
            RunId::new(),
            100,
            Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
        ));

        let err = store.verify(&token, Some(999)).unwrap_err();
        assert!(matches!(err, VerifyError::OutsideAncestry));
    }

    #[test]
    fn rejects_after_expiry() {
        let store = store_with(vec![]);
        let token = store.mint(binding(
            RunId::new(),
            100,
            Timestamp::parse("2000-01-01T00:00:00Z").unwrap(),
        ));

        let err = store.verify(&token, Some(100)).unwrap_err();
        assert!(matches!(err, VerifyError::InvalidToken));
    }

    /// R96: a run whose adapter died before its settlement hook never
    /// gets `revoke_for_run`, so its expired record would leak for the
    /// process lifetime. Any later `verify` -- for any token -- must
    /// sweep drained records, mirroring `RateLimiter::check` (R65).
    #[test]
    fn an_expired_record_is_swept_by_any_later_verify() {
        let store = store_with(vec![]);
        // Live first: `bind` sweeps too (batch-12 review S1), so minting
        // the expired record second leaves both present.
        let live = store.mint(binding(
            RunId::new(),
            200,
            Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
        ));
        let _leaked = store.mint(binding(
            RunId::new(),
            100,
            Timestamp::parse("2000-01-01T00:00:00Z").unwrap(),
        ));
        assert_eq!(store.tracked_records(), 2);

        store
            .verify(&live, Some(200))
            .expect("the live token still verifies");
        assert_eq!(
            store.tracked_records(),
            1,
            "the expired record must be swept, not leaked"
        );
    }

    #[test]
    fn rejects_an_unknown_token() {
        let store = store_with(vec![]);
        let err = store.verify("not-a-real-token", Some(100)).unwrap_err();
        assert!(matches!(err, VerifyError::InvalidToken));
    }

    #[test]
    fn a_restarted_descendant_may_reverify_the_same_token_while_the_run_is_live() {
        let store = store_with(vec![(201, 100), (202, 100)]);
        let run_id = RunId::new();
        let token = store.mint(binding(
            run_id,
            100,
            Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
        ));

        // First MCP subprocess initializes.
        assert!(store.verify(&token, Some(201)).is_ok());
        // It restarts under a new PID, still a descendant of the same
        // supervised vendor process, and reinitializes with the same token.
        let scoped = store
            .verify(&token, Some(202))
            .expect("restarted descendant reverifies");
        assert_eq!(scoped.run_id, run_id);
    }

    #[test]
    fn revoking_a_run_invalidates_its_token() {
        let store = store_with(vec![(100, 100)]);
        let run_id = RunId::new();
        let token = store.mint(binding(
            run_id,
            100,
            Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
        ));
        assert!(store.verify(&token, Some(100)).is_ok());

        store.revoke_for_run(run_id);

        let err = store.verify(&token, Some(100)).unwrap_err();
        assert!(matches!(err, VerifyError::InvalidToken));
    }

    #[test]
    fn rejects_when_the_platform_reports_no_peer_pid() {
        let store = store_with(vec![]);
        let token = store.mint(binding(
            RunId::new(),
            100,
            Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
        ));
        let err = store.verify(&token, None).unwrap_err();
        assert!(matches!(err, VerifyError::OutsideAncestry));
    }

    #[test]
    fn a_reserved_token_is_rejected_until_bound() {
        let store = store_with(vec![(100, 100)]);
        let token = store.reserve_token();

        let err = store.verify(&token, Some(100)).unwrap_err();
        assert!(matches!(err, VerifyError::InvalidToken));

        store
            .bind(
                token.clone(),
                binding(
                    RunId::new(),
                    100,
                    Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
                ),
            )
            .expect("first bind of a fresh reservation succeeds");
        assert!(store.verify(&token, Some(100)).is_ok());
    }

    #[test]
    fn binding_an_already_bound_token_is_rejected_without_disturbing_the_first_scope() {
        let store = store_with(vec![(100, 100)]);
        let token = store.reserve_token();
        let first_run = RunId::new();
        store
            .bind(
                token.clone(),
                binding(
                    first_run,
                    100,
                    Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
                ),
            )
            .expect("first bind succeeds");

        let err = store
            .bind(
                token.clone(),
                binding(
                    RunId::new(),
                    100,
                    Timestamp::parse("2099-01-01T00:00:00Z").unwrap(),
                ),
            )
            .expect_err("re-binding a live token must never succeed");
        assert!(matches!(err, BindError::AlreadyBound));

        // The original scope is untouched by the rejected re-bind attempt.
        let scoped = store
            .verify(&token, Some(100))
            .expect("original binding still verifies");
        assert_eq!(scoped.run_id, first_run);
    }
}
