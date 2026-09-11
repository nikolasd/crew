//! The real [`Adapter`] implementation for `AdapterMode::Protocol`:
//! spawns `claude` with [`super::launch::build_argv`]'s fixed launch
//! argv after [`super::trust::workspace_trust_accepted`] passes, delivers the
//! initial prompt as the first `stream-json` input message, hands its
//! stdout/stdin to [`super::reader::drive_turn`] for the whole turn, and
//! reconciles what that turn saw against claude's own durable transcript
//! via [`super::reconcile::find_gaps`] once it ends.
//!
//! This is the first real caller for three modules this spike built
//! ahead of it: `reconcile::find_gaps` (previously dead code, reachable
//! only from its own tests and the external drop-seam test),
//! `approval_bridge`'s whole module (previously `#![allow(dead_code)]`),
//! and `reader::drive_turn` itself. It is also the first real caller of
//! [`pane::PaneSupport`]: when a `TuiSupport` bundle was ever supplied
//! to the registry, [`Self::attach_pane`] opens a crew-rendered pane for
//! the turn, best-effort -- see `pane`'s own module doc comment for the
//! format and the provenance/redaction notes worth reading before
//! citing this pane's own text as evidence of anything.
//!
//! **Not in scope here** (see this spike's own PR body for the full
//! non-goals list): `resume`/`--continue`, the other three vendors, and
//! performance work.
//!
//! `--model` is threaded through as a plain, already-resolved parameter
//! this adapter does not itself resolve -- it is handed exactly
//! `profile.model`, the run's own resolved model, the same field the
//! fix for `build_tui_adapter`'s own model-resolution gap (that
//! adapter's boot-config snapshot silently outliving a later per-run
//! choice) threads through as `run_model`. There is no boot-loaded
//! per-vendor config for protocol mode to override in the first place
//! (no `crew.json` adapters map is ever read here), so that fix's
//! three-row precedence has nothing to collide with: this adapter's own
//! rule is simply "use it, trimmed, when non-empty; omit `--model`
//! otherwise and let claude's own default apply" -- [`Self::start`]'s
//! own test proves this against a REAL launched process's real argv,
//! not a configured value, mirroring that fix's own reproduction shape.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex as AsyncMutex;

use crew_protocol::{Classified, ContentClass};

use crate::adapter::AdapterFuture;
use crate::adapter::capability::AdapterCapabilities;
use crate::adapter::error::AdapterError;
use crate::adapter::event_sink::{AdapterEvent, AdapterEventPayload, AdapterEventSink};
use crate::adapter::r#trait::{
    Adapter, AdapterMessage, AdapterSnapshot, CancelScope, ProbeResult, StartSpec, VendorSessionRef,
};
use crate::approval::ApprovalService;
use crate::supervisor::EnvironmentPolicy;

use super::approval_bridge::ProtocolApprovalCallback;
use super::reader::{self, RunIdentity};
use super::{pane, reconcile, trust};

/// Everything [`ClaudeProtocolAdapter`] needs that only exists after the
/// daemon's own `ApprovalService` is constructed -- the server-owned
/// instance `approval/decide` dispatches through, and the callback map
/// [`build_argv`]'s caller registered as `ServerConfig::approval_callback`
/// in its place. One instance is shared by every `ClaudeProtocolAdapter`
/// this daemon ever constructs -- an approval is looked up by its own
/// id, never by which adapter instance created it, so sharing one
/// callback map across runs is correct, not merely convenient (see
/// `approval_bridge::ProtocolApprovalCallback`'s own doc comment on why
/// a per-adapter registry keyed by worker id was rejected).
#[derive(Clone)]
pub(crate) struct ProtocolBundle {
    pub(crate) approval_service: Arc<ApprovalService>,
    pub(crate) callback: Arc<ProtocolApprovalCallback>,
}

/// Drives one `claude` process over its `stream-json` control channel.
pub(crate) struct ClaudeProtocolAdapter {
    repo_root: PathBuf,
    /// `WorkerProfile::environmentAllowlist` -- mirrors
    /// `ClaudeTuiVendor`'s own field of the same name exactly.
    environment_allowlist: Vec<String>,
    /// The already-resolved model to launch with, if any -- see this
    /// module's own doc comment on why resolving it is not this
    /// adapter's job.
    model: Option<String>,
    bundle: ProtocolBundle,
    /// Where to read claude's own trust record from --
    /// `trust::default_claude_json_path()` in production; a temp path in
    /// a test. `None` only when `HOME` could not be resolved, in which
    /// case [`Self::start`] treats the workspace as untrusted (fail
    /// closed, never guessed).
    claude_json_path: Option<PathBuf>,
    /// The binary [`Self::start`] spawns -- `"claude"` (resolved via
    /// `PATH`) in production, a fake script's path in a test. A plain
    /// field, injected at construction, rather than a `PATH`-shadowing
    /// trick or a `#[cfg(test)]` branch: the same shape
    /// `AdapterConfig.bin` already gives `ClaudeTuiVendor`, so a test can
    /// prove what actually reached `Command::new` without touching
    /// global process state.
    bin: String,
    /// The live child process, held for the duration of one turn so
    /// [`Adapter::cancel`] can reach it. `None` before the first
    /// [`Adapter::start`] call and after the turn ends.
    child: AsyncMutex<Option<tokio::process::Child>>,
    /// Present only when a `TuiSupport` bundle was ever supplied to the
    /// registry -- see [`pane::PaneSupport`]'s own doc comment. `None`
    /// means this adapter runs with no pane at all, never a refusal to
    /// start.
    pane_support: Option<pane::PaneSupport>,
}

impl ClaudeProtocolAdapter {
    #[must_use]
    pub(crate) fn new(
        repo_root: PathBuf,
        environment_allowlist: Vec<String>,
        model: Option<String>,
        bundle: ProtocolBundle,
        pane_support: Option<pane::PaneSupport>,
    ) -> Self {
        Self {
            repo_root,
            environment_allowlist,
            model,
            bundle,
            claude_json_path: trust::default_claude_json_path(),
            bin: "claude".to_string(),
            child: AsyncMutex::new(None),
            pane_support,
        }
    }

    /// Overrides where [`Self::start`] reads the trust record from and
    /// which binary it spawns -- a test-only seam, never reachable from
    /// production construction (`Self::new` above is the only
    /// `pub(crate)` constructor `super::super::registry` can see).
    #[cfg(test)]
    fn with_test_overrides(mut self, claude_json_path: PathBuf, bin: String) -> Self {
        self.claude_json_path = Some(claude_json_path);
        self.bin = bin;
        self
    }

    fn env(&self) -> std::collections::HashMap<String, String> {
        let current: std::collections::HashMap<String, String> = std::env::vars().collect();
        EnvironmentPolicy::baseline().build(&current, &self.environment_allowlist)
    }

    /// Reconciles what [`reader::drive_turn`] actually saw against
    /// claude's own transcript, journaling the result through `sink`.
    /// Best-effort: a failure here is logged, never propagated -- the
    /// turn itself already completed (or failed) by the time this runs,
    /// and a reconciliation-journaling failure must not turn an
    /// otherwise-successful turn into a failed run.
    async fn reconcile_turn(
        &self,
        canonical_repo_root: &Path,
        outcome: &reader::TurnOutcome,
        sink: &Arc<dyn AdapterEventSink>,
        ids: RunIdentity,
    ) {
        let transcript = outcome.session_id.as_deref().and_then(|session_id| {
            std::fs::read(transcript_path(canonical_repo_root, session_id)).ok()
        });
        let (examined, gaps) = match &transcript {
            Some(bytes) => reconcile::find_gaps(bytes, &outcome.entry_ids),
            None => (0, Vec::new()),
        };
        // `examined == 0` is itself a finding (`reconcile::find_gaps`'s
        // own doc comment): a turn this adapter cannot reconcile at all
        // (no session id ever arrived, the transcript file does not
        // exist yet, or every line in it failed to parse) must never be
        // journaled as an ordinary, successful reconciliation -- it is
        // reported as a distinguishable protocol-health failure instead,
        // never silently folded into `gaps_found: 0`, which would read
        // identically to "checked, and found nothing wrong."
        let payload = if examined == 0 {
            AdapterEventPayload::ProtocolHealthChanged {
                healthy: false,
                detail: Classified {
                    class: ContentClass::Visible,
                    value: "could not reconcile this turn against claude's own transcript \
                            (no entries were readable)"
                        .to_string(),
                },
            }
        } else {
            AdapterEventPayload::ReconciliationCompleted {
                examined,
                gaps_found: gaps.len() as u64,
                // No repair mechanism exists yet -- this adapter only
                // detects and reports a gap, it does not yet re-journal
                // a missed entry. Left at `0` rather than guessed.
                gaps_repaired: 0,
            }
        };
        if let Err(err) = sink
            .emit(AdapterEvent {
                run_id: ids.run_id,
                task_id: ids.task_id,
                worker_id: ids.worker_id,
                payload,
                cursor: None,
            })
            .await
        {
            tracing::warn!(error = %err, run_id = %ids.run_id, "failed to journal claude-protocol reconciliation");
        }
    }

    /// Attaches this run's pane, if [`Self::pane_support`] was ever
    /// supplied -- best-effort: a failure here is logged and this
    /// adapter proceeds with no pane, never fails the turn (see
    /// [`pane::PaneSupport`]'s own doc comment on why this adapter's
    /// pane is a convenience, not a control surface).
    async fn attach_pane(&self, ids: RunIdentity) -> Option<AttachedPane> {
        let support = self.pane_support.as_ref()?;
        let (target, output_tx) = pane::PaneAttachTarget::new();
        let target = Arc::new(target);
        let socket_path = support.panes_dir.join(format!("{}.sock", ids.run_id));
        let attach_server = match crate::display::AttachServer::start(
            socket_path,
            Arc::clone(&target) as Arc<dyn crate::display::AttachTarget>,
            target.on_user_input(),
        ) {
            Ok(server) => server,
            Err(err) => {
                tracing::warn!(error = %err, run_id = %ids.run_id, "failed to start this run's attach server; continuing without a pane");
                return None;
            }
        };
        let outcome = support
            .pane_coordinator
            .attach(crate::display::PaneAttachRequest {
                run_id: ids.run_id,
                worker_id: ids.worker_id,
                adapter: self.kind().to_string(),
                placement: support.placement,
                forced_backend: support.forced_backend,
                launch_program: support.launch_program,
            })
            .await;
        // The banner is the operational form of this module's own
        // provenance note -- a viewer must never be able to read even
        // one line before knowing what this pane is (see
        // `pane::attach_banner`'s own doc comment).
        let _ = output_tx.send(pane::attach_banner());
        Some(AttachedPane {
            attach_server,
            outcome,
            output_tx,
        })
    }

    /// Releases the pane [`Self::attach_pane`] set up, if any. Called on
    /// every exit path from [`Self::start`], success or error.
    async fn detach_pane(&self, pane: AttachedPane, succeeded: bool) {
        if let Some(support) = &self.pane_support {
            support
                .pane_coordinator
                .detach(&pane.outcome, succeeded, support.close_on_exit)
                .await;
        }
        pane.attach_server.stop();
    }
}

/// What [`ClaudeProtocolAdapter::attach_pane`] set up, kept alive for the
/// duration of one turn so [`ClaudeProtocolAdapter::detach_pane`] can
/// release it afterward. `output_tx` is also read from directly, by
/// [`ClaudeProtocolAdapter::start`], to hand `reader::drive_turn` a
/// place to push formatted lines into.
struct AttachedPane {
    attach_server: crate::display::AttachServer,
    outcome: crate::display::PaneAttachOutcome,
    output_tx: tokio::sync::broadcast::Sender<Vec<u8>>,
}

/// `~/.claude/projects/<slug_cwd(canonical_repo_root)>/<session_id>.jsonl`
/// -- the same on-disk layout
/// `crate::adapter::tui::claude::ClaudeTuiVendor::transcript_root`
/// resolves for the TUI path, reused verbatim rather than
/// re-derived: both adapters read the identical vendor-owned directory,
/// just at different times (this one once, after the turn; that one
/// continuously, while it runs). `HOME` unresolved falls back to
/// `/root`, matching that function's own precedent.
fn transcript_path(canonical_repo_root: &Path, session_id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(crate::adapter::tui::claude::slug_cwd(canonical_repo_root))
        .join(format!("{session_id}.jsonl"))
}

impl Adapter for ClaudeProtocolAdapter {
    fn kind(&self) -> &str {
        "claude"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        super::declared_capabilities()
    }

    /// Spawns `claude --version` directly -- no model call, matching
    /// `TuiAdapter::probe`'s own committed shape (a synchronous
    /// `std::process::Command` inside this async fn) exactly, for the
    /// same reason: a probe is inherently a one-shot version check, not
    /// a long-lived stream this adapter needs to hold open.
    fn probe(&self) -> AdapterFuture<'_, ProbeResult> {
        Box::pin(async move {
            let output = std::process::Command::new(&self.bin)
                .arg("--version")
                .output()
                .map_err(|e| AdapterError::unavailable(self.kind(), "probe", e.to_string()))?;
            if !output.status.success() {
                return Err(AdapterError::unavailable(
                    self.kind(),
                    "probe",
                    "claude --version exited non-zero",
                ));
            }
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(ProbeResult {
                version: Some(version),
                auth_ready: true,
                capabilities: self.capabilities(),
                inventory_incomplete: true,
            })
        })
    }

    fn start(&self, spec: StartSpec, sink: Arc<dyn AdapterEventSink>) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let ids = RunIdentity {
                run_id: spec.run_id,
                task_id: spec.task_id,
                worker_id: spec.worker_id,
            };
            let canonical_repo_root =
                std::fs::canonicalize(&self.repo_root).unwrap_or_else(|_| self.repo_root.clone());

            let trusted = self
                .claude_json_path
                .as_deref()
                .is_some_and(|path| trust::workspace_trust_accepted(path, &canonical_repo_root));
            if !trusted {
                sink.emit(AdapterEvent {
                    run_id: ids.run_id,
                    task_id: ids.task_id,
                    worker_id: ids.worker_id,
                    payload: AdapterEventPayload::WorkspaceTrustPending,
                    cursor: None,
                })
                .await?;
                return Err(AdapterError::auth_required(
                    self.kind(),
                    "start",
                    "claude's one-time workspace-trust prompt for this repository has not been \
                     accepted yet; trust it once from an interactive claude session in this \
                     repository, then resubmit",
                ));
            }

            // Attached before the process spawns, so a viewer sees every
            // line from the very first one -- best-effort: unlike a
            // `TuiAdapter`, this adapter's pane is a convenience view, not
            // its control surface, so a failure to attach never fails the
            // turn, only logs and proceeds without one.
            let pane = self.attach_pane(ids).await;
            let pane_output = pane.as_ref().map(|p| &p.output_tx);

            let turn: Result<(), AdapterError> = async {
                let model = self
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty());
                let argv = super::launch::build_argv(model);
                let env = self.env();

                let mut command = tokio::process::Command::new(&self.bin);
                command
                    .args(&argv)
                    .current_dir(&canonical_repo_root)
                    .env_clear()
                    .envs(&env)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                let mut child = command
                    .spawn()
                    .map_err(|e| AdapterError::process(self.kind(), "start", e.to_string()))?;
                let pid = child.id().unwrap_or(0);
                let mut stdin = child
                    .stdin
                    .take()
                    .expect("stdin was requested as piped at spawn");
                let stdout = child
                    .stdout
                    .take()
                    .expect("stdout was requested as piped at spawn");

                let first_message = serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": spec.prompt}],
                    },
                });
                let mut first_line = serde_json::to_string(&first_message)
                    .expect("a constructed value always serializes");
                first_line.push('\n');
                stdin
                    .write_all(first_line.as_bytes())
                    .await
                    .map_err(|e| AdapterError::process(self.kind(), "start", e.to_string()))?;

                *self.child.lock().await = Some(child);

                sink.emit(AdapterEvent {
                    run_id: ids.run_id,
                    task_id: ids.task_id,
                    worker_id: ids.worker_id,
                    payload: AdapterEventPayload::ProcessStarted { pid },
                    cursor: None,
                })
                .await?;

                let outcome = reader::drive_turn(
                    stdout,
                    stdin,
                    &sink,
                    &self.bundle.approval_service,
                    &self.bundle.callback,
                    ids,
                    pane_output,
                )
                .await?;

                let status = {
                    let mut guard = self.child.lock().await;
                    let child = guard.as_mut().ok_or_else(|| {
                        AdapterError::invalid_vendor_state(
                            self.kind(),
                            "start",
                            "the spawned child process handle was missing at reap time",
                        )
                    })?;
                    child
                        .wait()
                        .await
                        .map_err(|e| AdapterError::process(self.kind(), "start", e.to_string()))?
                };
                *self.child.lock().await = None;

                sink.emit(AdapterEvent {
                    run_id: ids.run_id,
                    task_id: ids.task_id,
                    worker_id: ids.worker_id,
                    payload: AdapterEventPayload::ProcessExited {
                        exit_code: status.code(),
                        signal: None,
                    },
                    cursor: None,
                })
                .await?;

                self.reconcile_turn(&canonical_repo_root, &outcome, &sink, ids)
                    .await;

                Ok(())
            }
            .await;

            // Detached on every exit path from here, success or error --
            // `PaneCoordinator::detach` is what releases the live-pane
            // slot and journals `DisplayPaneDetached`; leaving it
            // unreached on an error path would leak both.
            if let Some(pane) = pane {
                self.detach_pane(pane, turn.is_ok()).await;
            }

            turn
        })
    }

    fn resume(
        &self,
        _session: VendorSessionRef,
        _sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Err(AdapterError::capability_unsupported(self.kind(), "resume")) })
    }

    /// Committed scope is one worker, one turn: no steering, no
    /// follow-up, no out-of-band answer delivery yet, matching
    /// `declared_capabilities().steering`'s own `None`.
    fn send(&self, _message: AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Err(AdapterError::capability_unsupported(self.kind(), "send")) })
    }

    /// The real bridge exists (`super::approval_bridge`), reached from
    /// `super::reader::drive_turn` while a turn is live -- this method
    /// is the same trait-level seam every other adapter's own
    /// `respond_to_approval` is (nothing calls it in production;
    /// `ApprovalService::decide` calls `ApprovalCallback::acknowledge`
    /// directly instead, per that trait's own doc comment). Kept a typed
    /// refusal here rather than duplicating the bridge behind a second
    /// entry point.
    fn respond_to_approval(&self, _approval_id: &str, _decision: &str) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            Err(AdapterError::capability_unsupported(
                self.kind(),
                "respond_to_approval",
            ))
        })
    }

    fn cancel(&self, _scope: CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut guard = self.child.lock().await;
            if let Some(child) = guard.as_mut() {
                child
                    .kill()
                    .await
                    .map_err(|e| AdapterError::process(self.kind(), "cancel", e.to_string()))?;
            }
            Ok(())
        })
    }

    fn snapshot(&self) -> AdapterFuture<'_, AdapterSnapshot> {
        Box::pin(async move {
            let running = self.child.lock().await.is_some();
            Ok(AdapterSnapshot {
                state_summary: if running {
                    "running".to_string()
                } else {
                    "idle".to_string()
                },
                children: Vec::new(),
                usage: None,
                artifacts: Vec::new(),
            })
        })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let mut guard = self.child.lock().await;
            if let Some(child) = guard.as_mut() {
                let _ = child.kill().await;
            }
            *guard = None;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc as StdArc;

    use tokio::sync::broadcast;

    use crew_protocol::{RunId, TaskId, WorkerId};

    use crate::approval::NoopApprovalCallback;
    use crate::db::DatabaseHandle;

    /// A minimal in-memory sink that just records every payload it was
    /// handed -- the same small fixture `reader.rs`'s own test module
    /// defines, duplicated here rather than exported across a private
    /// module boundary for one shared use.
    struct RecordingSink {
        events: StdArc<parking_lot::Mutex<Vec<AdapterEventPayload>>>,
    }

    impl AdapterEventSink for RecordingSink {
        fn emit(&self, event: AdapterEvent) -> AdapterFuture<'_, u64> {
            self.events.lock().push(event.payload);
            Box::pin(async { Ok(0) })
        }

        fn note_real_user_turn(&self, _run_id: RunId) -> AdapterFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A fake `claude` that dumps its own received argv (one entry per
    /// line) to `argv_path` before doing anything else, then emits just
    /// enough `stream-json` to let [`Adapter::start`] complete: a
    /// `system/init` line (so reconciliation has a session id to look
    /// for, even though no transcript file will exist for it) and a
    /// `result` line. Mirrors
    /// `crates/runtime/tests/tui_claude_registry.rs`'s own
    /// `write_argv_recording_claude_script` -- the same proof shape
    /// (read back what was actually executed, not a stored value that
    /// could look right while the real spawn used something else), a
    /// stream-json body instead of a PTY transcript.
    fn write_argv_recording_fake_claude(
        dir: &std::path::Path,
        argv_path: &std::path::Path,
    ) -> PathBuf {
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > "{argv_path}"
read -r _first_line
echo '{{"type":"system","subtype":"init","session_id":"sess-argv-test","claude_code_version":"2.1.268","permissionMode":"auto"}}'
echo '{{"type":"result","subtype":"success"}}'
"#,
            argv_path = argv_path.display(),
        );
        let path = dir.join("fake-claude.sh");
        std::fs::write(&path, script).expect("write fake claude script");
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// Keyed on the CANONICALIZED repo root -- `Self::start` canonicalizes
    /// before checking (matching `ClaudeTuiVendor::transcript_root`'s own
    /// precedent that a real recording proved necessary: a tempdir under
    /// `/tmp`/`/var` is frequently a symlink to `/private/tmp`/
    /// `/private/var` on macOS, so the raw and canonical paths can
    /// genuinely differ even for a path this test itself just created).
    fn trusted_claude_json(dir: &std::path::Path, repo_root: &std::path::Path) -> PathBuf {
        let canonical =
            std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
        let path = dir.join(".claude.json");
        let contents = serde_json::json!({
            "projects": {
                canonical.to_string_lossy().to_string(): { "hasTrustDialogAccepted": true },
            }
        });
        std::fs::write(&path, contents.to_string()).unwrap();
        path
    }

    /// The same reproduction shape the `build_tui_adapter` model-resolution
    /// fix used: assert against the ARGV THE REAL SPAWN RECEIVED, read
    /// back from a file the fake binary itself wrote, never a
    /// configured/stored value that could look right while the actual
    /// launch used something else. Proves `--model` reaches the real
    /// launch for this adapter the same way that fix proved it for
    /// `TuiAdapter`.
    #[tokio::test]
    async fn the_resolved_model_reaches_the_real_launched_argv() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let argv_path = dir.path().join("argv.txt");
        let bin = write_argv_recording_fake_claude(dir.path(), &argv_path);
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());

        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            Some("claude-sonnet-5".to_string()),
            ProtocolBundle {
                approval_service,
                callback,
            },
            None,
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string());

        let events = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink {
            events: StdArc::clone(&events),
        });

        adapter
            .start(
                StartSpec {
                    run_id: RunId::new(),
                    task_id: TaskId::new(),
                    worker_id: WorkerId::new(),
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            )
            .await
            .expect("start must succeed against the fake binary");

        let recorded_argv = std::fs::read_to_string(&argv_path).expect("fake binary must have run");
        let argv: Vec<&str> = recorded_argv.lines().collect();
        assert!(
            argv.windows(2).any(|w| w == ["--model", "claude-sonnet-5"]),
            "expected --model claude-sonnet-5 in the real launched argv, got: {argv:?}"
        );

        db.shutdown().await.ok();
    }

    /// A pane backend that always succeeds with a non-empty pane ref --
    /// mirrors `tests/tui_claude_registry.rs`'s own `FakeBackend`
    /// fixture exactly, for the identical reason: `HiddenDisplay`'s own
    /// `pane_ref` is empty, which would make "a real pane attached"
    /// indistinguishable from "no real pane at all" in this test's own
    /// assertions.
    struct FakePaneBackend;

    impl crate::display::DisplayBackendTrait for FakePaneBackend {
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

        fn create_pane(
            &self,
            req: crate::display::PaneRequest,
        ) -> crate::display::DisplayFuture<'_, crate::display::PaneHandle> {
            let handle = crate::display::PaneHandle {
                backend: crew_protocol::DisplayBackend::Tmux,
                pane_ref: "fake-pane-1".to_string(),
                placement: req.placement,
            };
            Box::pin(async move { Ok(handle) })
        }

        fn close_pane(
            &self,
            _handle: &crate::display::PaneHandle,
        ) -> crate::display::DisplayFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
    }

    /// End-to-end proof that `Self::attach_pane`/`Self::detach_pane`
    /// actually reach a real `PaneCoordinator`: drives a real turn (the
    /// same fake-script harness as the model-argv test above) with a
    /// real pane wired in, then asserts a real `DisplayPaneAttached` and
    /// `DisplayPaneDetached` were journaled with the fake backend's own
    /// non-empty pane ref -- not that the two pieces were built to
    /// agree, but that wiring them together actually reaches the
    /// journal.
    #[tokio::test]
    async fn a_real_turn_attaches_and_detaches_a_real_pane() {
        // `tempdir_in("/tmp")`, not the bare `TempDir::new()` default
        // (`std::env::temp_dir()`, a deeply-nested path on macOS): the
        // attach socket this test binds lives under this directory, and
        // a `run_id`-length UUID appended to that deep a path overflows
        // the platform `sun_path` limit -- the same reason
        // `tests/tui_claude_registry.rs`'s own harness uses this exact
        // override.
        let dir = tempfile::Builder::new()
            .prefix("bat-pane-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let argv_path = dir.path().join("argv.txt");
        let bin = write_argv_recording_fake_claude(dir.path(), &argv_path);
        let claude_json_path = trusted_claude_json(dir.path(), &repo_root);

        let state_dir = tempfile::TempDir::new().unwrap();
        let db = StdArc::new(
            DatabaseHandle::start(state_dir.path().join("runtime.db"))
                .await
                .unwrap(),
        );
        let project_id = crew_protocol::ProjectId::new();
        let approval_service = StdArc::new(ApprovalService::new(
            StdArc::clone(&db),
            project_id,
            StdArc::new(NoopApprovalCallback) as StdArc<dyn crate::approval::ApprovalCallback>,
            broadcast::channel(64).0,
        ));
        let callback = StdArc::new(ProtocolApprovalCallback::new());

        let mut display_registry = crate::display::DisplayRegistry::new();
        display_registry.register(Box::new(FakePaneBackend));
        let (events_tx, _events_rx) = broadcast::channel(64);
        let pane_coordinator = Arc::new(crate::display::PaneCoordinator::new(
            Arc::new(display_registry),
            StdArc::clone(&db),
            project_id,
            events_tx,
            std::path::PathBuf::from("/opt/crew/bin/crewd"),
            dir.path().to_path_buf(),
            repo_root.clone(),
            crate::security::redaction::Redactor::new(),
        ));
        let panes_dir = dir.path().join("panes");
        std::fs::create_dir_all(&panes_dir).unwrap();

        let adapter = ClaudeProtocolAdapter::new(
            repo_root.clone(),
            Vec::new(),
            None,
            ProtocolBundle {
                approval_service,
                callback,
            },
            Some(pane::PaneSupport {
                pane_coordinator,
                panes_dir,
                placement: crew_protocol::DisplayPlacement::SplitRight,
                forced_backend: None,
                launch_program: None,
                close_on_exit: crate::config::crew::CloseOnExit::Always,
            }),
        )
        .with_test_overrides(claude_json_path, bin.to_string_lossy().to_string());

        let events = StdArc::new(parking_lot::Mutex::new(Vec::new()));
        let sink: Arc<dyn AdapterEventSink> = Arc::new(RecordingSink { events });

        // `PaneCoordinator::attach`/`detach` journal against a real
        // `runs` row (a foreign-key reference, invariant 3) -- unlike
        // `RecordingSink`, they write through the real `db` this test
        // constructed, so the row has to actually exist first, the same
        // seeding `tests/tui_claude_registry.rs`'s own
        // `seed_worker_and_run` does.
        let task_id = TaskId::new();
        let worker_id = WorkerId::new();
        let run_id = RunId::new();
        db.run_domain_op(Box::new({
            let task_id = task_id.to_string();
            let worker_id = worker_id.to_string();
            let run_id = run_id.to_string();
            let project_id = project_id.to_string();
            move |conn| {
                conn.execute(
                    "INSERT INTO tasks (task_id, project_id, owner_client_instance_id, revision, created_at, updated_at) \
                     VALUES (?1, ?2, 'test-owner', 1, ?3, ?3)",
                    rusqlite::params![task_id, project_id, "2026-01-01T00:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO worker_profiles (id, fingerprint, adapter, model, permission_envelope) \
                     VALUES (?1, 'sha256:test', 'claude', 'test-model', '{}')",
                    rusqlite::params![worker_id.clone()],
                )?;
                conn.execute(
                    "INSERT INTO workers (worker_id, project_id, profile_id, resolved_profile_json, created_at) \
                     VALUES (?1, ?2, ?1, '{}', ?3)",
                    rusqlite::params![worker_id, project_id, "2026-01-01T00:00:00Z"],
                )?;
                conn.execute(
                    "INSERT INTO runs (run_id, task_id, worker_id, state, created_at) \
                     VALUES (?1, ?2, ?3, 'queued', ?4)",
                    rusqlite::params![run_id, task_id, worker_id, "2026-01-01T00:00:00Z"],
                )?;
                Ok(serde_json::Value::Null)
            }
        }))
        .await
        .expect("seed task/worker/run");

        adapter
            .start(
                StartSpec {
                    run_id,
                    task_id,
                    worker_id,
                    prompt: "hello".to_string(),
                    resume: None,
                },
                sink,
            )
            .await
            .expect("start must succeed against the fake binary and fake pane backend");

        let run_id_string = run_id.to_string();
        let dump: Vec<String> = db
            .run_domain_op(Box::new(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT event_json FROM events WHERE run_id = ?1 ORDER BY sequence")?;
                let rows = stmt
                    .query_map(rusqlite::params![run_id_string], |row| {
                        row.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(serde_json::json!(rows))
            }))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();

        assert!(
            dump.iter()
                .any(|e| e.contains("displayPaneAttached") && e.contains("fake-pane-1")),
            "expected a DisplayPaneAttached event naming the fake backend's own pane ref, got: {dump:#?}"
        );
        assert!(
            dump.iter().any(|e| e.contains("displayPaneDetached")),
            "expected a DisplayPaneDetached event once the turn settled, got: {dump:#?}"
        );

        db.shutdown().await.ok();
    }
}
