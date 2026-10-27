//! `stream_proxy`, ported from upstream `proxy.test.ts`, against a local HTTP server.

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use pi_agent::{ProxyStreamOptions, stream_proxy};
use pi_ai::{AssistantMessageEvent, ContentBlock, Context, Message, StopReason, ThinkingLevel};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one response and keep the request (head and body) it answered.
async fn serve(status: &'static str, body: String) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let request = Arc::new(Mutex::new(String::new()));
    let seen = request.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            received.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&received).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let length = text[..end]
                    .lines()
                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length: ").map(String::from))
                    .and_then(|length| length.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if received.len() >= end + 4 + length || read == 0 {
                    break;
                }
            }
        }
        *seen.lock().unwrap() = String::from_utf8_lossy(&received).to_string();
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    (url, request)
}

fn usage() -> Value {
    json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
}

fn frames(events: &[Value]) -> String {
    events.iter().map(|event| format!("data: {event}\n\n")).collect()
}

fn options(url: String) -> ProxyStreamOptions {
    ProxyStreamOptions { proxy_url: url, auth_token: "test-token".into(), ..Default::default() }
}

async fn collect(url: String, context: &Context) -> (Vec<AssistantMessageEvent>, pi_ai::AssistantMessage) {
    let model = pi_ai::providers::faux::FauxProvider::model();
    let mut stream = stream_proxy(&model, context, options(url));
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let last = events.last().and_then(AssistantMessageEvent::final_message).cloned().expect("a final event");
    (events, last)
}

fn kind(event: &AssistantMessageEvent) -> String {
    serde_json::to_value(event).unwrap()["type"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn preserves_tool_call_metadata_received_only_on_toolcall_end() {
    let body = frames(&[
        json!({"type": "start"}),
        json!({"type": "toolcall_start", "contentIndex": 0, "id": "call_test|fc_test", "toolName": "lookup"}),
        json!({"type": "toolcall_delta", "contentIndex": 0, "delta": "{\"value\":\"hello\"}"}),
        json!({"type": "toolcall_end", "contentIndex": 0, "toolCall": {"type": "toolCall", "id": "call_test|fc_test", "name": "lookup", "arguments": {"value": "hello"}, "namespace": "dynamic_tools"}}),
        json!({"type": "done", "reason": "toolUse", "usage": usage()}),
    ]);
    let (url, request) = serve("200 OK", body).await;
    let context =
        Context { system_prompt: Some("sys".into()), messages: vec![Message::user("hi")], ..Default::default() };
    let (events, result) = collect(url, &context).await;
    let end = events.iter().find_map(|event| match event {
        AssistantMessageEvent::ToolcallEnd { tool_call, .. } => Some(tool_call.clone()),
        _ => None,
    });
    assert_eq!(end.unwrap().namespace.as_deref(), Some("dynamic_tools"));
    let ContentBlock::ToolCall(call) = &result.content[0] else { panic!("expected a tool call") };
    assert_eq!((call.arguments["value"].as_str(), call.namespace.as_deref()), (Some("hello"), Some("dynamic_tools")));
    assert_eq!(result.stop_reason, StopReason::ToolUse);

    let request = request.lock().unwrap().clone();
    assert!(request.starts_with("POST /api/stream "), "{request}");
    assert!(request.to_ascii_lowercase().contains("authorization: bearer test-token"));
    let body: Value = serde_json::from_str(&request[request.find("\r\n\r\n").unwrap() + 4..]).unwrap();
    assert_eq!(body["model"]["id"], "faux-model");
    // The proxy gets a transcript: the prompt travels as the leading system message.
    assert_eq!(body["context"]["messages"][0]["role"], "system");
    assert_eq!(body["context"]["messages"][1]["role"], "user");
    assert_eq!(body["options"], json!({}));
}

#[tokio::test]
async fn processes_terminal_metadata_without_a_trailing_newline() {
    let start = format!("data: {}\n\n", json!({"type": "start"}));
    let done = format!(
        "data: {}",
        json!({"type": "done", "reason": "stop", "usage": usage(), "providerThinkingLevel": "high"})
    );
    let (url, _) = serve("200 OK", start + &done).await;
    let (events, result) = collect(url, &Context::default()).await;
    assert_eq!(events.iter().map(kind).collect::<Vec<_>>(), ["start", "done"]);
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!(result.provider_thinking_level.as_deref(), Some("high"));
}

#[tokio::test]
async fn reports_an_error_when_the_stream_ends_without_a_terminal_event() {
    let (url, _) = serve("200 OK", frames(&[json!({"type": "start"})])).await;
    let (events, result) = collect(url, &Context::default()).await;
    assert_eq!(events.iter().map(kind).collect::<Vec<_>>(), ["start", "error"]);
    assert_eq!(result.stop_reason, StopReason::Error);
    assert!(result.error_message.unwrap().contains("Connection closed by proxy server"));
}

#[tokio::test]
async fn rebuilds_text_and_thinking_and_reports_proxy_errors() {
    let body = frames(&[
        json!({"type": "start"}),
        json!({"type": "thinking_start", "contentIndex": 0}),
        json!({"type": "thinking_delta", "contentIndex": 0, "delta": "hm"}),
        json!({"type": "thinking_end", "contentIndex": 0, "contentSignature": "sig"}),
        json!({"type": "text_start", "contentIndex": 1}),
        json!({"type": "text_delta", "contentIndex": 1, "delta": "Hel"}),
        json!({"type": "text_delta", "contentIndex": 1, "delta": "lo"}),
        json!({"type": "text_end", "contentIndex": 1}),
        json!({"type": "done", "reason": "stop", "usage": usage()}),
    ]);
    let (url, _) = serve("200 OK", body).await;
    let (_, result) = collect(url, &Context::default()).await;
    assert_eq!(result.text(), "Hello");
    assert!(
        matches!(&result.content[0], ContentBlock::Thinking { thinking, thinking_signature: Some(sig), .. } if thinking == "hm" && sig == "sig")
    );

    let (url, _) = serve("401 Unauthorized", json!({"error": "bad token"}).to_string()).await;
    let (_, result) = collect(url, &Context::default()).await;
    assert_eq!(result.error_message.as_deref(), Some("Proxy error: bad token"));
    let (url, _) = serve("502 Bad Gateway", "not json".into()).await;
    let (_, result) = collect(url, &Context::default()).await;
    assert_eq!(result.error_message.as_deref(), Some("Proxy error: 502 Bad Gateway"));
    let (url, _) = serve(
        "200 OK",
        frames(&[json!({"type": "start"}), json!({"type": "text_delta", "contentIndex": 0, "delta": "x"})]),
    )
    .await;
    let (_, result) = collect(url, &Context::default()).await;
    assert_eq!(result.error_message.as_deref(), Some("Received text_delta for non-text content"));
}

#[tokio::test]
async fn forwards_request_options() {
    let (url, request) = serve("200 OK", frames(&[json!({"type": "done", "reason": "stop", "usage": usage()})])).await;
    let model = pi_ai::providers::faux::FauxProvider::model();
    let options = ProxyStreamOptions {
        max_tokens: Some(100),
        reasoning: Some(ThinkingLevel::High),
        session_id: Some("s1".into()),
        ..options(url)
    };
    let mut stream = stream_proxy(&model, &Context::default(), options);
    while stream.next().await.is_some() {}
    let request = request.lock().unwrap().clone();
    let body: Value = serde_json::from_str(&request[request.find("\r\n\r\n").unwrap() + 4..]).unwrap();
    assert_eq!(body["options"], json!({"maxTokens": 100, "reasoning": "high", "sessionId": "s1"}));
}

#[tokio::test]
async fn cancellation_ends_the_stream_as_aborted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let model = pi_ai::providers::faux::FauxProvider::model();
    let mut stream =
        stream_proxy(&model, &Context::default(), ProxyStreamOptions { cancel: cancel.clone(), ..options(url) });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = event.final_message().cloned();
    }
    let last = last.unwrap();
    assert_eq!(
        (last.stop_reason, last.error_message.as_deref()),
        (StopReason::Aborted, Some("Request aborted by user"))
    );
}

// #9265: re-parsing the accumulated arguments on every delta made a long tool call quadratic.
#[tokio::test]
async fn shows_growing_partial_arguments_without_keeping_the_raw_text_on_the_block() {
    let content = "line \"quoted\" \\ end\n".repeat(20_000);
    let text = json!({"path": "a.ts", "content": content}).to_string();
    let mut events = vec![
        json!({"type": "start"}),
        json!({"type": "toolcall_start", "contentIndex": 0, "id": "call_1", "toolName": "write"}),
    ];
    for start in (0..text.len()).step_by(32) {
        let delta = &text[start..text.len().min(start + 32)];
        events.push(json!({"type": "toolcall_delta", "contentIndex": 0, "delta": delta}));
    }
    let arguments = json!({"path": "a.ts", "content": content});
    events.push(json!({"type": "toolcall_end", "contentIndex": 0, "toolCall": {"type": "toolCall", "id": "call_1", "name": "write", "arguments": arguments}}));
    events.push(json!({"type": "done", "reason": "toolUse", "usage": usage()}));
    let (url, _) = serve("200 OK", frames(&events)).await;

    let started = std::time::Instant::now();
    let model = pi_ai::providers::faux::FauxProvider::model();
    let mut stream = stream_proxy(&model, &Context::default(), options(url));
    let (mut shown, mut deltas, mut result) = (0, 0, None);
    while let Some(event) = stream.next().await {
        if let Some(message) = event.final_message() {
            result = Some(message.clone());
        }
        let AssistantMessageEvent::ToolcallDelta { partial, .. } = event else { continue };
        let block = serde_json::to_value(&partial.content[0]).unwrap();
        let keys: Vec<&String> = block.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["type", "id", "name", "arguments"]);
        let Some(partial) = block["arguments"]["content"].as_str() else { continue };
        // Comparing the whole text on every delta would make this test itself quadratic.
        if deltas % 500 == 0 {
            assert!(content.starts_with(partial));
        }
        deltas += 1;
        assert!(partial.len() >= shown);
        shown = partial.len();
    }
    let result = result.expect("a final event");
    assert!(shown > content.len() - 64);
    let ContentBlock::ToolCall(call) = &result.content[0] else { panic!("expected a tool call") };
    assert_eq!(serde_json::Value::Object(call.arguments.clone()), arguments);
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "took {:?}", started.elapsed());
}
