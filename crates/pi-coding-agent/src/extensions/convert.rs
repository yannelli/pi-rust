//! JSON forms extensions see: system prompt options (upstream `BuildSystemPromptOptions`),
//! stream options, and messages they hand back.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use pi_ai::{AssistantMessage, AssistantMessageEvent, ContentBlock, Message, StopReason, StreamOptions, ThinkingLevel};
use serde_json::{Map, Value, json};

use crate::resources::{ContextFile, Skill, SourceInfo};
use crate::system_prompt::BuildSystemPromptOptions;

pub(crate) fn prompt_options_to_json(options: &BuildSystemPromptOptions) -> Value {
    let sections: Map<String, Value> =
        options.sections.iter().map(|(name, text)| (name.clone(), json!(text))).collect();
    let context_files: Vec<Value> = options
        .context_files
        .iter()
        .map(|file| json!({"path": file.path.display().to_string(), "content": file.content}))
        .collect();
    let mut value = json!({
        "cwd": options.cwd.display().to_string(),
        "selectedTools": options.selected_tools,
        "hiddenTools": options.hidden_tools,
        "toolSnippets": options.tool_snippets,
        "toolGuidelines": options.tool_guidelines,
        "promptGuidelines": options.prompt_guidelines,
        "appendSystemPrompt": options.append_system_prompt,
        "sections": sections,
        "contextFiles": context_files,
        "skills": options.skills,
    });
    if let Some(custom) = &options.custom_prompt {
        value["customPrompt"] = json!(custom);
    }
    if let Some(forced) = &options.force_system_prompt {
        value["forceSystemPrompt"] = json!(forced);
    }
    value
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn skill_from_json(value: &Value) -> Option<Skill> {
    let path = |key: &str| value[key].as_str().map(PathBuf::from);
    Some(Skill {
        name: value["name"].as_str()?.to_string(),
        description: value["description"].as_str().unwrap_or_default().to_string(),
        file_path: path("filePath")?,
        base_dir: path("baseDir").unwrap_or_default(),
        source_info: serde_json::from_value::<SourceInfo>(value["sourceInfo"].clone()).ok()?,
        disable_model_invocation: value["disableModelInvocation"].as_bool().unwrap_or(false),
        license: value["license"].as_str().map(str::to_string),
        compatibility: value["compatibility"].as_str().map(str::to_string),
        metadata: value["metadata"].as_object().cloned().unwrap_or_default(),
        allowed_tools: strings(&value["allowedTools"]),
    })
}

/// Options from a handler's JSON; fields it left out or broke keep their defaults.
pub(crate) fn prompt_options_from_json(value: &Value, cwd: &Path) -> BuildSystemPromptOptions {
    let mut options = BuildSystemPromptOptions::new(cwd);
    if let Some(cwd) = value["cwd"].as_str() {
        options.cwd = PathBuf::from(cwd);
    }
    options.custom_prompt = value["customPrompt"].as_str().map(str::to_string);
    options.force_system_prompt = value["forceSystemPrompt"].as_str().map(str::to_string);
    if value["selectedTools"].is_array() {
        options.selected_tools = strings(&value["selectedTools"]);
    }
    options.hidden_tools = strings(&value["hiddenTools"]);
    options.tool_snippets =
        serde_json::from_value::<HashMap<String, String>>(value["toolSnippets"].clone()).unwrap_or_default();
    options.tool_guidelines =
        serde_json::from_value::<HashMap<String, Vec<String>>>(value["toolGuidelines"].clone()).unwrap_or_default();
    options.prompt_guidelines = strings(&value["promptGuidelines"]);
    options.append_system_prompt = value["appendSystemPrompt"].as_str().unwrap_or_default().to_string();
    options.sections = value["sections"]
        .as_object()
        .map(|sections| {
            sections.iter().filter_map(|(name, text)| Some((name.clone(), text.as_str()?.to_string()))).collect()
        })
        .unwrap_or_default();
    options.context_files = value["contextFiles"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|file| {
                    Some(ContextFile {
                        path: PathBuf::from(file["path"].as_str()?),
                        content: file["content"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    options.skills = value["skills"]
        .as_array()
        .map(|skills| skills.iter().filter_map(skill_from_json).collect())
        .unwrap_or_default();
    options
}

/// Stream options from upstream `SimpleStreamOptions` JSON; `cancel` comes from the caller.
pub(crate) fn stream_options_from_json(value: &Value) -> StreamOptions {
    let mut options = StreamOptions::default();
    let string = |key: &str| value[key].as_str().map(str::to_string);
    options.api_key = string("apiKey");
    options.max_tokens = value["maxTokens"].as_u64();
    options.temperature = value["temperature"].as_f64();
    options.reasoning = string("reasoning").and_then(|level| crate::model_resolver::parse_thinking_level(&level));
    options.session_id = string("sessionId");
    options.tool_choice = string("toolChoice");
    options.service_tier = string("serviceTier");
    options.timeout_ms = value["timeoutMs"].as_u64();
    options.max_retries = value["maxRetries"].as_u64().map(|retries| retries as u32);
    options.metadata = value["metadata"].as_object().cloned();
    options.identity = serde_json::from_value(value["identity"].clone()).ok();
    options.env = serde_json::from_value(value["env"].clone()).unwrap_or_default();
    if let Some(headers) = value["headers"].as_object() {
        for (name, header) in headers {
            match header.as_str() {
                Some(text) => drop(options.headers.insert(name.clone(), text.to_string())),
                None if header.is_null() => options.suppressed_headers.push(name.clone()),
                None => {}
            }
        }
    }
    options
}

/// Headers as upstream `ProviderHeaders`: suppressed defaults are `null`.
pub(crate) fn headers_to_json(headers: &BTreeMap<String, String>, suppressed: &[String]) -> Value {
    let mut map: Map<String, Value> = headers.iter().map(|(name, value)| (name.clone(), json!(value))).collect();
    for name in suppressed {
        map.insert(name.clone(), Value::Null);
    }
    Value::Object(map)
}

pub(crate) fn headers_from_json(value: &Value) -> (BTreeMap<String, String>, Vec<String>) {
    let mut headers = BTreeMap::new();
    let mut suppressed = Vec::new();
    for (name, header) in value.as_object().into_iter().flatten() {
        match header.as_str() {
            Some(text) => drop(headers.insert(name.clone(), text.to_string())),
            None => suppressed.push(name.clone()),
        }
    }
    (headers, suppressed)
}

/// A message handed back by an extension, or `None` when it is not a valid message.
pub(crate) fn message_from_json(value: Value) -> Option<Message> {
    serde_json::from_value(value).ok()
}

/// Content of `sendUserMessage`: a string, or text and image parts.
pub(crate) fn user_content(value: &Value) -> (String, Vec<ContentBlock>) {
    match value {
        Value::String(text) => (text.clone(), Vec::new()),
        Value::Array(parts) => {
            let mut texts = Vec::new();
            let mut images = Vec::new();
            for part in parts {
                match serde_json::from_value::<ContentBlock>(part.clone()) {
                    Ok(ContentBlock::Text { text, .. }) => texts.push(text),
                    Ok(image @ ContentBlock::Image { .. }) => images.push(image),
                    _ => {}
                }
            }
            (texts.join("\n"), images)
        }
        _ => (String::new(), Vec::new()),
    }
}

pub(crate) fn thinking_level(value: &Value) -> Option<ThinkingLevel> {
    value.as_str().and_then(crate::model_resolver::parse_thinking_level)
}

/// Upstream `SimpleStreamOptions` JSON for a JavaScript stream; `signal` is a signal id.
pub(crate) fn stream_options_to_json(options: &StreamOptions, signal: u64) -> Value {
    let mut value = json!({
        "signal": signal,
        "headers": headers_to_json(&options.headers, &options.suppressed_headers),
    });
    let mut set = |key: &str, field: Value| {
        if !field.is_null() {
            value[key] = field;
        }
    };
    set("apiKey", json!(options.api_key));
    set("maxTokens", json!(options.max_tokens));
    set("temperature", json!(options.temperature));
    set("reasoning", json!(options.reasoning.filter(|level| *level != ThinkingLevel::Off)));
    set("sessionId", json!(options.session_id));
    set("cacheRetention", json!(options.cache_retention));
    set("toolChoice", json!(options.tool_choice));
    set("serviceTier", json!(options.service_tier));
    set("timeoutMs", json!(options.timeout_ms));
    set("maxRetries", json!(options.max_retries));
    set("metadata", json!(options.metadata));
    set("identity", json!(options.identity));
    set("env", if options.env.is_empty() { Value::Null } else { json!(options.env) });
    value
}

/// A stream event pushed by a JavaScript provider.
pub(crate) fn event_from_json(value: &Value) -> Option<AssistantMessageEvent> {
    let partial = || serde_json::from_value::<AssistantMessage>(value["partial"].clone()).ok();
    let index = || value["contentIndex"].as_u64().map(|index| index as usize);
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
    let reason = || serde_json::from_value::<StopReason>(value["reason"].clone()).unwrap_or(StopReason::Stop);
    Some(match value["type"].as_str()? {
        "start" => AssistantMessageEvent::Start { partial: partial()? },
        "text_start" => AssistantMessageEvent::TextStart { content_index: index()?, partial: partial()? },
        "text_delta" => {
            AssistantMessageEvent::TextDelta { content_index: index()?, delta: text("delta"), partial: partial()? }
        }
        "text_end" => {
            AssistantMessageEvent::TextEnd { content_index: index()?, content: text("content"), partial: partial()? }
        }
        "thinking_start" => AssistantMessageEvent::ThinkingStart { content_index: index()?, partial: partial()? },
        "thinking_delta" => {
            AssistantMessageEvent::ThinkingDelta { content_index: index()?, delta: text("delta"), partial: partial()? }
        }
        "thinking_end" => AssistantMessageEvent::ThinkingEnd {
            content_index: index()?,
            content: text("content"),
            partial: partial()?,
        },
        "toolcall_start" => AssistantMessageEvent::ToolcallStart { content_index: index()?, partial: partial()? },
        "toolcall_delta" => {
            AssistantMessageEvent::ToolcallDelta { content_index: index()?, delta: text("delta"), partial: partial()? }
        }
        "toolcall_end" => AssistantMessageEvent::ToolcallEnd {
            content_index: index()?,
            tool_call: serde_json::from_value(value["toolCall"].clone()).ok()?,
            partial: partial()?,
        },
        "done" => AssistantMessageEvent::Done {
            reason: reason(),
            message: serde_json::from_value(value["message"].clone()).ok()?,
        },
        "error" => AssistantMessageEvent::Error {
            reason: serde_json::from_value(value["reason"].clone()).unwrap_or(StopReason::Error),
            error: serde_json::from_value(value["error"].clone()).ok()?,
        },
        _ => return None,
    })
}
