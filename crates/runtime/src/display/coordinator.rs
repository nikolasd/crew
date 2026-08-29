//! Pane coordinator: resolves a display backend for a run, opens a
//! Crew-owned pane running `crewd attach <run-id>` around the run's own
//! attach socket, and journals `DisplayPaneAttached`/`DisplayPaneDetached`
//! with the *real* pane reference the backend returned.
//!
//! Not yet wired into any production call site. `start_queued_run`
//! (`crate::service::orchestration`) still journals the empty-pane-ref
//! placeholder for every run today (headless included) -- WP11's TUI
//! adapter is what will call [`PaneCoordinator::attach`]/[`PaneCoordinator::detach`],
//! once a PTY and an [`super::AttachServer`] exist for a run's pane
//! command to actually point at. This module is fully exercised here
//! against a fake [`super::DisplayBackendTrait`].

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::broadcast;

use crew_protocol::{DisplayBackend, DisplayPlacement, EventEnvelope, ProjectId, RunId, WorkerId};

use crate::config::crew::CloseOnExit;
use crate::db::DatabaseHandle;
use crate::domain::{DomainRepository, broadcast_committed, embed_envelope};

use super::{DisplayRegistry, PaneHandle, PaneRequest};

/// The default backend fallback chain, tried in this order whenever a
/// candidate is unavailable or fails to create a pane. `Hidden` is
/// always last and always available, so this chain never bottoms out
/// with nothing selected.
const DEFAULT_CHAIN: [DisplayBackend; 4] = [
    DisplayBackend::Herdr,
    DisplayBackend::Tmux,
    DisplayBackend::OsWindow,
    DisplayBackend::Hidden,
];

/// Orders the candidate backends to try: `forced` first (the merged
/// `display.backend` config, when it isn't `Auto`), then the default
/// chain with `forced` itself deduplicated out.
fn ordered_candidates(forced: Option<DisplayBackend>) -> Vec<DisplayBackend> {
    match forced {
        Some(backend) => {
            let mut ordered = vec![backend];
            ordered.extend(DEFAULT_CHAIN.into_iter().filter(|b| *b != backend));
            ordered
        }
        None => DEFAULT_CHAIN.to_vec(),
    }
}

/// Everything [`PaneCoordinator::attach`] needs to name, place, and
/// point one run's pane at its own attach socket.
#[derive(Debug, Clone)]
pub struct PaneAttachRequest {
    pub run_id: RunId,
    pub worker_id: WorkerId,
    /// The adapter name (`claude`, `codex`, ...), rendered into the
    /// pane's title.
    pub adapter: String,
    pub placement: DisplayPlacement,
    /// The config-forced backend (`display.backend`, anything but
    /// `Auto`; see `crate::config::protocol_display_backend`). `None`
    /// for `Auto`, meaning "try the default chain".
    pub forced_backend: Option<DisplayBackend>,
}

/// What a run's pane resolved to. `backend` is `Hidden` whenever every
/// real candidate was unavailable or a `create_pane` call itself failed
/// -- never an error on its own; `pane_ref` is empty in exactly that
/// case. Pass this to [`PaneCoordinator::detach`] once the run settles.
#[derive(Debug, Clone)]
pub struct PaneAttachOutcome {
    run_id: RunId,
    pub backend: DisplayBackend,
    pub placement: DisplayPlacement,
    pub pane_ref: String,
    handle: Option<PaneHandle>,
}

/// Resolves, opens, and later closes one run's Crew-owned pane, and owns
/// journaling both ends of its lifetime.
/// How many Crew-owned panes may be live at once before further attaches
/// degrade to `Hidden` (ADR-0027 wave 3).
///
/// A TUI vendor outlives the turn that opened it, so panes accumulate: every
/// finished-but-unclosed worker keeps a window or split on the user's
/// screen, and nothing bounded that. The cap is what stops a long session
/// from burying the user's own terminal under workers they are done with.
/// Deliberately generous -- it is a backstop against unbounded growth, not
/// a workflow limit.
pub const DEFAULT_MAX_LIVE_PANES: usize = 16;

pub struct PaneCoordinator {
    registry: Arc<DisplayRegistry>,
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    events_tx: broadcast::Sender<EventEnvelope>,
    /// This runtime's own verified binary path (`std::env::current_exe()`
    /// at startup) -- what the pane actually runs is `<crewd_path>
    /// attach <run-id> --repo <repository> --state-dir <state_dir>`.
    crewd_path: PathBuf,
    state_dir: PathBuf,
    repository: PathBuf,
    /// The runs that currently hold a real (non-hidden) pane. A set rather
    /// than a counter so re-attaching the same run is idempotent and a
    /// detach cannot double-decrement.
    live_panes: Arc<Mutex<HashSet<RunId>>>,
    max_live_panes: usize,
}

impl PaneCoordinator {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<DisplayRegistry>,
        db: Arc<DatabaseHandle>,
        project_id: ProjectId,
        events_tx: broadcast::Sender<EventEnvelope>,
        crewd_path: PathBuf,
        state_dir: PathBuf,
        repository: PathBuf,
    ) -> Self {
        Self {
            registry,
            db,
            project_id,
            events_tx,
            crewd_path,
            state_dir,
            repository,
            live_panes: Arc::new(Mutex::new(HashSet::new())),
            max_live_panes: DEFAULT_MAX_LIVE_PANES,
        }
    }

    /// Overrides the live-pane cap. Chainable so the nine existing
    /// `new()` call sites stay untouched.
    #[must_use]
    pub fn with_max_live_panes(mut self, max: usize) -> Self {
        self.max_live_panes = max;
        self
    }

    /// Reserves a live-pane slot for `run_id`, or reports the cap is full.
    ///
    /// Idempotent per run: a run that already holds a pane (a reopen)
    /// re-reserves its own slot rather than consuming a second one.
    fn reserve_pane(&self, run_id: RunId) -> bool {
        let mut live = self.live_panes.lock();
        if live.contains(&run_id) {
            return true;
        }
        if live.len() >= self.max_live_panes {
            return false;
        }
        live.insert(run_id);
        true
    }

    /// Releases `run_id`'s live-pane slot.
    fn release_pane(&self, run_id: RunId) {
        self.live_panes.lock().remove(&run_id);
    }

    /// Resolves a backend (forced backend first, then herdr/tmux/
    /// os-window/hidden by availability), opens a pane running `crewd
    /// attach <run-id> ...` at `req.placement`, and journals
    /// `DisplayPaneAttached` with the real pane reference. A
    /// `create_pane` failure on the resolved backend is never fatal to
    /// the run: it is journaled as a diagnostic and this falls back to
    /// `Hidden` (an empty `pane_ref`) instead of propagating the error.
    pub async fn attach(&self, req: PaneAttachRequest) -> PaneAttachOutcome {
        // ADR-0027 wave 3: a TUI vendor outlives its turn, so panes
        // accumulate. Past the cap, degrade to hidden -- and journal why,
        // rather than silently reporting a pane that was never opened.
        if !self.reserve_pane(req.run_id) {
            self.journal_diagnostic(
                req.run_id,
                format!(
                    "live pane cap of {} reached, attaching hidden instead; close a finished \
                     worker's pane to free one",
                    self.max_live_panes
                ),
            )
            .await;
            return self.attach_hidden(req.run_id, req.placement).await;
        }
        let candidates = ordered_candidates(req.forced_backend);
        let selection = self.registry.resolve(&crew_protocol::DisplayPreference {
            ordered: candidates,
            placement: req.placement,
        });

        let Some(backend) = selection.selected else {
            // Reachable only with a hand-built registry that never
            // registered `Hidden` (a test); production registries built
            // by `DisplayRegistry::with_default_backends` always do.
            return self.attach_hidden(req.run_id, req.placement).await;
        };

        let Some(display) = self.registry.find(backend) else {
            return self.attach_hidden(req.run_id, req.placement).await;
        };

        let pane_request = self.pane_request(&req);
        match display.create_pane(pane_request).await {
            Ok(handle) => {
                let pane_ref = handle.pane_ref.clone();
                self.journal_attach(req.run_id, backend, req.placement, pane_ref.clone())
                    .await;
                PaneAttachOutcome {
                    run_id: req.run_id,
                    backend,
                    placement: req.placement,
                    pane_ref,
                    handle: Some(handle),
                }
            }
            Err(err) => {
                // No real pane exists, so the reservation must not be held.
                self.release_pane(req.run_id);
                self.journal_diagnostic(
                    req.run_id,
                    format!("pane creation on {backend} failed, falling back to hidden: {err}"),
                )
                .await;
                self.attach_hidden(req.run_id, req.placement).await
            }
        }
    }

    /// Reopens a pane for an OMP-owned run. Unlike [`Self::attach`], the
    /// durable `DisplayPaneAttached` write verifies task ownership IN its
    /// transaction, so a reconcile rebind cannot interleave after a caller
    /// precheck and let a stale instance journal a pane event.
    pub async fn attach_owned(
        &self,
        req: PaneAttachRequest,
        owner_instance_id: String,
    ) -> Result<PaneAttachOutcome, crate::domain::DomainError> {
        let selection = self.registry.resolve(&crew_protocol::DisplayPreference {
            ordered: ordered_candidates(req.forced_backend),
            placement: req.placement,
        });
        let Some(backend) = selection.selected else {
            self.journal_attach_guarded(
                req.run_id,
                DisplayBackend::Hidden,
                req.placement,
                String::new(),
                owner_instance_id,
            )
            .await?;
            return Ok(PaneAttachOutcome {
                run_id: req.run_id,
                backend: DisplayBackend::Hidden,
                placement: req.placement,
                pane_ref: String::new(),
                handle: None,
            });
        };
        let Some(display) = self.registry.find(backend) else {
            self.journal_attach_guarded(
                req.run_id,
                DisplayBackend::Hidden,
                req.placement,
                String::new(),
                owner_instance_id,
            )
            .await?;
            return Ok(PaneAttachOutcome {
                run_id: req.run_id,
                backend: DisplayBackend::Hidden,
                placement: req.placement,
                pane_ref: String::new(),
                handle: None,
            });
        };
        match display.create_pane(self.pane_request(&req)).await {
            Ok(handle) => {
                let pane_ref = handle.pane_ref.clone();
                self.journal_attach_guarded(
                    req.run_id,
                    backend,
                    req.placement,
                    pane_ref.clone(),
                    owner_instance_id,
                )
                .await?;
                Ok(PaneAttachOutcome {
                    run_id: req.run_id,
                    backend,
                    placement: req.placement,
                    pane_ref,
                    handle: Some(handle),
                })
            }
            Err(err) => Err(crate::domain::DomainError::Sqlite(
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
                    "pane creation on {backend} failed: {err}"
                )))),
            )),
        }
    }

    /// Honors `close_on_exit` for a settled run: closes the pane (if
    /// any real one exists) and journals `DisplayPaneDetached` with the
    /// real ref, or leaves the pane alone for `Never`. A close failure
    /// is logged, never propagated -- the run already settled, there is
    /// no RPC caller left to report it to.
    pub async fn detach(
        &self,
        outcome: &PaneAttachOutcome,
        succeeded: bool,
        close_on_exit: CloseOnExit,
    ) {
        // Freed whatever the close policy decides below: a run that has
        // settled is no longer holding a pane the cap should count, and a
        // `Never` policy leaving the window on screen is the user's own
        // pane from here on, not one Crew will ever close.
        self.release_pane(outcome.run_id);
        let should_close = match close_on_exit {
            CloseOnExit::Always => true,
            CloseOnExit::OnSuccess => succeeded,
            CloseOnExit::Never => false,
        };
        if !should_close {
            return;
        }

        if let Some(handle) = &outcome.handle
            && let Some(display) = self.registry.find(outcome.backend)
            && let Err(err) = display.close_pane(handle).await
        {
            tracing::warn!(
                backend = %outcome.backend,
                error = %err,
                "failed to close a Crew-owned pane"
            );
        }

        self.journal_detach(
            outcome.run_id,
            outcome.backend,
            outcome.placement,
            outcome.pane_ref.clone(),
        )
        .await;
    }

    fn pane_request(&self, req: &PaneAttachRequest) -> PaneRequest {
        PaneRequest {
            title: format!("crew: {} ({})", req.worker_id, req.adapter),
            command: vec![
                self.crewd_path.to_string_lossy().into_owned(),
                "attach".to_string(),
                req.run_id.to_string(),
                "--repo".to_string(),
                self.repository.to_string_lossy().into_owned(),
                "--state-dir".to_string(),
                self.state_dir.to_string_lossy().into_owned(),
            ],
            placement: req.placement,
        }
    }

    async fn attach_hidden(&self, run_id: RunId, placement: DisplayPlacement) -> PaneAttachOutcome {
        self.journal_attach(run_id, DisplayBackend::Hidden, placement, String::new())
            .await;
        PaneAttachOutcome {
            run_id,
            backend: DisplayBackend::Hidden,
            placement,
            pane_ref: String::new(),
            handle: None,
        }
    }

    async fn journal_attach(
        &self,
        run_id: RunId,
        backend: DisplayBackend,
        placement: DisplayPlacement,
        pane_ref: String,
    ) {
        let project_id = self.project_id;
        let committed = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_display_event(
                    crew_protocol::RuntimeEventKind::DisplayPaneAttached,
                    run_id,
                    backend,
                    placement,
                    pane_ref,
                )
                .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await;
        self.commit_and_broadcast(committed, "DisplayPaneAttached")
            .await;
    }

    async fn journal_attach_guarded(
        &self,
        run_id: RunId,
        backend: DisplayBackend,
        placement: DisplayPlacement,
        pane_ref: String,
        owner_instance_id: String,
    ) -> Result<(), crate::domain::DomainError> {
        let project_id = self.project_id;
        let mut value = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let (task_id, owner): (String, String) = conn.query_row(
                    "SELECT t.task_id, t.owner_client_instance_id
                     FROM runs r JOIN tasks t ON t.task_id = r.task_id
                     WHERE r.run_id = ?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if owner != owner_instance_id {
                    return Err(crate::domain::DomainError::NotOwner {
                        task_id,
                        instance_id: owner_instance_id,
                    });
                }
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_display_event(
                    crew_protocol::RuntimeEventKind::DisplayPaneAttached,
                    run_id,
                    backend,
                    placement,
                    pane_ref,
                )
                .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await?;
        let _ = broadcast_committed(&self.events_tx, &mut value);
        Ok(())
    }

    async fn journal_detach(
        &self,
        run_id: RunId,
        backend: DisplayBackend,
        placement: DisplayPlacement,
        pane_ref: String,
    ) {
        let project_id = self.project_id;
        let committed = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_display_event(
                    crew_protocol::RuntimeEventKind::DisplayPaneDetached,
                    run_id,
                    backend,
                    placement,
                    pane_ref,
                )
                .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await;
        self.commit_and_broadcast(committed, "DisplayPaneDetached")
            .await;
    }

    async fn journal_diagnostic(&self, run_id: RunId, message: String) {
        let project_id = self.project_id;
        let committed = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_diagnostic(
                    run_id,
                    crew_protocol::DiagnosticLevel::Warning,
                    "pane_creation_failed",
                    message,
                )
                .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await;
        self.commit_and_broadcast(committed, "Diagnostic").await;
    }

    /// Shared tail of every journal call above: on a DB failure, log and
    /// swallow (there is no RPC caller left to report to on this path);
    /// on success, broadcast the same committed envelope so a live
    /// monitor sees it (invariant: every domain mutation commits and
    /// broadcasts in the same call).
    async fn commit_and_broadcast(
        &self,
        committed: Result<serde_json::Value, crate::domain::DomainError>,
        what: &str,
    ) {
        match committed {
            Ok(mut value) => {
                let _ = broadcast_committed(&self.events_tx, &mut value);
            }
            Err(err) => {
                tracing::warn!(error = %err, event = what, "failed to journal a pane lifecycle event");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crew_protocol::{DisplayConfig, DisplayStatus};
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::display::{DisplayBackendTrait, DisplayFuture};

    /// A fake backend whose `create_pane`/`close_pane` outcomes are
    /// controlled per-test, and which records every call it received.
    struct FakeBackend {
        name: &'static str,
        wire_backend: DisplayBackend,
        available: bool,
        create_result: Mutex<Option<Result<PaneHandle, String>>>,
        create_calls: AtomicUsize,
        close_calls: Mutex<Vec<PaneHandle>>,
    }

    impl FakeBackend {
        fn new(name: &'static str, wire_backend: DisplayBackend, available: bool) -> Self {
            Self {
                name,
                wire_backend,
                available,
                create_result: Mutex::new(None),
                create_calls: AtomicUsize::new(0),
                close_calls: Mutex::new(Vec::new()),
            }
        }

        fn succeeding(self, pane_ref: &str) -> Self {
            *self.create_result.lock() = Some(Ok(PaneHandle {
                backend: self.wire_backend,
                pane_ref: pane_ref.to_string(),
            }));
            self
        }

        fn failing(self, message: &str) -> Self {
            *self.create_result.lock() = Some(Err(message.to_string()));
            self
        }
    }

    impl DisplayBackendTrait for FakeBackend {
        fn backend_name(&self) -> &str {
            self.name
        }

        fn is_available(&self) -> bool {
            self.available
        }

        fn activate(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn status(&self) -> DisplayStatus {
            DisplayStatus::new(self.wire_backend, self.available, false)
        }

        fn create_pane(&self, _req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
            self.create_calls.fetch_add(1, Ordering::Relaxed);
            let result =
                self.create_result.lock().clone().unwrap_or_else(|| {
                    Err("FakeBackend has no create_result configured".to_string())
                });
            Box::pin(async move { result })
        }

        fn close_pane(&self, handle: &PaneHandle) -> DisplayFuture<'_, ()> {
            self.close_calls.lock().push(handle.clone());
            Box::pin(async { Ok(()) })
        }
    }

    async fn harness() -> (Arc<DatabaseHandle>, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix("bat-pane-coordinator-")
            .tempdir_in("/tmp")
            .expect("create temp dir");
        let db_path = dir.path().join("state.db");
        let db = Arc::new(
            DatabaseHandle::start(db_path)
                .await
                .expect("start database"),
        );
        (db, dir)
    }

    fn coordinator(
        registry: DisplayRegistry,
        db: Arc<DatabaseHandle>,
        events_tx: broadcast::Sender<EventEnvelope>,
    ) -> PaneCoordinator {
        PaneCoordinator::new(
            Arc::new(registry),
            db,
            ProjectId::new(),
            events_tx,
            PathBuf::from("/opt/crew/crewd"),
            PathBuf::from("/state"),
            PathBuf::from("/repo"),
        )
    }

    fn attach_request(forced_backend: Option<DisplayBackend>) -> PaneAttachRequest {
        PaneAttachRequest {
            run_id: RunId::new(),
            worker_id: WorkerId::new(),
            adapter: "claude".to_string(),
            placement: DisplayPlacement::SplitRight,
            forced_backend,
        }
    }

    fn is_display_event(
        event: &crew_protocol::RuntimeEvent,
        kind: crew_protocol::RuntimeEventKind,
    ) -> bool {
        matches!(
            event,
            crew_protocol::RuntimeEvent::DisplayEvent { kind: k, .. } if *k == kind
        )
    }

    #[tokio::test]
    async fn attach_journals_and_broadcasts_the_real_pane_ref_from_the_selected_backend() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).succeeding("w1:p2"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);

        let outcome = coordinator.attach(attach_request(None)).await;

        assert_eq!(outcome.backend, DisplayBackend::Herdr);
        assert_eq!(outcome.pane_ref, "w1:p2");

        let envelope = events_rx.try_recv().expect("attach must broadcast");
        assert!(is_display_event(
            &envelope.event,
            crew_protocol::RuntimeEventKind::DisplayPaneAttached
        ));

        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn attach_tries_the_forced_backend_before_the_default_chain() {
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        // Herdr would normally win by default-chain order, but the
        // forced backend is tmux -- it must be tried first and win.
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).succeeding("herdr-pane"),
        ));
        registry.register(Box::new(
            FakeBackend::new("tmux", DisplayBackend::Tmux, true).succeeding("%7"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, db.clone(), events_tx);

        let outcome = coordinator
            .attach(attach_request(Some(DisplayBackend::Tmux)))
            .await;

        assert_eq!(outcome.backend, DisplayBackend::Tmux);
        assert_eq!(outcome.pane_ref, "%7");
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_create_pane_failure_journals_a_diagnostic_and_falls_back_to_hidden() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).failing("herdr exploded"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);

        let outcome = coordinator.attach(attach_request(None)).await;

        assert_eq!(outcome.backend, DisplayBackend::Hidden);
        assert_eq!(outcome.pane_ref, "");

        // Diagnostic, then the Hidden DisplayPaneAttached -- both
        // broadcast, in that order.
        let diagnostic = events_rx.try_recv().expect("diagnostic must broadcast");
        assert!(matches!(
            diagnostic.event,
            crew_protocol::RuntimeEvent::Diagnostic { .. }
        ));
        let attached = events_rx.try_recv().expect("hidden attach must broadcast");
        assert!(is_display_event(
            &attached.event,
            crew_protocol::RuntimeEventKind::DisplayPaneAttached
        ));

        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn detach_always_closes_and_journals_regardless_of_success() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).succeeding("w1:p2"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);
        let outcome = coordinator.attach(attach_request(None)).await;
        let _ = events_rx.try_recv(); // drain the attach broadcast

        coordinator
            .detach(&outcome, false, CloseOnExit::Always)
            .await;

        let detached = events_rx.try_recv().expect("Always must journal a detach");
        assert!(is_display_event(
            &detached.event,
            crew_protocol::RuntimeEventKind::DisplayPaneDetached
        ));
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn detach_on_success_closes_only_when_the_run_succeeded() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).succeeding("w1:p2"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);
        let outcome = coordinator.attach(attach_request(None)).await;
        let _ = events_rx.try_recv();

        coordinator
            .detach(&outcome, false, CloseOnExit::OnSuccess)
            .await;
        assert!(
            events_rx.try_recv().is_err(),
            "a failed run under OnSuccess must never journal a detach"
        );

        coordinator
            .detach(&outcome, true, CloseOnExit::OnSuccess)
            .await;
        let detached = events_rx
            .try_recv()
            .expect("a succeeded run under OnSuccess must journal a detach");
        assert!(is_display_event(
            &detached.event,
            crew_protocol::RuntimeEventKind::DisplayPaneDetached
        ));
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn detach_under_never_leaves_the_pane_alone() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).succeeding("w1:p2"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);
        let outcome = coordinator.attach(attach_request(None)).await;
        let _ = events_rx.try_recv();

        coordinator.detach(&outcome, true, CloseOnExit::Never).await;
        assert!(
            events_rx.try_recv().is_err(),
            "Never must never journal a detach or close the pane"
        );
        db.shutdown().await.expect("shutdown database");
    }
    // ------------------------------ live-pane cap (CREW-3 wave 3)

    fn working_registry() -> DisplayRegistry {
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("tmux", DisplayBackend::Tmux, true).succeeding("w1:p1"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        registry
    }

    /// A TUI vendor outlives its turn, so panes accumulate with nothing
    /// bounding them. Past the cap an attach degrades to hidden -- and says
    /// so, rather than reporting a pane nobody opened.
    #[tokio::test]
    async fn attach_degrades_to_hidden_once_the_live_pane_cap_is_reached() {
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let coordinator =
            coordinator(working_registry(), Arc::clone(&db), events_tx).with_max_live_panes(2);

        let first = coordinator.attach(attach_request(None)).await;
        let second = coordinator.attach(attach_request(None)).await;
        assert_eq!(first.backend, DisplayBackend::Tmux);
        assert_eq!(second.backend, DisplayBackend::Tmux);

        let third = coordinator.attach(attach_request(None)).await;
        assert_eq!(
            third.backend,
            DisplayBackend::Hidden,
            "the third pane exceeds the cap of 2"
        );

        db.shutdown().await.expect("shutdown database");
    }

    /// Detaching frees the slot: the cap bounds *live* panes, not panes
    /// ever opened.
    #[tokio::test]
    async fn detaching_frees_a_live_pane_slot() {
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let coordinator =
            coordinator(working_registry(), Arc::clone(&db), events_tx).with_max_live_panes(1);

        let first = coordinator.attach(attach_request(None)).await;
        assert_eq!(first.backend, DisplayBackend::Tmux);
        assert_eq!(
            coordinator.attach(attach_request(None)).await.backend,
            DisplayBackend::Hidden,
            "the cap of 1 is full"
        );

        coordinator.detach(&first, true, CloseOnExit::Always).await;

        assert_eq!(
            coordinator.attach(attach_request(None)).await.backend,
            DisplayBackend::Tmux,
            "the freed slot must be reusable"
        );

        db.shutdown().await.expect("shutdown database");
    }

    /// Re-attaching the same run (a pane reopen) must not consume a second
    /// slot, or reopening would eat the cap.
    #[tokio::test]
    async fn re_attaching_the_same_run_does_not_consume_a_second_slot() {
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let coordinator =
            coordinator(working_registry(), Arc::clone(&db), events_tx).with_max_live_panes(1);

        let req = attach_request(None);
        assert_eq!(
            coordinator.attach(req.clone()).await.backend,
            DisplayBackend::Tmux
        );
        assert_eq!(
            coordinator.attach(req).await.backend,
            DisplayBackend::Tmux,
            "the same run re-attaching holds its own slot, not a new one"
        );

        db.shutdown().await.expect("shutdown database");
    }

    /// A backend failure falls back to hidden, and must not leave a
    /// reservation behind for a pane that does not exist.
    #[tokio::test]
    async fn a_failed_pane_creation_does_not_hold_a_slot() {
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("tmux", DisplayBackend::Tmux, true).failing("tmux exploded"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let failing = coordinator(registry, Arc::clone(&db), events_tx).with_max_live_panes(1);

        let failed = failing.attach(attach_request(None)).await;
        assert_eq!(failed.backend, DisplayBackend::Hidden);

        // The cap of 1 must still be entirely free.
        let (tx2, _rx2) = broadcast::channel(64);
        let second = coordinator(working_registry(), Arc::clone(&db), tx2).with_max_live_panes(1);
        assert_eq!(
            second.attach(attach_request(None)).await.backend,
            DisplayBackend::Tmux
        );

        db.shutdown().await.expect("shutdown database");
    }
}
