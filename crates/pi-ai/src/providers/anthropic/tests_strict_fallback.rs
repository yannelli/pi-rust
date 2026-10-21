//! Preferred strict tool fallback, ported from upstream `anthropic-strict-tool-fallback.test.ts`.
//! Hook exceptions have no Rust counterpart: hooks cannot fail.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::AnthropicMessages;
use super::tests::{API, message_end, message_start, options};
use crate::providers::mock_server::{MockResponse, MockServer, collect, frame, kinds, test_model};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions, payload_hook};
use crate::types::{AssistantMessage, Context, Message, Model, StopReason, Tool};

const GRAMMAR_ERROR: &str = "The compiled grammar is too large, which would cause performance issues. Simplify your tool schemas or reduce the number of strict tools.";

fn tool(name: &str, strict: Option<&str>) -> Tool {
    let parameters = json!({"type": "object", "properties": {"value": {"type": "string"},
        "optional": {"type": "number"}}, "required": ["value"]});
    let mut tool = Tool::new(name, "Look up a value", parameters);
    tool.constrained_sampling = strict.map(|strict| json!({"type": "json_schema", "strict": strict}));
    tool
}

fn model(base_url: &str, compat: Value) -> Model {
    Model { compat: Some(compat), ..test_model(API, "anthropic", base_url) }
}

fn context(tools: Vec<Tool>) -> Context {
    Context { messages: vec![Message::user("Hello")], tools, ..Context::default() }
}

fn rejection(message: &str, status: u16, kind: &str) -> MockResponse {
    MockResponse::Status(status, json!({"type": "error", "error": {"type": kind, "message": message}}).to_string())
}

fn grammar_rejection() -> MockResponse {
    rejection(GRAMMAR_ERROR, 400, "invalid_request_error")
}

fn success() -> MockResponse {
    let mut frames = vec![message_start(json!({"input_tokens": 10, "output_tokens": 0}))];
    frames.extend(message_end("end_turn", json!({"output_tokens": 1})));
    MockResponse::Sse(frames)
}

fn strict_options() -> StreamOptions {
    StreamOptions { max_retries: Some(0), cache_retention: Some(CacheRetention::Long), ..options() }
}

async fn run(
    responses: Vec<MockResponse>,
    context: &Context,
    options: StreamOptions,
) -> (Vec<Value>, Vec<String>, AssistantMessage) {
    let server = MockServer::sequence(responses).await;
    let model = model(&server.base_url, json!({"supportsStrictTools": true}));
    let (events, message) = collect(AnthropicMessages.stream(&model, context, options)).await;
    let bodies = server.requests().into_iter().map(|request| request.body).collect();
    (bodies, kinds(&events), message)
}

#[tokio::test]
async fn retries_known_limits_with_original_non_strict_schemas_and_keeps_required_tools() {
    let limits = [
        GRAMMAR_ERROR,
        "Too many strict tools (32). The maximum number of strict tools supported is 20. Try reducing the number of tools marked as strict.",
        "Schemas contains too many parameters with union types (480 parameters with type arrays or anyOf). This causes exponential compilation cost.",
    ];
    for limit in limits {
        let context =
            context(vec![tool("preferred", Some("prefer")), tool("required", Some("require")), tool("ordinary", None)]);
        let payloads = Arc::new(AtomicUsize::new(0));
        let responses = Arc::new(AtomicUsize::new(0));
        let (counted, observed) = (payloads.clone(), responses.clone());
        let options = StreamOptions {
            on_payload: Some(payload_hook(move |payload, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                let mut payload = payload.clone();
                payload["inference_geo"] = json!("us");
                Some(payload)
            })),
            on_response: Some(Arc::new(move |_, _| {
                observed.fetch_add(1, Ordering::SeqCst);
            })),
            ..strict_options()
        };
        let (bodies, kinds, message) =
            run(vec![rejection(limit, 400, "invalid_request_error"), success()], &context, options).await;
        assert_eq!(message.stop_reason, StopReason::Stop, "{limit}");
        assert_eq!(kinds, ["start", "done"]);
        assert_eq!((bodies.len(), payloads.load(Ordering::SeqCst), responses.load(Ordering::SeqCst)), (2, 2, 1));
        let (first, second) = (&bodies[0]["tools"], &bodies[1]["tools"]);
        assert_eq!(first[0]["strict"], true);
        assert_eq!(first[0]["input_schema"]["required"], json!(["value", "optional"]));
        assert_eq!(
            first[0]["input_schema"]["properties"]["optional"],
            json!({"anyOf": [{"type": "number"}, {"type": "null"}]})
        );
        assert!(second[0].get("strict").is_none());
        let original = json!({"type": "object", "properties": {"value": {"type": "string"},
            "optional": {"type": "number"}}, "required": ["value"]});
        assert_eq!(second[0]["input_schema"], original);
        assert_eq!(second[1], first[1]);
        assert_eq!(second[1]["strict"], true);
        assert_eq!(second[2], first[2]);
        assert_eq!(second[2]["cache_control"], json!({"type": "ephemeral", "ttl": "1h"}));
        assert_eq!(bodies[1]["inference_geo"], "us");
        let diagnostic = &message.diagnostics[0];
        assert_eq!(diagnostic["type"], "anthropic_strict_tools_fallback");
        assert_eq!(diagnostic["details"]["reason"], "strict_tool_limits_exceeded");
    }
}

#[tokio::test]
async fn downgrades_inline_additions_and_redefinitions_but_not_require() {
    let system = |tool: Tool, timestamp: u64| -> Message {
        serde_json::from_value(json!({"role": "system", "content": "", "toolsAdded": [tool], "timestamp": timestamp}))
            .unwrap()
    };
    let context = Context {
        messages: vec![
            system(tool("lookup", Some("require")), 0),
            Message::user("Hello"),
            system(tool("lookup", Some("prefer")), 2),
            system(tool("lookup", Some("require")), 3),
        ],
        ..Context::default()
    };
    let server = MockServer::sequence(vec![grammar_rejection(), success()]).await;
    let native = json!({"supportsStrictTools": true, "supportsMidConvoSystemMessages": true,
        "supportsMidConvoToolChanges": true});
    let model = model(&server.base_url, native);
    let (_, message) = collect(AnthropicMessages.stream(&model, &context, strict_options())).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let bodies: Vec<Value> = server.requests().into_iter().map(|request| request.body).collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["tools"], bodies[0]["tools"]);
    let additions = |body: &Value| -> Vec<Value> {
        let blocks = body["messages"].as_array().unwrap().iter().flat_map(|m| m["content"].as_array().cloned());
        let blocks = blocks.flatten().filter(|block| block["type"] == "tool_addition");
        blocks.map(|block| block["tool"]["definition"].clone()).collect()
    };
    let (before, after) = (additions(&bodies[0]), additions(&bodies[1]));
    assert_eq!(before.iter().map(|d| d["strict"].clone()).collect::<Vec<_>>(), [json!(true), json!(true)]);
    assert_eq!(after.iter().map(|d| d["strict"].clone()).collect::<Vec<_>>(), [Value::Null, json!(true)]);
    assert_eq!(after[0]["input_schema"]["required"], json!(["value"]));
    assert_eq!(after[1], before[1]);
}

#[tokio::test]
async fn does_not_retry_tool_sets_without_a_strict_preferred_tool() {
    let unsupported = {
        let mut tool = tool("lookup", Some("prefer"));
        tool.parameters = json!({"type": "object", "properties": {"value": {"type": "number", "minimum": 1}}});
        tool
    };
    for tools in [vec![tool("lookup", Some("require"))], vec![tool("lookup", None)], vec![unsupported]] {
        let (bodies, _, message) = run(vec![grammar_rejection()], &context(tools), strict_options()).await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(message.error_message.unwrap().contains(GRAMMAR_ERROR));
        assert_eq!(bodies.len(), 1);
    }
}

#[tokio::test]
async fn does_not_downgrade_other_rejections() {
    let cases = [
        ("Invalid tool schema", 400, "invalid_request_error"),
        (GRAMMAR_ERROR, 400, "authentication_error"),
        (GRAMMAR_ERROR, 500, "invalid_request_error"),
    ];
    for (text, status, kind) in cases {
        let context = context(vec![tool("lookup", Some("prefer"))]);
        let (bodies, _, message) = run(vec![rejection(text, status, kind)], &context, strict_options()).await;
        assert_eq!(message.stop_reason, StopReason::Error, "{text} {status} {kind}");
        assert_eq!(bodies.len(), 1);
        assert!(message.diagnostics.is_empty());
    }
}

#[tokio::test]
async fn surfaces_a_second_rejection_without_retrying_again() {
    let responses =
        vec![grammar_rejection(), rejection("Required grammar is still too large", 400, "invalid_request_error")];
    let context = context(vec![tool("lookup", Some("prefer")), tool("required", Some("require"))]);
    let (bodies, _, message) = run(responses, &context, strict_options()).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(message.error_message.unwrap().contains("Required grammar is still too large"));
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["tools"][1]["strict"], true);
}

#[tokio::test]
async fn honors_cancellation_before_the_fallback_attempt() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let server = MockServer::routed(move |_| {
        trigger.cancel();
        grammar_rejection()
    })
    .await;
    let model = model(&server.base_url, json!({"supportsStrictTools": true}));
    let context = context(vec![tool("lookup", Some("prefer"))]);
    let options = StreamOptions { cancel, ..strict_options() };
    let (_, message) = collect(AnthropicMessages.stream(&model, &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn does_not_retry_an_sse_error_after_the_response_started() {
    let error = json!({"type": "error", "error": {"type": "invalid_request_error", "message": GRAMMAR_ERROR}});
    let response = MockResponse::Sse(vec![frame(Some("error"), &error)]);
    let context = context(vec![tool("lookup", Some("prefer"))]);
    let (bodies, _, message) = run(vec![response], &context, strict_options()).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(bodies.len(), 1);
}

#[tokio::test]
async fn later_requests_send_strict_tools_again() {
    let server = MockServer::sequence(vec![grammar_rejection(), success(), success()]).await;
    let model = model(&server.base_url, json!({"supportsStrictTools": true}));
    let context = context(vec![tool("lookup", Some("prefer"))]);
    for _ in 0..2 {
        collect(AnthropicMessages.stream(&model, &context, strict_options())).await;
    }
    let strict: Vec<Value> =
        server.requests().iter().map(|request| request.body["tools"][0]["strict"].clone()).collect();
    assert_eq!(strict, [json!(true), Value::Null, json!(true)]);
}

#[tokio::test]
async fn the_payload_hook_sees_each_attempt() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let options = StreamOptions {
        on_payload: Some(payload_hook(move |payload, _| {
            log.lock().unwrap().push(payload["tools"][0]["strict"].clone());
            None
        })),
        ..strict_options()
    };
    let context = context(vec![tool("lookup", Some("prefer"))]);
    run(vec![grammar_rejection(), success()], &context, options).await;
    assert_eq!(*seen.lock().unwrap(), [json!(true), Value::Null]);
}
