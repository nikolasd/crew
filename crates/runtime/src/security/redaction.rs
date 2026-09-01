//! The redaction boundary: the single place raw, classified vendor content
//! is turned into the only event type the durable journal can accept.
//!
//! Raw vendor frames -- which may carry [`ContentClass::Thinking`] or
//! [`ContentClass::Secret`] fragments -- exist only in bounded process
//! memory. [`Redactor::sanitize`] is the sole path from that raw
//! representation to [`PersistableEvent`]: it drops `Thinking` and `Secret`
//! fragments entirely, and rewrites built-in regex-pattern matches (e.g.
//! API-key-shaped tokens) found in `Visible` text with a `[REDACTED:<rule
//! id>]` marker. `PersistableEvent`'s fields are private and it has no
//! public constructor, so the only way to obtain one -- anywhere in this
//! crate or downstream -- is through [`Redactor::sanitize`].

use crew_protocol::{
    Classified, ContentClass, DiagnosticLevel, ProjectId, RunId, RuntimeEvent, Timestamp,
};
use regex::Regex;

/// A raw, potentially-classified runtime event, as produced by a worker or
/// vendor process before it crosses the redaction boundary.
///
/// This type must never be persisted or logged directly -- only the
/// [`PersistableEvent`] produced by [`Redactor::sanitize`] may reach the
/// database actor.
#[derive(Debug, Clone)]
pub struct RawRuntimeEvent {
    pub timestamp: Timestamp,
    pub project_id: ProjectId,
    pub run_id: Option<RunId>,
    pub kind: RawEventKind,
}

/// The raw, pre-redaction payload of a [`RawRuntimeEvent`].
///
/// Mirrors [`RuntimeEvent`]'s shape, except [`RawEventKind::Diagnostic`]
/// carries a list of classified text fragments rather than a plain
/// `message`: vendor frames often interleave visible narration with
/// thinking or secret content, and every fragment's classification must be
/// honored independently when redacting.
#[derive(Debug, Clone)]
pub enum RawEventKind {
    RuntimeStarted,
    RuntimeStopping,
    Diagnostic {
        level: DiagnosticLevel,
        code: String,
        fragments: Vec<Classified<String>>,
    },
}

/// A sanitized event, the only type the database actor's journal accepts.
///
/// Fields are private; there is no public constructor. The only way to
/// obtain one is [`Redactor::sanitize`].
#[derive(Debug, Clone)]
pub struct PersistableEvent {
    timestamp: Timestamp,
    project_id: ProjectId,
    run_id: Option<RunId>,
    event_json: String,
}

impl PersistableEvent {
    /// The event's timestamp.
    #[must_use]
    pub fn timestamp(&self) -> &Timestamp {
        &self.timestamp
    }

    /// The project the event belongs to.
    #[must_use]
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// The run the event belongs to, if any.
    #[must_use]
    pub fn run_id(&self) -> Option<&RunId> {
        self.run_id.as_ref()
    }

    /// The sanitized event body, serialized as JSON text. Guaranteed to be
    /// the JSON serialization of a plain [`RuntimeEvent`] -- never a raw or
    /// classified value.
    #[must_use]
    pub fn event_json(&self) -> &str {
        &self.event_json
    }
}

/// Sanitized JSON text, the only type
/// [`crate::db::DatabaseHandle::record_operation_intent`] and
/// [`crate::db::DatabaseHandle::acknowledge_operation`] accept for their
/// intent/acknowledgement payloads.
///
/// There is no public constructor: the only way to obtain one, anywhere, is
/// [`Redactor::sanitize_json`], which deep-walks a `serde_json::Value` and
/// applies the same redaction rules used for events to every string (key
/// and value alike) before serializing it. This keeps unsanitized operation
/// payloads from reaching the durable `operations` table the same way
/// [`PersistableEvent`] keeps them out of `events`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedJson(String);

impl SanitizedJson {
    /// The sanitized JSON text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SanitizedJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One built-in, bounded regex rule applied to `Visible` text.
struct RedactionRule {
    pattern: Regex,
    /// The text each match is replaced with, precomputed at construction
    /// from the rule's id. Always carries the `[REDACTED:<id>]` marker.
    replacement: String,
}

impl RedactionRule {
    /// A rule whose entire match is the secret, replaced wholesale.
    fn new(id: &'static str, pattern: Regex) -> Self {
        Self {
            pattern,
            replacement: format!("[REDACTED:{id}]"),
        }
    }

    /// A rule whose pattern must also match the character *preceding* the
    /// secret in order to constrain what may precede it (capture group 1).
    /// That character is re-emitted, so only the secret itself is removed
    /// and the surrounding text survives the substitution.
    fn keeping_prefix(id: &'static str, pattern: Regex) -> Self {
        Self {
            pattern,
            replacement: format!("${{1}}[REDACTED:{id}]"),
        }
    }

    fn apply(&self, text: &str) -> String {
        self.pattern
            .replace_all(text, self.replacement.as_str())
            .to_string()
    }
}

/// Compiles the built-in redaction rules once, then sanitizes raw events
/// into [`PersistableEvent`]s: the only crossing point of the redaction
/// boundary.
pub struct Redactor {
    rules: Vec<RedactionRule>,
    /// Org-configured redaction rules, compiled once at startup.
    org_rules: Vec<crate::security::rules::OrgRedactionRule>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor {
    /// Compiles the built-in bounded regex rules. Intended to be called
    /// once at process startup and reused for every subsequent
    /// [`Redactor::sanitize`] call.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rules: vec![
                // `sk-`-prefixed vendor API keys. The character class must
                // include `-` and `_`: Anthropic's real shape is
                // `sk-ant-api03-<base64url>` and OpenAI's is
                // `sk-proj-<base64url>`, both of which carry hyphens and
                // underscores inside the token itself.
                //
                // Because that class accepts `-`, what may precede the token
                // has to be constrained or ordinary hyphenated prose gets
                // eaten: `disk-space-check-failed` contains
                // `sk-space-check-failed`. A leading `\b` is not enough -- `-`
                // is a non-word character, so `\b` still admits
                // `pre-sk-space-check-failed`. The preceding character is
                // therefore matched, constrained to something outside the
                // token alphabet (or start-of-text), and re-emitted by
                // `keeping_prefix`.
                RedactionRule::keeping_prefix(
                    "api_key",
                    Regex::new(r"(^|[^A-Za-z0-9_-])sk-[A-Za-z0-9_-]{16,}")
                        .expect("built-in api_key pattern is a valid, bounded regex"),
                ),
                // Long bearer-ish tokens surfaced in free text.
                RedactionRule::new(
                    "bearer_token",
                    Regex::new(r"Bearer\s+[A-Za-z0-9._-]{20,}")
                        .expect("built-in bearer_token pattern is a valid, bounded regex"),
                ),
                // GitHub personal access tokens (ghp_ prefix).
                RedactionRule::new(
                    "github_pat",
                    Regex::new(r"ghp_[A-Za-z0-9]{16,}")
                        .expect("built-in github_pat pattern is a valid, bounded regex"),
                ),
                // AWS access key IDs (AKIA prefix).
                RedactionRule::new(
                    "aws_access_key",
                    Regex::new(r"AKIA[0-9A-Z]{16}")
                        .expect("built-in aws_access_key pattern is a valid, bounded regex"),
                ),
                // JSON Web Tokens (three base64url-encoded segments).
                RedactionRule::new(
                    "jwt",
                    Regex::new(r"[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}")
                        .expect("built-in jwt pattern is a valid, bounded regex"),
                ),
            ],
            org_rules: Vec::new(),
        }
    }

    /// Creates a [`Redactor`] with both built-in and org-configured rules.
    ///
    /// # Errors
    ///
    /// Returns an error if any org pattern is not a valid regex.
    pub fn with_org_rules(org_patterns: &[String]) -> Result<Self, String> {
        let mut redactor = Self::new();

        // Compile org patterns into OrgRedactionRule instances
        for (i, pattern) in org_patterns.iter().enumerate() {
            let rule =
                crate::security::rules::OrgRedactionRule::new(format!("org_pattern_{i}"), pattern)
                    .map_err(|e| format!("invalid org pattern at index {i}: {e}"))?;
            redactor.org_rules.push(rule);
        }

        Ok(redactor)
    }

    /// Sanitizes a raw event into the only type the durable journal
    /// accepts: `Thinking` and `Secret` fragments are dropped entirely
    /// (never even scanned), and built-in and org-defined pattern matches in
    /// `Visible` text are replaced with `[REDACTED:<rule-id>]`.
    #[must_use]
    pub fn sanitize(&self, raw: RawRuntimeEvent) -> PersistableEvent {
        let event = match raw.kind {
            RawEventKind::RuntimeStarted => RuntimeEvent::RuntimeStarted,
            RawEventKind::RuntimeStopping => RuntimeEvent::RuntimeStopping,
            RawEventKind::Diagnostic {
                level,
                code,
                fragments,
            } => {
                let message = fragments
                    .into_iter()
                    .filter(|fragment| fragment.class == ContentClass::Visible)
                    .map(|fragment| self.redact_visible_text(&fragment.value))
                    .collect::<Vec<_>>()
                    .join("\n");
                RuntimeEvent::Diagnostic {
                    level,
                    code,
                    // Every fragment above went through `redact_visible_text`
                    // on this very line's `map`, so `from_sanitized` is the
                    // honest claim here -- this IS the redactor.
                    message: crew_protocol::Redacted::from_sanitized(message),
                }
            }
        };

        let event_json = serde_json::to_string(&event)
            .expect("a plain, already-sanitized RuntimeEvent always serializes");

        PersistableEvent {
            timestamp: raw.timestamp,
            project_id: raw.project_id,
            run_id: raw.run_id,
            event_json,
        }
    }

    /// Sanitizes an arbitrary `serde_json::Value` into [`SanitizedJson`]:
    /// the only path by which pre-serialized JSON (e.g. an operation's
    /// intent or acknowledgement payload) may cross the redaction boundary
    /// on its way to the durable `operations` table.
    ///
    /// Deep-walks the value, applying the same built-in and org-defined
    /// regex rules used for event text to every string found -- object keys
    /// and values alike, at any nesting depth -- replacing matches with
    /// `[REDACTED:<rule-id>]`. Unlike [`Redactor::sanitize`], nothing here
    /// is dropped based on a [`ContentClass`]: arbitrary JSON carries no
    /// classification, so every string is scanned. The result is
    /// serialized deterministically. This workspace enables `preserve_order`,
    /// so explicit [`crate::canonical_json::canonicalize_in_place`] makes
    /// equal JSON produce equal durable bytes regardless of input key order.
    #[must_use]
    pub fn sanitize_json(&self, value: &serde_json::Value) -> SanitizedJson {
        let mut redacted = self.redact_json_value(value);
        crate::canonical_json::canonicalize_in_place(&mut redacted);
        let text = serde_json::to_string(&redacted)
            .expect("a serde_json::Value built from redaction always serializes");
        SanitizedJson(text)
    }

    /// Sanitizes a single classified text fragment for a wire-shape field
    /// (as opposed to a whole [`RawRuntimeEvent`]): `Thinking`/`Secret`
    /// fragments are dropped (returned as `None`), and `Visible` text has
    /// the same built-in and org-defined regex rules applied as
    /// [`Redactor::sanitize`]. Used by adapter event normalization
    /// (`crate::adapter::event_sink`), which carries free-text vendor
    /// output (message chunks, tool details, diagnostics) as
    /// `Classified<String>` fields that must cross this exact boundary
    /// before becoming part of a durable `RuntimeEvent`.
    #[must_use]
    pub fn sanitize_fragment(&self, fragment: &Classified<String>) -> Option<String> {
        match fragment.class {
            ContentClass::Visible => Some(self.redact_visible_text(&fragment.value)),
            ContentClass::Thinking | ContentClass::Secret => None,
        }
    }

    /// Recursively rebuilds `value`, applying [`Redactor::redact_visible_text`]
    /// to every string it contains (both object keys and string values).
    /// Source keys are sorted before redaction, so collisions collapse
    /// deterministically with the lexicographically greatest source key winning.
    fn redact_json_value(&self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(text) => {
                serde_json::Value::String(self.redact_visible_text(text))
            }
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(|item| self.redact_json_value(item))
                    .collect(),
            ),
            serde_json::Value::Object(map) => {
                let mut redacted = serde_json::Map::with_capacity(map.len());
                let mut entries: Vec<_> = map.iter().collect();
                // Sorting source keys makes last-wins collision resolution
                // independent of insertion order.
                entries.sort_unstable_by_key(|(key, _)| *key);
                for (key, val) in entries {
                    let redacted_key = self.redact_visible_text(key);
                    redacted.insert(redacted_key, self.redact_json_value(val));
                }
                serde_json::Value::Object(redacted)
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
                value.clone()
            }
        }
    }

    /// Applies the same built-in and org-defined regex rules used for
    /// `Visible` text to an always-visible, non-classified string -- a short
    /// vendor-assigned label (tool name, vendor session/child/parent
    /// identifier, role, artifact kind, ...) that carries no `ContentClass`
    /// because it is never dropped for being `Thinking`/`Secret`, but is
    /// still vendor-sourced and must not be trusted to never accidentally
    /// contain a secret-shaped value.
    #[must_use]
    pub fn redact_text(&self, text: &str) -> String {
        self.redact_visible_text(text)
    }

    /// Applies every built-in and org rule to `text`, replacing each match
    /// with `[REDACTED:<rule-id>]`.
    fn redact_visible_text(&self, text: &str) -> String {
        let mut redacted = text.to_string();

        // Apply built-in rules
        for rule in &self.rules {
            redacted = rule.apply(&redacted);
        }

        // Apply org rules
        for rule in &self.org_rules {
            redacted = rule.apply(&redacted);
        }

        redacted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(fragments: Vec<Classified<String>>) -> RawRuntimeEvent {
        RawRuntimeEvent {
            timestamp: Timestamp::now(),
            project_id: ProjectId::new(),
            run_id: None,
            kind: RawEventKind::Diagnostic {
                level: DiagnosticLevel::Info,
                code: "test".to_string(),
                fragments,
            },
        }
    }

    fn visible(value: &str) -> Classified<String> {
        Classified {
            class: ContentClass::Visible,
            value: value.to_string(),
        }
    }

    fn secret(value: &str) -> Classified<String> {
        Classified {
            class: ContentClass::Secret,
            value: value.to_string(),
        }
    }

    fn thinking(value: &str) -> Classified<String> {
        Classified {
            class: ContentClass::Thinking,
            value: value.to_string(),
        }
    }

    #[test]
    fn visible_text_survives_unchanged() {
        let redactor = Redactor::new();
        let persisted = redactor.sanitize(event(vec![visible("hello world")]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert_eq!(message.as_str(), "hello world");
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    #[test]
    fn secret_fragments_are_dropped_entirely() {
        let redactor = Redactor::new();
        let persisted = redactor.sanitize(event(vec![secret("sk-ABC...UVWX")]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert_eq!(message.as_str(), "");
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    #[test]
    fn thinking_fragments_are_dropped_entirely() {
        let redactor = Redactor::new();
        let persisted = redactor.sanitize(event(vec![thinking("internal reasoning")]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert_eq!(message.as_str(), "");
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    #[test]
    fn api_key_shaped_visible_text_is_redacted() {
        let redactor = Redactor::new();
        let persisted = redactor.sanitize(event(vec![visible(
            "key is sk-ABCDEFGHIJKLMNOPQRSTUVWX here",
        )]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert!(message.as_str().contains("[REDACTED:api_key]"));
                assert!(!message.as_str().contains("sk-ABCDEFGHIJKLMNOPQRSTUVWX"));
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    #[test]
    fn anthropic_shaped_api_key_is_redacted() {
        let redactor = Redactor::new();
        let persisted = redactor.sanitize(event(vec![visible(
            "key is sk-ant-api03-FAKEKEY-for-tests_0123456789-abcdefghij here",
        )]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert!(message.as_str().contains("[REDACTED:api_key]"));
                assert!(
                    !message
                        .as_str()
                        .contains("sk-ant-api03-FAKEKEY-for-tests_0123456789-abcdefghij")
                );
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    #[test]
    fn openai_project_shaped_api_key_is_redacted() {
        let redactor = Redactor::new();
        let persisted = redactor.sanitize(event(vec![visible(
            "key is sk-proj-FAKEKEY-for-tests_0123456789-abcdefghij here",
        )]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert!(message.as_str().contains("[REDACTED:api_key]"));
                assert!(
                    !message
                        .as_str()
                        .contains("sk-proj-FAKEKEY-for-tests_0123456789-abcdefghij")
                );
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    /// The widened `api_key` character class accepts `-`, so an ordinary
    /// hyphenated diagnostic like `disk-space-check-failed` contains a
    /// perfectly legal nineteen-character match (`sk-space-check-failed`).
    /// Over-redacting diagnostics is a quieter failure than leaking a key,
    /// but it is still a failure.
    #[test]
    fn hyphenated_prose_is_not_mistaken_for_an_api_key() {
        let redactor = Redactor::new();
        let prose = "disk-space-check-failed while the disk-space-monitor-thread stalled";
        let persisted = redactor.sanitize(event(vec![visible(prose)]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert_eq!(message.as_str(), prose);
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    /// The stricter half of the guard above: a leading `\b` is *not*
    /// sufficient, because `-` is a non-word character, so `\b` happily
    /// admits `pre-sk-space-check-failed`. Only constraining the preceding
    /// character to something outside the token alphabet rejects it.
    #[test]
    fn hyphen_delimited_prose_is_not_mistaken_for_an_api_key() {
        let redactor = Redactor::new();
        let prose = "pre-sk-space-check-failed and retry-sk-space-check-failed-again";
        let persisted = redactor.sanitize(event(vec![visible(prose)]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert_eq!(message.as_str(), prose);
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }

    /// The `api_key` pattern matches the delimiter preceding a key so it can
    /// constrain it, and re-emits it via `${1}`. That must not consume the
    /// delimiter separating two adjacent keys, which would leave the second
    /// one unredacted: `replace_all` resumes scanning at the end of each
    /// match, so a swallowed separator means a missed secret.
    #[test]
    fn two_adjacent_api_keys_are_both_redacted() {
        let redactor = Redactor::new();
        let first = "sk-ant-api03-AAAAKEY-for-tests_0123456789-aaaaaa";
        let second = "sk-proj-BBBBKEY-for-tests_0123456789-bbbbbb";

        for text in [
            format!("{first} {second}"),
            format!("{first},{second}"),
            format!("env dump: {first} and {second} end"),
        ] {
            let persisted = redactor.sanitize(event(vec![visible(&text)]));
            match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
                Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                    assert!(
                        !message.as_str().contains(first),
                        "first key survived in {}",
                        message.as_str()
                    );
                    assert!(
                        !message.as_str().contains(second),
                        "second key survived in {}",
                        message.as_str()
                    );
                    assert_eq!(
                        message.as_str().matches("[REDACTED:api_key]").count(),
                        2,
                        "both keys must be redacted separately in {}",
                        message.as_str()
                    );
                }
                other => panic!("expected Diagnostic, got {:?}", other),
            }
        }
    }

    #[test]
    fn sanitize_json_redacts_secret_shaped_values_at_any_depth() {
        let redactor = Redactor::new();
        let value = serde_json::json!({
            "action": "spawn_worker",
            "nested": {
                "key": "sk-ABCDEFGHIJKLMNOPQRSTUVWX"
            }
        });

        let sanitized = redactor.sanitize_json(&value);
        let text = sanitized.as_str();
        assert!(text.contains("[REDACTED:api_key]"));
        assert!(!text.contains("sk-ABCDEFGHIJKLMNOPQRSTUVWX"));
    }

    /// The same widened rule must hold on the `SanitizedJson` path, which is
    /// how operation intent/acknowledgement payloads reach the durable
    /// `operations` table -- not just on the event path.
    #[test]
    fn sanitize_json_redacts_an_anthropic_shaped_key_at_any_depth() {
        let redactor = Redactor::new();
        let value = serde_json::json!({
            "action": "spawn_worker",
            "nested": {
                "key": "sk-ant-api03-FAKEKEY-for-tests_0123456789-abcdefghij"
            }
        });

        let sanitized = redactor.sanitize_json(&value);
        let text = sanitized.as_str();
        assert!(text.contains("[REDACTED:api_key]"));
        assert!(!text.contains("sk-ant-api03-FAKEKEY-for-tests_0123456789-abcdefghij"));
    }

    #[test]
    fn sanitize_json_redacts_secret_shaped_object_keys() {
        let redactor = Redactor::new();
        let value = serde_json::json!({
            "sk-ABCDEFGHIJKLMNOPQRSTUVWX": "value"
        });

        let sanitized = redactor.sanitize_json(&value);
        let text = sanitized.as_str();
        assert!(text.contains("[REDACTED:api_key]"));
        assert!(!text.contains("sk-ABCDEFGHIJKLMNOPQRSTUVWX"));
    }

    #[test]
    fn sanitize_json_is_byte_identical_for_two_differently_ordered_equal_objects() {
        let redactor = Redactor::new();
        let first = serde_json::json!({
            "alpha": "first key",
            "array": [{ "bravo": true, "alpha": false }],
            "middle": "value",
            "sk-ABCDEFGHIJKLMNOPQRSTUVWX": "redacted key",
            "zebra": { "delta": 2, "charlie": 3 }
        });
        let second = serde_json::json!({
            "zebra": { "charlie": 3, "delta": 2 },
            "sk-ABCDEFGHIJKLMNOPQRSTUVWX": "redacted key",
            "middle": "value",
            "array": [{ "alpha": false, "bravo": true }],
            "alpha": "first key"
        });

        let first_sanitized = redactor.sanitize_json(&first).as_str().to_string();
        let second_sanitized = redactor.sanitize_json(&second).as_str().to_string();

        assert_eq!(first_sanitized, second_sanitized);
        assert!(first_sanitized.starts_with(r#"{"[REDACTED:api_key]":"#));
    }

    #[test]
    fn sanitize_json_resolves_redacted_key_collisions_independently_of_input_order() {
        let redactor = Redactor::new();
        let first = serde_json::json!({
            "sk-ABCDEFGHIJKLMNOPQRSTUVWX": "first value",
            "sk-ZYXWVUTSRQPONMLKJIHGFEDC": "second value"
        });
        let second = serde_json::json!({
            "sk-ZYXWVUTSRQPONMLKJIHGFEDC": "second value",
            "sk-ABCDEFGHIJKLMNOPQRSTUVWX": "first value"
        });

        let first_text = redactor.sanitize_json(&first).as_str().to_string();
        let second_text = redactor.sanitize_json(&second).as_str().to_string();

        assert_eq!(first_text, second_text);
        assert!(first_text.contains("[REDACTED:api_key]"));
        assert!(!first_text.contains("sk-ABCDEFGHIJKLMNOPQRSTUVWX"));
        assert!(!first_text.contains("sk-ZYXWVUTSRQPONMLKJIHGFEDC"));
        assert_eq!(first_text, r#"{"[REDACTED:api_key]":"second value"}"#);
    }

    #[test]
    fn org_patterns_are_applied_during_redaction() {
        let redactor = Redactor::with_org_rules(&["CUSTOM_SECRET_[0-9A-Z]{16}".to_string()])
            .expect("valid pattern");
        let persisted = redactor.sanitize(event(vec![visible(
            "key is CUSTOM_SECRET_ABCDEFGHIJKLMNOP here",
        )]));

        match serde_json::from_str::<RuntimeEvent>(persisted.event_json()) {
            Ok(RuntimeEvent::Diagnostic { message, .. }) => {
                assert!(message.as_str().contains("[REDACTED:org_pattern_0]"));
                assert!(!message.as_str().contains("CUSTOM_SECRET_ABCDEFGHIJKLMNOP"));
            }
            other => panic!("expected Diagnostic, got {:?}", other),
        }
    }
}
