//! Integration tests for the TUI transcript tailer: byte-offset cursor
//! math (only complete lines consumed; idempotent re-parse from any
//! split), unknown/malformed entry degradation to `Raw`, the polling
//! tailer itself against a real file, and nonce-based transcript
//! discovery under a vendor session root.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crew_protocol::ContentClass;
use crew_runtime::adapter::tui::{
    Cursor, TranscriptFormat, TranscriptTailer, TuiEvent, find_transcript_by_nonce,
    parse_jsonl_chunk,
};

/// A minimal vendor-shaped JSONL format for tests:
/// `{"type":"text","text":"...","id":"..."}` becomes `AssistantText`;
/// anything else degrades to `Raw` carrying its `type`.
struct TestFormat;

impl TranscriptFormat for TestFormat {
    fn parse(&self, raw: &[u8], cursor: &Cursor) -> Vec<(TuiEvent, Cursor)> {
        parse_jsonl_chunk(raw, cursor, |value| {
            let entry_id = value
                .get("id")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            let event = match value.get("type").and_then(|v| v.as_str()) {
                Some("text") => TuiEvent::AssistantText {
                    text: crew_protocol::Classified {
                        class: ContentClass::Visible,
                        value: value
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    },
                    is_question: false,
                    ts: None,
                },
                Some(other) => TuiEvent::Raw {
                    entry_type: other.to_string(),
                },
                None => TuiEvent::Raw {
                    entry_type: "missing_type".to_string(),
                },
            };
            (vec![event], entry_id)
        })
    }
}

fn text_of(event: &TuiEvent) -> Option<&str> {
    match event {
        TuiEvent::AssistantText { text, .. } => Some(text.value.as_str()),
        _ => None,
    }
}

// --------------------------------------------------------- cursor math

#[test]
fn a_partial_trailing_line_is_held_back_and_not_consumed() {
    let data = b"{\"type\":\"text\",\"text\":\"one\",\"id\":\"a\"}\n{\"type\":\"text\",\"te";
    let tagged = TestFormat.parse(data, &Cursor::start());
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    let cursor = tagged
        .last()
        .map(|(_, c)| c.clone())
        .unwrap_or_else(Cursor::start);

    assert_eq!(events.len(), 1);
    assert_eq!(text_of(&events[0]), Some("one"));
    // Only the complete first line (plus its newline) is consumed.
    let first_line_len = data.iter().position(|&b| b == b'\n').unwrap() as u64 + 1;
    assert_eq!(cursor.offset, first_line_len);
    assert_eq!(cursor.last_entry_id.as_deref(), Some("a"));
}

#[test]
fn chunked_parse_at_arbitrary_splits_equals_one_shot_parse() {
    let data: Vec<u8> = [
        r#"{"type":"text","text":"héllo – 🚀","id":"e1"}"#,
        r#"{"type":"tool_use","id":"e2"}"#,
        r#"not json at all"#,
        r#"{"type":"text","text":"final","id":"e3"}"#,
    ]
    .join("\n")
    .into_bytes()
    .into_iter()
    .chain(std::iter::once(b'\n'))
    .collect();

    let oneshot_tagged = TestFormat.parse(&data, &Cursor::start());
    let oneshot_events: Vec<TuiEvent> = oneshot_tagged.iter().map(|(e, _)| e.clone()).collect();
    let oneshot_cursor = oneshot_tagged
        .last()
        .map(|(_, c)| c.clone())
        .unwrap_or_else(Cursor::start);
    assert_eq!(oneshot_events.len(), 4);
    assert_eq!(oneshot_cursor.offset, data.len() as u64);

    // Splits chosen to land mid-line, mid-multi-byte-char, on a newline,
    // and near the end -- the chunked walk must converge to the same
    // events and final cursor regardless.
    for split in [10usize, 24, 47, data.len() - 3] {
        let mut events = Vec::new();
        let mut cursor = Cursor::start();

        let first_tagged = TestFormat.parse(&data[..split], &cursor);
        let first_events: Vec<TuiEvent> = first_tagged.iter().map(|(e, _)| e.clone()).collect();
        events.extend(first_events);
        cursor = first_tagged
            .last()
            .map(|(_, c)| c.clone())
            .unwrap_or_else(Cursor::start);
        let rest_tagged = TestFormat.parse(&data[cursor.offset as usize..], &cursor);
        let rest_events: Vec<TuiEvent> = rest_tagged.iter().map(|(e, _)| e.clone()).collect();
        events.extend(rest_events);
        cursor = rest_tagged
            .last()
            .map(|(_, c)| c.clone())
            .unwrap_or_else(Cursor::start);

        assert_eq!(
            events.len(),
            oneshot_events.len(),
            "split at {split} must yield the same events"
        );
        assert_eq!(cursor.offset, oneshot_cursor.offset, "split at {split}");
        assert_eq!(
            cursor.last_entry_id, oneshot_cursor.last_entry_id,
            "split at {split}"
        );
    }
}

#[test]
fn a_malformed_line_degrades_to_raw_with_parse_error() {
    let data = b"this is not json\n{\"type\":\"text\",\"text\":\"ok\",\"id\":\"z\"}\n";
    let tagged = TestFormat.parse(data, &Cursor::start());
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    let cursor = tagged
        .last()
        .map(|(_, c)| c.clone())
        .unwrap_or_else(Cursor::start);

    assert_eq!(events.len(), 2);
    assert!(
        matches!(&events[0], TuiEvent::Raw { entry_type } if entry_type == "parse_error"),
        "malformed JSON must degrade to Raw {{ parse_error }}, got {:?}",
        events[0]
    );
    assert_eq!(text_of(&events[1]), Some("ok"));
    assert_eq!(cursor.offset, data.len() as u64);
}

#[test]
fn multi_byte_utf8_advances_the_cursor_by_bytes_not_chars() {
    let line = r#"{"type":"text","text":"日本語テキスト🚀","id":"u"}"#;
    let data = format!("{line}\n").into_bytes();
    let tagged = TestFormat.parse(&data, &Cursor::start());
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    let cursor = tagged
        .last()
        .map(|(_, c)| c.clone())
        .unwrap_or_else(Cursor::start);

    assert_eq!(events.len(), 1);
    assert_eq!(
        cursor.offset,
        data.len() as u64,
        "cursor must advance by byte length, not char count"
    );
    assert!(data.len() > line.chars().count() + 1);
}

#[test]
fn blank_lines_are_consumed_without_producing_events() {
    let data = b"\n\n{\"type\":\"text\",\"text\":\"after blanks\",\"id\":\"b\"}\n";
    let tagged = TestFormat.parse(data, &Cursor::start());
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    let cursor = tagged
        .last()
        .map(|(_, c)| c.clone())
        .unwrap_or_else(Cursor::start);

    assert_eq!(events.len(), 1);
    assert_eq!(text_of(&events[0]), Some("after blanks"));
    assert_eq!(cursor.offset, data.len() as u64);
}

// ------------------------------------------------------------- tailer

fn write_line(path: &std::path::Path, line: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
    file.flush().unwrap();
}

#[tokio::test]
async fn poll_once_returns_new_events_then_none_until_more_lines_arrive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_line(&path, r#"{"type":"text","text":"first","id":"1"}"#);

    let mut tailer = TranscriptTailer::new(
        path.clone(),
        Arc::new(TestFormat),
        Cursor::start(),
        Duration::from_millis(10),
    );

    let (tagged, cursor) = tailer.poll_once().await.expect("first poll sees the line");
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    assert_eq!(events.len(), 1);
    assert!(cursor.offset > 0);

    assert!(
        tailer.poll_once().await.is_none(),
        "no new complete lines means None"
    );

    // A partial line (no trailing newline) must not be consumed.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(br#"{"type":"text","te"#).unwrap();
    file.flush().unwrap();
    assert!(
        tailer.poll_once().await.is_none(),
        "a partial trailing line must be held back"
    );

    // Completing the line makes it visible.
    file.write_all(b"xt\":\"second\",\"id\":\"2\"}\n").unwrap();
    file.flush().unwrap();
    let (tagged2, _) = tailer.poll_once().await.expect("completed line arrives");
    let events: Vec<TuiEvent> = tagged2.iter().map(|(e, _)| e.clone()).collect();
    assert_eq!(events.len(), 1);
    assert_eq!(text_of(&events[0]), Some("second"));
}

#[tokio::test]
async fn spawned_tailer_delivers_batches_and_stops_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    write_line(&path, r#"{"type":"text","text":"pre-existing","id":"p"}"#);

    let tailer = TranscriptTailer::new(
        path.clone(),
        Arc::new(TestFormat),
        Cursor::start(),
        Duration::from_millis(10),
    );

    let (batch_tx, mut batch_rx) =
        tokio::sync::mpsc::unbounded_channel::<(Vec<(TuiEvent, Cursor)>, Cursor)>();
    let handle = tailer.spawn(move |tagged, cursor| {
        let _ = batch_tx.send((tagged, cursor));
    });

    let (tagged, _cursor) = tokio::time::timeout(Duration::from_secs(2), batch_rx.recv())
        .await
        .expect("the pre-existing line must arrive promptly")
        .expect("channel open");
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    assert_eq!(text_of(&events[0]), Some("pre-existing"));

    write_line(&path, r#"{"type":"text","text":"appended","id":"q"}"#);
    let (tagged, cursor) = tokio::time::timeout(Duration::from_secs(2), batch_rx.recv())
        .await
        .expect("the appended line must arrive promptly")
        .expect("channel open");
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    assert_eq!(text_of(&events[0]), Some("appended"));
    assert_eq!(cursor.last_entry_id.as_deref(), Some("q"));

    handle.stop();
    // After stop, further appends are not delivered.
    write_line(&path, r#"{"type":"text","text":"after stop","id":"r"}"#);
    let after = tokio::time::timeout(Duration::from_millis(200), batch_rx.recv()).await;
    assert!(
        after.is_err() || after.unwrap().is_none(),
        "a stopped tailer must not keep delivering"
    );
}

// ---------------------------------------------------------- discovery

#[tokio::test]
async fn discovery_finds_a_nonce_bearing_transcript_created_after_start() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("project-slug");
    std::fs::create_dir_all(&nested).unwrap();

    // An older transcript containing the nonce must be ignored: only
    // files modified at/after `started_at` qualify.
    let old = nested.join("old-session.jsonl");
    write_line(&old, r#"{"note":"stale [crew:nonce-123] mention"}"#);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started_at = SystemTime::now();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A decoy without the nonce, and the real transcript with it.
    let decoy = nested.join("decoy-session.jsonl");
    write_line(&decoy, r#"{"note":"no nonce here"}"#);
    let target = nested.join("new-session.jsonl");
    write_line(&target, r#"{"message":"prompt with [crew:nonce-123] tag"}"#);

    let found = find_transcript_by_nonce(
        dir.path(),
        started_at,
        "[crew:nonce-123]",
        Duration::from_secs(2),
    )
    .await
    .expect("must find the nonce-bearing transcript");
    assert_eq!(found, target);
}

#[tokio::test]
async fn discovery_times_out_cleanly_when_no_transcript_matches() {
    let dir = tempfile::tempdir().unwrap();
    write_line(
        &dir.path().join("unrelated.jsonl"),
        r#"{"note":"nothing relevant"}"#,
    );
    // A nonce-bearing file with the wrong extension must not count.
    write_line(
        &dir.path().join("notes.txt"),
        r#"contains [crew:nonce-xyz] but is not a transcript"#,
    );

    let started_at = SystemTime::now();
    let started = std::time::Instant::now();
    let result = find_transcript_by_nonce(
        dir.path(),
        started_at,
        "[crew:nonce-xyz]",
        Duration::from_millis(400),
    )
    .await;
    assert!(result.is_err(), "no match must yield a typed timeout error");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "timeout must be honored promptly"
    );
    let message = result.unwrap_err().to_string();
    assert!(
        message.contains("nonce-xyz"),
        "the error must name the nonce: {message}"
    );
}

#[tokio::test]
async fn discovery_sees_a_transcript_that_appears_while_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let root: PathBuf = dir.path().to_path_buf();
    let started_at = SystemTime::now();

    let writer_root = root.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        write_line(
            &writer_root.join("late-session.jsonl"),
            r#"{"message":"[crew:late-nonce] arrived late"}"#,
        );
    });

    let found = find_transcript_by_nonce(
        &root,
        started_at,
        "[crew:late-nonce]",
        Duration::from_secs(3),
    )
    .await
    .expect("a transcript appearing mid-wait must be found");
    assert_eq!(found, root.join("late-session.jsonl"));
    writer.await.unwrap();
}

#[tokio::test]
async fn discovery_rejects_empty_nonce() {
    let dir = tempfile::tempdir().unwrap();
    let result =
        find_transcript_by_nonce(dir.path(), SystemTime::now(), "", Duration::from_secs(1)).await;
    assert!(result.is_err(), "empty nonce must result in an error");
    let error_msg = result.unwrap_err().to_string();
    assert!(
        error_msg.contains("non-empty"),
        "error must indicate invalid nonce: {error_msg}"
    );
}
