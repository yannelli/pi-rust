//! Anthropic Messages (`POST {baseUrl}/v1/messages`) with SSE streaming.

mod request;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use serde_json::Value;

use self::request::{AnthropicRequest, CLAUDE_CODE_VERSION, build_request, from_claude_code_name};
use crate::providers::common::{
    SseHandler, compat_bool, compat_str, endpoint, has_header, header_map, spawn_sse_request,
};
use crate::registry::{ApiProvider, StreamOptions, http_client};
use crate::sse::SseEvent;
use crate::stream::{AssistantMessageEventStream, MessageBuilder, error_stream};
use crate::types::{Context, Model, ModelCost, StopReason};

pub struct AnthropicMessages;

impl ApiProvider for AnthropicMessages {
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        let api_key = options.api_key.clone().filter(|key| !key.is_empty());
        let header_auth =
            ["authorization", "x-api-key", "cf-aig-authorization"].iter().any(|name| has_header(model, &options, name));
        if api_key.is_none() && !header_auth {
            return error_stream(model, StopReason::Error, format!("No API key for provider: {}", model.provider));
        }
        let oauth = api_key.as_deref().is_some_and(|key| key.contains("sk-ant-oat"));
        let AnthropicRequest { body, betas } = build_request(model, context, &options, oauth);
        let mut defaults = vec![("anthropic-version", "2023-06-01".to_string())];
        match api_key {
            Some(key) if oauth => {
                defaults.push(("authorization", format!("Bearer {key}")));
                defaults.push(("user-agent", format!("claude-cli/{CLAUDE_CODE_VERSION}")));
                defaults.push(("x-app", "cli".into()));
            }
            Some(key) => defaults.push(("x-api-key", key)),
            None => {}
        }
        let openrouter = model.provider == "openrouter" || model.base_url.contains("openrouter.ai");
        if let Some(session) = &options.session_id
            && !oauth
            && compat_bool(model, "sendSessionAffinityHeaders").unwrap_or(openrouter)
        {
            let format = compat_str(model, "sessionAffinityFormat").or(openrouter.then_some("openrouter"));
            let name = if format == Some("openrouter") { "x-session-id" } else { "x-session-affinity" };
            defaults.push((name, session.clone()));
        }
        if !betas.is_empty() {
            defaults.push(("anthropic-beta", betas.join(",")));
        }
        let request = http_client()
            .post(endpoint(&model.base_url, "/v1/messages"))
            .headers(header_map(defaults, model, &options))
            .json(&body);
        let handler = AnthropicStream {
            cost: model.cost,
            oauth_tools: if oauth { context.tools.iter().map(|tool| tool.name.clone()).collect() } else { Vec::new() },
            blocks: HashMap::new(),
            stop: None,
            started: false,
            stopped: false,
        };
        spawn_sse_request(model, request, options.cancel, handler)
    }
}

enum Block {
    Text,
    Thinking { signature: String },
    Redacted,
    Tool(usize),
}

struct AnthropicStream {
    cost: ModelCost,
    /// Declared tool names, for mapping Claude Code casing back under OAuth.
    oauth_tools: Vec<String>,
    blocks: HashMap<u64, Block>,
    stop: Option<Result<StopReason, String>>,
    started: bool,
    stopped: bool,
}

impl SseHandler for AnthropicStream {
    fn event(&mut self, builder: &mut MessageBuilder, event: SseEvent) -> Result<bool, String> {
        if event.data.trim().is_empty() {
            return Ok(true);
        }
        let data: Value = serde_json::from_str(&event.data).map_err(|error| {
            format!("Could not parse Anthropic SSE event {}: {error}", event.event.as_deref().unwrap_or(""))
        })?;
        let kind = data["type"].as_str().or(event.event.as_deref()).unwrap_or_default();
        let index = data["index"].as_u64().unwrap_or_default();
        match kind {
            "message_start" => {
                self.started = true;
                let message = &data["message"];
                if let Some(id) = message["id"].as_str() {
                    builder.message_mut().response_id = Some(id.to_string());
                }
                self.apply_usage(builder, &message["usage"]);
            }
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
            "content_block_stop" => match self.blocks.get_mut(&index) {
                Some(Block::Thinking { signature }) => {
                    if !signature.is_empty() {
                        builder.thinking_signature(std::mem::take(signature), false);
                    }
                    builder.close_open_block();
                }
                Some(Block::Text | Block::Redacted) => builder.close_open_block(),
                Some(Block::Tool(_)) | None => {}
            },
            "message_delta" => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(map_stop_reason(reason, data.pointer("/delta/stop_details/explanation")));
                }
                self.apply_usage(builder, &data["usage"]);
            }
            "message_stop" => self.stopped = true,
            "error" => {
                let message = data.pointer("/error/message").and_then(Value::as_str);
                return Err(message.map_or_else(|| event.data.clone(), str::to_string));
            }
            _ => {}
        }
        Ok(true)
    }

    fn end(&mut self, _builder: &mut MessageBuilder) -> Result<StopReason, String> {
        if self.started && !self.stopped {
            return Err("Anthropic stream ended before message_stop".into());
        }
        self.stop.take().unwrap_or_else(|| Err("Anthropic stream ended without a stop reason".into()))
    }
}

impl AnthropicStream {
    fn start_block(&mut self, builder: &mut MessageBuilder, index: u64, block: &Value) -> Result<(), String> {
        let text = |key: &str| block[key].as_str().unwrap_or_default();
        let entry = match block["type"].as_str() {
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
            Some("fallback") if !builder.message().content.is_empty() => {
                return Err("Anthropic performed an unsupported mid-output model fallback".into());
            }
            _ => return Ok(()),
        };
        self.blocks.insert(index, entry);
        Ok(())
    }

    fn apply_usage(&self, builder: &mut MessageBuilder, raw: &Value) {
        if !raw.is_object() {
            return;
        }
        let usage = &mut builder.message_mut().usage;
        let set = |field: &mut u64, key: &str| {
            if let Some(value) = raw.get(key).and_then(Value::as_u64) {
                *field = value;
            }
        };
        set(&mut usage.input, "input_tokens");
        set(&mut usage.output, "output_tokens");
        set(&mut usage.cache_read, "cache_read_input_tokens");
        set(&mut usage.cache_write, "cache_creation_input_tokens");
        if let Some(thinking) = raw.pointer("/output_tokens_details/thinking_tokens").and_then(Value::as_u64) {
            usage.reasoning = Some(thinking);
        }
        usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
        usage.apply_cost(&self.cost);
    }
}

fn map_stop_reason(reason: &str, explanation: Option<&Value>) -> Result<StopReason, String> {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => Ok(StopReason::Stop),
        "max_tokens" => Ok(StopReason::Length),
        "tool_use" => Ok(StopReason::ToolUse),
        "refusal" => Err(explanation
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .unwrap_or("The model refused to complete the request")
            .to_string()),
        "sensitive" => Err("Provider stopped with: sensitive".into()),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}
