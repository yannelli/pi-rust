//! TypeBox `Value.Default` and `Value.Clean` on the plain JSON schemas TypeBox builders emit. The expected values
//! were checked against TypeBox 1.x (pi-chord's golden fixtures cover the same paths through `normalize`).

use pi_schema::{check, check_undefined, clean, default_value, pattern_test};
use serde_json::{Value, json};

/// `Value.Clean(schema, Value.Default(schema, value))`, as pi-chord's `normalize` runs them.
fn normalize(schema: &Value, value: Value) -> Value {
    clean(schema, default_value(schema, Some(value)).unwrap())
}

#[test]
fn defaults_fill_missing_values_and_append_new_keys() {
    let schema = json!({
        "type": "object",
        "required": ["b"],
        "properties": {
            "a": {"type": "number", "default": 1},
            "b": {"type": "array", "items": {"type": "object", "required": ["c"], "properties": {"c": {"type": "string", "default": "x"}}}}
        }
    });
    let value = default_value(&schema, Some(json!({"b": [{}, {"c": "y"}]}))).unwrap();
    assert_eq!(value.to_string(), r#"{"b":[{"c":"x"},{"c":"y"}],"a":1}"#);
    assert_eq!(default_value(&json!({"type": "string"}), None), None);
    assert_eq!(default_value(&json!({"type": "string", "default": "d"}), None), Some(json!("d")));
    let nested = json!({
        "type": "object",
        "required": ["o"],
        "properties": {"o": {"type": "object", "required": ["x"], "properties": {"x": {"type": "number", "default": 1}}, "default": {}}}
    });
    assert_eq!(default_value(&nested, Some(json!({}))), Some(json!({"o": {"x": 1}})));
}

#[test]
fn clean_removes_what_the_schema_does_not_describe() {
    let object = json!({"type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}});
    assert_eq!(clean(&object, json!({"a": "x", "z": 1})), json!({"a": "x"}));
    let additional = json!({
        "type": "object", "required": ["a"], "properties": {"a": {"type": "string"}},
        "additionalProperties": {"type": "number"}
    });
    assert_eq!(clean(&additional, json!({"a": "x", "z": 1, "y": "s"})), json!({"a": "x", "z": 1}));
    let open = json!({"type": "object", "properties": {}, "additionalProperties": true});
    assert_eq!(clean(&open, json!({"z": 1})), json!({"z": 1}));
    let tuple = json!({"type": "array", "additionalItems": false, "items": [object.clone(), {"type": "number"}], "minItems": 2});
    assert_eq!(clean(&tuple, json!([{"a": "x", "q": 1}, 2, 3])), json!([{"a": "x"}, 2]));
    let record = json!({"type": "object", "patternProperties": {"^.*$": object.clone()}});
    assert_eq!(clean(&record, json!({"k": {"a": "x", "z": 1}})), json!({"k": {"a": "x"}}));
    assert_eq!(clean(&json!({"type": "string"}), json!({"kept": true})), json!({"kept": true}));
}

#[test]
fn clean_keeps_the_narrowest_matching_union_branch() {
    let wide = json!({"type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}});
    let narrow = json!({
        "type": "object", "required": ["a", "b"], "properties": {"a": {"type": "string"}, "b": {"type": "number"}}
    });
    let schema = json!({"anyOf": [wide, narrow]});
    assert_eq!(clean(&schema, json!({"a": "x", "b": 1, "c": 2})), json!({"a": "x", "b": 1}));
    assert_eq!(clean(&schema, json!({"a": "x", "c": 2})), json!({"a": "x"}));
    let disjoint = json!({"anyOf": [
        {"type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}},
        {"type": "object", "required": ["b"], "properties": {"b": {"type": "number"}}}
    ]});
    assert_eq!(normalize(&disjoint, json!({"b": 1, "c": 2})), json!({"b": 1}));
}

#[test]
fn intersections_default_and_clean_as_one_object() {
    let schema = json!({"allOf": [
        {"type": "object", "required": ["a"], "properties": {"a": {"type": "string"}}},
        {"type": "object", "properties": {"n": {"type": "number", "default": 2}}}
    ]});
    let value = normalize(&schema, json!({"a": "x", "z": true}));
    assert_eq!(value, json!({"a": "x", "n": 2}));
    assert!(check(&schema, &value));
}

#[test]
fn missing_values_and_patterns() {
    assert!(check_undefined(&json!({})));
    assert!(check_undefined(&json!({"type": "void"})));
    assert!(check_undefined(&json!({"anyOf": [{"type": "string"}, {"type": "undefined"}]})));
    assert!(!check_undefined(&json!({"type": "string"})));
    assert!(!check_undefined(&json!({"const": 1})));
    assert!(!check_undefined(&json!({"not": {}})));
    assert_eq!(pattern_test("^x$", "x"), Some(true));
    assert_eq!(pattern_test("^x$", "y"), Some(false));
    assert_eq!(pattern_test("(", "x"), None);
}
