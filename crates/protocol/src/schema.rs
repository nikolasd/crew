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
// This struct and `crates/xtask/src/main.rs`'s TS export
// allowlist (`export_bindings`'s `export!` call) are two independent
// lists that must agree on every *wire-message* type (not on bare
// id/enum/param types -- see that file's `NOT_WIRE_MESSAGE_ROOTS`, which
// is where those belong instead). An earlier audit found `RunMessage` and
// `MessageListResult` on the TS side with no field here at all, and
// nothing caught it until a human noticed. Adding a message type to the
// TS export list without a matching field here (or vice versa) now fails
// `generate --check` via `check_export_list_is_schema_reachable` in that
// same xtask file -- if you add a wire-message type to one list, add it
// to the other too.
//
// Deliberately `//`, not `///`: a doc comment here is generated schema
// content (schemars emits it as this root's `description`), not private
// engineering commentary -- this is a code-organization note for future
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
    /// `$defs` key or a real wire value -- each entry is `(name,
    /// required_substring, reason)`. `required_substring` exists because
    /// a bare name alone would exempt that word in EVERY shipped
    /// description, anywhere, forever. Scoping the exemption to a phrase
    /// that only appears in the description(s) this entry actually
    /// justifies means an unrelated future mention of the bare word
    /// still gets flagged, exactly as it should. (The `Redactor` entry
    /// this file once carried is gone along with it: a later change reworded the
    /// two shipped docs that named it so they no longer need the
    /// exemption at all -- see `assert_no_stale_allowlist_entries` below,
    /// which is exactly the check that would have caught it going stale.)
    const ALLOWED_UNRESOLVED_BACKTICKED_NAMES: &[(&str, &str, &str)] = &[(
        "Terminal",
        "the retired `Terminal`",
        "DisplayBackend::Hidden's description deliberately names the \
         retired `Terminal` variant to explain what `hidden` replaced. \
         The sentence's whole point is that `Terminal` no longer exists --\
         \"fixing\" the reference would make the sentence false.",
    )];

    /// Same shape and discipline as `ALLOWED_UNRESOLVED_BACKTICKED_NAMES`
    /// (name, required_substring, reason) -- flags a backticked lowercase
    /// name that is either a real property somewhere but not of the object
    /// its description belongs to (the wrong-object property check), or a
    /// name that no reachable object has as a property at all but is still
    /// referenced in dotted form because the type it actually belongs to
    /// isn't itself reachable from `ProtocolDocument` (the dotted
    /// check, e.g. `policyQuarantined` below -- `RunFlags` has no `$defs`
    /// entry, so nothing in the shipped schema's own properties will ever
    /// contain this name, and that's exactly why it needs an entry here
    /// rather than resolving on its own). Three shapes of legitimate
    /// exception in total, none a sibling-reference bug: a deliberate
    /// cross-type mention naming a property that DOES exist elsewhere in
    /// the schema, a deliberate cross-type mention naming a property that
    /// exists NOWHERE in the schema because its owning type isn't
    /// reachable, and a deliberate-absence sentence (the same pattern
    /// `Terminal` already covers above, applied to a property name instead
    /// of a type name).
    ///
    /// This file used to carry a `role` entry for `ClientAuth`
    /// (internally tagged, `#[serde(tag = "role")]`): the walk once missed that
    /// was papering over a real gap in `collect_block`'s scope tracking,
    /// not a genuine cross-object mention, and fixed the walk itself
    /// (`shared_branch_properties`) instead, on the standing precedent:
    /// fix the mechanism, don't exempt around it.
    const ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES: &[(&str, &str, &str)] = &[
        (
            "id",
            "no `id`, for",
            "Different mechanism from `Terminal` above, not the same shape: \
             `Terminal` is irreducible because the name resolves NOWHERE, so \
             rewording it would make the sentence false. `id` resolves fine \
             -- it's a real property of JsonRpcRequest/JsonRpcResponse -- and \
             JsonRpcNotification's own doc names it deliberately to state ITS \
             OWN absence of that property, on an object where the same name \
             is legitimately in scope elsewhere in the schema.",
        ),
        (
            "policyQuarantined",
            "flags.policyQuarantined",
            "PolicyViolationRecorded's own doc deliberately names the run's \
             RunFlags.policyQuarantined (via the dotted `flags.policyQuarantined` \
             form, since RunFlags isn't itself reachable from ProtocolDocument -- \
             see the // comment above PolicyViolationRecorded) to explain where \
             quarantine/cancel state is actually tracked. It's a genuine \
             cross-type mention, not a sibling-reference bug: PolicyViolationRecorded \
             has no field of its own by that name and never claims to.",
        ),
    ];

    /// Every `const` string and every `enum` array element anywhere in
    /// `value`, recursively -- i.e. every string a wire consumer could
    /// legitimately see as a discriminator or literal value. Must walk
    /// every branch of every `oneOf`/`anyOf`, not just the first: the type-name check's
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

    /// Every `description` string anywhere in `value`, recursively, paired
    /// with the property keys of its ENCLOSING schema object (the object
    /// whose `properties` map this description is inside), if any.
    ///
    /// A description's own scope updates every time the walk
    /// enters a node that has its own `properties` map -- both that
    /// node's own top-level description (a struct/variant doc naming one
    /// of its own fields) and every individual field's description found
    /// inside `properties` (a field's doc naming a sibling field) share
    /// that same scope, since both are naming a key of the SAME object.
    /// A node with no `properties` of its own inherits its parent's scope
    /// unchanged, so a plain string/enum leaf still carries the scope of
    /// whichever object it's actually nested inside.
    ///
    /// Scope ACCUMULATES rather than replaces while descending through an
    /// ordinary object: `#[serde(tag = "type", content = "payload")]`
    /// (every `RuntimeEvent`/`WorkspaceEvent` variant) puts a variant's
    /// own description on the OUTER wrapper (`{"type", "payload"}` as its
    /// own literal `properties`), one level above where the actual
    /// fields live (nested inside `payload`'s own `properties`) --
    /// replacing scope at the wrapper would lose exactly the fields a
    /// wrapper-level description is about. Scope RESETS to empty at two
    /// real object boundaries: each branch of a `oneOf`/`anyOf` (a
    /// disjoint variant, never a sibling of any other branch) and each
    /// named entry under `$defs` (a genuinely different type, reached
    /// from arbitrarily many unrelated places via `$ref`).
    /// One pass over one BLOCK -- everything reachable from `value`
    /// without crossing a `oneOf`/`anyOf` branch or a `$defs` entry --
    /// collecting every `properties` key and every `description` string
    /// found anywhere in it, plus the nested schemas one level past each
    /// boundary crossed (each scanned as its own separate block by the
    /// caller). A block is the true unit of "one logical object": an
    /// adjacently-tagged variant's wrapper (`{"type","payload"}`, its own
    /// `description` if any) and its `payload`'s own nested `properties`
    /// are the SAME block, since neither key triggers a boundary --
    /// which is exactly why a variant-level description (attached to the
    /// wrapper) needs access to field names one level down. Two passes
    /// (keys first, matched against every description in the SAME block
    /// after) rather than one, so a description encountered before its
    /// block's later-nested `properties` still gets the block's COMPLETE
    /// key set, not just what had been seen so far.
    fn collect_block<'a>(
        value: &'a serde_json::Value,
        keys: &mut HashSet<String>,
        descriptions: &mut Vec<&'a str>,
        boundaries: &mut Vec<&'a serde_json::Value>,
    ) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(properties) = map.get("properties").and_then(|p| p.as_object()) {
                    keys.extend(properties.keys().cloned());
                }
                if let Some(serde_json::Value::String(s)) = map.get("description") {
                    descriptions.push(s);
                }
                for (key, v) in map {
                    match key.as_str() {
                        "oneOf" | "anyOf" => {
                            if let serde_json::Value::Array(items) = v {
                                // Internal tagging (`#[serde(tag =
                                // "role")]`, e.g. `ClientAuth`) folds the
                                // shared tag property into EVERY branch's
                                // own `properties`, and puts the enum's
                                // own doc as a sibling of this `oneOf` --
                                // one block UP from where `role` actually
                                // lands once each branch resets to a fresh
                                // scope below. A property every branch
                                // shares is the wrapper's own shared
                                // discriminant, not a foreign field, so it
                                // belongs in THIS block's scope too. Only
                                // counts when every branch is itself an
                                // object with its own `properties` --
                                // adjacent tagging's branches all trivially
                                // share `{"type","payload"}` this way too,
                                // which is harmless (both really are the
                                // wrapper's own keys), but a branch with no
                                // `properties` at all (e.g. a bare `$ref`)
                                // means "every branch shares this" can't be
                                // claimed, so the intersection is empty.
                                keys.extend(shared_branch_properties(items));
                                boundaries.extend(items);
                            }
                        }
                        "$defs" => {
                            if let serde_json::Value::Object(defs) = v {
                                boundaries.extend(defs.values());
                            }
                        }
                        _ => collect_block(v, keys, descriptions, boundaries),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_block(item, keys, descriptions, boundaries);
                }
            }
            _ => {}
        }
    }

    /// The property keys common to EVERY branch's own `properties` map --
    /// empty if any branch lacks a `properties` object of its own (nothing
    /// can be claimed shared then). See the call site's comment in
    /// `collect_block` for why this is the internal-tagging fix.
    ///
    /// That "any branch without `properties` -> empty" rule is also what
    /// makes reusing this for `anyOf` safe, not merely uniform with
    /// `oneOf`: every `anyOf` in the shipped schema is an `Option<T>`
    /// shape, whose `null` branch is a bare `{"type": "null"}` with no
    /// `properties` of its own. That branch alone zeroes the intersection,
    /// so an optional field's wrapper never inherits its inner type's
    /// whole property set. Simplifying this rule away (e.g. skipping
    /// branches with no `properties` instead of zeroing out) would make
    /// exactly that silently possible.
    fn shared_branch_properties(items: &[serde_json::Value]) -> HashSet<String> {
        let mut branch_keys = Vec::new();
        for item in items {
            match item.get("properties").and_then(|p| p.as_object()) {
                Some(properties) => {
                    branch_keys.push(properties.keys().cloned().collect::<HashSet<String>>());
                }
                None => return HashSet::new(),
            }
        }
        let mut iter = branch_keys.into_iter();
        match iter.next() {
            Some(first) => iter.fold(first, |acc, next| {
                acc.intersection(&next).cloned().collect()
            }),
            None => HashSet::new(),
        }
    }

    fn collect_descriptions_with_scope<'a>(
        value: &'a serde_json::Value,
        out: &mut Vec<(&'a str, HashSet<String>)>,
    ) {
        let mut keys = HashSet::new();
        let mut descriptions = Vec::new();
        let mut boundaries = Vec::new();
        collect_block(value, &mut keys, &mut descriptions, &mut boundaries);
        for desc in descriptions {
            out.push((desc, keys.clone()));
        }
        for boundary in boundaries {
            collect_descriptions_with_scope(boundary, out);
        }
    }

    /// Every `description` string anywhere in `value`, recursively, with
    /// its enclosing-object scope discarded -- for checks that only care
    /// about the text (the existing type-name check, the "None"
    /// Rust-ism check), not which object it belongs to.
    fn collect_descriptions<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
        let mut scoped = Vec::new();
        collect_descriptions_with_scope(value, &mut scoped);
        out.extend(scoped.into_iter().map(|(desc, _)| desc));
    }

    /// Every `properties` key anywhere in `value`, recursively -- i.e.
    /// every field name that is a real property of SOME schema object,
    /// regardless of which one. Used only to distinguish "this backticked
    /// name is a property of a different object" (fail, wrong
    /// scope) from "this backticked name isn't a property anywhere" (not
    /// this check's business -- could be a CLI flag, a config key,
    /// anything).
    fn collect_all_property_keys(value: &serde_json::Value, out: &mut HashSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(properties) = map.get("properties").and_then(|p| p.as_object()) {
                    out.extend(properties.keys().cloned());
                }
                for v in map.values() {
                    collect_all_property_keys(v, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_all_property_keys(item, out);
                }
            }
            _ => {}
        }
    }

    /// The outcome of checking one backticked lowercase name against the
    /// object its description actually belongs to. A pure function
    /// (`resolve_property_reference`) so the wrong-object bug shape can be
    /// reproduced and asserted against directly, without depending on the
    /// real derive macros drifting away from the scenario that shipped it.
    #[derive(Debug, PartialEq, Eq)]
    enum PropertyReferenceResolution {
        /// Not a property reference at all -- doesn't match any object's
        /// properties anywhere, so out of this check's business (a CLI
        /// flag, a config key, anything else backticked-lowercase).
        NotAProperty,
        /// A property of the object the description actually belongs to.
        ResolvedLocally,
        /// A real property, but of some OTHER object -- the exact
        /// bug shape: right word, wrong object.
        WrongObject,
        /// Not a property anywhere under its own spelling, but its
        /// camelCase form IS a property somewhere -- a Rust
        /// snake_case field name leaked into shipped prose instead of the
        /// object's actual (camelCase) wire name. Carries the camelCase
        /// form the description should have said instead.
        SnakeCaseLeak(String),
    }

    /// `snake_case` -> `camelCase`, e.g. `requested_backend` ->
    /// `requestedBackend`. A name with no underscore converts to itself,
    /// which is deliberate and harmless: `resolve_property_reference`
    /// only reaches this conversion after `all_property_keys.contains(name)`
    /// has already failed, so a no-op conversion just fails the same
    /// membership check again and falls through to `NotAProperty`, same as
    /// today -- this function never needs to special-case "no underscore".
    fn to_camel_case(name: &str) -> String {
        let mut parts = name.split('_');
        let mut out = parts.next().unwrap_or_default().to_string();
        for part in parts {
            let mut chars = part.chars();
            if let Some(first) = chars.next() {
                out.push(first.to_ascii_uppercase());
                out.extend(chars);
            }
        }
        out
    }

    fn resolve_property_reference(
        name: &str,
        scope: &HashSet<String>,
        all_property_keys: &HashSet<String>,
    ) -> PropertyReferenceResolution {
        if scope.contains(name) {
            PropertyReferenceResolution::ResolvedLocally
        } else if all_property_keys.contains(name) {
            PropertyReferenceResolution::WrongObject
        } else {
            let camel = to_camel_case(name);
            if camel != name && all_property_keys.contains(&camel) {
                PropertyReferenceResolution::SnakeCaseLeak(camel)
            } else {
                PropertyReferenceResolution::NotAProperty
            }
        }
    }

    /// Every backticked identifier in `text` that starts with an uppercase
    /// ASCII letter (e.g. `` `PlanProposed` `` -> `"PlanProposed"`), a
    /// generic parameter list like `<T>` stripped off (e.g.
    /// `` `Classified<T>` `` -> `"Classified"`) since the parameter isn't
    /// part of the name to look up.
    fn backticked_pascal_case_identifiers(text: &str) -> Vec<String> {
        backticked_identifiers(text, |c| c.is_ascii_uppercase())
    }

    /// Every backticked identifier in `text` that starts with a LOWERCASE
    /// ASCII letter -- this codebase's convention reserves PascalCase for
    /// type names and lower-camelCase/snake_case for field/property
    /// names, so this is the candidate set for the sibling-property
    /// check. Not every match is actually a property reference (a
    /// backticked lowercase word could be a CLI flag, a config key,
    /// anything) -- the caller decides that by checking it against the
    /// schema's actual property keys.
    /// Unlike the PascalCase extractor (which takes a leading run and
    /// discards the rest, so `` `Classified<T>` `` still resolves to
    /// `Classified`), a property-reference candidate must be the ENTIRE
    /// backtick span, not just its prefix: a truncating scan would read
    /// `` `message/send` `` (an RPC method name) as "message" and
    /// `` `DisplayEvent.pane_ref` `` (an already-unambiguous, explicitly
    /// type-scoped reference this codebase also uses) as "DisplayEvent",
    /// both real property names elsewhere in the schema purely by
    /// coincidence of English vocabulary. Requiring the whole span to be
    /// a bare identifier is what keeps this check narrow enough to be
    /// useful instead of flagging half the schema's ordinary words.
    fn backticked_lower_case_identifiers(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find('`') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('`') else {
                break;
            };
            let inner = &rest[..close];
            rest = &rest[close + 1..];
            let is_bare_identifier =
                !inner.is_empty() && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if is_bare_identifier && inner.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                out.push(inner.to_string());
            }
        }
        out
    }

    /// Shared backtick-span scanner behind both extractors above: finds
    /// every `` `...` `` span, takes its leading run of identifier
    /// characters (stopping at the first non-alnum/underscore, so
    /// `` `Classified<T>` `` -> `"Classified"` and `` `Redactor::redact` ``
    /// -> `"Redactor"`), and keeps it only if its first character
    /// satisfies `starts_with`.
    fn backticked_identifiers(text: &str, starts_with: impl Fn(char) -> bool) -> Vec<String> {
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
            if name.chars().next().is_some_and(&starts_with) {
                out.push(name);
            }
        }
        out
    }

    /// Every backticked span in `text` shaped like a dotted path -- e.g.
    /// `` `payload.vendor_child_id` `` or `` `RunFlags.needsReconciliation` ``
    /// -- as its first segment paired with its last segment. Every segment
    /// must itself be a bare identifier (alnum/underscore only) and there
    /// must be at least two of them, or the whole span is not a candidate
    /// (e.g. `` `Redactor::redact_text` `` has no `.` at all;
    /// `` `message/send` `` has a `/`, not a `.`).
    ///
    /// `backticked_lower_case_identifiers`'s whole-span-bare-
    /// identifier rule (needed to keep RPC method names and `TypeName.field`
    /// mentions like `` `DisplayEvent.pane_ref` `` out of its own,
    /// different check) had the side effect of excluding EVERY dotted span
    /// from the sibling-property check entirely, including ones that only
    /// LOOK like the `TypeName.field` convention -- `` `payload.vendor_child_id` ``
    /// reads the same way but `payload` is a wrapper's own FIELD name, not
    /// a type, so nothing ever validated the `vendor_child_id` half. The
    /// caller tells the two shapes apart by checking the first segment
    /// against `$defs`: present, it's the established convention and
    /// genuinely type-scoped (exempt outright); absent, it's either a
    /// dotted path notation (a table.column-style mention, a version
    /// format) or a fabrication wearing the convention's clothes, so the
    /// last segment gets checked against the enclosing scope exactly like
    /// any other backticked property reference.
    fn backticked_dotted_identifiers(text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find('`') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('`') else {
                break;
            };
            let inner = &rest[..close];
            rest = &rest[close + 1..];
            let segments: Vec<&str> = inner.split('.').collect();
            let is_dotted_bare_path = segments.len() >= 2
                && segments.iter().all(|segment| {
                    !segment.is_empty()
                        && segment
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                });
            if is_dotted_bare_path {
                let first = segments[0].to_string();
                let last = segments[segments.len() - 1].to_string();
                out.push((first, last));
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

    /// Regression guard for the type-name check: a shipped description's backticked
    /// PascalCase name must be either a real `$defs` type reference or an
    /// actual wire value (an enum/const string anywhere in the schema),
    /// unless it's in `ALLOWED_UNRESOLVED_BACKTICKED_NAMES` with a reason.
    /// Anything else is either a miscased wire name (`PlanProposed` where
    /// the wire says `planProposed` -- that check's first six fixes) or a
    /// dangling Rust-only name with nothing on the wire to resolve to
    /// (`Classified`, `RuntimePolicy` -- its two `//` moves).
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

        let mut entry_used = vec![false; ALLOWED_UNRESOLVED_BACKTICKED_NAMES.len()];
        let mut miscased = Vec::new();
        let mut unresolvable = Vec::new();
        for desc in &descriptions {
            for name in backticked_pascal_case_identifiers(desc) {
                if defs_keys.contains(&name)
                    || wire_values.contains(&name)
                    || allowlist_permits(
                        ALLOWED_UNRESOLVED_BACKTICKED_NAMES,
                        &name,
                        desc,
                        &mut entry_used,
                    )
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
             ALLOWED_UNRESOLVED_BACKTICKED_NAMES with a matching required_substring -- either \
             it's a Rust-only name that should move to a `//` comment (see the \
             `Classified`/`RuntimePolicy` fix), or it needs an allowlist entry explaining why \
             it's deliberately unresolved: {unresolvable:#?}"
        );

        assert_no_stale_allowlist_entries(
            "ALLOWED_UNRESOLVED_BACKTICKED_NAMES",
            ALLOWED_UNRESOLVED_BACKTICKED_NAMES,
            &entry_used,
        );
    }

    /// An allowlist entry only exempts `name` in a
    /// description that ALSO contains its `required_substring` -- not the
    /// bare word anywhere in any shipped description. Marks the matching
    /// entry used in `entry_used`, for `assert_no_stale_allowlist_entries`
    /// below. Shared by both this file's allowlists (type names and
    /// property names), so both get that discipline, not just the
    /// one that prompted it.
    fn allowlist_permits(
        allowlist: &[(&str, &str, &str)],
        name: &str,
        desc: &str,
        entry_used: &mut [bool],
    ) -> bool {
        allowlist
            .iter()
            .enumerate()
            .any(|(i, (allowed_name, required_substring, _))| {
                let matches = *allowed_name == name && desc.contains(required_substring);
                if matches {
                    entry_used[i] = true;
                }
                matches
            })
    }

    /// Every allowlist entry must actually have fired on some
    /// real shipped description -- otherwise it's a standing
    /// pre-authorization for a name/substring pair nothing needs anymore
    /// (the field it justified was redacted, renamed, or removed),
    /// silently ready to exempt whatever uses that word next. Mirrors
    /// `crates/xtask/src/main.rs`'s `NOT_WIRE_MESSAGE_ROOTS` reverse
    /// check. `list_name` is only for the failure message.
    /// Proves the assertion in `assert_no_stale_allowlist_entries` can
    /// actually fire, not just that its call sites happen to pass an
    /// all-true `entry_used` every real run (which every real run does,
    /// since every current entry is genuinely used -- so nothing here
    /// ever exercised the failure path without this test). Pure function,
    /// no schema involved, so this cannot drift when the real allowlists
    /// change shape.
    #[test]
    #[should_panic(expected = "stale-entry-name")]
    fn a_stale_allowlist_entry_is_reported_by_name() {
        assert_no_stale_allowlist_entries(
            "TEST_ALLOWLIST",
            &[
                ("used-entry-name", "irrelevant", "irrelevant"),
                ("stale-entry-name", "irrelevant", "irrelevant"),
            ],
            &[true, false],
        );
    }

    fn assert_no_stale_allowlist_entries(
        list_name: &str,
        allowlist: &[(&str, &str, &str)],
        entry_used: &[bool],
    ) {
        let stale_entries: Vec<&(&str, &str, &str)> = allowlist
            .iter()
            .zip(entry_used)
            .filter(|(_, used)| !**used)
            .map(|(entry, _)| entry)
            .collect();
        assert!(
            stale_entries.is_empty(),
            "{list_name} entr(y/ies) never actually exempted anything -- no shipped description \
             contains both the name and its required_substring, so the field it justified was \
             likely redacted, renamed, or removed. Remove the stale entry so it can't silently \
             exempt a future, unrelated description: {stale_entries:#?}"
        );
    }

    /// Reproduces the bare-name-matching gap directly. Before
    /// this fix, exempting `Redactor` for two fields' worth of reasons
    /// exempted the bare word EVERYWHERE, forever -- a wholly unrelated
    /// future description that happened to mention `Redactor` in a
    /// different, unjustified sentence would have silently passed too.
    #[test]
    fn an_allowlist_entry_never_exempts_its_name_outside_its_required_substring() {
        let allowlist: &[(&str, &str, &str)] = &[(
            "Redactor",
            "Redactor::redact_text",
            "the only reason this name is exempt",
        )];
        let mut entry_used = vec![false; allowlist.len()];

        // The description this entry actually justifies.
        assert!(allowlist_permits(
            allowlist,
            "Redactor",
            "Passed through `Redactor::redact_text` before this event is built.",
            &mut entry_used,
        ));

        // A wholly unrelated future description that happens to use the
        // same bare word, with no relation to the justified mechanism --
        // must NOT ride along on the same exemption.
        entry_used = vec![false; allowlist.len()];
        assert!(
            !allowlist_permits(
                allowlist,
                "Redactor",
                "See the `Redactor` type for background on this unrelated field.",
                &mut entry_used,
            ),
            "an allowlist entry must not exempt its name in a description that lacks the \
             required_substring that actually justifies it"
        );
    }

    /// Regression guard for the wrong-object property bug: the check above resolves a
    /// backticked type name globally (any `$defs` key, any wire value,
    /// anywhere in the schema) -- correct for TYPE names, which really
    /// are global. A backticked PROPERTY name is different: a
    /// description's "same as `x`" means "the SIBLING field named `x` on
    /// THIS object", and resolving that globally accepts a same-named
    /// property of a completely different object. This is exactly how
    /// `AdapterNestedWorkerEvent.vendorParentRef`'s doc shipped saying
    /// "on the same terms as `vendor_child_id`" -- the SNAKE_CASE wire
    /// name of `PolicyViolationRecorded`'s own field, not
    /// `AdapterNestedWorkerEvent`'s own (camelCase) `vendorChildId`. Both
    /// objects have a same-shaped field, `vendor_child_id` is a real
    /// property somewhere, and the type-name check above never looks past
    /// that -- it has no notion of "somewhere" being the wrong place.
    #[test]
    fn shipped_descriptions_reference_sibling_properties_within_their_own_object() {
        let schema_bytes = render_schema().expect("schema renders");
        let schema: serde_json::Value =
            serde_json::from_slice(&schema_bytes).expect("schema parses as JSON");

        let defs_keys: HashSet<String> = schema["$defs"]
            .as_object()
            .expect("schema has $defs")
            .keys()
            .cloned()
            .collect();

        let mut all_property_keys = HashSet::new();
        collect_all_property_keys(&schema, &mut all_property_keys);
        // A floor, not a non-empty check -- the same "nothing to iterate" trap
        // The `every_reachable_string_field_is_redacted_or_allowlisted`
        // and the wire-contract drift test both close with a minimum-carriers
        // assertion, applied here to a walk this check depends on just as
        // completely. If `collect_all_property_keys` ever returned an empty
        // (or badly shrunken) set -- a schemars shape change, a bug in the
        // walk itself -- EVERY reference below would classify `NotAProperty`
        // (nothing to match against), so both `wrong_object` and
        // `snake_case_leak` would report empty while the check had inspected
        // nothing at all. 211 real property keys exist in the schema as of
        // this writing; the floor is set at roughly half that, comfortably
        // below normal schema growth/shrinkage but nowhere near zero.
        assert!(
            all_property_keys.len() >= 100,
            "found only {} property keys across the whole schema -- the walk is broken, not the \
             surface clean; a collapsed all_property_keys silently disables both the \
             wrong-object and the snake-case-leak checks below, since every reference would \
             classify NotAProperty with nothing to match against",
            all_property_keys.len()
        );

        let mut descriptions = Vec::new();
        collect_descriptions_with_scope(&schema, &mut descriptions);

        let mut entry_used = vec![false; ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES.len()];
        let mut wrong_object = Vec::new();
        let mut snake_case_leak = Vec::new();
        for (desc, scope) in &descriptions {
            for name in backticked_lower_case_identifiers(desc) {
                if allowlist_permits(
                    ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES,
                    &name,
                    desc,
                    &mut entry_used,
                ) {
                    continue;
                }
                match resolve_property_reference(&name, scope, &all_property_keys) {
                    PropertyReferenceResolution::ResolvedLocally
                    | PropertyReferenceResolution::NotAProperty => {}
                    PropertyReferenceResolution::WrongObject => {
                        wrong_object.push((name, desc.to_string()));
                    }
                    PropertyReferenceResolution::SnakeCaseLeak(camel) => {
                        snake_case_leak.push((name, camel, desc.to_string()));
                    }
                }
            }
            // A dotted span whose first segment is not a real
            // `$defs` key is not actually using the `TypeName.field`
            // convention, however much it looks like it -- check what it
            // really names (its last segment) the same way a bare
            // reference would be checked.
            for (first, last) in backticked_dotted_identifiers(desc) {
                if defs_keys.contains(&first) {
                    continue;
                }
                if allowlist_permits(
                    ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES,
                    &last,
                    desc,
                    &mut entry_used,
                ) {
                    continue;
                }
                match resolve_property_reference(&last, scope, &all_property_keys) {
                    PropertyReferenceResolution::ResolvedLocally
                    | PropertyReferenceResolution::NotAProperty => {}
                    PropertyReferenceResolution::WrongObject => {
                        wrong_object.push((last, desc.to_string()));
                    }
                    PropertyReferenceResolution::SnakeCaseLeak(camel) => {
                        snake_case_leak.push((last, camel, desc.to_string()));
                    }
                }
            }
        }

        assert!(
            wrong_object.is_empty(),
            "shipped description(s) reference a backticked name that IS a real property key of \
             some schema object, but not of the object the description itself belongs to -- a \
             sibling-field reference must name a field of the SAME object (check for a \
             wrong-cased or wrong-object copy/paste), or if it's a deliberate cross-type mention \
             or a deliberate-absence sentence, add it to \
             ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES with a required_substring and reason: \
             {wrong_object:#?}"
        );

        assert!(
            snake_case_leak.is_empty(),
            "shipped description(s) name a backticked property by its Rust (snake_case) field \
             name instead of the object's actual wire (camelCase) name -- the name is \
             not a property of ANY object under its own spelling, but its camelCase form IS a \
             real property, which is what tells this apart from a genuine CLI flag or config \
             key. Each entry below is (snake_case name found, camelCase name it should say, \
             description): {snake_case_leak:#?}"
        );

        assert_no_stale_allowlist_entries(
            "ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES",
            ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES,
            &entry_used,
        );
    }

    /// The wrong-object property bug, reproduced directly against
    /// `resolve_property_reference` rather than the real derive macros --
    /// this is the shape that shipped: `AdapterNestedWorkerEvent`
    /// (camelCase wire fields: `vendorChildId`, `vendorParentRef`) had a
    /// field doc naming the sibling by its RUST name (`vendor_child_id`,
    /// snake_case) instead of its own object's actual wire name
    /// (`vendorChildId`) -- which happens to be a real property of a
    /// DIFFERENT object (`PolicyViolationRecorded`, which has no
    /// `rename_all_fields` and so genuinely does serialize snake_case).
    #[test]
    fn reintroducing_the_wrong_object_reference_fails_the_guard() {
        let camel_case_object_scope: HashSet<String> = ["vendorChildId", "vendorParentRef"]
            .into_iter()
            .map(String::from)
            .collect();
        let all_property_keys: HashSet<String> = [
            "vendorChildId",
            "vendorParentRef",
            // `PolicyViolationRecorded`'s own, genuinely snake_case fields.
            "vendor_child_id",
            "vendor_parent_ref",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        // The bug as it shipped: referencing the OTHER object's (correct,
        // for THAT object) snake_case name from within this camelCase one.
        assert_eq!(
            resolve_property_reference(
                "vendor_child_id",
                &camel_case_object_scope,
                &all_property_keys
            ),
            PropertyReferenceResolution::WrongObject,
            "a real property of a DIFFERENT object must not resolve locally"
        );

        // The fix: naming this object's OWN field.
        assert_eq!(
            resolve_property_reference(
                "vendorChildId",
                &camel_case_object_scope,
                &all_property_keys
            ),
            PropertyReferenceResolution::ResolvedLocally,
            "this object's own property must resolve locally"
        );

        // A backticked lowercase word that isn't a property of ANYTHING
        // is not this check's business -- e.g. a CLI flag or config key.
        assert_eq!(
            resolve_property_reference(
                "not_a_real_property",
                &camel_case_object_scope,
                &all_property_keys
            ),
            PropertyReferenceResolution::NotAProperty
        );
    }

    /// The Rust-only sibling-name gap, reproduced directly against
    /// `resolve_property_reference`: a real, currently-open case
    /// (`PaneDowngraded.attempted`'s own shipped description) named a
    /// sibling by its RUST field name, `` `requested_backend` `` --
    /// snake_case -- instead of the object's actual (camelCase) wire name,
    /// `requestedBackend`. Unlike the wrong-object bug, `requested_backend` is not
    /// a property of ANY object anywhere in the schema (every object here
    /// renames to camelCase), so it does not hit `WrongObject` --
    /// `resolve_property_reference` as it shipped for the wrong-object and
    /// dotted-span cases falls all
    /// the way through to `NotAProperty`, the same bucket a genuine CLI
    /// flag or config key belongs in. That is the exact gap: a Rust-only
    /// name is indistinguishable from an unrelated ordinary word once its
    /// own spelling resolves nowhere.
    #[test]
    fn a_snake_case_sibling_name_whose_camel_case_form_is_a_real_property_is_not_a_leak_by_accident()
     {
        let scope: HashSet<String> = [
            "requestedBackend",
            "requestedPlacement",
            "actualBackend",
            "reason",
            "attempted",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        // Every object in the shipped schema renames to camelCase, so the
        // snake_case spelling is a property of nothing, anywhere -- unlike
        // the wrong-object bug shape, there is no OTHER object to blame this on.
        let all_property_keys = scope.clone();

        assert_eq!(
            resolve_property_reference("requested_backend", &scope, &all_property_keys),
            PropertyReferenceResolution::SnakeCaseLeak("requestedBackend".to_string()),
            "a snake_case name that resolves nowhere, but whose camelCase form is a real \
             property of this very object, must be reported as a leak naming the wire form -- \
             not waved through as though it might be a CLI flag or config key"
        );

        // The fix: naming the object's own actual (camelCase) field.
        assert_eq!(
            resolve_property_reference("requestedBackend", &scope, &all_property_keys),
            PropertyReferenceResolution::ResolvedLocally,
            "the wire form itself must keep resolving locally, unaffected by this check"
        );

        // A genuine non-property word that merely contains an underscore
        // must still fall through as NotAProperty -- this check only fires
        // when the CAMEL-CASED form is itself a real property somewhere,
        // never merely because a name happens to look snake_case-shaped.
        assert_eq!(
            resolve_property_reference("not_a_real_property", &scope, &all_property_keys),
            PropertyReferenceResolution::NotAProperty,
            "a snake_case-shaped word whose camelCase form is ALSO not a property anywhere must \
             stay NotAProperty, exactly as before this check existed"
        );
    }

    /// `to_camel_case` in isolation, including the no-underscore case
    /// `resolve_property_reference` relies on being a harmless no-op (see
    /// its own doc comment).
    #[test]
    fn to_camel_case_converts_snake_case_and_leaves_bare_words_alone() {
        assert_eq!(to_camel_case("requested_backend"), "requestedBackend");
        assert_eq!(to_camel_case("vendor_child_id"), "vendorChildId");
        assert_eq!(to_camel_case("already_camel"), "alreadyCamel");
        assert_eq!(to_camel_case("bareword"), "bareword");
        assert_eq!(to_camel_case(""), "");
    }

    /// The dotted-span evasion, reproduced directly: a dotted span whose
    /// first segment is NOT a real `$defs` key reads exactly like the
    /// established `TypeName.field` convention but isn't one -- before the
    /// fix, `backticked_lower_case_identifiers`'s whole-span-bare-identifier
    /// rule excluded every dotted span unconditionally, so
    /// `` `payload.vendor_child_id` `` never reached
    /// `resolve_property_reference` at all and a wrong-object reference
    /// hiding behind a dotted prefix sailed through undetected.
    #[test]
    fn a_dotted_span_whose_first_segment_is_not_a_real_type_still_checks_its_last_segment() {
        let text = "Passed through, on the same terms as `payload.vendor_child_id`.";
        assert_eq!(
            backticked_dotted_identifiers(text),
            vec![("payload".to_string(), "vendor_child_id".to_string())],
            "the dotted extractor must still find the span even though it isn't the \
             TypeName.field convention"
        );

        let camel_case_object_scope: HashSet<String> = ["vendorChildId", "vendorParentRef"]
            .into_iter()
            .map(String::from)
            .collect();
        let all_property_keys: HashSet<String> =
            ["vendorChildId", "vendorParentRef", "vendor_child_id"]
                .into_iter()
                .map(String::from)
                .collect();

        // "payload" is not a $defs key, so the caller must fall through to
        // checking the LAST segment against the enclosing scope -- same
        // wrong-object shape already caught for a bare reference.
        assert_eq!(
            resolve_property_reference(
                "vendor_child_id",
                &camel_case_object_scope,
                &all_property_keys
            ),
            PropertyReferenceResolution::WrongObject,
            "a dotted span's last segment must resolve exactly like a bare reference once its \
             first segment fails to name a real type"
        );
    }

    /// The established, legitimate convention this whole check must leave
    /// alone: a dotted span whose FIRST segment genuinely IS a `$defs` key
    /// (e.g. `` `RunFlags.needsReconciliation` ``) is type-scoped by
    /// construction -- the caller exempts it outright without ever
    /// consulting `resolve_property_reference`, regardless of what the
    /// last segment is or which object's description it appears in.
    #[test]
    fn a_dotted_span_whose_first_segment_is_a_real_type_is_exempt_outright() {
        let text = "Sets the run's `RunFlags.needsReconciliation` flag.";
        let (first, last) = backticked_dotted_identifiers(text)
            .into_iter()
            .next()
            .expect("one dotted span");
        assert_eq!(first, "RunFlags");
        assert_eq!(last, "needsReconciliation");

        let defs_keys: HashSet<String> = ["RunFlags".to_string()].into_iter().collect();
        assert!(
            defs_keys.contains(&first),
            "a real $defs key must short-circuit before the last segment is ever checked"
        );
    }

    /// Non-dotted backtick shapes that must never be mistaken for the
    /// dotted-path convention: `` `Redactor::redact_text` `` (Rust path
    /// syntax, no `.` at all) and `` `message/send` `` (an RPC method
    /// name, `/`-separated). Neither should produce a candidate.
    #[test]
    fn non_dot_separated_backtick_spans_are_not_dotted_identifiers() {
        let text = "See `Redactor::redact_text` and `message/send` for background.";
        assert_eq!(
            backticked_dotted_identifiers(text),
            Vec::<(String, String)>::new(),
            "neither `::` nor `/` is the dotted-path separator this check looks for"
        );
    }

    /// The internally-tagged-enum gap, reproduced directly against
    /// `shared_branch_properties`: an internally-tagged enum
    /// (`#[serde(tag = "role")]`, e.g. `ClientAuth`) folds the shared tag
    /// property into EVERY branch's own `properties` -- before the fix,
    /// `collect_block` reset scope to empty at every `oneOf`/`anyOf`
    /// boundary unconditionally, so the enum's own doc (a sibling of the
    /// `oneOf`, one block above where `role` actually lands) never had
    /// `role` in scope, and the check could only be satisfied with a
    /// standing allowlist entry.
    #[test]
    fn a_property_shared_by_every_branch_is_claimed_for_the_wrapper_scope() {
        let branches = vec![
            serde_json::json!({
                "properties": {"role": {"const": "worker"}, "workerField": {"type": "string"}}
            }),
            serde_json::json!({
                "properties": {"role": {"const": "operator"}, "operatorField": {"type": "string"}}
            }),
        ];
        assert_eq!(
            shared_branch_properties(&branches),
            ["role".to_string()]
                .into_iter()
                .collect::<HashSet<String>>(),
            "role is the only property every branch shares -- the branch-specific fields must \
             not leak into the wrapper's scope"
        );
    }

    /// A genuinely disjoint `oneOf` (ordinary adjacent tagging, no shared
    /// discriminant beyond what schemars puts on every variant trivially)
    /// must not fabricate sharing that isn't there, and a branch with no
    /// `properties` object at all (a bare `$ref` or a non-object variant)
    /// must zero out the whole claim rather than silently skip that branch.
    #[test]
    fn branches_with_nothing_in_common_share_nothing() {
        let disjoint = vec![
            serde_json::json!({"properties": {"a": {}, "b": {}}}),
            serde_json::json!({"properties": {"c": {}, "d": {}}}),
        ];
        assert_eq!(shared_branch_properties(&disjoint), HashSet::new());

        let one_branch_has_no_properties = vec![
            serde_json::json!({"properties": {"role": {}}}),
            serde_json::json!({"$ref": "#/$defs/SomeOtherType"}),
        ];
        assert_eq!(
            shared_branch_properties(&one_branch_has_no_properties),
            HashSet::new(),
            "a branch with no properties of its own means \"every branch shares this\" can't be \
             claimed, even if every OTHER branch happens to agree"
        );
    }

    /// The single-branch case: nothing in the shipped schema actually
    /// produces a `oneOf`/`anyOf` with exactly one item (every real
    /// `oneOf` has 2-35 branches, every real `anyOf` exactly 2), but the
    /// function must still behave sensibly if one ever did. With one
    /// branch, "the properties common to EVERY branch" is trivially that
    /// branch's entire property set, so the wrapper's scope ends up
    /// admitting every field of its sole branch. That's a deliberate
    /// consequence of the definition, not a bug: this check only ever
    /// widens scope (a false negative risk, never a false positive one),
    /// and with exactly one branch the wrapper and the branch are
    /// effectively the same object anyway, so there is nothing left for
    /// the wrapper's own description to be "wrong" about.
    #[test]
    fn a_single_branch_shares_its_entire_property_set_with_the_wrapper() {
        let single = vec![serde_json::json!({
            "properties": {"onlyField": {}, "anotherField": {}}
        })];
        assert_eq!(
            shared_branch_properties(&single),
            ["onlyField".to_string(), "anotherField".to_string()]
                .into_iter()
                .collect::<HashSet<String>>(),
            "one branch's entire property set is trivially \"shared\" -- deliberate, since it \
             only widens the wrapper's scope rather than narrowing it"
        );
    }

    /// The type-name check and the property-name check are separated
    /// entirely by which extractor feeds them --
    /// asserts that separation directly, rather than relying on it being
    /// implied by the rest of this file. A legitimate cross-type
    /// reference like `` `RunId` `` must never even reach
    /// `resolve_property_reference`: it's PascalCase, so only the
    /// existing global $defs/wire-value check (unaffected by this ticket)
    /// ever sees it.
    #[test]
    fn pascal_case_type_references_are_never_treated_as_property_references() {
        let text = "See `RunId` and `PlanProposed`, on the same terms as `vendor_child_id`.";
        assert_eq!(
            backticked_lower_case_identifiers(text),
            vec!["vendor_child_id".to_string()],
            "only the lowercase-starting name is a property-reference candidate"
        );
        assert_eq!(
            backticked_pascal_case_identifiers(text),
            vec!["RunId".to_string(), "PlanProposed".to_string()],
            "the PascalCase names stay exactly where the existing type-name check already looks"
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
