//! Normalization of Mistral chat completion chunks.

use std::collections::HashMap;

use serde_json::Value;

use super::request::derive_tool_call_id;
use crate::providers::common::SseHandler;
use crate::sse::SseEvent;
use crate::stream::MessageBuilder;
use crate::types::{ModelCost, StopReason, Usage};

/// Usage keys that may carry cached prompt tokens, in priority order.
const CACHED_TOKEN_PATHS: [&str; 6] = [
    "/promptTokensDetails/cachedTokens",
    "/prompt_tokens_details/cached_tokens",
    "/promptTokenDetails/cachedTokens",
    "/prompt_token_details/cached_tokens",
    "/numCachedTokens",
    "/num_cached_tokens",
];

/// Stop reason for a finish reason, with the error message for error stops.
pub(super) fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" => (StopReason::Stop, None),
        "length" | "model_length" => (StopReason::Length, None),
        "tool_calls" => (StopReason::ToolUse, None),
        // Transient server failures; "server error" makes the message retryable.
        "error" => (StopReason::Error, Some("Provider stopped with: error (server error)".into())),
        other => (StopReason::Error, Some(format!("Provider stopped with: {other}"))),
    }
}

pub(super) struct MistralStream {
    cost: ModelCost,
    /// Tool call handles by stream index (or id when the index is missing).
    tools: HashMap<String, usize>,
    stop: Option<StopReason>,
    error: Option<String>,
}

impl MistralStream {
    pub fn new(cost: ModelCost) -> Self {
        Self { cost, tools: HashMap::new(), stop: None, error: None }
    }

    fn usage(&self, builder: &mut MessageBuilder, raw: &Value) {
        let prompt = raw["prompt_tokens"].as_u64().unwrap_or_default();
        let cached = CACHED_TOKEN_PATHS.iter().find_map(|path| raw.pointer(path).filter(|value| !value.is_null()));
        let cached = cached.and_then(Value::as_f64).filter(|value| value.is_finite()).unwrap_or(0.0);
        let cached = (cached.max(0.0) as u64).min(prompt);
        let mut usage = Usage {
            input: prompt - cached,
            output: raw["completion_tokens"].as_u64().unwrap_or_default(),
            cache_read: cached,
            ..Usage::default()
        };
        usage.total_tokens =
            raw["total_tokens"].as_u64().filter(|total| *total > 0).unwrap_or(usage.input + usage.output + cached);
        usage.apply_cost(&self.cost);
        builder.message_mut().usage = usage;
    }

    fn content(builder: &mut MessageBuilder, item: &Value) {
        match item {
            Value::String(text) => builder.text_delta(text),
            Value::Object(chunk) => match chunk.get("type").and_then(Value::as_str) {
                Some("thinking") => {
                    let parts = chunk.get("thinking").and_then(Value::as_array).into_iter().flatten();
                    let text: String = parts.filter_map(|part| part["text"].as_str()).collect();
                    if !text.is_empty() {
                        builder.thinking_delta(&text);
                    }
                }
                Some("text") => builder.text_delta(chunk.get("text").and_then(Value::as_str).unwrap_or_default()),
                _ => {}
            },
            _ => {}
        }
    }

    fn tool_call(&mut self, builder: &mut MessageBuilder, call: &Value) {
        builder.close_open_block();
        let index = call["index"].as_u64();
        let id = match call["id"].as_str().filter(|id| !id.is_empty() && *id != "null") {
            Some(id) => id.to_string(),
            None => derive_tool_call_id(&format!("toolcall:{}", index.unwrap_or(0)), 0),
        };
        let key = index.map_or_else(|| format!("id:{id}"), |index| format!("index:{index}"));
        let handle = match self.tools.get(&key) {
            Some(handle) => *handle,
            None => {
                let handle = builder
                    .tool_call_start(id, call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default());
                self.tools.insert(key, handle);
                handle
            }
        };
        let delta = match call.pointer("/function/arguments") {
            Some(Value::String(arguments)) => arguments.clone(),
            Some(arguments) if !arguments.is_null() => arguments.to_string(),
            _ => "{}".to_string(),
        };
        builder.tool_call_delta(handle, &delta);
    }
}

impl SseHandler for MistralStream {
    fn event(&mut self, builder: &mut MessageBuilder, event: SseEvent) -> Result<bool, String> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(true);
        }
        if data == "[DONE]" {
            return Ok(false);
        }
        let chunk: Value =
            serde_json::from_str(data).map_err(|error| format!("Invalid Mistral streaming event: {error}"))?;
        let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
            return Err("Invalid Mistral streaming event".into());
        };
        if builder.message().response_id.is_none()
            && let Some(id) = chunk["id"].as_str().filter(|id| !id.is_empty())
        {
            builder.message_mut().response_id = Some(id.to_string());
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.usage(builder, usage);
        }
        let Some(choice) = choices.first() else { return Ok(true) };
        if let Some(reason) = choice["finish_reason"].as_str() {
            builder.message_mut().raw_stop_reason = Some(reason.to_string());
            let (stop, error) = map_stop_reason(reason);
            self.stop = Some(stop);
            if error.is_some() {
                self.error = error;
            }
        }
        match &choice["delta"]["content"] {
            Value::Array(items) => items.iter().for_each(|item| Self::content(builder, item)),
            text @ Value::String(_) => Self::content(builder, text),
            _ => {}
        }
        for call in choice["delta"]["tool_calls"].as_array().into_iter().flatten() {
            self.tool_call(builder, call);
        }
        Ok(true)
    }

    fn end(&mut self, _builder: &mut MessageBuilder) -> Result<StopReason, String> {
        match self.stop {
            None => Err("Mistral stream ended without a finish reason".into()),
            Some(StopReason::Error | StopReason::Aborted) => {
                Err(self.error.take().unwrap_or_else(|| "An unknown error occurred".into()))
            }
            Some(reason) => Ok(reason),
        }
    }
}
