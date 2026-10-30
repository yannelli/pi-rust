//! The provider against an in-process ACP agent that calls pi's tools through the real bridge.

use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::runtime::{PassthroughMcpServer, redirect_target, set_passthrough_mcp_servers, set_test_agent};
use super::{API, AntigravityAcp};
use crate::providers::acp::connection::Connection;
use crate::providers::mock_server::{collect, kinds, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason, ThinkingLevel, Tool, ToolResultMessage,
};

/// Requests the agent received: method and params.
static SEEN: LazyLock<Mutex<Vec<(String, Value)>>> = LazyLock::new(Mutex::default);
/// The tests share the agent factory and the request log.
static SERIAL: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(tokio::sync::Mutex::default);

fn config(model: &str, thought: &str) -> Value {
    json!([
        {"id": "model", "category": "model", "type": "select", "currentValue": model,
         "options": [{"value": "gemini-3.8-flash", "name": "Flash"}, {"value": "gemini-3.1-pro", "name": "Pro"}]},
        {"id": "thought", "category": "thought_level", "type": "select", "currentValue": thought,
         "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
    ])
}

/// Calls `tool` on the bridge named in `session/new` and returns the result text.
async fn call_bridge(session_new: &Value, tool: &str) -> String {
    let bridge =
        session_new["mcpServers"].as_array().unwrap().iter().find(|server| server["name"] == "pi-bridge").unwrap();
    let request = json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": tool, "arguments": {"path": "a"}}});
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(bridge["url"].as_str().unwrap())
        .header("authorization", bridge["headers"][0]["value"].as_str().unwrap())
        .json(&request)
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    let data: Value = serde_json::from_str(body.lines().find_map(|line| line.strip_prefix("data: ")).unwrap()).unwrap();
    data["result"]["content"][0]["text"].as_str().unwrap().to_string()
}

async fn agent(stream: tokio::io::DuplexStream) {
    let (read, write) = tokio::io::split(stream);
    let write = Arc::new(tokio::sync::Mutex::new(write));
    let send = move |message: Value| {
        let write = write.clone();
        async move { write.lock().await.write_all(format!("{message}\n").as_bytes()).await.unwrap() }
    };
    let update = |update: Value| json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": update}});
    let mut lines = BufReader::new(read).lines();
    let (mut session_new, mut current) = (Value::Null, ("gemini-3.8-flash".to_string(), "low".to_string()));
    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).unwrap();
        let (Some(method), id) = (message["method"].as_str(), message["id"].clone()) else { continue };
        SEEN.lock().unwrap().push((method.to_string(), message["params"].clone()));
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
        match method {
            "initialize" => {
                send(reply(json!({"protocolVersion": 1,
                "agentInfo": {"name": "antigravity-acp", "version": "1.4.3"},
                "authMethods": [{"id": "oauth-personal", "name": "Log in with Google"}],
                "agentCapabilities": {"promptCapabilities": {"embeddedContext": true, "image": true},
                    "mcpCapabilities": {"http": true}, "_meta": {"antigravity": {"systemPrompt": {"append": true}}}}})))
                .await
            }
            "session/new" => {
                session_new = message["params"].clone();
                send(reply(json!({"sessionId": "s1", "configOptions": config(&current.0, &current.1)}))).await;
            }
            "session/set_config_option" => {
                let value = message["params"]["value"].as_str().unwrap().to_string();
                if message["params"]["configId"] == "model" {
                    current.0 = value
                } else {
                    current.1 = value
                }
                send(reply(json!({"configOptions": config(&current.0, &current.1)}))).await;
            }
            "session/prompt" => {
                let text = message["params"]["prompt"].to_string();
                if text.contains("use the tool") {
                    send(update(
                        json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "pi-bridge/pi_read"}),
                    ))
                    .await;
                    let (session, id, send) = (session_new.clone(), id.clone(), send.clone());
                    tokio::spawn(async move {
                        let result = call_bridge(&session, "pi_read").await;
                        let chunk = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("read: {result}")}});
                        send(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": chunk}})).await;
                        send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}})).await;
                    });
                    continue;
                }
                send(update(
                    json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "Plan"}}),
                ))
                .await;
                let permission = json!({"jsonrpc": "2.0", "id": "p1", "method": "session/request_permission", "params": {"sessionId": "s1",
                    "options": [{"optionId": "r1", "kind": "reject_once"}, {"optionId": "a1", "kind": "allow_once"}]}});
                send(permission).await;
                send(update(
                    json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hello"}}),
                ))
                .await;
                send(update(json!({"sessionUpdate": "usage_update", "used": 777, "size": 1_000_000}))).await;
                send(reply(json!({"stopReason": "end_turn", "usage": {"inputTokens": 50, "outputTokens": 5, "totalTokens": 55}}))).await;
            }
            _ => {}
        }
    }
}

fn install_agent() {
    set_test_agent(Some(Arc::new(|| {
        let (client, agent_side) = tokio::io::duplex(1 << 20);
        tokio::spawn(agent(agent_side));
        let (read, write) = tokio::io::split(client);
        Connection::new(super::LABEL, read, write)
    })));
}

fn model(id: &str) -> Model {
    let mut model = test_model(API, "google-antigravity", "acp://antigravity");
    model.id = id.into();
    model.thinking_level_map =
        Some([("off".to_string(), None), ("minimal".to_string(), None), ("medium".to_string(), None)].into());
    model
}

fn options(session: &str) -> StreamOptions {
    let api_key = Some("antigravity-acp".into());
    StreamOptions {
        api_key,
        session_id: Some(session.into()),
        reasoning: Some(ThinkingLevel::High),
        ..StreamOptions::default()
    }
}

fn read_tool() -> Tool {
    Tool::new("read", "Read a file", json!({"type": "object", "properties": {"path": {"type": "string"}}}))
}

#[tokio::test]
async fn streams_a_turn_with_model_effort_usage_and_permissions() {
    let _serial = SERIAL.lock().await;
    install_agent();
    SEEN.lock().unwrap().clear();
    let context = Context {
        system_prompt: Some("Be brief".into()),
        messages: vec![Message::user("hi")],
        tools: vec![read_tool()],
    };
    let (events, message) = collect(AntigravityAcp.stream(&model("gemini-3.1-pro"), &context, options("plain"))).await;
    assert_eq!(message.error_message, None);
    assert_eq!(
        kinds(&events),
        ["start", "thinking_start", "thinking_delta", "thinking_end", "text_start", "text_delta", "text_end", "done"]
    );
    assert_eq!((message.stop_reason, message.usage.input, message.usage.total_tokens), (StopReason::Stop, 50, 777));
    let seen = SEEN.lock().unwrap().clone();
    let new = &seen.iter().find(|(method, _)| method == "session/new").unwrap().1;
    assert_eq!(new["_meta"]["antigravity"]["enabledTools"], json!([]));
    assert!(new["_meta"]["antigravity"]["systemPrompt"]["append"].as_str().unwrap().starts_with("Be brief"));
    let set: Vec<&Value> = seen
        .iter()
        .filter(|(method, _)| method == "session/set_config_option")
        .map(|(_, params)| &params["value"])
        .collect();
    assert_eq!(set, [&json!("gemini-3.1-pro"), &json!("high")]);
    let prompt = &seen.iter().find(|(method, _)| method == "session/prompt").unwrap().1["prompt"];
    assert_eq!(prompt, &json!([{"type": "text", "text": "\n\nhi"}]));
}

#[tokio::test]
async fn pi_runs_bridged_tool_calls_and_the_turn_continues() {
    let _serial = SERIAL.lock().await;
    install_agent();
    SEEN.lock().unwrap().clear();
    let passthrough = PassthroughMcpServer {
        acp: json!({"name": "docs", "command": "docs-mcp", "args": [], "env": []}),
        tool_prefix: "mcp__docs__".into(),
    };
    set_passthrough_mcp_servers(vec![passthrough]);
    let docs_tool = Tool::new("mcp__docs__search", "Search docs", json!({"type": "object"}));
    let mut context = Context {
        system_prompt: None,
        messages: vec![Message::user("use the tool")],
        tools: vec![read_tool(), docs_tool],
    };
    let model = model("gemini-3.8-flash");
    let (_, first) = collect(AntigravityAcp.stream(&model, &context, options("tools"))).await;
    set_passthrough_mcp_servers(Vec::new());
    assert_eq!((first.stop_reason, first.error_message.as_deref()), (StopReason::ToolUse, None));
    let ContentBlock::ToolCall(call) = &first.content[0] else { panic!("tool call: {:?}", first.content) };
    assert_eq!((call.name.as_str(), &call.arguments["path"]), ("read", &json!("a")));
    let result = ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: "read".into(),
        content: vec![ContentBlock::text("file body")],
        ..ToolResultMessage::default()
    };
    context.messages.push(Message::Assistant(AssistantMessage { api: API.into(), ..first.clone() }));
    context.messages.push(Message::ToolResult(result));
    let (_, second) = collect(AntigravityAcp.stream(&model, &context, options("tools"))).await;
    assert_eq!((second.stop_reason, second.error_message.as_deref()), (StopReason::Stop, None));
    assert_eq!(second.content.iter().filter_map(ContentBlock::as_text).collect::<String>(), "read: file body");
    let seen = SEEN.lock().unwrap().clone();
    let new = &seen.iter().rev().find(|(method, _)| method == "session/new").unwrap().1;
    assert_eq!(new["mcpServers"][1]["name"], "docs");
    assert_eq!(seen.iter().filter(|(method, _)| method == "session/prompt").count(), 1);
}

#[tokio::test]
async fn a_missing_login_or_server_fails_the_stream() {
    let _serial = SERIAL.lock().await;
    let context = Context { messages: vec![Message::user("hi")], ..Context::default() };
    let no_key = StreamOptions::default();
    let (_, message) = collect(AntigravityAcp.stream(&model("gemini-3.1-pro"), &context, no_key)).await;
    assert!(message.error_message.unwrap().contains("/login google-antigravity"));
}

#[test]
fn a_pasted_redirect_goes_to_the_loopback_port_of_the_sign_in_url() {
    let sign_in =
        "https://accounts.google.com/o/oauth2/auth?client_id=x&redirect_uri=http%3A%2F%2F127.0.0.1%3A51234%2F&state=s";
    let target = redirect_target(sign_in, " http://localhost:9/?state=s&code=4%2Fabc&scope=email#x ").unwrap();
    assert_eq!(target.as_str(), "http://127.0.0.1:51234/?state=s&code=4%2Fabc&scope=email");
    assert_eq!(redirect_target(sign_in, "state=s&code=c").unwrap().as_str(), "http://127.0.0.1:51234/?state=s&code=c");
    assert!(redirect_target(sign_in, "4/abc").is_err());
    assert!(redirect_target("https://accounts.google.com/o/oauth2/auth", "code=c").is_err());
}
