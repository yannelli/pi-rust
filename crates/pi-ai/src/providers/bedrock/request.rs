//! ConverseStream request bodies: messages, system prompt, tools, caching, and thinking.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::model_info::{
    gpt_effort, gpt_oss_effort, is_claude, is_gov_cloud, map_effort, supports_adaptive_thinking,
    supports_block_binding, supports_prompt_caching,
};
use crate::providers::common::{
    MIN_ANSWER_TOKENS, compat_bool, sanitize_id, take_chars, thinking_budget, transform_messages,
};
use crate::providers::constrained_sampling::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::providers::provider_env::provider_env_value;
use crate::registry::{CacheRetention, StreamOptions, ThinkingBudgets};
use crate::types::{ContentBlock, Context, Message, Model, ThinkingLevel, ToolResultMessage, UserContent};

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// The request as upstream passes it to `onPayload`: the Converse body plus `modelId`.
pub(super) fn build_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    configured_region: Option<&str>,
) -> Result<Value, String> {
    let cache = cache_retention(options);
    let reasoning = options.reasoning.filter(|level| *level != ThinkingLevel::Off && model.reasoning);
    let base_max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    let mut budgets = options.thinking_budgets.unwrap_or_default();
    let mut max_tokens = base_max_tokens;
    if let Some(level) = reasoning.filter(|_| is_claude(model) && !supports_adaptive_thinking(model)) {
        let clamped = clamp_level(level);
        let mut budget = budget_for(&budgets, clamped).unwrap_or_else(|| thinking_budget(clamped));
        max_tokens = (base_max_tokens + budget).min(model.max_tokens);
        if max_tokens <= budget {
            budget = budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS));
        }
        set_budget(&mut budgets, clamped, budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS)));
    }
    let mut body = Map::new();
    body.insert("modelId".into(), json!(model.id));
    body.insert("messages".into(), Value::Array(convert_messages(model, context, cache, options)?));
    if let Some(system) = context.system_prompt.as_deref().filter(|system| !system.is_empty()) {
        let mut blocks = vec![json!({"text": system})];
        if let Some(point) = cache_point(model, cache, options) {
            blocks.push(point);
        }
        body.insert("system".into(), Value::Array(blocks));
    }
    let mut inference = Map::new();
    if max_tokens > 0 {
        inference.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        inference.insert("temperature".into(), json!(temperature));
    }
    body.insert("inferenceConfig".into(), Value::Object(inference));
    if let Some(tool_config) = tool_config(model, context, options)? {
        body.insert("toolConfig".into(), tool_config);
    }
    if let Some(level) = reasoning
        && let Some(fields) = additional_fields(model, level, &budgets, configured_region)
    {
        body.insert("additionalModelRequestFields".into(), fields);
    }
    Ok(Value::Object(body))
}

fn cache_retention(options: &StreamOptions) -> CacheRetention {
    options.cache_retention.unwrap_or_else(|| match provider_env_value("PI_CACHE_RETENTION", &options.env) {
        Some(value) if value == "long" => CacheRetention::Long,
        _ => CacheRetention::Short,
    })
}

fn cache_point(model: &Model, cache: CacheRetention, options: &StreamOptions) -> Option<Value> {
    if cache == CacheRetention::None || !supports_prompt_caching(model, &options.env) {
        return None;
    }
    Some(match cache {
        CacheRetention::Long => json!({"cachePoint": {"type": "default", "ttl": "1h"}}),
        _ => json!({"cachePoint": {"type": "default"}}),
    })
}

fn clamp_level(level: ThinkingLevel) -> ThinkingLevel {
    match level {
        ThinkingLevel::Xhigh | ThinkingLevel::Max => ThinkingLevel::High,
        level => level,
    }
}

fn budget_for(budgets: &ThinkingBudgets, level: ThinkingLevel) -> Option<u64> {
    match level {
        ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        ThinkingLevel::High => budgets.high,
        _ => None,
    }
}

fn set_budget(budgets: &mut ThinkingBudgets, level: ThinkingLevel, budget: u64) {
    match level {
        ThinkingLevel::Minimal => budgets.minimal = Some(budget),
        ThinkingLevel::Low => budgets.low = Some(budget),
        ThinkingLevel::Medium => budgets.medium = Some(budget),
        ThinkingLevel::High => budgets.high = Some(budget),
        _ => {}
    }
}

fn additional_fields(
    model: &Model,
    level: ThinkingLevel,
    budgets: &ThinkingBudgets,
    configured_region: Option<&str>,
) -> Option<Value> {
    if is_claude(model) {
        let gov_cloud = is_gov_cloud(model, configured_region);
        let binding = !gov_cloud && supports_block_binding(model);
        let adaptive = supports_adaptive_thinking(model);
        let mut thinking = if adaptive {
            json!({"type": "adaptive"})
        } else {
            let budget = budget_for(budgets, clamp_level(level)).unwrap_or_else(|| thinking_budget(level));
            json!({"type": "enabled", "budget_tokens": budget})
        };
        if !gov_cloud {
            thinking["display"] = json!("summarized");
        }
        let mut fields = json!({"thinking": thinking});
        if adaptive {
            if binding {
                fields["thinking"]["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
            }
            fields["output_config"] = json!({"effort": map_effort(model, level)});
            if binding {
                fields["anthropic_beta"] = json!([THINKING_BINDING_CONTROLS_BETA]);
            }
        } else {
            fields["anthropic_beta"] = json!([INTERLEAVED_THINKING_BETA]);
        }
        return Some(fields);
    }
    if let Some(effort) = gpt_oss_effort(model, level) {
        return Some(json!({"reasoning_effort": effort}));
    }
    gpt_effort(model, level).map(|effort| json!({"reasoning": {"effort": effort}}))
}

fn tool_config(model: &Model, context: &Context, options: &StreamOptions) -> Result<Option<Value>, String> {
    let choice = options.tool_choice.as_deref();
    if context.tools.is_empty() || choice == Some("none") {
        return Ok(None);
    }
    let supports_strict = compat_bool(model, "supportsStrictMode").unwrap_or(false);
    let mut tools = Vec::new();
    for tool in &context.tools {
        let strict = resolve_json_schema_strict_sampling(tool, supports_strict, None)?;
        let mut spec = json!({
            "name": tool.name,
            "description": tool.description,
            "inputSchema": {"json": json_schema_tool_parameters(tool, strict == Some(true))},
        });
        if strict == Some(true) {
            spec["strict"] = json!(true);
        }
        tools.push(json!({"toolSpec": spec}));
    }
    let mut config = json!({"tools": tools});
    match choice {
        Some("auto") => config["toolChoice"] = json!({"auto": {}}),
        Some("any") => config["toolChoice"] = json!({"any": {}}),
        _ => {}
    }
    Ok(Some(config))
}

fn normalize_tool_call_id(id: &str) -> String {
    take_chars(&sanitize_id(id), 64)
}

fn non_blank_text(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"text": text}))
}

fn image_block(mime_type: &str, data: &str) -> Result<Value, String> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => return Err(format!("Unknown image type: {other}")),
    };
    Ok(json!({"image": {"format": format, "source": {"bytes": data}}}))
}

/// Drop object keys that are empty strings; Bedrock rejects them in replayed tool input.
fn sanitize_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_document).collect()),
        Value::Object(object) => Value::Object(
            object.iter().filter(|(key, _)| !key.is_empty()).map(|(k, v)| (k.clone(), sanitize_document(v))).collect(),
        ),
        other => other.clone(),
    }
}

fn tool_result(result: &ToolResultMessage) -> Result<Value, String> {
    let mut content = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Image { data, mime_type } => content.push(image_block(mime_type, data)?),
            ContentBlock::Text { text, .. } => content.extend(non_blank_text(text)),
            _ => {}
        }
    }
    if content.is_empty() {
        content.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
    }
    let status = if result.is_error { "error" } else { "success" };
    Ok(json!({"toolResult": {"toolUseId": result.tool_call_id, "content": content, "status": status}}))
}

fn convert_assistant(model: &Model, content: &[ContentBlock]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text, .. } => blocks.extend(non_blank_text(text)),
            ContentBlock::ToolCall(call) => blocks.push(json!({"toolUse": {
                "toolUseId": call.id, "name": call.name, "input": sanitize_document(&Value::Object(call.arguments.clone())),
            }})),
            ContentBlock::Thinking { redacted: true, thinking_signature, .. } => {
                let decoded = thinking_signature.as_deref().and_then(|signature| STANDARD.decode(signature).ok());
                if let Some(bytes) = decoded.filter(|bytes| !bytes.is_empty()) {
                    blocks.push(json!({"reasoningContent": {"redactedContent": STANDARD.encode(bytes)}}));
                }
            }
            ContentBlock::Thinking { thinking, thinking_signature, .. } => {
                if thinking.trim().is_empty() {
                    continue;
                }
                let signature = thinking_signature.as_deref().filter(|signature| !signature.trim().is_empty());
                blocks.push(match (is_claude(model), signature) {
                    (true, None) => json!({"text": thinking}),
                    (true, Some(signature)) => {
                        json!({"reasoningContent": {"reasoningText": {"text": thinking, "signature": signature}}})
                    }
                    (false, _) => json!({"reasoningContent": {"reasoningText": {"text": thinking}}}),
                });
            }
            ContentBlock::Image { .. } => {}
        }
    }
    blocks
}

fn convert_messages(
    model: &Model,
    context: &Context,
    cache: CacheRetention,
    options: &StreamOptions,
) -> Result<Vec<Value>, String> {
    let messages = transform_messages(&context.messages, model, &|id, _| normalize_tool_call_id(id));
    let mut result: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User(user) => {
                let mut content = Vec::new();
                match &user.content {
                    UserContent::Text(text) => content.extend(non_blank_text(text)),
                    UserContent::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                ContentBlock::Text { text, .. } => content.extend(non_blank_text(text)),
                                ContentBlock::Image { data, mime_type } => content.push(image_block(mime_type, data)?),
                                _ => {}
                            }
                        }
                    }
                }
                if content.is_empty() {
                    content.push(json!({"text": EMPTY_TEXT_PLACEHOLDER}));
                }
                result.push(json!({"role": "user", "content": content}));
            }
            Message::Assistant(assistant) => {
                let content = convert_assistant(model, &assistant.content);
                if !content.is_empty() {
                    result.push(json!({"role": "assistant", "content": content}));
                }
            }
            Message::ToolResult(_) => {
                let mut content = Vec::new();
                while let Some(Message::ToolResult(next)) = messages.get(index) {
                    content.push(tool_result(next)?);
                    index += 1;
                }
                result.push(json!({"role": "user", "content": content}));
                continue;
            }
            Message::System(_) | Message::Custom(_) => {}
        }
        index += 1;
    }
    if let Some(point) = cache_point(model, cache, options)
        && let Some(last) = result.last_mut().filter(|last| last["role"] == "user")
        && let Some(content) = last["content"].as_array_mut()
    {
        content.push(point);
    }
    Ok(result)
}
