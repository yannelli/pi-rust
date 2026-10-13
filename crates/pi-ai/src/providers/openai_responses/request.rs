//! Request body for `openai-responses`, ported from upstream `api/openai-responses.ts`
//! (`buildParams`) with the `streamSimple` option mapping.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::convert::{ConvertOptions, convert_messages};
use crate::providers::common::{compat_bool, compat_str};
use crate::providers::constrained_sampling::{
    ResponsesToolOptions, convert_responses_tools, grammar_tool_input_properties,
};
use crate::registry::{CacheRetention, StreamOptions};
use crate::simple_options::{
    MappedLevel, clamp_max_tokens_to_context, clamp_openai_prompt_cache_key, clamp_thinking_level,
    mapped_thinking_level, provider_thinking_value, resolve_cache_retention, resolve_sampling_params,
};
use crate::transcript::{declared_tools, resolve_transcript_tools};
use crate::types::{Message, Model, ThinkingLevel};

/// OpenAI rejects `max_output_tokens` below 16.
const MIN_OUTPUT_TOKENS: u64 = 16;

/// Resolved `OpenAIResponsesCompat` flags with upstream defaults.
pub(crate) struct Compat {
    pub supports_mid_convo_system_messages: bool,
    pub session_affinity_format: String,
    pub supports_long_cache_retention: bool,
    pub supports_strict_mode: bool,
    pub supports_openai_grammar_tools: bool,
    pub supports_additional_tools: bool,
    pub supports_tool_search: bool,
    pub supports_explicit_prompt_cache_mode: bool,
    pub supports_max_output_tokens: bool,
}

impl Compat {
    pub fn resolve(model: &Model) -> Self {
        let flag = |key: &str, default: bool| compat_bool(model, key).unwrap_or(default);
        let openrouter = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
        let detected = if openrouter { "openrouter" } else { "openai" };
        Self {
            supports_mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            session_affinity_format: compat_str(model, "sessionAffinityFormat").unwrap_or(detected).to_string(),
            supports_long_cache_retention: flag("supportsLongCacheRetention", true),
            supports_strict_mode: flag("supportsStrictMode", false),
            supports_openai_grammar_tools: flag("supportsOpenAIGrammarTools", false),
            supports_additional_tools: flag("supportsAdditionalTools", false),
            supports_tool_search: flag("supportsToolSearch", false),
            supports_explicit_prompt_cache_mode: flag("supportsExplicitPromptCacheMode", false),
            supports_max_output_tokens: flag("supportsMaxOutputTokens", true),
        }
    }
}

/// OpenAI API keys start with `sk-`; another credential sent to OpenAI is a Sign in with ChatGPT token.
fn chatgpt_sign_in(model: &Model, options: &StreamOptions) -> bool {
    model.provider == "openai"
        && model.base_url == "https://api.openai.com/v1"
        && options.api_key.as_deref().is_some_and(|key| !key.starts_with("sk-"))
}

/// The effort to request: the clamped level, `None` when off.
pub(crate) fn reasoning_effort(model: &Model, options: &StreamOptions) -> Option<ThinkingLevel> {
    let level = clamp_thinking_level(model, options.reasoning?);
    (level != ThinkingLevel::Off).then_some(level)
}

/// Grammar tools by name with their input property, for the request and its stream.
pub(crate) fn grammar_properties(messages: &[Message], compat: &Compat) -> Result<BTreeMap<String, String>, String> {
    let properties = grammar_tool_input_properties(&declared_tools(messages), compat.supports_openai_grammar_tools)?;
    Ok(properties.into_iter().collect())
}

/// Build the body from a resolved transcript (`messages` starts with the leading system message).
pub(crate) fn build_body(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    compat: &Compat,
    grammar: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let tools = resolve_transcript_tools(messages, compat.supports_additional_tools || compat.supports_tool_search);
    let tool_options = ResponsesToolOptions {
        supports_strict_mode: compat.supports_strict_mode,
        supports_openai_grammar_tools: compat.supports_openai_grammar_tools,
        ..ResponsesToolOptions::default()
    };
    let convert = ConvertOptions {
        supports_mid_convo_system_messages: compat.supports_mid_convo_system_messages,
        supports_additional_tools: compat.supports_additional_tools,
        supports_tool_search: compat.supports_tool_search,
        grammar_tool_input_properties: grammar.clone(),
        tool_options,
        ..ConvertOptions::default()
    };
    let retention = resolve_cache_retention(options.cache_retention, &options.env);
    let omit_unsupported = chatgpt_sign_in(model, options);
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("input".into(), Value::Array(convert_messages(model, messages, &convert)?));
    body.insert("stream".into(), json!(true));
    if let (Some(session), false) = (&options.session_id, retention == CacheRetention::None) {
        body.insert("prompt_cache_key".into(), json!(clamp_openai_prompt_cache_key(session)));
    }
    let long = retention == CacheRetention::Long && compat.supports_long_cache_retention;
    if !omit_unsupported && long && !compat.supports_explicit_prompt_cache_mode {
        body.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if !omit_unsupported && compat.supports_explicit_prompt_cache_mode {
        if retention == CacheRetention::None {
            body.insert("prompt_cache_options".into(), json!({"mode": "explicit"}));
        } else if long {
            body.insert("prompt_cache_options".into(), json!({"ttl": "30m"}));
        }
    }
    body.insert("store".into(), json!(false));
    let max_tokens = clamp_max_tokens_to_context(model, messages, options.max_tokens.unwrap_or(model.max_tokens));
    if max_tokens > 0 && compat.supports_max_output_tokens && !omit_unsupported {
        body.insert("max_output_tokens".into(), json!(max_tokens.max(MIN_OUTPUT_TOKENS)));
    }
    if let (Some(temperature), false) = (options.temperature, omit_unsupported) {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(tier) = &options.service_tier {
        body.insert("service_tier".into(), json!(tier));
    }
    if !tools.request_tools.is_empty() {
        body.insert("tools".into(), Value::Array(convert_responses_tools(&tools.request_tools, tool_options)?));
    }
    if let Some(choice) = &options.tool_choice {
        body.insert("tool_choice".into(), json!(choice));
    }
    let effort = reasoning_effort(model, options);
    if model.reasoning {
        match effort {
            Some(level) => {
                let effort = provider_thinking_value(model, level).unwrap_or_else(|| level.as_str().to_string());
                body.insert("reasoning".into(), json!({"effort": effort, "summary": "auto"}));
                body.insert("include".into(), json!(["reasoning.encrypted_content"]));
            }
            None if model.provider != "github-copilot" => match mapped_thinking_level(model, ThinkingLevel::Off) {
                MappedLevel::Unsupported => {}
                MappedLevel::Default => {
                    body.insert("reasoning".into(), json!({"effort": "none"}));
                }
                MappedLevel::Value(value) => {
                    body.insert("reasoning".into(), json!({"effort": value}));
                }
            },
            None => {}
        }
        if model.provider == "xai" {
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        }
    }
    let level = effort.unwrap_or(ThinkingLevel::Off);
    if let Some(sampling) = resolve_sampling_params(model, level, options.sampling_params.as_ref()) {
        body.extend(sampling);
    }
    Ok(Value::Object(body))
}
