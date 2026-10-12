//! Chat Completions chunk handling, ported from the streaming loop of upstream
//! `api/openai-completions.ts`. One text block and one thinking block collect their deltas for
//! the whole response, so the handler emits those block events itself.

use std::collections::HashMap;

use serde_json::Value;

use super::convert::REASONING_FIELDS;
use super::reasoning_details::{append_reasoning_detail, is_reasoning_detail};
use crate::json::parse_streaming_json;
use crate::providers::common::SseHandler;
use crate::sse::SseEvent;
use crate::stream::{AssistantMessageEvent, EventSender, MessageBuilder};
use crate::types::{ContentBlock, JsonObject, Model, ModelCost, StopReason, ToolCall, Usage};

/// A tool call being streamed: where it lives in the content and its raw argument JSON.
struct StreamingTool {
    content_index: usize,
    partial_args: String,
    stream_index: Option<u64>,
}

pub(super) struct CompletionsStream {
    sender: EventSender,
    model_id: String,
    cost: ModelCost,
    opencode_go: bool,
    supports_finish_reason: bool,
    text: Option<usize>,
    thinking: Option<usize>,
    tools: Vec<StreamingTool>,
    tools_by_index: HashMap<u64, usize>,
    tools_by_id: HashMap<String, usize>,
    finish: Option<Result<StopReason, String>>,
    reasoning_details: Option<Vec<Value>>,
}

impl CompletionsStream {
    pub fn new(model: &Model, supports_finish_reason: bool, sender: EventSender) -> Self {
        Self {
            sender,
            model_id: model.id.clone(),
            cost: model.cost.clone(),
            opencode_go: model.provider == "opencode-go",
            supports_finish_reason,
            text: None,
            thinking: None,
            tools: Vec::new(),
            tools_by_index: HashMap::new(),
            tools_by_id: HashMap::new(),
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
            self.tool_call_delta(builder, call);
        }
        let details = delta.get("reasoning_details").and_then(Value::as_array);
        for detail in details.into_iter().flatten().filter(|detail| is_reasoning_detail(detail)) {
            self.ensure_thinking(builder, "");
            append_reasoning_detail(self.reasoning_details.get_or_insert_default(), detail);
        }
        Ok(true)
    }

    fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, String> {
        self.finish_blocks(builder);
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

    /// Match a delta to its call by stream index, then by id, so providers that change ids
    /// mid-stream still extend one call.
    fn tool_call_delta(&mut self, builder: &mut MessageBuilder, call: &Value) {
        let stream_index = call.get("index").and_then(Value::as_u64);
        let id = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
        let name = [call.pointer("/function/name"), call.pointer("/custom/name")]
            .into_iter()
            .find_map(|name| name.and_then(Value::as_str))
            .filter(|name| !name.is_empty());
        let existing = stream_index
            .and_then(|index| self.tools_by_index.get(&index))
            .or_else(|| id.and_then(|id| self.tools_by_id.get(id)))
            .copied();
        let slot = match existing {
            Some(slot) => slot,
            None => self.start_tool_call(builder, stream_index, id, name),
        };
        if let (Some(index), None) = (stream_index, self.tools[slot].stream_index) {
            self.tools[slot].stream_index = Some(index);
            self.tools_by_index.insert(index, slot);
        }
        if let Some(id) = id {
            self.tools_by_id.insert(id.to_string(), slot);
        }
        let arguments = call.pointer("/function/arguments").and_then(Value::as_str).filter(|args| !args.is_empty());
        let tool = &mut self.tools[slot];
        if let Some(arguments) = arguments {
            tool.partial_args.push_str(arguments);
        }
        let parsed = arguments.map(|_| streaming_arguments(&tool.partial_args));
        let content_index = tool.content_index;
        if let ContentBlock::ToolCall(block) = &mut builder.message_mut().content[content_index] {
            if let (true, Some(id)) = (block.id.is_empty(), id) {
                block.id = id.to_string();
            }
            if let (true, Some(name)) = (block.name.is_empty(), name) {
                block.name = name.to_string();
            }
            if let Some(parsed) = parsed {
                block.arguments = parsed;
            }
        }
        let partial = builder.message().clone();
        let delta = arguments.unwrap_or_default().to_string();
        self.push(AssistantMessageEvent::ToolcallDelta { content_index, delta, partial });
    }

    fn start_tool_call(
        &mut self,
        builder: &mut MessageBuilder,
        stream_index: Option<u64>,
        id: Option<&str>,
        name: Option<&str>,
    ) -> usize {
        let content = &mut builder.message_mut().content;
        content.push(ContentBlock::ToolCall(ToolCall {
            id: id.unwrap_or_default().to_string(),
            name: name.unwrap_or_default().to_string(),
            ..Default::default()
        }));
        let content_index = content.len() - 1;
        let slot = self.tools.len();
        self.tools.push(StreamingTool { content_index, partial_args: String::new(), stream_index });
        if let Some(index) = stream_index {
            self.tools_by_index.insert(index, slot);
        }
        if let Some(id) = id {
            self.tools_by_id.insert(id.to_string(), slot);
        }
        self.push(AssistantMessageEvent::ToolcallStart { content_index, partial: builder.message().clone() });
        slot
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
    fn finish_blocks(&mut self, builder: &mut MessageBuilder) {
        self.apply_reasoning_details(builder);
        for tool in &self.tools {
            if let ContentBlock::ToolCall(call) = &mut builder.message_mut().content[tool.content_index] {
                call.arguments = streaming_arguments(&tool.partial_args);
            }
        }
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
                ContentBlock::ToolCall(call) => {
                    let tool_call = call.clone();
                    AssistantMessageEvent::ToolcallEnd { content_index: index, tool_call, partial: message.clone() }
                }
                _ => continue,
            };
            self.push(event);
        }
        self.text = None;
        self.thinking = None;
        self.tools.clear();
    }
}

/// Arguments from partial JSON; an unparseable prefix yields an empty object.
fn streaming_arguments(partial: &str) -> JsonObject {
    match parse_streaming_json(partial) {
        Some(Value::Object(arguments)) => arguments,
        _ => JsonObject::new(),
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
