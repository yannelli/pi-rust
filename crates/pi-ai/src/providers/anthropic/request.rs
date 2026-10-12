//! Request building for `anthropic-messages`, ported from upstream `api/anthropic-messages.ts`
//! (`streamSimple`, `buildParams`, `getBetaFeatures`).

use serde_json::{Map, Value, json};

use super::convert::{
    ConvertOptions, ToolFormat, convert_messages, convert_tools, deferred_tool_placeholder,
    insert_thinking_level_messages, normalize_tool_call_id,
};
use crate::providers::common::{compat_bool, compat_str, compat_value, transform_messages};
use crate::registry::{CacheRetention, StreamOptions};
use crate::simple_options::{
    MIN_ANSWER_TOKENS, MappedLevel, adjust_max_tokens_for_thinking, clamp_max_tokens_to_context, mapped_thinking_level,
    resolve_cache_retention,
};
use crate::transcript::{current_tools, initial_system_message, system_message_text};
use crate::types::{Message, Model, ThinkingLevel};

pub(super) const CLAUDE_CODE_VERSION: &str = "2.1.280";
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MID_CONVERSATION_OUTPUT_CONFIG_BETA: &str = "mid-conversation-output-config-2026-07-01";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";

/// Resolved `AnthropicMessagesCompat` flags with upstream defaults.
pub(super) struct Compat {
    pub eager_input_streaming: bool,
    pub long_cache_retention: bool,
    pub send_session_affinity_headers: bool,
    pub session_affinity_format: Option<String>,
    pub cache_control_on_tools: bool,
    pub temperature: bool,
    pub allow_empty_signature: bool,
    pub mid_convo_system_messages: bool,
    pub mid_convo_tool_changes: bool,
    pub force_adaptive_thinking: bool,
    pub mid_convo_effort: bool,
    pub fallback_models: Vec<String>,
}

impl Compat {
    pub fn resolve(model: &Model) -> Self {
        let flag = |key: &str, default: bool| compat_bool(model, key).unwrap_or(default);
        let openrouter = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
        let fallback_models = compat_value(model, "allowedFallbackModels")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(|entry| entry["model"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        Self {
            eager_input_streaming: flag("supportsEagerToolInputStreaming", true),
            long_cache_retention: flag("supportsLongCacheRetention", true),
            send_session_affinity_headers: flag("sendSessionAffinityHeaders", openrouter),
            session_affinity_format: compat_str(model, "sessionAffinityFormat")
                .map(str::to_string)
                .or(openrouter.then(|| "openrouter".to_string())),
            cache_control_on_tools: flag("supportsCacheControlOnTools", true),
            temperature: flag("supportsTemperature", true),
            allow_empty_signature: flag("allowEmptySignature", false),
            mid_convo_system_messages: flag("supportsMidConvoSystemMessages", false),
            mid_convo_tool_changes: flag("supportsMidConvoToolChanges", false),
            force_adaptive_thinking: compat_bool(model, "forceAdaptiveThinking") == Some(true),
            mid_convo_effort: compat_bool(model, "supportsMidConvoEffort") == Some(true),
            fallback_models,
        }
    }
}

/// Thinking and output settings derived from the request's reasoning level.
pub(super) struct Thinking {
    pub enabled: bool,
    pub max_tokens: u64,
    pub budget_tokens: u64,
    /// Adaptive effort; only set for `forceAdaptiveThinking` models.
    pub effort: Option<String>,
}

/// Map pi reasoning options onto Anthropic thinking, as upstream `streamSimple` does.
pub(super) fn resolve_thinking(
    model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    compat: &Compat,
) -> Thinking {
    let base = clamp_max_tokens_to_context(model, messages, options.max_tokens.unwrap_or(model.max_tokens));
    let disabled = Thinking { enabled: false, max_tokens: base, budget_tokens: 0, effort: None };
    let Some(level) = options.reasoning.filter(|level| *level != ThinkingLevel::Off) else { return disabled };
    if compat.force_adaptive_thinking {
        return Thinking { enabled: true, effort: Some(adaptive_effort(model, level)), ..disabled };
    }
    let (adjusted, budget) =
        adjust_max_tokens_for_thinking(Some(base), model.max_tokens, level, options.thinking_budgets.as_ref());
    let max_tokens = clamp_max_tokens_to_context(model, messages, adjusted);
    let budget = budget.min(max_tokens.saturating_sub(MIN_ANSWER_TOKENS));
    Thinking { enabled: true, max_tokens, budget_tokens: if budget == 0 { 1024 } else { budget }, effort: None }
}

fn adaptive_effort(model: &Model, level: ThinkingLevel) -> String {
    if let MappedLevel::Value(value) = mapped_thinking_level(model, level) {
        return value.to_string();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
    .to_string()
}

/// `{type: ephemeral}` with a 1h TTL for long retention; `None` when caching is off.
pub(super) fn cache_control(retention: CacheRetention, compat: &Compat) -> Option<Value> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Long if compat.long_cache_retention => Some(json!({"type": "ephemeral", "ttl": "1h"})),
        _ => Some(json!({"type": "ephemeral"})),
    }
}

pub(super) struct Params<'a> {
    pub model: &'a Model,
    /// Resolved transcript, leading system message first.
    pub messages: &'a [Message],
    pub options: &'a StreamOptions,
    pub compat: &'a Compat,
    pub thinking: &'a Thinking,
    pub oauth: bool,
}

/// The request body and the `anthropic-beta` features.
pub(super) fn build_params(params: &Params) -> (Value, Vec<String>) {
    let Params { model, messages, options, compat, thinking, oauth } = *params;
    let cache_control = cache_control(resolve_cache_retention(options.cache_retention, &options.env), compat);
    let initial = initial_system_message(messages);
    let initial_text = initial.map(system_message_text).unwrap_or_default();
    let transformed = transform_messages(messages, model, &|id, _| normalize_tool_call_id(id));
    let conversation = if initial.is_some() { &transformed[1..] } else { &transformed[..] };
    let initial_tools = initial.and_then(|system| system.tools_added.as_deref()).unwrap_or_default();
    let native_tools = compat.mid_convo_system_messages && compat.mid_convo_tool_changes && !initial_tools.is_empty();
    let format = ToolFormat { oauth, eager_input_streaming: compat.eager_input_streaming };
    let converted = convert_messages(
        conversation,
        &ConvertOptions {
            oauth,
            cache_control: cache_control.as_ref(),
            allow_empty_signature: compat.allow_empty_signature,
            managed_provider: compat.mid_convo_effort.then_some(model.provider.as_str()),
            native_tools: native_tools.then_some(format),
        },
    );
    let betas = beta_features(params, native_tools);
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    let wire_messages = if compat.mid_convo_effort {
        insert_thinking_level_messages(converted, thinking.effort.as_deref().unwrap_or("high"))
    } else {
        converted.messages
    };
    body.insert("messages".into(), Value::Array(wire_messages));
    body.insert("max_tokens".into(), json!(thinking.max_tokens));
    body.insert("stream".into(), json!(true));
    let text_block = |text: &str| {
        let mut block = json!({"type": "text", "text": text});
        if let Some(cache_control) = &cache_control {
            block["cache_control"] = cache_control.clone();
        }
        block
    };
    let mut system = Vec::new();
    if oauth {
        system.push(text_block(CLAUDE_CODE_IDENTITY));
    }
    if !initial_text.is_empty() {
        system.push(text_block(&initial_text));
    }
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }
    if let Some(temperature) = options.temperature
        && !thinking.enabled
        && !compat.mid_convo_effort
        && compat.temperature
    {
        body.insert("temperature".into(), json!(temperature));
    }
    let tool_cache = cache_control.as_ref().filter(|_| compat.cache_control_on_tools);
    if native_tools {
        let mut tools = convert_tools(initial_tools, format, tool_cache);
        tools.push(deferred_tool_placeholder());
        body.insert("tools".into(), Value::Array(tools));
    } else {
        let tools = current_tools(messages);
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(convert_tools(&tools, format, tool_cache)));
        }
    }
    insert_thinking(&mut body, model, compat, thinking);
    if let Some(user_id) = options.metadata.as_ref().and_then(|metadata| metadata.get("user_id")?.as_str()) {
        body.insert("metadata".into(), json!({"user_id": user_id}));
    }
    if let Some(choice) = &options.tool_choice {
        body.insert("tool_choice".into(), json!({"type": choice}));
    }
    if !compat.fallback_models.is_empty() {
        let fallbacks: Vec<Value> = compat.fallback_models.iter().map(|model| json!({"model": model})).collect();
        body.insert("fallbacks".into(), Value::Array(fallbacks));
    }
    (Value::Object(body), betas)
}

fn insert_thinking(body: &mut Map<String, Value>, model: &Model, compat: &Compat, thinking: &Thinking) {
    if compat.mid_convo_effort {
        // Managed effort models always think adaptively, so prefix mismatches drop blocks.
        body.insert(
            "thinking".into(),
            json!({"type": "adaptive", "display": "summarized",
                "block_binding": {"prefix_mismatch_behavior": "drop_block"}}),
        );
        body.insert("output_config".into(), json!({"effort": "high"}));
    } else if model.reasoning && thinking.enabled {
        if compat.force_adaptive_thinking {
            body.insert("thinking".into(), json!({"type": "adaptive", "display": "summarized"}));
            if let Some(effort) = &thinking.effort {
                body.insert("output_config".into(), json!({"effort": effort}));
            }
        } else {
            let budget = thinking.budget_tokens;
            body.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": budget, "display": "summarized"}),
            );
        }
    } else if model.reasoning && mapped_thinking_level(model, ThinkingLevel::Off) != MappedLevel::Unsupported {
        body.insert("thinking".into(), json!({"type": "disabled"}));
    }
}

/// Configured `anthropic-beta` header values replace the computed features; a suppressed
/// header sends none.
fn beta_features(params: &Params, native_tools: bool) -> Vec<String> {
    let Params { model, messages, options, compat, thinking, oauth } = *params;
    let is_beta = |name: &String| name.eq_ignore_ascii_case("anthropic-beta");
    if options.suppressed_headers.iter().any(is_beta) {
        return Vec::new();
    }
    let configured = model.headers.iter().chain(&options.headers).rev().find(|(name, _)| is_beta(name));
    let mut features: Vec<String> = Vec::new();
    let mut push = |feature: &str| {
        if !feature.is_empty() && !features.iter().any(|existing| existing == feature) {
            features.push(feature.to_string());
        }
    };
    if let Some((_, value)) = configured {
        value.split(',').map(str::trim).for_each(&mut push);
        return features;
    }
    if oauth {
        push("claude-code-20250219");
        push("oauth-2025-04-20");
    }
    if !compat.eager_input_streaming && !current_tools(messages).is_empty() {
        push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if model.reasoning && thinking.enabled && !compat.force_adaptive_thinking {
        push(INTERLEAVED_THINKING_BETA);
    }
    if !compat.fallback_models.is_empty() {
        push(SERVER_SIDE_FALLBACK_BETA);
    }
    if compat.mid_convo_effort {
        push(MID_CONVERSATION_OUTPUT_CONFIG_BETA);
        push(THINKING_BINDING_CONTROLS_BETA);
    }
    if native_tools {
        push(INLINE_TOOLS_BETA);
    }
    features
}
