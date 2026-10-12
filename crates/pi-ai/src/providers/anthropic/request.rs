//! Request building for `anthropic-messages`: message conversion, prompt
//! caching, thinking configuration, tools, and beta features.

use serde_json::{Map, Value, json};

use crate::providers::common::{
    MIN_ANSWER_TOKENS, compat_bool, has_images, long_cache_retention, reasoning_level, sanitize_id, take_chars,
    thinking_budget, transform_messages,
};
use crate::registry::StreamOptions;
use crate::types::{ContentBlock, Context, Message, Model, ThinkingLevel, Tool, ToolResultMessage, UserContent};

pub(super) const CLAUDE_CODE_VERSION: &str = "2.1.280";
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const CLAUDE_CODE_TOOLS: [&str; 17] = [
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

/// Claude Code canonical casing for OAuth requests.
fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|tool| tool.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_string(), |tool| tool.to_string())
}

/// Map a streamed tool name back to the declared tool name.
pub(super) fn from_claude_code_name(name: &str, tools: &[String]) -> String {
    tools.iter().find(|tool| tool.eq_ignore_ascii_case(name)).cloned().unwrap_or_else(|| name.to_string())
}

pub(super) struct AnthropicRequest {
    pub body: Value,
    pub betas: Vec<&'static str>,
}

pub(super) fn build_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    oauth: bool,
) -> AnthropicRequest {
    let long_cache = long_cache_retention() && compat_bool(model, "supportsLongCacheRetention").unwrap_or(true);
    let cache_control =
        if long_cache { json!({"type": "ephemeral", "ttl": "1h"}) } else { json!({"type": "ephemeral"}) };
    let eager_tool_streaming = compat_bool(model, "supportsEagerToolInputStreaming").unwrap_or(true);
    let allow_empty_signature = compat_bool(model, "allowEmptySignature").unwrap_or(false);
    let adaptive = compat_bool(model, "forceAdaptiveThinking") == Some(true);
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert(
        "messages".into(),
        Value::Array(convert_messages(model, context, oauth, allow_empty_signature, &cache_control)),
    );
    let mut max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let level = reasoning_level(model, options);
    let mut betas = Vec::new();
    if oauth {
        betas.extend(["claude-code-20250219", "oauth-2025-04-20"]);
    }
    if !context.tools.is_empty() && !eager_tool_streaming {
        betas.push("fine-grained-tool-streaming-2025-05-14");
    }
    if model.reasoning {
        let thinking = match level {
            Some(level) if adaptive => {
                body.insert("output_config".into(), json!({"effort": adaptive_effort(level)}));
                json!({"type": "adaptive", "display": "summarized"})
            }
            Some(level) => {
                let (adjusted, budget) = budget_thinking(level, max_tokens, model.max_tokens);
                max_tokens = adjusted;
                betas.push("interleaved-thinking-2025-05-14");
                json!({"type": "enabled", "budget_tokens": budget, "display": "summarized"})
            }
            None => json!({"type": "disabled"}),
        };
        body.insert("thinking".into(), thinking);
    }
    body.insert("max_tokens".into(), json!(max_tokens));
    body.insert("stream".into(), json!(true));
    let mut system = Vec::new();
    if oauth {
        system.push(json!({"type": "text", "text": CLAUDE_CODE_IDENTITY, "cache_control": cache_control}));
    }
    if let Some(prompt) = context.system_prompt.as_deref().filter(|prompt| !prompt.is_empty()) {
        system.push(json!({"type": "text", "text": prompt, "cache_control": cache_control}));
    }
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    if let Some(temperature) = options.temperature
        && level.is_none()
        && compat_bool(model, "supportsTemperature").unwrap_or(true)
    {
        body.insert("temperature".into(), json!(temperature));
    }
    if !context.tools.is_empty() {
        let tool_cache = compat_bool(model, "supportsCacheControlOnTools").unwrap_or(true).then_some(&cache_control);
        let last = context.tools.len() - 1;
        let tools: Vec<Value> = context
            .tools
            .iter()
            .enumerate()
            .map(|(index, tool)| convert_tool(tool, oauth, eager_tool_streaming, tool_cache.filter(|_| index == last)))
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    AnthropicRequest { body: Value::Object(body), betas }
}

fn adaptive_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
}

/// `(max_tokens, budget_tokens)`: the budget is added on top of the requested
/// output cap, limited by the model cap, and always leaves room for the answer.
fn budget_thinking(level: ThinkingLevel, requested: u64, model_max: u64) -> (u64, u64) {
    let budget = thinking_budget(level);
    let max_tokens = if model_max > 0 { (requested + budget).min(model_max) } else { requested + budget };
    let budget = budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS));
    (max_tokens, if budget == 0 { 1024 } else { budget })
}

fn convert_tool(tool: &Tool, oauth: bool, eager: bool, cache_control: Option<&Value>) -> Value {
    let name = if oauth { to_claude_code_name(&tool.name) } else { tool.name.clone() };
    let properties = tool.parameters.get("properties").cloned().unwrap_or_else(|| json!({}));
    let required = tool.parameters.get("required").cloned().unwrap_or_else(|| json!([]));
    let mut value = json!({"name": name, "description": tool.description});
    if eager {
        value["eager_input_streaming"] = json!(true);
    }
    value["input_schema"] = json!({"type": "object", "properties": properties, "required": required});
    if let Some(cache_control) = cache_control {
        value["cache_control"] = cache_control.clone();
    }
    value
}

fn content_blocks(content: &[ContentBlock]) -> Value {
    if !has_images(content) {
        return json!(content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n"));
    }
    let mut blocks: Vec<Value> = content.iter().filter_map(block_param).collect();
    if !content.iter().any(|block| block.as_text().is_some()) {
        blocks.insert(0, json!({"type": "text", "text": "(see attached image)"}));
    }
    Value::Array(blocks)
}

fn block_param(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text, .. } => Some(json!({"type": "text", "text": text})),
        ContentBlock::Image { data, mime_type } => {
            Some(json!({"type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data}}))
        }
        _ => None,
    }
}

fn tool_result(result: &ToolResultMessage) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": result.tool_call_id,
        "content": content_blocks(&result.content),
        "is_error": result.is_error,
    })
}

fn convert_messages(
    model: &Model,
    context: &Context,
    oauth: bool,
    allow_empty_signature: bool,
    cache_control: &Value,
) -> Vec<Value> {
    let messages = transform_messages(&context.messages, model, &|id, _| take_chars(&sanitize_id(id), 64));
    let mut params: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User(user) => match &user.content {
                UserContent::Text(text) if !text.trim().is_empty() => {
                    params.push(json!({"role": "user", "content": text}))
                }
                UserContent::Text(_) => {}
                UserContent::Blocks(blocks) => {
                    let blocks: Vec<Value> = blocks
                        .iter()
                        .filter(|block| block.as_text().is_none_or(|text| !text.trim().is_empty()))
                        .filter_map(block_param)
                        .collect();
                    if !blocks.is_empty() {
                        params.push(json!({"role": "user", "content": blocks}));
                    }
                }
            },
            Message::Assistant(assistant) => {
                let blocks: Vec<Value> = assistant
                    .content
                    .iter()
                    .filter_map(|block| assistant_block(block, oauth, allow_empty_signature))
                    .collect();
                if !blocks.is_empty() {
                    params.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Message::System(_) => {}
            Message::ToolResult(_) => {
                let mut results = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(index) {
                    results.push(tool_result(result));
                    index += 1;
                }
                params.push(json!({"role": "user", "content": results}));
                continue;
            }
        }
        index += 1;
    }
    if let Some(last) = params.last_mut().filter(|last| last["role"] == "user") {
        match &mut last["content"] {
            Value::String(text) => {
                let text = std::mem::take(text);
                last["content"] = json!([{"type": "text", "text": text, "cache_control": cache_control}]);
            }
            Value::Array(blocks) => {
                if let Some(block) = blocks
                    .last_mut()
                    .filter(|block| matches!(block["type"].as_str(), Some("text" | "image" | "tool_result")))
                {
                    block["cache_control"] = cache_control.clone();
                }
            }
            _ => {}
        }
    }
    params
}

fn assistant_block(block: &ContentBlock, oauth: bool, allow_empty_signature: bool) -> Option<Value> {
    match block {
        ContentBlock::Text { text, .. } if !text.trim().is_empty() => Some(json!({"type": "text", "text": text})),
        ContentBlock::Thinking { thinking_signature, redacted: true, .. } => {
            Some(json!({"type": "redacted_thinking", "data": thinking_signature.clone().unwrap_or_default()}))
        }
        ContentBlock::Thinking { thinking, thinking_signature, .. } => {
            let signature = thinking_signature.as_deref().filter(|signature| !signature.trim().is_empty());
            match signature {
                Some(signature) => Some(json!({"type": "thinking", "thinking": thinking, "signature": signature})),
                None if thinking.trim().is_empty() => None,
                None if allow_empty_signature => {
                    Some(json!({"type": "thinking", "thinking": thinking, "signature": ""}))
                }
                None => Some(json!({"type": "text", "text": thinking})),
            }
        }
        ContentBlock::ToolCall(call) => {
            let name = if oauth { to_claude_code_name(&call.name) } else { call.name.clone() };
            Some(json!({"type": "tool_use", "id": call.id, "name": name, "input": call.arguments}))
        }
        _ => None,
    }
}
