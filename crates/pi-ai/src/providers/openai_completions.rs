//! OpenAI Chat Completions (`POST {baseUrl}/chat/completions`) with SSE
//! streaming, including the common OpenAI-compatible server variants.

mod reasoning_details;
mod request;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use serde_json::Value;

use self::reasoning_details::{append_reasoning_detail, is_reasoning_detail};
use self::request::{Compat, REASONING_FIELDS, build_body};
use crate::providers::common::{SseHandler, as_u64, endpoint, header_map, spawn_sse_request};
use crate::registry::{ApiProvider, StreamOptions, http_client};
use crate::sse::SseEvent;
use crate::stream::{AssistantMessageEventStream, MessageBuilder};
use crate::types::{ContentBlock, Context, Model, ModelCost, StopReason, Usage};

pub struct OpenAICompletions;

impl ApiProvider for OpenAICompletions {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let compat = Compat::resolve(model);
        let body = build_body(model, context, &options, &compat);
        let mut defaults = Vec::new();
        if let Some(key) = options.api_key.as_deref().filter(|key| !key.is_empty()) {
            defaults.push(("authorization", format!("Bearer {key}")));
        }
        if let (Some(session), true) = (&options.session_id, compat.send_session_affinity_headers) {
            if compat.session_affinity_format == "openrouter" {
                defaults.push(("x-session-id", session.clone()));
            } else {
                if compat.session_affinity_format == "openai" {
                    defaults.push(("session_id", session.clone()));
                }
                defaults.push(("x-client-request-id", session.clone()));
                defaults.push(("x-session-affinity", session.clone()));
            }
        }
        let request = http_client()
            .post(endpoint(&model.base_url, "/chat/completions"))
            .headers(header_map(defaults, model, &options))
            .json(&body);
        let handler = CompletionsStream {
            cost: model.cost.clone(),
            opencode_go: model.provider == "opencode-go",
            supports_finish_reason: compat.supports_finish_reason,
            tools_by_index: HashMap::new(),
            tools_by_id: HashMap::new(),
            finish: None,
            reasoning_details: Vec::new(),
        };
        spawn_sse_request(model, request, options.cancel, handler)
    }
}

struct CompletionsStream {
    cost: ModelCost,
    opencode_go: bool,
    supports_finish_reason: bool,
    tools_by_index: HashMap<u64, usize>,
    tools_by_id: HashMap<String, usize>,
    finish: Option<Result<StopReason, String>>,
    reasoning_details: Vec<Value>,
}

impl SseHandler for CompletionsStream {
    fn event(&mut self, builder: &mut MessageBuilder, event: SseEvent) -> Result<bool, String> {
        let data = event.data.trim();
        if data == "[DONE]" {
            return Ok(false);
        }
        if data.is_empty() {
            return Ok(true);
        }
        let chunk: Value = serde_json::from_str(data).map_err(|error| format!("Invalid stream chunk: {error}"))?;
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            let message =
                error.get("message").and_then(Value::as_str).map_or_else(|| error.to_string(), str::to_string);
            return Err(message);
        }
        if builder.message().response_id.is_none()
            && let Some(id) = chunk.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())
        {
            builder.message_mut().response_id = Some(id.to_string());
        }
        let Some(choice) = chunk.get("choices").and_then(|choices| choices.get(0)) else {
            self.apply_usage(builder, chunk.get("usage"));
            return Ok(true);
        };
        self.apply_usage(builder, chunk.get("usage").filter(|usage| usage.is_object()).or_else(|| choice.get("usage")));
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(map_finish_reason(reason));
        }
        let Some(delta) = choice.get("delta") else { return Ok(true) };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            builder.text_delta(text);
        }
        let reasoning = REASONING_FIELDS.iter().find_map(|field| {
            delta.get(*field).and_then(Value::as_str).filter(|text| !text.is_empty()).map(|text| (*field, text))
        });
        if let Some((field, text)) = reasoning {
            let field = if self.opencode_go && field == "reasoning" { "reasoning_content" } else { field };
            builder.thinking_delta(text);
            builder.thinking_signature(field.to_string(), false);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.tool_call_delta(builder, call);
            }
        }
        if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
            for detail in details.iter().filter(|detail| is_reasoning_detail(detail)) {
                let has_thinking =
                    builder.message().content.iter().any(|block| matches!(block, ContentBlock::Thinking { .. }));
                if !has_thinking {
                    builder.thinking_delta("");
                }
                append_reasoning_detail(&mut self.reasoning_details, detail);
            }
        }
        Ok(true)
    }

    fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, String> {
        if !self.reasoning_details.is_empty() {
            let signature = Value::Array(std::mem::take(&mut self.reasoning_details)).to_string();
            let thinking = builder.message_mut().content.iter_mut().find_map(|block| match block {
                ContentBlock::Thinking { thinking_signature, .. } => Some(thinking_signature),
                _ => None,
            });
            if let Some(slot) = thinking {
                *slot = Some(signature);
            }
        }
        match self.finish.take() {
            Some(result) => result,
            None if !self.supports_finish_reason => Ok(StopReason::Stop),
            None => Err("Stream ended without finish_reason".into()),
        }
    }
}

impl CompletionsStream {
    fn apply_usage(&self, builder: &mut MessageBuilder, raw: Option<&Value>) {
        let Some(raw) = raw.filter(|raw| raw.is_object()) else { return };
        let prompt = as_u64(raw.get("prompt_tokens"));
        let cache_read = [
            raw.pointer("/prompt_tokens_details/cached_tokens"),
            raw.get("prompt_cache_hit_tokens"),
            raw.get("cached_tokens"),
        ]
        .into_iter()
        .flatten()
        .find_map(Value::as_u64)
        .unwrap_or_default();
        let cache_write = as_u64(raw.pointer("/prompt_tokens_details/cache_write_tokens"));
        let input = prompt.saturating_sub(cache_read + cache_write);
        let output = as_u64(raw.get("completion_tokens"));
        let mut usage = Usage {
            input,
            output,
            cache_read,
            cache_write,
            reasoning: raw.pointer("/completion_tokens_details/reasoning_tokens").and_then(Value::as_u64),
            total_tokens: input + output + cache_read + cache_write,
            ..Usage::default()
        };
        usage.apply_cost(&self.cost);
        builder.message_mut().usage = usage;
    }

    fn tool_call_delta(&mut self, builder: &mut MessageBuilder, call: &Value) {
        let index = call.get("index").and_then(Value::as_u64);
        let id = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
        let name = call.pointer("/function/name").and_then(Value::as_str).filter(|name| !name.is_empty());
        let existing = index
            .and_then(|index| self.tools_by_index.get(&index))
            .or_else(|| id.and_then(|id| self.tools_by_id.get(id)))
            .copied();
        let handle = match existing {
            Some(handle) => {
                if let Some(tool_call) = builder.tool_call_mut(handle) {
                    if let (true, Some(id)) = (tool_call.id.is_empty(), id) {
                        tool_call.id = id.to_string();
                    }
                    if let (true, Some(name)) = (tool_call.name.is_empty(), name) {
                        tool_call.name = name.to_string();
                    }
                }
                handle
            }
            None => builder.tool_call_start(id.unwrap_or_default(), name.unwrap_or_default()),
        };
        if let Some(index) = index {
            self.tools_by_index.entry(index).or_insert(handle);
        }
        if let Some(id) = id {
            self.tools_by_id.insert(id.to_string(), handle);
        }
        if let Some(arguments) =
            call.pointer("/function/arguments").and_then(Value::as_str).filter(|args| !args.is_empty())
        {
            builder.tool_call_delta(handle, arguments);
        }
    }
}

fn map_finish_reason(reason: &str) -> Result<StopReason, String> {
    match reason {
        "stop" | "end" => Ok(StopReason::Stop),
        "length" => Ok(StopReason::Length),
        "function_call" | "tool_calls" => Ok(StopReason::ToolUse),
        other => Err(format!("Provider finish_reason: {other}")),
    }
}
