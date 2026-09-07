//! The canonical JSON Schema document for the Crew wire protocol.
//!
//! [`ProtocolDocument`] exists solely to give `schemars` a single root that
//! transitively references every exported request, result, and event type,
//! so one invocation produces a schema with everything reachable from the
//! wire protocol in `$defs`.
//!
//! [`render_schema`] is the sole renderer. `crew-xtask generate` writes
//! its output to `packages/protocol-ts/schema/crew.schema.json`, and
//! `crewd doctor`'s `schema_compatibility` check compares the committed
//! file against it -- both must derive the schema the same way or the check
//! would report drift that does not exist.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    ApplyResult, ArtifactFetchResult, ArtifactListResult, DisplayBackend, DisplayConfig,
    DisplayStatus, EventEnvelope, InitializeParams, InitializeResult, InspectResult,
    JsonRpcErrorResponse, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, MessageListResult,
    PaneReopenResult, PlanDecideResult, PlanGetResult, PlanProposeResult,
    PolicyViolationListResult, RetentionCleanResult, RunResultResult, RunTimeoutAckResult,
    RuntimeEvent, RuntimeStatus, WorkspaceInfo,
};

/// Root schema document referencing every exported request/result/event
/// type, so that a single `schemars` invocation produces one JSON Schema
/// with everything reachable from the wire protocol in `$defs`.
//
// CREW-44: this struct and `crates/xtask/src/main.rs`'s TS export
// allowlist (`export_bindings`'s `export!` call) are two independent
// lists that must agree on every *wire-message* type (not on bare
// id/enum/param types -- see that file's `NOT_WIRE_MESSAGE_ROOTS`, which
// is where those belong instead). CREW-43 found `RunMessage` and
// `MessageListResult` on the TS side with no field here at all, and
// nothing caught it until a human noticed. Adding a message type to the
// TS export list without a matching field here (or vice versa) now fails
// `generate --check` via `check_export_list_is_schema_reachable` in that
// same xtask file -- if you add a wire-message type to one list, add it
// to the other too.
//
// Deliberately `//`, not `///`: a doc comment here is generated schema
// content (schemars emits it as this root's `description`), not private
// engineering commentary -- CREW-44 is a code-organization note for future
// editors of this file, and belongs out of band from what every schema
// consumer reads as the file's own description.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolDocument {
    initialize_params: InitializeParams,
    initialize_result: InitializeResult,
    event_envelope: EventEnvelope,
    runtime_event: RuntimeEvent,
    display_backend: DisplayBackend,
    display_config: DisplayConfig,
    display_status: DisplayStatus,
    json_rpc_request: JsonRpcRequest<serde_json::Value>,
    json_rpc_response: JsonRpcResponse<serde_json::Value>,
    json_rpc_error_response: JsonRpcErrorResponse,
    json_rpc_notification: JsonRpcNotification<serde_json::Value>,
    runtime_status: RuntimeStatus,
    artifact_list_result: ArtifactListResult,
    artifact_fetch_result: ArtifactFetchResult,
    inspect_result: InspectResult,
    apply_result: ApplyResult,
    workspace_info: WorkspaceInfo,
    policy_violation_list_result: PolicyViolationListResult,
    /// `run/result` result payload.
    run_result_result: RunResultResult,
    /// `plan/propose` result payload.
    plan_propose_result: PlanProposeResult,
    /// `plan/decide` result payload.
    plan_decide_result: PlanDecideResult,
    /// `plan/get` result payload.
    plan_get_result: PlanGetResult,
    /// `run/timeoutAck` result payload.
    run_timeout_ack_result: RunTimeoutAckResult,
    /// `retention/clean` result payload.
    retention_clean_result: RetentionCleanResult,
    /// `pane/reopen` result payload.
    pane_reopen_result: PaneReopenResult,
    /// `message/list` result payload.
    message_list_result: MessageListResult,
}

/// Renders the [`ProtocolDocument`] schema as pretty JSON with a trailing
/// newline -- byte-for-byte what the committed schema file must contain.
///
/// # Errors
/// Returns the `serde_json` error if the schema fails to serialize, which
/// can only happen if a `JsonSchema` derive produces a non-serializable
/// value.
pub fn render_schema() -> Result<Vec<u8>, serde_json::Error> {
    let schema = schemars::schema_for!(ProtocolDocument);
    let mut text = serde_json::to_string_pretty(&schema)?;
    text.push('\n');
    Ok(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::render_schema;

    /// Backticked names this test does not require to resolve as a
    /// `$defs` key or a real wire value -- each with why it's exempt, not
    /// just that it is. CREW-46 (see `docs/engineering-lessons.md`).
    const ALLOWED_UNRESOLVED_BACKTICKED_NAMES: &[(&str, &str)] = &[(
        "Terminal",
        "DisplayBackend::Hidden's description deliberately names the \
         retired `Terminal` variant to explain what `hidden` replaced. \
         The sentence's whole point is that `Terminal` no longer exists --\
         \"fixing\" the reference would make the sentence false.",
    )];

    /// Every `const` string and every `enum` array element anywhere in
    /// `value`, recursively -- i.e. every string a wire consumer could
    /// legitimately see as a discriminator or literal value. Must walk
    /// every branch of every `oneOf`/`anyOf`, not just the first: CREW-46's
    /// own review nearly shipped a version of this check that read only
    /// `RuntimeEventKind`'s first `oneOf` branch (a 23-value enum) and
    /// missed the other ~20 single-`const` branches, which is exactly how
    /// `approvalDecided` was first misreported as unresolvable.
    fn collect_wire_values(value: &serde_json::Value, out: &mut HashSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(s)) = map.get("const") {
                    out.insert(s.clone());
                }
                if let Some(serde_json::Value::Array(items)) = map.get("enum") {
                    for item in items {
                        if let serde_json::Value::String(s) = item {
                            out.insert(s.clone());
                        }
                    }
                }
                for v in map.values() {
                    collect_wire_values(v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_wire_values(item, out);
                }
            }
            _ => {}
        }
    }

    /// Every `description` string anywhere in `value`, recursively.
    fn collect_descriptions<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(s)) = map.get("description") {
                    out.push(s);
                }
                for v in map.values() {
                    collect_descriptions(v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_descriptions(item, out);
                }
            }
            _ => {}
        }
    }

    /// Every backticked identifier in `text` that starts with an uppercase
    /// ASCII letter (e.g. `` `PlanProposed` `` -> `"PlanProposed"`), a
    /// generic parameter list like `<T>` stripped off (e.g.
    /// `` `Classified<T>` `` -> `"Classified"`) since the parameter isn't
    /// part of the name to look up.
    fn backticked_pascal_case_identifiers(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find('`') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('`') else {
                break;
            };
            let inner = &rest[..close];
            rest = &rest[close + 1..];
            let name: String = inner
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                out.push(name);
            }
        }
        out
    }

    fn lower_first(name: &str) -> String {
        let mut chars = name.chars();
        match chars.next() {
            Some(first) => first.to_lowercase().chain(chars).collect(),
            None => String::new(),
        }
    }

    /// Regression guard for CREW-46: a shipped description's backticked
    /// PascalCase name must be either a real `$defs` type reference or an
    /// actual wire value (an enum/const string anywhere in the schema),
    /// unless it's in `ALLOWED_UNRESOLVED_BACKTICKED_NAMES` with a reason.
    /// Anything else is either a miscased wire name (`PlanProposed` where
    /// the wire says `planProposed` -- CREW-46's first six fixes) or a
    /// dangling Rust-only name with nothing on the wire to resolve to
    /// (`Classified`, `RuntimePolicy` -- CREW-46's two `//` moves).
    #[test]
    fn shipped_descriptions_only_name_defs_keys_or_real_wire_values() {
        let schema_bytes = render_schema().expect("schema renders");
        let schema: serde_json::Value =
            serde_json::from_slice(&schema_bytes).expect("schema parses as JSON");

        let defs_keys: HashSet<String> = schema["$defs"]
            .as_object()
            .expect("schema has $defs")
            .keys()
            .cloned()
            .collect();

        let mut wire_values = HashSet::new();
        collect_wire_values(&schema, &mut wire_values);

        let mut descriptions = Vec::new();
        collect_descriptions(&schema, &mut descriptions);

        let allowed: HashSet<&str> = ALLOWED_UNRESOLVED_BACKTICKED_NAMES
            .iter()
            .map(|(name, _)| *name)
            .collect();

        let mut miscased = Vec::new();
        let mut unresolvable = Vec::new();
        for desc in &descriptions {
            for name in backticked_pascal_case_identifiers(desc) {
                if defs_keys.contains(&name)
                    || wire_values.contains(&name)
                    || allowed.contains(name.as_str())
                {
                    continue;
                }
                if wire_values.contains(&lower_first(&name)) {
                    miscased.push((name, desc.to_string()));
                } else {
                    unresolvable.push((name, desc.to_string()));
                }
            }
        }

        assert!(
            miscased.is_empty(),
            "shipped description(s) name a PascalCase identifier whose camelCase form IS a \
             real wire value -- rewrite to the wire form: {miscased:#?}"
        );
        assert!(
            unresolvable.is_empty(),
            "shipped description(s) name a backticked identifier that is neither a $defs key \
             nor any enum/const value anywhere in the schema, nor listed in \
             ALLOWED_UNRESOLVED_BACKTICKED_NAMES with a reason -- either it's a Rust-only name \
             that should move to a `//` comment (see CREW-46's `Classified`/`RuntimePolicy` \
             fix), or it needs an allowlist entry explaining why it's deliberately unresolved: \
             {unresolvable:#?}"
        );
    }

    /// Regression guard for the null-vs-absent fold-in: a shipped
    /// description must never say the Rust-ism "None" (either
    /// `` `field: None` `` prose or a bare "None means ..." sentence) --
    /// the wire has no such value, only `null` (a value the field takes)
    /// or omission (a key the field doesn't have), and which one applies
    /// depends on `#[serde(skip_serializing_if)]` and whether the field is
    /// read or written by its consumer. Both wordings were wrong at least
    /// once each (see the fold-in's own commit history), so this only
    /// prevents the *word* from regressing.
    ///
    /// What this does NOT check: it cannot tell a correct "null" from a
    /// correct "absent", or catch a wrong one that never says "None" to
    /// begin with. That judgment call -- read the field's actual serde
    /// attributes and decide which wording is true -- still has to be
    /// made by a person for every new `Option` field's doc, the same way
    /// it was made for every field in this class so far. This test only
    /// stops the specific, already-recurred mistake of writing the Rust
    /// name instead of either wire word. One unavoidable false positive:
    /// a sentence-initial "None of the backends were available." is
    /// correct English and still trips this -- the test can't tell that
    /// apart from the Rust-ism, so the fix there is to reword the
    /// sentence (e.g. "No backend was available"), not to pick null vs.
    /// absent.
    #[test]
    fn shipped_descriptions_never_say_the_rust_ism_none() {
        let schema_bytes = render_schema().expect("schema renders");
        let schema: serde_json::Value =
            serde_json::from_slice(&schema_bytes).expect("schema parses as JSON");

        let mut descriptions = Vec::new();
        collect_descriptions(&schema, &mut descriptions);

        let offenders: Vec<&str> = descriptions
            .into_iter()
            .filter(|desc| {
                desc.split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|word| word == "None")
            })
            .collect();

        assert!(
            offenders.is_empty(),
            "shipped description(s) say the Rust-ism \"None\" -- say `null` if the field is \
             read by its consumer (result/event) and has no skip_serializing_if, or \"absent\"/\
             \"omitted\" if the field is written by its consumer (request/config) or does have \
             skip_serializing_if. Verify the field's actual serde attributes before choosing --\
             don't assume from a sibling field of the same name: {offenders:#?}"
        );
    }
}
