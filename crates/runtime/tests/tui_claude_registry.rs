//! End-to-end proof that `AdapterRegistry` actually reaches the TUI path
//! for a Claude worker profile with `mode: "tui"`: submits a run through
//! the real `RunDriver::start` seam, with `AdapterConfig.bin` pointed at
//! a fake `/bin/sh` script (never the real `claude` CLI) so this stays a
//! zero-model-call, no-real-vendor test -- mirroring
//! `tests/adapter_registry.rs`'s own harness and `tests/tui_adapter.rs`'s
//! own mock-PTY-script style.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crew_protocol::{ProjectId, RunId, TaskId, WorkerId};
use crew_runtime::adapter::tui::{Cursor, TuiTimings};
use crew_runtime::adapter::{
    AdapterKind, AdapterMode as ProfileAdapterMode, AdapterRegistry, ClaudeStartupOptions,
    FixtureAuthorization, ResumeSupport, StartupOptions, TuiSupport, VendorSessionRef,
    WorkerProfile,
};
use crew_runtime::config::crew::{
    AdapterConfig, AdapterMode as CrewAdapterMode, CloseOnExit, PermissionMode,
};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::display::{DisplayRegistry, HiddenDisplay};
use crew_runtime::policy::ViolationService;
use crew_runtime::service::{RunDriver, RunDriverContext};
use crew_runtime::supervisor::EscalationTimings;

use crew_runtime::display::{DisplayBackendTrait, DisplayFuture, PaneHandle, PaneRequest};

/// A pane backend that always succeeds with a non-empty pane ref --
/// mirrors `tests/tui_adapter.rs`'s own fixture. The pairing test below
/// registers this ahead of `HiddenDisplay` precisely so the *real*
/// attach/detach pair carries a non-empty `pane_ref`: that is what makes
/// an "empty pane_ref" assertion able to catch a placeholder event
/// leaking into a TUI run's stream at all (a `Hidden` backend's own
/// legitimate refs are empty and would mask it).
struct FakePaneBackend;

impl DisplayBackendTrait for FakePaneBackend {
    fn backend_name(&self) -> &str {
        "tmux"
    }

    fn is_available(&self) -> bool {
        true
    }

    fn activate(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn status(&self) -> crew_protocol::DisplayStatus {
        crew_protocol::DisplayStatus::new(crew_protocol::DisplayBackend::Tmux, true, false)
    }

    fn create_pane(&self, _req: PaneRequest) -> DisplayFuture<'_, PaneHandle> {
        let handle = PaneHandle {
            backend: crew_protocol::DisplayBackend::Tmux,
            pane_ref: "fake-pane-1".to_string(),
        };
        Box::pin(async move { Ok(handle) })
    }

    fn close_pane(&self, _handle: &PaneHandle) -> DisplayFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

async fn harness() -> (Arc<DatabaseHandle>, tempfile::TempDir, ProjectId) {
    let dir = tempfile::Builder::new()
        .prefix("bat-tui-registry-")
        .tempdir_in("/tmp")
        .unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(DatabaseHandle::start(db_path).await.unwrap());
    (db, dir, ProjectId::new())
}

fn claude_tui_profile() -> WorkerProfile {
    WorkerProfile {
        id: crew_runtime::adapter::ProfileId::new(),
        adapter: AdapterKind::Claude.wire_name().to_string(),
        model: String::new(),
        permission_envelope: serde_json::Value::Object(serde_json::Map::new()),
        startup_options: StartupOptions::Claude(ClaudeStartupOptions {
            mode: ProfileAdapterMode::Tui,
            ..ClaudeStartupOptions::default()
        }),
        environment_allowlist: Vec::new(),
        source: "test".to_string(),
    }
}

/// Mirrors `tests/adapter_registry.rs`'s own `seed_worker_and_run`
/// exactly (raw SQL, same shape) -- going through the full domain-
/// repository event pipeline is unnecessary for a registry test
/// fixture.
async fn seed_worker_and_run(
    db: &Arc<DatabaseHandle>,
    project_id: ProjectId,
    profile: &WorkerProfile,
) -> (RunId, TaskId, WorkerId) {
    let task_id = TaskId::new();
    let worker_id = WorkerId::new();
    let run_id = RunId::new();
    let profile_row_id = WorkerId::new().to_string();
    let resolved_profile_json = serde_json::to_string(profile).unwrap();
    db.run_domain_op(Box::new(move |conn| {
        conn.execute(
            "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, 1, ?4, ?4)",
            rusqlite::params![task_id.to_string(), project_id.to_string(), "test-owner", "2026-01-01T00:00:00Z"],
        )?;
        conn.execute(
            "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope)
             VALUES (?1, 'sha256:test', 'claude', 'test-model', '{}')",
            rusqlite::params![profile_row_id],
        )?;
        conn.execute(
            "INSERT INTO workers (worker_id, project_id, profile_id, resolved_profile_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                worker_id.to_string(),
                project_id.to_string(),
                profile_row_id,
                resolved_profile_json,
                "2026-01-01T00:00:00Z",
            ],
        )?;
        conn.execute(
            "INSERT INTO runs (run_id, task_id, worker_id, state, created_at)
             VALUES (?1, ?2, ?3, 'queued', ?4)",
            rusqlite::params![run_id.to_string(), task_id.to_string(), worker_id.to_string(), "2026-01-01T00:00:00Z"],
        )?;
        Ok(serde_json::Value::Null)
    }))
    .await
    .unwrap();
    (run_id, task_id, worker_id)
}

fn ctx(
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    prompt: &str,
) -> RunDriverContext {
    ctx_with_display(db, project_id, run_id, task_id, worker_id, prompt, None).0
}

/// Like [`ctx`], but also accepts `display` (the placeholder-pane
/// selection `run/submit` would have resolved at the orchestration
/// level) and returns the context's own `events_tx` subscribed
/// receiver, so a caller can observe every `RuntimeEvent` this run
/// journals for itself -- including, for the double-detach rider test,
/// counting `DisplayPaneDetached` occurrences.
fn ctx_with_display(
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    prompt: &str,
    display: Option<crew_protocol::DisplaySelection>,
) -> (
    RunDriverContext,
    tokio::sync::broadcast::Receiver<crew_protocol::EventEnvelope>,
) {
    let (events_tx, events_rx) = tokio::sync::broadcast::channel(100);
    let violation_service = Arc::new(ViolationService::new(
        Arc::clone(&db),
        project_id,
        events_tx.clone(),
        None,
        crew_runtime::config::NestedViolationAction::default(),
    ));
    (
        RunDriverContext {
            db,
            project_id,
            run_id,
            task_id,
            worker_id,
            events_tx,
            prompt: Some(prompt.to_string()),
            violation_service,
            workspace_path: None,
            policy: None,
            display,
        },
        events_rx,
    )
}

/// A fake `claude` binary: prints a ready line, then on receiving a
/// line containing `[crew:` on stdin (the injected nonce), writes one
/// real-shaped session JSONL entry -- embedding the exact received line
/// so nonce-discovery's own content scan finds it -- to `session_dir`.
/// Reads directly with a plain `read -r line` loop, exactly like
/// `tests/tui_adapter.rs`'s own mock scripts: a pty left in its default
/// canonical mode translates a received `\r`
/// (`ClaudeTuiVendor::compose_input`'s own submit convention) to `\n`
/// at the tty-driver level before this process's `read` ever sees it, so
/// no explicit `\r`-to-`\n` translation belongs in the script itself --
/// piping through an external `tr` here previously deadlocked instead
/// (its stdout block-buffers once it is not itself a tty, so the
/// injected line's translated bytes sat in `tr`'s buffer forever with
/// the pty never closed to flush them).
fn write_fake_claude_script(
    scripts_dir: &std::path::Path,
    session_dir: &std::path::Path,
) -> PathBuf {
    let script = format!(
        r#"#!/bin/sh
echo "Welcome to Claude Code!"
SESSION_ID="11111111-1111-4111-8111-000000000099"
TRANSCRIPT="{session_dir}/$SESSION_ID.jsonl"
if [ "$1" = "--resume" ]; then
  # Resumed continuation (WP14): nothing may ever be written to our stdin.
  # Append one fresh post-resume entry so the tailer has something new to
  # journal, then keep reading: if a prompt IS injected anyway, that is an
  # injection regression -- surface it in the transcript as INJECTED text
  # the test can assert the absence of.
  (
    sleep 0.3
    printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:30Z","message":{{"content":[{{"type":"text","text":"post-resume answer"}}]}}}}' >> "$TRANSCRIPT"
  ) &
  while IFS= read -r line; do
    case "$line" in
      *"[crew:"*)
        printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:31Z","message":{{"content":[{{"type":"text","text":"INJECTED ON RESUME"}}]}}}}' >> "$TRANSCRIPT"
        ;;
    esac
  done
  exit 0
fi
while IFS= read -r line; do
  case "$line" in
    *"[crew:"*)
      SESSION_ID="11111111-1111-4111-8111-000000000099"
      TRANSCRIPT="{session_dir}/$SESSION_ID.jsonl"
      printf '%s\n' '{{"type":"user","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":"'"$line"'"}}}}' >> "$TRANSCRIPT"
      printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:01Z","message":{{"content":[{{"type":"text","text":"hi from the fixture e2e"}}]}}}}' >> "$TRANSCRIPT"
      printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","uuid":"entry-tool-1","timestamp":"2026-01-01T00:00:02Z","message":{{"content":[{{"type":"tool_use","name":"Bash","id":"toolu_1","input":{{"command":"ls"}}}}]}}}}' >> "$TRANSCRIPT"
      ;;
  esac
done
"#,
        session_dir = session_dir.display(),
    );
    let path = scripts_dir.join("fake-claude.sh");
    std::fs::write(&path, script).expect("write fake claude script");
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// A `RunDriverContext` the test owns the event sender of, so the very
/// same broadcast the resumed run's sink stack fans out on can also be
/// observed here -- plus a `ViolationService` built on that same channel,
/// which is exactly what production `ResumeSupport` carries.
fn own_ctx(
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    run_id: RunId,
    task_id: TaskId,
    worker_id: WorkerId,
    prompt: &str,
) -> (
    RunDriverContext,
    tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
) {
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);
    let violation_service = Arc::new(ViolationService::new(
        Arc::clone(&db),
        project_id,
        events_tx.clone(),
        None,
        crew_runtime::config::NestedViolationAction::default(),
    ));
    (
        RunDriverContext {
            db: Arc::clone(&db),
            project_id,
            run_id,
            task_id,
            worker_id,
            events_tx: events_tx.clone(),
            prompt: Some(prompt.to_string()),
            violation_service,
            workspace_path: None,
            policy: None,
            display: None,
        },
        events_tx,
    )
}

/// The full registry fixture for resume tests: real `AdapterRegistry`,
/// `TuiSupport` pointed at the fake script, and `ResumeSupport` wired to
/// the same db/project/event channel the submitted run uses.
async fn resume_registry(
    db: &Arc<DatabaseHandle>,
    dir: &tempfile::TempDir,
    project_id: ProjectId,
    events_tx: tokio::sync::broadcast::Sender<crew_protocol::EventEnvelope>,
    script_path: &std::path::Path,
    session_dir: &std::path::Path,
    allow: bool,
) -> AdapterRegistry {
    let mut adapters = BTreeMap::new();
    adapters.insert(
        "claude".to_string(),
        AdapterConfig {
            enabled: true,
            bin: script_path.to_string_lossy().into_owned(),
            mode: CrewAdapterMode::Tui,
            permission_mode: PermissionMode::Default,
            model: None,
            profile: "test".to_string(),
            session_dir: Some(session_dir.to_string_lossy().into_owned()),
            extra_args: Vec::new(),
        },
    );
    let mut display_registry = DisplayRegistry::new();
    display_registry.register(Box::new(HiddenDisplay::new(
        crew_protocol::DisplayConfig::default(),
    )));
    let panes_dir = dir.path().join("panes");
    std::fs::create_dir_all(&panes_dir).expect("create panes dir");

    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow }),
        dir.path().to_path_buf(),
        None,
        vec![],
    );
    registry.set_tui_support(Arc::new(TuiSupport {
        display_registry: Arc::new(display_registry),
        panes_dir,
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: dir.path().to_path_buf(),
        close_on_exit: CloseOnExit::Always,
        forced_backend: None,
        adapters,
        timings: fast_timings(),
    }));
    registry.set_resume_support(Arc::new(ResumeSupport {
        db: Arc::clone(db),
        project_id,
        violation_service: Arc::new(ViolationService::new(
            Arc::clone(db),
            project_id,
            events_tx.clone(),
            None,
            crew_runtime::config::NestedViolationAction::default(),
        )),
        events_tx,
    }));
    registry
}

const SESSION_ID: &str = "11111111-1111-4111-8111-000000000099";

/// One journaled-event count for this run, matched by a raw substring of
/// the stored `event_json`.
async fn journal_count(db: &Arc<DatabaseHandle>, run_id: RunId, marker: &str) -> usize {
    let run_id = run_id.to_string();
    let marker_owned = marker.to_string();
    let value = db
        .run_domain_op(Box::new(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_json LIKE ?2",
                rusqlite::params![run_id, format!("%{marker_owned}%")],
                |row| row.get(0),
            )?;
            Ok(serde_json::json!(count))
        }))
        .await
        .expect("journal count query");
    value.as_i64().expect("count is an integer") as usize
}

/// The stored transcript cursor for this run, parsed back into a `Cursor`
/// exactly the way WP15's sweep will.
async fn stored_cursor(db: &Arc<DatabaseHandle>, run_id: RunId) -> Option<Cursor> {
    let run_id_string = run_id.to_string();
    let value = db
        .run_domain_op(Box::new(move |conn| {
            let json: Option<String> = conn.query_row(
                "SELECT transcript_cursor FROM runs WHERE run_id = ?1",
                [&run_id_string],
                |row| row.get(0),
            )?;
            Ok(serde_json::json!(json))
        }))
        .await
        .expect("cursor read");
    let json = value.as_str()?;
    Some(serde_json::from_str(json).expect("stored cursor must parse"))
}

fn fast_timings() -> TuiTimings {
    TuiTimings {
        readiness_quiet: Duration::from_millis(80),
        readiness_cap: Duration::from_secs(4),
        discovery_timeout: Duration::from_secs(4),
        tailer_poll: Duration::from_millis(40),
        escalation: EscalationTimings {
            sigint_to_sigterm: Duration::from_millis(150),
            sigterm_to_sigkill: Duration::from_millis(150),
        },
    }
}

#[tokio::test]
async fn submitting_a_tui_mode_claude_run_reaches_the_tui_path_and_emits_lifecycle_events() {
    let (db, dir, project_id) = harness().await;
    let profile = claude_tui_profile();
    let (run_id, task_id, worker_id) = seed_worker_and_run(&db, project_id, &profile).await;

    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script_path = write_fake_claude_script(dir.path(), &session_dir);

    let mut adapters = BTreeMap::new();
    adapters.insert(
        "claude".to_string(),
        AdapterConfig {
            enabled: true,
            bin: script_path.to_string_lossy().into_owned(),
            mode: CrewAdapterMode::Tui,
            permission_mode: PermissionMode::Default,
            model: None,
            profile: "test".to_string(),
            session_dir: Some(session_dir.to_string_lossy().into_owned()),
            extra_args: Vec::new(),
        },
    );

    let mut display_registry = DisplayRegistry::new();
    display_registry.register(Box::new(HiddenDisplay::new(
        crew_protocol::DisplayConfig::default(),
    )));

    let panes_dir = dir.path().join("panes");
    std::fs::create_dir_all(&panes_dir).expect("create panes dir");

    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        dir.path().to_path_buf(),
        None,
        vec![],
    );
    registry.set_tui_support(Arc::new(TuiSupport {
        display_registry: Arc::new(display_registry),
        panes_dir,
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: dir.path().to_path_buf(),
        close_on_exit: CloseOnExit::Always,
        forced_backend: None,
        adapters,
        timings: fast_timings(),
    }));

    let result = registry
        .start(ctx(
            Arc::clone(&db),
            project_id,
            run_id,
            task_id,
            worker_id,
            "say hi",
        ))
        .await;
    assert!(
        result.is_ok(),
        "starting a TUI-mode Claude run must succeed: {result:?}"
    );

    // The adapter constructed must be the real TuiAdapter, not a
    // TuiModeUnavailable refusal and not the headless ClaudeAdapter --
    // `kind()` alone doesn't distinguish those, but `capabilities()`
    // does (only `TuiAdapter` declares `ProtocolKind::Terminal`).
    let adapter = registry
        .running_adapter(run_id)
        .expect("the started adapter must be tracked as running");
    assert_eq!(adapter.kind(), "claude");
    assert_eq!(
        adapter.capabilities().protocol,
        crew_runtime::adapter::ProtocolKind::Terminal,
        "expected the real TuiAdapter's capability profile, not the headless ClaudeAdapter's"
    );

    // Lifecycle events actually reached the database: wait for the run
    // to leave `queued`/`starting` (proof the pipeline ran the fake
    // script through spawn -> readiness -> inject -> discovery -> tail,
    // not just that `build_adapter` returned Ok).
    let reached_working_or_further = wait_until(
        || {
            let db = Arc::clone(&db);
            async move {
                db.run_domain_op(Box::new(move |conn| {
                    let state: String = conn.query_row(
                        "SELECT state FROM runs WHERE run_id = ?1",
                        rusqlite::params![run_id.to_string()],
                        |row| row.get(0),
                    )?;
                    Ok(serde_json::Value::String(state))
                }))
                .await
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
            }
        },
        |state: &Option<String>| {
            matches!(
                state.as_deref(),
                Some("working") | Some("succeeded") | Some("failed")
            )
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        reached_working_or_further,
        "expected the run to leave queued/starting once the tui adapter's lifecycle events \
         (ProcessStarted, VendorSessionEstablished, ...) were journaled"
    );

    let _ = adapter.dispose().await;
    db.shutdown().await.expect("shutdown database");
}

/// Rider: collapse the double `DisplayPaneDetached`, and assert the full
/// attach/detach pairing for a TUI run. Before the registry threading
/// this test file exercises, a TUI run reachable through `AdapterRegistry`
/// would get *two* `DisplayPaneDetached` events for one run:
/// `TuiAdapter`'s own exit watcher journals the real one (via
/// `PaneCoordinator::detach`), and `watch_settlement`'s placeholder-pane
/// mechanism (driven by `ctx.display`, which `run/submit` populates for
/// every run whenever a backend resolves at submit time, headless
/// included) would journal a second, placeholder one for the same run.
/// This asserts exactly one real attach, exactly one real detach, and no
/// empty-`pane_ref` (placeholder) event anywhere in the stream, with
/// `ctx.display` deliberately set to `Some` so every placeholder path is
/// actually armed and would fire if either half of the fix regressed --
/// including `start_queued_run`'s submit-time placeholder attach skip,
/// whose end state this stream shape pins from the adapter side.
#[tokio::test]
async fn exactly_one_display_pane_detached_is_journaled_for_a_tui_run() {
    let (db, dir, project_id) = harness().await;
    let profile = claude_tui_profile();
    let (run_id, task_id, worker_id) = seed_worker_and_run(&db, project_id, &profile).await;

    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script_path = write_fake_claude_script(dir.path(), &session_dir);

    let mut adapters = BTreeMap::new();
    adapters.insert(
        "claude".to_string(),
        AdapterConfig {
            enabled: true,
            bin: script_path.to_string_lossy().into_owned(),
            mode: CrewAdapterMode::Tui,
            permission_mode: PermissionMode::Default,
            model: None,
            profile: "test".to_string(),
            session_dir: Some(session_dir.to_string_lossy().into_owned()),
            extra_args: Vec::new(),
        },
    );

    let mut display_registry = DisplayRegistry::new();
    display_registry.register(Box::new(FakePaneBackend));
    display_registry.register(Box::new(HiddenDisplay::new(
        crew_protocol::DisplayConfig::default(),
    )));
    let panes_dir = dir.path().join("panes");
    std::fs::create_dir_all(&panes_dir).expect("create panes dir");

    let registry = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        dir.path().to_path_buf(),
        None,
        vec![],
    );
    registry.set_tui_support(Arc::new(TuiSupport {
        display_registry: Arc::new(display_registry),
        panes_dir,
        crewd_path: PathBuf::from("/opt/crew/bin/crewd"),
        state_dir: dir.path().to_path_buf(),
        close_on_exit: CloseOnExit::Always,
        forced_backend: None,
        adapters,
        timings: fast_timings(),
    }));

    // The placeholder-pane path armed: `run/submit` would have already
    // resolved (and journaled) exactly this selection at submit time,
    // before `RunDriver::start` is ever called.
    let (ctx, mut events_rx) = ctx_with_display(
        Arc::clone(&db),
        project_id,
        run_id,
        task_id,
        worker_id,
        "say hi",
        Some(crew_protocol::DisplaySelection {
            selected: Some(crew_protocol::DisplayBackend::Hidden),
            placement: crew_protocol::DisplayPlacement::SplitRight,
            attempts: vec![crew_protocol::DisplayBackend::Hidden],
        }),
    );

    registry.start(ctx).await.expect("start must succeed");
    let adapter = registry
        .running_adapter(run_id)
        .expect("adapter must be running");

    // Let the run settle (cancel is enough; the exit watcher's own
    // detach + ProcessExited drive `watch_settlement`).
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        adapter.cancel(crew_runtime::adapter::CancelScope::Worker),
    )
    .await;

    // Drain every event this run journaled for a bounded window,
    // counting DisplayPaneAttached/DisplayPaneDetached occurrences and
    // rejecting any empty-pane_ref (placeholder) event outright.
    let mut attached_count = 0usize;
    let mut detached_count = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events_rx.recv()).await {
            Ok(Ok(envelope)) => {
                if let crew_protocol::RuntimeEvent::DisplayEvent { kind, pane_ref, .. } =
                    &envelope.event
                {
                    match kind {
                        crew_protocol::RuntimeEventKind::DisplayPaneAttached => {
                            attached_count += 1;
                        }
                        crew_protocol::RuntimeEventKind::DisplayPaneDetached => {
                            detached_count += 1;
                        }
                        _ => {}
                    }
                    assert!(
                        !pane_ref.is_empty(),
                        "a placeholder pane event leaked into this TUI run's stream: \
                         {kind:?} pane_ref={pane_ref:?}"
                    );
                }
            }
            _ => break,
        }
    }

    assert_eq!(
        attached_count, 1,
        "expected exactly one DisplayPaneAttached for this TUI run -- the real one, \
         never a submit-time placeholder plus it"
    );
    assert_eq!(
        detached_count, 1,
        "expected exactly one DisplayPaneDetached for this TUI run, not a placeholder-plus-real double"
    );

    db.shutdown().await.expect("shutdown database");
}

/// Polls `fetch` until `matches` its result or `timeout` elapses.
async fn wait_until<F, Fut, T>(
    mut fetch: F,
    matches: impl Fn(&T) -> bool,
    timeout: Duration,
) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let value = fetch().await;
        if matches(&value) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

// ------------------------------------------------------------------- WP14

/// The end-to-end resume contract: a crashed TUI run's `resume_run`
/// respawns the vendor via its resume launch, never injects the prompt,
/// re-tails from the cursor the pre-crash run stored (zero duplicate
/// journal entries), reopens a pane, and journals resume-flavored
/// diagnostics. A duplicate `resume_run` while the adapter is live is a
/// typed rejection.
#[tokio::test]
async fn resume_run_continues_a_crashed_tui_run_from_its_stored_cursor() {
    let (db, dir, project_id) = harness().await;
    let profile = claude_tui_profile();
    let (run_id, task_id, worker_id) = seed_worker_and_run(&db, project_id, &profile).await;

    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script_path = write_fake_claude_script(dir.path(), &session_dir);

    let (ctx, events_tx) = own_ctx(
        Arc::clone(&db),
        project_id,
        run_id,
        task_id,
        worker_id,
        "say hi",
    );
    let registry = resume_registry(
        &db,
        &dir,
        project_id,
        events_tx.clone(),
        &script_path,
        &session_dir,
        true,
    )
    .await;

    // ---- phase 1: the original (pre-crash) run
    registry.start(ctx).await.expect("the fresh run must start");

    // Wait until the fixture's greeting is journaled AND the sink
    // has persisted the tailer cursor that covers it.
    let phase1_settled = wait_until(
        || {
            let db = Arc::clone(&db);
            async move {
                journal_count(&db, run_id, "hi from the fixture e2e").await >= 1
                    && stored_cursor(&db, run_id).await.is_some()
                    && journal_count(&db, run_id, "Bash").await >= 1
            }
        },
        |ready| *ready,
        Duration::from_secs(10),
    )
    .await;
    if !phase1_settled {
        let cursor = stored_cursor(&db, run_id).await;
        panic!(
            "phase 1 stalled: greeting={}, Bash={}, cursor={cursor:?}",
            journal_count(&db, run_id, "hi from the fixture e2e").await,
            journal_count(&db, run_id, "Bash").await,
        );
    }

    let pre_greeting = journal_count(&db, run_id, "hi from the fixture e2e").await;
    let pre_bash_started = journal_count(&db, run_id, "adapterToolStarted").await;
    let pre_bash_result = journal_count(&db, run_id, "adapterToolResult").await;
    let pre_attached = journal_count(&db, run_id, "PaneAttached").await;
    assert_eq!(pre_greeting, 1);
    assert!(pre_bash_started >= 1 && pre_bash_result >= 1);
    assert_eq!(
        pre_attached, 1,
        "exactly one pane attach for the fresh start"
    );

    // ---- crash boundary: drop the first incarnation without any new
    // vendor output, exactly like a daemon death leaves nothing new in
    // the transcript.
    let adapter = registry.running_adapter(run_id).expect("running adapter");
    adapter.dispose().await.expect("dispose");
    let evicted = wait_until(
        || {
            let registry = &registry;
            async move { registry.running_count() == 0 }
        },
        |empty| *empty,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        evicted,
        "the crashed incarnation must leave the running map"
    );

    // A second resume while anything is still running would be a duplicate.
    // (Nothing is running here anymore, but the guard itself is proven by
    // the mid-flight rejection further below.)

    // ---- phase 2: resume from the stored cursor
    let session = VendorSessionRef(SESSION_ID.to_string());
    // Duplicate-start guard: while an adapter IS live (never here -- but
    // exercise it right before the real resume against the reserved slot
    // of the concurrent path by simply calling twice; the second call must
    // fail once the first succeeds).
    registry
        .resume_run(run_id, session.clone(), stored_cursor(&db, run_id).await)
        .await
        .expect("resume_run must succeed");

    // The slot is now taken again: a second concurrent resume is a typed
    // duplicate rejection, never two live adapters for one run.
    let duplicate = registry
        .resume_run(run_id, session, None)
        .await
        .expect_err("duplicate resume must be rejected");
    assert!(
        duplicate.contains("already has a running adapter"),
        "expected DuplicateStart, got: {duplicate}"
    );

    // Wait for the post-resume entry the fake script appends on --resume.
    let resumed = wait_until(
        || {
            let db = Arc::clone(&db);
            async move { journal_count(&db, run_id, "post-resume answer").await >= 1 }
        },
        |done| *done,
        Duration::from_secs(10),
    )
    .await;
    assert!(
        resumed,
        "the resumed vendor's fresh output must be journaled"
    );

    // No duplicates: everything the pre-crash run journaled appears exactly
    // as often as before the resume (the stored cursor covers it).
    assert_eq!(
        journal_count(&db, run_id, "hi from the fixture e2e").await,
        pre_greeting,
        "pre-crash message must not be re-journaled"
    );
    assert_eq!(
        journal_count(&db, run_id, "adapterToolStarted").await,
        pre_bash_started,
        "pre-crash tool starts must not be re-journaled"
    );
    assert_eq!(
        journal_count(&db, run_id, "adapterToolResult").await,
        pre_bash_result,
        "pre-crash tool results must not be re-journaled"
    );

    // No prompt injection on resume.
    assert_eq!(
        journal_count(&db, run_id, "INJECTED ON RESUME").await,
        0,
        "resume must never inject the prompt"
    );

    // Pane reopened: exactly one more real attach.
    assert_eq!(
        journal_count(&db, run_id, "PaneAttached").await,
        pre_attached + 1,
        "resume must reopen exactly one pane"
    );

    // Resume-flavored diagnostics.
    assert!(
        journal_count(&db, run_id, "resumed vendor session").await >= 1,
        "a resume diagnostic naming the continued session must be journaled"
    );

    let _ = registry.running_adapter(run_id).unwrap().dispose().await;
    db.shutdown().await.expect("shutdown database");
}

/// WP12 rider, proved at the crash boundary: a batch whose cursor lands on
/// its `ToolResult` can die between the `ToolStarted` commit and the rest
/// of the batch. Resuming with no stored cursor re-tails from byte zero
/// and must tolerate exactly one replayed `ToolStarted` while never
/// double-journaling anything else.
#[tokio::test]
async fn resume_run_tolerates_exactly_one_duplicate_tool_started_at_the_crash_boundary() {
    let (db, dir, project_id) = harness().await;
    let profile = claude_tui_profile();
    let (run_id, task_id, worker_id) = seed_worker_and_run(&db, project_id, &profile).await;

    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script_path = write_fake_claude_script(dir.path(), &session_dir);

    // Pre-crash state, built directly instead of through a live run so the
    // crash point is exact:
    //   * the transcript already contains one full turn (text + tool call),
    //   * the journal contains ONLY the `ToolStarted` half of the tool
    //     activity -- the crash happened after that commit but before the
    //     `ToolResult`-bearing batch advanced the cursor,
    //   * `runs.transcript_cursor` is therefore still NULL.
    let transcript = session_dir.join(format!("{SESSION_ID}.jsonl"));
    std::fs::write(
        &transcript,
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{SESSION_ID}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"message\":{{\"role\":\"user\",\"content\":\"go\"}}}}\n\
             {{\"type\":\"assistant\",\"sessionId\":\"{SESSION_ID}\",\"uuid\":\"e1\",\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Bash\",\"id\":\"t1\",\"input\":{{\"command\":\"ls\"}}}}]}}}}\n"
        ),
    )
    .expect("seed pre-crash transcript");

    {
        let violation_service = Arc::new(ViolationService::new(
            Arc::clone(&db),
            project_id,
            tokio::sync::broadcast::channel(16).0,
            None,
            crew_runtime::config::NestedViolationAction::default(),
        ));
        db.run_domain_op(Box::new(move |conn| {
            let mut repo = crew_runtime::domain::DomainRepository::new(conn, project_id);
            repo.record_adapter_event(
                &crew_protocol::RuntimeEvent::AdapterToolEvent {
                    kind: crew_protocol::RuntimeEventKind::AdapterToolStarted,
                    run_id,
                    task_id,
                    worker_id,
                    tool_call_id: "toolu_pre".to_string(),
                    name: "Bash".to_string(),
                    ok: None,
                    detail: None,
                },
                task_id,
                worker_id,
                run_id,
                None,
            )
            .map(|_| serde_json::Value::Null)
        }))
        .await
        .expect("seed the committed ToolStarted");
        drop(violation_service);
    }

    let (_, events_tx) = own_ctx(
        Arc::clone(&db),
        project_id,
        run_id,
        task_id,
        worker_id,
        "unused",
    );
    let registry = resume_registry(
        &db,
        &dir,
        project_id,
        events_tx,
        &script_path,
        &session_dir,
        true,
    )
    .await;

    // Cursor `None`: the crash left nothing durable to continue from, so
    // the tail legitimately starts at byte zero.
    registry
        .resume_run(run_id, VendorSessionRef(SESSION_ID.to_string()), None)
        .await
        .expect("resume_run must succeed");

    let settled = wait_until(
        || {
            let db = Arc::clone(&db);
            async move {
                journal_count(&db, run_id, "post-resume answer").await >= 1
                    && journal_count(&db, run_id, "adapterToolResult").await >= 1
            }
        },
        |done| *done,
        Duration::from_secs(10),
    )
    .await;
    assert!(settled, "the resumed run must reach its post-resume output");
    // Give the pump a beat to flush the whole replayed batch before
    // counting.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Exactly the tolerated shape: the replayed tool activity produces ONE
    // extra `ToolStarted` (the window across the crash boundary), while
    // the `ToolResult` and the assistant text are journaled once each --
    // nothing else doubled.
    assert_eq!(
        journal_count(&db, run_id, "adapterToolStarted").await,
        2,
        "one pre-crash ToolStarted + exactly one tolerated replay"
    );
    assert_eq!(
        journal_count(&db, run_id, "adapterToolResult").await,
        1,
        "the ToolResult half must not double"
    );
    assert_eq!(
        journal_count(&db, run_id, "post-resume answer").await,
        1,
        "post-resume output journaled once"
    );

    let _ = registry.running_adapter(run_id).unwrap().dispose().await;
    db.shutdown().await.expect("shutdown database");
}

/// Typed refusals around `resume_run`: missing support bundle, unknown
/// run, and authorization denial -- none of which may ever spawn a
/// process or reserve a running slot.
#[tokio::test]
async fn resume_run_refuses_without_support_for_unknown_runs_and_on_denial() {
    let (db, dir, project_id) = harness().await;
    let profile = claude_tui_profile();
    let (run_id, task_id, worker_id) = seed_worker_and_run(&db, project_id, &profile).await;

    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let script_path = write_fake_claude_script(dir.path(), &session_dir);

    let (_, events_tx) = own_ctx(
        Arc::clone(&db),
        project_id,
        run_id,
        task_id,
        worker_id,
        "unused",
    );

    // 1. No support supplied yet -> typed refusal.
    let bare = AdapterRegistry::new(
        Arc::new(FixtureAuthorization { allow: true }),
        dir.path().to_path_buf(),
        None,
        vec![],
    );
    let err = bare
        .resume_run(run_id, VendorSessionRef(SESSION_ID.to_string()), None)
        .await
        .expect_err("resume without support must be refused");
    assert!(
        err.contains("resume support was never supplied"),
        "expected ResumeUnsupported, got: {err}"
    );
    assert_eq!(bare.running_count(), 0, "no slot may leak on refusal");

    let registry = resume_registry(
        &db,
        &dir,
        project_id,
        events_tx,
        &script_path,
        &session_dir,
        true,
    )
    .await;

    // 2. Unknown run -> refusal, no slot leaked.
    let unknown = RunId::new();
    let err = registry
        .resume_run(unknown, VendorSessionRef(SESSION_ID.to_string()), None)
        .await
        .expect_err("an unknown run must be refused");
    assert!(err.contains("unreadable"), "unexpected error shape: {err}");
    assert_eq!(registry.running_count(), 0);

    // 3. Authorization denial -> refusal, no process spawned, no slot.
    let denied = resume_registry(
        &db,
        &dir,
        project_id,
        tokio::sync::broadcast::channel(16).0,
        &script_path,
        &session_dir,
        false,
    )
    .await;
    let err = denied
        .resume_run(run_id, VendorSessionRef(SESSION_ID.to_string()), None)
        .await
        .expect_err("denied resume must be refused");
    assert!(
        err.contains("authorization denied"),
        "expected AuthorizationDenied, got: {err}"
    );
    assert_eq!(denied.running_count(), 0);

    db.shutdown().await.expect("shutdown database");
}
