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
    /// `$defs` key or a real wire value -- each entry is `(name,
    /// required_substring, reason)`. `required_substring` is CREW-68(b):
    /// a bare name alone would exempt that word in EVERY shipped
    /// description, anywhere, forever. Scoping the exemption to a phrase
    /// that only appears in the description(s) this entry actually
    /// justifies means an unrelated future mention of the bare word
    /// still gets flagged, exactly as it should. (The `Redactor` entry
    /// this file once carried is gone along with it: #100 reworded the
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
    /// (name, required_substring, reason) -- CREW-67's own check flags a
    /// backticked lowercase name that IS a real property somewhere but
    /// not of the object its description belongs to. Two shapes of
    /// legitimate exception, neither a sibling-reference bug: a
    /// deliberate cross-type mention (the description is explaining an
    /// effect on, or relationship to, a DIFFERENT type's field, not
    /// claiming the name as its own) and a deliberate-absence sentence
    /// (the same pattern `Terminal` already covers above, applied to a
    /// property name instead of a type name).
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
            "role",
            "The `role` tag",
            "ClientAuth is internally tagged (#[serde(tag = \"role\")]); schemars \
             places the enum's own doc on the wrapper, one level above where \
             `role` actually lands as a property (folded into each `oneOf` \
             branch by internal tagging). The doc is naming its own shared \
             discriminant, not a foreign field -- it just lives one block \
             down from where the doc is attached.",
        ),
    ];

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

    /// Every `description` string anywhere in `value`, recursively, paired
    /// with the property keys of its ENCLOSING schema object (the object
    /// whose `properties` map this description is inside), if any.
    ///
    /// CREW-67: a description's own scope updates every time the walk
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
    /// about the text (the existing CREW-46 type-name check, the "None"
    /// Rust-ism check), not which object it belongs to.
    fn collect_descriptions<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
        let mut scoped = Vec::new();
        collect_descriptions_with_scope(value, &mut scoped);
        out.extend(scoped.into_iter().map(|(desc, _)| desc));
    }

    /// Every `properties` key anywhere in `value`, recursively -- i.e.
    /// every field name that is a real property of SOME schema object,
    /// regardless of which one. Used only to distinguish "this backticked
    /// name is a property of a different object" (CREW-67: fail, wrong
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
    /// (`resolve_property_reference`) so CREW-67's exact bug shape can be
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
        /// A real property, but of some OTHER object -- CREW-67's exact
        /// bug shape: right word, wrong object.
        WrongObject,
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
            PropertyReferenceResolution::NotAProperty
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
    /// names, so this is the candidate set for CREW-67's sibling-property
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
             it's a Rust-only name that should move to a `//` comment (see CREW-46's \
             `Classified`/`RuntimePolicy` fix), or it needs an allowlist entry explaining why \
             it's deliberately unresolved: {unresolvable:#?}"
        );

        assert_no_stale_allowlist_entries(
            "ALLOWED_UNRESOLVED_BACKTICKED_NAMES",
            ALLOWED_UNRESOLVED_BACKTICKED_NAMES,
            &entry_used,
        );
    }

    /// CREW-68(b): an allowlist entry only exempts `name` in a
    /// description that ALSO contains its `required_substring` -- not the
    /// bare word anywhere in any shipped description. Marks the matching
    /// entry used in `entry_used`, for `assert_no_stale_allowlist_entries`
    /// below. Shared by both this file's allowlists (type names and
    /// property names), so both get CREW-68's discipline, not just the
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

    /// CREW-68(a): every allowlist entry must actually have fired on some
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

    /// CREW-68(b): reproduces the bare-name-matching gap directly. Before
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

    /// Regression guard for CREW-67: the check above resolves a
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
    /// property somewhere, and the CREW-46 check above never looks past
    /// that -- it has no notion of "somewhere" being the wrong place.
    #[test]
    fn shipped_descriptions_reference_sibling_properties_within_their_own_object() {
        let schema_bytes = render_schema().expect("schema renders");
        let schema: serde_json::Value =
            serde_json::from_slice(&schema_bytes).expect("schema parses as JSON");

        let mut all_property_keys = HashSet::new();
        collect_all_property_keys(&schema, &mut all_property_keys);

        let mut descriptions = Vec::new();
        collect_descriptions_with_scope(&schema, &mut descriptions);

        let mut entry_used = vec![false; ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES.len()];
        let mut wrong_object = Vec::new();
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

        assert_no_stale_allowlist_entries(
            "ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES",
            ALLOWED_CROSS_OBJECT_PROPERTY_REFERENCES,
            &entry_used,
        );
    }

    /// CREW-67's exact bug, reproduced directly against
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

    /// The type-name check (CREW-46) and the property-name check
    /// (CREW-67) are separated entirely by which extractor feeds them --
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
            "the PascalCase names stay exactly where the existing CREW-46 check already looks"
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
