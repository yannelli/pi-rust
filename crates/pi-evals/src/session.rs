//! Reads a pi session file into messages, transcript events, and usage.

use std::path::Path;

use pi_ai::{ContentBlock, Message, ModelRegistry, UserContent};
use serde_json::{Value, json};

use crate::fsutil::json_number;

pub struct SessionSnapshot {
    pub session_id: Option<String>,
    pub messages: Vec<Message>,
}

/// Concatenated text blocks, as upstream `contentText`.
pub fn content_text(content: &[ContentBlock]) -> String {
    content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("")
}

fn user_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => content_text(blocks),
    }
}

pub fn read_session(path: &Path) -> anyhow::Result<SessionSnapshot> {
    let text = std::fs::read_to_string(path)?;
    let mut snapshot = SessionSnapshot { session_id: None, messages: Vec::new() };
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let entry: Value = serde_json::from_str(line)?;
        match entry["type"].as_str() {
            Some("session") => snapshot.session_id = entry["id"].as_str().map(str::to_string),
            Some("message") => {
                if let Ok(message) = serde_json::from_value::<Message>(entry["message"].clone()) {
                    snapshot.messages.push(message);
                }
            }
            _ => {}
        }
    }
    Ok(snapshot)
}

impl SessionSnapshot {
    /// vitest-evals transcript events: messages, tool calls, and tool results.
    pub fn events(&self) -> Vec<Value> {
        let mut events = Vec::new();
        for message in &self.messages {
            match message {
                Message::User(user) => {
                    events.push(json!({ "type": "message", "role": "user", "content": user_text(&user.content) }));
                }
                Message::Assistant(assistant) => {
                    let text = content_text(&assistant.content);
                    if !text.is_empty() {
                        events.push(json!({ "type": "message", "role": "assistant", "content": text }));
                    }
                    for block in &assistant.content {
                        if let ContentBlock::ToolCall(call) = block {
                            events.push(json!({
                                "type": "tool_call", "id": call.id, "name": call.name, "arguments": call.arguments,
                            }));
                        }
                    }
                }
                Message::ToolResult(result) => {
                    let text = content_text(&result.content);
                    let all_text = result.content.iter().all(|block| block.as_text().is_some());
                    let content = if all_text { json!(text) } else { json!(result.content) };
                    let mut event = json!({
                        "type": "tool_result", "toolCallId": result.tool_call_id, "name": result.tool_name,
                        "content": content,
                    });
                    if result.is_error {
                        let message = if text.is_empty() { "Tool failed".to_string() } else { text };
                        event["error"] = json!({ "message": message });
                    }
                    events.push(event);
                }
                _ => {}
            }
        }
        events
    }

    /// Session totals as a vitest-evals usage summary; the estimated cost is reported only
    /// when the model has pricing.
    pub fn usage(&self, agent_dir: &Path) -> Value {
        let (mut input, mut output, mut cache_read, mut cache_write, mut total, mut cost) = (0, 0, 0, 0, 0, 0.0);
        let mut tool_calls = 0;
        let mut model = None;
        for message in &self.messages {
            let Message::Assistant(assistant) = message else { continue };
            input += assistant.usage.input;
            output += assistant.usage.output;
            cache_read += assistant.usage.cache_read;
            cache_write += assistant.usage.cache_write;
            total += assistant.usage.total_tokens;
            cost += assistant.usage.cost.total;
            tool_calls += assistant.content.iter().filter(|block| matches!(block, ContentBlock::ToolCall(_))).count();
            model = Some((assistant.provider.clone(), assistant.model.clone()));
        }
        let Some((provider, id)) = model else { return json!({}) };
        let registry = ModelRegistry::load_lenient(&agent_dir.join("models.json")).0;
        let has_pricing = registry.find(&provider, &id).is_some_and(|model| {
            let price = &model.cost;
            price.input > 0.0 || price.output > 0.0 || price.cache_read > 0.0 || price.cache_write > 0.0
        });
        let mut metadata = json!({ "cacheReadTokens": cache_read, "cacheWriteTokens": cache_write });
        if has_pricing {
            metadata["estimatedCostUsd"] = json_number(cost);
        }
        json!({
            "provider": provider, "model": id, "inputTokens": input, "outputTokens": output,
            "totalTokens": total, "toolCalls": tool_calls, "metadata": metadata,
        })
    }
}
