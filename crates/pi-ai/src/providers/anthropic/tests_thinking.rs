//! Thinking, effort, fallback, and stream parsing behavior, ported from upstream
//! `anthropic-mid-conversation-effort`, `anthropic-thinking-disable`, `anthropic-force-adaptive-thinking`,
//! `anthropic-temperature-compat`, `anthropic-cache-write-1h-cost`, and `anthropic-sse-parsing` tests.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::AnthropicMessages;
use super::tests::{API, block_start, block_stop, delta, event, message_end, message_start, options};
use crate::providers::mock_server::{MockResponse, MockServer, capture_payload, collect, test_model};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions, ThinkingBudgets};
use crate::types::{AssistantMessage, ContentBlock, Context, Message, Model, StopReason, ThinkingLevel};

const UNREACHABLE: &str = "http://127.0.0.1:9";

fn managed() -> Model {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    model.thinking_level_map = Some(BTreeMap::from(
        [("off", None), ("minimal", Some("low")), ("low", Some("low")), ("medium", Some("medium"))]
            .map(|(level, value)| (level.to_string(), value.map(str::to_string))),
    ));
    model.thinking_level_map.as_mut().unwrap().insert("high".into(), Some("high".into()));
    model.thinking_level_map.as_mut().unwrap().insert("max".into(), Some("max".into()));
    model.compat = Some(json!({"forceAdaptiveThinking": true, "supportsMidConvoEffort": true}));
    model
}

fn reasoning(level: Option<ThinkingLevel>) -> StreamOptions {
    StreamOptions { reasoning: level, cache_retention: Some(CacheRetention::None), ..options() }
}

fn context(messages: Vec<Message>) -> Context {
    Context { messages, ..Context::default() }
}

fn assistant(level: Option<&str>, provider: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: "reasoning".into(),
                thinking_signature: Some("signature".into()),
                redacted: false,
            },
            ContentBlock::text("answer"),
        ],
        api: API.into(),
        provider: provider.into(),
        model: "test-model".into(),
        provider_thinking_level: level.map(str::to_string),
        stop_reason: StopReason::Stop,
        timestamp: 1,
        ..Default::default()
    })
}

async fn payload(model: &Model, messages: Vec<Message>, options: StreamOptions) -> Value {
    capture_payload(&AnthropicMessages, model, &context(messages), options).await
}

fn markers(payload: &Value) -> Vec<Value> {
    payload["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "system").cloned().collect()
}

fn marker(effort: &str) -> Value {
    json!({"role": "system", "content": [], "output_config": {"effort": effort}})
}

#[tokio::test]
async fn managed_effort_replays_markers_and_appends_the_active_one() {
    let first = payload(&managed(), vec![Message::user("one")], reasoning(Some(ThinkingLevel::Low))).await;
    assert_eq!(first["messages"], json!([{"role": "user", "content": "one"}, marker("low")]));
    let history = vec![Message::user("one"), assistant(Some("low"), "anthropic"), Message::user("two")];
    let second = payload(&managed(), history, reasoning(Some(ThinkingLevel::High))).await;
    assert_eq!(second["messages"].as_array().unwrap()[..2], first["messages"].as_array().unwrap()[..]);
    assert_eq!(second["messages"].as_array().unwrap().last().unwrap(), &marker("high"));
    assert_eq!(
        (&first["output_config"], &second["output_config"]),
        (&json!({"effort": "high"}), &json!({"effort": "high"}))
    );
    let binding = json!({"type": "adaptive", "display": "summarized", "block_binding": {"prefix_mismatch_behavior": "drop_block"}});
    assert_eq!(second["thinking"], binding);
    let legacy = vec![
        Message::user("one"),
        assistant(None, "anthropic"),
        Message::user("two"),
        assistant(Some("low"), "other-provider"),
        Message::user("three"),
    ];
    let payload = payload(&managed(), legacy, reasoning(Some(ThinkingLevel::Medium))).await;
    assert_eq!(markers(&payload), [marker("medium")]);
}

#[tokio::test]
async fn managed_effort_sets_the_level_and_betas() {
    let mut frames = vec![message_start(json!({"input_tokens": 1}))];
    frames.extend(message_end("end_turn", json!({"output_tokens": 1})));
    let mut server = MockServer::start(MockResponse::Sse(frames)).await;
    let mut model = managed();
    model.base_url = server.base_url.clone();
    let (_, message) =
        collect(AnthropicMessages.stream(&model, &context(vec![Message::user("one")]), reasoning(None))).await;
    assert_eq!((message.stop_reason, message.provider_thinking_level.as_deref()), (StopReason::Stop, Some("high")));
    let betas = server.request().await.headers["anthropic-beta"].clone();
    assert!(betas.contains("mid-conversation-output-config-2026-07-01"));
    assert!(betas.contains("thinking-binding-controls-2026-08-01"));
    model.compat = Some(json!({"forceAdaptiveThinking": true}));
    let body = payload(&model, vec![Message::user("one")], reasoning(Some(ThinkingLevel::Low))).await;
    assert_eq!(body["messages"], json!([{"role": "user", "content": "one"}]));
    assert_eq!(body["output_config"], json!({"effort": "low"}));
    assert_eq!(body["thinking"], json!({"type": "adaptive", "display": "summarized"}));
}

#[tokio::test]
async fn thinking_off_disables_unless_the_map_marks_off_unsupported() {
    let budget = test_model(API, "anthropic", UNREACHABLE);
    let off = payload(&budget, vec![Message::user("x")], reasoning(Some(ThinkingLevel::Off))).await;
    assert_eq!((off["thinking"].clone(), off.get("output_config")), (json!({"type": "disabled"}), None));
    let mut adaptive = budget.clone();
    adaptive.compat = Some(json!({"forceAdaptiveThinking": true}));
    let off = payload(&adaptive, vec![Message::user("x")], reasoning(None)).await;
    assert_eq!((off["thinking"].clone(), off.get("output_config")), (json!({"type": "disabled"}), None));
    adaptive.thinking_level_map =
        Some(BTreeMap::from([("off".to_string(), None), ("xhigh".into(), Some("xhigh".into()))]));
    let off = payload(&adaptive, vec![Message::user("x")], reasoning(None)).await;
    assert!(off.get("thinking").is_none() && off.get("output_config").is_none());
    let xhigh = payload(&adaptive, vec![Message::user("x")], reasoning(Some(ThinkingLevel::Xhigh))).await;
    assert_eq!(xhigh["output_config"], json!({"effort": "xhigh"}));
    let max = payload(&adaptive, vec![Message::user("x")], reasoning(Some(ThinkingLevel::Max))).await;
    assert_eq!(max["output_config"], json!({"effort": "high"}), "unmapped levels fall back to high");
}

#[tokio::test]
async fn budget_thinking_uses_custom_budgets_and_leaves_answer_room() {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    model.max_tokens = 32_000;
    let budgets = ThinkingBudgets { low: Some(5000), ..Default::default() };
    let options = StreamOptions {
        max_tokens: Some(2000),
        thinking_budgets: Some(budgets),
        ..reasoning(Some(ThinkingLevel::Low))
    };
    let body = payload(&model, vec![Message::user("x")], options).await;
    assert_eq!(body["max_tokens"], 7000);
    assert_eq!(body["thinking"], json!({"type": "enabled", "budget_tokens": 5000, "display": "summarized"}));
    model.context_window = 10_000;
    let body = payload(&model, vec![Message::user("x")], reasoning(Some(ThinkingLevel::High))).await;
    assert_eq!(body["max_tokens"], 5903);
    assert_eq!(body["thinking"]["budget_tokens"], 4879);
}

#[tokio::test]
async fn temperature_needs_support_and_no_thinking() {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    let with = |level| StreamOptions { temperature: Some(0.0), ..reasoning(level) };
    assert_eq!(payload(&model, vec![Message::user("x")], with(None)).await["temperature"], 0.0);
    assert!(
        payload(&model, vec![Message::user("x")], with(Some(ThinkingLevel::Low))).await.get("temperature").is_none()
    );
    model.compat = Some(json!({"supportsTemperature": false}));
    assert!(payload(&model, vec![Message::user("x")], with(None)).await.get("temperature").is_none());
    assert!(payload(&managed(), vec![Message::user("x")], with(None)).await.get("temperature").is_none());
}

async fn stream_frames(
    model: &mut Model,
    frames: Vec<String>,
    options: StreamOptions,
) -> (Vec<String>, AssistantMessage) {
    let mut server = MockServer::start(MockResponse::Sse(frames)).await;
    model.base_url = server.base_url.clone();
    let (_, message) = collect(AnthropicMessages.stream(model, &context(vec![Message::user("hi")]), options)).await;
    let betas = server.request().await.headers.get("anthropic-beta").cloned().unwrap_or_default();
    (betas.split(',').map(str::to_string).collect(), message)
}

#[tokio::test]
async fn long_cache_writes_cost_twice_the_input_rate() {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    model.cost.input = 5.0;
    model.cost.cache_write = 6.25;
    let start = |breakdown: Option<Value>| {
        let mut usage = json!({"input_tokens": 100, "output_tokens": 0, "cache_creation_input_tokens": 1_000_000});
        if let Some(breakdown) = breakdown {
            usage["cache_creation"] = breakdown;
        }
        message_start(usage)
    };
    let end = |usage: Value| message_end("end_turn", usage);
    let full = json!({"input_tokens": 100, "output_tokens": 5, "cache_creation_input_tokens": 1_000_000});
    let mut frames =
        vec![start(Some(json!({"ephemeral_5m_input_tokens": 600_000, "ephemeral_1h_input_tokens": 400_000})))];
    frames.extend(end(full.clone()));
    let (_, message) = stream_frames(&mut model, frames, options()).await;
    assert_eq!((message.usage.cache_write, message.usage.cache_write_1h), (1_000_000, Some(400_000)));
    assert!((message.usage.cost.cache_write - 7.75).abs() < 1e-9);
    let mut frames = vec![start(None)];
    frames.extend(end(
        json!({"cache_creation_input_tokens": 6535, "cache_creation": {"ephemeral_1h_input_tokens": 6535}}),
    ));
    let (_, message) = stream_frames(&mut model, frames, options()).await;
    assert_eq!((message.usage.cache_write, message.usage.cache_write_1h), (6535, Some(6535)));
    assert!((message.usage.cost.cache_write - 6535.0 * 10.0 / 1e6).abs() < 1e-12);
    let mut frames = vec![start(None)];
    frames.extend(end(full));
    let (_, message) = stream_frames(&mut model, frames, options()).await;
    assert_eq!(message.usage.cache_write_1h, Some(0));
    assert!((message.usage.cost.cache_write - 6.25).abs() < 1e-9);
}

#[tokio::test]
async fn fallback_models_price_responses_and_reject_late_fallbacks() {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    model.compat = Some(json!({"allowedFallbackModels": [{"provider": "anthropic", "model": "fallback-model",
        "cost": {"input": 3, "output": 5, "cacheRead": 0, "cacheWrite": 0}}]}));
    let body = payload(&model, vec![Message::user("x")], options()).await;
    assert_eq!(body["fallbacks"], json!([{"model": "fallback-model"}]));
    let mut frames = vec![event(json!({"type": "message_start",
        "message": {"id": "m", "model": "fallback-model", "usage": {"input_tokens": 100, "output_tokens": 0}}}))];
    frames.extend(message_end("end_turn", json!({"output_tokens": 20})));
    let (betas, message) = stream_frames(&mut model, frames, options()).await;
    assert!(betas.contains(&"server-side-fallback-2026-07-01".to_string()));
    assert_eq!((message.model.as_str(), message.response_model.as_deref()), ("test-model", Some("fallback-model")));
    assert!((message.usage.cost.input - 0.0003).abs() < 1e-12 && (message.usage.cost.output - 0.0001).abs() < 1e-12);
    let frames = vec![
        message_start(json!({"input_tokens": 1})),
        block_start(0, json!({"type": "text", "text": "partial"})),
        block_stop(0),
        block_start(1, json!({"type": "fallback", "from": {"model": "a"}, "to": {"model": "b"}})),
    ];
    let (_, message) = stream_frames(&mut model, frames, options()).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message.error_message.unwrap().contains("unsupported mid-output model fallback"));
}

#[tokio::test]
async fn stream_parsing_repairs_json_and_reports_transformations() {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    model.reasoning = false;
    let malformed = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1	col2\"}"}}"#;
    let mut frames = vec![
        event(json!({"type": "message_start", "message": {"id": "m", "usage": {"input_tokens": 12},
            "input_transformations": [{"type": "thinking_dropped", "path": "messages.1", "reason": "prefix"}]}})),
        event(json!({"type": "ping"})),
        block_start(0, json!({"type": "tool_use", "id": "toolu_test", "name": "edit", "input": {}})),
        format!("event: content_block_delta\ndata: {malformed}\n\n"),
        block_stop(0),
        event(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 5},
            "input_transformations": [{"type": "thinking_dropped", "path": "messages.3", "reason": null}]})),
        event(json!({"type": "message_stop"})),
    ];
    frames.push(event(json!({"type": "unknown_event"})));
    let (betas, message) = stream_frames(&mut model, frames, options()).await;
    assert!(!betas.contains(&"interleaved-thinking-2025-05-14".to_string()));
    assert_eq!((message.stop_reason, message.error_message.as_deref()), (StopReason::ToolUse, None));
    let call = message.tool_calls().next().unwrap();
    assert_eq!(Value::Object(call.arguments.clone()), json!({"path": "A\\H", "text": "col1\tcol2"}));
    assert_eq!(message.diagnostics.len(), 1);
    assert_eq!(message.diagnostics[0]["type"], "anthropic_input_transformations");
    let details = json!({"transformations": [{"type": "thinking_dropped", "path": "messages.3"}]});
    assert_eq!(message.diagnostics[0]["details"], details);
}

#[tokio::test]
async fn stop_reasons_keep_raw_values_and_explanations() {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    for (reason, details, error) in [
        ("refusal", json!({"explanation": "Not allowed."}), "Not allowed."),
        ("refusal", Value::Null, "The model refused to complete the request"),
        ("sensitive", Value::Null, "Provider stopped with: sensitive"),
        ("brand_new", Value::Null, "Unhandled stop reason: brand_new"),
    ] {
        let frames = vec![
            message_start(json!({"input_tokens": 1})),
            event(json!({"type": "message_delta", "delta": {"stop_reason": reason, "stop_details": details}})),
            event(json!({"type": "message_stop"})),
        ];
        let (_, message) = stream_frames(&mut model, frames, options()).await;
        assert_eq!((message.stop_reason, message.error_message.as_deref()), (StopReason::Error, Some(error)));
        assert_eq!(message.raw_stop_reason.as_deref(), Some(reason));
        assert_eq!(message.usage.input, 1, "message_delta without usage keeps the start usage");
    }
}

#[tokio::test]
async fn payload_replacements_keep_streaming_and_error_events_fail() {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    let mut frames =
        vec![message_start(json!({"input_tokens": 1})), delta(9, json!({"type": "text_delta", "text": "x"}))];
    frames.extend(message_end("end_turn", json!({})));
    let mut server = MockServer::start(MockResponse::Sse(frames)).await;
    model.base_url = server.base_url.clone();
    let options = StreamOptions {
        on_payload: Some(crate::registry::payload_hook(|payload: &Value, _: &Model| {
            let mut next = payload.clone();
            next["stream"] = json!(false);
            Some(next)
        })),
        ..options()
    };
    let (_, message) = collect(AnthropicMessages.stream(&model, &context(vec![Message::user("x")]), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(server.request().await.body["stream"], true);
    let error = "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"Overloaded\"}}\n\n".to_string();
    let (_, message) = stream_frames(&mut model, vec![message_start(json!({})), error], self::options()).await;
    assert_eq!(message.error_message.as_deref(), Some("{\"type\":\"error\",\"error\":{\"message\":\"Overloaded\"}}"));
}
