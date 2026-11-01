//! Transcript and tool conversion for Chat Completions, ported from upstream
//! `convertMessages` and `convertTools` in `api/openai-completions.ts`.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::compat::Compat;
use super::reasoning_details::{parse_legacy_encrypted_detail, parse_reasoning_details};
use crate::providers::common::{
    data_url, has_images, sanitize_id, short_hash, take_chars, tool_result_text, transform_messages,
};
use crate::providers::constrained_sampling::{
    grammar_tool_input, json_schema_tool_parameters, resolve_grammar_constrained_sampling,
    resolve_json_schema_strict_sampling,
};
use crate::transcript::{render_system_message_update, resolve_transcript_tools, system_message_text};
use crate::types::{ContentBlock, Message, Model, Tool, ToolCall, UserContent};

/// Fields a provider may use for reasoning text on assistant messages.
pub(super) const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

const TOOL_RESULT_BRIDGE: &str = "I have processed the tool results.";

/// Tool name to the argument property that holds a grammar tool's raw input.
pub(super) type GrammarInputs = HashMap<String, String>;

/// Grammar tools become `custom` tools; JSON-schema tools are `function` tools with `strict`
/// only when the server supports strict mode. Fails when a tool requires unsupported sampling.
pub(super) fn convert_tools(tools: &[Tool], compat: &Compat) -> Result<Vec<Value>, String> {
    tools
        .iter()
        .map(|tool| {
            if let Some(grammar) = resolve_grammar_constrained_sampling(tool, compat.supports_openai_grammar_tools)? {
                let format = json!({"type": "grammar",
                    "grammar": {"syntax": grammar.format.as_str(), "definition": grammar.definition}});
                let custom = json!({"name": tool.name, "description": tool.description, "format": format});
                return Ok(json!({"type": "custom", "custom": custom}));
            }
            let strict = resolve_json_schema_strict_sampling(tool, compat.supports_strict_mode, None)?;
            let mut parameters = json_schema_tool_parameters(tool, strict == Some(true));
            if compat.requires_typed_tool_properties {
                super::kimi_schema::normalize(&mut parameters);
            }
            let mut function = json!({"name": tool.name, "description": tool.description, "parameters": parameters});
            if compat.supports_strict_mode {
                function["strict"] = json!(strict.unwrap_or(false));
            }
            Ok(json!({"type": "function", "function": function}))
        })
        .collect()
}

/// Chat Completions ids: `call|item` ids from the Responses API keep item uniqueness within 40
/// characters, and OpenAI itself truncates ids to 40 characters.
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

fn bridge() -> Value {
    json!({"role": "assistant", "content": TOOL_RESULT_BRIDGE})
}

/// Messages for a resolved transcript whose leading system message, if any, is the prompt.
pub(super) fn convert_messages(
    model: &Model,
    messages: &[Message],
    compat: &Compat,
    grammar: &GrammarInputs,
) -> Result<Vec<Value>, String> {
    let transformed = transform_messages(messages, model, &|id, _| normalize_tool_call_id(model, id));
    let folded = resolve_transcript_tools(messages, compat.tool_support()).folded_system_messages;
    let mut system_index = 0;
    let instruction_role = if model.reasoning && compat.supports_developer_role { "developer" } else { "system" };
    let mut params = Vec::new();
    let mut last_role: Option<&str> = None;
    let mut index = 0;
    while index < transformed.len() {
        let message = &transformed[index];
        if compat.requires_assistant_after_tool_result
            && last_role == Some("toolResult")
            && matches!(message, Message::User(_))
        {
            params.push(bridge());
        }
        match message {
            Message::System(system) => {
                let added = system.tools_added.as_deref().unwrap_or_default();
                // `transform_messages` keeps system messages in order, so their position matches the transcript.
                let anchors = system_index >= folded;
                system_index += 1;
                if anchors && !added.is_empty() {
                    params.push(json!({"role": "system", "tools": convert_tools(added, compat)?}));
                }
                let text = if index == 0 { system_message_text(system) } else { render_system_message_update(system) };
                if !text.is_empty() {
                    params.push(json!({"role": instruction_role, "content": text}));
                }
                last_role = Some("system");
            }
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => json!(text),
                    UserContent::Blocks(blocks) => {
                        let parts: Vec<Value> = blocks.iter().filter_map(user_part).collect();
                        if parts.is_empty() {
                            index += 1;
                            continue;
                        }
                        Value::Array(parts)
                    }
                };
                params.push(json!({"role": "user", "content": content}));
                last_role = Some("user");
            }
            Message::Assistant(assistant) => {
                if let Some(converted) = convert_assistant(model, &assistant.content, compat, grammar)? {
                    params.push(converted);
                    last_role = Some("assistant");
                }
            }
            // Dropped by `transform_messages`.
            Message::Custom(_) => {}
            Message::ToolResult(_) => {
                let mut images = Vec::new();
                while let Some(Message::ToolResult(result)) = transformed.get(index) {
                    let text = tool_result_text(&result.content);
                    let with_images = has_images(&result.content);
                    let content = match () {
                        _ if !text.trim().is_empty() => text,
                        _ if with_images => "(see attached image)".to_string(),
                        _ => "(no tool output)".to_string(),
                    };
                    let mut converted =
                        json!({"role": "tool", "content": content, "tool_call_id": result.tool_call_id});
                    if compat.requires_tool_result_name && !result.tool_name.is_empty() {
                        converted["name"] = json!(result.tool_name);
                    }
                    params.push(converted);
                    if with_images && model.supports_images() {
                        images.extend(result.content.iter().filter_map(image_part));
                    }
                    index += 1;
                }
                if images.is_empty() {
                    last_role = Some("toolResult");
                } else {
                    if compat.requires_assistant_after_tool_result {
                        params.push(bridge());
                    }
                    let mut content = vec![json!({"type": "text", "text": "Attached image(s) from tool result:"})];
                    content.extend(images);
                    params.push(json!({"role": "user", "content": content}));
                    last_role = Some("user");
                }
                continue;
            }
        }
        index += 1;
    }
    Ok(params)
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

/// A replayed tool call: grammar tools replay their raw input as a `custom` call.
fn convert_tool_call(call: &ToolCall, grammar: &GrammarInputs) -> Result<Value, String> {
    if let Some(property) = grammar.get(&call.name) {
        let input = grammar_tool_input(&call.name, &call.arguments, property)?;
        return Ok(json!({"id": call.id, "type": "custom", "custom": {"name": call.name, "input": input}}));
    }
    let arguments = Value::Object(call.arguments.clone()).to_string();
    Ok(json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": arguments}}))
}

/// An assistant message, or `None` when it has neither content nor tool calls.
fn convert_assistant(
    model: &Model,
    content: &[ContentBlock],
    compat: &Compat,
    grammar: &GrammarInputs,
) -> Result<Option<Value>, String> {
    let texts: Vec<&str> =
        content.iter().filter_map(ContentBlock::as_text).filter(|text| !text.trim().is_empty()).collect();
    let joined = texts.concat();
    let mut thinking: Vec<(&str, Option<&str>)> = Vec::new();
    let mut calls: Vec<&ToolCall> = Vec::new();
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
    let visible: Vec<&(&str, Option<&str>)> = thinking.iter().filter(|(text, _)| !text.trim().is_empty()).collect();
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
        let tool_calls: Result<Vec<Value>, String> =
            calls.iter().map(|call| convert_tool_call(call, grammar)).collect();
        message["tool_calls"] = Value::Array(tool_calls?);
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
    Ok((has_content || !calls.is_empty()).then_some(message))
}

/// Anthropic-style markers: the first instruction message, the last tool, and the last
/// conversation message that has text.
pub(super) fn apply_cache_control(messages: &mut [Value], tools: Option<&mut Vec<Value>>, cache_control: &Value) {
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
    let Some(content) = message.get_mut("content") else { return false };
    match content {
        Value::String(text) if !text.is_empty() => {
            let text = std::mem::take(text);
            *content = json!([{"type": "text", "text": text, "cache_control": cache_control}]);
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
