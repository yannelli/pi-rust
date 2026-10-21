use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::AzureOpenAIResponses;
use crate::providers::mock_http::{Body, MockHttp, Reply};
use crate::providers::mock_server::{cancel_after_first_text, collect, frame, test_model};
use crate::registry::{ApiProvider, ProviderResponse, StreamOptions};
use crate::types::{ContentBlock, Context, Message, Model, StopReason, ThinkingLevel, Tool};

const API: &str = "azure-openai-responses";

fn event(data: Value) -> String {
    let kind = data["type"].as_str().unwrap().to_string();
    frame(Some(&kind), &data)
}

fn text_stream(text: &str, output: Value) -> Reply {
    let item = json!({"type": "message", "id": "msg_1", "role": "assistant", "content": []});
    let done = json!({"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": text}]});
    Reply::sse(vec![
        event(json!({"type": "response.created", "response": {"id": "resp_azure"}})),
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": item})),
        event(json!({"type": "response.output_text.delta", "output_index": 0, "delta": text})),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": done})),
        event(
            json!({"type": "response.completed", "response": {"id": "resp_azure", "status": "completed", "output": output,
            "usage": {"input_tokens": 12, "output_tokens": 3, "total_tokens": 15}}}),
        ),
    ])
}

fn model() -> Model {
    Model { id: "gpt-5-mini".into(), base_url: String::new(), ..test_model(API, "azure", "") }
}

fn context() -> Context {
    Context { messages: vec![Message::user("hello")], ..Context::default() }
}

fn keyed(server: &MockHttp) -> StreamOptions {
    let mut options = StreamOptions { api_key: Some("azure-key".into()), ..StreamOptions::default() };
    options.env.insert("AZURE_OPENAI_BASE_URL".into(), format!("{}/openai/v1", server.base_url));
    options
}

#[tokio::test]
async fn streams_through_the_responses_handler_with_azure_auth_and_deployments() {
    let server = MockHttp::start(vec![text_stream("Hi there", json!([]))]).await;
    let mut target = model();
    target.sampling_params = Some(json!({"top_p": 0.5}).as_object().unwrap().clone());
    let responses = Arc::new(Mutex::new(Vec::<ProviderResponse>::new()));
    let hook = responses.clone();
    let mut options = StreamOptions {
        session_id: Some("x".repeat(67)),
        reasoning: Some(ThinkingLevel::High),
        tool_choice: Some("required".into()),
        on_response: Some(Arc::new(move |response: &ProviderResponse, _: &Model| {
            hook.lock().unwrap().push(response.clone())
        })),
        ..keyed(&server)
    };
    options.env.insert("AZURE_OPENAI_DEPLOYMENT_NAME_MAP".into(), "gpt-5-mini=mini-prod".into());
    options.env.insert("AZURE_OPENAI_API_VERSION".into(), "2025-04-01-preview".into());
    let tool = Tool::new("read", "Read a file", json!({"type": "object", "properties": {"path": {"type": "string"}}}));
    let context = Context { system_prompt: Some("Be brief".into()), tools: vec![tool], ..context() };
    let (_, message) = collect(AzureOpenAIResponses.stream(&target, &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!((message.text().as_str(), message.response_id.as_deref()), ("Hi there", Some("resp_azure")));
    assert_eq!((message.usage.input, message.usage.output), (12, 3));
    assert!(message.usage.cost.total > 0.0);
    assert_eq!(responses.lock().unwrap()[0].status, 200);
    let request = server.request(0);
    assert_eq!(request.path, "/openai/v1/responses?api-version=2025-04-01-preview");
    assert_eq!(request.headers["api-key"], "azure-key");
    assert!(request.headers["user-agent"].starts_with("pi ("));
    let body = request.json();
    assert_eq!(body["model"], "mini-prod");
    assert_eq!((body["store"].clone(), body["stream"].clone()), (json!(false), json!(true)));
    assert_eq!(body["prompt_cache_key"], "x".repeat(64));
    assert_eq!(body["max_output_tokens"], 4096);
    assert_eq!(body["reasoning"], json!({"effort": "high", "summary": "auto"}));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!((body["tool_choice"].clone(), body["top_p"].clone()), (json!("required"), json!(0.5)));
    assert_eq!(body["tools"][0]["strict"], false);
    assert_eq!(body["input"][0], json!({"role": "developer", "content": "Be brief"}));
}

#[test]
fn additional_tools_models_declare_tools_in_the_input() {
    let mut target = model();
    target.compat = Some(json!({"supportsMidConvoSystemMessages": true, "supportsAdditionalTools": true}));
    let tool = Tool::new("read", "d", json!({"type": "object", "properties": {}}));
    let context = Context { tools: vec![tool], ..context() };
    let (body, _) = super::build_body(&target, &context, &StreamOptions::default()).unwrap();
    assert!(body.get("tools").is_none());
    assert_eq!(body["input"][0]["type"], "additional_tools");
    assert_eq!(body["input"][0]["tools"][0]["name"], "read");
}

#[tokio::test]
async fn strict_mode_compat_and_disabled_reasoning() {
    let server = MockHttp::start(vec![text_stream("ok", json!([]))]).await;
    let mut target = model();
    target.compat = Some(json!({"supportsStrictMode": false}));
    let mut tool = Tool::new("preferred", "d", json!({"type": "object", "properties": {"value": {"type": "string"}}}));
    tool.constrained_sampling = Some(json!({"type": "json_schema", "strict": "prefer"}));
    let context = Context { tools: vec![tool], ..context() };
    let (_, message) = collect(AzureOpenAIResponses.stream(&target, &context, keyed(&server))).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let body = server.request(0).json();
    assert!(body["tools"][0].get("strict").is_none());
    assert_eq!(body["reasoning"], json!({"effort": "none"}));
}

#[tokio::test]
async fn replays_encrypted_reasoning_from_the_completed_response() {
    let reasoning = json!({"type": "reasoning", "id": "rs_missing", "summary": []});
    let completed = json!([{"type": "reasoning", "id": "rs_missing", "summary": [], "encrypted_content": "from-response-completed"}]);
    let first = Reply::sse(vec![
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": reasoning})),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning})),
        event(
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": completed}}),
        ),
    ]);
    let server = MockHttp::start(vec![first, text_stream("ok", json!([]))]).await;
    let target = model();
    let (_, assistant) = collect(AzureOpenAIResponses.stream(&target, &context(), keyed(&server))).await;
    assert!(matches!(assistant.content[0], ContentBlock::Thinking { .. }));
    let history = Context {
        messages: vec![Message::user("first"), Message::Assistant(assistant), Message::user("follow-up")],
        ..Context::default()
    };
    collect(AzureOpenAIResponses.stream(&target, &history, keyed(&server))).await;
    let input = server.request(1).json()["input"].clone();
    let replayed = input.as_array().unwrap().iter().find(|item| item["type"] == "reasoning").cloned().unwrap();
    assert_eq!(
        (replayed["id"].clone(), replayed["encrypted_content"].clone()),
        (json!("rs_missing"), json!("from-response-completed"))
    );
}

#[tokio::test]
async fn stream_events_retries_and_error_bodies() {
    let busy = json!({"error": {"message": "Rate limited", "code": "429"}});
    let server =
        MockHttp::start(vec![Reply::json(429, &busy).header("retry-after-ms", "10"), text_stream("ok", json!([]))])
            .await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let hook = seen.clone();
    let options = StreamOptions {
        max_retries: Some(1),
        on_provider_stream_event: Some(Arc::new(move |event: &Value, _: &Model| {
            hook.lock().unwrap().push(event.clone())
        })),
        ..keyed(&server)
    };
    let (_, message) = collect(AzureOpenAIResponses.stream(&model(), &context(), options)).await;
    assert_eq!((message.stop_reason, message.text().as_str()), (StopReason::Stop, "ok"));
    let seen = seen.lock().unwrap().clone();
    assert_eq!((seen.len(), seen[0]["type"].as_str()), (5, Some("response.created")));

    let server =
        MockHttp::start(vec![Reply::json(400, &json!({"error": {"message": "Bad input", "code": "bad"}}))]).await;
    let (_, message) = collect(AzureOpenAIResponses.stream(&model(), &context(), keyed(&server))).await;
    assert_eq!(
        message.error_message.as_deref(),
        Some(r#"Azure OpenAI API error (400): {"message":"Bad input","code":"bad"}"#)
    );
    let server = MockHttp::start(vec![Reply::text(502, "text/plain", "upstream down")]).await;
    let (_, message) = collect(AzureOpenAIResponses.stream(&model(), &context(), keyed(&server))).await;
    assert_eq!(message.error_message.as_deref(), Some("Azure OpenAI API error (502): 502 upstream down"));
}

#[tokio::test]
async fn aborts_and_validates_configuration() {
    let item = json!({"type": "message", "id": "msg_1", "role": "assistant", "content": []});
    let frames = vec![
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": item})),
        event(json!({"type": "response.output_text.delta", "output_index": 0, "delta": "partial"})),
    ];
    let server = MockHttp::start(vec![Reply { body: Body::ChunksThenHang(frames), ..Reply::sse(Vec::new()) }]).await;
    let options = keyed(&server);
    let cancel = options.cancel.clone();
    let message = cancel_after_first_text(AzureOpenAIResponses.stream(&model(), &context(), options), cancel).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);

    let (_, message) = collect(AzureOpenAIResponses.stream(&model(), &context(), StreamOptions::default())).await;
    assert_eq!(message.error_message.as_deref(), Some("No API key for provider: azure"));
    let mut invalid = StreamOptions { api_key: Some("k".into()), ..StreamOptions::default() };
    invalid.env.insert("AZURE_OPENAI_BASE_URL".into(), "not-a-url".into());
    let (_, message) = collect(AzureOpenAIResponses.stream(&model(), &context(), invalid)).await;
    assert!(message.error_message.unwrap().contains("Invalid Azure OpenAI base URL"));
}

#[tokio::test]
async fn streams_tool_calls_and_sends_images() {
    let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": ""});
    let done = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a.png\"}"});
    let server = MockHttp::start(vec![Reply::sse(vec![
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": call})),
        event(json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"path\":\"a.png\"}"})),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": done})),
        event(json!({"type": "response.completed", "response": {"id": "resp_2", "status": "completed", "output": []}})),
    ])])
    .await;
    let image = ContentBlock::Image { data: "aGVsbG8=".into(), mime_type: "image/png".into() };
    let user = Message::User(crate::types::UserMessage {
        content: crate::types::UserContent::Blocks(vec![ContentBlock::text("open it"), image]),
        timestamp: 1,
    });
    let context = Context { messages: vec![user], ..Context::default() };
    let (_, message) = collect(AzureOpenAIResponses.stream(&model(), &context, keyed(&server))).await;
    assert_eq!(message.stop_reason, StopReason::ToolUse, "{:?}", message.error_message);
    let call = message.tool_calls().next().unwrap();
    assert_eq!((call.id.as_str(), &call.arguments["path"]), ("call_1|fc_1", &json!("a.png")));
    let input = server.request(0).json()["input"].clone();
    assert_eq!(input[0]["content"][1]["type"], "input_image");
    assert_eq!(input[0]["content"][1]["image_url"], "data:image/png;base64,aGVsbG8=");
}

#[tokio::test]
async fn reasoning_summary_and_endpoint_options() {
    let server = MockHttp::start(vec![text_stream("ok", json!([]))]).await;
    let mut options = keyed(&server);
    options.env.remove("AZURE_OPENAI_BASE_URL");
    options.azure.base_url = Some(format!("{}/openai/v1", server.base_url));
    options.azure.deployment_name = Some("option-deploy".into());
    options.azure.api_version = Some("2025-03-01-preview".into());
    options.reasoning_summary = Some(crate::provider_options::ReasoningSummary::Detailed);
    let (_, message) = collect(AzureOpenAIResponses.stream(&model(), &context(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let request = server.request(0);
    assert_eq!(request.path, "/openai/v1/responses?api-version=2025-03-01-preview");
    let body = request.json();
    assert_eq!(body["model"], "option-deploy");
    assert_eq!(body["reasoning"], json!({"effort": "medium", "summary": "detailed"}));
}
