//! Mid-conversation system messages, tool additions, request options, and terminal events,
//! ported from upstream `transcript-tool-changes`, `openai-responses-*`, and `cache-retention` tests.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{ConvertOptions, OpenAIResponses, convert_messages};
use crate::providers::mock_server::{MockResponse, MockServer, capture_payload, collect, frame, test_model};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::transcript::normalize_context;
use crate::types::{Context, Message, StopReason, Tool};

const API: &str = "openai-responses";
const UNREACHABLE: &str = "http://127.0.0.1:9";

fn tool(name: &str) -> Tool {
    Tool::new(name, format!("{name} tool"), json!({"type": "object", "properties": {}}))
}

fn system(value: Value) -> Message {
    serde_json::from_value(value).unwrap()
}

fn options() -> StreamOptions {
    StreamOptions { api_key: Some("sk-test".into()), ..StreamOptions::default() }
}

/// System update that removes `base_tool`, adds `late_tool`, and patches sections.
fn changing_context() -> Context {
    let messages = vec![
        system(json!({"role": "system", "content": "base prompt",
            "sections": {"rules": "<rules>\nold rules\n</rules>", "docs": "<docs>\nread docs\n</docs>"},
            "toolsAdded": [tool("base_tool")], "timestamp": 0})),
        Message::user("before"),
        system(json!({"role": "system", "content": "updated guidance",
            "sections": {"rules": "<rules>\nnew rules\n</rules>", "docs": null},
            "toolsRemoved": [{"name": "base_tool"}], "toolsAdded": [tool("late_tool")], "timestamp": 2})),
    ];
    Context { messages, ..Context::default() }
}

fn addition_context() -> Context {
    let messages = vec![
        system(json!({"role": "system", "content": "base prompt", "toolsAdded": [tool("base_tool")], "timestamp": 0})),
        Message::user("before"),
        system(
            json!({"role": "system", "content": "updated guidance", "toolsAdded": [tool("late_tool")], "timestamp": 2}),
        ),
    ];
    Context { messages, ..Context::default() }
}

fn model(compat: Value) -> crate::types::Model {
    let mut model = test_model(API, "openai", UNREACHABLE);
    model.compat = Some(compat);
    model
}

fn names(tools: &Value) -> Vec<String> {
    tools.as_array().into_iter().flatten().map(|tool| tool["name"].as_str().unwrap().to_string()).collect()
}

/// Compat for models that declare every tool in `additional_tools` items.
fn additional_tools_model() -> crate::types::Model {
    model(json!({"supportsMidConvoSystemMessages": true, "supportsAdditionalTools": true}))
}

/// Names of the first tool in each `additional_tools` item.
fn additional_tool_names(input: &[Value]) -> Vec<&str> {
    let items = input.iter().filter(|item| item["type"] == "additional_tools");
    items.map(|item| item["tools"][0]["name"].as_str().unwrap()).collect()
}

/// Input items whose text contains `needle`.
fn texts_containing<'a>(input: &'a [Value], needle: &str) -> Vec<&'a str> {
    let texts = input.iter().filter_map(|item| item["content"].as_str());
    texts.filter(|text| text.contains(needle)).collect()
}

#[tokio::test]
async fn declares_every_tool_in_additional_tools_at_its_developer_message() {
    let payload = capture_payload(&OpenAIResponses, &additional_tools_model(), &addition_context(), options()).await;
    // A later additional_tools item cannot replace a top-level tool, so no tool is declared there.
    assert!(payload.get("tools").is_none());
    let input = payload["input"].as_array().unwrap();
    assert_eq!(additional_tool_names(input), ["base_tool", "late_tool"]);
    // The incremental notice keeps models from reading a later item as the complete tool set.
    let shape: Vec<&Value> = input.iter().map(|item| item.get("type").unwrap_or(&item["content"])).collect();
    assert_eq!(shape[..2], [&json!("additional_tools"), &json!("base prompt")]);
    assert!(shape[2].is_array());
    assert!(shape[3].as_str().unwrap().contains("incremental tools update"));
    assert_eq!(shape[4..], [&json!("additional_tools"), &json!("updated guidance")]);
}

#[tokio::test]
async fn tool_choice_applies_to_tools_declared_in_additional_tools() {
    let context = Context { tools: vec![tool("ping")], messages: vec![Message::user("x")], ..Context::default() };
    let options = StreamOptions { tool_choice: Some("required".into()), ..options() };
    let payload = capture_payload(&OpenAIResponses, &additional_tools_model(), &context, options).await;
    assert_eq!(payload["tool_choice"], "required");
    assert!(payload.get("tools").is_none());
    assert_eq!(payload["input"][0]["type"], "additional_tools");
    assert_eq!(names(&payload["input"][0]["tools"]), ["ping"]);
    assert_eq!(payload["input"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn maps_additions_into_synthetic_tool_search() {
    let model = model(json!({"supportsMidConvoSystemMessages": true, "supportsToolSearch": true}));
    let payload = capture_payload(&OpenAIResponses, &model, &addition_context(), options()).await;
    assert_eq!(names(&payload["tools"]), ["base_tool"]);
    let input = payload["input"].as_array().unwrap();
    let call = input.iter().find(|item| item["type"] == "tool_search_call").unwrap();
    let output = input.iter().find(|item| item["type"] == "tool_search_output").unwrap();
    assert_eq!(call["call_id"], output["call_id"]);
    assert_eq!(call["arguments"], json!({"query": "late_tool", "limit": 1}));
    assert_eq!(names(&output["tools"]), ["late_tool"]);
    assert_eq!(output["tools"][0]["defer_loading"], true);
}

#[tokio::test]
async fn folds_updates_into_the_leading_message_without_native_support() {
    let model = model(json!({"supportsAdditionalTools": true}));
    let payload = capture_payload(&OpenAIResponses, &model, &changing_context(), options()).await;
    assert_eq!(names(&payload["tools"]), ["late_tool"]);
    let input = payload["input"].as_array().unwrap();
    let kinds: Vec<&str> = input.iter().map(|item| item["type"].as_str().or(item["role"].as_str()).unwrap()).collect();
    assert_eq!(kinds, ["developer", "user"]);
    assert_eq!(input[0]["content"], "base prompt\n\nupdated guidance\n\n<rules>\nnew rules\n</rules>");
}

#[tokio::test]
async fn announces_removals_as_text() {
    let payload = capture_payload(&OpenAIResponses, &additional_tools_model(), &changing_context(), options()).await;
    assert!(payload.get("tools").is_none());
    let input = payload["input"].as_array().unwrap();
    assert_eq!(additional_tool_names(input), ["base_tool", "late_tool"]);
    let update = input.last().unwrap()["content"].as_str().unwrap();
    assert!(update.contains("updated guidance") && update.contains("Removed system prompt section \"docs\"."));
    assert!(update.contains("The following tools are no longer available. Do not call them:\n- base_tool"));
}

#[tokio::test]
async fn redefines_a_tool_with_additional_tools_instead_of_announcing_a_removal() {
    let changed = Tool::new("base_tool", "changed", json!({"type": "object", "properties": {}}));
    let messages = vec![
        system(json!({"role": "system", "content": "base prompt", "toolsAdded": [tool("base_tool")], "timestamp": 0})),
        Message::user("before"),
        system(json!({"role": "system", "content": "", "toolsRemoved": [{"name": "base_tool"}],
            "toolsAdded": [changed], "timestamp": 2})),
    ];
    let context = Context { messages, ..Context::default() };
    let payload = capture_payload(&OpenAIResponses, &additional_tools_model(), &context, options()).await;
    assert!(payload.get("tools").is_none());
    let input = payload["input"].as_array().unwrap();
    assert_eq!(input.len(), 5);
    assert_eq!(input[0]["tools"][0]["description"], "base_tool tool");
    assert_eq!(input[1], json!({"role": "developer", "content": "base prompt"}));
    assert_eq!(input[2]["role"], "user");
    assert!(input[3]["content"].as_str().unwrap().contains("latest definition replaces the earlier one"));
    assert_eq!(
        (input[4]["type"].as_str(), input[4]["tools"][0]["description"].as_str()),
        (Some("additional_tools"), Some("changed"))
    );
}

/// One request after each step: the initial tools, the removal of `drop_tool`, then two loads.
async fn capture_tool_history(model: &crate::types::Model) -> Vec<Value> {
    let steps = [
        vec![Message::user("first")],
        vec![
            system(json!({"role": "system", "content": "", "toolsRemoved": [{"name": "drop_tool"}], "timestamp": 2})),
            Message::user("after removal"),
        ],
        vec![
            system(json!({"role": "system", "content": "", "toolsAdded": [tool("late_tool")], "timestamp": 4})),
            Message::user("after first load"),
        ],
        vec![
            system(json!({"role": "system", "content": "", "toolsAdded": [tool("later_tool")], "timestamp": 6})),
            Message::user("after second load"),
        ],
    ];
    let mut messages = vec![system(json!({"role": "system", "content": "base prompt",
        "toolsAdded": [tool("base_tool"), tool("drop_tool")], "timestamp": 0}))];
    let mut payloads = Vec::new();
    for step in steps {
        messages.extend(step);
        let context = Context { messages: messages.clone(), ..Context::default() };
        payloads.push(capture_payload(&OpenAIResponses, model, &context, options()).await);
    }
    payloads
}

/// Every request's input starts with the previous request's input.
fn assert_growing_prefix(payloads: &[Value]) {
    for pair in payloads.windows(2) {
        let (previous, next) = (pair[0]["input"].as_array().unwrap(), pair[1]["input"].as_array().unwrap());
        assert_eq!(&next[..previous.len()], previous.as_slice());
    }
}

#[tokio::test]
async fn keeps_the_request_prefix_across_a_removal_and_later_loads() {
    let payloads = capture_tool_history(&additional_tools_model()).await;
    assert!(payloads.iter().all(|payload| payload.get("tools").is_none()));
    assert_growing_prefix(&payloads);
    let input = payloads.last().unwrap()["input"].as_array().unwrap();
    let declared: Vec<Vec<String>> =
        input.iter().filter(|item| item["type"] == "additional_tools").map(|item| names(&item["tools"])).collect();
    assert_eq!(declared, [vec!["base_tool", "drop_tool"], vec!["late_tool"], vec!["later_tool"]]);
    assert_eq!(texts_containing(input, "no longer available. Do not call them:\n- drop_tool").len(), 1);
    // Each later load restates the tools that are still unavailable.
    let notices = texts_containing(input, "incremental tools update");
    assert_eq!(notices.len(), 2);
    assert!(notices.iter().all(|notice| notice.contains("\nStill unavailable: drop_tool.")));
}

#[tokio::test]
async fn stops_listing_a_tool_as_unavailable_once_it_is_declared_again() {
    let messages = vec![
        system(json!({"role": "system", "content": "base prompt",
            "toolsAdded": [tool("base_tool"), tool("drop_tool")], "timestamp": 0})),
        system(json!({"role": "system", "content": "", "toolsRemoved": [{"name": "drop_tool"}], "timestamp": 1})),
        system(json!({"role": "system", "content": "", "toolsAdded": [tool("drop_tool")], "timestamp": 2})),
        Message::user("go"),
    ];
    let context = Context { messages, ..Context::default() };
    let payload = capture_payload(&OpenAIResponses, &additional_tools_model(), &context, options()).await;
    let input = payload["input"].as_array().unwrap();
    let notices = texts_containing(input, "incremental tools update");
    assert_eq!(notices.len(), 1);
    assert!(!notices[0].contains("Still unavailable"));
    let before_user = &input[input.len() - 2];
    assert_eq!(before_user["type"], "additional_tools");
    assert_eq!(names(&before_user["tools"]), ["drop_tool"]);
}

#[tokio::test]
async fn keeps_the_tool_search_prefix_across_loads_after_a_removal() {
    let model = model(json!({"supportsMidConvoSystemMessages": true, "supportsToolSearch": true}));
    let payloads = capture_tool_history(&model).await;
    assert_eq!(names(&payloads[0]["tools"]), ["base_tool", "drop_tool"]);
    for payload in &payloads[1..] {
        assert_eq!(names(&payload["tools"]), ["base_tool"]);
    }
    assert_growing_prefix(&payloads[1..]);
    let input = payloads.last().unwrap()["input"].as_array().unwrap();
    let loaded: Vec<String> = input
        .iter()
        .filter(|item| item["type"] == "tool_search_output")
        .map(|item| item["tools"][0]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(loaded, ["late_tool", "later_tool"]);
}

#[tokio::test]
async fn request_options_map_to_body_fields() {
    let mut model = model(json!({"supportsStrictMode": true}));
    model.thinking_level_map =
        Some(BTreeMap::from([("off".to_string(), None), ("high".to_string(), Some("hi".into()))]));
    model.sampling_params = json!({"top_p": 0.9}).as_object().cloned();
    let context = Context { tools: vec![tool("read")], messages: vec![Message::user("x")], ..Context::default() };
    let options = StreamOptions {
        tool_choice: Some("none".into()),
        reasoning: Some(crate::types::ThinkingLevel::High),
        sampling_params: json!({"top_k": 5}).as_object().cloned(),
        session_id: Some("s".repeat(70)),
        ..options()
    };
    let payload = capture_payload(&OpenAIResponses, &model, &context, options).await;
    assert_eq!(payload["tool_choice"], "none");
    assert_eq!(payload["reasoning"], json!({"effort": "hi", "summary": "auto"}));
    assert_eq!((payload["top_p"].clone(), payload["top_k"].clone()), (json!(0.9), json!(5)));
    assert_eq!(payload["tools"][0]["strict"], false);
    assert_eq!(payload["prompt_cache_key"].as_str().unwrap().len(), 64);
    let off = capture_payload(&OpenAIResponses, &model, &context, self::options()).await;
    assert!(off.get("reasoning").is_none(), "off: null omits reasoning");
}

#[tokio::test]
async fn cache_retention_controls_prompt_cache_fields() {
    let explicit = model(json!({"supportsExplicitPromptCacheMode": true}));
    let context = Context { messages: vec![Message::user("x")], ..Context::default() };
    let with =
        |retention| StreamOptions { cache_retention: Some(retention), session_id: Some("s1".into()), ..options() };
    let none = capture_payload(&OpenAIResponses, &explicit, &context, with(CacheRetention::None)).await;
    assert!(none.get("prompt_cache_key").is_none());
    assert_eq!(none["prompt_cache_options"], json!({"mode": "explicit"}));
    let long = capture_payload(&OpenAIResponses, &explicit, &context, with(CacheRetention::Long)).await;
    assert_eq!(
        (long["prompt_cache_options"].clone(), long.get("prompt_cache_retention")),
        (json!({"ttl": "30m"}), None)
    );
    let plain = model(json!({}));
    let long = capture_payload(&OpenAIResponses, &plain, &context, with(CacheRetention::Long)).await;
    assert_eq!((long["prompt_cache_retention"].as_str(), long["prompt_cache_key"].as_str()), (Some("24h"), Some("s1")));
    let unsupported = model(json!({"supportsLongCacheRetention": false}));
    let long = capture_payload(&OpenAIResponses, &unsupported, &context, with(CacheRetention::Long)).await;
    assert!(long.get("prompt_cache_retention").is_none());
}

fn completed_with(status: &str, reason: Option<&str>) -> String {
    let mut response = json!({"id": "resp_1", "status": status,
        "usage": {"input_tokens": 20, "output_tokens": 7, "total_tokens": 27,
            "input_tokens_details": {"cached_tokens": 2, "cache_write_tokens": 3}}});
    if let Some(reason) = reason {
        response["incomplete_details"] = json!({"reason": reason});
    }
    let kind = if status == "completed" { "response.completed" } else { "response.incomplete" };
    frame(None, &json!({"type": kind, "response": response}))
}

#[tokio::test]
async fn terminal_events_set_raw_stop_reasons() {
    for (status, reason, stop, raw, error) in [
        ("completed", None, StopReason::Stop, "completed", None),
        ("incomplete", Some("max_output_tokens"), StopReason::Length, "incomplete.max_output_tokens", None),
        (
            "incomplete",
            Some("content_filter"),
            StopReason::Error,
            "incomplete.content_filter",
            Some("Response incomplete: content_filter"),
        ),
    ] {
        let server = MockServer::start(MockResponse::Sse(vec![completed_with(status, reason)])).await;
        let model = test_model(API, "openai", &server.base_url);
        let context = Context { messages: vec![Message::user("x")], ..Context::default() };
        let (_, message) = collect(OpenAIResponses.stream(&model, &context, options())).await;
        assert_eq!((message.stop_reason, message.raw_stop_reason.as_deref()), (stop, Some(raw)));
        assert_eq!(message.error_message.as_deref(), error);
        assert_eq!((message.usage.input, message.usage.cache_read, message.usage.cache_write), (15, 2, 3));
    }
    let failed = json!({"type": "response.failed", "response": {"status": "failed",
        "error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "Usage limit reached."}}});
    let server = MockServer::start(MockResponse::Sse(vec![frame(None, &failed)])).await;
    let model = test_model(API, "openai", &server.base_url);
    let context = Context { messages: vec![Message::user("x")], ..Context::default() };
    let (_, message) = collect(OpenAIResponses.stream(&model, &context, options())).await;
    assert_eq!(message.raw_stop_reason.as_deref(), Some("failed"));
    assert!(message.error_message.unwrap().ends_with("Check your ChatGPT usage: https://chatgpt.com/settings/usage"));
}

#[tokio::test]
async fn namespaces_and_grammar_tool_calls_round_trip() {
    let item = |done: bool| {
        let mut item = json!({"type": "function_call", "id": "fc_test", "call_id": "call_test", "name": "lookup",
            "arguments": if done { "{\"value\":\"hello\"}" } else { "" }});
        if done {
            item["namespace"] = json!("dynamic_tools");
        }
        item
    };
    let custom =
        json!({"type": "custom_tool_call", "id": "ctc_test", "call_id": "call_custom", "name": "query", "input": ""});
    let frames = vec![
        frame(None, &json!({"type": "response.output_item.added", "output_index": 0, "item": item(false)})),
        frame(None, &json!({"type": "response.output_item.done", "output_index": 0, "item": item(true)})),
        frame(None, &json!({"type": "response.output_item.added", "output_index": 1, "item": custom})),
        frame(None, &json!({"type": "response.custom_tool_call_input.delta", "output_index": 1, "delta": "hel\"lo"})),
        frame(
            None,
            &json!({"type": "response.output_item.done", "output_index": 1,
            "item": {"type": "custom_tool_call", "id": "ctc_test", "call_id": "call_custom", "name": "query",
                "input": "hel\"lo", "namespace": "dynamic_tools"}}),
        ),
        frame(None, &json!({"type": "response.completed", "response": {"id": "resp", "status": "completed"}})),
    ];
    let server = MockServer::start(MockResponse::Sse(frames)).await;
    let model = test_model(API, "openai", &server.base_url);
    let context = Context { messages: vec![Message::user("x")], ..Context::default() };
    let (_, message) = collect(OpenAIResponses.stream(&model, &context, options())).await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let calls: Vec<_> = message.tool_calls().cloned().collect();
    assert_eq!((calls[0].id.as_str(), calls[0].namespace.as_deref()), ("call_test|fc_test", Some("dynamic_tools")));
    assert_eq!(calls[1].arguments["input"], "hel\"lo");
    let grammar = ConvertOptions {
        grammar_tool_input_properties: BTreeMap::from([("query".to_string(), "input".to_string())]),
        ..ConvertOptions::default()
    };
    let history =
        normalize_context(&Context { messages: vec![Message::Assistant(message.clone())], ..Context::default() });
    let replayed = convert_messages(&model, &history, &grammar).unwrap();
    assert_eq!(
        replayed[0],
        json!({"type": "function_call", "call_id": "call_test", "name": "lookup", "id": "fc_test",
        "arguments": "{\"value\":\"hello\"}", "namespace": "dynamic_tools"})
    );
    assert_eq!(
        replayed[1],
        json!({"type": "custom_tool_call", "call_id": "call_custom", "name": "query",
        "input": "hel\"lo", "id": "ctc_test", "namespace": "dynamic_tools"})
    );
    let other = crate::types::Model { id: "other".into(), ..model.clone() };
    let replayed = convert_messages(&other, &history, &ConvertOptions::default()).unwrap();
    assert!(replayed[0].get("namespace").is_none() && replayed[0].get("id").is_none());
}

fn constrained(name: &str, config: Value) -> Tool {
    let parameters = json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]});
    let mut tool = Tool::new(name, format!("{name} tool"), parameters);
    tool.constrained_sampling = Some(config);
    tool
}

#[tokio::test]
async fn grammar_and_strict_tools_use_constrained_sampling() {
    let model = model(json!({"supportsOpenAIGrammarTools": true, "supportsStrictMode": true}));
    let grammar = constrained("query", json!({"type": "grammar", "variants": {"openai_lark": "start: /.+/"}}));
    let strict = constrained("lookup", json!({"type": "json_schema", "strict": "prefer"}));
    let context = Context {
        tools: vec![grammar, strict, tool("plain")],
        messages: vec![Message::user("x")],
        ..Context::default()
    };
    let payload = capture_payload(&OpenAIResponses, &model, &context, options()).await;
    let tools = payload["tools"].as_array().unwrap();
    assert_eq!(
        tools[0],
        json!({"type": "custom", "name": "query", "description": "query tool",
        "format": {"type": "grammar", "syntax": "lark", "definition": "start: /.+/"}})
    );
    assert_eq!(
        (tools[1]["strict"].clone(), tools[1]["parameters"]["additionalProperties"].clone()),
        (json!(true), json!(false))
    );
    assert_eq!(tools[2]["strict"], false);
    let required = constrained("lookup", json!({"type": "json_schema", "strict": "require"}));
    let context = Context { tools: vec![required], messages: vec![Message::user("x")], ..Context::default() };
    let (_, message) = collect(OpenAIResponses.stream(&self::model(json!({})), &context, options())).await;
    let expected = "Tool \"lookup\" requires JSON-schema constrained sampling, but strict tools are unsupported.";
    assert_eq!(message.error_message.as_deref(), Some(expected));
}

#[tokio::test]
async fn grammar_tool_calls_stream_into_their_input_property() {
    let parameters = json!({"type": "object", "properties": {"sql": {"type": "string"}}, "required": ["sql"]});
    let mut grammar = Tool::new("query", "query tool", parameters);
    grammar.constrained_sampling = Some(json!({"type": "grammar", "variants": {"openai_regex": "SELECT .+"}}));
    let item = json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "query", "input": ""});
    let frames = vec![
        frame(None, &json!({"type": "response.output_item.added", "output_index": 0, "item": item})),
        frame(None, &json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "delta": "SELECT 1"})),
        frame(
            None,
            &json!({"type": "response.output_item.done", "output_index": 0,
            "item": {"type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1", "name": "query", "input": "SELECT 1"}}),
        ),
        frame(None, &json!({"type": "response.completed", "response": {"id": "resp", "status": "completed"}})),
    ];
    let mut server = MockServer::start(MockResponse::Sse(frames)).await;
    let mut model = test_model(API, "github-copilot", &server.base_url);
    model.compat = Some(json!({"supportsOpenAIGrammarTools": true}));
    let context = Context { tools: vec![grammar], messages: vec![Message::user("x")], ..Context::default() };
    let (_, message) = collect(OpenAIResponses.stream(&model, &context, options())).await;
    let call = message.tool_calls().next().unwrap();
    assert_eq!(Value::Object(call.arguments.clone()), json!({"sql": "SELECT 1"}));
    let request = server.request().await;
    assert_eq!(request.body["tools"][0]["format"]["syntax"], "regex");
    assert_eq!(
        (request.headers["x-initiator"].as_str(), request.headers["openai-intent"].as_str()),
        ("user", "conversation-edits")
    );
}

#[tokio::test]
async fn service_tiers_are_sent_and_priced() {
    for (requested, reported, multiplier) in
        [("flex", None, 0.5), ("auto", Some("priority"), 2.0), ("default", None, 1.0)]
    {
        let mut response = json!({"id": "r", "status": "completed",
            "usage": {"input_tokens": 1000, "output_tokens": 1000, "total_tokens": 2000}});
        if let Some(tier) = reported {
            response["service_tier"] = json!(tier);
        }
        let frames = vec![frame(None, &json!({"type": "response.completed", "response": response}))];
        let mut server = MockServer::start(MockResponse::Sse(frames)).await;
        let model = test_model(API, "openai", &server.base_url);
        let context = Context { messages: vec![Message::user("x")], ..Context::default() };
        let options = StreamOptions { service_tier: Some(requested.into()), ..options() };
        let (_, message) = collect(OpenAIResponses.stream(&model, &context, options)).await;
        let expected = (1000.0 * 1.0 + 1000.0 * 2.0) / 1e6 * multiplier;
        assert!((message.usage.cost.total - expected).abs() < 1e-12, "{requested}: {}", message.usage.cost.total);
        assert_eq!(server.request().await.body["service_tier"], requested);
    }
}

#[tokio::test]
async fn mid_convo_effort_sends_levels_as_configuration_updates() {
    let model = model(json!({"supportsMidConvoEffort": true}));
    let reply = |level: Option<&str>| {
        Message::Assistant(crate::types::AssistantMessage {
            content: vec![crate::types::ContentBlock::text("a")],
            api: API.into(),
            provider: "openai".into(),
            model: "test-model".into(),
            provider_thinking_level: level.map(str::to_string),
            stop_reason: StopReason::Stop,
            ..Default::default()
        })
    };
    let messages =
        vec![Message::user("one"), reply(Some("low")), Message::user("two"), reply(None), Message::user("three")];
    let context = Context { messages, ..Context::default() };
    let high = StreamOptions { reasoning: Some(crate::types::ThinkingLevel::High), ..options() };
    let payload = capture_payload(&OpenAIResponses, &model, &context, high.clone()).await;
    assert_eq!(payload["reasoning"]["effort"], "medium", "the request effort stays fixed");
    let input = payload["input"].as_array().unwrap();
    let updates: Vec<(usize, &Value)> = input
        .iter()
        .enumerate()
        .filter(|(_, item)| item["type"] == "configuration_update")
        .map(|(index, item)| (index, &item["reasoning"]["effort"]))
        .collect();
    assert_eq!(updates, [(1, &json!("low")), (input.len() - 1, &json!("high"))]);

    let mut server = MockServer::start(MockResponse::Sse(vec![frame(
        None,
        &json!({"type": "response.completed",
        "response": {"status": "completed", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    )]))
    .await;
    let mut live = model.clone();
    live.base_url = server.base_url.clone();
    let (_, message) = collect(OpenAIResponses.stream(&live, &context, high.clone())).await;
    assert_eq!(message.provider_thinking_level.as_deref(), Some("high"));
    server.request().await;

    let sign_in = StreamOptions { api_key: Some("chatgpt-token".into()), ..high };
    let mut openai = model.clone();
    openai.base_url = "https://api.openai.com/v1".into();
    let payload = capture_payload(&OpenAIResponses, &openai, &context, sign_in).await;
    assert_eq!(payload["reasoning"]["effort"], "high");
    assert!(payload["input"].as_array().unwrap().iter().all(|item| item["type"] != "configuration_update"));
}
