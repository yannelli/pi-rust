//! Chat Completions chunk handling, ported from the streaming loop of upstream
//! `api/openai-completions.ts`. One text block and one thinking block collect their deltas for
//! the whole response, so the handler emits those block events itself.

use serde_json::Value;

use super::convert::{GrammarInputs, REASONING_FIELDS};
use super::reasoning_details::{append_reasoning_detail, is_reasoning_detail};
use super::tool_calls::ToolCalls;
use crate::providers::common::SseHandler;
use crate::sse::SseEvent;
use crate::stream::{AssistantMessageEvent, EventSender, MessageBuilder};
use crate::types::{ContentBlock, Model, ModelCost, StopReason, Usage};

pub(super) struct CompletionsStream {
    sender: EventSender,
    model_id: String,
    cost: ModelCost,
    opencode_go: bool,
    supports_finish_reason: bool,
    text: Option<usize>,
    thinking: Option<usize>,
    tools: ToolCalls,
    finish: Option<Result<StopReason, String>>,
    reasoning_details: Option<Vec<Value>>,
}

impl CompletionsStream {
    pub fn new(model: &Model, supports_finish_reason: bool, grammar: GrammarInputs, sender: EventSender) -> Self {
        Self {
            sender,
            model_id: model.id.clone(),
            cost: model.cost.clone(),
            opencode_go: model.provider == "opencode-go",
            supports_finish_reason,
            text: None,
            thinking: None,
            tools: ToolCalls::new(grammar),
            finish: None,
            reasoning_details: None,
        }
    }
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
        if !chunk.is_object() {
            return Ok(true);
        }
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            self.apply_reasoning_details(builder);
            return Err(chunk_error_message(error));
        }
        let message = builder.message_mut();
        if let (None, Some(id)) = (&message.response_id, chunk.get("id").and_then(Value::as_str)) {
            message.response_id = Some(id.to_string()).filter(|id| !id.is_empty());
        }
        if let Some(routed) = chunk.get("model").and_then(Value::as_str)
            && !routed.is_empty()
            && routed != self.model_id
            && message.response_model.is_none()
        {
            message.response_model = Some(routed.to_string());
        }
        let chunk_usage = chunk.get("usage").filter(|usage| usage.is_object());
        if let Some(usage) = chunk_usage {
            message.usage = parse_usage(usage, &self.cost);
        }
        let Some(choice) = chunk.get("choices").and_then(Value::as_array).and_then(|choices| choices.first()) else {
            return Ok(true);
        };
        // Some providers (for example Moonshot) report usage on the choice instead.
        if let (None, Some(usage)) = (chunk_usage, choice.get("usage").filter(|usage| usage.is_object())) {
            builder.message_mut().usage = parse_usage(usage, &self.cost);
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str).filter(|reason| !reason.is_empty()) {
            builder.message_mut().raw_stop_reason = Some(reason.to_string());
            self.finish = Some(map_finish_reason(reason));
        }
        let Some(delta) = choice.get("delta").filter(|delta| delta.is_object()) else { return Ok(true) };
        if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|text| !text.is_empty()) {
            self.text_delta(builder, text);
        }
        let reasoning = REASONING_FIELDS.iter().find_map(|field| {
            delta.get(*field).and_then(Value::as_str).filter(|text| !text.is_empty()).map(|text| (*field, text))
        });
        if let Some((field, text)) = reasoning {
            let signature = if self.opencode_go && field == "reasoning" { "reasoning_content" } else { field };
            self.thinking_delta(builder, signature, text);
        }
        for call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
            if let Err(error) = self.tools.delta(builder, &self.sender, call) {
                self.apply_reasoning_details(builder);
                return Err(error);
            }
        }
        let details = delta.get("reasoning_details").and_then(Value::as_array);
        for detail in details.into_iter().flatten().filter(|detail| is_reasoning_detail(detail)) {
            self.ensure_thinking(builder, "");
            append_reasoning_detail(self.reasoning_details.get_or_insert_default(), detail);
        }
        Ok(true)
    }

    fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, String> {
        self.finish_blocks(builder)?;
        match self.finish.take() {
            Some(result) => result,
            None if !self.supports_finish_reason => Ok(StopReason::Stop),
            None => Err("Stream ended without finish_reason".into()),
        }
    }
}

impl CompletionsStream {
    fn push(&self, event: AssistantMessageEvent) {
        self.sender.push(event);
    }

    fn text_delta(&mut self, builder: &mut MessageBuilder, delta: &str) {
        let index = match self.text {
            Some(index) => index,
            None => {
                let content = &mut builder.message_mut().content;
                content.push(ContentBlock::text(""));
                let index = content.len() - 1;
                self.text = Some(index);
                self.push(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: builder.message().clone(),
                });
                index
            }
        };
        if let ContentBlock::Text { text, .. } = &mut builder.message_mut().content[index] {
            text.push_str(delta);
        }
        let partial = builder.message().clone();
        self.push(AssistantMessageEvent::TextDelta { content_index: index, delta: delta.to_string(), partial });
    }

    /// The thinking block, created with `signature` when the response has none yet.
    fn ensure_thinking(&mut self, builder: &mut MessageBuilder, signature: &str) -> usize {
        if let Some(index) = self.thinking {
            return index;
        }
        let content = &mut builder.message_mut().content;
        content.push(ContentBlock::Thinking {
            thinking: String::new(),
            thinking_signature: Some(signature.to_string()),
            redacted: false,
        });
        let index = content.len() - 1;
        self.thinking = Some(index);
        self.push(AssistantMessageEvent::ThinkingStart { content_index: index, partial: builder.message().clone() });
        index
    }

    fn thinking_delta(&mut self, builder: &mut MessageBuilder, signature: &str, delta: &str) {
        let index = self.ensure_thinking(builder, signature);
        if let ContentBlock::Thinking { thinking, .. } = &mut builder.message_mut().content[index] {
            thinking.push_str(delta);
        }
        let partial = builder.message().clone();
        self.push(AssistantMessageEvent::ThinkingDelta { content_index: index, delta: delta.to_string(), partial });
    }

    /// Store the streamed `reasoning_details` as the thinking signature for replay.
    fn apply_reasoning_details(&mut self, builder: &mut MessageBuilder) {
        let (Some(index), Some(details)) = (self.thinking, &self.reasoning_details) else { return };
        let signature = Value::Array(details.clone()).to_string();
        if let ContentBlock::Thinking { thinking_signature, .. } = &mut builder.message_mut().content[index] {
            *thinking_signature = Some(signature);
        }
    }

    /// Finalize every block in content order and emit its end event.
    fn finish_blocks(&mut self, builder: &mut MessageBuilder) -> Result<(), String> {
        self.apply_reasoning_details(builder);
        for index in 0..builder.message().content.len() {
            let message = builder.message();
            let event = match &message.content[index] {
                ContentBlock::Text { text, .. } if self.text == Some(index) => {
                    let content = text.clone();
                    AssistantMessageEvent::TextEnd { content_index: index, content, partial: message.clone() }
                }
                ContentBlock::Thinking { thinking, .. } if self.thinking == Some(index) => {
                    let content = thinking.clone();
                    AssistantMessageEvent::ThinkingEnd { content_index: index, content, partial: message.clone() }
                }
                ContentBlock::ToolCall(_) => {
                    self.tools.finish(builder, &self.sender, index)?;
                    continue;
                }
                _ => continue,
            };
            self.push(event);
        }
        Ok(())
    }
}

/// The in-stream error text, with OpenRouter's raw provider metadata when it adds detail.
fn chunk_error_message(error: &Value) -> String {
    let mut message = match error.get("message") {
        Some(Value::String(text)) if !text.is_empty() => text.clone(),
        None | Some(Value::Null | Value::String(_) | Value::Bool(false)) => error.to_string(),
        Some(other) => other.to_string(),
    };
    let raw = error.pointer("/metadata/raw").filter(|raw| !raw.is_null());
    let raw = raw.map(|raw| raw.as_str().map_or_else(|| raw.to_string(), str::to_string));
    if let Some(raw) = raw.filter(|raw| !raw.is_empty() && !message.contains(raw.as_str())) {
        message.push('\n');
        message.push_str(&raw);
    }
    message
}

fn number(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value.as_u64().or_else(|| value.as_f64().filter(|number| *number >= 0.0).map(|number| number as u64))
}

/// Usage from a chunk. `cached_tokens` counts cache reads and `cache_write_tokens` writes;
/// `completion_tokens` already includes reasoning tokens.
fn parse_usage(raw: &Value, cost: &ModelCost) -> Usage {
    let prompt = number(raw.get("prompt_tokens")).unwrap_or_default();
    let cache_read = [
        raw.pointer("/prompt_tokens_details/cached_tokens"),
        raw.get("prompt_cache_hit_tokens"),
        raw.get("cached_tokens"),
    ]
    .into_iter()
    .find_map(|value| value.filter(|value| !value.is_null()))
    .and_then(|value| number(Some(value)))
    .unwrap_or_default();
    let cache_write = number(raw.pointer("/prompt_tokens_details/cache_write_tokens")).unwrap_or_default();
    let input = prompt.saturating_sub(cache_read + cache_write);
    let output = number(raw.get("completion_tokens")).unwrap_or_default();
    let mut usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        reasoning: Some(number(raw.pointer("/completion_tokens_details/reasoning_tokens")).unwrap_or_default()),
        total_tokens: input + output + cache_read + cache_write,
        ..Usage::default()
    };
    usage.apply_cost(cost);
    usage
}

fn map_finish_reason(reason: &str) -> Result<StopReason, String> {
    match reason {
        "stop" | "end" => Ok(StopReason::Stop),
        "length" => Ok(StopReason::Length),
        "function_call" | "tool_calls" => Ok(StopReason::ToolUse),
        other => Err(format!("Provider finish_reason: {other}")),
    }
}
