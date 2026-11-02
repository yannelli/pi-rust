//! ConverseStream events: content blocks, stop reasons, usage, and stream exceptions.

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::providers::aws::event_stream::Message as Frame;
use crate::providers::common::as_u64;
use crate::stream::MessageBuilder;
use crate::types::{ContentBlock, ModelCost, StopReason, Usage, now_ms};

const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const DATA_RETENTION_DOCS_URL: &str = "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";
/// Longer diagnostic values are dropped: a truncated request id is not a request id.
const MAX_DIAGNOSTIC_VALUE_CHARS: usize = 200;

/// Human-readable prefixes that retry and overflow detection match on.
pub(super) fn error_prefix(code: &str) -> Option<&'static str> {
    match code {
        "InternalServerException" | "internalServerException" => Some("Internal server error"),
        "ModelStreamErrorException" | "modelStreamErrorException" => Some("Model stream error"),
        "ValidationException" | "validationException" => Some("Validation error"),
        "ThrottlingException" | "throttlingException" => Some("Throttling error"),
        "ServiceUnavailableException" | "serviceUnavailableException" => Some("Service unavailable"),
        _ => None,
    }
}

/// Append the AWS data retention docs link when the error is about retention modes.
pub(super) fn with_retention_hint(message: String) -> String {
    if message.to_lowercase().contains("data retention mode") {
        format!("{message} See {DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        message
    }
}

/// A failure with the metadata upstream records in a `bedrock_response_failure` diagnostic.
#[derive(Debug, Default)]
pub(super) struct Failure {
    pub message: String,
    pub status: Option<u16>,
    pub error_code: Option<String>,
    /// The message of a rejected request as the service sent it, without the error prefix.
    pub provider_message: Option<String>,
}

impl Failure {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Self::default() }
    }

    pub fn diagnostic(&self, request_id: Option<&str>) -> Option<Value> {
        let mut details = serde_json::Map::new();
        if let Some(status) = self.status {
            details.insert("status".into(), json!(status));
        }
        if let Some(code) = diagnostic_value(self.error_code.as_deref()).filter(|code| code.ends_with("Exception")) {
            details.insert("errorCode".into(), json!(code));
        }
        if let Some(request_id) = diagnostic_value(request_id) {
            details.insert("requestId".into(), json!(request_id));
        }
        (!details.is_empty())
            .then(|| json!({"type": "bedrock_response_failure", "timestamp": now_ms(), "details": details}))
    }
}

pub(super) fn diagnostic_value(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty() && value.chars().count() <= MAX_DIAGNOSTIC_VALUE_CHARS)
}

fn payload_message(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload).into_owned();
    let parsed = serde_json::from_slice::<Value>(payload).ok();
    let message = parsed.as_ref().and_then(|value| value.get("message").or(value.get("Message"))?.as_str());
    message.map_or(text, str::to_string)
}

enum Block {
    Text,
    Thinking { index: usize, signature: String, redacted: bool, chunks: Vec<u8> },
    Tool,
}

/// Normalizes ConverseStream events into the message builder.
pub(super) struct BedrockEvents {
    pub cost: ModelCost,
    blocks: HashMap<u64, Block>,
    tools: HashMap<u64, usize>,
    started: bool,
    stop: Option<(StopReason, Option<String>)>,
}

impl BedrockEvents {
    pub fn new(cost: ModelCost) -> Self {
        Self { cost, blocks: HashMap::new(), tools: HashMap::new(), started: false, stop: None }
    }

    /// The SDK-shaped item for `on_provider_stream_event`, such as `{"contentBlockDelta": {...}}`.
    pub fn item(frame: &Frame) -> Option<Value> {
        let kind = match frame.header_str(":message-type") {
            Some("exception") => frame.header_str(":exception-type")?,
            _ => frame.header_str(":event-type")?,
        };
        let mut payload: Value = serde_json::from_slice(&frame.payload).unwrap_or_else(|_| json!({}));
        if let Some(object) = payload.as_object_mut() {
            object.remove("p");
        }
        Some(json!({ kind: payload }))
    }

    pub fn ensure_started(&mut self, builder: &mut MessageBuilder) {
        if !self.started {
            self.started = true;
            builder.start();
        }
    }

    /// Apply one frame. Exceptions and error frames end the stream with a failure.
    pub fn frame(&mut self, builder: &mut MessageBuilder, frame: &Frame, item: &Value) -> Result<(), Failure> {
        match frame.header_str(":message-type") {
            Some("exception") => {
                let kind = frame.header_str(":exception-type").unwrap_or("UnknownException");
                let message = payload_message(&frame.payload);
                return Err(match error_prefix(kind) {
                    Some(prefix) => Failure::new(with_retention_hint(format!("{prefix}: {message}"))),
                    None => {
                        Failure { error_code: Some(kind.to_string()), ..Failure::new(with_retention_hint(message)) }
                    }
                });
            }
            Some("error") => {
                let message = frame.header_str(":error-message").unwrap_or("UnknownError");
                let code = frame.header_str(":error-code").map(str::to_string);
                return Err(Failure { error_code: code, ..Failure::new(with_retention_hint(message.to_string())) });
            }
            _ => {}
        }
        let Some((kind, event)) = item.as_object().and_then(|object| object.iter().next()) else { return Ok(()) };
        let index = as_u64(event.get("contentBlockIndex"));
        match kind.as_str() {
            "messageStart" => {
                if event["role"].as_str().is_some_and(|role| role != "assistant") {
                    return Err(Failure::new("Unexpected assistant message start but got user message start instead"));
                }
                self.ensure_started(builder);
            }
            "contentBlockStart" => {
                self.ensure_started(builder);
                if let Some(tool) = event.pointer("/start/toolUse") {
                    let id = tool["toolUseId"].as_str().unwrap_or_default();
                    let handle = builder.tool_call_start(id, tool["name"].as_str().unwrap_or_default());
                    self.tools.insert(index, handle);
                    self.blocks.insert(index, Block::Tool);
                }
            }
            "contentBlockDelta" => {
                self.ensure_started(builder);
                self.delta(builder, index, &event["delta"]);
            }
            "contentBlockStop" => self.stop_block(builder, index),
            "messageStop" => {
                let raw = event["stopReason"].as_str().map(str::to_string);
                builder.message_mut().raw_stop_reason = raw.clone();
                self.stop = Some(match raw.as_deref() {
                    Some("end_turn" | "stop_sequence") => (StopReason::Stop, None),
                    Some("max_tokens" | "model_context_window_exceeded") => (StopReason::Length, None),
                    Some("tool_use") => (StopReason::ToolUse, None),
                    Some(other) => (StopReason::Error, Some(format!("Provider stopped with: {other}"))),
                    None => (StopReason::Error, None),
                });
            }
            "metadata" => {
                if let Some(raw) = event.get("usage").filter(|usage| usage.is_object()) {
                    self.usage(builder, raw);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn delta(&mut self, builder: &mut MessageBuilder, index: u64, delta: &Value) {
        if let Some(text) = delta.get("text").and_then(Value::as_str) {
            if let Block::Text = self.blocks.entry(index).or_insert(Block::Text) {
                builder.text_delta(text);
            }
        } else if let Some(input) = delta.pointer("/toolUse/input") {
            if let Some(&handle) = self.tools.get(&index) {
                builder.tool_call_delta(handle, input.as_str().unwrap_or_default());
            }
        } else if let Some(reasoning) = delta.get("reasoningContent") {
            let block = self.blocks.entry(index).or_insert_with(|| {
                builder.thinking_delta("");
                let index = builder.message().content.len() - 1;
                Block::Thinking { index, signature: String::new(), redacted: false, chunks: Vec::new() }
            });
            let Block::Thinking { index: content_index, signature, redacted, chunks } = block else { return };
            if let Some(text) = reasoning.get("text").and_then(Value::as_str) {
                builder.thinking_delta(text);
            }
            if let Some(part) = reasoning.get("signature").and_then(Value::as_str).filter(|_| !*redacted) {
                signature.push_str(part);
                set_signature(builder, *content_index, signature.clone(), false);
            }
            let bytes =
                reasoning.get("redactedContent").and_then(Value::as_str).and_then(|data| STANDARD.decode(data).ok());
            if let Some(bytes) = bytes.filter(|bytes| !bytes.is_empty()) {
                if !*redacted {
                    *redacted = true;
                    signature.clear();
                    set_signature(builder, *content_index, String::new(), true);
                    builder.thinking_delta(REDACTED_THINKING_PLACEHOLDER);
                }
                chunks.extend(bytes);
            }
        }
    }

    fn stop_block(&mut self, builder: &mut MessageBuilder, index: u64) {
        match self.blocks.remove(&index) {
            Some(Block::Text) => builder.close_open_block(),
            Some(Block::Thinking { index, redacted, chunks, .. }) => {
                if redacted {
                    set_signature(builder, index, STANDARD.encode(chunks), true);
                }
                builder.close_open_block();
            }
            Some(Block::Tool) | None => {}
        }
    }

    fn usage(&self, builder: &mut MessageBuilder, raw: &Value) {
        let cache_write_1h = raw.get("cacheDetails").and_then(Value::as_array).map(|details| {
            details.iter().filter(|detail| detail["ttl"] == "1h").map(|detail| as_u64(detail.get("inputTokens"))).sum()
        });
        let mut usage = Usage {
            input: as_u64(raw.get("inputTokens")),
            output: as_u64(raw.get("outputTokens")),
            cache_read: as_u64(raw.get("cacheReadInputTokens")),
            cache_write: as_u64(raw.get("cacheWriteInputTokens")),
            cache_write_1h,
            ..Usage::default()
        };
        usage.total_tokens = raw
            .get("totalTokens")
            .and_then(Value::as_u64)
            .filter(|total| *total > 0)
            .unwrap_or(usage.input + usage.output);
        usage.apply_cost(&self.cost);
        builder.message_mut().usage = usage;
    }

    /// Flush buffered redacted reasoning for blocks the stream never stopped.
    pub fn finalize(&mut self, builder: &mut MessageBuilder) {
        let pending: Vec<u64> = self.blocks.keys().copied().collect();
        for index in pending {
            if let Some(Block::Thinking { index, redacted: true, chunks, .. }) = self.blocks.remove(&index) {
                set_signature(builder, index, STANDARD.encode(chunks), true);
            }
        }
    }

    /// The final stop reason, or the failure upstream reports for missing or error stops.
    pub fn result(&mut self) -> Result<StopReason, Failure> {
        match self.stop.take() {
            None => Err(Failure::new("Bedrock stream ended without a stop reason")),
            Some((StopReason::Error, message)) => {
                Err(Failure::new(message.unwrap_or_else(|| "An unknown error occurred".into())))
            }
            Some((reason, _)) => Ok(reason),
        }
    }
}

fn set_signature(builder: &mut MessageBuilder, index: usize, value: String, is_redacted: bool) {
    if let Some(ContentBlock::Thinking { thinking_signature, redacted, .. }) =
        builder.message_mut().content.get_mut(index)
    {
        *thinking_signature = Some(value);
        *redacted |= is_redacted;
    }
}
