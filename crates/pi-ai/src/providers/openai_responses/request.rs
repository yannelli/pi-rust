//! Request building for `openai-responses`: input item conversion and body parameters.

use serde_json::{Map, Value, json};

use crate::providers::common::{
    compat_bool, data_url, has_images, long_cache_retention, reasoning_level, sanitize_id, short_hash, take_chars,
    tool_result_text, transform_messages,
};
use crate::registry::StreamOptions;
use crate::types::{AssistantMessage, ContentBlock, Context, Message, Model, UserContent};

/// Providers whose `call_id|item_id` tool call ids are replayed with their item id.
const TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
/// OpenAI rejects `max_output_tokens` below 16.
const MIN_OUTPUT_TOKENS: u64 = 16;

pub(super) fn build_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let supports_long = compat_bool(model, "supportsLongCacheRetention").unwrap_or(true);
    let explicit_cache = compat_bool(model, "supportsExplicitPromptCacheMode").unwrap_or(false);
    let long_cache = long_cache_retention() && supports_long;
    let chatgpt_sign_in = model.provider == "openai"
        && model.base_url == "https://api.openai.com/v1"
        && options.api_key.as_deref().is_some_and(|key| !key.starts_with("sk-"));
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("input".into(), Value::Array(convert_messages(model, context)));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    if let Some(session) = &options.session_id {
        body.insert("prompt_cache_key".into(), json!(take_chars(session, 64)));
    }
    if long_cache && !explicit_cache && !chatgpt_sign_in {
        body.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if long_cache && explicit_cache && !chatgpt_sign_in {
        body.insert("prompt_cache_options".into(), json!({"ttl": "30m"}));
    }
    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    if max_tokens > 0 && compat_bool(model, "supportsMaxOutputTokens").unwrap_or(true) && !chatgpt_sign_in {
        body.insert("max_output_tokens".into(), json!(max_tokens.max(MIN_OUTPUT_TOKENS)));
    }
    if let (Some(temperature), false) = (options.temperature, chatgpt_sign_in) {
        body.insert("temperature".into(), json!(temperature));
    }
    if !context.tools.is_empty() {
        let strict = compat_bool(model, "supportsStrictMode").unwrap_or(false);
        let tools: Vec<Value> = context
            .tools
            .iter()
            .map(|tool| {
                let mut value = json!({
                    "type": "function", "name": tool.name, "description": tool.description, "parameters": tool.parameters
                });
                if strict {
                    value["strict"] = json!(false);
                }
                value
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if model.reasoning {
        if let Some(level) = reasoning_level(model, options) {
            body.insert("reasoning".into(), json!({"effort": level.as_str(), "summary": "auto"}));
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        } else if model.provider != "github-copilot" {
            body.insert("reasoning".into(), json!({"effort": "none"}));
        }
        if model.provider == "xai" {
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }
    Value::Object(body)
}

fn normalize_part(part: &str) -> String {
    take_chars(&sanitize_id(part), 64).trim_end_matches('_').to_string()
}

fn normalize_tool_call_id(model: &Model, id: &str, source: &AssistantMessage) -> String {
    let mut parts = id.split('|');
    let (Some(call_id), Some(item_id), true) =
        (parts.next(), parts.next(), TOOL_CALL_PROVIDERS.contains(&model.provider.as_str()))
    else {
        return normalize_part(id);
    };
    let foreign = source.provider != model.provider || source.api != model.api;
    let mut item =
        if foreign { take_chars(&format!("fc_{}", short_hash(item_id)), 64) } else { normalize_part(item_id) };
    if !item.starts_with("fc_") {
        item = normalize_part(&format!("fc_{item}"));
    }
    format!("{}|{item}", normalize_part(call_id))
}

/// `(id, phase)` from a stored text signature: `{"v":1,"id":...}` or a legacy plain id.
fn parse_text_signature(signature: Option<&str>) -> Option<(String, Option<String>)> {
    let signature = signature.filter(|signature| !signature.is_empty())?;
    if signature.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed["v"] == 1
        && let Some(id) = parsed["id"].as_str()
    {
        let phase = parsed["phase"].as_str().filter(|phase| matches!(*phase, "commentary" | "final_answer"));
        return Some((id.to_string(), phase.map(str::to_string)));
    }
    Some((signature.to_string(), None))
}

fn convert_messages(model: &Model, context: &Context) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(system) = context.system_prompt.as_deref().filter(|system| !system.is_empty()) {
        let developer = model.reasoning && compat_bool(model, "supportsDeveloperRole") != Some(false);
        items.push(json!({"role": if developer { "developer" } else { "system" }, "content": system}));
    }
    let messages =
        transform_messages(&context.messages, model, &|id, source| normalize_tool_call_id(model, id, source));
    let mut message_index = 0;
    for message in &messages {
        match message {
            Message::User(user) => {
                let content: Vec<Value> = match &user.content {
                    UserContent::Text(text) => vec![json!({"type": "input_text", "text": text})],
                    UserContent::Blocks(blocks) => blocks.iter().filter_map(input_part).collect(),
                };
                if content.is_empty() {
                    continue;
                }
                items.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant(assistant) => {
                let output = convert_assistant(model, assistant, message_index);
                if output.is_empty() {
                    continue;
                }
                items.extend(output);
            }
            Message::System(_) => {}
            Message::ToolResult(result) => {
                let call_id = result.tool_call_id.split('|').next().unwrap_or_default();
                let text = tool_result_text(&result.content);
                let output = if !has_images(&result.content) || !model.supports_images() {
                    let fallback =
                        if has_images(&result.content) { "(see attached image)" } else { "(no tool output)" };
                    json!(if text.is_empty() { fallback.to_string() } else { text })
                } else {
                    let mut parts = Vec::new();
                    if !text.is_empty() {
                        parts.push(json!({"type": "input_text", "text": text}));
                    }
                    parts
                        .extend(result.content.iter().filter(|block| block.as_text().is_none()).filter_map(input_part));
                    Value::Array(parts)
                };
                items.push(json!({"type": "function_call_output", "call_id": call_id, "output": output}));
            }
        }
        message_index += 1;
    }
    items
}

fn input_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text, .. } => Some(json!({"type": "input_text", "text": text})),
        ContentBlock::Image { data, mime_type } => {
            Some(json!({"type": "input_image", "detail": "auto", "image_url": data_url(mime_type, data)}))
        }
        _ => None,
    }
}

fn convert_assistant(model: &Model, assistant: &AssistantMessage, message_index: usize) -> Vec<Value> {
    let same_provider_and_api = assistant.provider == model.provider && assistant.api == model.api;
    let different_model = same_provider_and_api && assistant.model != model.id;
    let mut output = Vec::new();
    let mut text_index = 0;
    for block in &assistant.content {
        match block {
            ContentBlock::Thinking { thinking_signature: Some(signature), .. } if !signature.is_empty() => {
                if let Ok(item) = serde_json::from_str::<Value>(signature) {
                    output.push(item);
                }
            }
            ContentBlock::Text { text, text_signature } => {
                let parsed = parse_text_signature(text_signature.as_deref());
                let fallback = match text_index {
                    0 => format!("msg_pi_{message_index}"),
                    index => format!("msg_pi_{message_index}_{index}"),
                };
                text_index += 1;
                let id = match parsed.as_ref().map(|(id, _)| id.as_str()) {
                    None | Some("") => fallback,
                    Some(id) if id.chars().count() > 64 => format!("msg_{}", short_hash(id)),
                    Some(id) => id.to_string(),
                };
                let mut item = json!({
                    "type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                    "status": "completed", "id": id,
                });
                if let Some(phase) = parsed.and_then(|(_, phase)| phase) {
                    item["phase"] = json!(phase);
                }
                output.push(item);
            }
            ContentBlock::ToolCall(call) => {
                let mut parts = call.id.split('|');
                let call_id = parts.next().unwrap_or_default();
                let mut item = json!({"type": "function_call", "call_id": call_id, "name": call.name});
                if let Some(item_id) = parts.next().filter(|id| id.starts_with("fc_") && !different_model) {
                    item["id"] = json!(item_id);
                }
                item["arguments"] = json!(Value::Object(call.arguments.clone()).to_string());
                output.push(item);
            }
            _ => {}
        }
    }
    output
}
