use serde_json::{Value, json};

use super::*;

fn tool(parameters: Value, constrained: Option<Value>) -> Tool {
    Tool { constrained_sampling: constrained, ..Tool::new("sample_tool", "Sample tool", parameters) }
}

fn payload_schema() -> Value {
    json!({"type": "object", "properties": {"payload": {"type": "string"}}, "required": ["payload"], "additionalProperties": false})
}

#[test]
fn converts_supported_constraints_and_falls_back_when_unsupported() {
    let prefer = tool(payload_schema(), Some(json!({"type": "json_schema", "strict": "prefer"})));
    let converted = convert_responses_tools(&[prefer], ResponsesToolOptions::default()).unwrap();
    assert_eq!((converted[0]["type"].as_str(), converted[0]["strict"].as_bool()), (Some("function"), Some(true)));

    let require = tool(payload_schema(), Some(json!({"type": "json_schema", "strict": "require"})));
    let options = ResponsesToolOptions { supports_strict_mode: false, ..Default::default() };
    let error = convert_responses_tools(&[require], options).unwrap_err();
    assert!(error.starts_with("Tool \"sample_tool\" requires JSON-schema constrained sampling"), "{error}");

    let grammar =
        tool(payload_schema(), Some(json!({"type": "grammar", "variants": {"openai_lark": "start: /[a-z]+/"}})));
    let options = ResponsesToolOptions { supports_openai_grammar_tools: true, ..Default::default() };
    let converted = convert_responses_tools(std::slice::from_ref(&grammar), options).unwrap();
    assert_eq!(converted[0]["type"], "custom");
    assert_eq!(converted[0]["format"], json!({"type": "grammar", "syntax": "lark", "definition": "start: /[a-z]+/"}));

    let empty = tool(payload_schema(), Some(json!({"type": "grammar", "variants": {}})));
    let error = convert_responses_tools(&[empty], options).unwrap_err();
    assert_eq!(
        error,
        "Tool \"sample_tool\" cannot use grammar constrained sampling: no supported grammar variant was provided."
    );

    let options = ResponsesToolOptions { supports_strict_mode: false, ..Default::default() };
    let fallback = convert_responses_tools(&[grammar], options).unwrap();
    assert_eq!(fallback[0]["type"], "function");
    assert!(fallback[0].get("strict").is_none());

    let disabled = convert_responses_tools(&[tool(payload_schema(), Some(json!(false)))], Default::default());
    assert_eq!(disabled, convert_responses_tools(&[tool(payload_schema(), None)], Default::default()));
    let null_strict = ResponsesToolOptions { strict: None, ..Default::default() };
    assert_eq!(
        convert_responses_tools(&[tool(payload_schema(), None)], null_strict).unwrap()[0]["strict"],
        Value::Null
    );
}

#[test]
fn derives_strict_schemas_without_changing_tool_definitions() {
    let parameters = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "offset": {"type": "number"},
            "metadata": {"type": "object", "properties": {"enabled": {"type": "boolean"}}},
            "nullable": {"anyOf": [{"type": "string"}, {"type": "null"}]},
        },
        "required": ["path", "metadata"],
    });
    let strict = make_strict_json_schema(&parameters, None).unwrap();
    assert!(parameters.get("additionalProperties").is_none());
    assert_eq!(strict["additionalProperties"], false);
    assert_eq!(strict["required"], json!(["path", "offset", "metadata", "nullable"]));
    assert_eq!(strict["properties"]["offset"], json!({"anyOf": [{"type": "number"}, {"type": "null"}]}));
    assert_eq!(strict["properties"]["metadata"]["required"], json!(["enabled"]));
    assert_eq!(
        strict["properties"]["metadata"]["properties"]["enabled"],
        json!({"anyOf": [{"type": "boolean"}, {"type": "null"}]})
    );
    assert_eq!(strict["properties"]["nullable"], json!({"anyOf": [{"type": "string"}, {"type": "null"}]}));
}

#[test]
fn falls_back_or_rejects_schemas_that_cannot_be_converted() {
    let cases = [
        (
            json!({"type": "object", "properties": {"metadata": {"type": "object", "properties": {}, "additionalProperties": {"type": "string"}}}, "required": ["metadata"]}),
            "additionalProperties is unsupported",
        ),
        (
            json!({"allOf": [{"type": "object", "properties": {"a": {"type": "string"}}}, {"type": "object"}]}),
            "allOf schemas are unsupported",
        ),
        (
            json!({"type": "object", "properties": {"value": {"anyOf": [{"type": "object", "properties": {"nested": {"type": "string"}}}, {"type": "null"}]}}, "required": ["value"]}),
            "object and array unions are unsupported",
        ),
        (
            json!({"type": "object", "properties": {"child": {"$ref": "https://example.com/child.json"}}, "required": ["child"]}),
            "$ref schemas are unsupported",
        ),
    ];
    for (parameters, error) in cases {
        let mut candidate = tool(parameters.clone(), Some(json!({"type": "json_schema", "strict": "prefer"})));
        let message = make_strict_json_schema(&parameters, None).unwrap_err().0;
        assert!(message.contains(error), "{message}");
        assert_eq!(resolve_json_schema_strict_sampling(&candidate, true, None), Ok(None));
        let converted = convert_responses_tools(std::slice::from_ref(&candidate), Default::default()).unwrap();
        assert_eq!((converted[0]["strict"].as_bool(), &converted[0]["parameters"]), (Some(false), &parameters));
        candidate.constrained_sampling = Some(json!({"type": "json_schema", "strict": "require"}));
        let rejected = resolve_json_schema_strict_sampling(&candidate, true, None).unwrap_err();
        assert!(rejected.contains(error), "{rejected}");
    }
    let keyword = |key: &str, _: &Value| key == "format";
    let with_format = json!({"type": "object", "properties": {"a": {"type": "string", "format": "uri"}}});
    assert_eq!(make_strict_json_schema(&with_format, Some(&keyword)).unwrap_err().0, "format: \"uri\" is unsupported");
}

#[test]
fn keeps_grammar_input_json_deltas_append_only() {
    let mut buffer = GrammarToolInputJsonBuffer::default();
    let first = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"", false).unwrap().unwrap();
    let second = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true).unwrap().unwrap();
    let parsed: Value = serde_json::from_str(&format!("{first}{second}")).unwrap();
    assert_eq!(parsed, json!({"payload": "a\"\nb"}));
    assert_eq!(append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true), Ok(None));
    assert_eq!(
        append_grammar_tool_input_json_delta(&mut buffer, "payload", "changed", true),
        Err("grammar tool input for property \"payload\" changed after it was closed".into())
    );
    let mut fresh = GrammarToolInputJsonBuffer { input: "abc".into(), started: true, closed: false };
    assert!(append_grammar_tool_input_json_delta(&mut fresh, "payload", "xyz", false).is_err());
}

#[test]
fn grammar_tools_need_one_required_string_property() {
    let grammar = Some(json!({"type": "grammar", "variants": {"openai_regex": "[a-z]+"}}));
    let resolved = resolve_grammar_constrained_sampling(&tool(payload_schema(), grammar.clone()), true).unwrap();
    assert_eq!(
        resolved,
        Some(GrammarConstrainedSampling {
            format: GrammarFormat::Regex,
            definition: "[a-z]+".into(),
            input_property: "payload".into()
        })
    );
    assert_eq!(resolve_grammar_constrained_sampling(&tool(payload_schema(), grammar.clone()), false), Ok(None));
    let two_required = json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {"type": "string"}}, "required": ["a", "b"]});
    let error = resolve_grammar_constrained_sampling(&tool(two_required, grammar.clone()), true).unwrap_err();
    assert!(error.ends_with("requires exactly one required string property."), "{error}");
    let tools = [tool(payload_schema(), grammar), tool(payload_schema(), None)];
    let properties = grammar_tool_input_properties(&tools, true).unwrap();
    assert_eq!(properties.get("sample_tool").map(String::as_str), Some("payload"));
    let mut arguments = JsonObject::new();
    arguments.insert("payload".into(), json!(42));
    assert_eq!(
        grammar_tool_input("sample_tool", &arguments, "payload"),
        Err("Grammar tool call \"sample_tool\" requires argument \"payload\" to be a string.".into())
    );
}
