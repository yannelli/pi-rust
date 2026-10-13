//! Ported from upstream `packages/ai/test/validation.test.ts`, plus TypeBox error formats.

use serde_json::{Value, json};

use super::{is_valid, validate_tool_arguments};

fn plain(schema: Value) -> Value {
    json!({ "type": "object", "properties": { "value": schema }, "required": ["value"] })
}

fn validate(schema: &Value, args: Value) -> Result<Value, String> {
    validate_tool_arguments("echo", schema, &args)
}

#[test]
fn coerces_plain_json_schemas_with_primitive_rules() {
    let cases = [
        (json!({"type": "number"}), json!("42"), json!(42)),
        (json!({"type": "number"}), json!(true), json!(1)),
        (json!({"type": "number"}), json!(null), json!(0)),
        (json!({"type": "integer"}), json!("42"), json!(42)),
        (json!({"type": "boolean"}), json!("true"), json!(true)),
        (json!({"type": "boolean"}), json!("false"), json!(false)),
        (json!({"type": "boolean"}), json!(1), json!(true)),
        (json!({"type": "boolean"}), json!(0), json!(false)),
        (json!({"type": "string"}), json!(null), json!("")),
        (json!({"type": "string"}), json!(true), json!("true")),
        (json!({"type": "null"}), json!(""), json!(null)),
        (json!({"type": "null"}), json!(0), json!(null)),
        (json!({"type": "null"}), json!(false), json!(null)),
        (json!({"type": ["number", "string"]}), json!("1"), json!("1")),
        (json!({"type": ["boolean", "number"]}), json!("1"), json!(1)),
        (json!({"type": "number"}), json!("0x1A"), json!(26)),
    ];
    for (schema, input, expected) in cases {
        let result = validate(&plain(schema.clone()), json!({ "value": input }));
        assert_eq!(result, Ok(json!({ "value": expected })), "{schema} {input}");
    }
}

#[test]
fn treats_null_as_omission_for_optional_non_nullable_properties() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "offset": {"type": "number"},
            "nullable": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            "metadata": {"type": "object", "properties": {"enabled": {"type": "boolean"}}}
        },
        "required": ["path", "metadata"]
    });
    let args = json!({"path": "file.txt", "offset": null, "nullable": null, "metadata": {"enabled": null}});
    assert_eq!(validate(&schema, args), Ok(json!({"path": "file.txt", "nullable": null, "metadata": {}})));
}

#[test]
fn keeps_nulls_that_referenced_or_union_schemas_accept() {
    let referenced = json!({
        "type": "object",
        "properties": {"value": {"$ref": "#/$defs/value"}},
        "$defs": {"value": {"anyOf": [{"type": "number"}, {"type": "null"}]}}
    });
    assert_eq!(validate(&referenced, json!({"value": null})), Ok(json!({"value": null})));
    let union = plain(json!({"anyOf": [{"type": "number"}, {"type": "null"}]}));
    assert_eq!(validate(&union, json!({"value": null})), Ok(json!({"value": null})));
    let one_of = plain(json!({"oneOf": [{"type": "number"}, {"type": "null"}]}));
    assert_eq!(validate(&one_of, json!({"value": null})), Ok(json!({"value": null})));
    assert_eq!(validate(&union, json!({"value": "42"})), Ok(json!({"value": 42})));
    let nullable_array = plain(json!({"type": ["array", "null"], "items": {"type": "string"}}));
    assert_eq!(validate(&nullable_array, json!({"value": null})), Ok(json!({"value": null})));
}

#[test]
fn rejects_invalid_coercions() {
    let cases = [
        (json!({"type": "boolean"}), json!("1")),
        (json!({"type": "boolean"}), json!("0")),
        (json!({"type": "null"}), json!("null")),
        (json!({"type": "integer"}), json!("42.1")),
        (json!({"type": "string"}), json!({"nested": true})),
    ];
    for (schema, input) in cases {
        let error = validate(&plain(schema.clone()), json!({ "value": input })).unwrap_err();
        assert!(error.starts_with("Validation failed"), "{schema}: {error}");
    }
}

#[test]
fn reports_errors_in_the_typebox_format() {
    let schema = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "limit": {"type": "integer", "minimum": 1},
            "mode": {"type": "string", "enum": ["a", "b"]},
            "edits": {"type": "array", "items": {"type": "object", "properties": {"oldText": {"type": "string"}}, "required": ["oldText"]}}
        },
        "required": ["path", "edits"],
        "additionalProperties": false
    });
    let args = json!({"limit": 0, "extra": 1, "mode": "c", "edits": [{}]});
    let error = validate_tool_arguments("read", &schema, &args).unwrap_err();
    let expected = "Validation failed for tool \"read\":\n  - path: must have required properties path\n  - extra: schema is false\n  - root: must not have additional properties\n  - limit: must be >= 1\n  - mode: must be equal to one of the allowed values\n  - edits.0.oldText: must have required properties oldText\n\nReceived arguments:\n";
    assert!(error.starts_with(expected), "{error}");
    assert!(error.ends_with(&serde_json::to_string_pretty(&args).unwrap()));
}

#[test]
fn checks_strings_numbers_arrays_and_unions() {
    let schema = plain(json!({"type": "string", "minLength": 2, "pattern": "^[a-z]+$", "format": "email"}));
    let error = validate(&schema, json!({"value": "A"})).unwrap_err();
    assert!(error.contains("  - value: must not have fewer than 2 characters"), "{error}");
    assert!(error.contains("  - value: must match format \"email\""), "{error}");
    assert!(error.contains("  - value: must match pattern \"^[a-z]+$\""), "{error}");
    let numbers = plain(json!({"type": "number", "exclusiveMaximum": 5, "multipleOf": 2}));
    let error = validate(&numbers, json!({"value": 7})).unwrap_err();
    assert!(error.contains("must be < 5") && error.contains("must be multiple of 2"), "{error}");
    let arrays = plain(json!({"type": "array", "maxItems": 1, "uniqueItems": true}));
    let error = validate(&arrays, json!({"value": [1, 1]})).unwrap_err();
    assert!(error.contains("must not have more than 1 items") && error.contains("must not have duplicate items"));
    let union = plain(json!({"anyOf": [{"type": "string"}, {"type": "number"}]}));
    let error = validate(&union, json!({"value": [true]})).unwrap_err();
    assert!(
        error.contains("  - value: must be string\n  - value: must be number\n  - value: must match a schema in anyOf")
    );
    let typed = plain(json!({"type": ["string", "number"]}));
    assert!(validate(&typed, json!({"value": {}})).unwrap_err().contains("must be either string or number"));
    assert!(
        validate(&plain(json!({"type": "string", "format": "date-time"})), json!({"value": "2020-12-12T20:20:40Z"}))
            .is_ok()
    );
    assert!(is_valid(&json!({"const": 1}), &json!(1.0)));
}

#[test]
fn caps_errors_at_eight() {
    let properties: serde_json::Map<String, Value> =
        (0..12).map(|index| (format!("p{index}"), json!({"type": "string"}))).collect();
    let schema = json!({"type": "object", "properties": properties});
    let args: serde_json::Map<String, Value> = (0..12).map(|index| (format!("p{index}"), json!([index]))).collect();
    let error = validate(&schema, Value::Object(args)).unwrap_err();
    assert_eq!(error.matches("must be string").count(), 8);
}
