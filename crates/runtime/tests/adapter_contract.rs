//! Integration tests for the worker adapter contract: capability
//! round-tripping, explicit unsupported-operation errors, adapter event
//! sink emission (including `NestedWorkerObserved` even when `nested` is
//! declared `none`), and `WorkerProfile` startup-option/environment
//! validation, including the `profile/register` + `worker/create`
//! `profileId` gating RPC surface.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use batman_protocol::{ProjectId, RunId, TaskId, WorkerId};
use batman_runtime::adapter::{
    Adapter, AdapterCapabilities, AdapterError, AdapterEvent, AdapterEventPayload,
    AdapterEventSink, AdapterFuture, AdapterKind, ApprovalsCapability, DurabilityCapability,
    EffectivePolicy, NativeViewCapability, NestedCapability, ProtocolKind, ResumeCapability,
    StartupOptions, SteeringCapability, UsageCapability, WorkerProfile, WorkspaceControlCapability,
};
use batman_runtime::db::DatabaseHandle;
use batman_runtime::ipc::{PeerCredentialReader, PeerCredentials, Server, ServerConfig};
use batman_runtime::paths::RuntimePaths;
use batman_runtime::policy::ViolationService;
use nix::unistd::Uid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// ------------------------------------------------------------------ fakes

struct FakeReader {
    uid: Option<u32>,
}

impl PeerCredentialReader for FakeReader {
    fn read(&self, _stream: &UnixStream) -> PeerCredentials {
        PeerCredentials {
            uid: self.uid,
            pid: Some(4242),
        }
    }
}

fn matching_reader() -> Arc<dyn PeerCredentialReader> {
    Arc::new(FakeReader {
        uid: Some(Uid::current().as_raw()),
    })
}

// --------------------------------------------------------------- harness

struct Harness {
    socket: PathBuf,
    owned_dir: PathBuf,
    db: Arc<DatabaseHandle>,
    project_id: ProjectId,
    _state: tempfile::TempDir,
    _repo: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl Harness {
    async fn start() -> Self {
        let state = tempfile::Builder::new()
            .prefix("bat-ac-s-")
            .tempdir_in("/tmp")
            .unwrap();
        let repo = tempfile::Builder::new()
            .prefix("bat-ac-r-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();

        let paths = RuntimePaths::resolve(state.path(), repo.path()).unwrap();
        let db = Arc::new(DatabaseHandle::start(paths.database.clone()).await.unwrap());

        let config = ServerConfig {
            credential_reader: matching_reader(),
            ..Default::default()
        };

        let server = Server::bind(paths.socket.clone(), db.clone(), paths.project_id, config)
            .await
            .unwrap();
        let socket = server.socket_path().to_path_buf();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let join = tokio::spawn(async move {
            let _ = server
                .serve(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let owned_dir = std::fs::canonicalize(repo.path()).unwrap();

        Self {
            socket,
            owned_dir,
            project_id: paths.project_id,
            db,
            _state: state,
            _repo: repo,
            shutdown: Some(shutdown_tx),
            join: Some(join),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

// ---------------------------------------------------------------- client

struct Client {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, writer) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer,
        }
    }

    async fn send(&mut self, value: &Value) {
        let line = serde_json::to_string(value).unwrap();
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.write_all(b"\n").await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim_end()).unwrap()
    }

    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        self.recv().await
    }
}

async fn omp_client(harness: &Harness, instance_id: &str) -> Client {
    let mut client = Client::connect(&harness.socket).await;
    client
        .send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "client": { "name": "@nikolasd/crew", "version": "0.1.0" },
                "supported": { "min": { "major": 1, "minor": 0 }, "max": { "major": 1, "minor": 0 } },
                "repository": {
                    "canonicalPath": harness.owned_dir.to_str().unwrap(),
                    "vcsRoot": harness.owned_dir.to_str().unwrap()
                },
                "auth": { "role": "ompExtension", "instanceId": instance_id, "agentDirectory": harness.owned_dir.to_str().unwrap() },
                "capabilities": { "eventReplay": true, "maxFrameBytes": 1048576 },
                "lastSequence": null
            }
        }))
        .await;
    let init = client.recv().await;
    assert!(init.get("error").is_none(), "initialize failed: {init:?}");
    client
}

// --------------------------------------------------------- fixture adapter

/// A minimal [`Adapter`] used only to prove the trait's unsupported-
/// operation and capability-declaration behavior. Declares no approvals,
/// no resume, no steering, and `nested: none`.
struct FixtureAdapter {
    capabilities: AdapterCapabilities,
}

impl FixtureAdapter {
    fn new() -> Self {
        Self {
            capabilities: AdapterCapabilities {
                protocol: ProtocolKind::Structured,
                resume: ResumeCapability::None,
                steering: SteeringCapability::None,
                approvals: ApprovalsCapability::None,
                structured_result: true,
                usage: UsageCapability::None,
                nested: NestedCapability::None,
                native_view: NativeViewCapability::None,
                workspace_control: WorkspaceControlCapability::ReadOnly,
                durability: DurabilityCapability::RuntimeScoped,
            },
        }
    }
}

impl Adapter for FixtureAdapter {
    fn kind(&self) -> &str {
        "fixture"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        self.capabilities
    }

    fn respond_to_approval(&self, _approval_id: &str, _decision: &str) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            Err(AdapterError::capability_unsupported(
                self.kind(),
                "respondToApproval",
            ))
        })
    }

    fn probe(&self) -> AdapterFuture<'_, batman_runtime::adapter::ProbeResult> {
        Box::pin(async move {
            Ok(batman_runtime::adapter::ProbeResult {
                version: Some("0.0.0-fixture".to_string()),
                auth_ready: true,
                capabilities: self.capabilities,
                inventory_incomplete: false,
            })
        })
    }

    fn start(
        &self,
        _spec: batman_runtime::adapter::StartSpec,
        _sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn resume(
        &self,
        _session: batman_runtime::adapter::VendorSessionRef,
        _sink: Arc<dyn AdapterEventSink>,
    ) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Err(AdapterError::capability_unsupported(self.kind(), "resume")) })
    }

    fn send(&self, _message: batman_runtime::adapter::AdapterMessage) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Err(AdapterError::capability_unsupported(self.kind(), "send")) })
    }

    fn cancel(&self, _scope: batman_runtime::adapter::CancelScope) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn snapshot(&self) -> AdapterFuture<'_, batman_runtime::adapter::AdapterSnapshot> {
        Box::pin(async move { Ok(batman_runtime::adapter::AdapterSnapshot::default()) })
    }

    fn dispose(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move { Ok(()) })
    }
}

// --------------------------------------------------------------- Task 1.1

#[tokio::test]
async fn unsupported_operation_returns_explicit_capability_error() {
    let adapter = FixtureAdapter::new();

    let err = adapter
        .respond_to_approval("approval-1", "approve")
        .await
        .expect_err("an adapter declaring approvals:none must reject respondToApproval");

    assert_eq!(err.code(), "capability_unsupported");
    assert_eq!(err.operation(), "respondToApproval");
    assert_eq!(err.adapter(), "fixture");
}

#[tokio::test]
async fn capabilities_round_trip_strict_camel_case_and_reject_unknown_enum_value() {
    let adapter = FixtureAdapter::new();
    let value = serde_json::to_value(adapter.capabilities()).unwrap();

    // Strict camelCase field names, exact enum wire strings.
    assert_eq!(value["protocol"], "structured");
    assert_eq!(value["resume"], "none");
    assert_eq!(value["steering"], "none");
    assert_eq!(value["approvals"], "none");
    assert_eq!(value["structuredResult"], true);
    assert_eq!(value["usage"], "none");
    assert_eq!(value["nested"], "none");
    assert_eq!(value["nativeView"], "none");
    assert_eq!(value["workspaceControl"], "readOnly");
    assert_eq!(value["durability"], "runtimeScoped");

    // Round-trips.
    let round_tripped: AdapterCapabilities = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(round_tripped, adapter.capabilities());

    // An unknown enum value is rejected, not silently coerced.
    let mut corrupted = value;
    corrupted["steering"] = json!("bogus-value");
    let result: Result<AdapterCapabilities, _> = serde_json::from_value(corrupted);
    assert!(
        result.is_err(),
        "an unknown capability enum value must be rejected"
    );
}

// --------------------------------------------------------------- Task 1.3

#[tokio::test]
async fn nested_worker_observed_emits_without_upgrading_declared_capability() {
    let harness = Harness::start().await;
    let mut client = omp_client(&harness, "omp-1").await;

    let upsert = client
        .call(
            2,
            "task/upsert",
            json!({ "ownerClientInstanceId": "omp-1", "revision": 1 }),
        )
        .await;
    let task_id = TaskId::parse(upsert["result"]["taskId"].as_str().unwrap()).unwrap();

    let worker = client
        .call(
            3,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    let worker_id = WorkerId::parse(worker["result"]["workerId"].as_str().unwrap()).unwrap();

    let submit = client
        .call(
            4,
            "run/submit",
            json!({ "taskId": task_id.to_string(), "workerId": worker_id.to_string() }),
        )
        .await;
    // run/submit fails adapter_unavailable (no RunDriver wired), but the
    // run is still durably queued -- look it up via run/list.
    let _ = submit;
    let list = client
        .call(5, "run/list", json!({ "taskId": task_id.to_string() }))
        .await;
    let run_id = RunId::parse(
        list["result"]["runs"].as_array().unwrap()[0]["runId"]
            .as_str()
            .unwrap(),
    )
    .unwrap();

    // The fixture adapter declares nested:none.
    let adapter = FixtureAdapter::new();
    assert_eq!(adapter.capabilities().nested, NestedCapability::None);

    // Prove real live-broadcast delivery (not just replay): subscribe to
    // the sink's own channel *before* emitting, then receive the
    // notification the sink actually sends, under a bounded timeout so a
    // regressed sink that forgets to broadcast (architecture.md §18 item
    // 3's exact failure mode) hangs the test loudly rather than passing
    // silently.
    let (events_tx, mut events_rx) = tokio::sync::broadcast::channel(16);
    let violation_service = std::sync::Arc::new(ViolationService::new(
        harness.db.clone(),
        harness.project_id,
        events_tx.clone(),
        None,
        batman_runtime::config::NestedViolationAction::default(),
    ));
    let sink = batman_runtime::adapter::DomainAdapterEventSink::new(
        harness.db.clone(),
        harness.project_id,
        events_tx,
        vec![],
        false,
        violation_service,
        None,
    )
    .expect("built-in patterns always compile");

    let sequence = sink
        .emit(AdapterEvent {
            run_id,
            task_id,
            worker_id,
            payload: AdapterEventPayload::NestedWorkerObserved {
                vendor_child_id: "child-vendor-1".to_string(),
                vendor_parent_ref: "parent-vendor-1".to_string(),
            },
        })
        .await
        .expect("emitting a NestedWorkerObserved event must succeed");
    assert!(sequence > 0);

    let live_envelope = tokio::time::timeout(std::time::Duration::from_secs(5), events_rx.recv())
        .await
        .expect("the sink must broadcast the committed event live, not only durably")
        .expect("the broadcast channel must not be closed");
    assert_eq!(live_envelope.sequence, sequence);
    assert_eq!(live_envelope.run_id, Some(run_id));

    // Emitting the event never upgrades the adapter's own declared
    // capability -- there is no code path that could, since `emit` takes
    // no capability parameter at all.
    assert_eq!(adapter.capabilities().nested, NestedCapability::None);

    // The event is also durably correlated and replayable (a genuinely
    // different property from live delivery: a session that starts
    // *after* this commit must still see it).
    let replay = client
        .call(6, "events/replay", json!({ "afterSequence": 0 }))
        .await;
    let events = replay["result"].as_array().unwrap();
    let nested_event = events
        .iter()
        .find(|e| e["event"]["type"] == "adapterNestedWorkerEvent")
        .expect("a adapterNestedWorkerEvent must be present in the replayed events");
    assert_eq!(
        nested_event["event"]["payload"]["runId"],
        run_id.to_string()
    );
    assert_eq!(
        nested_event["event"]["payload"]["vendorChildId"],
        "child-vendor-1"
    );
    assert_eq!(
        nested_event["event"]["payload"]["vendorParentRef"],
        "parent-vendor-1"
    );
}

// --------------------------------------------------------------- Task 1.4

fn codex_reviewer_profile(environment_allowlist: Vec<&str>) -> Value {
    json!({
        "adapter": "codex",
        "model": "gpt-5-codex",
        "permissionEnvelope": { "fullAuto": false },
        "startupOptions": {
            "codex": {
                "sandboxMode": "workspace-write",
                "approvalPolicy": "on-request"
            }
        },
        "environmentAllowlist": environment_allowlist,
        "source": "engineer-local"
    })
}

#[test]
fn fixture_profile_rejects_unknown_startup_option_keys() {
    let mut raw = codex_reviewer_profile(vec![]);
    raw["startupOptions"]["codex"]["unknownField"] = json!("nope");
    let result: Result<WorkerProfile, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "unknown startup-option keys must be rejected"
    );
}

#[test]
fn fixture_profile_rejects_inline_environment_values() {
    let mut raw = codex_reviewer_profile(vec![]);
    // Inline values (an object), not a plain list of allowed names.
    raw["environmentAllowlist"] = json!({ "ANTHROPIC_API_KEY": "sk-inline-secret-value" });
    let result: Result<WorkerProfile, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "inline environment values must be rejected structurally"
    );
}

#[test]
fn fixture_profile_rejects_environment_names_outside_org_allowlist() {
    let raw = codex_reviewer_profile(vec!["TOTALLY_UNAPPROVED_VAR"]);
    let profile: WorkerProfile = serde_json::from_value(raw).unwrap();

    let policy = EffectivePolicy::baseline();
    let err = profile
        .validate(&policy)
        .expect_err("an environment name absent from the org allowlist must be rejected");
    assert!(err.to_string().contains("TOTALLY_UNAPPROVED_VAR"));
}

#[test]
fn fixture_profile_rejects_secret_shaped_permission_envelope() {
    let mut raw = codex_reviewer_profile(vec![]);
    raw["permissionEnvelope"] = json!({ "apiKey": "sk-abcdefghijklmnopqrst0123456789" });
    let profile: WorkerProfile = serde_json::from_value(raw).unwrap();

    let policy = EffectivePolicy::baseline();
    let err = profile
        .validate(&policy)
        .expect_err("a secret-shaped permissionEnvelope value must be rejected outright");
    assert!(err.to_string().contains("permissionEnvelope"));

    // Defense in depth: `fingerprint()` internally re-sanitizes
    // `permissionEnvelope` before hashing (in case `validate` is ever
    // bypassed by a future call site). Prove that by constructing a
    // second profile whose `permissionEnvelope` already carries the
    // *redacted* form and asserting the two fingerprints are identical --
    // real evidence the raw secret-shaped text never reaches the hash
    // input, not a tautology about hex digests never containing "sk-".
    let mut pre_redacted = codex_reviewer_profile(vec![]);
    pre_redacted["permissionEnvelope"] = json!({ "apiKey": "[REDACTED:api_key]" });
    let redacted_profile: WorkerProfile = serde_json::from_value(pre_redacted).unwrap();
    assert_eq!(
        profile.fingerprint(),
        redacted_profile.fingerprint(),
        "fingerprint() must sanitize permissionEnvelope before hashing"
    );
}

#[test]
fn profiles_differing_only_in_permission_envelope_key_order_share_one_fingerprint() {
    let mut first_raw = codex_reviewer_profile(vec![]);
    first_raw["permissionEnvelope"] = json!({
        "allow": ["read"],
        "deny": ["write"]
    });
    let mut second_raw = codex_reviewer_profile(vec![]);
    second_raw["permissionEnvelope"] = json!({
        "deny": ["write"],
        "allow": ["read"]
    });

    let first: WorkerProfile = serde_json::from_value(first_raw).unwrap();
    let second: WorkerProfile = serde_json::from_value(second_raw).unwrap();

    assert_eq!(first.fingerprint(), second.fingerprint());
}

#[test]
fn a_permission_envelope_with_unsorted_keys_is_not_mistaken_for_a_secret() {
    let mut raw = codex_reviewer_profile(vec![]);
    raw["permissionEnvelope"] = json!({ "zeta": "ok", "alpha": "ok" });
    let profile: WorkerProfile = serde_json::from_value(raw).unwrap();

    profile
        .validate(&EffectivePolicy::baseline())
        .expect("an unsorted permission envelope is not secret-shaped");
}

#[test]
fn fixture_profile_allows_explicitly_permitted_secret_env_name_but_never_stores_its_value() {
    let raw = codex_reviewer_profile(vec!["ANTHROPIC_API_KEY"]);
    let profile: WorkerProfile = serde_json::from_value(raw).unwrap();

    let mut policy = EffectivePolicy::baseline();
    policy.allow_env_name("ANTHROPIC_API_KEY");
    profile
        .validate(&policy)
        .expect("an explicitly allowed secret name must validate");

    // The validated profile snapshot carries only the variable *name*,
    // never a value -- there is no field anywhere in `WorkerProfile` that
    // could hold one, by construction (`environmentAllowlist` is a plain
    // `Vec<String>` of names).
    assert_eq!(
        profile.environment_allowlist(),
        &["ANTHROPIC_API_KEY".to_string()]
    );
    let snapshot_json = serde_json::to_string(&profile).unwrap();
    assert!(
        !snapshot_json.contains("sk-"),
        "no secret value shape may appear in the snapshot"
    );

    let fingerprint = profile.fingerprint();
    assert!(fingerprint.starts_with("sha256:"));
    assert!(!fingerprint.contains("sk-"));
}

#[test]
fn adapter_kind_reserved_names_match_wire_strings() {
    assert_eq!(AdapterKind::Claude.wire_name(), "claude");
    assert_eq!(AdapterKind::Codex.wire_name(), "codex");
    assert_eq!(AdapterKind::Copilot.wire_name(), "copilot");
    assert_eq!(AdapterKind::OmpRpc.wire_name(), "ompRpc");
    assert_eq!(
        AdapterKind::from_wire_name("claude"),
        Some(AdapterKind::Claude)
    );
    assert_eq!(AdapterKind::from_wire_name("fake"), None);
}

#[test]
fn startup_options_tag_matches_declared_adapter_kind() {
    let raw = codex_reviewer_profile(vec![]);
    let profile: WorkerProfile = serde_json::from_value(raw).unwrap();
    assert_eq!(profile.adapter_kind(), Some(AdapterKind::Codex));
    match profile.startup_options() {
        StartupOptions::Codex(opts) => {
            assert_eq!(opts.sandbox_mode.as_deref(), Some("workspace-write"));
            assert_eq!(opts.approval_policy.as_deref(), Some("on-request"));
        }
        other => panic!("expected Codex startup options, got {other:?}"),
    }
}

// ----------------------------------------------------- profile/register RPC

#[tokio::test]
async fn worker_create_requires_profile_id_for_reserved_adapter_kinds() {
    let harness = Harness::start().await;
    let mut client = omp_client(&harness, "omp-1").await;

    let create = client
        .call(
            2,
            "worker/create",
            json!({ "fingerprint": "sha256:x", "adapter": "claude", "model": "claude-sonnet" }),
        )
        .await;
    assert!(
        create.get("error").is_some(),
        "a reserved adapter kind without profileId must be rejected: {create:?}"
    );
    assert_eq!(create["error"]["code"], -32007);
}

#[tokio::test]
async fn worker_create_accepts_legacy_raw_fields_for_non_reserved_adapters() {
    let harness = Harness::start().await;
    let mut client = omp_client(&harness, "omp-1").await;

    let create = client
        .call(
            2,
            "worker/create",
            json!({ "fingerprint": "sha256:f", "adapter": "fake", "model": "m" }),
        )
        .await;
    assert!(
        create.get("error").is_none(),
        "legacy adapter:fake path must be unaffected: {create:?}"
    );
}

#[tokio::test]
async fn changing_the_source_profile_after_worker_creation_never_mutates_the_stored_snapshot() {
    let harness = Harness::start().await;
    let mut client = omp_client(&harness, "omp-1").await;

    let register = client
        .call(2, "profile/register", codex_reviewer_profile(vec![]))
        .await;
    assert!(
        register.get("error").is_none(),
        "profile/register failed: {register:?}"
    );
    let profile_id = register["result"]["profileId"]
        .as_str()
        .unwrap()
        .to_string();
    let original_fingerprint = register["result"]["fingerprint"]
        .as_str()
        .unwrap()
        .to_string();

    let create = client
        .call(
            3,
            "worker/create",
            json!({ "profileId": profile_id.clone() }),
        )
        .await;
    assert!(
        create.get("error").is_none(),
        "worker/create with profileId failed: {create:?}"
    );
    let worker_id = create["result"]["workerId"].as_str().unwrap().to_string();

    let get_before = client
        .call(4, "worker/get", json!({ "workerId": worker_id.clone() }))
        .await;
    assert_eq!(
        get_before["result"]["profileRef"]["fingerprint"],
        original_fingerprint
    );

    // Re-register the *same* profileId's underlying configuration is not
    // possible (profiles are immutable once registered); simulate a
    // changed source profile by registering a new one with the same
    // logical name/content changed, then confirm the *existing* worker's
    // stored snapshot is untouched -- only a fresh worker/create would see
    // the new profile, and only via its own fresh profileId.
    let mut changed = codex_reviewer_profile(vec![]);
    changed["model"] = json!("gpt-5-codex-updated");
    let re_register = client.call(5, "profile/register", changed).await;
    let new_profile_id = re_register["result"]["profileId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        new_profile_id, profile_id,
        "a changed profile registers as a distinct id"
    );

    let get_after = client
        .call(6, "worker/get", json!({ "workerId": worker_id }))
        .await;
    assert_eq!(
        get_after["result"]["profileRef"]["fingerprint"], original_fingerprint,
        "the already-created worker's stored profile snapshot must never change"
    );
}

/// R14: a sink whose org redaction patterns do not compile must not be
/// constructed at all -- the old fallback silently degraded to built-in
/// rules only, one config-reload away from journaling text the org's
/// redaction rules were meant to remove (invariant 4).
#[tokio::test]
async fn a_sink_with_invalid_org_patterns_fails_closed() {
    let harness = Harness::start().await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(16);
    let violation_service = std::sync::Arc::new(ViolationService::new(
        harness.db.clone(),
        harness.project_id,
        events_tx.clone(),
        None,
        batman_runtime::config::NestedViolationAction::default(),
    ));
    let result = batman_runtime::adapter::DomainAdapterEventSink::new(
        harness.db.clone(),
        harness.project_id,
        events_tx,
        vec!["[invalid-regex".to_string()],
        false,
        violation_service,
        None,
    );
    let err = result
        .err()
        .expect("an invalid org pattern must refuse construction");
    assert!(
        err.contains("[invalid-regex") || err.to_lowercase().contains("regex"),
        "the error must name the failing pattern: {err}"
    );
}
