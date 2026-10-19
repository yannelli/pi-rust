//! Transcript conversion, ported from upstream `transcript-tool-changes` (completions parts),
//! `openai-completions-tool-result-images`, `-thinking-as-text`, and replay cases of `-tool-choice`.

use serde_json::{Value, json};

use super::test_support::{
    API, assistant, capture, catalog, options, plain_catalog, system, tool, tool_call, tool_result, with_role,
};
use crate::providers::mock_server::test_model;
use crate::registry::StreamOptions;
use crate::types::{ContentBlock, Context, Message, Model, StopReason, ThinkingLevel, UserContent, UserMessage};

fn model(id: &str, provider: &str, compat: Value) -> Model {
    let mut model = test_model(API, provider, "http://127.0.0.1:9/v1");
    model.id = id.into();
    model.compat = Some(compat);
    model
}

/// A system update that removes `base_tool`, adds `late_tool`, and patches sections.
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

fn tool_names(tools: &Value) -> Vec<String> {
    let names = tools.as_array().into_iter().flatten();
    names.map(|tool| tool["function"]["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn kimi_additions_anchor_in_tool_bearing_system_messages() {
    let kimi = model(
        "kimi-k3",
        "moonshotai",
        json!({"supportsMidConvoSystemMessages": true, "supportsMidConvoToolAdditions": true}),
    );
    let body = capture(&kimi, &addition_context(), options()).await;
    assert_eq!(tool_names(&body["tools"]), ["base_tool"]);
    let anchored = body["messages"].as_array().unwrap().iter().find(|message| message.get("tools").is_some());
    assert_eq!(tool_names(&anchored.unwrap()["tools"]), ["late_tool"]);
    let contents: Vec<&Value> = with_role(&body, "system").into_iter().map(|message| &message["content"]).collect();
    assert_eq!(contents, [&json!("base prompt"), &Value::Null, &json!("updated guidance")]);
    assert!(anchored.unwrap().get("content").is_none());
}

#[tokio::test]
async fn kimi_k2_keeps_system_text_inline_without_tool_messages() {
    let kimi = model("kimi-k2.7-code", "moonshotai", json!({"supportsMidConvoSystemMessages": true}));
    let body = capture(&kimi, &addition_context(), options()).await;
    assert_eq!(tool_names(&body["tools"]), ["base_tool", "late_tool"]);
    assert!(!body["messages"].as_array().unwrap().iter().any(|message| message.get("tools").is_some()));
    let contents: Vec<&Value> = with_role(&body, "system").into_iter().map(|message| &message["content"]).collect();
    assert_eq!(contents, [&json!("base prompt"), &json!("updated guidance")]);
}

#[tokio::test]
async fn removals_render_framed_updates_with_the_current_tool_state() {
    let kimi = model(
        "kimi-k3",
        "moonshotai",
        json!({"supportsMidConvoSystemMessages": true, "supportsMidConvoToolAdditions": true}),
    );
    let body = capture(&kimi, &changing_context(), options()).await;
    assert_eq!(tool_names(&body["tools"]), ["late_tool"]);
    assert!(!body["messages"].as_array().unwrap().iter().any(|message| message.get("tools").is_some()));
    let update = with_role(&body, "system").last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(update.contains("Updated system prompt section \"rules\""), "{update}");
    assert!(update.contains("Removed system prompt section \"docs\"."), "{update}");
    let leading = with_role(&body, "system")[0]["content"].as_str().unwrap().to_string();
    assert_eq!(leading, "base prompt\n\n<rules>\nold rules\n</rules>\n\n<docs>\nread docs\n</docs>");
}

#[tokio::test]
async fn updates_fold_into_the_leading_message_without_native_support() {
    let mut custom = model("custom-model", "custom-provider", json!({}));
    custom.reasoning = false;
    let body = capture(&custom, &changing_context(), options()).await;
    assert_eq!(tool_names(&body["tools"]), ["late_tool"]);
    let roles: Vec<&str> = body["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user"]);
    assert_eq!(body["messages"][0]["content"], "base prompt\n\nupdated guidance\n\n<rules>\nnew rules\n</rules>");
}

fn image(data: &str) -> ContentBlock {
    ContentBlock::Image { data: data.into(), mime_type: "image/png".into() }
}

#[tokio::test]
async fn user_messages_drop_empty_text_parts() {
    let model = plain_catalog("openai", "gpt-4o-mini");
    let user = Message::User(UserMessage {
        content: UserContent::Blocks(vec![ContentBlock::text(""), image("ZmFrZQ==")]),
        timestamp: 1,
    });
    let body = capture(&model, &Context { messages: vec![user], ..Context::default() }, options()).await;
    let expected = json!([{"role": "user",
        "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,ZmFrZQ=="}}]}]);
    assert_eq!(body["messages"], expected);
}

#[tokio::test]
async fn tool_result_images_follow_consecutive_tool_results() {
    let model = plain_catalog("openai", "gpt-4o-mini");
    let calls = vec![
        ContentBlock::ToolCall(tool_call("tool-1", "read", json!({"path": "img-1.png"}))),
        ContentBlock::ToolCall(tool_call("tool-2", "read", json!({"path": "img-2.png"}))),
    ];
    let result =
        |id: &str| tool_result(id, "read", vec![ContentBlock::text("Read image file [image/png]"), image("ZmFrZQ==")]);
    let messages = vec![
        Message::user("Read the images"),
        assistant(&model, calls, StopReason::ToolUse),
        result("tool-1"),
        result("tool-2"),
    ];
    let body = capture(&model, &Context { messages, ..Context::default() }, options()).await;
    let messages = body["messages"].as_array().unwrap();
    let roles: Vec<&str> = messages.iter().map(|message| message["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "tool", "tool", "user"]);
    let parts = messages[4]["content"].as_array().unwrap();
    assert_eq!(parts.iter().filter(|part| part["type"] == "image_url").count(), 2);
}

#[tokio::test]
async fn empty_tool_results_use_a_placeholder_and_bridges_follow_tool_results() {
    let mut model = plain_catalog("openai", "gpt-4o-mini");
    model.compat = Some(json!({"requiresAssistantAfterToolResult": true, "requiresToolResultName": true}));
    let call = ContentBlock::ToolCall(tool_call("tool-1", "bash", json!({"command": "true"})));
    let messages = vec![
        Message::user("Run the command"),
        assistant(&model, vec![call], StopReason::ToolUse),
        tool_result("tool-1", "bash", vec![ContentBlock::text("")]),
        Message::user("next"),
    ];
    let body = capture(&model, &Context { messages, ..Context::default() }, options()).await;
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[1]["content"], "", "assistant content defaults to an empty string");
    assert_eq!(
        messages[2],
        json!({"role": "tool", "content": "(no tool output)", "tool_call_id": "tool-1", "name": "bash"})
    );
    assert_eq!(messages[3], json!({"role": "assistant", "content": "I have processed the tool results."}));
    assert_eq!(messages[4]["role"], "user");
}

fn thinking(text: &str, signature: Option<&str>) -> ContentBlock {
    ContentBlock::Thinking { thinking: text.into(), thinking_signature: signature.map(str::to_string), redacted: false }
}

#[tokio::test]
async fn thinking_as_text_replays_reasoning_as_text_parts() {
    let model = model("repro-model", "repro-provider", json!({"requiresThinkingAsText": true}));
    let replay = |content: Vec<ContentBlock>| Context {
        messages: vec![Message::user("hello"), assistant(&model, content, StopReason::Stop), Message::user("continue")],
        ..Context::default()
    };
    let both = replay(vec![thinking("internal reasoning", None), ContentBlock::text("visible answer")]);
    let body = capture(&model, &both, options()).await;
    let expected = json!({"role": "assistant", "content": [
        {"type": "text", "text": "internal reasoning"}, {"type": "text", "text": "visible answer"}]});
    assert_eq!(body["messages"][1], expected);
    let body = capture(&model, &replay(vec![thinking("internal reasoning", None)]), options()).await;
    let expected = json!({"role": "assistant", "content": [{"type": "text", "text": "internal reasoning"}]});
    assert_eq!(body["messages"][1], expected);
}

fn tool_turn(model: &Model, content: Vec<ContentBlock>) -> Context {
    let messages = vec![
        Message::user("Read README.md"),
        assistant(model, content, StopReason::ToolUse),
        tool_result("call_1", "read", vec![ContentBlock::text("contents")]),
        Message::user("Continue"),
    ];
    Context { messages, ..Context::default() }
}

#[tokio::test]
async fn reasoning_fields_replay_on_assistant_messages() {
    let call = || ContentBlock::ToolCall(tool_call("call_1", "read", json!({"path": "README.md"})));
    let high = || StreamOptions { reasoning: Some(ThinkingLevel::High), ..options() };
    let zai = catalog("zai", "glm-5.2");
    let context = tool_turn(&zai, vec![thinking("prior reasoning", Some("reasoning_content")), call()]);
    let body = capture(&zai, &context, high()).await;
    assert_eq!(with_role(&body, "assistant")[0]["reasoning_content"], "prior reasoning");
    assert_eq!(body["thinking"], json!({"type": "enabled", "clear_thinking": false}));
    let mimo = catalog("xiaomi", "mimo-v2.6-pro");
    let body = capture(&mimo, &tool_turn(&mimo, vec![call()]), high()).await;
    assert_eq!(with_role(&body, "assistant")[0]["reasoning_content"], "");
    let go = plain_catalog("opencode-go", "kimi-k3");
    let body = capture(&go, &tool_turn(&go, vec![thinking("think", Some("reasoning")), call()]), options()).await;
    let replayed = with_role(&body, "assistant")[0];
    assert_eq!(replayed["reasoning_content"], "think");
    assert!(replayed.get("reasoning").is_none());
}

#[tokio::test]
async fn reasoning_details_replay_instead_of_raw_reasoning_fields() {
    let model = catalog("openrouter", "openai/gpt-5.2-codex");
    let detail = json!({"type": "reasoning.encrypted", "id": "rs_1", "data": "opaque"});
    let signature = Value::Array(vec![detail.clone()]).to_string();
    let call = ContentBlock::ToolCall(tool_call("call_1", "read", json!({"path": "README.md"})));
    let body = capture(&model, &tool_turn(&model, vec![thinking("summary", Some(&signature)), call]), options()).await;
    let replayed = with_role(&body, "assistant")[0];
    assert_eq!(replayed["reasoning_details"], json!([detail]));
    assert!(replayed.get("reasoning").is_none());
    let mut legacy = tool_call("call_1", "read", json!({"path": "README.md"}));
    legacy.thought_signature = Some(detail.to_string());
    let context = tool_turn(&model, vec![ContentBlock::ToolCall(legacy)]);
    let body = capture(&model, &context, options()).await;
    assert_eq!(with_role(&body, "assistant")[0]["reasoning_details"], json!([detail]));
}

#[tokio::test]
async fn responses_tool_call_ids_are_shortened_for_chat_completions() {
    let model = plain_catalog("openai", "gpt-4o-mini");
    let long_id = format!("call_abc|{}", "x+/=".repeat(100));
    let call = ContentBlock::ToolCall(tool_call(&long_id, "read", json!({})));
    let mut source = assistant(&model, vec![call], StopReason::ToolUse);
    if let Message::Assistant(message) = &mut source {
        message.api = "openai-responses".into();
    }
    let messages = vec![Message::user("go"), source, tool_result(&long_id, "read", vec![ContentBlock::text("ok")])];
    let body = capture(&model, &Context { messages, ..Context::default() }, options()).await;
    let id = with_role(&body, "assistant")[0]["tool_calls"][0]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("call_abc_") && id.chars().count() <= 40, "{id}");
    assert_eq!(with_role(&body, "tool")[0]["tool_call_id"], id.as_str());
}
