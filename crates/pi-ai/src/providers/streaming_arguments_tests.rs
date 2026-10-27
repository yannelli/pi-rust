//! Tool-call argument streaming through every provider that streams arguments, ported from
//! upstream `streaming-tool-arguments.test.ts` and `bedrock-stream-state.test.ts` (#9265).

use futures::StreamExt;
use serde_json::{Value, json};

use crate::providers::anthropic::AnthropicMessages;
use crate::providers::aws::event_stream::{HeaderValue, Message as Frame, encode};
use crate::providers::bedrock::BedrockConverseStream;
use crate::providers::faux::{FauxProvider, FauxResponse};
use crate::providers::http_mock::{Reply, RouteServer};
use crate::providers::mistral::MistralConversations;
use crate::providers::mock_server::{MockResponse, MockServer, frame, test_model};
use crate::providers::openai_completions::OpenAICompletions;
use crate::providers::openai_responses::OpenAIResponses;
use crate::providers::pi_messages::PiMessages;
use crate::registry::{ApiProvider, StreamOptions};
use crate::stream::{AssistantMessageEvent, AssistantMessageEventStream};
use crate::types::{AssistantMessage, ContentBlock, Context, Message, Model, StopReason};

/// `write` arguments with `size` bytes of file content, and their JSON text.
fn write_call(size: usize) -> (Value, String) {
    let mut content = String::new();
    let mut line = 0;
    while content.len() < size {
        content.push_str(&format!("const line{line} = \"value {line}\"; // \\ done\n"));
        line += 1;
    }
    content.truncate(size);
    let arguments = json!({"path": "/src/file.ts", "content": content});
    let text = arguments.to_string();
    (arguments, text)
}

fn deltas_of(text: &str, size: usize) -> Vec<&str> {
    (0..text.len()).step_by(size).map(|start| &text[start..text.len().min(start + size)]).collect()
}

fn context() -> Context {
    Context { messages: vec![Message::user("write")], ..Context::default() }
}

fn keyed(key: &str) -> StreamOptions {
    StreamOptions { api_key: Some(key.into()), ..StreamOptions::default() }
}

fn named(data: Value) -> String {
    let kind = data["type"].as_str().unwrap().to_string();
    frame(Some(&kind), &data)
}

fn anthropic_frames(deltas: &[&str]) -> Vec<String> {
    let mut frames = vec![
        named(
            json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-test", "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ),
        named(
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "write", "input": {}}}),
        ),
    ];
    for delta in deltas {
        let event = json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": delta}});
        frames.push(named(event));
    }
    frames.push(named(json!({"type": "content_block_stop", "index": 0})));
    frames.push(named(
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 10}}),
    ));
    frames.push(named(json!({"type": "message_stop"})));
    frames
}

fn chat_frames(deltas: &[&str], with_id_every_chunk: bool) -> Vec<String> {
    let call = |fields: Value| {
        let mut call = json!({"index": 0});
        call.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        frame(None, &json!({"id": "chat_1", "choices": [{"index": 0, "delta": {"tool_calls": [call]}}]}))
    };
    let mut frames =
        vec![call(json!({"id": "call_1", "type": "function", "function": {"name": "write", "arguments": ""}}))];
    for delta in deltas {
        frames.push(call(if with_id_every_chunk {
            json!({"id": "call_1", "function": {"name": "write", "arguments": delta}})
        } else {
            json!({"function": {"arguments": delta}})
        }));
    }
    let usage = json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2});
    let finish = json!({"index": 0, "delta": {}, "finish_reason": "tool_calls"});
    frames.push(frame(None, &json!({"id": "chat_1", "choices": [finish], "usage": usage})));
    frames.push("data: [DONE]\n\n".into());
    frames
}

fn responses_frames(deltas: &[&str], text: &str) -> Vec<String> {
    let item = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "write", "arguments": ""});
    let mut frames = vec![frame(None, &json!({"type": "response.output_item.added", "output_index": 0, "item": item}))];
    for delta in deltas {
        let event = json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": delta});
        frames.push(frame(None, &event));
    }
    let done = json!({"type": "response.function_call_arguments.done", "output_index": 0, "arguments": text});
    frames.push(frame(None, &done));
    let mut item = item;
    item["arguments"] = json!(text);
    frames.push(frame(None, &json!({"type": "response.output_item.done", "output_index": 0, "item": item})));
    let usage = json!({"input_tokens": 1, "output_tokens": 1, "total_tokens": 2});
    let response = json!({"id": "resp_1", "status": "completed", "output": [], "usage": usage});
    frames.push(frame(None, &json!({"type": "response.completed", "response": response})));
    frames
}

fn pi_messages_frames(deltas: &[&str], arguments: &Value) -> Vec<String> {
    let mut events = vec![
        json!({"type": "start"}),
        json!({"type": "toolcall_start", "contentIndex": 0, "id": "call_1", "toolName": "write"}),
    ];
    events.extend(deltas.iter().map(|delta| json!({"type": "toolcall_delta", "contentIndex": 0, "delta": delta})));
    let tool_call = json!({"type": "toolCall", "id": "call_1", "name": "write", "arguments": arguments});
    events.push(json!({"type": "toolcall_end", "contentIndex": 0, "toolCall": tool_call}));
    let cost = json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0});
    let usage = json!({"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2, "cost": cost});
    events.push(json!({"type": "done", "reason": "toolUse", "usage": usage}));
    events.iter().map(|event| frame(None, event)).collect()
}

fn bedrock_event(kind: &str, payload: Value) -> Vec<u8> {
    encode(&Frame {
        headers: vec![
            (":message-type".into(), HeaderValue::String("event".into())),
            (":event-type".into(), HeaderValue::String(kind.into())),
            (":content-type".into(), HeaderValue::String("application/json".into())),
        ],
        payload: payload.to_string().into_bytes(),
    })
}

/// Encrypted reasoning, then a `write` call whose arguments arrive in 32-byte deltas.
fn bedrock_frames(text: &str) -> Vec<Vec<u8>> {
    let redacted = |bytes: &[u8]| {
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
        json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"redactedContent": encoded}}})
    };
    let start = json!({"contentBlockIndex": 1, "start": {"toolUse": {"toolUseId": "tool_1", "name": "write"}}});
    let mut frames = vec![
        bedrock_event("messageStart", json!({"role": "assistant"})),
        bedrock_event("contentBlockDelta", redacted(&[1, 2])),
        bedrock_event("contentBlockDelta", redacted(&[3])),
        bedrock_event("contentBlockStop", json!({"contentBlockIndex": 0})),
        bedrock_event("contentBlockStart", start),
    ];
    for delta in deltas_of(text, 32) {
        let event = json!({"contentBlockIndex": 1, "delta": {"toolUse": {"input": delta}}});
        frames.push(bedrock_event("contentBlockDelta", event));
    }
    frames.push(bedrock_event("contentBlockStop", json!({"contentBlockIndex": 1})));
    frames.push(bedrock_event("messageStop", json!({"stopReason": "tool_use"})));
    frames
}

/// Stream one `write` call whose argument text arrives in `delta`-byte pieces through `path`.
async fn stream_path(path: &str, arguments: &Value, text: &str, delta: usize) -> AssistantMessageEventStream {
    let deltas = deltas_of(text, delta);
    let sse = |frames: Vec<String>| MockServer::start(MockResponse::Sse(frames));
    match path {
        "anthropic-messages" => {
            let server = sse(anthropic_frames(&deltas)).await;
            let model = test_model(path, "anthropic", &server.base_url);
            AnthropicMessages.stream(&model, &context(), keyed("sk-ant-api-test"))
        }
        "openai-completions" | "mistral-conversations" => {
            let server = sse(chat_frames(&deltas, path == "mistral-conversations")).await;
            let model = Model { reasoning: false, ..test_model(path, "openai", &server.base_url) };
            match path {
                "openai-completions" => OpenAICompletions.stream(&model, &context(), keyed("sk-test")),
                _ => MistralConversations.stream(&model, &context(), keyed("test")),
            }
        }
        "openai-responses" => {
            let server = sse(responses_frames(&deltas, text)).await;
            OpenAIResponses.stream(&test_model(path, "openai", &server.base_url), &context(), keyed("sk-test"))
        }
        "pi-messages" => {
            let server = sse(pi_messages_frames(&deltas, arguments)).await;
            let model = test_model(path, "radius", &format!("{}/v1", server.base_url));
            PiMessages.stream(&model, &context(), keyed("test"))
        }
        "bedrock-converse-stream" => {
            let frames = bedrock_frames(text);
            let server = RouteServer::start(move |_| Reply {
                status: 200,
                headers: vec![("content-type".into(), "application/vnd.amazon.eventstream".into())],
                chunks: frames.clone(),
                hang: false,
            })
            .await;
            let model = Model { reasoning: false, ..test_model(path, "amazon-bedrock", &server.base_url) };
            BedrockConverseStream.stream(&model, &context(), keyed("bedrock-key"))
        }
        _ => {
            let faux = FauxProvider::new([FauxResponse::tool_call("call_1", "write", arguments.clone())]);
            faux.stream(&FauxProvider::model(), &context(), StreamOptions::default())
        }
    }
}

const PATHS: [&str; 7] = [
    "anthropic-messages",
    "openai-completions",
    "openai-responses",
    "mistral-conversations",
    "pi-messages",
    "faux",
    "bedrock-converse-stream",
];

/// Content block keys as they serialize; anything else would be stream state leaking into partials.
fn expect_no_stream_state(message: &AssistantMessage, label: &str) {
    let allowed = [
        "type",
        "text",
        "textSignature",
        "thinking",
        "thinkingSignature",
        "redacted",
        "id",
        "name",
        "arguments",
        "argumentsError",
        "thoughtSignature",
        "namespace",
    ];
    for block in &message.content {
        let value = serde_json::to_value(block).unwrap();
        for key in value.as_object().unwrap().keys() {
            assert!(allowed.contains(&key.as_str()), "{label}: {key}");
        }
    }
}

#[tokio::test]
async fn every_path_shows_growing_partial_arguments_and_finishes_with_the_exact_arguments() {
    let (arguments, text) = write_call(5_000);
    let expected = arguments["content"].as_str().unwrap();
    for path in PATHS {
        let mut stream = stream_path(path, &arguments, &text, 24).await;
        let (mut shown, mut partials, mut message) = (0, 0, None);
        while let Some(event) = stream.next().await {
            if let Some(last) = event.final_message() {
                expect_no_stream_state(last, path);
                message = Some(last.clone());
                continue;
            }
            let AssistantMessageEvent::ToolcallDelta { content_index, partial, .. } = &event else { continue };
            expect_no_stream_state(partial, path);
            let ContentBlock::ToolCall(call) = &partial.content[*content_index] else { panic!("{path}") };
            let Some(content) = call.arguments.get("content").and_then(Value::as_str) else { continue };
            partials += 1;
            assert!(expected.starts_with(content) && content.len() >= shown, "{path}");
            shown = content.len();
        }
        let message = message.unwrap();
        assert_eq!(message.stop_reason, StopReason::ToolUse, "{path}: {:?}", message.error_message);
        assert!(partials > 100, "{path}: {partials} partials");
        let calls: Vec<_> = message.tool_calls().collect();
        assert_eq!(calls.len(), 1, "{path}");
        assert_eq!((calls[0].name.as_str(), calls[0].arguments_error.as_deref()), ("write", None), "{path}");
        assert_eq!(Value::Object(calls[0].arguments.clone()), arguments, "{path}");
    }
}

#[tokio::test]
async fn bedrock_keeps_encrypted_reasoning_off_the_blocks_until_it_ends() {
    let (arguments, text) = write_call(2_000);
    let stream = stream_path("bedrock-converse-stream", &arguments, &text, 32).await;
    let message = stream.result(&FauxProvider::model()).await;
    assert_eq!(message.stop_reason, StopReason::ToolUse, "{:?}", message.error_message);
    let ContentBlock::Thinking { thinking, thinking_signature, redacted } = &message.content[0] else { panic!() };
    assert_eq!(
        (thinking.as_str(), thinking_signature.as_deref(), *redacted),
        ("[Reasoning redacted]", Some("AQID"), true)
    );
}

#[tokio::test]
async fn a_stream_cut_off_mid_call_keeps_the_partial_arguments_on_the_error_message() {
    let (_, text) = write_call(2_000);
    let deltas = deltas_of(&text, 16);
    let mut frames = anthropic_frames(&deltas[..20]);
    // Drop the block stop and the message end, as a dropped connection does.
    frames.truncate(frames.len() - 3);
    let server = MockServer::start(MockResponse::Sse(frames)).await;
    let model = test_model("anthropic-messages", "anthropic", &server.base_url);
    let message = AnthropicMessages.stream(&model, &context(), keyed("sk-ant-api-test")).result(&model).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    expect_no_stream_state(&message, "error");
    let ContentBlock::ToolCall(call) = &message.content[0] else { panic!("{:?}", message.content) };
    assert_eq!(call.arguments["path"], "/src/file.ts");
}

#[tokio::test]
async fn custom_tool_input_streams_as_append_only_json_deltas() {
    let deltas: Vec<String> = (0..800).map(|index| format!("{:x<32}", format!("chunk {index} \"q\" \\ \n"))).collect();
    let input = format!("start:{}", deltas.concat());
    let item = json!({"type": "custom_tool_call", "call_id": "call_1", "id": "ctc_1", "name": "sample_tool", "input": "start:"});
    let mut frames = vec![frame(None, &json!({"type": "response.output_item.added", "output_index": 0, "item": item}))];
    for delta in &deltas {
        let event = json!({"type": "response.custom_tool_call_input.delta", "output_index": 0, "delta": delta});
        frames.push(frame(None, &event));
    }
    let done = json!({"type": "response.custom_tool_call_input.done", "output_index": 0, "input": input});
    frames.push(frame(None, &done));
    let mut item = item;
    item["input"] = json!(input);
    frames.push(frame(None, &json!({"type": "response.output_item.done", "output_index": 0, "item": item})));
    frames
        .push(frame(None, &json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}})));
    let server = MockServer::start(MockResponse::Sse(frames)).await;
    let model = test_model("openai-responses", "openai", &server.base_url);
    let mut stream = OpenAIResponses.stream(&model, &context(), keyed("sk-test"));
    let (mut json_text, mut shown, mut message) = (String::new(), 0, None);
    while let Some(event) = stream.next().await {
        if let Some(last) = event.final_message() {
            message = Some(last.clone());
        }
        let AssistantMessageEvent::ToolcallDelta { delta, partial, .. } = event else { continue };
        json_text.push_str(&delta);
        let ContentBlock::ToolCall(call) = &partial.content[0] else { panic!() };
        let partial = call.arguments["input"].as_str().unwrap();
        assert!(input.starts_with(partial) && partial.len() >= shown);
        shown = partial.len();
    }
    assert_eq!(serde_json::from_str::<Value>(&json_text).unwrap(), json!({"input": input}));
    let message = message.unwrap();
    assert_eq!(message.stop_reason, StopReason::ToolUse, "{:?}", message.error_message);
    assert_eq!(message.tool_calls().next().unwrap().arguments["input"], json!(input));
}
