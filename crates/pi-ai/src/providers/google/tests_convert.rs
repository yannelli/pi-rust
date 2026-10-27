use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::build_params;
use super::convert::{
    convert_messages, convert_tools, function_calling_mode, requires_tool_call_id, supports_strict_tool_sampling,
};
use super::thinking::resolve_google_thinking_level;
use crate::registry::{StreamOptions, ThinkingBudgets};
use crate::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason, ThinkingLevel, Tool, ToolCall,
    ToolResultMessage,
};

const SIG: &str = "AAAAAAAAAAAAAAAAAAAAAA==";

fn model(api: &str, provider: &str, id: &str, images: bool) -> Model {
    let input = if images { vec!["text".into(), "image".into()] } else { vec!["text".into()] };
    Model {
        id: id.into(),
        api: api.into(),
        provider: provider.into(),
        reasoning: true,
        input,
        max_tokens: 4096,
        ..Model::default()
    }
}

fn call(id: &str, signature: Option<&str>) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        arguments: json!({"command": "ls"}).as_object().unwrap().clone(),
        thought_signature: signature.map(str::to_string),
        ..ToolCall::default()
    })
}

fn assistant(source: &Model, content: Vec<ContentBlock>) -> Message {
    Message::Assistant(AssistantMessage {
        content,
        api: source.api.clone(),
        provider: source.provider.clone(),
        model: source.id.clone(),
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::default()
    })
}

fn result(id: &str, content: Vec<ContentBlock>) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "bash".into(),
        content,
        ..ToolResultMessage::default()
    })
}

fn contents(target: &Model, messages: Vec<Message>) -> Vec<Value> {
    let mut all = vec![Message::user("Hi")];
    all.extend(messages);
    convert_messages(target, &Context { messages: all, ..Context::default() })
}

fn model_turn(contents: &[Value]) -> Value {
    contents.iter().find(|content| content["role"] == "model").cloned().unwrap()
}

#[test]
fn gemini3_keeps_tool_call_ids_and_only_valid_same_model_signatures() {
    let target = model("google-generative-ai", "google", "gemini-3-pro-preview", false);
    let history = |source: &Model, signature| {
        vec![
            assistant(source, vec![call("call_1", signature), call("call_2", None)]),
            result("call_1", vec![ContentBlock::text("hi")]),
            result("call_2", vec![ContentBlock::text("files")]),
        ]
    };
    let converted = contents(&target, history(&target, Some(SIG)));
    let turn = model_turn(&converted);
    assert_eq!(turn["parts"][0]["functionCall"]["id"], "call_1");
    assert_eq!(turn["parts"][0]["thoughtSignature"], SIG);
    assert!(turn["parts"][1].get("thoughtSignature").is_none());
    let responses = converted[2]["parts"].as_array().unwrap();
    let ids: Vec<&str> = responses.iter().map(|part| part["functionResponse"]["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["call_1", "call_2"]);
    let other = Model { id: "other-model".into(), ..target.clone() };
    let turn = model_turn(&contents(&target, history(&other, Some(SIG))));
    assert!(!turn.to_string().contains(SIG) && !turn.to_string().contains("skip_thought_signature_validator"));

    let gemini2 = model("google-generative-ai", "google", "gemini-2.5-flash", false);
    let converted = contents(&gemini2, history(&other, None));
    assert!(model_turn(&converted)["parts"][0]["functionCall"].get("id").is_none());
    assert!(converted[2]["parts"][0]["functionResponse"].get("id").is_none());
    for (expected, id) in
        [(false, "gemini-2.5-flash"), (true, "gemini-3.6-flash"), (true, "claude-sonnet-4-5"), (true, "gpt-oss-120b")]
    {
        assert_eq!(requires_tool_call_id(id), expected, "{id}");
    }
}

#[test]
fn image_tool_results_nest_for_gemini3_and_split_for_older_models() {
    let history = |target: &Model| {
        vec![
            assistant(target, vec![call("call_a", None), call("call_img", None), call("call_b", None)]),
            result("call_a", vec![ContentBlock::text("alpha text")]),
            result("call_img", vec![ContentBlock::Image { data: "abc".into(), mime_type: "image/png".into() }]),
            result("call_b", vec![ContentBlock::text("beta text")]),
        ]
    };
    let old = model("google-generative-ai", "google", "gemini-2.5-flash", true);
    let converted = contents(&old, history(&old));
    assert_eq!(converted.len(), 5);
    assert_eq!(converted[3]["parts"][0]["text"], "Tool result image:");
    assert!(converted[3]["parts"][1]["inlineData"].is_object());
    assert!(converted[4]["parts"][0]["functionResponse"].is_object());

    let new = model("google-generative-ai", "google", "gemini-3-pro-preview", true);
    let converted = contents(&new, history(&new));
    assert_eq!(converted.len(), 3);
    let response = &converted[2]["parts"][1]["functionResponse"];
    assert_eq!(response["response"], json!({"output": "(see attached image)"}));
    assert!(response["parts"][0]["inlineData"].is_object());
}

#[test]
fn signed_empty_blocks_survive_only_for_the_same_model() {
    let target = model("google-generative-ai", "google", "gemini-3-pro-preview", false);
    let thinking = |signature: Option<&str>| ContentBlock::Thinking {
        thinking: String::new(),
        thinking_signature: signature.map(str::to_string),
        redacted: false,
    };
    let text = |text: &str, signature: Option<&str>| ContentBlock::Text {
        text: text.into(),
        text_signature: signature.map(str::to_string),
    };
    let turn = model_turn(&contents(&target, vec![assistant(&target, vec![thinking(Some(SIG)), call("c", None)])]));
    assert_eq!(
        (turn["parts"][0]["thought"].clone(), turn["parts"][0]["thoughtSignature"].clone()),
        (json!(true), json!(SIG))
    );
    let turn = model_turn(&contents(&target, vec![assistant(&target, vec![text("", Some(SIG)), call("c", None)])]));
    assert_eq!(turn["parts"][0], json!({"text": "", "thoughtSignature": SIG}));
    let unsigned = vec![thinking(None), text("   ", None), call("c", None)];
    let turn = model_turn(&contents(&target, vec![assistant(&target, unsigned)]));
    assert_eq!(turn["parts"].as_array().unwrap().len(), 1);
    let other = Model { id: "other-model".into(), ..target.clone() };
    let turn = model_turn(&contents(
        &target,
        vec![assistant(&other, vec![thinking(Some(SIG)), text("", Some(SIG)), call("c", None)])],
    ));
    assert_eq!(turn["parts"].as_array().unwrap().len(), 1);
}

#[test]
fn tool_declarations_strip_meta_keys_only_for_openapi_parameters() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#", "$id": "urn:x", "$comment": "c",
        "$defs": {"d": {"type": "string"}}, "definitions": {"l": {"type": "number"}}, "type": "object",
        "properties": {"deep": {"$id": "urn:nested", "type": "string"}, "ref": {"$ref": "#/$defs/d"}}, "required": ["deep"]
    });
    let tools = vec![Tool::new("test_tool", "A test tool", schema.clone())];
    let openapi = convert_tools(&tools, true, true).unwrap().unwrap();
    assert_eq!(
        openapi[0]["functionDeclarations"][0]["parameters"],
        json!({"type": "object", "properties": {"deep": {"type": "string"}, "ref": {"$ref": "#/$defs/d"}}, "required": ["deep"]})
    );
    assert_eq!(tools[0].parameters, schema);
    let json_schema = convert_tools(&tools, false, true).unwrap().unwrap();
    assert_eq!(json_schema[0]["functionDeclarations"][0]["parametersJsonSchema"], schema);
    assert_eq!(convert_tools(&[], false, true), Ok(None));
}

#[test]
fn strict_tools_use_validated_mode_on_gemini3() {
    let mut tool = Tool::new("test_tool", "A test tool", json!({"type": "object", "properties": {}}));
    tool.constrained_sampling = Some(json!({"type": "json_schema", "strict": "require"}));
    assert!(supports_strict_tool_sampling("gemini-3.1-pro-preview"));
    assert!(!supports_strict_tool_sampling("gemini-2.5-pro"));
    assert_eq!(function_calling_mode(std::slice::from_ref(&tool), None, true), Ok(Some("VALIDATED")));
    assert_eq!(function_calling_mode(std::slice::from_ref(&tool), Some("none"), true), Ok(Some("NONE")));
    let error = function_calling_mode(&[tool], None, false).unwrap_err();
    assert!(error.contains("Tool \"test_tool\" requires JSON-schema constrained sampling"));
}

fn mapped(id: &str, entries: &[(&str, Option<&str>)]) -> Model {
    let map: BTreeMap<String, Option<String>> =
        entries.iter().map(|(key, value)| (key.to_string(), value.map(str::to_string))).collect();
    Model {
        provider: "test-google".into(),
        thinking_level_map: Some(map),
        ..model("google-generative-ai", "test-google", id, false)
    }
}

fn thinking_config(
    model: &Model,
    reasoning: Option<ThinkingLevel>,
    budgets: Option<ThinkingBudgets>,
    vertex: bool,
) -> Value {
    let options = StreamOptions { reasoning, thinking_budgets: budgets, ..StreamOptions::default() };
    let context = Context { messages: vec![Message::user("Hello")], ..Context::default() };
    build_params(model, &context, &options, vertex).unwrap()["config"]["thinkingConfig"].clone()
}

#[test]
fn thinking_level_maps_resolve_like_upstream() {
    for (mapped_value, expected) in [("minimal", "minimal"), ("LOW", "low"), ("Medium", "medium"), ("HIGH", "high")] {
        let target = mapped(
            "gemini-3.7-flash",
            &[("high", Some(mapped_value)), ("xhigh", Some(mapped_value)), ("max", Some(mapped_value))],
        );
        for level in [ThinkingLevel::High, ThinkingLevel::Xhigh, ThinkingLevel::Max] {
            assert_eq!(resolve_google_thinking_level(&target, level).unwrap().as_str(), expected);
        }
    }
    let invalid = mapped("gemini-3.7-flash", &[("xhigh", Some("extreme"))]);
    assert_eq!(
        resolve_google_thinking_level(&invalid, ThinkingLevel::Xhigh).unwrap_err(),
        "Unsupported Google thinking level mapping for test-google/gemini-3.7-flash: xhigh -> extreme"
    );
    assert_eq!(
        resolve_google_thinking_level(&mapped("gemini-3.7-flash", &[]), ThinkingLevel::Max).unwrap_err(),
        "Unsupported Google thinking level mapping for test-google/gemini-3.7-flash: max -> undefined"
    );
    let low_to_high = [
        ("off", None),
        ("minimal", None),
        ("low", Some("low")),
        ("medium", Some("medium")),
        ("high", Some("high")),
        ("xhigh", None),
        ("max", None),
    ];
    for vertex in [false, true] {
        assert_eq!(
            thinking_config(&mapped("gemini-3.8-flash", &low_to_high), None, None, vertex),
            json!({"thinkingLevel": "LOW"})
        );
        let pro = mapped("gemini-3.1-pro-preview", &low_to_high);
        assert_eq!(
            thinking_config(&pro, Some(ThinkingLevel::Medium), None, vertex),
            json!({"includeThoughts": true, "thinkingLevel": "MEDIUM"})
        );
        assert_eq!(thinking_config(&mapped("gemini-2.5-flash", &[]), None, None, vertex), json!({"thinkingBudget": 0}));
        assert_eq!(
            thinking_config(&mapped("gemini-4-argon", &[]), Some(ThinkingLevel::High), None, vertex),
            json!({"includeThoughts": true, "thinkingLevel": "HIGH"})
        );
    }
    let extended = mapped("gemini-3.7-flash", &[("xhigh", Some("high")), ("max", Some("high"))]);
    assert_eq!(thinking_config(&extended, Some(ThinkingLevel::Max), None, false)["thinkingLevel"], "HIGH");
    assert_eq!(
        thinking_config(&mapped("gemini-3.7-flash", &[("high", Some("LOW"))]), Some(ThinkingLevel::High), None, false)
            ["thinkingLevel"],
        "LOW"
    );
    let budgets = ThinkingBudgets { high: Some(1234), ..ThinkingBudgets::default() };
    let flash = mapped("gemini-2.5-flash", &[("xhigh", Some("high"))]);
    assert_eq!(thinking_config(&flash, Some(ThinkingLevel::Xhigh), Some(budgets), false)["thinkingBudget"], 1234);
    let lite = mapped("gemini-2.5-flash-lite", &[]);
    assert_eq!(thinking_config(&lite, Some(ThinkingLevel::Minimal), None, false)["thinkingBudget"], 512);
    assert_eq!(thinking_config(&lite, Some(ThinkingLevel::Minimal), None, true)["thinkingBudget"], 128);
    assert_eq!(
        thinking_config(&mapped("gemini-exp", &[]), Some(ThinkingLevel::Low), None, false)["thinkingBudget"],
        -1
    );
}

fn json_schema_declaration(parameters: &Value) -> Value {
    let tools = vec![Tool::new("test_tool", "A test tool", parameters.clone())];
    convert_tools(&tools, false, true).unwrap().unwrap()[0]["functionDeclarations"][0]["parametersJsonSchema"].clone()
}

#[test]
fn const_values_become_single_value_enums_in_json_schema() {
    let parameters = json!({
        "type": "object",
        "properties": {
            "origin": {"type": "string", "const": "zq-src-7"},
            "mode": {"anyOf": [{"type": "string", "const": "a"}, {"type": "string", "const": "b"}]},
            "level": {"oneOf": [{"const": 1}, {"const": 2}]},
            "flag": {"type": "boolean", "const": true},
            "list": {"type": "array", "items": {"const": "x"}},
            "const": {"type": "string"}
        },
        "required": ["origin"]
    });
    let original = parameters.clone();
    assert_eq!(
        json_schema_declaration(&parameters),
        json!({
            "type": "object",
            "properties": {
                "origin": {"type": "string", "enum": ["zq-src-7"]},
                "mode": {"anyOf": [{"type": "string", "enum": ["a"]}, {"type": "string", "enum": ["b"]}]},
                "level": {"oneOf": [{"enum": [1]}, {"enum": [2]}]},
                "flag": {"type": "boolean", "const": true},
                "list": {"type": "array", "items": {"enum": ["x"]}},
                "const": {"type": "string"}
            },
            "required": ["origin"]
        })
    );
    assert_eq!(parameters, original);
}
