//! Mistral chat payloads: SDK-style (camelCase) for `on_payload`, then the snake_case wire form.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::providers::common::{compat_bool, data_url, short_hash, transform_messages};
use crate::providers::strict_schema::{resolve_strict_sampling, tool_parameters};
use crate::registry::{CacheRetention, StreamOptions};
use crate::simple_options::{MappedLevel, clamp_max_tokens_to_context, clamp_thinking_level, mapped_thinking_level};
use crate::transcript::{
    collapse_system_messages, current_tools, normalize_context, render_system_message_update, system_message_text,
};
use crate::types::{AssistantMessage, ContentBlock, Context, Message, Model, ThinkingLevel, Tool, UserContent};

const TOOL_CALL_ID_LENGTH: usize = 9;
const PAYLOAD_KEYS: [(&str, &str); 12] = [
    ("topP", "top_p"),
    ("maxTokens", "max_tokens"),
    ("randomSeed", "random_seed"),
    ("responseFormat", "response_format"),
    ("toolChoice", "tool_choice"),
    ("presencePenalty", "presence_penalty"),
    ("frequencyPenalty", "frequency_penalty"),
    ("parallelToolCalls", "parallel_tool_calls"),
    ("reasoningEffort", "reasoning_effort"),
    ("promptMode", "prompt_mode"),
    ("promptCacheKey", "prompt_cache_key"),
    ("safePrompt", "safe_prompt"),
];
const CHUNK_KEYS: [(&str, &str); 6] = [
    ("imageUrl", "image_url"),
    ("documentUrl", "document_url"),
    ("documentName", "document_name"),
    ("fileId", "file_id"),
    ("referenceIds", "reference_ids"),
    ("inputAudio", "input_audio"),
];

/// A 9-character alphanumeric id derived from `id`; collision `n` hashes `id:n` instead.
pub(super) fn derive_tool_call_id(id: &str, collision: usize) -> String {
    let normalized: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    if collision == 0 && normalized.len() == TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let base = if normalized.is_empty() { id } else { &normalized };
    let seed = if collision == 0 { base.to_string() } else { format!("{base}:{collision}") };
    short_hash(&seed).chars().filter(char::is_ascii_alphanumeric).take(TOOL_CALL_ID_LENGTH).collect()
}

/// Maps foreign tool call ids to unique Mistral ids for one request.
#[derive(Default)]
struct IdNormalizer {
    ids: HashMap<String, String>,
    owners: HashMap<String, String>,
}

impl IdNormalizer {
    fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.ids.get(id) {
            return existing.clone();
        }
        let mut collision = 0;
        loop {
            let candidate = derive_tool_call_id(id, collision);
            if self.owners.get(&candidate).is_none_or(|owner| owner == id) {
                self.ids.insert(id.to_string(), candidate.clone());
                self.owners.insert(candidate.clone(), id.to_string());
                return candidate;
            }
            collision += 1;
        }
    }
}

/// Session id used for prompt caching, unless caching is disabled.
pub(super) fn prompt_cache_session(options: &StreamOptions) -> Option<String> {
    options.session_id.clone().filter(|_| options.cache_retention != Some(CacheRetention::None))
}

/// `(promptMode, reasoningEffort)`: models with a thinking level map use `reasoning_effort`,
/// other reasoning models use `prompt_mode`.
fn reasoning_controls(model: &Model, options: &StreamOptions) -> (Option<&'static str>, Option<String>) {
    let reasoning =
        options.reasoning.map(|level| clamp_thinking_level(model, level)).filter(|level| *level != ThinkingLevel::Off);
    let effort_map = model.reasoning && model.thinking_level_map.is_some();
    let effort = match (effort_map, reasoning) {
        (false, _) => None,
        (true, Some(level)) => match mapped_thinking_level(model, level) {
            MappedLevel::Value(value) => Some(value.to_string()),
            _ => Some("high".to_string()),
        },
        (true, None) => match mapped_thinking_level(model, ThinkingLevel::Off) {
            MappedLevel::Value(value) => Some(value.to_string()),
            _ => None,
        },
    };
    let prompt_mode = (model.reasoning && !effort_map && reasoning.is_some()).then_some("reasoning");
    (prompt_mode, effort)
}

fn tool_choice(choice: &str) -> Value {
    match choice {
        "auto" | "none" | "any" | "required" => json!(choice),
        name => json!({"type": "function", "function": {"name": name}}),
    }
}

fn function_tools(tools: &[Tool]) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            let strict = resolve_strict_sampling(tool, true)?;
            Ok(json!({"type": "function", "function": {
                "name": tool.name, "description": tool.description,
                "parameters": tool_parameters(tool, strict), "strict": strict.unwrap_or(false),
            }}))
        })
        .collect()
}

/// The SDK-style payload passed to `on_payload`.
pub(super) fn build_payload(model: &Model, context: &Context, options: &StreamOptions) -> Result<Value, String> {
    let normalized = normalize_context(context);
    let transcript = if compat_bool(model, "supportsMidConvoSystemMessages") == Some(true) {
        normalized
    } else {
        collapse_system_messages(&normalized)
    };
    let normalizer = RefCell::new(IdNormalizer::default());
    let normalize = |id: &str, _: &AssistantMessage| normalizer.borrow_mut().normalize(id);
    let messages = transform_messages(&transcript, model, &normalize);
    let mut payload = Map::new();
    payload.insert("model".into(), json!(model.id));
    payload.insert("stream".into(), json!(true));
    payload.insert("messages".into(), Value::Array(chat_messages(&messages, model.supports_images())));
    let tools = current_tools(&transcript);
    if !tools.is_empty() {
        payload.insert("tools".into(), Value::Array(function_tools(&tools)?));
    }
    if let Some(temperature) = options.temperature {
        payload.insert("temperature".into(), json!(temperature));
    }
    let max_tokens = clamp_max_tokens_to_context(model, &transcript, options.max_tokens.unwrap_or(model.max_tokens));
    payload.insert("maxTokens".into(), json!(max_tokens));
    if let Some(choice) = options.tool_choice.as_deref() {
        payload.insert("toolChoice".into(), tool_choice(choice));
    }
    let (prompt_mode, effort) = reasoning_controls(model, options);
    if let Some(mode) = prompt_mode {
        payload.insert("promptMode".into(), json!(mode));
    }
    if let Some(effort) = effort {
        payload.insert("reasoningEffort".into(), json!(effort));
    }
    if let Some(session) = prompt_cache_session(options) {
        payload.insert("promptCacheKey".into(), json!(session));
    }
    Ok(Value::Object(payload))
}

fn tool_result_text(text: &str, has_images: bool, supports_images: bool, is_error: bool) -> String {
    let prefix = if is_error { "[tool error] " } else { "" };
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        let suffix =
            if has_images && !supports_images { "\n[tool image omitted: model does not support images]" } else { "" };
        return format!("{prefix}{trimmed}{suffix}");
    }
    let body = match (has_images, supports_images) {
        (true, true) => "(see attached image)",
        (true, false) => "(image omitted: model does not support images)",
        (false, _) => "(no tool output)",
    };
    format!("{prefix}{body}")
}

fn image_chunk(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Image { data, mime_type } => {
            Some(json!({"type": "image_url", "imageUrl": data_url(mime_type, data)}))
        }
        _ => None,
    }
}

fn user_message(content: &UserContent, supports_images: bool) -> Option<Value> {
    let UserContent::Blocks(blocks) = content else {
        return Some(json!({"role": "user", "content": content.text()}));
    };
    let chunks: Vec<Value> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(json!({"type": "text", "text": text})),
            image if supports_images => image_chunk(image),
            _ => None,
        })
        .collect();
    if !chunks.is_empty() {
        return Some(json!({"role": "user", "content": chunks}));
    }
    let had_images = blocks.iter().any(|block| matches!(block, ContentBlock::Image { .. }));
    (had_images && !supports_images)
        .then(|| json!({"role": "user", "content": "(image omitted: model does not support images)"}))
}

fn assistant_message(assistant: &AssistantMessage) -> Option<Value> {
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &assistant.content {
        match block {
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                content.push(json!({"type": "text", "text": text}));
            }
            ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                content.push(json!({"type": "thinking", "thinking": [{"type": "text", "text": thinking}]}));
            }
            ContentBlock::ToolCall(call) => tool_calls.push(json!({
                "id": call.id, "type": "function", "index": 0,
                "function": {"name": call.name, "arguments": Value::Object(call.arguments.clone()).to_string()},
            })),
            _ => {}
        }
    }
    if content.is_empty() && tool_calls.is_empty() {
        return None;
    }
    let mut item = json!({"role": "assistant", "prefix": false});
    if !content.is_empty() {
        item["content"] = Value::Array(content);
    }
    if !tool_calls.is_empty() {
        item["toolCalls"] = Value::Array(tool_calls);
    }
    Some(item)
}

fn chat_messages(messages: &[Message], supports_images: bool) -> Vec<Value> {
    let mut result = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let item = match message {
            Message::System(system) => {
                let text = if index == 0 { system_message_text(system) } else { render_system_message_update(system) };
                (!text.is_empty()).then(|| json!({"role": "system", "content": text}))
            }
            Message::User(user) => user_message(&user.content, supports_images),
            Message::Assistant(assistant) => assistant_message(assistant),
            Message::ToolResult(tool_result) => {
                let texts: Vec<&str> = tool_result.content.iter().filter_map(ContentBlock::as_text).collect();
                let has_images = tool_result.content.iter().any(|block| matches!(block, ContentBlock::Image { .. }));
                let text = tool_result_text(&texts.join("\n"), has_images, supports_images, tool_result.is_error);
                let mut content = vec![json!({"type": "text", "text": text})];
                if supports_images {
                    content.extend(tool_result.content.iter().filter_map(image_chunk));
                }
                Some(json!({
                    "role": "tool", "toolCallId": tool_result.tool_call_id, "name": tool_result.tool_name, "content": content,
                }))
            }
            Message::Custom(_) => None,
        };
        result.extend(item);
    }
    result
}

fn remap(object: &mut Map<String, Value>, source: &str, target: &str) {
    if let Some(value) = object.remove(source) {
        object.insert(target.into(), value);
    }
}

/// Rename SDK-style fields to the snake_case names of the HTTP API.
pub(super) fn to_wire(payload: Value) -> Value {
    let Value::Object(mut wire) = payload else { return payload };
    for (source, target) in PAYLOAD_KEYS {
        remap(&mut wire, source, target);
    }
    if let Some(Value::Array(messages)) = wire.get_mut("messages") {
        for message in messages.iter_mut().filter_map(Value::as_object_mut) {
            remap(message, "toolCalls", "tool_calls");
            remap(message, "toolCallId", "tool_call_id");
            for chunk in message.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
                if let Some(chunk) = chunk.as_object_mut() {
                    CHUNK_KEYS.iter().for_each(|(source, target)| remap(chunk, source, target));
                }
            }
        }
    }
    if let Some(format) = wire.get_mut("response_format").and_then(Value::as_object_mut) {
        remap(format, "jsonSchema", "json_schema");
        if let Some(schema) = format.get_mut("json_schema").and_then(Value::as_object_mut) {
            remap(schema, "schemaDefinition", "schema");
        }
    }
    Value::Object(wire)
}
