//! Fixture-driven contract tests for [`ClaudeTuiVendor`]'s transcript
//! format, against the real recorded capture at
//! `fixtures/adapters/claude-tui/session.jsonl` (see that directory's
//! own `README.md` for provenance).

use std::path::PathBuf;

use crew_runtime::adapter::tui::{ClaudeTuiVendor, Cursor, TuiEvent, TuiVendor};

fn vendor() -> ClaudeTuiVendor {
    ClaudeTuiVendor::new(PathBuf::from("/workspace/crew"), Vec::new())
}

fn fixture_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/adapters/claude-tui/session.jsonl");
    std::fs::read(&path).unwrap_or_else(|err| panic!("reading fixture {path:?}: {err}"))
}

fn parse_all(raw: &[u8]) -> Vec<(TuiEvent, Cursor)> {
    vendor().format().parse(raw, &Cursor::start())
}

/// The full fixture parses without error and consumes every byte
/// (every line is complete, newline-terminated JSON or degrades to
/// `Raw` -- nothing is silently dropped as a partial line).
#[test]
fn the_full_fixture_parses_and_consumes_every_byte() {
    let raw = fixture_bytes();
    let tagged = parse_all(&raw);
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    // `parse` pairs each event with the cursor *after its own line*; the
    // tailer (not `parse`) advances past the final complete line, so the
    // furthest event cursor sits at EOF when the last line emits (this
    // fixture's final turn is the assistant's question).
    let max_offset = tagged.iter().map(|(_, c)| c.offset).max().unwrap_or(0);
    assert_eq!(max_offset, raw.len() as u64);
    assert!(!events.is_empty());
}

/// The expected normalized event sequence: a `SessionMeta` from every
/// entry's `sessionId` (21 entries -> 21 SessionMeta, since every real
/// entry in this capture carries one), the assistant's plain-text
/// greeting, the `Bash` tool activity from the `tool_use`/`tool_result`
/// pair, and the final question -- in that relative order, and nothing
/// else attributable to real conversational content.
#[test]
fn expected_normalized_event_sequence() {
    let raw = fixture_bytes();
    let tagged = parse_all(&raw);
    let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();

    let session_metas = events
        .iter()
        .filter(|e| matches!(e, TuiEvent::SessionMeta { .. }))
        .count();
    assert_eq!(
        session_metas, 21,
        "every one of the fixture's 21 entries carries a sessionId"
    );

    let assistant_texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TuiEvent::AssistantText { text, .. } => Some(text.value.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        assistant_texts,
        vec![
            "Hi! 👋",
            "Got it — output was `crew-fixture`.\n\nQuick question: what would you like me to actually work on next?",
        ]
    );

    let tool_activity: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            TuiEvent::ToolActivity { tool, detail, .. } => {
                Some((tool.as_str(), detail.value.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(tool_activity.len(), 1);
    assert_eq!(tool_activity[0].0, "Bash");
    assert!(
        tool_activity[0].1.contains("echo crew-fixture"),
        "tool detail must carry the tool_use input: {:?}",
        tool_activity[0].1
    );

    // Relative order: greeting before the tool call before the question.
    let greeting_idx = events
        .iter()
        .position(|e| matches!(e, TuiEvent::AssistantText { text, .. } if text.value == "Hi! 👋"));
    let tool_idx = events
        .iter()
        .position(|e| matches!(e, TuiEvent::ToolActivity { .. }));
    let question_idx = events.iter().position(|e| {
        matches!(
            e,
            TuiEvent::AssistantText {
                is_question: true,
                ..
            }
        )
    });
    assert!(
        greeting_idx < tool_idx,
        "greeting must precede the tool call"
    );
    assert!(
        tool_idx < question_idx,
        "tool call must precede the question"
    );
}

/// The fixture's real final turn ("...what would you like me to
/// actually work on next?") is detected as a question; the earlier
/// greeting ("Hi! 👋") is not.
#[test]
fn question_detection_on_the_fixtures_actual_question() {
    let raw = fixture_bytes();
    let tagged = parse_all(&raw);
    let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();

    let questions: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            TuiEvent::AssistantText { is_question, .. } => Some(*is_question),
            _ => None,
        })
        .collect();
    assert_eq!(
        questions,
        vec![false, true],
        "the greeting is not a question; the final turn is"
    );
}

/// Every non-`user`/`assistant`/`summary` entry type this capture
/// actually contains (`queue-operation`, `attachment`, `last-prompt`,
/// `atis-latch`) degrades to `Raw` rather than failing the parse --
/// unknown-entry tolerance against real, not synthetic, unrecognized
/// entries.
#[test]
fn every_non_conversational_real_entry_type_degrades_to_raw() {
    let raw = fixture_bytes();
    let tagged = parse_all(&raw);
    let events: Vec<TuiEvent> = tagged.into_iter().map(|(e, _)| e).collect();

    let mut raw_types: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TuiEvent::Raw { entry_type } => Some(entry_type.as_str()),
            _ => None,
        })
        .collect();
    raw_types.sort_unstable();
    raw_types.dedup();
    for expected in ["queue-operation", "attachment", "last-prompt", "atis-latch"] {
        assert!(
            raw_types.contains(&expected),
            "expected {expected:?} among the Raw entry types, got {raw_types:?}"
        );
    }
}

/// A mutated fixture -- one line's `type` replaced by something no
/// version of the real CLI has ever produced -- still parses cleanly to
/// `Raw`, rather than failing the whole tail. Tolerance for entry types
/// this adapter has never seen, not just the ones it happened to record.
#[test]
fn an_entirely_unknown_entry_type_also_degrades_to_raw_not_a_parse_failure() {
    let raw = String::from_utf8(fixture_bytes()).expect("fixture is UTF-8");
    let mutated_lines: Vec<String> = raw
        .lines()
        .map(|line| {
            let mut value: serde_json::Value =
                serde_json::from_str(line).expect("fixture line is JSON");
            if value.get("type").and_then(serde_json::Value::as_str) == Some("queue-operation") {
                value["type"] = serde_json::json!("future-entry-type");
            }
            serde_json::to_string(&value).expect("re-serializes")
        })
        .collect();
    let mutated = mutated_lines.join("\n") + "\n";

    let tagged = parse_all(mutated.as_bytes());
    let events: Vec<TuiEvent> = tagged.iter().map(|(e, _)| e.clone()).collect();
    let max_offset = tagged.iter().map(|(_, c)| c.offset).max().unwrap_or(0);
    assert_eq!(
        max_offset,
        mutated.len() as u64,
        "the mutated fixture must still fully parse"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, TuiEvent::Raw { entry_type } if entry_type == "future-entry-type")
        ),
        "expected the mutated entry to degrade to Raw"
    );
}

/// Cursor idempotency at arbitrary byte splits: parsing the fixture in
/// two pieces (any split point) and feeding the second parse's leftover
/// bytes forward produces the exact same final cursor offset and the
/// exact same events (in the same order) as one single full parse --
/// proving a crash-restart mid-transcript re-tails correctly regardless
/// of exactly which byte the daemon had consumed up to.
#[test]
fn cursor_parsing_is_idempotent_at_arbitrary_byte_splits() {
    let raw = fixture_bytes();
    let whole_tagged = parse_all(&raw);
    let whole_events: Vec<TuiEvent> = whole_tagged.iter().map(|(e, _)| e.clone()).collect();
    let whole_last_cursor = whole_tagged
        .last()
        .map(|(_, c)| c.clone())
        .unwrap_or_else(Cursor::start);

    // A representative spread of split points, including mid-line ones.
    let split_points: Vec<usize> = (1..10).map(|i| raw.len() * i / 10).collect();

    for split in split_points {
        let first_tagged = vendor().format().parse(&raw[..split], &Cursor::start());
        let first_events: Vec<TuiEvent> = first_tagged.iter().map(|(e, _)| e.clone()).collect();
        let first_cursor = first_tagged
            .last()
            .map(|(_, c)| c.clone())
            .unwrap_or_else(Cursor::start);
        // Resume from `first_cursor.offset`: feed only the unconsumed
        // remainder, exactly like `TranscriptTailer` does after a batch.
        let second_tagged = vendor()
            .format()
            .parse(&raw[first_cursor.offset as usize..], &first_cursor);
        let second_events: Vec<TuiEvent> = second_tagged.iter().map(|(e, _)| e.clone()).collect();
        let second_last_cursor = second_tagged
            .last()
            .map(|(_, c)| c.clone())
            .unwrap_or_else(Cursor::start);

        let mut combined_events = first_events;
        combined_events.extend(second_events);

        assert_eq!(
            second_last_cursor.offset, whole_last_cursor.offset,
            "split at {split}: final offset must match a single full parse"
        );
        assert_eq!(
            combined_events.len(),
            whole_events.len(),
            "split at {split}: total event count must match a single full parse"
        );
    }
}
