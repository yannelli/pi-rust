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

#[tokio::test]
async fn anchors_additions_at_their_developer_message() {
    let model = model(json!({"supportsMidConvoSystemMessages": true, "supportsAdditionalTools": true}));
    let payload = capture_payload(&OpenAIResponses, &model, &addition_context(), options()).await;
    assert_eq!(names(&payload["tools"]), ["base_tool"]);
    let input = payload["input"].as_array().unwrap();
    let additional = input.iter().find(|item| item["type"] == "additional_tools").unwrap();
    assert_eq!(names(&additional["tools"]), ["late_tool"]);
    let developer: Vec<&Value> = input
        .iter()
        .filter(|item| item["role"] == "developer" && item["type"].is_null())
        .map(|item| &item["content"])
        .collect();
    assert_eq!(developer, [&json!("base prompt"), &json!("updated guidance")]);
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
async fn removals_fall_back_to_the_complete_current_tool_state() {
    let model = model(json!({"supportsMidConvoSystemMessages": true, "supportsAdditionalTools": true}));
    let payload = capture_payload(&OpenAIResponses, &model, &changing_context(), options()).await;
    assert_eq!(names(&payload["tools"]), ["late_tool"]);
    let input = payload["input"].as_array().unwrap();
    assert!(!input.iter().any(|item| item["type"] == "additional_tools"));
    assert_eq!(input.iter().filter(|item| item["role"] == "developer").count(), 2);
    let update = input.last().unwrap()["content"].as_str().unwrap();
    assert!(update.contains("updated guidance") && update.contains("Removed system prompt section \"docs\"."));
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
    let replayed = convert_messages(&model, &history, &grammar);
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
    let replayed = convert_messages(&other, &history, &ConvertOptions::default());
    assert!(replayed[0].get("namespace").is_none() && replayed[0].get("id").is_none());
}
