//! Stream event handling for `anthropic-messages`, ported from upstream `api/anthropic-messages.ts`
//! (`iterateAnthropicEvents` and the event loop of `stream`).

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::convert::from_claude_code_name;
use crate::json::parse_json_with_repair;
use crate::providers::common::{SseHandler, compat_value};
use crate::sse::SseEvent;
use crate::stream::{MessageBuilder, diagnostic};
use crate::types::{Model, ModelCost, StopReason, Tool};

const MESSAGE_EVENTS: [&str; 6] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

enum Block {
    Text,
    Thinking { signature: String },
    Redacted,
    Tool(usize),
}

pub(super) struct AnthropicStream {
    model: Model,
    cost: ModelCost,
    /// Current tools, for mapping Claude Code casing back under OAuth; empty otherwise.
    oauth_tools: Vec<Tool>,
    provider_thinking_level: Option<String>,
    blocks: HashMap<u64, Block>,
    stop: Option<Result<StopReason, String>>,
    input_transformations: Option<Vec<Value>>,
    started: bool,
    stopped: bool,
}

impl AnthropicStream {
    pub(super) fn new(model: &Model, oauth_tools: Vec<Tool>, provider_thinking_level: Option<String>) -> Self {
        Self {
            model: model.clone(),
            cost: model.cost.clone(),
            oauth_tools,
            provider_thinking_level,
            blocks: HashMap::new(),
            stop: None,
            input_transformations: None,
            started: false,
            stopped: false,
        }
    }
}

impl SseHandler for AnthropicStream {
    fn start(&mut self, builder: &mut MessageBuilder) {
        builder.message_mut().provider_thinking_level = self.provider_thinking_level.clone();
    }

    fn event(&mut self, builder: &mut MessageBuilder, event: SseEvent) -> Result<bool, String> {
        let name = event.event.as_deref().unwrap_or_default();
        if name == "error" {
            return Err(event.data);
        }
        if !MESSAGE_EVENTS.contains(&name) {
            return Ok(true);
        }
        let data = parse_json_with_repair(&event.data).map_err(|error| {
            let raw: Vec<String> = std::iter::once(format!("event: {name}"))
                .chain(event.data.lines().map(|line| format!("data: {line}")))
                .collect();
            format!("Could not parse Anthropic SSE event {name}: {error}; data={}; raw={}", event.data, raw.join("\\n"))
        })?;
        let index = data["index"].as_u64().unwrap_or_default();
        match data["type"].as_str().unwrap_or(name) {
            "message_start" => self.message_start(builder, &data["message"]),
            "content_block_start" => self.start_block(builder, index, &data["content_block"])?,
            "content_block_delta" => {
                let delta = &data["delta"];
                let text = |key: &str| delta[key].as_str().unwrap_or_default();
                match (delta["type"].as_str(), self.blocks.get_mut(&index)) {
                    (Some("text_delta"), Some(Block::Text)) => builder.text_delta(text("text")),
                    (Some("thinking_delta"), Some(Block::Thinking { .. })) => builder.thinking_delta(text("thinking")),
                    (Some("signature_delta"), Some(Block::Thinking { signature })) => {
                        signature.push_str(text("signature"))
                    }
                    (Some("input_json_delta"), Some(Block::Tool(handle))) => {
                        builder.tool_call_delta(*handle, text("partial_json"));
                    }
                    _ => {}
                }
            }
            "content_block_stop" => match self.blocks.remove(&index) {
                Some(Block::Thinking { signature }) => {
                    if !signature.is_empty() {
                        builder.thinking_signature(signature, false);
                    }
                    builder.close_open_block();
                }
                Some(Block::Text | Block::Redacted) => builder.close_open_block(),
                Some(Block::Tool(handle)) => builder.end_tool_call(handle),
                None => {}
            },
            "message_delta" => self.message_delta(builder, &data)?,
            "message_stop" => self.stopped = true,
            _ => {}
        }
        Ok(true)
    }

    fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, String> {
        if self.started && !self.stopped {
            return Err("Anthropic stream ended before message_stop".into());
        }
        let reason = self.stop.take().unwrap_or_else(|| Err("Anthropic stream ended without a stop reason".into()))?;
        if let Some(transformations) = self.input_transformations.take().filter(|list| !list.is_empty()) {
            let transformations: Vec<Value> = transformations.iter().map(transformation_summary).collect();
            let details = Map::from_iter([("transformations".to_string(), Value::Array(transformations))]);
            builder.add_diagnostic(diagnostic("anthropic_input_transformations", None, Some(details)));
        }
        Ok(reason)
    }
}

/// `{type, path, reason}` of one input transformation, without absent fields.
fn transformation_summary(transformation: &Value) -> Value {
    let fields = ["type", "path", "reason"].into_iter().filter_map(|key| {
        let value = transformation.get(key).filter(|value| !value.is_null())?;
        Some((key.to_string(), value.clone()))
    });
    Value::Object(fields.collect())
}

impl AnthropicStream {
    fn message_start(&mut self, builder: &mut MessageBuilder, message: &Value) {
        self.started = true;
        if let Some(id) = message["id"].as_str() {
            builder.message_mut().response_id = Some(id.to_string());
        }
        if let Some(list) = message["input_transformations"].as_array() {
            self.input_transformations = Some(list.clone());
        }
        if let Some(response_model) = message["model"].as_str().filter(|id| *id != self.model.id) {
            builder.message_mut().response_model = Some(response_model.to_string());
            if let Some(cost) = self.fallback_cost(response_model) {
                self.cost = cost;
            }
        }
        let raw = &message["usage"];
        let count = |key: &str| raw[key].as_u64().unwrap_or_default();
        let usage = &mut builder.message_mut().usage;
        usage.input = count("input_tokens");
        usage.output = count("output_tokens");
        usage.cache_read = count("cache_read_input_tokens");
        usage.cache_write = count("cache_creation_input_tokens");
        usage.cache_write_1h = Some(raw.pointer("/cache_creation/ephemeral_1h_input_tokens").map_or(0, as_count));
        usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
        usage.apply_cost(&self.cost);
    }

    /// Local pricing of a server-side fallback model from `compat.allowedFallbackModels`.
    fn fallback_cost(&self, response_model: &str) -> Option<ModelCost> {
        let fallbacks = compat_value(&self.model, "allowedFallbackModels")?.as_array()?;
        let fallback = fallbacks.iter().find(|fallback| {
            fallback["provider"].as_str() == Some(self.model.provider.as_str())
                && fallback["model"].as_str() == Some(response_model)
        })?;
        serde_json::from_value(fallback["cost"].clone()).ok()
    }

    fn message_delta(&mut self, builder: &mut MessageBuilder, data: &Value) -> Result<(), String> {
        if let Some(list) = data["input_transformations"].as_array() {
            self.input_transformations = Some(list.clone());
        }
        if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
            builder.message_mut().raw_stop_reason = Some(reason.to_string());
            self.stop = Some(map_stop_reason(reason, data.pointer("/delta/stop_details/explanation"))?);
        }
        let raw = &data["usage"];
        let usage = &mut builder.message_mut().usage;
        if raw.is_object() {
            let set = |field: &mut u64, key: &str| {
                if let Some(value) = raw.get(key).and_then(Value::as_u64) {
                    *field = value;
                }
            };
            set(&mut usage.input, "input_tokens");
            set(&mut usage.output, "output_tokens");
            set(&mut usage.cache_read, "cache_read_input_tokens");
            set(&mut usage.cache_write, "cache_creation_input_tokens");
            if let Some(long) = raw.pointer("/cache_creation/ephemeral_1h_input_tokens").and_then(Value::as_u64) {
                usage.cache_write_1h = Some(long);
            }
            if let Some(thinking) = raw.pointer("/output_tokens_details/thinking_tokens").and_then(Value::as_u64) {
                usage.reasoning = Some(thinking);
            }
        }
        usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
        usage.apply_cost(&self.cost);
        Ok(())
    }

    fn start_block(&mut self, builder: &mut MessageBuilder, index: u64, block: &Value) -> Result<(), String> {
        let text = |key: &str| block[key].as_str().unwrap_or_default();
        let entry = match block["type"].as_str() {
            Some("fallback") if !builder.message().content.is_empty() => {
                return Err("Anthropic performed an unsupported mid-output model fallback".into());
            }
            Some("text") => {
                builder.close_open_block();
                builder.text_delta(text("text"));
                Block::Text
            }
            Some("thinking") => {
                builder.close_open_block();
                builder.thinking_delta(text("thinking"));
                Block::Thinking { signature: text("signature").to_string() }
            }
            Some("redacted_thinking") => {
                builder.close_open_block();
                builder.thinking_delta("[Reasoning redacted]");
                builder.thinking_signature(text("data").to_string(), true);
                Block::Redacted
            }
            Some("tool_use") => {
                let name = text("name");
                let name = if self.oauth_tools.is_empty() {
                    name.to_string()
                } else {
                    from_claude_code_name(name, &self.oauth_tools)
                };
                let handle = builder.tool_call_start(text("id"), name);
                if let Some(input) = block["input"].as_object().filter(|input| !input.is_empty()) {
                    builder.tool_call_arguments(handle, input.clone());
                }
                Block::Tool(handle)
            }
            _ => return Ok(()),
        };
        self.blocks.insert(index, entry);
        Ok(())
    }
}

fn as_count(value: &Value) -> u64 {
    value.as_u64().unwrap_or_default()
}

/// The stop reason, or an error message for refusals and filtered output. Unknown reasons fail
/// the stream right away.
fn map_stop_reason(reason: &str, explanation: Option<&Value>) -> Result<Result<StopReason, String>, String> {
    Ok(match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => Ok(StopReason::Stop),
        "max_tokens" => Ok(StopReason::Length),
        "tool_use" => Ok(StopReason::ToolUse),
        "refusal" => Err(explanation
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("The model refused to complete the request")
            .to_string()),
        "sensitive" => Err("Provider stopped with: sensitive".into()),
        other => return Err(format!("Unhandled stop reason: {other}")),
    })
}
