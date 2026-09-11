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
//! and `reader::drive_turn` itself.
//!
//! **Not in scope here** (see this spike's own PR body for the full
//! non-goals list): a renderer beyond bare legibility, `resume`/
//! `--continue`, the other three vendors, and performance work.
//! `--model` is threaded through as a plain parameter this adapter does
//! not resolve itself -- the run-specific-over-boot-config precedence a
//! correct answer needs belongs to the fix for `build_adapter`'s other
//! callers' own model-resolution gap, landing separately on the shared
//! construction path first; this adapter inherits whatever it is
//! handed, the same way
//! `crate::adapter::tui::claude::ClaudeTuiVendor::base_args` reads
//! `cfg.model` today, not a fix invented here ahead of it landing.

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
use super::{reconcile, trust};

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
    /// `Some(trust::default_claude_json_path())` in production; `None`
    /// only when `HOME` could not be resolved, in which case
    /// [`Self::start`] treats the workspace as untrusted (fail closed,
    /// never guessed).
    claude_json_path: Option<PathBuf>,
    /// The live child process, held for the duration of one turn so
    /// [`Adapter::cancel`] can reach it. `None` before the first
    /// [`Adapter::start`] call and after the turn ends.
    child: AsyncMutex<Option<tokio::process::Child>>,
}

impl ClaudeProtocolAdapter {
    #[must_use]
    pub(crate) fn new(
        repo_root: PathBuf,
        environment_allowlist: Vec<String>,
        model: Option<String>,
        bundle: ProtocolBundle,
    ) -> Self {
        Self {
            repo_root,
            environment_allowlist,
            model,
            bundle,
            claude_json_path: trust::default_claude_json_path(),
            child: AsyncMutex::new(None),
        }
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
            let output = std::process::Command::new("claude")
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

            let model = self
                .model
                .as_deref()
                .map(str::trim)
                .filter(|model| !model.is_empty());
            let argv = super::launch::build_argv(model);
            let env = self.env();

            let mut command = tokio::process::Command::new("claude");
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
