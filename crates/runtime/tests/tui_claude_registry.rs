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
use crew_runtime::adapter::tui::TuiTimings;
use crew_runtime::adapter::{
    AdapterKind, AdapterMode as ProfileAdapterMode, AdapterRegistry, ClaudeStartupOptions,
    FixtureAuthorization, StartupOptions, TuiSupport, WorkerProfile,
};
use crew_runtime::config::crew::{
    AdapterConfig, AdapterMode as CrewAdapterMode, CloseOnExit, PermissionMode,
};
use crew_runtime::db::DatabaseHandle;
use crew_runtime::display::{DisplayRegistry, HiddenDisplay};
use crew_runtime::policy::ViolationService;
use crew_runtime::service::{RunDriver, RunDriverContext};
use crew_runtime::supervisor::EscalationTimings;

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
while IFS= read -r line; do
  case "$line" in
    *"[crew:"*)
      SESSION_ID="11111111-1111-4111-8111-000000000099"
      TRANSCRIPT="{session_dir}/$SESSION_ID.jsonl"
      printf '%s\n' '{{"type":"user","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":"'"$line"'"}}}}' >> "$TRANSCRIPT"
      printf '%s\n' '{{"type":"assistant","sessionId":"'"$SESSION_ID"'","timestamp":"2026-01-01T00:00:00Z","message":{{"content":[{{"type":"text","text":"hi from the fixture e2e"}}]}}}}' >> "$TRANSCRIPT"
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

/// Rider: collapse the double `DisplayPaneDetached`. Before the registry
/// threading this test file exercises, a TUI run reachable through
/// `AdapterRegistry` would get *two* `DisplayPaneDetached` events for one
/// run: `TuiAdapter`'s own exit watcher journals the real one (via
/// `PaneCoordinator::detach`), and `watch_settlement`'s placeholder-pane
/// mechanism (driven by `ctx.display`, which `run/submit` populates for
/// every run whenever a backend resolves at submit time, headless
/// included) would journal a second, placeholder one for the same run.
/// This asserts exactly one, with `ctx.display` deliberately set to
/// `Some` so the placeholder path is actually armed and would fire if
/// the collapsing fix regressed.
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
    // counting DisplayPaneDetached occurrences.
    let mut detached_count = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events_rx.recv()).await {
            Ok(Ok(envelope)) => {
                if matches!(
                    envelope.event,
                    crew_protocol::RuntimeEvent::DisplayEvent {
                        kind: crew_protocol::RuntimeEventKind::DisplayPaneDetached,
                        ..
                    }
                ) {
                    detached_count += 1;
                }
            }
            _ => break,
        }
    }

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
