//! Azure OpenAI Responses (`POST {base}/responses?api-version=...`). Messages, tools, and the
//! event stream reuse the `openai-responses` conversion and handler.

mod config;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use self::config::{resolve_deployment_name, responses_url};
use crate::http::pi_user_agent;
use crate::providers::common::{HttpRequest, compat_bool, header_map};
use crate::providers::constrained_sampling::{
    ResponsesToolOptions, convert_responses_tools, resolve_grammar_constrained_sampling,
};
use crate::providers::http_stream::{ResponseReport, SendConfig, spawn_builder_stream};
use crate::providers::openai_responses::{ConvertOptions, ResponsesStream, convert_messages};
use crate::registry::{ApiProvider, StreamOptions};
use crate::request::{MAX_PROVIDER_ERROR_BODY_CHARS, apply_payload_hook, truncate_error_text};
use crate::retry::ProviderRequestError;
use crate::simple_options::{
    MappedLevel, clamp_max_tokens_to_context, clamp_openai_prompt_cache_key, clamp_thinking_level,
    mapped_thinking_level, provider_thinking_value, resolve_sampling_params,
};
use crate::stream::{AssistantMessageEventStream, error_stream};
use crate::transcript::{normalize_context, resolve_transcript, resolve_transcript_tools};
use crate::types::{Context, Message, Model, StopReason, ThinkingLevel};

/// Providers whose `call_id|item_id` tool call ids are replayed with their item id.
const TOOL_CALL_PROVIDERS: &[&str] = &["openai", "openai-codex", "opencode", "azure"];
/// OpenAI Responses rejects `max_output_tokens` below 16.
const MIN_OUTPUT_TOKENS: u64 = 16;

pub struct AzureOpenAIResponses;

impl ApiProvider for AzureOpenAIResponses {
    fn supports_system_messages(&self) -> bool {
        true
    }

    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let Some(api_key) = options.api_key.clone().filter(|key| !key.is_empty()) else {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        };
        let url = match responses_url(model, &options) {
            Ok(url) => url,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let (body, grammar) = match build_body(model, context, &options) {
            Ok(built) => built,
            Err(error) => return error_stream(model, StopReason::Error, error),
        };
        let body = apply_payload_hook(body, options.on_payload.as_ref(), model);
        let defaults = vec![("user-agent", pi_user_agent()), ("api-key", api_key)];
        let request = HttpRequest { url, headers: header_map(defaults, model, &options), body };
        let config = SendConfig {
            max_retries: options.max_retries.unwrap_or(0),
            timeout: options.timeout_ms.map(|ms| (ms, "Request timed out.".to_string())),
            report: ResponseReport::SuccessOnly,
        };
        let handler = ResponsesStream::new(model).with_grammar_tool_input_properties(grammar);
        spawn_builder_stream(model, options, request, config, azure_error, handler)
    }
}

/// Upstream `formatProviderError(error, "Azure OpenAI API error")` over an OpenAI SDK error.
fn azure_error(error: ProviderRequestError) -> String {
    let Some(status) = error.status else { return error.message };
    let body = error.body.unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(&body).ok();
    let detail = parsed.as_ref().and_then(|value| value.get("error")).filter(|detail| !detail.is_null());
    if let Some(Value::Object(detail)) = detail
        && !detail.is_empty()
    {
        let body = Value::Object(detail.clone()).to_string();
        return format!(
            "Azure OpenAI API error ({status}): {}",
            truncate_error_text(&body, MAX_PROVIDER_ERROR_BODY_CHARS)
        );
    }
    let message = match (detail, &parsed) {
        (Some(detail), _) => {
            detail.get("message").and_then(Value::as_str).map_or_else(|| detail.to_string(), str::to_string)
        }
        (None, None) if !body.trim().is_empty() => body,
        _ => String::new(),
    };
    let message =
        if message.is_empty() { format!("{status} status code (no body)") } else { format!("{status} {message}") };
    format!("Azure OpenAI API error ({status}): {message}")
}

/// Grammar tools by name with the argument property that carries their raw input.
fn grammar_properties(messages: &[Message], supports_grammar: bool) -> Result<BTreeMap<String, String>, String> {
    let declared = messages.iter().filter_map(|message| match message {
        Message::System(system) => system.tools_added.as_ref(),
        _ => None,
    });
    let mut properties = BTreeMap::new();
    for tool in declared.flatten() {
        if let Some(grammar) = resolve_grammar_constrained_sampling(tool, supports_grammar)? {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

/// The request body (upstream `buildParams`) and the grammar tool input properties.
fn build_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<(Value, BTreeMap<String, String>), String> {
    let flag = |key: &str, default: bool| compat_bool(model, key).unwrap_or(default);
    let (supports_strict, supports_grammar) =
        (flag("supportsStrictMode", true), flag("supportsOpenAIGrammarTools", false));
    let (additional_tools, tool_search) = (flag("supportsAdditionalTools", false), flag("supportsToolSearch", false));
    let mid_convo = flag("supportsMidConvoSystemMessages", false);
    let messages = resolve_transcript(normalize_context(context), mid_convo);
    let grammar = grammar_properties(&messages, supports_grammar)?;
    let tools = resolve_transcript_tools(&messages, additional_tools || tool_search);
    let convert = ConvertOptions {
        supports_mid_convo_system_messages: mid_convo,
        supports_additional_tools: additional_tools,
        supports_tool_search: tool_search,
        tool_call_providers: TOOL_CALL_PROVIDERS,
        grammar_tool_input_properties: grammar.clone(),
        tool_strict: supports_strict.then_some(false),
        ..ConvertOptions::default()
    };
    let mut body = Map::new();
    body.insert("model".into(), json!(resolve_deployment_name(model, options)));
    body.insert("input".into(), Value::Array(convert_messages(model, &messages, &convert)));
    body.insert("stream".into(), json!(true));
    if let Some(session) = &options.session_id {
        body.insert("prompt_cache_key".into(), json!(clamp_openai_prompt_cache_key(session)));
    }
    body.insert("store".into(), json!(false));
    let max_tokens = clamp_max_tokens_to_context(model, &messages, options.max_tokens.unwrap_or(model.max_tokens));
    if max_tokens > 0 {
        body.insert("max_output_tokens".into(), json!(max_tokens.max(MIN_OUTPUT_TOKENS)));
    }
    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if !tools.request_tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(convert_responses_tools(
                &tools.request_tools,
                ResponsesToolOptions {
                    supports_strict_mode: supports_strict,
                    supports_openai_grammar_tools: supports_grammar,
                    ..ResponsesToolOptions::default()
                },
            )?),
        );
    }
    if let Some(choice) = &options.tool_choice {
        body.insert("tool_choice".into(), json!(choice));
    }
    let effort =
        options.reasoning.map(|level| clamp_thinking_level(model, level)).filter(|level| *level != ThinkingLevel::Off);
    if model.reasoning {
        match effort {
            Some(level) => {
                let value = provider_thinking_value(model, level).unwrap_or_else(|| level.as_str().to_string());
                body.insert("reasoning".into(), json!({"effort": value, "summary": "auto"}));
                body.insert("include".into(), json!(["reasoning.encrypted_content"]));
            }
            None => match mapped_thinking_level(model, ThinkingLevel::Off) {
                MappedLevel::Unsupported => {}
                MappedLevel::Default => {
                    body.insert("reasoning".into(), json!({"effort": "none"}));
                }
                MappedLevel::Value(value) => {
                    body.insert("reasoning".into(), json!({"effort": value}));
                }
            },
        }
    }
    let level = effort.unwrap_or(ThinkingLevel::Off);
    if let Some(sampling) = resolve_sampling_params(model, level, options.sampling_params.as_ref()) {
        body.extend(sampling);
    }
    Ok((Value::Object(body), grammar))
}
