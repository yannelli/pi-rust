//! Transcript and tool conversion for the Responses API, ported from upstream
//! `api/openai-responses-shared.ts` (`convertResponsesMessages`, `convertResponsesTools`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::providers::common::{
    compat_bool, data_url, has_images, sanitize_id, short_hash, take_chars, tool_result_text, transform_messages,
};
use crate::transcript::{
    render_system_message_update, resolve_transcript, resolve_transcript_tools, system_message_text,
};
use crate::types::{AssistantMessage, ContentBlock, Message, Model, SystemMessage, Tool, UserContent};

/// Providers whose `call_id|item_id` tool call ids are replayed with their item id.
pub(crate) const TOOL_CALL_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode"];

/// How [`convert_messages`] renders a transcript.
#[derive(Clone, Debug)]
pub(crate) struct ConvertOptions {
    /// Send the leading system message as the first input item.
    pub include_system_prompt: bool,
    /// Send later system messages in place; otherwise fold them into the leading one.
    pub supports_mid_convo_system_messages: bool,
    /// Anchor tool additions as `additional_tools` items.
    pub supports_additional_tools: bool,
    /// Anchor tool additions as synthetic client tool search results.
    pub supports_tool_search: bool,
    pub tool_call_providers: &'static [&'static str],
    /// Grammar tools by name, with the argument property that holds their raw input.
    pub grammar_tool_input_properties: BTreeMap<String, String>,
    /// `strict` for function tools inside tool additions; `None` omits the field.
    pub tool_strict: Option<bool>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            include_system_prompt: true,
            supports_mid_convo_system_messages: false,
            supports_additional_tools: false,
            supports_tool_search: false,
            tool_call_providers: TOOL_CALL_PROVIDERS,
            grammar_tool_input_properties: BTreeMap::new(),
            tool_strict: None,
        }
    }
}

/// Function tools. `strict` is sent when `Some`, as upstream does when strict mode is supported.
pub(crate) fn convert_tools(tools: &[Tool], strict: Option<bool>) -> Vec<Value> {
    convert_tools_with(tools, strict, false)
}

fn convert_tools_with(tools: &[Tool], strict: Option<bool>, defer_loading: bool) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut value = json!({
                "type": "function", "name": tool.name, "description": tool.description, "parameters": tool.parameters,
            });
            if defer_loading {
                value["defer_loading"] = json!(true);
            }
            if let Some(strict) = strict {
                value["strict"] = json!(strict);
            }
            value
        })
        .collect()
}

fn normalize_part(part: &str) -> String {
    take_chars(&sanitize_id(part), 64).trim_end_matches('_').to_string()
}

fn normalize_tool_call_id(model: &Model, providers: &[&str], id: &str, source: &AssistantMessage) -> String {
    let (Some((call_id, item_id)), true) = (id.split_once('|'), providers.contains(&model.provider.as_str())) else {
        return normalize_part(id);
    };
    let item_id = item_id.split('|').next().unwrap_or_default();
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

/// Input items for a normalized transcript (leading system message first).
pub(crate) fn convert_messages(model: &Model, messages: &[Message], options: &ConvertOptions) -> Vec<Value> {
    let messages = resolve_transcript(messages.to_vec(), options.supports_mid_convo_system_messages);
    let providers = options.tool_call_providers;
    let normalize = |id: &str, source: &AssistantMessage| normalize_tool_call_id(model, providers, id, source);
    let transformed = transform_messages(&messages, model, &normalize);
    let anchors =
        resolve_transcript_tools(&messages, options.supports_additional_tools || options.supports_tool_search)
            .anchors_additions;
    let developer = model.reasoning && compat_bool(model, "supportsDeveloperRole") != Some(false);
    let instruction_role = if developer { "developer" } else { "system" };
    let mut items = Vec::new();
    let mut message_index = 0;
    for (source_index, message) in transformed.iter().enumerate() {
        let leading = source_index == 0 && matches!(message, Message::System(_));
        match message {
            Message::System(system) => {
                if !leading && anchors {
                    items.extend(tool_additions(system, &format!("system:{message_index}"), options));
                }
                if !leading || options.include_system_prompt {
                    let text = if leading { system_message_text(system) } else { render_system_message_update(system) };
                    if !text.is_empty() {
                        items.push(json!({"role": instruction_role, "content": text}));
                    }
                }
            }
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
                let output = convert_assistant(model, assistant, message_index, options);
                if output.is_empty() {
                    continue;
                }
                items.extend(output);
            }
            Message::ToolResult(result) => {
                let call_id = result.tool_call_id.split('|').next().unwrap_or_default();
                let kind = if options.grammar_tool_input_properties.contains_key(&result.tool_name) {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                items.push(
                    json!({"type": kind, "call_id": call_id, "output": tool_result_output(model, &result.content)}),
                );
            }
            Message::Custom(_) => continue,
        }
        if !leading {
            message_index += 1;
        }
    }
    items
}

/// Items that load the tools a later system message adds.
fn tool_additions(system: &SystemMessage, seed: &str, options: &ConvertOptions) -> Vec<Value> {
    let tools = system.tools_added.as_deref().unwrap_or_default();
    if tools.is_empty() {
        return Vec::new();
    }
    if options.supports_additional_tools {
        let tools = convert_tools(tools, options.tool_strict);
        return vec![json!({"type": "additional_tools", "role": "developer", "tools": tools})];
    }
    if !options.supports_tool_search {
        return Vec::new();
    }
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    let call_id = format!("pi_tool_load_{}", short_hash(&format!("{seed}:{}", names.join(","))));
    vec![
        json!({
            "type": "tool_search_call", "call_id": call_id, "execution": "client", "status": "completed",
            "arguments": {"query": names.join(" "), "limit": names.len()},
        }),
        json!({
            "type": "tool_search_output", "call_id": call_id, "execution": "client", "status": "completed",
            "tools": convert_tools_with(tools, options.tool_strict, true),
        }),
    ]
}

fn tool_result_output(model: &Model, content: &[ContentBlock]) -> Value {
    let text = tool_result_text(content);
    if !has_images(content) || !model.supports_images() {
        let fallback = if has_images(content) { "(see attached image)" } else { "(no tool output)" };
        return json!(if text.is_empty() { fallback.to_string() } else { text });
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "input_text", "text": text}));
    }
    parts.extend(content.iter().filter(|block| block.as_text().is_none()).filter_map(input_part));
    Value::Array(parts)
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

fn convert_assistant(
    model: &Model,
    assistant: &AssistantMessage,
    message_index: usize,
    options: &ConvertOptions,
) -> Vec<Value> {
    let same_provider_and_api = assistant.provider == model.provider && assistant.api == model.api;
    let same_model = same_provider_and_api && assistant.model == model.id;
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
                    Some(id) if id.encode_utf16().count() > 64 => format!("msg_{}", short_hash(id)),
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
                let custom = options.grammar_tool_input_properties.get(&call.name);
                let prefix = if custom.is_some() { "ctc_" } else { "fc_" };
                let mut item = match custom {
                    Some(property) => {
                        let input = call.arguments.get(property).and_then(Value::as_str).unwrap_or_default();
                        json!({"type": "custom_tool_call", "call_id": call_id, "name": call.name, "input": input})
                    }
                    None => json!({"type": "function_call", "call_id": call_id, "name": call.name}),
                };
                if let Some(item_id) = parts.next().filter(|id| id.starts_with(prefix) && !different_model) {
                    item["id"] = json!(item_id);
                }
                if custom.is_none() {
                    item["arguments"] = json!(Value::Object(call.arguments.clone()).to_string());
                }
                if let (true, Some(namespace)) = (same_model, &call.namespace) {
                    item["namespace"] = json!(namespace);
                }
                output.push(item);
            }
            _ => {}
        }
    }
    output
}
