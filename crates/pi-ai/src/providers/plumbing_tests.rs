//! Request plumbing shared by the HTTP providers: retries, timeouts, hooks, proxies, headers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::mock_server::{MockResponse, MockServer, collect, frame, test_model};
use super::openai_completions::OpenAICompletions;
use crate::registry::{ApiProvider, ProviderResponse, StreamOptions};
use crate::types::{Context, Message, StopReason};

const API: &str = "openai-completions";

fn chunks() -> Vec<String> {
    vec![
        frame(None, &json!({"id": "c1", "choices": [{"index": 0, "delta": {"content": "hi"}}]})),
        frame(None, &json!({"id": "c1", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})),
        "data: [DONE]\n\n".to_string(),
    ]
}

fn context() -> Context {
    Context { messages: vec![Message::user("hello")], ..Context::default() }
}

fn options() -> StreamOptions {
    StreamOptions { api_key: Some("sk-test".into()), ..StreamOptions::default() }
}

#[tokio::test]
async fn retries_retryable_statuses_after_the_server_delay() {
    let mut server = MockServer::sequence(vec![
        MockResponse::with_headers(429, &[("retry-after-ms", "10")], r#"{"error":{"message":"slow down"}}"#),
        MockResponse::Sse(chunks()),
    ])
    .await;
    let model = test_model(API, "openai", &format!("{}/v1", server.base_url));
    let options = StreamOptions { max_retries: Some(1), ..options() };
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), options)).await;
    assert_eq!((message.stop_reason, message.text()), (StopReason::Stop, "hi".to_string()));
    server.request().await;
    server.request().await;
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn failures_are_not_retried_by_default_and_long_delays_fail_fast() {
    let server = MockServer::sequence(vec![MockResponse::Status(503, r#"{"error":{"message":"busy"}}"#.into())]).await;
    let model = test_model(API, "openai", &server.base_url);
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), options())).await;
    assert_eq!(message.error_message.as_deref(), Some("Provider HTTP 503: busy"));

    let server = MockServer::sequence(vec![MockResponse::with_headers(429, &[("retry-after", "120")], "")]).await;
    let model = test_model(API, "openai", &server.base_url);
    let options = StreamOptions { max_retries: Some(2), ..options() };
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), options)).await;
    let error = message.error_message.unwrap();
    assert!(error.starts_with("Server requested 120s retry delay (max: 60s)"), "{error}");
}

#[tokio::test]
async fn timeout_covers_the_wait_for_response_headers() {
    let delayed = MockResponse::Delayed(Duration::from_secs(5), Box::new(MockResponse::Sse(chunks())));
    let server = MockServer::sequence(vec![delayed]).await;
    let model = test_model(API, "openai", &server.base_url);
    let options = StreamOptions { timeout_ms: Some(50), ..options() };
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Request timed out after 50ms"));
}

#[tokio::test]
async fn hooks_see_payload_response_and_stream_events() {
    let mut server = MockServer::start(MockResponse::Sse(chunks())).await;
    let model = test_model(API, "openai", &server.base_url);
    let responses: Arc<Mutex<Vec<ProviderResponse>>> = Arc::default();
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let (seen_responses, seen_events) = (responses.clone(), events.clone());
    let options = StreamOptions {
        on_payload: Some(Arc::new(|payload, model| {
            let mut payload = payload.clone();
            payload["metadata"] = json!({"model": model.id});
            Some(payload)
        })),
        on_response: Some(Arc::new(move |response, _| seen_responses.lock().unwrap().push(response.clone()))),
        on_provider_stream_event: Some(Arc::new(move |event, _| seen_events.lock().unwrap().push(event.clone()))),
        ..options()
    };
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(server.request().await.body["metadata"], json!({"model": "test-model"}));
    let responses = responses.lock().unwrap();
    assert_eq!((responses[0].status, responses[0].headers["x-request-id"].as_str()), (200, "req_1"));
    assert_eq!(events.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn scoped_proxy_variables_route_requests_through_the_proxy() {
    let mut proxy = MockServer::start(MockResponse::Sse(chunks())).await;
    let model = test_model(API, "openai", "http://api.example.invalid/v1");
    let env = BTreeMap::from([("HTTP_PROXY".to_string(), proxy.base_url.clone())]);
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), StreamOptions { env, ..options() })).await;
    assert_eq!(message.text(), "hi");
    assert_eq!(proxy.request().await.path, "http://api.example.invalid/v1/chat/completions");

    let env = BTreeMap::from([("https_proxy".to_string(), "socks5://127.0.0.1:1".to_string())]);
    let model = test_model(API, "openai", "https://api.example.invalid/v1");
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), StreamOptions { env, ..options() })).await;
    assert!(message.error_message.unwrap().contains("SOCKS and PAC proxy URLs are not supported"));
}

#[tokio::test]
async fn suppressed_headers_remove_provider_defaults() {
    let mut server = MockServer::start(MockResponse::Sse(chunks())).await;
    let model = test_model(API, "openai", &server.base_url);
    let options = StreamOptions {
        headers: BTreeMap::from([("X-Extra".to_string(), "1".to_string())]),
        suppressed_headers: vec!["Authorization".into()],
        ..options()
    };
    collect(OpenAICompletions.stream(&model, &context(), options)).await;
    let request = server.request().await;
    assert!(!request.headers.contains_key("authorization"));
    assert_eq!(request.headers["x-extra"], "1");
}
