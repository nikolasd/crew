//! Security-critical redaction boundary test.
//!
//! Pushes a raw fixture containing visible text, a secret-classified token,
//! a thinking block, a generic API-key-shaped string, and an
//! Anthropic-shaped (`sk-ant-api03-...`) key -- the last two embedded in
//! otherwise visible text -- through the append path, then scans the raw
//! bytes of the database file, the WAL file (whichever exist --
//! checkpointed or not), the runtime log, and the replay output. Visible
//! text must survive; every raw secret and thinking byte sequence must be
//! entirely absent.
//!
//! The runtime log is made genuinely load-bearing here: a real
//! `tracing_subscriber` is installed, writing to the exact
//! `RuntimePaths::log` location, around the append flow, so `runtime.log`
//! receives real bytes from the code under test (the database actor emits
//! one content-free `tracing::debug!(sequence, ...)` per append -- see
//! `db/actor.rs`) rather than being scanned only "if it happens to exist".
//!
//! It also proves the operation-intent/acknowledgement path -- not just
//! the event-append path -- is redacted before it reaches the durable
//! store: an API-key-shaped string embedded in an intent JSON payload must
//! not survive into the db/WAL bytes, only its `SanitizedJson` form.

use std::fs::{self, File};
use std::io;
use std::sync::{Arc, Mutex};

use crew_protocol::{Classified, ContentClass, DiagnosticLevel, OperationId, ProjectId, Timestamp};
use crew_runtime::RuntimePaths;
use crew_runtime::db::DatabaseHandle;
use crew_runtime::security::redaction::{RawEventKind, RawRuntimeEvent, Redactor};

const VISIBLE_PLAIN: &str = "worker requested deployment approval for the prod cluster";
const SECRET_TOKEN: &str = "SECRET_TOKEN_ABC123XYZ";
const THINKING_TEXT: &str = "chain-of-thought: consider bypassing the approval gate quietly";
const API_KEY: &str = "sk-ABCDEFGHIJKLMNOPQRSTUVWX";
const INTENT_API_KEY: &str = "sk-INTENTKEYABCDEFGHIJKLMNOP";
/// A real vendor key shape: hyphens and base64url underscores inside the
/// token, which the pre-R49 `sk-[A-Za-z0-9]{16,}` pattern could not match.
const ANTHROPIC_API_KEY: &str = "sk-ant-api03-BOUNDARYFAKE-for-tests_0123456789-abcdef";

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// A `Write` implementation that shares a single underlying file across
/// every `tracing` event, rather than opening/truncating it repeatedly.
#[derive(Clone)]
struct SharedFileWriter(Arc<Mutex<File>>);

impl io::Write for SharedFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("log file mutex not poisoned")
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().expect("log file mutex not poisoned").flush()
    }
}

/// Installs a real `tracing_subscriber` writing to `log_path`, as a
/// **process-global** default rather than a thread-local (`with_default`)
/// one: the database actor runs on its own dedicated `std::thread` (see
/// `db/actor.rs`), which never inherits a calling thread's thread-local
/// subscriber override, so only a global default is actually visible to
/// the code under test. This test file contains the sole test in its own
/// binary, so installing it once here does not leak across tests.
fn install_log_subscriber(log_path: &std::path::Path) -> SharedFileWriter {
    let file = File::create(log_path).expect("must be able to create runtime.log for the test");
    let writer = SharedFileWriter(Arc::new(Mutex::new(file)));
    let for_subscriber = writer.clone();

    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || for_subscriber.clone())
        .with_ansi(false)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("no global tracing subscriber should be set yet in this test binary");

    writer
}

#[tokio::test]
async fn redaction_boundary_holds_across_database_wal_log_and_replay() {
    let repo_dir = tempfile::tempdir().unwrap();
    let state_root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(state_root.path(), repo_dir.path()).unwrap();

    let db_path = paths.database.clone();
    let wal_path = {
        let mut wal = db_path.clone().into_os_string();
        wal.push("-wal");
        std::path::PathBuf::from(wal)
    };
    let log_path = paths.log.clone();

    // Install the subscriber before starting the actor, so its dedicated
    // thread picks up the global default from the moment it spawns.
    let log_writer = install_log_subscriber(&log_path);

    let handle = DatabaseHandle::start(db_path.clone()).await.unwrap();
    let redactor = Redactor::new();

    let raw = RawRuntimeEvent {
        timestamp: Timestamp::now(),
        project_id: ProjectId::new(),
        run_id: None,
        kind: RawEventKind::Diagnostic {
            level: DiagnosticLevel::Info,
            code: "redaction-boundary-fixture".to_string(),
            fragments: vec![
                Classified {
                    class: ContentClass::Visible,
                    value: VISIBLE_PLAIN.to_string(),
                },
                Classified {
                    class: ContentClass::Secret,
                    value: SECRET_TOKEN.to_string(),
                },
                Classified {
                    class: ContentClass::Thinking,
                    value: THINKING_TEXT.to_string(),
                },
                Classified {
                    class: ContentClass::Visible,
                    value: format!("found a leaked key {API_KEY} in the config dump"),
                },
                Classified {
                    class: ContentClass::Visible,
                    value: format!("vendor echoed {ANTHROPIC_API_KEY} in an error"),
                },
            ],
        },
    };

    let sanitized = redactor.sanitize(raw);
    let sanitized_json = sanitized.event_json().to_string();

    // The redacted event JSON itself, still only in bounded process memory,
    // must already have dropped the secret/thinking bytes and the raw key.
    assert!(!sanitized_json.contains(SECRET_TOKEN));
    assert!(!sanitized_json.contains(THINKING_TEXT));
    assert!(!sanitized_json.contains(API_KEY));
    assert!(!sanitized_json.contains(ANTHROPIC_API_KEY));
    assert!(sanitized_json.contains(VISIBLE_PLAIN));

    handle.append_event(sanitized).await.unwrap();

    // Also exercise the operation intent/acknowledgement path: an
    // API-key-shaped string embedded in a raw JSON payload must be
    // redacted by `Redactor::sanitize_json` before it can reach the
    // durable `operations` table.
    let operation_id = OperationId::new();
    handle
        .record_operation_intent(
            operation_id,
            "spawn_worker",
            redactor.sanitize_json(&serde_json::json!({
                "worker": "example",
                "credential": format!("leaked key {INTENT_API_KEY} inline"),
            })),
            Timestamp::now(),
        )
        .await
        .unwrap();
    handle
        .acknowledge_operation(
            operation_id,
            redactor.sanitize_json(&serde_json::json!({"status": "ok"})),
        )
        .await
        .unwrap();

    let replayed = handle.replay_events(0).await.unwrap();
    assert_eq!(replayed.len(), 1);
    let replayed_json = replayed[0].event_json.clone();

    handle.shutdown().await.unwrap();

    // Flush the log writer explicitly before reading it back: the actor
    // thread has already joined (via `shutdown`), but be defensive about
    // any buffering `tracing_subscriber` itself might do.
    {
        use std::io::Write as _;
        log_writer
            .0
            .lock()
            .expect("log file mutex not poisoned")
            .flush()
            .unwrap();
    }

    let db_bytes = fs::read(&db_path).unwrap();
    let wal_bytes = if wal_path.exists() {
        fs::read(&wal_path).unwrap()
    } else {
        Vec::new()
    };

    // Unlike the WAL file (which may or may not exist depending on
    // whether SQLite has checkpointed it away), the runtime log MUST
    // exist: the actor emits a tracing event on every successful append,
    // and the subscriber installed above is wired to write it to
    // `log_path`. A missing file here is a real test failure, not a
    // silently-skipped assertion.
    assert!(
        log_path.exists(),
        "runtime.log must exist -- the database actor emits a tracing event on every append"
    );
    let log_bytes = fs::read(&log_path).unwrap();
    assert!(
        !log_bytes.is_empty(),
        "runtime.log must contain the append-event tracing output"
    );
    assert!(
        contains_bytes(&log_bytes, b"event appended"),
        "runtime.log must contain the content-free append-event marker"
    );

    for (label, haystack) in [
        ("database file", db_bytes.as_slice()),
        ("wal file", wal_bytes.as_slice()),
        ("runtime log", log_bytes.as_slice()),
        ("replay output", replayed_json.as_bytes()),
    ] {
        assert!(
            !contains_bytes(haystack, SECRET_TOKEN.as_bytes()),
            "{label} leaked the raw secret token"
        );
        assert!(
            !contains_bytes(haystack, THINKING_TEXT.as_bytes()),
            "{label} leaked the raw thinking text"
        );
        assert!(
            !contains_bytes(haystack, API_KEY.as_bytes()),
            "{label} leaked the raw API key"
        );
        assert!(
            !contains_bytes(haystack, INTENT_API_KEY.as_bytes()),
            "{label} leaked the raw operation-intent API key"
        );
        assert!(
            !contains_bytes(haystack, ANTHROPIC_API_KEY.as_bytes()),
            "{label} leaked the raw Anthropic-shaped API key"
        );
    }

    // Visible text must survive verbatim in the durable store and replay.
    assert!(contains_bytes(&db_bytes, VISIBLE_PLAIN.as_bytes()));
    assert!(replayed_json.contains(VISIBLE_PLAIN));

    // The API-key-shaped substring must have been replaced by the redaction
    // marker, while the surrounding visible text around it survives.
    assert!(replayed_json.contains("[REDACTED:api_key]"));
    assert!(replayed_json.contains("found a leaked key"));
    assert!(replayed_json.contains("in the config dump"));

    // The operation-intent payload, once redacted, must contain the marker
    // rather than the raw key, and the durable db bytes must contain the
    // sanitized form's marker (proving `SanitizedJson` -- not raw text --
    // reached the `operations` table).
    assert!(contains_bytes(&db_bytes, b"[REDACTED:api_key]"));
}
