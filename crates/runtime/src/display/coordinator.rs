//! Pane coordinator: resolves a display backend for a run, opens a
//! Crew-owned pane running `crewd attach <run-id>` around the run's own
//! attach socket, and journals `DisplayPaneAttached`/`DisplayPaneDetached`
//! with the *real* pane reference the backend returned.
//!
//! Wired into every TUI-mode run: `TuiAdapter::run_pipeline`
//! (`crate::adapter::tui::adapter`) calls [`PaneCoordinator::attach`] once
//! its PTY and [`super::AttachServer`] exist for the pane command to
//! actually point at, and [`PaneCoordinator::detach`] once the run
//! settles. `start_queued_run` (`crate::service::orchestration`) itself
//! journals no submit-time placeholder attach event -- the only
//! honest attach event is the real one, journaled here by whatever
//! component actually performs it. This module is fully exercised here
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

/// One redacted sentence summarizing every failed `create_pane` attempt
/// in a `PaneCoordinator::attach` walk -- once retries across candidates
/// were added, concatenating each candidate's own stderr would multiply
/// the same raw-subprocess-stderr leak surface by the number of
/// candidates tried. The typed `attempted` field on
/// `PaneDowngraded` already says WHICH backends were tried and in what
/// order; this says only how many create_pane calls failed and shows the
/// LAST one's own text, never every one's.
fn summarize_pane_creation_failures(failures: &[String]) -> String {
    match failures {
        [] => String::new(), // unreachable: only called after >=1 failure
        [only] => only.clone(),
        [.., last] => format!(
            "{} attempts failed pane creation; last: {last}",
            failures.len()
        ),
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
    /// The submitting caller's own `$TERM_PROGRAM` hint, from the
    /// run's `displayPreference.launchProgram`. Only `OsWindowDisplay`
    /// reads it; every other backend ignores it entirely.
    pub launch_program: Option<crew_protocol::HostProgramHint>,
}

/// What a run's pane resolved to. `backend` is `Hidden` whenever every
/// real candidate was either unavailable or its own `create_pane` call
/// failed (`attach` retries the remaining candidates in order
/// before giving up) -- never an error on its own; `pane_ref` is empty
/// in exactly that case. Pass this to [`PaneCoordinator::detach`] once
/// the run settles.
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
    // `PaneDowngraded.reason` embeds subprocess stderr
    // (tmux/herdr's own error text), never runtime-authored -- the same
    // full, configured redactor (built-in rules plus compiled
    // `security.patterns`) every other journal-text crossing uses, never
    // a built-ins-only instance.
    redactor: crate::security::redaction::Redactor,
    /// Forces every `attach` on this coordinator to `Hidden`,
    /// regardless of `PaneAttachRequest.forced_backend`, when set by
    /// [`Self::with_force_hidden_displays`]. Default `false` (today's
    /// behavior). Never read from the environment here -- see that
    /// method's own doc comment for why the read happens exactly once,
    /// at the single production construction site.
    force_hidden_displays: bool,
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
        redactor: crate::security::redaction::Redactor,
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
            redactor,
            force_hidden_displays: false,
        }
    }

    /// Overrides the live-pane cap. Chainable so the nine existing
    /// `new()` call sites stay untouched.
    #[must_use]
    pub fn with_max_live_panes(mut self, max: usize) -> Self {
        self.max_live_panes = max;
        self
    }

    /// Forces every subsequent `attach` to `Hidden`, regardless
    /// of what `PaneAttachRequest.forced_backend` asks for.
    ///
    /// **The env var is read exactly once, by `lifecycle.rs`'s real
    /// `serve()` at daemon startup, and passed in here as a plain
    /// `bool`** -- never read inside this struct or inside `attach`
    /// itself. That is deliberate, not an arbitrary style choice: reading
    /// `std::env::var` from code a unit test calls directly would make
    /// that test's outcome depend on whatever the *test process's*
    /// environment happens to hold, which is shared and mutable across
    /// every test in the binary (`cargo test` runs them concurrently by
    /// default) -- a classic hidden-shared-state hazard. Taking a `bool`
    /// here instead makes both states directly constructible in a unit
    /// test with no environment mutation at all (see
    /// `attach_forces_hidden_when_visible_displays_are_disabled` below).
    #[must_use]
    pub fn with_force_hidden_displays(mut self, force: bool) -> Self {
        self.force_hidden_displays = force;
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
    /// attach <run-id> ...`, and journals `DisplayPaneAttached` with the
    /// real pane reference. A `create_pane` failure on the resolved
    /// backend is never fatal to the run: it retries the remaining
    /// candidates in order before falling all the way back to
    /// `Hidden` (an empty `pane_ref`), which never fails.
    ///
    /// `req.placement` is honored as-is only for the FIRST candidate --
    /// it is a concrete, previously-resolved value (see
    /// `crate::adapter::registry`'s own earlier `resolve()` call), and
    /// `resolve()` only falls back to a backend's `natural_placement()`
    /// for a caller that never specified one in the first place. Every
    /// retry passes no explicit placement, so `resolve()` re-derives the
    /// NEW candidate's own natural form instead: the caller's placement
    /// was only ever meaningful for the backend that just failed, and an
    /// explicit request (e.g. `Workspace`, valid for herdr) carried
    /// verbatim into a backend that refuses it (tmux refuses
    /// `Workspace`/`Window` outright) would reproduce the exact backend/
    /// placement mismatch ADR-0029 exists to prevent.
    pub async fn attach(&self, req: PaneAttachRequest) -> PaneAttachOutcome {
        // A daemon built with visible displays disabled never
        // even tries a real backend -- checked before anything else in
        // this method, using `req.forced_backend` (still the TRUE
        // config/caller value; nothing upstream of this coordinator
        // rewrites it) to report what was actually being asked for.
        // Config that already says `Hidden` is not a downgrade -- there
        // is nothing being overridden, so nothing is journaled.
        if self.force_hidden_displays {
            let chain = ordered_candidates(req.forced_backend);
            let first = chain.first().copied().unwrap_or(DisplayBackend::Hidden);
            if first != DisplayBackend::Hidden {
                self.journal_visible_displays_disabled(req.run_id, first, chain, req.placement)
                    .await;
            }
            return self.attach_hidden(req.run_id, req.placement).await;
        }
        let mut candidates = ordered_candidates(req.forced_backend);
        let mut attempted: Vec<DisplayBackend> = Vec::new();
        let mut requested_backend: Option<DisplayBackend> = None;
        let mut requested_placement = req.placement;
        let mut failures: Vec<String> = Vec::new();
        let mut reserved = false;

        loop {
            let is_first_attempt = requested_backend.is_none();
            let selection = self.registry.resolve(&crew_protocol::DisplayPreference {
                ordered: candidates.clone(),
                placement: if is_first_attempt {
                    Some(req.placement)
                } else {
                    None
                },
                launch_program: req.launch_program,
            });
            attempted.extend(selection.attempts.iter().copied());

            let Some(backend) = selection.selected else {
                // No candidate left to try. Reachable only with a
                // hand-built registry that never registered `Hidden` (a
                // test); production registries built by
                // `DisplayRegistry::with_default_backends` always do. Can
                // still be hit AFTER a real candidate was reserved and
                // then failed (a retry's remaining candidates all turn
                // out unavailable), so the reservation must be released
                // here too, exactly like the sibling `registry.find`
                // branch just below -- a debug assertion is not a
                // release, and would leak the slot silently in a release
                // build.
                if reserved {
                    self.release_pane(req.run_id);
                }
                return self.attach_hidden(req.run_id, req.placement).await;
            };
            if is_first_attempt {
                requested_backend = Some(backend);
                requested_placement = selection.placement;
            }

            let Some(display) = self.registry.find(backend) else {
                if reserved {
                    self.release_pane(req.run_id);
                }
                return self.attach_hidden(req.run_id, req.placement).await;
            };

            // ADR-0027 wave 3's pane cap: reserved ONCE for the whole
            // attach, before the first REAL (non-Hidden) candidate is
            // tried, and released only if every candidate ultimately
            // fails -- at most one pane ever results from an attach, so
            // one reservation is the honest model. Hidden is not a pane:
            // it occupies no screen, so it neither consumes the cap nor
            // is refused by it. The bound on concurrent turns is the
            // registry's live-SESSION cap, which exists whether or not
            // the user wants windows; conflating the two would have left
            // a hidden-display setup unbounded.
            if backend != DisplayBackend::Hidden && !reserved {
                if !self.reserve_pane(req.run_id) {
                    self.journal_diagnostic(
                        req.run_id,
                        // The only interpolation is a configured integer,
                        // so no caller or vendor text can reach this
                        // sentence.
                        crew_protocol::Redacted::assert_runtime_authored(format!(
                            "live pane cap of {} reached, attaching hidden instead; close a \
                             finished worker's pane to free one",
                            self.max_live_panes
                        )),
                    )
                    .await;
                    // Cap-reached is a distinct, unretried short-circuit:
                    // nothing was attempted, so nothing to journal as a
                    // downgrade -- straight to hidden, exactly as before.
                    return self.attach_hidden(req.run_id, req.placement).await;
                }
                reserved = true;
            }

            let pane_request = self.pane_request(&req, selection.placement);
            match display.create_pane(pane_request).await {
                Ok(handle) => {
                    let pane_ref = handle.pane_ref.clone();
                    // The actual placement, not the requested one:
                    // OsWindowDisplay may report `Window` for a `Tab` request.
                    let placement = handle.placement;
                    let requested_backend =
                        requested_backend.expect("set on the first loop iteration");
                    // A `PaneDowngraded` whenever the actual backend
                    // diverges from the one first requested, even though
                    // THIS attempt succeeded -- an operator needs to see
                    // that the preferred backend lost, not just a total
                    // failure: that divergence, requested vs. actual, is
                    // the whole reason this event exists.
                    // Journaled BEFORE the attach event, matching the
                    // single-attempt shape this replaces: "why" precedes
                    // "what happened".
                    if backend != requested_backend {
                        self.journal_pane_downgraded(
                            req.run_id,
                            requested_backend,
                            requested_placement,
                            backend,
                            attempted.clone(),
                            summarize_pane_creation_failures(&failures),
                        )
                        .await;
                    }
                    self.journal_attach(req.run_id, backend, placement, pane_ref.clone())
                        .await;
                    if backend == DisplayBackend::Hidden && reserved {
                        // Landed on hidden after every real candidate
                        // failed -- no real pane exists, so the
                        // reservation must not be held.
                        self.release_pane(req.run_id);
                    }
                    return PaneAttachOutcome {
                        run_id: req.run_id,
                        backend,
                        placement,
                        pane_ref,
                        handle: Some(handle),
                    };
                }
                Err(err) => {
                    failures.push(format!("{backend} failed: {err}"));
                    let idx = candidates
                        .iter()
                        .position(|b| *b == backend)
                        .expect("backend was just selected from this exact list");
                    candidates = candidates[idx + 1..].to_vec();
                    if backend == DisplayBackend::Hidden {
                        // Hidden's `create_pane` is a documented no-op
                        // that always succeeds; if it somehow returned
                        // Err, do not loop forever retrying an empty
                        // candidate list.
                        if reserved {
                            self.release_pane(req.run_id);
                        }
                        return self.attach_hidden(req.run_id, req.placement).await;
                    }
                }
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
            // See the identical comment in `Self::attach`.
            placement: Some(req.placement),
            launch_program: req.launch_program,
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
        match display
            .create_pane(self.pane_request(&req, req.placement))
            .await
        {
            Ok(handle) => {
                let pane_ref = handle.pane_ref.clone();
                // The actual placement, not the requested one:
                // OsWindowDisplay may report `Window` for a `Tab` request.
                let placement = handle.placement;
                self.journal_attach_guarded(
                    req.run_id,
                    backend,
                    placement,
                    pane_ref.clone(),
                    owner_instance_id,
                )
                .await?;
                Ok(PaneAttachOutcome {
                    run_id: req.run_id,
                    backend,
                    placement,
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

    /// `placement` is passed explicitly, not read from `req.placement`
    /// directly: `attach`'s own retry loop asks each candidate for a placement
    /// re-derived per-candidate (see [`Self::attach`]'s own doc comment),
    /// so the caller picks which value applies -- `attach_owned`, which
    /// never retries, always passes `req.placement` unchanged.
    fn pane_request(&self, req: &PaneAttachRequest, placement: DisplayPlacement) -> PaneRequest {
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
            placement,
            launch_program: req.launch_program,
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

    /// Journals a pane-creation-failure fallback with typed
    /// fields, replacing the generic `Diagnostic` this used to be --
    /// see [`crate::domain::DomainRepository::record_pane_downgraded`].
    async fn journal_pane_downgraded(
        &self,
        run_id: RunId,
        requested_backend: DisplayBackend,
        requested_placement: DisplayPlacement,
        actual_backend: DisplayBackend,
        attempted: Vec<DisplayBackend>,
        reason: String,
    ) {
        // `reason` is subprocess stderr (tmux/herdr's own error text), not
        // runtime-authored -- `assert_runtime_authored` would be a false
        // claim here. `sanitize_fragment` on a `Visible` fragment only
        // returns `None` for `Thinking`/`Secret` classes, so this is
        // unreachable for a `Visible` fragment; fail loud rather than
        // silently drop the reason if that ever changes.
        let reason = match self.redactor.sanitize_fragment(&crew_protocol::Classified {
            class: crew_protocol::ContentClass::Visible,
            value: reason,
        }) {
            Some(sanitized) => crew_protocol::Redacted::from_sanitized(sanitized),
            None => {
                tracing::warn!(
                    run_id = %run_id,
                    "a Visible fragment always sanitizes to Some; journaling PaneDowngraded with an empty reason"
                );
                crew_protocol::Redacted::from_sanitized(String::new())
            }
        };
        self.commit_pane_downgraded(
            run_id,
            requested_backend,
            requested_placement,
            actual_backend,
            attempted,
            reason,
        )
        .await;
    }

    /// A policy decision this daemon made itself (visible displays
    /// disabled), never subprocess stderr -- `reason` is authored here.
    /// The only interpolated values are `DisplayBackend` variants, a
    /// closed protocol enum with every byte ours; that is what makes
    /// `Redacted::assert_runtime_authored` the correct call here rather
    /// than [`Self::journal_pane_downgraded`]'s sanitize-fragment path,
    /// which exists for text this runtime did not write itself. Adding
    /// any interpolation that is not a `DisplayBackend` must revisit
    /// that choice.
    async fn journal_visible_displays_disabled(
        &self,
        run_id: RunId,
        first: DisplayBackend,
        chain: Vec<DisplayBackend>,
        placement: DisplayPlacement,
    ) {
        // Names the whole chain, not just `first`: under `Auto` (the
        // configuration that actually produced the orphaned processes
        // this exists to prevent) the daemon would have walked every
        // candidate in order, and this sentence is the only diagnostic
        // an operator gets for a pane that silently never appeared --
        // understating it to "would have tried Herdr" would send them
        // looking at herdr alone when tmux or the OS window backend was
        // equally in play.
        let chain_desc = chain
            .iter()
            .map(|b| format!("{b:?}"))
            .collect::<Vec<_>>()
            .join(" -> ");
        let reason = crew_protocol::Redacted::assert_runtime_authored(format!(
            "visible displays disabled by CREW_FORCE_HIDDEN_DISPLAYS; would have tried {chain_desc}"
        ));
        self.commit_pane_downgraded(
            run_id,
            first,
            placement,
            DisplayBackend::Hidden,
            Vec::new(),
            reason,
        )
        .await;
    }

    /// The shared append-and-broadcast tail for a `PaneDowngraded` event,
    /// once its `reason` has already crossed the redaction boundary by
    /// whichever route its caller's content requires (subprocess stderr
    /// via [`Self::journal_pane_downgraded`]'s sanitizer, or a fixed
    /// runtime-authored sentence via [`Self::journal_visible_displays_disabled`]).
    async fn commit_pane_downgraded(
        &self,
        run_id: RunId,
        requested_backend: DisplayBackend,
        requested_placement: DisplayPlacement,
        actual_backend: DisplayBackend,
        attempted: Vec<DisplayBackend>,
        reason: crew_protocol::Redacted,
    ) {
        let project_id = self.project_id;
        let committed = self
            .db
            .run_domain_op(Box::new(move |conn| {
                let mut repo = DomainRepository::new(conn, project_id);
                repo.record_pane_downgraded(
                    run_id,
                    requested_backend,
                    requested_placement,
                    actual_backend,
                    attempted,
                    reason,
                )
                .map(|c| embed_envelope(json!({ "sequence": c.sequence }), &c.envelope))
            }))
            .await;
        self.commit_and_broadcast(committed, "PaneDowngraded").await;
    }

    /// Takes `Redacted`, not `String`, so each caller states where
    /// its text came from rather than this helper deciding for all of them.
    async fn journal_diagnostic(&self, run_id: RunId, message: crew_protocol::Redacted) {
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
        /// `Arc` so a test can hold its own handle and change the
        /// outcome BETWEEN `attach()` calls on the same coordinator --
        /// needed to prove a released reservation is actually reusable
        /// (the same coordinator, same cap, a later call that must now
        /// succeed), rather than comparing against an unrelated fresh
        /// coordinator whose own cap was never at risk.
        create_result: Arc<Mutex<Option<Result<PaneHandle, String>>>>,
        create_calls: AtomicUsize,
        /// Every `PaneRequest` this backend's `create_pane` actually
        /// received, in call order -- the retry test needs this to
        /// prove which PLACEMENT a candidate was asked for, not just that
        /// it was asked. `Arc` so a test can hold its own handle to read
        /// this back AFTER the backend itself has been boxed and moved
        /// into the registry.
        create_requests: Arc<Mutex<Vec<PaneRequest>>>,
        close_calls: Mutex<Vec<PaneHandle>>,
        /// What [`DisplayBackendTrait::natural_placement`] reports for
        /// this fake -- defaults to the trait's own default (`SplitRight`)
        /// so existing tests are unaffected; the retry test sets
        /// this to something else on the SECOND candidate to prove a
        /// retry re-derives placement per-candidate rather than carrying
        /// the first candidate's requested value forward.
        natural_placement: DisplayPlacement,
    }

    impl FakeBackend {
        fn new(name: &'static str, wire_backend: DisplayBackend, available: bool) -> Self {
            Self {
                name,
                wire_backend,
                available,
                create_result: Arc::new(Mutex::new(None)),
                create_calls: AtomicUsize::new(0),
                create_requests: Arc::new(Mutex::new(Vec::new())),
                close_calls: Mutex::new(Vec::new()),
                natural_placement: DisplayPlacement::SplitRight,
            }
        }

        fn succeeding(self, pane_ref: &str) -> Self {
            self.succeeding_with_placement(pane_ref, DisplayPlacement::SplitRight)
        }

        /// Like [`Self::succeeding`], but lets a test control the *actual*
        /// placement the fake reports -- the honest-reporting tests
        /// need this to differ from whatever was requested.
        fn succeeding_with_placement(self, pane_ref: &str, placement: DisplayPlacement) -> Self {
            *self.create_result.lock() = Some(Ok(PaneHandle {
                backend: self.wire_backend,
                pane_ref: pane_ref.to_string(),
                placement,
            }));
            self
        }

        fn failing(self, message: &str) -> Self {
            *self.create_result.lock() = Some(Err(message.to_string()));
            self
        }

        /// Overrides this fake's `natural_placement()` away from the
        /// trait default.
        fn with_natural_placement(mut self, placement: DisplayPlacement) -> Self {
            self.natural_placement = placement;
            self
        }

        /// A handle to this fake's received-request log, cloneable BEFORE
        /// the fake itself is boxed and moved into a [`DisplayRegistry`].
        fn create_requests_handle(&self) -> Arc<Mutex<Vec<PaneRequest>>> {
            Arc::clone(&self.create_requests)
        }

        /// A handle to this fake's create-pane outcome, cloneable BEFORE
        /// the fake is boxed and moved into a [`DisplayRegistry`], so a
        /// test can flip a backend from failing to succeeding (or back)
        /// between `attach()` calls on the SAME coordinator.
        fn create_result_handle(&self) -> Arc<Mutex<Option<Result<PaneHandle, String>>>> {
            Arc::clone(&self.create_result)
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

        fn natural_placement(&self) -> DisplayPlacement {
            self.natural_placement
        }

        fn create_pane(&self, req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
            self.create_calls.fetch_add(1, Ordering::Relaxed);
            self.create_requests.lock().push(req);
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
            crate::security::redaction::Redactor::new(),
        )
    }

    fn attach_request(forced_backend: Option<DisplayBackend>) -> PaneAttachRequest {
        PaneAttachRequest {
            run_id: RunId::new(),
            worker_id: WorkerId::new(),
            adapter: "claude".to_string(),
            placement: DisplayPlacement::SplitRight,
            forced_backend,
            launch_program: None,
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

    /// The outcome (and the journaled event) must reflect what the
    /// backend actually did, not what was requested -- `OsWindowDisplay`
    /// can report `Window` for a `Tab` request, and this must be visible
    /// all the way out through `PaneAttachOutcome`, not silently
    /// overwritten by `req.placement`.
    #[tokio::test]
    async fn attach_reports_the_backends_actual_placement_not_the_requested_one() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("osWindow", DisplayBackend::OsWindow, true)
                .succeeding_with_placement("tab 1 of window id 1", DisplayPlacement::Window),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);

        let mut req = attach_request(None);
        req.placement = DisplayPlacement::Tab; // requested a tab...
        let outcome = coordinator.attach(req).await;

        // ...but the backend actually opened a window, and the outcome
        // must say so.
        assert_eq!(outcome.placement, DisplayPlacement::Window);

        let envelope = events_rx.try_recv().expect("attach must broadcast");
        match &envelope.event {
            crew_protocol::RuntimeEvent::DisplayEvent { placement, .. } => {
                assert_eq!(*placement, DisplayPlacement::Window);
            }
            other => panic!("expected a DisplayEvent, got {other:?}"),
        }

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

    /// Protects the mechanism `with_force_hidden_displays` implements,
    /// not any particular caller. A real, available, WOULD-succeed
    /// herdr backend is
    /// registered specifically so this test can prove `create_pane` was
    /// never even called on it -- not just that the outcome happened to
    /// be `Hidden` (which a coincidentally-failing candidate could also
    /// produce). This is the "one unit test on the threading itself"
    /// staff asked for: if a future `attach()` refactor drops this
    /// check, this is what goes red, rather than every caller quietly
    /// resuming real pane creation with nothing failing.
    #[tokio::test]
    async fn attach_forces_hidden_when_visible_displays_are_disabled() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        let herdr = FakeBackend::new("herdr", DisplayBackend::Herdr, true).succeeding("w1:p2");
        let herdr_requests = herdr.create_requests_handle();
        registry.register(Box::new(herdr));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator =
            coordinator(registry, Arc::clone(&db), events_tx).with_force_hidden_displays(true);

        // Auto (`None`) would normally try herdr first; an explicit
        // request for herdr must be overridden just as hard.
        for forced in [None, Some(DisplayBackend::Herdr)] {
            let outcome = coordinator.attach(attach_request(forced)).await;
            assert_eq!(outcome.backend, DisplayBackend::Hidden);
            assert!(
                herdr_requests.lock().is_empty(),
                "a disabled display must never even be asked to create a pane"
            );
        }

        // The first attach (forced=None, i.e. Auto) must have journaled a
        // typed PaneDowngraded naming herdr as what it would have tried --
        // never silent, per invariant 5.
        let mut saw_downgrade = false;
        while let Ok(envelope) = events_rx.try_recv() {
            if let crew_protocol::RuntimeEvent::PaneDowngraded {
                requested_backend,
                actual_backend,
                reason,
                ..
            } = &envelope.event
            {
                saw_downgrade = true;
                assert_eq!(*requested_backend, DisplayBackend::Herdr);
                assert_eq!(*actual_backend, DisplayBackend::Hidden);
                assert!(
                    reason.as_str().contains("CREW_FORCE_HIDDEN_DISPLAYS"),
                    "reason must name the actual cause, not just say something failed: {}",
                    reason.as_str()
                );
                // The whole chain, not just the first candidate: under
                // `Auto` every one of these would actually have been
                // tried, and an operator reading only "Herdr" would look
                // in the wrong place if tmux or the OS window backend
                // was really what mattered here.
                assert!(
                    reason.as_str().contains("Tmux") && reason.as_str().contains("OsWindow"),
                    "reason must name the whole candidate chain, not just the first: {}",
                    reason.as_str()
                );
            }
        }
        assert!(
            saw_downgrade,
            "expected a PaneDowngraded event to be broadcast"
        );

        db.shutdown().await.expect("shutdown database");
    }

    /// A daemon whose config already says `Hidden` is not being
    /// overridden by anything -- no downgrade to report.
    #[tokio::test]
    async fn attach_with_hidden_already_configured_journals_no_downgrade_when_forced() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let registry = DisplayRegistry::new();
        let coordinator =
            coordinator(registry, Arc::clone(&db), events_tx).with_force_hidden_displays(true);

        let outcome = coordinator
            .attach(attach_request(Some(DisplayBackend::Hidden)))
            .await;
        assert_eq!(outcome.backend, DisplayBackend::Hidden);

        while let Ok(envelope) = events_rx.try_recv() {
            assert!(
                !matches!(
                    envelope.event,
                    crew_protocol::RuntimeEvent::PaneDowngraded { .. }
                ),
                "config already asked for Hidden -- nothing was overridden, nothing to report"
            );
        }

        db.shutdown().await.expect("shutdown database");
    }

    /// The retry loop's own trap, caught before it shipped: `req.placement` is a
    /// concrete value already resolved for the FIRST candidate (herdr's
    /// natural form here, `SplitRight`, and irrelevant to this test only
    /// because we never ask herdr to honor it). A naive retry that just
    /// carries `req.placement` into the next candidate would ask tmux for
    /// a placement that was never really tmux's -- exactly the backend/
    /// placement mismatch ADR-0029 exists to prevent. The fix re-derives
    /// placement from EACH candidate's own `natural_placement()` on every
    /// retry, so tmux is asked for its own natural form (`Tab`, forced
    /// here to differ from both the trait default and the request),
    /// never the value that was only ever meaningful for herdr.
    #[tokio::test]
    async fn a_retry_re_derives_placement_from_the_next_candidates_own_natural_form() {
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).failing("herdr exploded"),
        ));
        let tmux = FakeBackend::new("tmux", DisplayBackend::Tmux, true)
            .with_natural_placement(DisplayPlacement::Tab)
            .succeeding("%7");
        let tmux_requests = tmux.create_requests_handle();
        registry.register(Box::new(tmux));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);

        let mut req = attach_request(None);
        // Explicit and deliberately unlike either candidate's natural
        // form, so a leaked value is unmistakable in the assertion below.
        req.placement = DisplayPlacement::Workspace;
        let outcome = coordinator.attach(req).await;

        assert_eq!(outcome.backend, DisplayBackend::Tmux);
        {
            let received = tmux_requests.lock();
            assert_eq!(
                received.len(),
                1,
                "tmux's create_pane must have been tried exactly once"
            );
            assert_eq!(
                received[0].placement,
                DisplayPlacement::Tab,
                "a retry must ask the new candidate for ITS OWN natural placement, never the \
                 value carried from the failed first candidate's request"
            );
        }
        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn every_candidate_failing_journals_a_typed_pane_downgraded_event_and_falls_back_to_hidden()
     {
        // This used to journal a free-text `Diagnostic` -- the exact
        // "durable condition on an ephemeral channel" bug typed events
        // exist to close. A listener now gets typed fields instead of
        // a message meant for a human to read.
        //
        // Renamed from "a create_pane failure ... falls back to
        // hidden" -- a single failure no longer falls back to hidden, it
        // retries the next candidate (see
        // `a_retry_re_derives_placement_from_the_next_candidates_own_natural_form`
        // and `a_later_candidate_succeeding_still_journals_the_downgrade`).
        // This registry has only herdr and hidden, so tmux/os_window are
        // simply never available -- every REAL candidate fails or is
        // unavailable, which is exactly the "hidden is the last resort"
        // case this test now covers.
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
        let request = attach_request(None);
        let expected_run_id = request.run_id;

        let outcome = coordinator.attach(request).await;

        assert_eq!(outcome.backend, DisplayBackend::Hidden);
        assert_eq!(outcome.pane_ref, "");

        // PaneDowngraded, then the Hidden DisplayPaneAttached -- both
        // broadcast, in that order.
        let downgraded = events_rx.try_recv().expect("PaneDowngraded must broadcast");
        match downgraded.event {
            crew_protocol::RuntimeEvent::PaneDowngraded {
                run_id,
                requested_backend,
                requested_placement,
                actual_backend,
                attempted,
                reason,
            } => {
                assert_eq!(run_id, expected_run_id);
                assert_eq!(requested_backend, DisplayBackend::Herdr);
                assert_eq!(requested_placement, DisplayPlacement::SplitRight);
                assert_eq!(actual_backend, DisplayBackend::Hidden);
                // herdr fails, so the retry walks the rest of
                // the default chain -- tmux and os_window are never
                // registered here, so resolve() finds them unavailable
                // and keeps walking, landing on hidden. `attempted` is
                // the FULL sequence, not just herdr.
                assert_eq!(
                    attempted,
                    vec![
                        DisplayBackend::Herdr,
                        DisplayBackend::Tmux,
                        DisplayBackend::OsWindow,
                        DisplayBackend::Hidden,
                    ],
                    "attempted must carry every backend the retry walked, not just the first"
                );
                assert!(
                    reason.as_str().contains("herdr exploded"),
                    "reason must carry the underlying create_pane error: {reason:?}"
                );
            }
            other => panic!("expected PaneDowngraded, got {other:?}"),
        }
        let attached = events_rx.try_recv().expect("hidden attach must broadcast");
        assert!(is_display_event(
            &attached.event,
            crew_protocol::RuntimeEventKind::DisplayPaneAttached
        ));

        db.shutdown().await.expect("shutdown database");
    }

    /// A `PaneDowngraded` fires whenever the actual
    /// backend diverges from the one first requested, EVEN THOUGH a
    /// later candidate succeeded -- a run landing in tmux when herdr was
    /// preferred is a divergence an operator needs to see, and staying
    /// silent on it (because the run technically got a real pane) would
    /// hide exactly the case `PaneDowngraded` exists to surface.
    #[tokio::test]
    async fn a_later_candidate_succeeding_still_journals_the_downgrade() {
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).failing("herdr exploded"),
        ));
        registry.register(Box::new(
            FakeBackend::new("tmux", DisplayBackend::Tmux, true).succeeding("%3"),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);

        let outcome = coordinator.attach(attach_request(None)).await;

        // The second candidate actually gets the pane.
        assert_eq!(outcome.backend, DisplayBackend::Tmux);
        assert_eq!(outcome.pane_ref, "%3");

        // A PaneDowngraded still fires, naming herdr as requested and
        // tmux as actual -- not silence just because SOMETHING succeeded.
        let downgraded = events_rx
            .try_recv()
            .expect("a PaneDowngraded must broadcast even though attach ultimately succeeded");
        match downgraded.event {
            crew_protocol::RuntimeEvent::PaneDowngraded {
                requested_backend,
                actual_backend,
                attempted,
                reason,
                ..
            } => {
                assert_eq!(requested_backend, DisplayBackend::Herdr);
                assert_eq!(actual_backend, DisplayBackend::Tmux);
                assert_eq!(attempted, vec![DisplayBackend::Herdr, DisplayBackend::Tmux]);
                assert!(reason.as_str().contains("herdr exploded"));
            }
            other => panic!("expected PaneDowngraded, got {other:?}"),
        }
        let attached = events_rx
            .try_recv()
            .expect("the successful tmux attach must also broadcast");
        assert!(is_display_event(
            &attached.event,
            crew_protocol::RuntimeEventKind::DisplayPaneAttached
        ));

        db.shutdown().await.expect("shutdown database");
    }

    /// The reservation is held once for the whole
    /// attach and released only if EVERY candidate fails -- proven here
    /// by two REAL candidates (not just one, as
    /// `a_failed_pane_creation_does_not_hold_a_slot` already covers)
    /// both failing, landing on hidden, and a fresh attach against the
    /// same cap still finding it entirely free.
    #[tokio::test]
    async fn no_slot_is_held_when_every_real_candidate_fails() {
        // Deliberately the SAME coordinator (and so the same cap state)
        // for both attaches: a second, independently-constructed
        // coordinator has its own fresh `live_panes` set regardless of
        // what the first one reserved or released, so comparing against
        // one would prove nothing about release actually happening --
        // exactly the shape that made this test vacuous before this
        // fix (mutation-caught: deleting the release-on-hidden-landing
        // line left this assertion passing anyway).
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).failing("herdr exploded"),
        ));
        let tmux = FakeBackend::new("tmux", DisplayBackend::Tmux, true).failing("tmux exploded");
        let tmux_result = tmux.create_result_handle();
        registry.register(Box::new(tmux));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx).with_max_live_panes(1);

        let first = coordinator.attach(attach_request(None)).await;
        assert_eq!(
            first.backend,
            DisplayBackend::Hidden,
            "both real candidates fail, so the first attach lands on hidden"
        );

        // Let tmux succeed now, on the SAME coordinator/cap: if the first
        // attach's reservation was released, this attach (a different
        // run) still has room for a real pane.
        *tmux_result.lock() = Some(Ok(PaneHandle {
            backend: DisplayBackend::Tmux,
            pane_ref: "%9".to_string(),
            placement: DisplayPlacement::SplitRight,
        }));
        let second = coordinator.attach(attach_request(None)).await;
        assert_eq!(
            second.backend,
            DisplayBackend::Tmux,
            "the cap must have room for a real pane -- the all-failed first attach must not \
             have left its reservation held"
        );

        db.shutdown().await.expect("shutdown database");
    }

    #[tokio::test]
    async fn a_create_pane_failures_secret_shaped_stderr_is_actually_redacted_before_journaling() {
        // `PaneDowngraded.reason` embeds subprocess
        // stderr -- tmux/herdr's own error text, never runtime-authored --
        // and a `Redacted` type alone only proves *some* sanitization
        // happened, not that it actually masked anything. This drives a
        // real secret-shaped substring through the real `create_pane`
        // failure path and asserts the JOURNALED reason has it masked,
        // not just that the field's type claims sanitization occurred.
        let (db, _dir) = harness().await;
        let (events_tx, mut events_rx) = broadcast::channel(16);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(
            FakeBackend::new("herdr", DisplayBackend::Herdr, true).failing(
                "tmux exited with error: sk-ant-api03-thisisafaketokenthatlooksrealbutisnot",
            ),
        ));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx);

        let _ = coordinator.attach(attach_request(None)).await;

        let downgraded = events_rx.try_recv().expect("PaneDowngraded must broadcast");
        match downgraded.event {
            crew_protocol::RuntimeEvent::PaneDowngraded { reason, .. } => {
                assert!(
                    !reason.as_str().contains("sk-ant-api03-"),
                    "the journaled reason must never carry an unredacted API-key-shaped \
                     substring: {reason:?}"
                );
                // Absence of the secret is
                // satisfied just as well by an EMPTY reason -- including via
                // the `None` branch that yields `from_sanitized(String::new())`.
                // Asserting the surrounding message survived turns "no secret"
                // into "masked selectively", which is what the comment above
                // actually claims.
                assert!(
                    reason.as_str().contains("tmux exited with error"),
                    "the redactor must mask the key and KEEP the message, not blank the \
                     field: {reason:?}"
                );
            }
            other => panic!("expected PaneDowngraded, got {other:?}"),
        }

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
    // ------------------------------ live-pane cap

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

    /// The pane cap is about SCREEN REAL ESTATE, not concurrency. A run
    /// that resolves to hidden occupies no screen, so it must neither
    /// consume the cap nor be refused by it -- otherwise a hidden-display
    /// setup would be limited for no reason, and (worse) the cap would
    /// look like a bound on live sessions when it is not. That bound is
    /// the registry's live-session cap.
    #[tokio::test]
    async fn a_hidden_pane_neither_consumes_nor_is_refused_by_the_cap() {
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let mut registry = DisplayRegistry::new();
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx).with_max_live_panes(1);

        // Three hidden attaches against a cap of one: all succeed, because
        // none of them is a pane.
        for _ in 0..3 {
            let outcome = coordinator
                .attach(attach_request(Some(DisplayBackend::Hidden)))
                .await;
            assert_eq!(outcome.backend, DisplayBackend::Hidden);
        }

        db.shutdown().await.expect("shutdown database");
    }

    /// A backend failure falls back to hidden, and must not leave a
    /// reservation behind for a pane that does not exist.
    #[tokio::test]
    async fn a_failed_pane_creation_does_not_hold_a_slot() {
        // Deliberately the SAME coordinator (and so the same cap state)
        // for both attaches -- a second, independently-constructed
        // coordinator has its own fresh `live_panes` set regardless of
        // what the first one reserved or released, so comparing against
        // one proves nothing about release actually happening. Found via
        // mutation testing (a sibling test had the identical
        // shape); fixed here rather than left standing next to it.
        let (db, _dir) = harness().await;
        let (events_tx, _events_rx) = broadcast::channel(64);
        let mut registry = DisplayRegistry::new();
        let tmux = FakeBackend::new("tmux", DisplayBackend::Tmux, true).failing("tmux exploded");
        let tmux_result = tmux.create_result_handle();
        registry.register(Box::new(tmux));
        registry.register(Box::new(super::super::HiddenDisplay::new(
            DisplayConfig::default(),
        )));
        let coordinator = coordinator(registry, Arc::clone(&db), events_tx).with_max_live_panes(1);

        let failed = coordinator.attach(attach_request(None)).await;
        assert_eq!(failed.backend, DisplayBackend::Hidden);

        // The cap of 1 must still be entirely free on this SAME
        // coordinator: let tmux succeed now, and a different run gets a
        // real pane.
        *tmux_result.lock() = Some(Ok(PaneHandle {
            backend: DisplayBackend::Tmux,
            pane_ref: "%4".to_string(),
            placement: DisplayPlacement::SplitRight,
        }));
        assert_eq!(
            coordinator.attach(attach_request(None)).await.backend,
            DisplayBackend::Tmux,
            "the failed attach's reservation must not still be held"
        );

        db.shutdown().await.expect("shutdown database");
    }
}
