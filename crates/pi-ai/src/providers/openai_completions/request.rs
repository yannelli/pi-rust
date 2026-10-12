//! Request building for `openai-completions`: compat detection, message
//! conversion, and body parameters.

use serde_json::{Map, Value, json};

use super::reasoning_details::{parse_legacy_encrypted_detail, parse_reasoning_details};
use crate::providers::common::{
    MIN_ANSWER_TOKENS, compat_bool, compat_str, compat_value, data_url, has_images, long_cache_retention,
    reasoning_level, sanitize_id, short_hash, take_chars, thinking_budget, tool_result_text, transform_messages,
};
use crate::registry::StreamOptions;
use crate::types::{ContentBlock, Context, Message, Model, Tool, UserContent};

pub(super) const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

/// Resolved compatibility flags: detected from provider and URL, then overridden by `model.compat`.
pub(super) struct Compat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_finish_reason: bool,
    pub max_tokens_field: String,
    pub requires_tool_result_name: bool,
    pub requires_assistant_after_tool_result: bool,
    pub requires_thinking_as_text: bool,
    pub requires_reasoning_content: bool,
    pub thinking_format: String,
    pub supports_strict_mode: bool,
    pub anthropic_cache_control: bool,
    pub send_session_affinity_headers: bool,
    pub session_affinity_format: String,
    pub supports_long_cache_retention: bool,
    pub zai_tool_stream: bool,
    pub thinking_budget_field: Option<String>,
}

impl Compat {
    pub fn resolve(model: &Model) -> Self {
        let provider = model.provider.as_str();
        let url = model.base_url.as_str();
        let zai =
            matches!(provider, "zai" | "zai-coding-cn") || url.contains("api.z.ai") || url.contains("open.bigmodel.cn");
        let together = provider == "together" || url.contains("api.together.ai") || url.contains("api.together.xyz");
        let moonshot = matches!(provider, "moonshotai" | "moonshotai-cn") || url.contains("api.moonshot.");
        let openrouter = provider == "openrouter" || url.contains("openrouter.ai");
        let cf_workers = provider == "cloudflare-workers-ai" || url.contains("api.cloudflare.com");
        let cf_gateway = provider == "cloudflare-ai-gateway" || url.contains("gateway.ai.cloudflare.com");
        let nvidia = provider == "nvidia" || url.contains("integrate.api.nvidia.com");
        let ant_ling = provider == "ant-ling" || url.contains("api.ant-ling.com");
        let cerebras = provider == "cerebras" || url.contains("cerebras.ai");
        let deepseek = provider == "deepseek" || url.to_lowercase().contains("deepseek.com");
        let grok = provider == "xai" || url.contains("api.x.ai");
        let chutes = url.contains("chutes.ai");
        let opencode = provider == "opencode" || url.contains("opencode.ai");
        let non_standard = nvidia
            || cerebras
            || grok
            || together
            || chutes
            || deepseek
            || zai
            || moonshot
            || opencode
            || cf_workers
            || cf_gateway
            || ant_ling;
        let use_max_tokens = chutes || deepseek || moonshot || cf_gateway || together || nvidia || ant_ling || zai;
        let openrouter_developer =
            openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
        let detected_format = match () {
            _ if deepseek => "deepseek",
            _ if zai => "zai",
            _ if together => "together",
            _ if ant_ling => "ant-ling",
            _ if openrouter => "openrouter",
            _ => "openai",
        };
        let flag = |key: &str, detected: bool| compat_bool(model, key).unwrap_or(detected);
        let text = |key: &str, detected: &str| compat_str(model, key).unwrap_or(detected).to_string();
        let budget_field = compat_str(model, "thinkingTokenBudgetField")
            .map(str::to_string)
            .or_else(|| flag("supportsThinkingTokenBudget", false).then(|| "thinking_token_budget".to_string()));
        Self {
            supports_store: flag("supportsStore", !non_standard),
            supports_developer_role: flag(
                "supportsDeveloperRole",
                openrouter_developer || (!non_standard && !openrouter),
            ),
            supports_reasoning_effort: flag(
                "supportsReasoningEffort",
                !grok && !zai && !moonshot && !together && !cf_gateway && !nvidia && !ant_ling,
            ),
            supports_usage_in_streaming: flag("supportsUsageInStreaming", true),
            supports_finish_reason: flag("supportsFinishReason", true),
            max_tokens_field: text(
                "maxTokensField",
                if use_max_tokens { "max_tokens" } else { "max_completion_tokens" },
            ),
            requires_tool_result_name: flag("requiresToolResultName", false),
            requires_assistant_after_tool_result: flag("requiresAssistantAfterToolResult", false),
            requires_thinking_as_text: flag("requiresThinkingAsText", false),
            requires_reasoning_content: flag("requiresReasoningContentOnAssistantMessages", deepseek),
            thinking_format: text("thinkingFormat", detected_format),
            supports_strict_mode: flag("supportsStrictMode", false),
            anthropic_cache_control: compat_str(model, "cacheControlFormat")
                .map_or(provider == "openrouter" && model.id.starts_with("anthropic/"), |format| format == "anthropic"),
            send_session_affinity_headers: flag("sendSessionAffinityHeaders", openrouter),
            session_affinity_format: text("sessionAffinityFormat", if openrouter { "openrouter" } else { "openai" }),
            supports_long_cache_retention: flag(
                "supportsLongCacheRetention",
                !(together || cf_workers || cf_gateway || nvidia || ant_ling),
            ),
            zai_tool_stream: flag("zaiToolStream", false),
            thinking_budget_field: budget_field,
        }
    }
}

pub(super) fn build_body(model: &Model, context: &Context, options: &StreamOptions, compat: &Compat) -> Value {
    let long_cache = long_cache_retention() && compat.supports_long_cache_retention;
    let mut messages = convert_messages(model, context, compat);
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("stream".into(), json!(true));
    if let Some(session) = &options.session_id
        && (model.base_url.contains("api.openai.com") || long_cache)
    {
        body.insert("prompt_cache_key".into(), json!(take_chars(session, 64)));
    }
    if long_cache {
        body.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if compat.supports_usage_in_streaming {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if compat.supports_store {
        body.insert("store".into(), json!(false));
    }
    let max_tokens = options.max_tokens.unwrap_or(model.max_tokens);
    if max_tokens > 0 {
        body.insert(compat.max_tokens_field.clone(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    let mut tools: Option<Vec<Value>> = None;
    if !context.tools.is_empty() {
        tools = Some(context.tools.iter().map(|tool| convert_tool(tool, compat)).collect());
        if compat.zai_tool_stream {
            body.insert("tool_stream".into(), json!(true));
        }
    } else if has_tool_history(&context.messages) {
        tools = Some(Vec::new());
    }
    if compat.anthropic_cache_control {
        let mut cache_control = json!({"type": "ephemeral"});
        if long_cache {
            cache_control["ttl"] = json!("1h");
        }
        apply_cache_control(&mut messages, tools.as_mut(), &cache_control);
    }
    body.insert("messages".into(), Value::Array(messages));
    if let Some(tools) = tools {
        body.insert("tools".into(), Value::Array(tools));
    }
    apply_thinking(model, options, compat, max_tokens, &mut body);
    if let Some(routing) = compat_value(model, "openRouterRouting").filter(|value| non_empty_object(value)) {
        body.insert("provider".into(), routing.clone());
    }
    if let Some(routing) = compat_value(model, "vercelGatewayRouting") {
        let gateway: Map<String, Value> = ["only", "order"]
            .into_iter()
            .filter_map(|key| routing.get(key).map(|value| (key.to_string(), value.clone())))
            .collect();
        if !gateway.is_empty() {
            body.insert("providerOptions".into(), json!({"gateway": gateway}));
        }
    }
    Value::Object(body)
}

fn non_empty_object(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}

/// Thinking parameters per `thinkingFormat`. Formats that need per-model level
/// maps or chat template variables (`chat-template`, `baseten`, `ant-ling`) send nothing.
fn apply_thinking(
    model: &Model,
    options: &StreamOptions,
    compat: &Compat,
    ceiling: u64,
    body: &mut Map<String, Value>,
) {
    if !model.reasoning {
        return;
    }
    let level = reasoning_level(model, options);
    let effort = level.map(|level| json!(level.as_str()));
    let enabled = level.is_some();
    let format = compat.thinking_format.as_str();
    let thinking = match format {
        "zai" if enabled => Some(("thinking", json!({"type": "enabled", "clear_thinking": false}))),
        "zai" => Some(("thinking", json!({"type": "disabled"}))),
        "qwen" => Some(("enable_thinking", json!(enabled))),
        "qwen-chat-template" => {
            Some(("chat_template_kwargs", json!({"enable_thinking": enabled, "preserve_thinking": true})))
        }
        "deepseek" => Some(("thinking", json!({"type": if enabled { "enabled" } else { "disabled" }}))),
        "openrouter" => Some(("reasoning", json!({"effort": effort.clone().unwrap_or(json!("none"))}))),
        "together" => Some(("reasoning", json!({"enabled": enabled}))),
        "string-thinking" => Some(("thinking", effort.clone().unwrap_or(json!("none")))),
        _ => None,
    };
    if let Some((key, value)) = thinking {
        body.insert(key.into(), value);
    }
    let effort_format = !matches!(
        format,
        "qwen-chat-template" | "openrouter" | "string-thinking" | "chat-template" | "baseten" | "ant-ling"
    );
    if let Some(effort) = effort
        && compat.supports_reasoning_effort
        && effort_format
    {
        body.insert("reasoning_effort".into(), effort);
    }
    if let (Some(field), Some(level)) = (&compat.thinking_budget_field, level) {
        let budget = thinking_budget(level).min(ceiling.saturating_sub(MIN_ANSWER_TOKENS));
        if budget > 0 {
            body.insert(field.clone(), json!(budget));
        }
    }
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant.tool_calls().next().is_some(),
        Message::User(_) => false,
    })
}

fn convert_tool(tool: &Tool, compat: &Compat) -> Value {
    let mut function = json!({"name": tool.name, "description": tool.description, "parameters": tool.parameters});
    if compat.supports_strict_mode {
        function["strict"] = json!(false);
    }
    json!({"type": "function", "function": function})
}

fn normalize_tool_call_id(model: &Model, id: &str) -> String {
    if let Some((call, item)) = id.split_once('|') {
        let call_id = sanitize_id(call);
        let item_id = sanitize_id(item);
        let combined = if item_id.is_empty() { call_id.clone() } else { format!("{call_id}_{item_id}") };
        if combined.chars().count() <= 40 {
            return combined;
        }
        let hash = take_chars(&short_hash(id), 8);
        return format!("{}_{hash}", take_chars(&call_id, (40 - hash.len() - 1).max(1)));
    }
    if model.provider == "openai" { take_chars(id, 40) } else { id.to_string() }
}

fn convert_messages(model: &Model, context: &Context, compat: &Compat) -> Vec<Value> {
    let mut params = Vec::new();
    if let Some(system) = context.system_prompt.as_deref().filter(|system| !system.is_empty()) {
        let role = if model.reasoning && compat.supports_developer_role { "developer" } else { "system" };
        params.push(json!({"role": role, "content": system}));
    }
    let messages = transform_messages(&context.messages, model, &|id, _| normalize_tool_call_id(model, id));
    let bridge = || json!({"role": "assistant", "content": "I have processed the tool results."});
    let mut last_was_tool_result = false;
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User(user) => {
                if compat.requires_assistant_after_tool_result && last_was_tool_result {
                    params.push(bridge());
                }
                last_was_tool_result = false;
                match &user.content {
                    UserContent::Text(text) => params.push(json!({"role": "user", "content": text})),
                    UserContent::Blocks(blocks) => {
                        let parts: Vec<Value> = blocks.iter().filter_map(user_part).collect();
                        if !parts.is_empty() {
                            params.push(json!({"role": "user", "content": parts}));
                        }
                    }
                }
            }
            Message::Assistant(assistant) => {
                last_was_tool_result = false;
                params.extend(convert_assistant(model, &assistant.content, compat));
            }
            Message::ToolResult(_) => {
                let mut images = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(index) {
                    let text = tool_result_text(&result.content);
                    let with_images = has_images(&result.content);
                    let content = match () {
                        _ if !text.is_empty() => text,
                        _ if with_images => "(see attached image)".to_string(),
                        _ => "(no tool output)".to_string(),
                    };
                    let mut message = json!({"role": "tool", "content": content, "tool_call_id": result.tool_call_id});
                    if compat.requires_tool_result_name && !result.tool_name.is_empty() {
                        message["name"] = json!(result.tool_name);
                    }
                    params.push(message);
                    if with_images && model.supports_images() {
                        images.extend(result.content.iter().filter_map(image_part));
                    }
                    index += 1;
                }
                last_was_tool_result = images.is_empty();
                if !images.is_empty() {
                    if compat.requires_assistant_after_tool_result {
                        params.push(bridge());
                    }
                    let mut content = vec![json!({"type": "text", "text": "Attached image(s) from tool result:"})];
                    content.extend(images);
                    params.push(json!({"role": "user", "content": content}));
                }
                continue;
            }
        }
        index += 1;
    }
    params
}

fn user_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text, .. } if !text.is_empty() => Some(json!({"type": "text", "text": text})),
        _ => image_part(block),
    }
}

fn image_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Image { data, mime_type } => {
            Some(json!({"type": "image_url", "image_url": {"url": data_url(mime_type, data)}}))
        }
        _ => None,
    }
}

fn convert_assistant(model: &Model, content: &[ContentBlock], compat: &Compat) -> Option<Value> {
    let texts: Vec<&str> =
        content.iter().filter_map(ContentBlock::as_text).filter(|text| !text.trim().is_empty()).collect();
    let joined = texts.concat();
    let mut thinking: Vec<(&str, Option<&str>)> = Vec::new();
    let mut calls = Vec::new();
    for block in content {
        match block {
            ContentBlock::Thinking { thinking: text, thinking_signature, .. } => {
                thinking.push((text, thinking_signature.as_deref()));
            }
            ContentBlock::ToolCall(call) => calls.push(call),
            _ => {}
        }
    }
    let details = thinking.iter().find_map(|(_, signature)| parse_reasoning_details(*signature)).or_else(|| {
        let legacy: Vec<Value> =
            calls.iter().filter_map(|call| parse_legacy_encrypted_detail(call.thought_signature.as_deref())).collect();
        (!legacy.is_empty()).then_some(legacy)
    });
    let empty_content = if compat.requires_assistant_after_tool_result { json!("") } else { Value::Null };
    let mut message = json!({"role": "assistant", "content": empty_content});
    let visible: Vec<_> = thinking.iter().filter(|(text, _)| !text.trim().is_empty()).collect();
    if !visible.is_empty() && compat.requires_thinking_as_text {
        let thinking_text = visible.iter().map(|(text, _)| *text).collect::<Vec<_>>().join("\n\n");
        let mut parts = vec![json!({"type": "text", "text": thinking_text})];
        parts.extend(texts.iter().map(|text| json!({"type": "text", "text": text})));
        message["content"] = Value::Array(parts);
    } else {
        if !joined.is_empty() {
            message["content"] = json!(joined);
        }
        if let (Some((_, signature)), None) = (visible.first(), &details) {
            let field = match *signature {
                Some("reasoning") if model.provider == "opencode-go" => Some("reasoning_content"),
                other => other,
            };
            if let Some(field) = field.filter(|field| REASONING_FIELDS.contains(field)) {
                message[field] = json!(visible.iter().map(|(text, _)| *text).collect::<Vec<_>>().join("\n"));
            }
        }
    }
    if !calls.is_empty() {
        let tool_calls: Vec<Value> = calls
            .iter()
            .map(|call| {
                let arguments = Value::Object(call.arguments.clone()).to_string();
                json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": arguments}})
            })
            .collect();
        message["tool_calls"] = Value::Array(tool_calls);
    }
    if let Some(details) = details {
        message["reasoning_details"] = Value::Array(details);
    }
    if compat.requires_reasoning_content && model.reasoning && message.get("reasoning_content").is_none() {
        message["reasoning_content"] = json!("");
    }
    let has_content = match &message["content"] {
        Value::String(text) => !text.is_empty(),
        Value::Array(parts) => !parts.is_empty(),
        _ => false,
    };
    (has_content || !calls.is_empty()).then_some(message)
}

fn apply_cache_control(messages: &mut [Value], tools: Option<&mut Vec<Value>>, cache_control: &Value) {
    let instruction = |message: &&mut Value| matches!(message["role"].as_str(), Some("system" | "developer"));
    if let Some(system) = messages.iter_mut().find(instruction) {
        add_cache_control(system, cache_control);
    }
    if let Some(last) = tools.and_then(|tools| tools.last_mut()) {
        last["cache_control"] = cache_control.clone();
    }
    for message in messages.iter_mut().rev() {
        let conversation = matches!(message["role"].as_str(), Some("user" | "assistant" | "tool"));
        if conversation && add_cache_control(message, cache_control) {
            return;
        }
    }
}

fn add_cache_control(message: &mut Value, cache_control: &Value) -> bool {
    match &mut message["content"] {
        Value::String(text) if !text.is_empty() => {
            let text = std::mem::take(text);
            message["content"] = json!([{"type": "text", "text": text, "cache_control": cache_control}]);
            true
        }
        Value::Array(parts) => match parts.iter_mut().rev().find(|part| part["type"] == "text") {
            Some(part) => {
                part["cache_control"] = cache_control.clone();
                true
            }
            None => false,
        },
        _ => false,
    }
}
