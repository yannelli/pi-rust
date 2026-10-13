//! Responses API stream events to assistant message events, ported from upstream
//! `processResponsesStream` in `api/openai-responses-shared.ts`.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use crate::json::parse_streaming_json;
use crate::providers::common::{SseHandler, as_u64};
use crate::sse::SseEvent;
use crate::stream::MessageBuilder;
use crate::types::{ContentBlock, JsonObject, Model, ModelCost, StopReason, Usage};

const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/settings/usage";

/// Streamed raw input of a grammar (custom) tool, re-encoded as a JSON object delta.
#[derive(Default)]
struct GrammarInput {
    property: String,
    input: String,
    started: bool,
    closed: bool,
}

impl GrammarInput {
    /// The JSON text to append for `next` (the whole input so far), closing the object when `close`.
    fn append(&mut self, next: &str, close: bool) -> Result<Option<String>, String> {
        if self.closed {
            if close && next == self.input {
                return Ok(None);
            }
            return Err(format!("grammar tool input for property \"{}\" changed after it was closed", self.property));
        }
        let Some(delta) = next.strip_prefix(self.input.as_str()) else {
            return Err(format!("grammar tool input for property \"{}\" changed non-monotonically", self.property));
        };
        if !close && delta.is_empty() {
            return Ok(None);
        }
        let mut text = String::new();
        if !self.started {
            text.push_str(&format!("{{{}:\"", json!(self.property)));
            self.started = true;
        }
        let encoded = json!(delta).to_string();
        text.push_str(&encoded[1..encoded.len() - 1]);
        self.input = next.to_string();
        if close {
            text.push_str("\"}");
            self.closed = true;
        }
        Ok(Some(text))
    }
}

enum Slot {
    /// Content index of the thinking block.
    Thinking(usize),
    /// Text streamed so far.
    Text(String),
    Function {
        handle: usize,
        json: String,
    },
    Custom {
        handle: usize,
        grammar: GrammarInput,
    },
}

/// SSE handler for Responses API streams.
pub(crate) struct ResponsesStream {
    cost: ModelCost,
    grammar_properties: BTreeMap<String, String>,
    slots: HashMap<u64, Slot>,
    reasoning_blocks: HashMap<String, usize>,
    unfinished_tools: HashMap<usize, String>,
    terminal: Option<Result<StopReason, String>>,
    /// Model id and requested service tier, when tier pricing applies.
    service_tier_pricing: Option<(String, Option<String>)>,
}

/// Cost multiplier of an OpenAI service tier.
fn service_tier_multiplier(model_id: &str, tier: Option<&str>) -> f64 {
    match tier {
        Some("flex") => 0.5,
        Some("priority" | "fast") if model_id == "gpt-5.5" => 2.5,
        Some("priority" | "fast") => 2.0,
        _ => 1.0,
    }
}

impl ResponsesStream {
    pub(crate) fn new(model: &Model) -> Self {
        Self {
            cost: model.cost.clone(),
            grammar_properties: BTreeMap::new(),
            slots: HashMap::new(),
            reasoning_blocks: HashMap::new(),
            unfinished_tools: HashMap::new(),
            terminal: None,
            service_tier_pricing: None,
        }
    }

    /// Price usage by the service tier the response reports, else the requested one.
    pub(crate) fn with_service_tier_pricing(mut self, model: &Model, requested: Option<String>) -> Self {
        self.service_tier_pricing = Some((model.id.clone(), requested));
        self
    }

    /// Grammar tools by name, with the argument property that receives their raw input.
    pub(crate) fn with_grammar_tool_input_properties(mut self, properties: BTreeMap<String, String>) -> Self {
        self.grammar_properties = properties;
        self
    }
}

impl SseHandler for ResponsesStream {
    fn event(&mut self, builder: &mut MessageBuilder, event: SseEvent) -> Result<bool, String> {
        if event.data.trim().is_empty() || event.data.trim() == "[DONE]" {
            return Ok(true);
        }
        let data: Value =
            serde_json::from_str(&event.data).map_err(|error| format!("Invalid stream event: {error}"))?;
        let kind = data["type"].as_str().or(event.event.as_deref()).unwrap_or_default().to_string();
        let output_index = data["output_index"].as_u64().unwrap_or_default();
        let delta = data["delta"].as_str().unwrap_or_default();
        match kind.as_str() {
            "response.created" => {
                if let Some(id) = data.pointer("/response/id").and_then(Value::as_str) {
                    builder.message_mut().response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => self.create_slot(builder, output_index, &data["item"]),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(Slot::Thinking(_)) = self.slots.get(&output_index) {
                    builder.thinking_delta(delta);
                }
            }
            "response.reasoning_summary_part.done" => {
                if let Some(Slot::Thinking(_)) = self.slots.get(&output_index) {
                    builder.thinking_delta("\n\n");
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(Slot::Text(text)) = self.slots.get_mut(&output_index) {
                    text.push_str(delta);
                    builder.text_delta(delta);
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(Slot::Function { handle, json }) = self.slots.get_mut(&output_index) {
                    json.push_str(delta);
                    builder.tool_call_delta(*handle, delta);
                }
            }
            "response.function_call_arguments.done" => {
                if let (Some(Slot::Function { handle, json }), Some(arguments)) =
                    (self.slots.get_mut(&output_index), data["arguments"].as_str())
                {
                    match arguments.strip_prefix(json.as_str()) {
                        Some(rest) if !rest.is_empty() => builder.tool_call_delta(*handle, rest),
                        Some(_) => {}
                        None => builder.tool_call_arguments(*handle, parse_object(arguments)),
                    }
                    *json = arguments.to_string();
                }
            }
            "response.custom_tool_call_input.delta" | "response.custom_tool_call_input.done" => {
                if let Some(Slot::Custom { handle, grammar }) = self.slots.get_mut(&output_index) {
                    let done = kind.ends_with(".done");
                    let next = if done {
                        data["input"].as_str().unwrap_or_default().to_string()
                    } else {
                        format!("{}{delta}", grammar.input)
                    };
                    if let Some(text) = grammar.append(&next, done)? {
                        builder.tool_call_delta(*handle, &text);
                    }
                }
            }
            "response.output_item.done" => self.finish_item(builder, output_index, &data["item"])?,
            "response.completed" | "response.incomplete" => self.finalize(builder, &data["response"]),
            "error" => {
                let code = data["code"].as_str().map_or_else(|| data["code"].to_string(), str::to_string);
                return Err(format!("Error Code {code}: {}", data["message"].as_str().unwrap_or("Unknown error")));
            }
            "response.failed" => {
                let response = &data["response"];
                builder.message_mut().raw_stop_reason = response["status"].as_str().map(str::to_string);
                return Err(failure_message(response));
            }
            _ => {}
        }
        Ok(true)
    }

    fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, String> {
        let reason = self
            .terminal
            .take()
            .unwrap_or_else(|| Err("OpenAI Responses stream ended before a terminal response event".into()))?;
        let has_tools = builder.message().tool_calls().next().is_some();
        if (reason == StopReason::ToolUse || (reason == StopReason::Stop && has_tools))
            && let Some(name) = self.unfinished_tools.values().next()
        {
            return Err(format!("OpenAI Responses stream completed with an unfinished tool call: {name}"));
        }
        Ok(reason)
    }

    fn error_message(&self, message: String) -> String {
        if message.contains("subscription_sharing_usage_limit_exceeded") {
            return format!("{message}\nCheck your ChatGPT usage: {CHATGPT_USAGE_URL}");
        }
        message
    }
}

impl ResponsesStream {
    fn create_slot(&mut self, builder: &mut MessageBuilder, output_index: u64, item: &Value) {
        let text = |key: &str| item[key].as_str().unwrap_or_default().to_string();
        let slot = match item["type"].as_str() {
            Some("reasoning") => {
                builder.thinking_delta("");
                Slot::Thinking(builder.message().content.len() - 1)
            }
            Some("message") => Slot::Text(String::new()),
            Some(kind @ ("function_call" | "custom_tool_call")) => {
                let (id, name) = (format!("{}|{}", text("call_id"), text("id")), text("name"));
                let handle = builder.tool_call_start(id.clone(), name.clone());
                if let (Some(call), Some(namespace)) = (builder.tool_call_mut(handle), item["namespace"].as_str()) {
                    call.namespace = Some(namespace.to_string());
                }
                self.unfinished_tools.insert(handle, format!("{name} ({id})"));
                if kind == "function_call" {
                    let json = text("arguments");
                    if !json.is_empty() {
                        builder.tool_call_delta(handle, &json);
                    }
                    Slot::Function { handle, json }
                } else {
                    let property = self.grammar_properties.get(&name).cloned().unwrap_or_else(|| "input".into());
                    let arguments = JsonObject::from_iter([(property.clone(), json!(text("input")))]);
                    builder.tool_call_arguments(handle, arguments);
                    Slot::Custom { handle, grammar: GrammarInput { property, ..GrammarInput::default() } }
                }
            }
            _ => return,
        };
        self.slots.insert(output_index, slot);
    }

    fn finish_item(&mut self, builder: &mut MessageBuilder, output_index: u64, item: &Value) -> Result<(), String> {
        if !self.slots.contains_key(&output_index) {
            self.create_slot(builder, output_index, item);
        }
        match (self.slots.remove(&output_index), item["type"].as_str()) {
            (Some(Slot::Thinking(index)), Some("reasoning")) => {
                let join = |key: &str| {
                    let parts: Vec<&str> =
                        item[key].as_array().into_iter().flatten().filter_map(|part| part["text"].as_str()).collect();
                    parts.join("\n\n")
                };
                let (summary, content) = (join("summary"), join("content"));
                if let Some(ContentBlock::Thinking { thinking, thinking_signature, .. }) =
                    builder.message_mut().content.get_mut(index)
                {
                    if !summary.is_empty() || !content.is_empty() {
                        *thinking = if summary.is_empty() { content } else { summary };
                    }
                    *thinking_signature = Some(item.to_string());
                }
                if let Some(id) = item["id"].as_str() {
                    self.reasoning_blocks.insert(id.to_string(), index);
                }
                builder.close_open_block();
            }
            (Some(Slot::Text(streamed)), Some("message")) => {
                let parts = item["content"].as_array().into_iter().flatten();
                let text: String =
                    parts.filter_map(|part| part["text"].as_str().or(part["refusal"].as_str())).collect();
                match text.strip_prefix(streamed.as_str()) {
                    Some(rest) => builder.text_delta(rest),
                    None => replace_last_text(builder, text),
                }
                let mut signature = json!({"v": 1, "id": item["id"].as_str().unwrap_or_default()});
                if let Some(phase) = item["phase"].as_str() {
                    signature["phase"] = json!(phase);
                }
                builder.text_signature(signature.to_string());
                builder.close_open_block();
            }
            (Some(Slot::Function { handle, json }), Some("function_call")) => {
                let arguments = item["arguments"].as_str().filter(|arguments| !arguments.is_empty()).unwrap_or(&json);
                builder.tool_call_arguments(handle, parse_object(arguments));
                self.finish_tool(builder, handle, item);
            }
            (Some(Slot::Custom { handle, mut grammar }), Some("custom_tool_call")) => {
                let input = item["input"].as_str().map_or_else(|| grammar.input.clone(), str::to_string);
                if let Some(text) = grammar.append(&input, true)? {
                    builder.tool_call_delta(handle, &text);
                }
                builder.tool_call_arguments(handle, JsonObject::from_iter([(grammar.property, json!(input))]));
                self.finish_tool(builder, handle, item);
            }
            (Some(slot), _) => {
                self.slots.insert(output_index, slot);
            }
            (None, _) => {}
        }
        Ok(())
    }

    fn finish_tool(&mut self, builder: &mut MessageBuilder, handle: usize, item: &Value) {
        if let (Some(call), Some(namespace)) = (builder.tool_call_mut(handle), item["namespace"].as_str()) {
            call.namespace = Some(namespace.to_string());
        }
        self.unfinished_tools.remove(&handle);
    }

    fn finalize(&mut self, builder: &mut MessageBuilder, response: &Value) {
        for item in response["output"].as_array().into_iter().flatten() {
            let (Some(id), Some(encrypted)) = (item["id"].as_str(), item.get("encrypted_content")) else { continue };
            let Some(&index) = self.reasoning_blocks.get(id) else { continue };
            if let Some(ContentBlock::Thinking { thinking_signature: Some(signature), .. }) =
                builder.message_mut().content.get_mut(index)
                && let Ok(mut stored) = serde_json::from_str::<Value>(signature)
                && stored.get("encrypted_content").is_none_or(Value::is_null)
                && !encrypted.is_null()
            {
                stored["encrypted_content"] = encrypted.clone();
                *signature = stored.to_string();
            }
        }
        let message = builder.message_mut();
        if let Some(id) = response["id"].as_str() {
            message.response_id = Some(id.to_string());
        }
        if let Some(raw) = response.get("usage").filter(|usage| usage.is_object()) {
            let cache_read = as_u64(raw.pointer("/input_tokens_details/cached_tokens"));
            let cache_write = as_u64(raw.pointer("/input_tokens_details/cache_write_tokens"));
            message.usage = Usage {
                input: as_u64(raw.get("input_tokens")).saturating_sub(cache_read + cache_write),
                output: as_u64(raw.get("output_tokens")),
                cache_read,
                cache_write,
                reasoning: Some(as_u64(raw.pointer("/output_tokens_details/reasoning_tokens"))),
                total_tokens: as_u64(raw.get("total_tokens")),
                ..Usage::default()
            };
        }
        message.usage.apply_cost(&self.cost);
        if let Some((model_id, requested)) = &self.service_tier_pricing {
            let tier = response["service_tier"].as_str().or(requested.as_deref());
            let multiplier = service_tier_multiplier(model_id, tier);
            if multiplier != 1.0 {
                let cost = &mut message.usage.cost;
                for part in [&mut cost.input, &mut cost.output, &mut cost.cache_read, &mut cost.cache_write] {
                    *part *= multiplier;
                }
                cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
            }
        }
        let status = response["status"].as_str();
        let reason = response.pointer("/incomplete_details/reason").and_then(Value::as_str);
        message.raw_stop_reason = match (status, reason) {
            (Some(status), Some(reason)) => Some(format!("{status}.{reason}")),
            (status, _) => status.map(str::to_string),
        };
        self.terminal = Some(match (status, reason) {
            (None | Some("completed" | "in_progress" | "queued"), _) => Ok(StopReason::Stop),
            (Some("incomplete"), Some("max_output_tokens")) => Ok(StopReason::Length),
            (Some("incomplete"), Some(reason)) => Err(format!("Response incomplete: {reason}")),
            (Some("incomplete"), None) => Err("Response incomplete without a provider reason".into()),
            (Some("failed" | "cancelled"), _) => Err("An unknown error occurred".into()),
            (Some(other), _) => Err(format!("Unhandled stop reason: {other}")),
        });
    }
}

fn replace_last_text(builder: &mut MessageBuilder, replacement: String) {
    let last = builder.message_mut().content.iter_mut().rev().find_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(text),
        _ => None,
    });
    if let Some(text) = last {
        *text = replacement;
    }
}

fn parse_object(json: &str) -> JsonObject {
    match parse_streaming_json(json) {
        Some(Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

fn failure_message(response: &Value) -> String {
    if let Some(error) = response.get("error").filter(|error| error.is_object()) {
        let code = error["code"].as_str().unwrap_or("unknown");
        return format!("{code}: {}", error["message"].as_str().unwrap_or("no message"));
    }
    match response.pointer("/incomplete_details/reason").and_then(Value::as_str) {
        Some(reason) => format!("incomplete: {reason}"),
        None => "Unknown error (no error details in response)".into(),
    }
}
