//! Preferred strict tool fallback, ported from upstream `bedrock-strict-tool-fallback.test.ts`
//! (upstream issue 11416).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::BedrockConverseStream;
use super::tests::{bearer_options, bedrock_model, event, stream_reply};
use crate::providers::http_mock::{Reply, RouteServer};
use crate::providers::mock_server::{collect, kinds};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions, payload_hook};
use crate::types::{AssistantMessage, Context, Message, Model, StopReason, Tool};

const UNION_LIMIT: &str = "The model returned the following errors: Schemas contains too many parameters with union types (20 parameters with type arrays or anyOf). This causes exponential compilation cost. Reduce the number of nullable or union-typed parameters (limit: 16 parameters with unions).";

fn tool(name: &str, strict: Option<&str>) -> Tool {
    let parameters = json!({"type": "object", "properties": {"value": {"type": "string"},
        "optional": {"type": "number"}}, "required": ["value"]});
    let mut tool = Tool::new(name, "Look up a value", parameters);
    tool.constrained_sampling = strict.map(|strict| json!({"type": "json_schema", "strict": strict}));
    tool
}

fn model(base_url: &str) -> Model {
    Model {
        reasoning: false,
        max_tokens: 100,
        compat: Some(json!({"supportsStrictMode": true})),
        ..bedrock_model("us.anthropic.claude-opus-5-5", base_url)
    }
}

fn rejection(code: &str, message: &str) -> Reply {
    Reply::json(400, &json!({"message": message})).header("x-amzn-errortype", code)
}

fn success() -> Reply {
    stream_reply(vec![
        event("messageStart", json!({"role": "assistant"})),
        event("messageStop", json!({"stopReason": "end_turn"})),
    ])
}

fn options() -> StreamOptions {
    StreamOptions { cache_retention: Some(CacheRetention::None), ..bearer_options() }
}

async fn run(
    reply: impl Fn(usize) -> Reply + Send + Sync + 'static,
    tools: Vec<Tool>,
    options: StreamOptions,
) -> (Vec<Value>, Vec<String>, AssistantMessage) {
    let attempts = AtomicUsize::new(0);
    let server = RouteServer::start(move |_| reply(attempts.fetch_add(1, Ordering::SeqCst) + 1)).await;
    let context = Context { messages: vec![Message::user("Hello")], tools, ..Context::default() };
    let (events, message) = collect(BedrockConverseStream.stream(&model(&server.base_url), &context, options)).await;
    let bodies = server.requests().iter().map(|request| request.json()).collect();
    (bodies, kinds(&events), message)
}

#[tokio::test]
async fn retries_a_strict_tool_limit_rejection_with_preferred_tools_non_strict_and_keeps_required_tools_strict() {
    let payloads = Arc::new(AtomicUsize::new(0));
    let counted = payloads.clone();
    let options = StreamOptions {
        on_payload: Some(payload_hook(move |payload, _| {
            counted.fetch_add(1, Ordering::SeqCst);
            let mut payload = payload.clone();
            payload["requestMetadata"] = json!({"team": "a"});
            Some(payload)
        })),
        ..options()
    };
    let reply = |attempt: usize| if attempt == 1 { rejection("ValidationException", UNION_LIMIT) } else { success() };
    let tools = vec![tool("preferred", Some("prefer")), tool("required", Some("require")), tool("ordinary", None)];
    let (bodies, kinds, message) = run(reply, tools, options).await;

    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(kinds, ["start", "done"]);
    assert_eq!((bodies.len(), payloads.load(Ordering::SeqCst)), (2, 2));
    let (first, second) = (&bodies[0]["toolConfig"]["tools"], &bodies[1]["toolConfig"]["tools"]);
    assert_eq!(first[0]["toolSpec"]["strict"], true);
    assert_eq!(first[0]["toolSpec"]["inputSchema"]["json"]["required"], json!(["value", "optional"]));
    assert_eq!(
        first[0]["toolSpec"]["inputSchema"]["json"]["properties"]["optional"],
        json!({"anyOf": [{"type": "number"}, {"type": "null"}]})
    );
    assert!(second[0]["toolSpec"].get("strict").is_none());
    assert_eq!(
        second[0]["toolSpec"]["inputSchema"]["json"],
        json!({"type": "object", "properties": {"value": {"type": "string"}, "optional": {"type": "number"}},
            "required": ["value"]})
    );
    assert_eq!(second[1], first[1]);
    assert_eq!(second[1]["toolSpec"]["strict"], true);
    assert_eq!(second[2], first[2]);
    assert_eq!(bodies[1]["requestMetadata"], json!({"team": "a"}));
    assert_eq!(message.diagnostics.len(), 1, "{:?}", message.diagnostics);
    let diagnostic = &message.diagnostics[0];
    assert_eq!(diagnostic["type"], "bedrock_strict_tools_fallback");
    assert_eq!(diagnostic["error"], json!({"message": UNION_LIMIT}));
    assert_eq!(diagnostic["details"], json!({"reason": "strict_tool_limits_exceeded"}));
}

#[tokio::test]
async fn retries_each_known_strict_tool_limit() {
    let limits = [
        "The compiled grammar is too large, which would cause performance issues.",
        "Too many strict tools (32). The maximum number of strict tools supported is 20.",
        UNION_LIMIT,
    ];
    for limit in limits {
        let reply =
            move |attempt: usize| if attempt == 1 { rejection("ValidationException", limit) } else { success() };
        let (bodies, _, message) = run(reply, vec![tool("preferred", Some("prefer"))], options()).await;
        assert_eq!((message.stop_reason, bodies.len()), (StopReason::Stop, 2), "{limit}");
        assert_eq!(message.diagnostics[0]["error"]["message"], limit);
    }
}

#[tokio::test]
async fn does_not_retry_without_preferred_strict_tools() {
    let reply = |_: usize| rejection("ValidationException", UNION_LIMIT);
    let tools = vec![tool("required", Some("require")), tool("ordinary", None)];
    let (bodies, kinds, message) = run(reply, tools, options()).await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message, Some(format!("Validation error: {UNION_LIMIT}")));
    assert_eq!(kinds, ["error"]);
    assert_eq!(bodies.len(), 1);
}

#[tokio::test]
async fn does_not_retry_other_validation_errors() {
    let reply = |_: usize| rejection("ValidationException", "messages: field required");
    let (bodies, _, message) = run(reply, vec![tool("preferred", Some("prefer"))], options()).await;
    assert_eq!((message.stop_reason, bodies.len()), (StopReason::Error, 1));

    let reply = |_: usize| rejection("AccessDeniedException", UNION_LIMIT);
    let (bodies, _, message) = run(reply, vec![tool("preferred", Some("prefer"))], options()).await;
    assert_eq!((message.stop_reason, bodies.len()), (StopReason::Error, 1));
    assert!(message.diagnostics.iter().all(|diagnostic| diagnostic["type"] != "bedrock_strict_tools_fallback"));
}

#[tokio::test]
async fn retries_only_once() {
    let reply = |_: usize| rejection("ValidationException", UNION_LIMIT);
    let (bodies, kinds, message) = run(reply, vec![tool("preferred", Some("prefer"))], options()).await;

    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message, Some(format!("Validation error: {UNION_LIMIT}")));
    assert_eq!(kinds, ["error"]);
    assert_eq!(bodies.len(), 2);
    let types: Vec<&str> = message.diagnostics.iter().filter_map(|diagnostic| diagnostic["type"].as_str()).collect();
    assert_eq!(types, ["bedrock_strict_tools_fallback", "bedrock_response_failure"]);
}

#[tokio::test]
async fn does_not_retry_after_cancellation() {
    let options = options();
    let cancel = options.cancel.clone();
    let sends = Arc::new(AtomicUsize::new(0));
    let counted = sends.clone();
    let reply = move |_: usize| {
        counted.fetch_add(1, Ordering::SeqCst);
        cancel.cancel();
        rejection("ValidationException", UNION_LIMIT)
    };
    let (_, kinds, message) = run(reply, vec![tool("preferred", Some("prefer"))], options).await;

    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.error_message.as_deref(), Some("Request was aborted"));
    assert_eq!(kinds, ["error"]);
    assert_eq!(sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_retry_reuses_the_resolved_auth_and_custom_headers() {
    let headers: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let seen = headers.clone();
    let attempts = AtomicUsize::new(0);
    let server = RouteServer::start(move |request| {
        let authorization = request.headers.get("authorization").cloned().unwrap_or_default();
        let custom = request.headers.get("x-team").cloned().unwrap_or_default();
        seen.lock().unwrap().push((authorization, custom));
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            rejection("ValidationException", UNION_LIMIT)
        } else {
            success()
        }
    })
    .await;
    let mut options = options();
    options.headers.insert("x-team".into(), "a".into());
    let tools = vec![tool("preferred", Some("prefer"))];
    let context = Context { messages: vec![Message::user("Hello")], tools, ..Context::default() };
    let (_, message) = collect(BedrockConverseStream.stream(&model(&server.base_url), &context, options)).await;

    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let expected = ("Bearer bedrock-key".to_string(), "a".to_string());
    assert_eq!(*headers.lock().unwrap(), [expected.clone(), expected]);
}
