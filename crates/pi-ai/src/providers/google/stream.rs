//! Normalization of streamed `GenerateContentResponse` chunks into assistant events.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use crate::providers::emitter::Emitter;
use crate::stream::AssistantMessageEvent;
use crate::types::{ContentBlock, JsonObject, Model, StopReason, ToolCall, Usage, now_ms};

/// Suffix for generated tool call ids, unique within the process.
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Gemini finish reason to stop reason; unknown values are an error.
pub(crate) fn map_stop_reason(reason: &str) -> Result<StopReason, String> {
    match reason {
        "STOP" => Ok(StopReason::Stop),
        "MAX_TOKENS" => Ok(StopReason::Length),
        "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "SAFETY"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "IMAGE_OTHER"
        | "RECITATION"
        | "FINISH_REASON_UNSPECIFIED"
        | "OTHER"
        | "LANGUAGE"
        | "MALFORMED_FUNCTION_CALL"
        | "UNEXPECTED_TOOL_CALL"
        | "TOO_MANY_TOOL_CALLS"
        | "NO_IMAGE" => Ok(StopReason::Error),
        other => Err(format!("Unhandled stop reason: {other}")),
    }
}

pub(crate) struct GoogleStream {
    /// Adapter name for the missing-finish-reason error.
    label: &'static str,
    /// Content index of the open text or thinking block.
    current: Option<usize>,
    stop: Option<StopReason>,
}

impl GoogleStream {
    pub fn new(label: &'static str) -> Self {
        Self { label, current: None, stop: None }
    }

    fn close_current(&mut self, out: &Emitter) {
        if let Some(index) = self.current.take() {
            out.close(index);
        }
    }

    pub fn chunk(&mut self, out: &mut Emitter, model: &Model, chunk: &Value) -> Result<(), String> {
        if let Some(error) = chunk.get("error").filter(|error| error.is_object()) {
            let code = error["code"].as_u64().unwrap_or_default();
            if (400..600).contains(&code) {
                return Err(format!("got status: {}. {chunk}", error["status"].as_str().unwrap_or_default()));
            }
        }
        if out.message.response_id.is_none()
            && let Some(id) = chunk["responseId"].as_str().filter(|id| !id.is_empty())
        {
            out.message.response_id = Some(id.to_string());
        }
        let candidate = &chunk["candidates"][0];
        for part in candidate.pointer("/content/parts").and_then(Value::as_array).into_iter().flatten() {
            if let Some(text) = part["text"].as_str() {
                self.text_part(out, part, text);
            }
            if let Some(call) = part.get("functionCall").filter(|call| call.is_object()) {
                self.function_call(out, part, call);
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            out.message.raw_stop_reason = Some(reason.to_string());
            let mut stop = map_stop_reason(reason)?;
            if stop == StopReason::Stop && out.message.tool_calls().next().is_some() {
                stop = StopReason::ToolUse;
            }
            self.stop = Some(stop);
        }
        if let Some(meta) = chunk.get("usageMetadata").filter(|meta| meta.is_object()) {
            let count = |key: &str| meta[key].as_u64().unwrap_or_default();
            let mut usage = Usage {
                input: count("promptTokenCount").saturating_sub(count("cachedContentTokenCount")),
                output: count("candidatesTokenCount") + count("thoughtsTokenCount"),
                cache_read: count("cachedContentTokenCount"),
                reasoning: Some(count("thoughtsTokenCount")),
                total_tokens: count("totalTokenCount"),
                ..Usage::default()
            };
            usage.apply_cost(&model.cost);
            out.message.usage = usage;
        }
        Ok(())
    }

    fn text_part(&mut self, out: &mut Emitter, part: &Value, text: &str) {
        let thinking = part["thought"] == true;
        let open_kind = self
            .current
            .and_then(|index| out.message.content.get(index))
            .map(|block| matches!(block, ContentBlock::Thinking { .. }));
        if open_kind != Some(thinking) {
            self.close_current(out);
            let block = if thinking {
                ContentBlock::Thinking { thinking: String::new(), thinking_signature: None, redacted: false }
            } else {
                ContentBlock::text("")
            };
            self.current = Some(out.open(block));
        }
        let Some(content_index) = self.current else { return };
        let signature = part["thoughtSignature"].as_str().filter(|signature| !signature.is_empty());
        match &mut out.message.content[content_index] {
            ContentBlock::Thinking { thinking, thinking_signature, .. } => {
                thinking.push_str(text);
                if let Some(signature) = signature {
                    *thinking_signature = Some(signature.to_string());
                }
            }
            ContentBlock::Text { text: block_text, text_signature } => {
                block_text.push_str(text);
                if let Some(signature) = signature {
                    *text_signature = Some(signature.to_string());
                }
            }
            _ => {}
        }
        let delta = text.to_string();
        if thinking {
            out.emit(|partial| AssistantMessageEvent::ThinkingDelta { content_index, delta, partial });
        } else {
            out.emit(|partial| AssistantMessageEvent::TextDelta { content_index, delta, partial });
        }
    }

    fn function_call(&mut self, out: &mut Emitter, part: &Value, call: &Value) {
        self.close_current(out);
        let name = call["name"].as_str().unwrap_or_default().to_string();
        let provided = call["id"].as_str().filter(|id| !id.is_empty());
        let duplicate = provided.is_some_and(|id| out.message.tool_calls().any(|existing| existing.id == id));
        let id = match provided {
            Some(id) if !duplicate => id.to_string(),
            _ => format!("{name}_{}_{}", now_ms(), TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1),
        };
        let arguments: JsonObject = call["args"].as_object().cloned().unwrap_or_default();
        let delta = Value::Object(arguments.clone()).to_string();
        let thought_signature =
            part["thoughtSignature"].as_str().filter(|signature| !signature.is_empty()).map(str::to_string);
        let tool_call = ToolCall { id, name, arguments, thought_signature, ..ToolCall::default() };
        let content_index = out.open(ContentBlock::ToolCall(tool_call.clone()));
        out.emit(|partial| AssistantMessageEvent::ToolcallDelta { content_index, delta, partial });
        out.emit(|partial| AssistantMessageEvent::ToolcallEnd { content_index, tool_call, partial });
    }

    /// Close the open block and return the final stop reason.
    pub fn end(&mut self, out: &mut Emitter) -> Result<StopReason, String> {
        self.close_current(out);
        match self.stop {
            None => Err(format!("{} stream ended without a finish reason", self.label)),
            Some(StopReason::Error | StopReason::Aborted) => Err(match &out.message.raw_stop_reason {
                Some(raw) => format!("Provider stopped with: {raw}"),
                None => "An unknown error occurred".into(),
            }),
            Some(reason) => Ok(reason),
        }
    }
}
