use std::path::Path;

use serde_json::json;

use super::*;
use crate::cli::parse_args;

fn schema(value: Value) -> OutputSchema {
    OutputSchema { path: "schema.json".into(), schema: value, retries_flag: None }
}

fn person() -> OutputSchema {
    schema(json!({
        "type": "object",
        "properties": {"name": {"type": "string"}, "age": {"type": "integer", "minimum": 0}},
        "required": ["name", "age"],
        "additionalProperties": false
    }))
}

#[test]
fn strips_one_surrounding_json_fence() {
    assert_eq!(strip_fence("  {\"a\":1}\n"), "{\"a\":1}");
    assert_eq!(strip_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    assert_eq!(strip_fence("```JSON\r\n[1, 2]\r\n```\n"), "[1, 2]");
    assert_eq!(strip_fence("```\n\"text\"\n```"), "\"text\"");
    assert_eq!(strip_fence("```rust\nfn main() {}\n```"), "```rust\nfn main() {}\n```");
    assert_eq!(strip_fence("```json\n1\n```\n```json\n2\n```"), "```json\n1\n```\n```json\n2\n```");
    assert_eq!(strip_fence("Here:\n```json\n1\n```"), "Here:\n```json\n1\n```");
}

#[test]
fn checks_responses_against_the_schema() {
    let person = person();
    assert_eq!(person.check("{\"name\": \"Ada\", \"age\": 36}"), Ok(json!({"name": "Ada", "age": 36})));
    assert_eq!(person.check("```json\n{\"name\": \"Ada\", \"age\": 36}\n```").unwrap()["age"], 36);
    assert_eq!(
        person.check("{\"name\": 1, \"age\": -1, \"extra\": true}"),
        Err(vec![
            "extra: schema is false".into(),
            "root: must not have additional properties".into(),
            "name: must be string".into(),
            "age: must be >= 0".into(),
        ])
    );
    assert_eq!(person.check("{\"age\": 3}"), Err(vec!["name: must have required properties name".into()]));
    let errors = person.check("Sure! {\"name\": \"Ada\"}").unwrap_err();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].starts_with("the response is not valid JSON: expected value"), "{errors:?}");
}

#[test]
fn the_instruction_and_correction_name_the_schema_and_errors() {
    let instruction = person().instruction();
    assert!(instruction.contains("only a JSON value that matches the JSON Schema"), "{instruction}");
    assert!(instruction.contains("\"required\": [\n    \"name\",\n    \"age\"\n  ]"), "{instruction}");
    let prompt = correction_prompt(&["name: must be string".into(), "age: must be >= 0".into()]);
    assert!(prompt.contains("\n- name: must be string\n- age: must be >= 0\n"), "{prompt}");
    assert!(prompt.ends_with("with no other text and no code fence."), "{prompt}");
}

#[test]
fn reports_schemas_pi_schema_cannot_enforce() {
    let problems = |value: Value| support::unsupported(&value);
    assert!(problems(person().schema).is_empty());
    let recursive = json!({"$defs": {"node": {"type": "object", "properties": {
        "children": {"type": "array", "items": {"$ref": "#/$defs/node"}}}}}, "$ref": "#/$defs/node"});
    assert!(problems(recursive).is_empty(), "recursion through items nests");
    assert!(problems(json!({"anyOf": [true, {"type": ["string", "null"]}], "not": false})).is_empty());
    for (value, expected) in [
        (json!([1]), "#: the schema must be a JSON object"),
        (json!({"type": "strng"}), "#/type: unknown type \"strng\""),
        (json!({"properties": {"a/b": {"type": ["string", 1]}}}), "#/properties/a~1b/type: unknown type 1"),
        (json!({"items": [{"type": "string"}, 3]}), "#/items/1: a schema must be an object or a boolean"),
        (json!({"$ref": "https://example.com/s.json"}), "#/$ref: \"https://example.com/s.json\" is not a local"),
        (json!({"$ref": "#/$defs/missing"}), "#/$ref: \"#/$defs/missing\" is not a local"),
        (json!({"pattern": "(?=a)"}), "#/pattern: pattern \"(?=a)\" does not compile: error: look-around"),
        (json!({"patternProperties": {"[": {}}}), "#/patternProperties/[: pattern \"[\" does not compile"),
        (json!({"anyOf": {"type": "string"}}), "#/anyOf: must be an array of schemas"),
        (json!({"unevaluatedProperties": false}), "#: \"unevaluatedProperties\" is not supported"),
        (json!({"$ref": "#"}), "#: $ref refers to itself without nesting"),
        (
            json!({"properties": {"a": {"anyOf": [{"$ref": "#/properties/a"}]}}}),
            "#/properties/a: $ref refers to itself without nesting",
        ),
    ] {
        let found = problems(value.clone());
        assert!(found.len() == 1 && found[0].starts_with(expected), "{value}: {found:?}");
    }
}

#[test]
fn reads_the_flags_and_the_file_up_front() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("person.json"), person().schema.to_string()).unwrap();
    std::fs::write(dir.path().join("broken.json"), "{\"type\": ").unwrap();
    std::fs::write(dir.path().join("odd.json"), "{\"type\": \"date\"}").unwrap();
    let load = |line: &str, print_mode: bool| {
        let args = parse_args(&line.split_whitespace().map(String::from).collect::<Vec<_>>());
        assert!(args.errors.is_empty(), "{:?}", args.errors);
        OutputSchema::from_args(&args, dir.path(), print_mode)
    };
    let loaded = load("--output-schema person.json --output-retries 2 -p hi", true).unwrap().unwrap();
    assert_eq!((&loaded.schema, loaded.retries()), (&person().schema, 2));
    assert_eq!(load("--output-schema=person.json", true).unwrap().unwrap().retries(), DEFAULT_OUTPUT_RETRIES);
    assert_eq!(load("-p hi", true), Ok(None));
    assert_eq!(load("--output-retries 3 -p hi", true).unwrap_err(), "--output-retries requires --output-schema");
    assert!(load("--output-schema missing.json", true).unwrap_err().starts_with("--output-schema: cannot read "));
    let broken = load("--output-schema broken.json", true).unwrap_err();
    assert!(broken.starts_with("--output-schema: broken.json is not valid JSON: EOF"), "{broken}");
    assert_eq!(
        load("--output-schema odd.json", true).unwrap_err(),
        "--output-schema: odd.json is not a supported JSON Schema:\n  #/type: unknown type \"date\""
    );
}

#[test]
fn interactive_mode_rejects_the_flag() {
    let args = parse_args(&["--output-schema".into(), "missing.json".into()]);
    let error = OutputSchema::from_args(&args, Path::new("."), false).unwrap_err();
    assert_eq!(error, "--output-schema requires print mode (-p or --mode json)", "checked before the file");
}

#[test]
fn extensions_that_register_the_flags_keep_them() {
    let schema = OutputSchema { retries_flag: Some(3), ..person() };
    assert_eq!(schema.unclaimed(|_| false), Some(schema.clone()));
    assert_eq!(schema.unclaimed(|name| name == "output-retries").unwrap().retries(), DEFAULT_OUTPUT_RETRIES);
    assert_eq!(schema.unclaimed(|name| name == "output-schema"), None);
}
