//! OpenAI Responses (`POST {baseUrl}/responses`) with SSE streaming.

mod request;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use serde_json::{Value, json};

use self::request::build_body;
use crate::json::parse_streaming_json;
use crate::providers::common::{SseHandler, as_u64, compat_str, endpoint, has_header, header_map, spawn_sse_request};
use crate::registry::{ApiProvider, StreamOptions, http_client};
use crate::sse::SseEvent;
use crate::stream::{AssistantMessageEventStream, MessageBuilder, error_stream};
use crate::types::{ContentBlock, Context, JsonObject, Model, ModelCost, StopReason, Usage};

pub struct OpenAIResponses;

impl ApiProvider for OpenAIResponses {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let api_key = options.api_key.clone().filter(|key| !key.is_empty());
        let header_auth =
            ["authorization", "cf-aig-authorization"].iter().any(|name| has_header(model, &options, name));
        if api_key.is_none() && !header_auth {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        }
        let body = build_body(model, context, &options);
        let mut defaults = Vec::new();
        if let Some(key) = api_key {
            defaults.push(("authorization", format!("Bearer {key}")));
        }
        if let Some(session) = &options.session_id {
            let detected = if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
                "openrouter"
            } else {
                "openai"
            };
            match compat_str(model, "sessionAffinityFormat").unwrap_or(detected) {
                "openrouter" => defaults.push(("x-session-id", session.clone())),
                format => {
                    if format == "openai" {
                        defaults.push(("session_id", session.clone()));
                    }
                    defaults.push(("x-client-request-id", session.clone()));
                }
            }
        }
        let request = http_client()
            .post(endpoint(&model.base_url, "/responses"))
            .headers(header_map(defaults, model, &options))
            .json(&body);
        let handler = ResponsesStream { cost: model.cost.clone(), ..ResponsesStream::default() };
        spawn_sse_request(model, request, options.cancel, handler)
    }
}

enum Slot {
    /// Content index of the thinking block.
    Thinking(usize),
    /// Text streamed so far.
    Text(String),
    Tool {
        handle: usize,
        json: String,
    },
}

#[derive(Default)]
struct ResponsesStream {
    cost: ModelCost,
    slots: HashMap<u64, Slot>,
    reasoning_blocks: HashMap<String, usize>,
    unfinished_tools: HashMap<usize, String>,
    terminal: Option<Result<StopReason, String>>,
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
                if let Some(Slot::Tool { handle, json }) = self.slots.get_mut(&output_index) {
                    json.push_str(delta);
                    builder.tool_call_delta(*handle, delta);
                }
            }
            "response.function_call_arguments.done" => {
                if let (Some(Slot::Tool { handle, json }), Some(arguments)) =
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
            "response.output_item.done" => self.finish_item(builder, output_index, &data["item"]),
            "response.completed" | "response.incomplete" => self.finalize(builder, &data["response"]),
            "error" => {
                let code = data["code"].as_str().map_or_else(|| data["code"].to_string(), str::to_string);
                return Err(format!("Error Code {code}: {}", data["message"].as_str().unwrap_or("Unknown error")));
            }
            "response.failed" => return Err(failure_message(&data["response"])),
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
}

impl ResponsesStream {
    fn create_slot(&mut self, builder: &mut MessageBuilder, output_index: u64, item: &Value) {
        let slot = match item["type"].as_str() {
            Some("reasoning") => {
                builder.thinking_delta("");
                Slot::Thinking(builder.message().content.len() - 1)
            }
            Some("message") => Slot::Text(String::new()),
            Some("function_call") => {
                let id = format!(
                    "{}|{}",
                    item["call_id"].as_str().unwrap_or_default(),
                    item["id"].as_str().unwrap_or_default()
                );
                let handle = builder.tool_call_start(id.clone(), item["name"].as_str().unwrap_or_default());
                let json = item["arguments"].as_str().unwrap_or_default().to_string();
                if !json.is_empty() {
                    builder.tool_call_delta(handle, &json);
                }
                self.unfinished_tools.insert(handle, format!("{} ({id})", item["name"].as_str().unwrap_or_default()));
                Slot::Tool { handle, json }
            }
            _ => return,
        };
        self.slots.insert(output_index, slot);
    }

    fn finish_item(&mut self, builder: &mut MessageBuilder, output_index: u64, item: &Value) {
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
            (Some(Slot::Tool { handle, json }), Some("function_call")) => {
                let arguments = item["arguments"].as_str().filter(|arguments| !arguments.is_empty()).unwrap_or(&json);
                builder.tool_call_arguments(handle, parse_object(arguments));
                self.unfinished_tools.remove(&handle);
            }
            (Some(slot), _) => {
                self.slots.insert(output_index, slot);
            }
            (None, _) => {}
        }
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
        if let Some(id) = response["id"].as_str() {
            builder.message_mut().response_id = Some(id.to_string());
        }
        if let Some(raw) = response.get("usage").filter(|usage| usage.is_object()) {
            let cache_read = as_u64(raw.pointer("/input_tokens_details/cached_tokens"));
            let cache_write = as_u64(raw.pointer("/input_tokens_details/cache_write_tokens"));
            let mut usage = Usage {
                input: as_u64(raw.get("input_tokens")).saturating_sub(cache_read + cache_write),
                output: as_u64(raw.get("output_tokens")),
                cache_read,
                cache_write,
                reasoning: raw.pointer("/output_tokens_details/reasoning_tokens").and_then(Value::as_u64),
                total_tokens: as_u64(raw.get("total_tokens")),
                ..Usage::default()
            };
            usage.apply_cost(&self.cost);
            builder.message_mut().usage = usage;
        }
        let reason = response.pointer("/incomplete_details/reason").and_then(Value::as_str);
        self.terminal = Some(match (response["status"].as_str(), reason) {
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
