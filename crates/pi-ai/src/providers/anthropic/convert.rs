//! Transcript and tool conversion for `anthropic-messages`, ported from upstream
//! `api/anthropic-messages.ts` (`convertMessages`, `convertTools`, `insertThinkingLevelMessages`).

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map, Value, json};

use crate::providers::common::{has_images, sanitize_id, split_error_tool_result_images, take_chars};
use crate::providers::constrained_sampling::{json_schema_tool_parameters, resolve_json_schema_strict_sampling};
use crate::transcript::render_system_message_update;
use crate::types::{AssistantMessage, ContentBlock, Message, Tool, ToolResultMessage, UserContent};

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

/// Stable deferred tool declared with native tool changes, so Anthropic's scaffolding for
/// tool changes is cached from the first request. It is never activated.
pub(super) const DEFERRED_TOOL_PLACEHOLDER: &str = "__pi_deferred_placeholder__";

const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Claude Code canonical casing for OAuth requests.
pub(super) fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|tool| tool.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_string(), |tool| tool.to_string())
}

/// Map a streamed tool name back to the declared tool name.
pub(super) fn from_claude_code_name(name: &str, tools: &[Tool]) -> String {
    tools.iter().find(|tool| tool.name.eq_ignore_ascii_case(name)).map_or_else(|| name.to_string(), |t| t.name.clone())
}

pub(super) fn normalize_tool_call_id(id: &str) -> String {
    take_chars(&sanitize_id(id), 64)
}

/// How tool definitions are rendered.
#[derive(Clone, Copy)]
pub(super) struct ToolFormat {
    pub oauth: bool,
    pub eager_input_streaming: bool,
    pub strict_tools: bool,
    /// Send `strict: "prefer"` tools with their original schemas, as after a strict tool limit
    /// rejection.
    pub disable_preferred_strict_tools: bool,
}

/// Whether the tool asks for JSON-schema strict sampling only when available.
fn prefers_strict(tool: &Tool) -> bool {
    tool.constrained_sampling.as_ref().is_some_and(|config| {
        config.get("type").and_then(Value::as_str) == Some("json_schema")
            && config.get("strict").and_then(Value::as_str) == Some("prefer")
    })
}

/// Whether any `strict: "prefer"` tool was sent strict, so a retry without strict can help.
pub(super) fn has_preferred_strict(tools: &[Tool], definitions: &[Value]) -> bool {
    tools.iter().zip(definitions).any(|(tool, definition)| definition["strict"] == true && prefers_strict(tool))
}

/// Keywords that Anthropic strict tool use rejects for the whole request.
const STRICT_UNSUPPORTED_KEYWORDS: [&str; 11] = [
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "maxItems",
    "uniqueItems",
    "minContains",
    "maxContains",
    "minProperties",
    "maxProperties",
];
const STRICT_STRING_FORMATS: [&str; 10] =
    ["date-time", "time", "date", "duration", "email", "hostname", "uri", "ipv4", "ipv6", "uuid"];

fn strict_unsupported(key: &str, value: &Value) -> bool {
    match key {
        "minItems" => value.as_u64().is_none_or(|count| count > 1),
        "format" => value.as_str().is_none_or(|format| !STRICT_STRING_FORMATS.contains(&format)),
        key => STRICT_UNSUPPORTED_KEYWORDS.contains(&key),
    }
}

pub(super) fn deferred_tool_placeholder() -> Value {
    json!({
        "name": DEFERRED_TOOL_PLACEHOLDER,
        "description": "Reserved placeholder. Never available. Never call this.",
        "input_schema": {"type": "object", "properties": {}, "required": []},
        "defer_loading": true,
    })
}

/// Tool definitions; `cache_control` goes on the last one. Fails when a tool requires strict
/// sampling that the model or its schema cannot provide.
pub(super) fn convert_tools(
    tools: &[Tool],
    format: ToolFormat,
    cache_control: Option<&Value>,
) -> Result<Vec<Value>, String> {
    let last = tools.len().saturating_sub(1);
    let mut converted = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let strict = !(format.disable_preferred_strict_tools && prefers_strict(tool))
            && resolve_json_schema_strict_sampling(tool, format.strict_tools, Some(&strict_unsupported))? == Some(true);
        let parameters = json_schema_tool_parameters(tool, strict);
        let name = if format.oauth { to_claude_code_name(&tool.name) } else { tool.name.clone() };
        let mut input_schema = if strict { parameters.as_object().cloned().unwrap_or_default() } else { Map::new() };
        input_schema.insert("type".into(), json!("object"));
        input_schema.insert("properties".into(), parameters.get("properties").cloned().unwrap_or_else(|| json!({})));
        input_schema.insert("required".into(), parameters.get("required").cloned().unwrap_or_else(|| json!([])));
        let mut value = json!({"name": name, "description": tool.description});
        if format.eager_input_streaming {
            value["eager_input_streaming"] = json!(true);
        }
        if strict {
            value["strict"] = json!(true);
        }
        value["input_schema"] = Value::Object(input_schema);
        if let (Some(cache_control), true) = (cache_control, index == last) {
            value["cache_control"] = cache_control.clone();
        }
        converted.push(value);
    }
    Ok(converted)
}

pub(super) struct ConvertOptions<'a> {
    pub oauth: bool,
    pub cache_control: Option<&'a Value>,
    pub allow_empty_signature: bool,
    /// Provider whose assistant effort levels are replayed as markers (managed effort models).
    pub managed_provider: Option<&'a str>,
    /// Format for native `tool_addition` blocks; `None` when tool changes are not native.
    pub native_tools: Option<ToolFormat>,
}

pub(super) struct Converted {
    pub messages: Vec<Value>,
    /// Whether a native tool addition sent a preferred-strict tool strict.
    pub preferred_strict_tools: bool,
    /// Effort of replayed assistant messages, by index in `messages`.
    pub assistant_levels: BTreeMap<usize, String>,
}

/// Convert a transformed transcript without its leading system message. Later system
/// messages are held until the next assistant message, since a system message between a
/// `tool_use` and its `tool_result` is rejected.
pub(super) fn convert_messages(messages: &[Message], options: &ConvertOptions) -> Result<Converted, String> {
    let mut params: Vec<Value> = Vec::new();
    let mut assistant_levels = BTreeMap::new();
    let mut preferred_strict_tools = false;
    let mut pending: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::System(system) => {
                let mut blocks = Vec::new();
                let text = render_system_message_update(system);
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                if let Some(format) = options.native_tools {
                    let added = system.tools_added.as_deref().unwrap_or_default();
                    let redefined: HashSet<&str> = added.iter().map(|tool| tool.name.as_str()).collect();
                    for removed in system.tools_removed.iter().flatten() {
                        if redefined.contains(removed.name.as_str()) {
                            continue;
                        }
                        let name =
                            if options.oauth { to_claude_code_name(&removed.name) } else { removed.name.clone() };
                        blocks.push(json!({"type": "tool_removal", "tool": {"type": "tool_reference", "name": name}}));
                    }
                    let definitions = convert_tools(added, format, None)?;
                    preferred_strict_tools |= has_preferred_strict(added, &definitions);
                    for definition in definitions {
                        blocks.push(
                            json!({"type": "tool_addition", "tool": {"type": "tool_definition", "definition": definition}}),
                        );
                    }
                }
                if !blocks.is_empty() {
                    pending.push(json!({"role": "system", "content": blocks}));
                }
            }
            Message::User(user) => match &user.content {
                UserContent::Text(text) if !text.trim().is_empty() => {
                    params.push(json!({"role": "user", "content": text}));
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
                let blocks: Vec<Value> =
                    assistant.content.iter().filter_map(|block| assistant_block(block, options)).collect();
                // A skipped assistant keeps updates pending, so none lands before a user message.
                if !blocks.is_empty() {
                    params.append(&mut pending);
                    if let Some(level) = managed_level(assistant, options.managed_provider) {
                        assistant_levels.insert(params.len(), level);
                    }
                    params.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Message::ToolResult(_) => {
                let mut results = Vec::new();
                // Error tool results cannot hold images, and tool results must lead the user message.
                let mut error_images = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(index) {
                    let split = split_error_tool_result_images(result);
                    results.push(tool_result(result, split.as_ref().map_or(&result.content, |split| &split.content)));
                    if let Some(split) = split {
                        error_images.push(json!({"type": "text", "text": split.caption}));
                        error_images.extend(split.images.iter().filter_map(block_param));
                    }
                    index += 1;
                }
                results.append(&mut error_images);
                params.push(json!({"role": "user", "content": results}));
                continue;
            }
            Message::Custom(_) => {}
        }
        index += 1;
    }
    params.append(&mut pending);
    if let Some(cache_control) = options.cache_control {
        cache_last_message(&mut params, cache_control);
    }
    Ok(Converted { messages: params, preferred_strict_tools, assistant_levels })
}

fn managed_level(assistant: &AssistantMessage, managed_provider: Option<&str>) -> Option<String> {
    let level = assistant.provider_thinking_level.as_deref().filter(|level| EFFORTS.contains(level))?;
    let managed = managed_provider == Some(assistant.provider.as_str()) && assistant.api == "anthropic-messages";
    managed.then(|| level.to_string())
}

/// Cache the conversation history at the last user or system message.
fn cache_last_message(params: &mut [Value], cache_control: &Value) {
    let Some(last) = params.last_mut().filter(|last| matches!(last["role"].as_str(), Some("user" | "system"))) else {
        return;
    };
    match &mut last["content"] {
        Value::String(text) => {
            let text = std::mem::take(text);
            last["content"] = json!([{"type": "text", "text": text, "cache_control": cache_control}]);
        }
        Value::Array(blocks) => {
            let cacheable = ["text", "image", "tool_result", "tool_addition", "tool_removal"];
            if let Some(block) =
                blocks.last_mut().filter(|block| block["type"].as_str().is_some_and(|kind| cacheable.contains(&kind)))
            {
                block["cache_control"] = cache_control.clone();
            }
        }
        _ => {}
    }
}

/// Effort markers before replayed assistant messages and one for the active effort at the end.
pub(super) fn insert_thinking_level_messages(converted: Converted, active_effort: &str) -> Vec<Value> {
    let mut messages = Vec::with_capacity(converted.messages.len() + 1);
    for (index, message) in converted.messages.into_iter().enumerate() {
        if let Some(effort) = converted.assistant_levels.get(&index) {
            messages.push(json!({"role": "system", "content": [], "output_config": {"effort": effort}}));
        }
        messages.push(message);
    }
    messages.push(json!({"role": "system", "content": [], "output_config": {"effort": active_effort}}));
    messages
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

fn tool_result(result: &ToolResultMessage, content: &[ContentBlock]) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": result.tool_call_id,
        "content": content_blocks(content),
        "is_error": result.is_error,
    })
}

fn assistant_block(block: &ContentBlock, options: &ConvertOptions) -> Option<Value> {
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
                None if options.allow_empty_signature => {
                    Some(json!({"type": "thinking", "thinking": thinking, "signature": ""}))
                }
                None => Some(json!({"type": "text", "text": thinking})),
            }
        }
        ContentBlock::ToolCall(call) => {
            let name = if options.oauth { to_claude_code_name(&call.name) } else { call.name.clone() };
            Some(json!({"type": "tool_use", "id": call.id, "name": name, "input": call.arguments}))
        }
        _ => None,
    }
}
