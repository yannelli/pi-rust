//! Ported from upstream pi-schema `coerce.test.ts` (the plain JSON Schema cases) and the
//! unresolved reference cases of `validator.test.ts`.

use pi_schema::{ValidationError, coerce, parse};
use serde_json::{Value, json};

/// The coerced `value` property of `{ value }` under an object schema with one required property.
fn coerce_value(schema: Value, value: Value) -> Value {
    let root = json!({"type": "object", "properties": {"value": schema}, "required": ["value"]});
    coerce(&root, &json!({ "value": value }))["value"].clone()
}

fn error(path: &str, keyword: &'static str, message: &str) -> ValidationError {
    ValidationError { path: path.into(), keyword, message: message.into() }
}

#[test]
fn leaves_values_it_cannot_coerce_for_the_check_to_report() {
    let schema = json!({"type": "object", "properties": {"count": {"type": "number"}}, "required": ["count"]});
    assert_eq!(coerce(&schema, &json!({"count": "many"})), json!({"count": "many"}));
    let schema = json!({"type": "object", "properties": {"count": {"type": "number"}, "name": {"type": "string"}}});
    assert_eq!(
        parse(&schema, &json!({"count": "many", "name": 7})),
        Err(vec![error("/count", "type", "expected number, got string")])
    );
    let tags = json!({"type": "object", "properties": {"count": {"type": "number"}, "tags": {"type": "array", "items": {"type": "string"}}}});
    assert_eq!(parse(&tags, &json!({"count": "42", "tags": "a"})), Ok(json!({"count": 42, "tags": ["a"]})));
}

#[test]
fn pass_1_drops_optional_nulls_also_in_array_items() {
    let item =
        json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "integer"}}, "required": ["a"]});
    assert_eq!(coerce(&json!({"type": "array", "items": item}), &json!([{"a": "x", "b": null}])), json!([{"a": "x"}]));
    let name = json!({"type": "object", "properties": {"name": {"type": "string"}}});
    // Pass 2 alone would give "null".
    assert_eq!(coerce(&name, &json!({"name": null})), json!({}));
}

#[test]
fn pass_2_converts_by_typebox_rules() {
    let cases = [
        (json!({"type": "number"}), json!("42"), json!(42)),
        (json!({"type": "number"}), json!(""), json!(0)),
        (json!({"type": "number"}), json!("true"), json!(1)),
        (json!({"type": "integer"}), json!("3.5"), json!(3)),
        (json!({"type": "integer"}), json!(1.5), json!(1)),
        (json!({"type": "boolean"}), json!("1"), json!(true)),
        (json!({"type": "boolean"}), json!("FALSE"), json!(false)),
        (json!({"type": "null"}), json!("null"), json!(null)),
        (json!({"type": "string"}), json!(42), json!("42")),
        (json!({"type": "string"}), json!(null), json!("null")),
        (json!({"type": "array", "items": {"type": "number"}}), json!("1"), json!([1])),
        (json!({"type": "array", "items": {"type": "string"}}), json!(null), json!(["null"])),
        (json!({"const": 1}), json!("1"), json!(1)),
        (json!({"const": 1}), json!("2"), json!("2")),
        (json!({"enum": [1, 2, "hello"]}), json!("2"), json!(2)),
        (json!({"enum": [1, 2, "hello"]}), json!("world"), json!("world")),
        (json!({"anyOf": [{"type": "number"}, {"type": "null"}]}), json!(null), json!(null)),
        (json!({"anyOf": [{"type": "number"}, {"type": "null"}]}), json!("42"), json!(42)),
        (json!({"anyOf": [{"type": "number"}, {"type": "null"}]}), json!("null"), json!(null)),
        (json!({"oneOf": [{"type": "boolean"}, {"type": "number"}]}), json!("1"), json!(true)),
    ];
    for (schema, input, expected) in cases {
        assert_eq!(coerce_value(schema.clone(), input.clone()), expected, "{schema} {input}");
    }
    let tuple = json!({"type": "array", "prefixItems": [{"type": "number"}, {"type": "boolean"}], "items": false});
    assert_eq!(coerce_value(tuple.clone(), json!(["1", "true"])), json!([1, true]));
    assert_eq!(coerce_value(tuple, json!("1")), json!("1"));
}

#[test]
fn pass_2_follows_all_of_refs_and_member_keywords() {
    let all_of = json!({"allOf": [
        {"type": "object", "properties": {"a": {"type": "number"}}},
        {"type": "object", "properties": {"b": {"type": "boolean"}}}
    ]});
    assert_eq!(coerce(&all_of, &json!({"a": "1", "b": "true"})), json!({"a": 1, "b": true}));
    let reference = json!({
        "type": "object",
        "properties": {"item": {"$ref": "#/$defs/item"}},
        "$defs": {"item": {"type": "object", "properties": {"id": {"type": "integer"}}}}
    });
    assert_eq!(coerce(&reference, &json!({"item": {"id": "7"}})), json!({"item": {"id": 7}}));
    let members = json!({
        "type": "object",
        "properties": {"a": {"type": "number"}},
        "patternProperties": {"^n_": {"type": "number"}},
        "additionalProperties": {"type": "boolean"}
    });
    assert_eq!(
        coerce(&members, &json!({"a": "1", "n_x": "2", "other": "true"})),
        json!({"a": 1, "n_x": 2, "other": true})
    );
}

#[test]
fn pass_3_coerces_like_ajv() {
    let cases = [
        (json!({"type": "boolean"}), json!(0), json!(false)),
        (json!({"type": "string"}), json!(true), json!("true")),
        (json!({"type": "null"}), json!(""), json!(null)),
        (json!({"type": ["number", "string"]}), json!("1"), json!("1")),
        (json!({"type": ["boolean", "number"]}), json!("1"), json!(1)),
        (json!({"type": ["integer", "null"]}), json!("5"), json!(5)),
        (json!({"type": ["array", "null"], "items": {"type": "string"}}), json!(null), json!(null)),
    ];
    for (schema, input, expected) in cases {
        assert_eq!(coerce_value(schema.clone(), input.clone()), expected, "{schema} {input}");
    }
    let integer = json!({"type": "object", "properties": {"value": {"type": "integer"}}, "required": ["value"]});
    for input in [json!("abc"), json!("4x"), json!({}), json!(["1", "2"])] {
        assert!(parse(&integer, &json!({ "value": input })).is_err(), "{input}");
    }
    let tuple = json!({"type": "array", "prefixItems": [{"type": ["integer", "null"]}, {"type": ["boolean", "null"]}]});
    assert_eq!(coerce(&tuple, &json!(["5", "true"])), json!([5, true]));
}

#[test]
fn coerces_through_recursive_schemas() {
    let tree = json!({
        "type": "object",
        "properties": {"t": {"$ref": "#/$defs/Tree"}},
        "$defs": {"Tree": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"$ref": "#/$defs/Tree"}}]}}
    });
    assert_eq!(parse(&tree, &json!({"t": ["a", ["b"]]})), Ok(json!({"t": ["a", ["b"]]})));
    assert_eq!(parse(&tree, &json!({"t": 5})), Ok(json!({"t": "5"})));
    assert_eq!(parse(&tree, &json!({"t": [5]})), Ok(json!({"t": ["5"]})));
    assert!(parse(&tree, &json!({"t": {}})).is_err());
    assert_eq!(coerce(&tree, &json!({"t": {"a": 1}})), json!({"t": {"a": 1}}));
    let own = json!({"anyOf": [{"type": "number"}, {"type": "array", "items": {"$ref": "#"}}]});
    for _ in 0..3 {
        assert!(parse(&own, &json!({})).is_err());
        assert_eq!(parse(&own, &json!("42")), Ok(json!(42)));
        assert_eq!(parse(&own, &json!([1, "2", ["3"]])), Ok(json!([1, 2, [3]])));
    }
}

#[test]
fn coerces_member_keywords_in_every_pass() {
    let plain = json!({
        "type": "object",
        "patternProperties": {"^n_": {"type": ["integer", "null"]}},
        "additionalProperties": {"type": "string"}
    });
    assert_eq!(coerce(&plain, &json!({"n_a": "5", "other": 7})), json!({"n_a": 5, "other": "7"}));
    let item = json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 1}}});
    let schemas = [
        json!({"type": "object", "properties": {"entry": item}, "required": ["entry"]}),
        json!({"type": "object", "additionalProperties": item}),
        json!({"type": "object", "patternProperties": {"^entry$": item}, "additionalProperties": false}),
    ];
    for schema in schemas {
        assert_eq!(parse(&schema, &json!({"entry": {"n": null}})), Ok(json!({"entry": {}})), "{schema}");
    }
    let keep = json!({"type": "object", "additionalProperties": {"type": ["integer", "null"]}});
    assert_eq!(parse(&keep, &json!({"n": null})), Ok(json!({"n": null})));
    let union = json!({
        "type": "object",
        "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
        "anyOf": [{"required": ["a"]}, {"required": ["b"]}]
    });
    assert_eq!(parse(&union, &json!({"a": "3.5"})), Ok(json!({"a": 3})));
    for keyword in ["anyOf", "oneOf"] {
        let mut schema = json!({"type": "object", "properties": {"c": {"type": "integer"}}, "required": ["c"]});
        schema[keyword] = json!([
            {"type": "object", "properties": {"a": {"type": "array", "items": {"type": "integer"}}}, "required": ["a"]},
            {"type": "object", "properties": {"b": {"type": "boolean"}}, "required": ["b"]}
        ]);
        assert_eq!(parse(&schema, &json!({"a": "3", "c": "2"})), Ok(json!({"a": [3], "c": 2})), "{keyword}");
    }
}

#[test]
fn unresolved_references_reject_only_values_at_their_place() {
    let dangling = json!({"type": "object", "properties": {"t": {"$ref": "#/definitions/Missing"}}});
    let external = json!({"type": "object", "properties": {"t": {"$ref": "https://example.com/schemas/t.json"}}});
    for schema in [&dangling, &external] {
        assert_eq!(parse(schema, &json!({})), Ok(json!({})));
        assert!(parse(schema, &json!({"t": 1})).is_err());
    }
    assert_eq!(
        parse(&dangling, &json!({"t": "1"})),
        Err(vec![error("/t", "$ref", "unresolved reference #/definitions/Missing")])
    );
}
