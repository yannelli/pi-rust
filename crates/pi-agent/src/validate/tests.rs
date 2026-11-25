//! Ported from upstream `packages/ai/test/validation.test.ts` and the plain JSON Schema cases of
//! `validation-probes.test.ts`.

use serde_json::{Value, json};

use super::{is_valid, validate_tool_arguments};
use pi_ai::json_schema::json_equal;

fn plain(schema: Value) -> Value {
    json!({ "type": "object", "properties": { "value": schema }, "required": ["value"] })
}

fn validate(schema: &Value, args: Value) -> Result<Value, String> {
    validate_tool_arguments("echo", schema, &args)
}

#[test]
fn coerces_serialized_plain_json_schemas_like_typebox_schemas() {
    let cases = [
        (json!({"type": "number"}), json!("42"), json!(42)),
        (json!({"type": "number"}), json!(true), json!(1)),
        (json!({"type": "number"}), json!(null), json!(0)),
        (json!({"type": "integer"}), json!("42"), json!(42)),
        (json!({"type": "boolean"}), json!("true"), json!(true)),
        (json!({"type": "boolean"}), json!("false"), json!(false)),
        (json!({"type": "boolean"}), json!(1), json!(true)),
        (json!({"type": "boolean"}), json!(0), json!(false)),
        // TypeBox's conversion rules come first: null is "null" for a required string.
        (json!({"type": "string"}), json!(null), json!("null")),
        (json!({"type": "string"}), json!(true), json!("true")),
        (json!({"type": "null"}), json!(""), json!(null)),
        (json!({"type": "null"}), json!(0), json!(null)),
        (json!({"type": "null"}), json!(false), json!(null)),
        (json!({"type": ["number", "string"]}), json!("1"), json!("1")),
        (json!({"type": ["boolean", "number"]}), json!("1"), json!(1)),
        (json!({"type": "boolean"}), json!("1"), json!(true)),
        (json!({"type": "boolean"}), json!("0"), json!(false)),
        (json!({"type": "null"}), json!("null"), json!(null)),
        (json!({"type": "integer"}), json!("42.1"), json!(42)),
        (json!({"type": "array", "items": {"type": "number"}}), json!("7"), json!([7])),
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
        (json!({"type": "boolean"}), json!("yes")),
        (json!({"type": "number"}), json!("abc")),
        (json!({"type": "null"}), json!("nil")),
        (json!({"type": "integer"}), json!("4x")),
    ];
    for (schema, input) in cases {
        let error = validate(&plain(schema.clone()), json!({ "value": input })).unwrap_err();
        assert!(error.starts_with("Validation failed"), "{schema}: {error}");
    }
}

#[test]
fn lists_pi_schema_errors_then_the_arguments_it_received() {
    let error = validate(&plain(json!({"type": "number"})), json!({"value": "many"})).unwrap_err();
    assert_eq!(
        error,
        "Validation failed for tool \"echo\":\n  - value: expected number, got string\n\nReceived arguments:\n{\n  \"value\": \"many\"\n}"
    );
    let missing = validate(&plain(json!({"type": "string"})), json!({})).unwrap_err();
    assert!(missing.contains("  - value: missing"), "{missing}");
    assert!(is_valid(&json!({"const": 1}), &json!(1.0)));
}

/// The outcome of one call, as the upstream probes print it: `ok <arguments>` or
/// `REJECT <first error line>`.
fn outcome(schema: &Value, args: &Value) -> Result<Value, String> {
    validate_tool_arguments("probe", schema, args)
        .map_err(|error| format!("REJECT {}", error.lines().nth(1).unwrap_or_default().trim()))
}

fn object(properties: Value, extra: Value) -> Value {
    let mut schema = json!({"type": "object", "properties": properties});
    schema.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
    schema
}

#[test]
fn matches_the_upstream_probes() {
    let tree = json!({
        "type": "object",
        "properties": {"t": {"$ref": "#/$defs/Tree"}},
        "$defs": {"Tree": {"anyOf": [{"type": "string"}, {"type": "array", "items": {"$ref": "#/$defs/Tree"}}]}}
    });
    let json_schema = json!({
        "type": "object",
        "properties": {"data": {"$ref": "#/$defs/Json"}},
        "required": ["data"],
        "$defs": {"Json": {"anyOf": [
            {"type": "string"}, {"type": "number"}, {"type": "boolean"},
            {"type": "array", "items": {"$ref": "#/$defs/Json"}},
            {"type": "object", "additionalProperties": {"$ref": "#/$defs/Json"}}
        ]}}
    });
    let filter = json!({
        "type": "object",
        "properties": {"where": {"$ref": "#/$defs/F"}},
        "$defs": {"F": {"oneOf": [
            {"type": "object", "properties": {"field": {"type": "string"}, "eq": {"type": "string"}}, "required": ["field", "eq"], "additionalProperties": false},
            {"type": "object", "properties": {"and": {"type": "array", "items": {"$ref": "#/$defs/F"}}}, "required": ["and"], "additionalProperties": false}
        ]}}
    });
    let ok = |value: Value| Ok::<Value, String>(value);
    let reject = |line: &str| Err::<Value, String>(format!("REJECT {line}"));
    let enum_values: Vec<String> = (0..5000).map(|index| format!("v{index}")).collect();
    let cases: Vec<(Value, Value, Result<Value, String>)> = vec![
        (
            object(json!({"a": {"type": "string"}}), json!({"additionalProperties": false})),
            json!({"a": "x", "b": 1}),
            reject("- b: unknown field"),
        ),
        (
            object(
                json!({"v": {"anyOf": [
                    {"type": "object", "properties": {"k": {"const": "x"}, "n": {"type": "number"}}, "required": ["k", "n"]},
                    {"type": "object", "properties": {"k": {"const": "y"}, "s": {"type": "string"}}, "required": ["k", "s"]}
                ]}}),
                json!({"required": ["v"]}),
            ),
            json!({"v": {"k": "y", "s": 2}}),
            ok(json!({"v": {"k": "y", "s": "2"}})),
        ),
        (
            object(json!({"s": {"type": "string", "minLength": 2}}), json!({})),
            json!({"s": "😀"}),
            reject("- s: expected at least 2 characters"),
        ),
        (
            object(json!({"s": {"type": "string", "pattern": "^\\p{L}+$"}}), json!({})),
            json!({"s": "p{L}"}),
            reject("- s: expected to match ^\\p{L}+$"),
        ),
        (
            object(json!({"n": {"type": "number", "multipleOf": 0.01}}), json!({})),
            json!({"n": 1.1}),
            ok(json!({"n": 1.1})),
        ),
        (
            object(json!({"a": {"type": "array", "uniqueItems": true}}), json!({})),
            json!({"a": [{"x": 1, "y": 2}, {"y": 2, "x": 1}]}),
            reject("- a: expected unique items"),
        ),
        (
            object(json!({"e": {"type": "string", "format": "email"}}), json!({})),
            json!({"e": "not-an-email"}),
            reject("- e: expected format email"),
        ),
        (
            object(json!({"u": {"type": "string", "format": "uri"}}), json!({})),
            json!({"u": "relative/path"}),
            reject("- u: expected format uri"),
        ),
        (
            object(json!({"h": {"type": "string", "format": "hostname"}}), json!({})),
            json!({"h": "under_score.example"}),
            reject("- h: expected format hostname"),
        ),
        (
            json!({"type": "object", "properties": {"t": {"$ref": "#/definitions/T"}}, "required": ["t"], "definitions": {"T": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}),
            json!({"t": {"path": 42}}),
            ok(json!({"t": {"path": "42"}})),
        ),
        (
            json!({"type": "object", "properties": {"t": {"$ref": "#/definitions/Missing"}}}),
            json!({"t": 1}),
            reject("- t: unresolved reference #/definitions/Missing"),
        ),
        (json!({"type": "object", "properties": {"t": {"$ref": "#/definitions/Missing"}}}), json!({}), ok(json!({}))),
        (
            json!({"type": "object", "properties": {"t": {"$ref": "https://example.com/schemas/t.json"}}}),
            json!({"t": 1}),
            reject("- t: unresolved reference https://example.com/schemas/t.json"),
        ),
        (
            json!({"type": "object", "properties": {"s": {"type": "string", "nullable": true}}, "required": ["s"]}),
            json!({"s": null}),
            ok(json!({"s": "null"})),
        ),
        (
            json!({"type": "object", "properties": {"p": {"type": "array", "items": [{"type": "number"}, {"type": "string"}], "additionalItems": false}}}),
            json!({"p": [1, "a", 3]}),
            reject("- p.2: no value is allowed here"),
        ),
        (
            json!({"type": "object", "properties": {"p": {"type": "array", "items": [{"type": "number"}, {"type": "string"}], "additionalItems": false}}}),
            json!({"p": ["1", 2]}),
            ok(json!({"p": [1, "2"]})),
        ),
        (
            json!({"type": "object", "properties": {"s": {"$ref": "#/definitions/S", "maxLength": 1}}, "definitions": {"S": {"type": "string"}}}),
            json!({"s": "abc"}),
            reject("- s: expected at most 1 characters"),
        ),
        (json!({"type": "object", "properties": {"n": {"type": "integer"}}}), json!({"n": 1.5}), ok(json!({"n": 1}))),
        (
            json!({"type": "object", "properties": {"p": {"type": "array", "items": {"type": "string"}}}, "required": ["p"]}),
            json!({"p": "[\"a\",\"b\"]"}),
            ok(json!({"p": ["[\"a\",\"b\"]"]})),
        ),
        (
            json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]}),
            json!({"n": "3.7"}),
            ok(json!({"n": 3})),
        ),
        (
            json!({"type": "object", "properties": {"a": true, "b": false}}),
            json!({"b": 1}),
            reject("- b: no value is allowed here"),
        ),
        (
            json!({"type": "object", "properties": {"e": {"type": "string", "enum": enum_values}}}),
            json!({"e": "v5000"}),
            reject(
                "- e: expected one of [\"v0\",\"v1\",\"v2\",\"v3\",\"v4\",\"v5\",\"v6\",\"v7\",\"v8\",\"v9\",\"v10\",\"v11\",\"v12\",\"v13\",\"v14\",\"v15\",\"v16\",\"v17\",\"v18\",\"v19\", ... and 4980 more]",
            ),
        ),
        (
            object(json!({"v": {"oneOf": [{"type": "string"}, {"type": "string", "maxLength": 5}]}}), json!({})),
            json!({"v": "abc"}),
            reject("- v: matches more than one alternative"),
        ),
        (
            object(json!({"v": {"oneOf": [{"type": "number"}, {"type": "integer"}]}}), json!({})),
            json!({"v": 2.5}),
            ok(json!({"v": 2.5})),
        ),
        (
            object(
                json!({"kind": {"type": "string"}, "n": {"type": "number"}}),
                json!({"if": {"properties": {"kind": {"const": "a"}}}, "then": {"required": ["n"]}}),
            ),
            json!({"kind": "a"}),
            reject("- n: missing"),
        ),
        (
            object(
                json!({"a": {"type": "string"}, "b": {"type": "string"}}),
                json!({"dependentRequired": {"a": ["b"]}}),
            ),
            json!({"a": "x"}),
            reject("- b: required when a is present"),
        ),
        (
            object(json!({"a": {"type": "string"}, "b": {"type": "string"}}), json!({"dependencies": {"a": ["b"]}})),
            json!({"a": "x"}),
            reject("- b: required when a is present"),
        ),
        (
            object(json!({}), json!({"propertyNames": {"pattern": "^[a-z]+$"}})),
            json!({"ABC": 1}),
            reject("- ABC: invalid property name"),
        ),
        (
            object(json!({"l": {"type": "array", "contains": {"type": "number"}}}), json!({})),
            json!({"l": ["a"]}),
            reject("- l: expected at least 1 matching item"),
        ),
        (object(json!({"s": {"not": {"type": "null"}}}), json!({})), json!({"s": null}), ok(json!({}))),
        (
            object(json!({"m": {"type": "object", "minProperties": 1}}), json!({})),
            json!({"m": {}}),
            reject("- m: expected at least 1 properties"),
        ),
        (
            json!({"allOf": [{"type": "object", "properties": {"a": {"type": "string"}}, "additionalProperties": false}, {"type": "object", "properties": {"b": {"type": "string"}}}]}),
            json!({"a": "x", "b": "y"}),
            reject("- b: unknown field"),
        ),
        (
            object(json!({"c": {"const": {"x": 1, "y": 2}}}), json!({})),
            json!({"c": {"y": 2, "x": 1}}),
            ok(json!({"c": {"y": 2, "x": 1}})),
        ),
        (object(json!({"v": {"type": ["string", "number"]}}), json!({})), json!({"v": true}), ok(json!({"v": "true"}))),
        (object(json!({"v": {"type": ["string", "number"]}}), json!({})), json!({"v": null}), ok(json!({}))),
        (
            object(json!({"v": {"type": ["string", "null"]}}), json!({"required": ["v"]})),
            json!({"v": 5}),
            ok(json!({"v": "5"})),
        ),
        (
            object(json!({"n": {"type": "number", "exclusiveMinimum": 0}}), json!({})),
            json!({"n": "0"}),
            reject("- n: expected > 0"),
        ),
        (
            object(json!({"l": {"type": "array", "maxItems": 1}}), json!({})),
            json!({"l": [1, 2]}),
            reject("- l: expected at most 1 items"),
        ),
        (object(json!({"l": {"type": "array", "maxItems": 1}}), json!({})), json!({"l": "x"}), ok(json!({"l": ["x"]}))),
        (
            object(
                json!({"p": {
                    "oneOf": [
                        object(json!({"kind": {"type": "string", "enum": ["a"]}, "x": {"type": "number"}}), json!({"required": ["kind"]})),
                        object(json!({"kind": {"type": "string", "enum": ["b"]}}), json!({"required": ["kind"]}))
                    ],
                    "discriminator": {"propertyName": "kind"}
                }}),
                json!({}),
            ),
            json!({"p": {"kind": "a", "x": "1"}}),
            ok(json!({"p": {"kind": "a", "x": 1}})),
        ),
        (
            json!({"$ref": "#/$defs/Root", "$defs": {"Root": object(json!({"a": {"type": "string"}}), json!({"required": ["a"]}))}}),
            json!({"a": 1}),
            ok(json!({"a": "1"})),
        ),
        (
            json!({"$ref": "#/$defs/Root", "$defs": {"Root": object(json!({"a": {"type": "string"}}), json!({"required": ["a"]}))}}),
            json!({}),
            reject("- a: missing"),
        ),
        (
            object(
                json!({"root": {"$ref": "#/$defs/N"}}),
                json!({"$defs": {"N": object(json!({"c": {"type": "array", "items": {"$ref": "#/$defs/N"}}, "v": {"type": "integer"}}), json!({}))}}),
            ),
            json!({"root": {"v": "2", "c": [{"v": "3", "c": []}]}}),
            ok(json!({"root": {"v": 2, "c": [{"v": 3, "c": []}]}})),
        ),
        (
            object(json!({"a": {"$ref": "#/$defs/a%20b"}}), json!({"$defs": {"a b": {"type": "string"}}})),
            json!({"a": 1}),
            ok(json!({"a": "1"})),
        ),
        (
            object(json!({"a": {"$ref": "#/$defs/a~1b"}}), json!({"$defs": {"a/b": {"type": "string"}}})),
            json!({"a": "x"}),
            ok(json!({"a": "x"})),
        ),
        (
            object(
                json!({"a": {"$ref": "item.json"}}),
                json!({"$defs": {"item": {"$id": "item.json", "type": "string"}}}),
            ),
            json!({"a": 2}),
            ok(json!({"a": "2"})),
        ),
        (
            object(json!({"d": {"type": "string", "format": "date-time"}}), json!({})),
            json!({"d": "2024-01-01 00:00:00Z"}),
            reject("- d: expected format date-time"),
        ),
        (
            object(json!({"u": {"type": "string", "format": "url"}}), json!({})),
            json!({"u": "not a url"}),
            reject("- u: expected format url"),
        ),
        (json!({"type": "object", "required": ["a"]}), json!({}), reject("- a: missing")),
        (json!({"type": "object", "required": ["a"]}), json!({"a": null}), ok(json!({"a": null}))),
        (object(json!({"n": {"type": "number"}}), json!({})), json!({"n": " 12 "}), ok(json!({"n": 12}))),
        (object(json!({"n": {"type": "number"}}), json!({})), json!({"n": "0x10"}), ok(json!({"n": 16}))),
        (object(json!({"n": {"type": "number"}}), json!({})), json!({"n": "1e3"}), ok(json!({"n": 1000}))),
        (object(json!({"n": {"type": "number"}}), json!({})), json!({"n": ""}), ok(json!({"n": 0}))),
        (tree.clone(), json!({"t": ["a", ["b"]]}), ok(json!({"t": ["a", ["b"]]}))),
        (tree.clone(), json!({"t": 5}), ok(json!({"t": "5"}))),
        (tree.clone(), json!({"t": [5]}), ok(json!({"t": ["5"]}))),
        (tree, json!({"t": {}}), reject("- t: expected string, got object")),
        (json_schema.clone(), json!({"data": null}), ok(json!({"data": "null"}))),
        (json_schema, json!({"data": {"a": [1, null]}}), ok(json!({"data": {"a": [1, "null"]}}))),
        (
            filter.clone(),
            json!({"where": {"and": [{"field": "a", "eq": "b"}]}}),
            ok(json!({"where": {"and": [{"field": "a", "eq": "b"}]}})),
        ),
        (
            filter.clone(),
            json!({"where": {"and": [{"field": "a", "eq": 1, "x": 2}]}}),
            reject("- where.and.0.eq: expected string, got integer"),
        ),
        (
            filter,
            json!({"where": {"and": [{"and": [{"and": [{"and": [{"and": [{"field": 1}]}]}]}]}]}}),
            reject("- where.and.0.and.0.and.0.and.0.and.0.eq: missing"),
        ),
        (
            object(
                json!({"value": {"anyOf": [{"type": "number"}, {"type": "array", "items": {"$ref": "#/properties/value"}}]}}),
                json!({"required": ["value"]}),
            ),
            json!({"value": "42"}),
            ok(json!({"value": 42})),
        ),
        (
            object(json!({"mode": {"type": "string", "enum": ["", "x"]}}), json!({"required": ["mode"]})),
            json!({"mode": null}),
            ok(json!({"mode": ""})),
        ),
    ];
    for (schema, args, expected) in cases {
        let actual = outcome(&schema, &args);
        let same = match (&actual, &expected) {
            (Ok(actual), Ok(expected)) => json_equal(actual, expected),
            _ => actual == expected,
        };
        assert!(same, "{schema} {args}: {actual:?} != {expected:?}");
    }
}

#[test]
fn explains_a_deep_value_every_branch_of_a_recursive_union_rejects() {
    let branch = |required: &str| json!({"type": "object", "additionalProperties": {"$ref": "#/$defs/S"}, "required": [required]});
    let schema = json!({
        "type": "object",
        "properties": {"s": {"$ref": "#/$defs/S"}},
        "$defs": {"S": {"anyOf": [branch("a"), branch("b")]}}
    });
    let mut value = json!({"x": 1});
    for _ in 0..40 {
        value = json!({"c": value});
    }
    let error = validate_tool_arguments("deep", &schema, &json!({"s": value})).unwrap_err();
    let lines: Vec<&str> = error.lines().skip(1).take_while(|line| !line.is_empty()).collect();
    assert_eq!(lines.len(), 42);
    assert_eq!(lines[0], "  - s.a: missing");
    assert_eq!(lines[41], format!("  - s{}.x: expected object, got integer", ".c".repeat(40)));
}
