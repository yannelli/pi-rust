//! Request body for `openai-completions`, ported from upstream `buildParams` with the
//! `streamSimple` option mapping (clamped effort and context-clamped output cap).

use serde_json::{Map, Value, json};

use super::compat::Compat;
use super::convert::{GrammarInputs, apply_cache_control, convert_messages, convert_tools};
use super::thinking::{apply_thinking, reasoning_effort, thinking_budget};
use crate::registry::{CacheRetention, StreamOptions};
use crate::simple_options::{
    clamp_max_tokens_to_context, clamp_openai_prompt_cache_key, resolve_cache_retention, resolve_sampling_params,
};
use crate::transcript::resolve_transcript_tools;
use crate::types::{Message, Model, ThinkingLevel};

/// Proxies in front of Anthropic require `tools` whenever the history has tool calls or results.
fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant.tool_calls().next().is_some(),
        Message::System(_) | Message::User(_) | Message::Custom(_) => false,
    })
}

/// JavaScript truthiness for compat values that upstream tests with `if (value)`.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Messages and options that shape one request body.
pub(super) struct BodyInput<'a> {
    /// The resolved transcript.
    pub messages: &'a [Message],
    /// The normalized transcript, used to clamp the output cap to the context window.
    pub estimate: &'a [Message],
    pub grammar: &'a GrammarInputs,
}

/// Build the body. Fails when a tool requires constrained sampling the server lacks.
pub(super) fn build_body(
    model: &Model,
    input: &BodyInput,
    options: &StreamOptions,
    compat: &Compat,
) -> Result<Value, String> {
    let messages = input.messages;
    let tools = resolve_transcript_tools(messages, compat.anchors_tool_additions());
    let mut converted = convert_messages(model, messages, compat, input.grammar)?;
    let retention = resolve_cache_retention(options.cache_retention, &options.env);
    let long = retention == CacheRetention::Long && compat.supports_long_cache_retention;
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    // Placeholder that keeps upstream field order; filled once cache markers are applied.
    body.insert("messages".into(), Value::Null);
    body.insert("stream".into(), json!(true));
    let direct_openai = model.base_url.contains("api.openai.com") && retention != CacheRetention::None;
    if let (Some(session), true) = (&options.session_id, direct_openai || long) {
        body.insert("prompt_cache_key".into(), json!(clamp_openai_prompt_cache_key(session)));
    }
    if long {
        body.insert("prompt_cache_retention".into(), json!("24h"));
    }
    if compat.supports_usage_in_streaming {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if compat.supports_store {
        body.insert("store".into(), json!(false));
    }
    let max_tokens = clamp_max_tokens_to_context(model, input.estimate, options.max_tokens.unwrap_or(model.max_tokens));
    if max_tokens > 0 {
        let field = if compat.max_tokens_field == "max_tokens" { "max_tokens" } else { "max_completion_tokens" };
        body.insert(field.into(), json!(max_tokens));
    }
    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    let mut request_tools = None;
    if !tools.request_tools.is_empty() {
        request_tools = Some(convert_tools(&tools.request_tools, compat)?);
        if compat.zai_tool_stream {
            body.insert("tool_stream".into(), json!(true));
        }
    } else if has_tool_history(messages) {
        request_tools = Some(Vec::new());
    }
    if compat.anthropic_cache_control && retention != CacheRetention::None {
        let mut cache_control = json!({"type": "ephemeral"});
        if long {
            cache_control["ttl"] = json!("1h");
        }
        apply_cache_control(&mut converted, request_tools.as_mut(), &cache_control);
    }
    body.insert("messages".into(), Value::Array(converted));
    if let Some(request_tools) = request_tools {
        body.insert("tools".into(), Value::Array(request_tools));
    }
    if let Some(choice) = &options.tool_choice {
        body.insert("tool_choice".into(), json!(choice));
    }
    if let Some(priority) = &compat.vllm_priority {
        body.insert("priority".into(), priority.clone());
    }
    let effort = reasoning_effort(model, options);
    let ceiling = if max_tokens > 0 { max_tokens } else { model.max_tokens };
    let budget = thinking_budget(model, options, effort, ceiling);
    apply_thinking(model, compat, effort, budget, &mut body);
    if let (Some(field), Some(budget)) = (&compat.thinking_budget_field, budget) {
        body.insert(field.clone(), json!(budget));
    }
    if let Some(routing) = compat.open_router_routing.as_ref().filter(|routing| truthy(routing)) {
        body.insert("provider".into(), routing.clone());
    }
    if let Some(routing) = &compat.vercel_gateway_routing {
        let gateway: Map<String, Value> = ["only", "order"]
            .into_iter()
            .filter_map(|key| {
                routing.get(key).filter(|value| truthy(value)).map(|value| (key.to_string(), value.clone()))
            })
            .collect();
        if !gateway.is_empty() {
            body.insert("providerOptions".into(), json!({"gateway": gateway}));
        }
    }
    let level = effort.unwrap_or(ThinkingLevel::Off);
    if let Some(sampling) = resolve_sampling_params(model, level, options.sampling_params.as_ref()) {
        body.extend(sampling);
    }
    Ok(Value::Object(body))
}
