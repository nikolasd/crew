//! Canonical JSON bytes for hashing and durable storage.
//!
//! This workspace enables `serde_json`'s `preserve_order` feature
//! (`Cargo.toml`), so `serde_json::Map` is insertion-ordered: two
//! structurally-equal values built in different key orders serialize to
//! different bytes. Every caller that hashes JSON or writes it to the
//! journal must therefore canonicalize first -- key order is not a
//! property anything downstream may depend on.

use serde_json::Value;

/// Canonicalizes `value` in place by sorting every object's keys
/// lexicographically at every nesting depth. Arrays keep their order because
/// element order is semantic in JSON, while key order is not.
///
/// Use this when the caller owns the value and can mutate it directly. This
/// delegates to `serde_json::Value::sort_all_objects`, which is why
/// `Cargo.toml` requires `serde_json` 1.0.129 or later.
pub fn canonicalize_in_place(value: &mut Value) {
    value.sort_all_objects();
}

/// Clones `value` and canonicalizes every object's keys lexicographically at
/// every nesting depth. Arrays keep their order because element order is
/// semantic in JSON, while key order is not.
///
/// Use [`canonicalize_in_place`] when the caller owns a mutable value and
/// wants to avoid this clone.
#[must_use]
pub fn canonicalize(value: &Value) -> Value {
    let mut owned = value.clone();
    canonicalize_in_place(&mut owned);
    owned
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::canonicalize;

    #[test]
    fn differently_ordered_equal_objects_canonicalize_to_identical_bytes() {
        let first = json!({"b": 1, "a": {"d": 2, "c": 3}});
        let second = json!({"a": {"c": 3, "d": 2}, "b": 1});

        assert_eq!(
            canonicalize(&first).to_string(),
            canonicalize(&second).to_string()
        );
    }

    #[test]
    fn array_element_order_is_preserved() {
        let numbers = json!([3, 1, 2]);
        let objects = json!([{"b": 1, "a": 2}]);

        assert_eq!(canonicalize(&numbers), numbers);
        assert_eq!(canonicalize(&objects).to_string(), r#"[{"a":2,"b":1}]"#);
    }

    #[test]
    fn nested_objects_inside_arrays_are_canonicalized() {
        let first = json!([
            {"outer": {"d": 4, "c": 3}},
            {"outer": {"b": 2, "a": 1}}
        ]);
        let second = json!([
            {"outer": {"c": 3, "d": 4}},
            {"outer": {"a": 1, "b": 2}}
        ]);

        assert_eq!(
            canonicalize(&first).to_string(),
            canonicalize(&second).to_string()
        );
    }

    #[test]
    fn scalars_and_empty_containers_round_trip_unchanged() {
        let values = [
            json!(null),
            json!(true),
            json!(1.5),
            json!(-42),
            json!("value"),
            json!({}),
            json!([]),
        ];

        for value in values {
            assert_eq!(canonicalize(&value).to_string(), value.to_string());
        }
    }
}
