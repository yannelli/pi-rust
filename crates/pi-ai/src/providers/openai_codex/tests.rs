use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};

use super::*;
use crate::providers::http_mock::{Recorded, Reply, RouteServer};
use crate::providers::mock_server::{cancel_after_first_text, collect, kinds};
use crate::types::{ModelCost, ThinkingLevel, Tool};

pub(super) fn mock_token(account: &str) -> String {
    let claims = json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}});
    format!("aaa.{}.bbb", base64::engine::general_purpose::STANDARD.encode(claims.to_string()))
}

pub(super) fn codex_model(base_url: &str) -> Model {
    Model {
        id: "gpt-5.1-codex".into(),
        name: "GPT-5.1 Codex".into(),
        api: "openai-codex-responses".into(),
        provider: "openai-codex".into(),
        base_url: base_url.into(),
        reasoning: true,
        input: vec!["text".into()],
        cost: ModelCost { input: 1.0, output: 2.0, ..ModelCost::default() },
        context_window: 400_000,
        max_tokens: 128_000,
        ..Model::default()
    }
}

pub(super) fn context() -> Context {
    Context {
        system_prompt: Some("You are a helpful assistant.".into()),
        messages: vec![Message::user("Say hello")],
        tools: Vec::new(),
    }
}

/// Upstream `buildSSEPayload` events, without `output_index` like the Codex backend.
pub(super) fn events(terminal: &str, status: &str, end_turn: Option<bool>) -> Vec<Value> {
    let mut response = json!({"status": status, "usage": {"input_tokens": 5, "output_tokens": 3, "total_tokens": 8, "input_tokens_details": {"cached_tokens": 0}}});
    if let Some(end_turn) = end_turn {
        response["end_turn"] = json!(end_turn);
    }
    if status == "incomplete" {
        response["incomplete_details"] = json!({"reason": "max_output_tokens"});
    }
    vec![
        json!({"type": "response.output_item.added", "item": {"type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": []}}),
        json!({"type": "response.content_part.added", "part": {"type": "output_text", "text": ""}}),
        json!({"type": "response.output_text.delta", "delta": "Hello"}),
        json!({"type": "response.output_item.done", "item": {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "Hello"}]}}),
        json!({"type": terminal, "response": response}),
    ]
}

pub(super) fn sse_body(events: &[Value]) -> String {
    events.iter().map(|event| format!("data: {event}\n\n")).collect()
}

pub(super) fn sse_reply(events: &[Value]) -> Reply {
    Reply::new(200, "text/event-stream", sse_body(events))
}

pub(super) fn decoded_body(request: &Recorded) -> Value {
    let mut decoded = Vec::new();
    ruzstd::decoding::StreamingDecoder::new(&request.body[..]).unwrap().read_to_end(&mut decoded).unwrap();
    serde_json::from_slice(&decoded).unwrap()
}

fn sse_options(token: &str) -> StreamOptions {
    StreamOptions { api_key: Some(token.into()), transport: Some(Transport::Sse), ..StreamOptions::default() }
}

#[tokio::test]
async fn streams_sse_with_account_headers_zstd_bodies_and_raw_events() {
    let payload = events("response.completed", "completed", None);
    let server = RouteServer::start(move |_| sse_reply(&payload)).await;
    let token = mock_token("acc_test");
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorder = seen.clone();
    let options = StreamOptions {
        on_provider_stream_event: Some(Arc::new(move |event, _| {
            recorder.lock().unwrap().push(event["type"].as_str().unwrap_or_default().to_string());
        })),
        ..sse_options(&token)
    };
    let (events, message) =
        collect(OpenAICodexResponses.stream(&codex_model(&server.base_url), &context(), options)).await;
    assert_eq!(kinds(&events), ["start", "text_start", "text_delta", "text_end", "done"]);
    assert_eq!((message.text(), message.stop_reason), ("Hello".to_string(), StopReason::Stop));
    assert_eq!((message.usage.input, message.usage.output, message.usage.total_tokens), (5, 3, 8));
    let expected = ["response.output_item.added", "response.content_part.added", "response.output_text.delta"];
    assert_eq!(seen.lock().unwrap()[..3], expected);
    assert_eq!(seen.lock().unwrap()[4], "response.completed");

    let request = &server.requests()[0];
    assert_eq!(request.path, "/codex/responses");
    let headers = &request.headers;
    assert_eq!(headers["authorization"], format!("Bearer {token}"));
    assert_eq!(headers["chatgpt-account-id"], "acc_test");
    assert_eq!(headers["openai-beta"], "responses=experimental");
    assert_eq!((headers["originator"].as_str(), headers["accept"].as_str()), ("pi", "text/event-stream"));
    assert!(headers["user-agent"].starts_with("pi ("), "{}", headers["user-agent"]);
    assert_eq!(headers["content-encoding"], "zstd");
    assert!(!headers.contains_key("x-api-key") && !headers.contains_key("session-id"));
    let body = decoded_body(request);
    assert_eq!(body["instructions"], "You are a helpful assistant.");
    assert_eq!(body["input"], json!([{"role": "user", "content": [{"type": "input_text", "text": "Say hello"}]}]));
    assert_eq!((body["store"].clone(), body["tool_choice"].clone()), (json!(false), json!("auto")));
    assert_eq!(body["text"], json!({"verbosity": "low"}));
    assert_eq!(body["reasoning"], json!({"effort": "none"}));
}

#[tokio::test]
async fn completes_on_terminal_events_even_when_the_body_stays_open() {
    let payload = events("response.done", "completed", Some(false));
    let server = RouteServer::start(move |_| {
        let mut body = sse_body(&payload);
        body.push_str("data: [DONE]\n\n");
        Reply { hang: true, ..Reply::new(200, "text/event-stream", body) }
    })
    .await;
    let model = codex_model(&server.base_url);
    let stream = OpenAICodexResponses.stream(&model, &context(), sse_options(&mock_token("acc")));
    let message = tokio::time::timeout(std::time::Duration::from_secs(2), stream.result(&model)).await.unwrap();
    assert_eq!(
        (message.text(), message.stop_reason, message.end_turn),
        ("Hello".to_string(), StopReason::Stop, Some(false))
    );

    let payload = events("response.incomplete", "incomplete", None);
    let trimmed = sse_body(&payload).trim_end().to_string();
    let server = RouteServer::start(move |_| Reply::new(200, "text/event-stream", trimmed.clone())).await;
    let model = codex_model(&server.base_url);
    let message = OpenAICodexResponses.stream(&model, &context(), sse_options(&mock_token("acc"))).result(&model).await;
    assert_eq!(message.stop_reason, StopReason::Length);
}

#[tokio::test]
async fn sends_session_affinity_reasoning_tools_and_header_overrides() {
    let payload = events("response.completed", "completed", None);
    let server = RouteServer::start(move |_| sse_reply(&payload)).await;
    let session = "s".repeat(80);
    let mut model = codex_model(&server.base_url);
    model.id = "gpt-5.5".into();
    model.thinking_level_map = Some([("xhigh".to_string(), Some("xhigh".to_string()))].into_iter().collect());
    model.headers.insert("originator".into(), "custom".into());
    let mut context = context();
    context.tools = vec![Tool::new("read", "Read a file", json!({"type": "object", "properties": {}}))];
    let options = StreamOptions {
        session_id: Some(session.clone()),
        reasoning: Some(ThinkingLevel::Xhigh),
        tool_choice: Some("required".into()),
        headers: [("User-Agent".to_string(), "caller".to_string())].into_iter().collect(),
        ..sse_options(&mock_token("acc"))
    };
    let (_, message) = collect(OpenAICodexResponses.stream(&model, &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let request = &server.requests()[0];
    let clamped = "s".repeat(64);
    assert_eq!(
        (request.headers["session-id"].as_str(), request.headers["x-client-request-id"].as_str()),
        (clamped.as_str(), clamped.as_str())
    );
    assert_eq!((request.headers["originator"].as_str(), request.headers["user-agent"].as_str()), ("custom", "caller"));
    let body = decoded_body(request);
    assert_eq!(body["prompt_cache_key"], clamped);
    assert_eq!(body["reasoning"], json!({"effort": "xhigh", "summary": "auto"}));
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["tools"][0]["strict"], Value::Null);

    let options = StreamOptions {
        session_id: Some("x".into()),
        cache_retention: Some(crate::registry::CacheRetention::None),
        ..sse_options(&mock_token("acc"))
    };
    let payload = events("response.completed", "completed", None);
    let server = RouteServer::start(move |_| sse_reply(&payload)).await;
    let _ = collect(OpenAICodexResponses.stream(&codex_model(&server.base_url), &context, options)).await;
    let request = &server.requests()[0];
    assert!(!request.headers.contains_key("session-id"));
    assert!(decoded_body(request).get("prompt_cache_key").is_none());
}

#[tokio::test]
async fn retries_with_server_delays_and_reports_usage_limits() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let payload = events("response.completed", "completed", None);
    let server = RouteServer::start(move |_| match counter.fetch_add(1, Ordering::SeqCst) {
        0 => Reply::json(503, &json!({"error": {"message": "overloaded"}})).header("retry-after-ms", "10"),
        _ => sse_reply(&payload),
    })
    .await;
    let options = StreamOptions { max_retries: Some(2), ..sse_options(&mock_token("acc")) };
    let model = codex_model(&server.base_url);
    let message = OpenAICodexResponses.stream(&model, &context(), options).result(&model).await;
    assert_eq!((message.text(), calls.load(Ordering::SeqCst)), ("Hello".to_string(), 2));

    let server =
        RouteServer::start(|_| Reply::json(503, &json!({"error": {"message": "busy"}})).header("retry-after", "2"))
            .await;
    let options =
        StreamOptions { max_retries: Some(2), max_retry_delay_ms: Some(1000), ..sse_options(&mock_token("acc")) };
    let model = codex_model(&server.base_url);
    let message = OpenAICodexResponses.stream(&model, &context(), options).result(&model).await;
    assert_eq!(message.error_message.as_deref(), Some("Server requested 2s retry delay (max: 1s)"));
    assert_eq!(server.requests().len(), 1);

    let limit = json!({"error": {"code": "usage_limit_reached", "plan_type": "PRO", "resets_at": crate::types::now_ms() / 1000 + 600}});
    let server = RouteServer::start(move |_| Reply::json(429, &limit)).await;
    let model = codex_model(&server.base_url);
    let options = StreamOptions { max_retries: Some(3), ..sse_options(&mock_token("acc")) };
    let message = OpenAICodexResponses.stream(&model, &context(), options).result(&model).await;
    let error = message.error_message.unwrap();
    assert!(error.starts_with("You have hit your ChatGPT usage limit (pro plan). Try again in ~"), "{error}");
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn codex_error_events_and_bad_tokens_fail_the_turn() {
    let failed =
        vec![json!({"type": "response.failed", "response": {"error": {"code": "server_error", "message": "boom"}}})];
    let server = RouteServer::start(move |_| sse_reply(&failed)).await;
    let model = codex_model(&server.base_url);
    let message = OpenAICodexResponses.stream(&model, &context(), sse_options(&mock_token("acc"))).result(&model).await;
    assert_eq!((message.stop_reason, message.error_message.as_deref()), (StopReason::Error, Some("boom")));

    let error = vec![json!({"type": "error", "error": {"code": "invalid_request", "message": "Bad input"}})];
    let server = RouteServer::start(move |_| sse_reply(&error)).await;
    let model = codex_model(&server.base_url);
    let message = OpenAICodexResponses.stream(&model, &context(), sse_options(&mock_token("acc"))).result(&model).await;
    assert_eq!(message.error_message.as_deref(), Some("Codex error: Bad input"));

    let message = OpenAICodexResponses.stream(&model, &context(), sse_options("not-a-jwt")).result(&model).await;
    assert_eq!(message.error_message.as_deref(), Some("Failed to extract accountId from token"));
    let message = OpenAICodexResponses.stream(&model, &context(), StreamOptions::default()).result(&model).await;
    assert_eq!(message.error_message.as_deref(), Some("No API key for provider: openai-codex"));
}

#[tokio::test]
async fn streams_reasoning_tool_calls_and_service_tier_pricing() {
    let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Think"}], "encrypted_content": "enc"});
    let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"});
    let payload = vec![
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "Think"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"path\":\"a\"}"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": call}),
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "service_tier": "priority", "usage": {"input_tokens": 1_000_000, "output_tokens": 0, "total_tokens": 1_000_000}}}),
    ];
    let server = RouteServer::start(move |_| sse_reply(&payload)).await;
    let model = codex_model(&server.base_url);
    let (events, message) =
        collect(OpenAICodexResponses.stream(&model, &context(), sse_options(&mock_token("acc")))).await;
    assert!(kinds(&events).contains(&"thinking_delta".to_string()));
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.tool_calls().next().unwrap().id, "call_1|fc_1");
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(message.usage.cost.input, 2.0);
}

#[tokio::test]
async fn aborts_an_open_sse_stream() {
    let payload = events("response.completed", "completed", None)[..3].to_vec();
    let server = RouteServer::start(move |_| Reply { hang: true, ..sse_reply(&payload) }).await;
    let options = sse_options(&mock_token("acc"));
    let cancel = options.cancel.clone();
    let stream = OpenAICodexResponses.stream(&codex_model(&server.base_url), &context(), options);
    let message = cancel_after_first_text(stream, cancel).await;
    assert_eq!(
        (message.stop_reason, message.error_message.as_deref()),
        (StopReason::Aborted, Some("Request was aborted"))
    );
}

#[tokio::test]
async fn times_out_when_sse_headers_never_arrive() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    let options = StreamOptions { timeout_ms: Some(50), ..sse_options(&mock_token("acc")) };
    let model = codex_model(&base_url);
    let message = OpenAICodexResponses.stream(&model, &context(), options).result(&model).await;
    assert_eq!(message.error_message.as_deref(), Some("Codex SSE response headers timed out after 50ms"));
}
