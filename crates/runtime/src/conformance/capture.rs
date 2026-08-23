//! Automated fixture capture: drives a real vendor CLI turn, records JSON
//! stdout frames, scrubs known nondeterministic values, and writes the
//! resulting fixed-point fixture bytes in place.

use crate::adapter::{
    Adapter, AdapterEvent, AdapterEventSink, AdapterFuture, AdapterKind, ClaudeStartupOptions,
    CodexStartupOptions, CopilotStartupOptions, OmpRpcAdapterOptions, StartSpec,
};
use crate::conformance::scrub::Scrubber;
use crate::conformance::{
    ConformanceReport, run_fixture_conformance, vendor_cli_invocation_disabled,
};
use crate::supervisor::install_frame_tap;
use batman_protocol::{RunId, TaskId, WorkerId};
use serde_yaml_ng as serde_yaml;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/adapters");
const MANIFEST_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/adapters/capture-manifest.yml"
);

/// The on-disk directory name for each adapter kind. Note `ompRpc` maps
/// to the hyphenated `omp-rpc`.
fn adapter_fixture_dir(kind: AdapterKind) -> &'static str {
    match kind {
        AdapterKind::Claude => "claude",
        AdapterKind::Codex => "codex",
        AdapterKind::Copilot => "copilot",
        AdapterKind::OmpRpc => "omp-rpc",
    }
}

/// One fixture file and the invocation that reproduces it.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureEntry {
    /// The committed fixture filename this entry regenerates.
    fixture: String,
    /// The turn text. Absent only when `handshake_only` is set.
    #[serde(default)]
    prompt: Option<String>,
    /// Model selector; required for `ompRpc`, ignored by the others.
    #[serde(default)]
    model: Option<String>,
    /// Adapter-specific startup options.
    #[serde(default)]
    startup_options: Option<serde_json::Value>,
    /// When true, capture stops after the protocol handshake and never
    /// sends a turn.
    #[serde(default)]
    handshake_only: bool,
}

/// Result of capturing one fixture file.
#[derive(Debug)]
pub struct CapturedFixture {
    /// The fixture filename.
    pub fixture: String,
    /// Number of frames captured (after scrubbing).
    pub frames: usize,
    /// True when the rendered capture bytes matched the pre-persistence
    /// fixture bytes. It does not assert that an unchanged CLI was captured.
    pub unchanged: bool,
}

/// Outcome of a full capture run for one adapter.
#[derive(Debug)]
pub struct CaptureOutcome {
    /// The fixtures that were captured.
    pub written: Vec<CapturedFixture>,
    /// The fixture suite result after the rewrite. `None` in dry-run.
    pub report: Option<ConformanceReport>,
}

/// Captures every fixture the manifest declares for `kind`, updates only files
/// whose rendered bytes differ, then re-runs `kind`'s fixture conformance
/// suite so the caller learns immediately whether the new captures satisfy it.
///
/// # Errors
/// Returns `Err` when the manifest is unreadable, `kind` has no manifest
/// entries, the vendor CLI cannot be started, or `CREW_DISABLE_VENDOR_CLI=1`
/// is set.
pub async fn capture_adapter(
    kind: AdapterKind,
    only: Option<&str>,
    dry_run: bool,
) -> Result<CaptureOutcome, String> {
    if vendor_cli_invocation_disabled() {
        return Err(
            "CREW_DISABLE_VENDOR_CLI=1 forbids capture; it spawns real vendor CLIs".to_string(),
        );
    }

    let manifest = load_manifest(&kind)?;
    let entries: Vec<&CaptureEntry> = manifest
        .iter()
        .filter(|e| only.is_none_or(|f| e.fixture == f))
        .collect();

    if entries.is_empty() {
        let hint = only
            .map(|f| format!("; no entry for \"{}\"", f))
            .unwrap_or_default();
        return Err(format!(
            "no manifest entries for {}{}",
            kind.wire_name(),
            hint
        ));
    }

    // Install the tap once, before the entry loop.
    let (tap_tx, mut tap_rx) = mpsc::unbounded_channel();
    install_frame_tap(tap_tx).map_err(|e| format!("failed to install frame tap: {}", e))?;

    // Create a scratch working directory and seed it with config.toml.
    let scratch =
        tempfile::tempdir().map_err(|e| format!("failed to create scratch dir: {}", e))?;
    let scratch_path = scratch.path().to_path_buf();
    let config_toml = scratch_path.join("config.toml");
    std::fs::write(&config_toml, "[read_timeout]\nvalue = 30\n")
        .map_err(|e| format!("failed to write config.toml: {}", e))?;

    let mut prepared = Vec::with_capacity(entries.len());

    for entry in entries {
        let frames = capture_one(&kind, scratch_path.clone(), entry, &mut tap_rx).await?;
        let fixture_path = PathBuf::from(FIXTURES_DIR)
            .join(adapter_fixture_dir(kind))
            .join(&entry.fixture);
        let content = render_fixture_content(&entry.fixture, &frames)?;
        prepared.push((entry.fixture.clone(), fixture_path, content, frames.len()));

        // Drain remaining frames from the tap before the next entry.
        drain_tap(&mut tap_rx).await;
    }

    let mut written = Vec::with_capacity(prepared.len());
    for (fixture, fixture_path, content, frames) in prepared {
        let unchanged = persist_fixture_content(&fixture_path, &content, dry_run)?;

        if dry_run {
            // Print to stdout instead of writing fixture files.
            let mut out = std::io::stdout().lock();
            out.write_all(content.as_bytes())
                .map_err(|error| format!("failed to write dry-run output: {}", error))?;
            out.flush()
                .map_err(|error| format!("failed to flush dry-run output: {}", error))?;
        }

        written.push(CapturedFixture {
            fixture,
            frames,
            unchanged,
        });
    }

    // Re-run the fixture conformance suite so the caller learns immediately
    // whether the new captures still satisfy it. Skip in dry-run.
    let report = if !dry_run {
        Some(run_fixture_conformance(kind).await)
    } else {
        None
    };

    Ok(CaptureOutcome { written, report })
}

/// Renders scrubbed frames into the bytes expected by a supported fixture.
fn render_fixture_content(fixture: &str, frames: &[String]) -> Result<String, String> {
    if fixture.ends_with(".jsonl") {
        return Ok(format!("{}\n", frames.join("\n")));
    }

    if fixture.ends_with(".json") {
        let [frame] = frames else {
            return Err(format!(
                "JSON fixture {} requires exactly one frame, captured {}",
                fixture,
                frames.len()
            ));
        };
        let value: serde_json::Value = serde_json::from_str(frame).map_err(|e| {
            format!(
                "failed to parse JSON fixture {} from frame {:?}: {}",
                fixture,
                frame_preview(frame),
                e
            )
        })?;
        return serde_json::to_string_pretty(&value)
            .map(|rendered| format!("{}\n", rendered))
            .map_err(|e| format!("failed to render JSON fixture {}: {}", fixture, e));
    }

    Err(format!("unsupported fixture extension: {}", fixture))
}

/// Limits fixture-frame text included in a capture error.
fn frame_preview(frame: &str) -> String {
    const MAX_CHARS: usize = 80;

    let mut chars = frame.chars();
    let preview: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{}…", preview)
    } else {
        preview
    }
}

/// Compares rendered content to the existing target before optionally replacing it.
fn persist_fixture_content(path: &Path, content: &str, dry_run: bool) -> Result<bool, String> {
    let unchanged = match std::fs::read(path) {
        Ok(existing) => existing == content.as_bytes(),
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!("failed to read {}: {}", path.display(), error));
        }
    };

    if !dry_run && !unchanged {
        let parent = path
            .parent()
            .ok_or_else(|| format!("failed to determine parent for {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create dir {}: {}", parent.display(), error))?;

        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("failed to create temporary {}: {}", path.display(), error))?;
        temporary
            .write_all(content.as_bytes())
            .map_err(|error| format!("failed to write {}: {}", path.display(), error))?;
        temporary
            .flush()
            .map_err(|error| format!("failed to flush {}: {}", path.display(), error))?;
        temporary
            .persist(path)
            .map_err(|error| format!("failed to replace {}: {}", path.display(), error.error))?;
    }

    Ok(unchanged)
}

/// Captures one manifest entry. Returns the scrubbed frame strings.
async fn capture_one(
    kind: &AdapterKind,
    scratch: PathBuf,
    entry: &CaptureEntry,
    tap_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<Vec<String>, String> {
    let adapter = build_adapter(kind, scratch.clone(), entry)?;

    if entry.handshake_only {
        // Probe performs the handshake with no turn.
        adapter
            .probe()
            .await
            .map_err(|e| format!("probe failed: {}", e))?;
    } else {
        let prompt = entry.prompt.as_deref().unwrap_or("(no prompt)").to_string();
        let spec = StartSpec {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            worker_id: WorkerId::new(),
            prompt,
            resume: None,
        };
        let sink = Arc::new(DiscardingSink);
        adapter
            .start(spec, sink)
            .await
            .map_err(|e| format!("start failed: {}", e))?;
    }

    // Collect frames until the turn settles or the deadline elapses.
    let raw_frames = collect_frames(tap_rx).await;
    adapter.dispose().await.ok();
    // Scrub the frames.
    let cwd = scratch
        .to_str()
        .ok_or_else(|| "scratch path is not valid UTF-8".to_string())
        .map(String::from)?;
    let mut scrubber = Scrubber::new(cwd);
    let mut scrubbed = Vec::new();
    for frame in raw_frames {
        if let Some(line) = scrub_captured_frame(*kind, &mut scrubber, &frame) {
            scrubbed.push(line);
        }
    }

    if scrubbed.is_empty() {
        return Err(format!(
            "captured no JSON frames for {}; the CLI produced no fixture data",
            entry.fixture
        ));
    }

    Ok(scrubbed)
}

/// Scrubs one frame; non-JSON frames are never capture-managed fixture data.
fn scrub_captured_frame(
    _kind: AdapterKind,
    scrubber: &mut Scrubber,
    frame: &[u8],
) -> Option<String> {
    scrubber.scrub_line(frame)
}

/// Collects frames from the tap until the turn settles (10s gap) or
/// the 180s deadline elapses.
async fn collect_frames(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut idle_since = tokio::time::Instant::now();

    loop {
        // Deadline check.
        if tokio::time::Instant::now() >= deadline {
            break;
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let idle_remaining = (idle_since + Duration::from_secs(10))
            .saturating_duration_since(tokio::time::Instant::now());
        let timeout = remaining.min(idle_remaining);

        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(frame)) => {
                frames.push(frame);
                idle_since = tokio::time::Instant::now();
            }
            Ok(None) => {
                // Tap closed (shouldn't happen in capture, but be safe).
                break;
            }
            Err(_) => {
                // Timeout — idle period elapsed or deadline reached.
                break;
            }
        }
    }

    frames
}

/// Drains remaining frames from the tap (used between entries).
async fn drain_tap(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {}
}

/// Loads the manifest entries for `kind`.
fn load_manifest(kind: &AdapterKind) -> Result<Vec<CaptureEntry>, String> {
    let content = std::fs::read_to_string(MANIFEST_PATH)
        .map_err(|e| format!("failed to read manifest: {}", e))?;
    let manifest: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|e| format!("failed to parse manifest: {}", e))?;

    let wire_name = kind.wire_name();
    let entries = manifest
        .get(wire_name)
        .ok_or_else(|| format!("no '{}' section in manifest", wire_name))?;

    let seq = entries
        .as_sequence()
        .ok_or_else(|| format!("'{}' section in manifest is not a list", wire_name))?;

    seq.iter()
        .map(|v| serde_yaml::from_value(v.clone()).map_err(|e| format!("invalid entry: {}", e)))
        .collect()
}

/// Builds an adapter instance for `kind` from the manifest entry.
fn build_adapter(
    kind: &AdapterKind,
    cwd: PathBuf,
    entry: &CaptureEntry,
) -> Result<Arc<dyn Adapter>, String> {
    match kind {
        AdapterKind::Claude => {
            let opts: ClaudeStartupOptions = entry
                .startup_options
                .as_ref()
                .map(|v| serde_json::from_value(v.clone()))
                .unwrap_or(Ok(ClaudeStartupOptions::default()))
                .map_err(|e| format!("failed to parse Claude startup options: {}", e))?;

            let adapter = crate::adapter::claude::ClaudeAdapter::new(
                opts,
                cwd,
                Vec::new(),
                RunId::new(),
                TaskId::new(),
                WorkerId::new(),
                None,
            );
            Ok(Arc::new(adapter))
        }
        AdapterKind::Codex => {
            let opts: CodexStartupOptions = entry
                .startup_options
                .as_ref()
                .map(|v| serde_json::from_value(v.clone()))
                .unwrap_or(Ok(CodexStartupOptions::default()))
                .map_err(|e| format!("failed to parse Codex startup options: {}", e))?;

            let adapter = crate::adapter::codex::CodexAdapter::new(cwd, opts, Vec::new(), None);
            Ok(Arc::new(adapter))
        }
        AdapterKind::Copilot => {
            let opts: CopilotStartupOptions = entry
                .startup_options
                .as_ref()
                .map(|v| serde_json::from_value(v.clone()))
                .unwrap_or(Ok(CopilotStartupOptions::default()))
                .map_err(|e| format!("failed to parse Copilot startup options: {}", e))?;

            let adapter = crate::adapter::copilot::CopilotAdapter::new(
                PathBuf::from("copilot"),
                cwd,
                opts,
                Vec::new(),
                RunId::new(),
                TaskId::new(),
                WorkerId::new(),
                None,
            );
            Ok(Arc::new(adapter))
        }
        AdapterKind::OmpRpc => {
            let model = entry.model.as_deref().ok_or_else(|| {
                format!(
                    "ompRpc capture entry '{}' requires a model selector",
                    entry.fixture
                )
            })?;

            let profile = crate::adapter::omp_rpc::conformance::conformance_profile(model);
            let adapter = crate::adapter::omp_rpc::OmpRpcAdapter::new(
                profile,
                OmpRpcAdapterOptions::default(),
                None,
            );
            Ok(Arc::new(adapter))
        }
    }
}

/// A no-op [`AdapterEventSink`] that discards everything.
/// Capture wants raw frames from the tap, not normalized events.
struct DiscardingSink;

impl AdapterEventSink for DiscardingSink {
    fn emit(&self, _event: AdapterEvent) -> AdapterFuture<'_, u64> {
        Box::pin(std::future::ready(Ok(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn adapter_fixture_dir_maps_correctly() {
        assert_eq!(adapter_fixture_dir(AdapterKind::Claude), "claude");
        assert_eq!(adapter_fixture_dir(AdapterKind::Codex), "codex");
        assert_eq!(adapter_fixture_dir(AdapterKind::Copilot), "copilot");
        assert_eq!(adapter_fixture_dir(AdapterKind::OmpRpc), "omp-rpc");
    }

    #[test]
    fn load_manifest_parses_claude() {
        let entries = load_manifest(&AdapterKind::Claude);
        assert!(entries.is_ok());
        let entries = entries.unwrap();
        assert!(!entries.is_empty());
        assert!(entries.iter().any(|e| e.fixture == "initialize.jsonl"));
    }

    #[test]
    fn load_manifest_parses_omprpc() {
        let entries = load_manifest(&AdapterKind::OmpRpc);
        assert!(entries.is_ok());
        let entries = entries.unwrap();
        assert!(!entries.is_empty());
        // OMP-RPC entries must have a model.
        for entry in &entries {
            assert!(entry.model.is_some(), "ompRpc entry must have a model");
        }
    }

    #[test]
    fn manifest_fixtures_are_scrub_render_fixed_points() {
        let kinds = [
            AdapterKind::Claude,
            AdapterKind::Codex,
            AdapterKind::Copilot,
            AdapterKind::OmpRpc,
        ];
        let mut manifest_paths = BTreeSet::new();

        for &kind in &kinds {
            for entry in load_manifest(&kind).expect("manifest must load") {
                let qualified_fixture = format!("{}/{}", adapter_fixture_dir(kind), entry.fixture);
                assert!(
                    manifest_paths.insert(qualified_fixture),
                    "manifest must not list a fixture more than once"
                );

                let fixture_path = PathBuf::from(FIXTURES_DIR)
                    .join(adapter_fixture_dir(kind))
                    .join(&entry.fixture);
                assert!(
                    fixture_path.is_file(),
                    "manifest target must exist: {}",
                    fixture_path.display()
                );
                let existing = std::fs::read_to_string(&fixture_path)
                    .expect("manifest target must be readable");
                let mut scrubber = Scrubber::new("/workspace/crew".into());
                let frames: Vec<String> = if entry.fixture.ends_with(".jsonl") {
                    existing
                        .lines()
                        .filter(|line| !line.is_empty())
                        .filter_map(|line| {
                            scrub_captured_frame(kind, &mut scrubber, line.as_bytes())
                        })
                        .collect()
                } else if entry.fixture.ends_with(".json") {
                    scrub_captured_frame(kind, &mut scrubber, existing.as_bytes())
                        .into_iter()
                        .collect()
                } else {
                    panic!("unsupported fixture extension: {}", entry.fixture);
                };
                let rendered =
                    render_fixture_content(&entry.fixture, &frames).expect("fixture must render");

                assert_eq!(
                    rendered,
                    existing,
                    "manifest fixture must be a fixed point: {}",
                    fixture_path.display()
                );
            }
        }

        let mut discovered_paths = BTreeSet::new();
        for &kind in &kinds {
            let fixture_dir = PathBuf::from(FIXTURES_DIR).join(adapter_fixture_dir(kind));
            for entry in
                std::fs::read_dir(&fixture_dir).expect("fixture directory must be readable")
            {
                let entry = entry.expect("fixture directory entry must be readable");
                let file_name = entry.file_name();
                if file_name.to_string_lossy().starts_with('.') {
                    continue;
                }

                assert!(
                    entry
                        .file_type()
                        .expect("fixture type must be readable")
                        .is_file(),
                    "fixture directory must contain files: {}",
                    entry.path().display()
                );
                discovered_paths.insert(format!(
                    "{}/{}",
                    adapter_fixture_dir(kind),
                    file_name.to_string_lossy()
                ));
            }
        }

        let expected_paths = manifest_paths
            .union(&BTreeSet::from([
                "codex/schema-version.json".to_string(),
                "claude/result.jsonl".to_string(),
            ]))
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(
            discovered_paths, expected_paths,
            "every adapter fixture must be manifest-managed or explicitly excluded"
        );
    }

    #[test]
    fn persist_fixture_content_returns_true_for_equal_existing_bytes() {
        let dir = tempfile::tempdir().expect("temp directory must be created");
        let fixture_path = dir.path().join("fixture.jsonl");
        std::fs::write(&fixture_path, "canonical\n").expect("fixture must be seeded");

        assert!(
            persist_fixture_content(&fixture_path, "canonical\n", false)
                .expect("persistence must succeed")
        );
        assert_eq!(
            std::fs::read_to_string(&fixture_path).expect("fixture must remain readable"),
            "canonical\n"
        );
    }

    #[test]
    fn persist_fixture_content_replaces_differing_existing_bytes() {
        let dir = tempfile::tempdir().expect("temp directory must be created");
        let fixture_path = dir.path().join("fixture.jsonl");
        std::fs::write(&fixture_path, "old\n").expect("fixture must be seeded");

        assert!(
            !persist_fixture_content(&fixture_path, "new\n", false)
                .expect("persistence must succeed")
        );
        assert_eq!(
            std::fs::read_to_string(&fixture_path).expect("fixture must be replaced"),
            "new\n"
        );
    }

    #[test]
    fn persist_fixture_content_creates_missing_file_as_changed() {
        let dir = tempfile::tempdir().expect("temp directory must be created");
        let fixture_path = dir.path().join("nested/fixture.jsonl");

        assert!(
            !persist_fixture_content(&fixture_path, "new\n", false)
                .expect("persistence must succeed")
        );
        assert_eq!(
            std::fs::read_to_string(&fixture_path).expect("fixture must be created"),
            "new\n"
        );
    }

    #[test]
    fn persist_fixture_content_dry_run_reports_equal_without_mutating() {
        let dir = tempfile::tempdir().expect("temp directory must be created");
        let fixture_path = dir.path().join("fixture.jsonl");
        std::fs::write(&fixture_path, "canonical\n").expect("fixture must be seeded");

        assert!(
            persist_fixture_content(&fixture_path, "canonical\n", true)
                .expect("dry-run comparison must succeed")
        );
        assert_eq!(
            std::fs::read_to_string(&fixture_path).expect("fixture must remain readable"),
            "canonical\n"
        );
    }

    #[test]
    fn persist_fixture_content_dry_run_reports_differences_without_mutating() {
        let dir = tempfile::tempdir().expect("temp directory must be created");
        let fixture_path = dir.path().join("fixture.jsonl");
        std::fs::write(&fixture_path, "old\n").expect("fixture must be seeded");

        assert!(
            !persist_fixture_content(&fixture_path, "new\n", true)
                .expect("dry-run comparison must succeed")
        );
        assert_eq!(
            std::fs::read_to_string(&fixture_path).expect("fixture must remain readable"),
            "old\n"
        );
    }

    #[test]
    fn persist_fixture_content_dry_run_reports_missing_without_creating() {
        let dir = tempfile::tempdir().expect("temp directory must be created");
        let fixture_path = dir.path().join("nested/fixture.jsonl");

        assert!(
            !persist_fixture_content(&fixture_path, "new\n", true)
                .expect("dry-run comparison must succeed")
        );
        assert!(!fixture_path.exists(), "dry-run must not create a fixture");
    }

    #[test]
    fn render_json_fixture_as_pretty_document_with_trailing_newline() {
        let rendered = render_fixture_content(
            "initialize-v1.json",
            &[r#"{"z":1,"a":{"b":2}}"#.to_string()],
        )
        .expect("single JSON frame must render");

        assert_eq!(
            rendered,
            "{\n  \"z\": 1,\n  \"a\": {\n    \"b\": 2\n  }\n}\n"
        );
    }

    #[test]
    fn render_json_fixture_rejects_multiple_frames() {
        let result = render_fixture_content(
            "initialize-v1.json",
            &[
                r#"{"first":true}"#.to_string(),
                r#"{"second":true}"#.to_string(),
            ],
        );

        assert!(
            result.is_err(),
            "JSON fixtures must contain exactly one frame"
        );
    }

    #[test]
    fn scrub_captured_frame_applies_each_adapter_reader_policy() {
        let raw = b"cwd=/tmp/capture-123 token=sk-ABCDEFGHIJKLMNOPQRSTUVWX";

        let mut omp_rpc_scrubber = Scrubber::new("/tmp/capture-123".into());
        assert_eq!(
            scrub_captured_frame(AdapterKind::OmpRpc, &mut omp_rpc_scrubber, raw),
            None
        );

        let mut claude_scrubber = Scrubber::new("/tmp/capture-123".into());
        assert_eq!(
            scrub_captured_frame(AdapterKind::Claude, &mut claude_scrubber, b"vendor banner"),
            None
        );

        let mut codex_scrubber = Scrubber::new("/tmp/capture-123".into());
        assert_eq!(
            scrub_captured_frame(AdapterKind::Codex, &mut codex_scrubber, raw),
            None
        );

        let mut copilot_scrubber = Scrubber::new("/tmp/capture-123".into());
        assert_eq!(
            scrub_captured_frame(AdapterKind::Copilot, &mut copilot_scrubber, raw),
            None
        );
    }

    #[test]
    fn render_fixture_content_rejects_unsupported_extensions() {
        let error = render_fixture_content("capture.txt", &["frame".into()])
            .expect_err("unsupported fixture extension must fail");

        assert!(error.contains("capture.txt"));
    }

    #[test]
    fn render_json_fixture_invalid_frame_error_names_fixture_and_bounds_preview() {
        let frame = format!("not-json-{}-tail", "x".repeat(1024));
        let error = render_fixture_content("broken.json", std::slice::from_ref(&frame))
            .expect_err("invalid JSON fixture frame must fail");

        assert!(error.contains("broken.json"));
        assert!(error.contains("not-json-"));
        assert!(!error.contains("-tail"));
        assert!(
            error.len() < frame.len(),
            "error preview must be bounded instead of repeating the full frame"
        );
    }

    #[test]
    fn persist_fixture_content_dry_run_errors_for_a_directory_target() {
        let dir = tempfile::tempdir().expect("temp directory must be created");
        let error = persist_fixture_content(dir.path(), "new\n", true)
            .expect_err("dry-run must surface unreadable targets");

        assert!(error.contains(&dir.path().display().to_string()));
    }
}
