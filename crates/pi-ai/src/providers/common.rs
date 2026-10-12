//! Shared plumbing for the HTTP streaming providers: request driving, error
//! formatting, header merging, and the message normalization from upstream `transformMessages`.

use std::collections::HashMap;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::registry::StreamOptions;
use crate::sse::{SseEvent, sse_events};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, event_channel};
use crate::types::{
    AssistantMessage, ContentBlock, Message, Model, StopReason, ThinkingLevel, ToolResultMessage, UserContent, now_ms,
};

/// Largest error body read from a failed response.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_ERROR_MESSAGE_CHARS: usize = 500;

/// Consumes the SSE events of one response.
pub(crate) trait SseHandler: Send + 'static {
    /// Handle one event. Return `Ok(false)` to stop reading the body.
    fn event(&mut self, builder: &mut MessageBuilder, event: SseEvent) -> Result<bool, String>;

    /// Called after the last event; returns the stop reason for `Done`.
    fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, String>;
}

/// Send `request` on a background task and stream its SSE body through `handler`.
pub(crate) fn spawn_sse_request<H: SseHandler>(
    model: &Model,
    request: reqwest::RequestBuilder,
    cancel: CancellationToken,
    mut handler: H,
) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let mut builder = MessageBuilder::new(model, sender);
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(String::new()),
            result = drive(&mut builder, request, &mut handler) => result,
        };
        if cancel.is_cancelled() {
            return builder.fail(StopReason::Aborted, "Request was aborted");
        }
        match result {
            Ok(reason) => builder.finish(reason),
            Err(error) => builder.fail(StopReason::Error, error),
        }
    });
    stream
}

async fn drive<H: SseHandler>(
    builder: &mut MessageBuilder,
    request: reqwest::RequestBuilder,
    handler: &mut H,
) -> Result<StopReason, String> {
    let response = request.send().await.map_err(|error| format!("Request failed: {}", error_chain(&error)))?;
    let status = response.status();
    if !status.is_success() {
        let body = read_bounded(response).await;
        return Err(http_error_message(status.as_u16(), &body));
    }
    builder.start();
    let mut events = std::pin::pin!(sse_events(Box::pin(response.bytes_stream())));
    while let Some(event) = events.next().await {
        if !handler.event(builder, event?)? {
            break;
        }
    }
    handler.end(builder)
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

async fn read_bounded(response: reqwest::Response) -> String {
    let mut body = Vec::new();
    let mut chunks = std::pin::pin!(response.bytes_stream());
    while let Some(Ok(chunk)) = chunks.next().await {
        let room = MAX_ERROR_BODY_BYTES - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= MAX_ERROR_BODY_BYTES {
            break;
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// `Provider HTTP {status}: {message}`, preferring the JSON error message in `body`.
pub(crate) fn http_error_message(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!("Provider HTTP {status}");
    }
    let json_message = serde_json::from_str::<Value>(trimmed).ok().and_then(|value| {
        [value.pointer("/error/message"), value.get("message"), value.get("error"), value.get("detail")]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .find(|message| !message.is_empty())
            .map(str::to_string)
    });
    let message = json_message.unwrap_or_else(|| trimmed.to_string());
    format!("Provider HTTP {status}: {}", truncate_chars(&message, MAX_ERROR_MESSAGE_CHARS))
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_string(),
    }
}

/// Merge `defaults`, then `model.headers`, then `options.headers`; later names replace earlier ones.
pub(crate) fn header_map(defaults: Vec<(&str, String)>, model: &Model, options: &StreamOptions) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let custom = model.headers.iter().chain(options.headers.iter()).map(|(name, value)| (name.as_str(), value.clone()));
    for (name, value) in defaults.into_iter().chain(custom) {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
            headers.insert(name, value);
        }
    }
    headers
}

/// True when the caller supplied header `name` with a non-empty value.
pub(crate) fn has_header(model: &Model, options: &StreamOptions, name: &str) -> bool {
    model
        .headers
        .iter()
        .chain(options.headers.iter())
        .any(|(key, value)| key.eq_ignore_ascii_case(name) && !value.trim().is_empty())
}

pub(crate) fn endpoint(base_url: &str, path: &str) -> String {
    format!("{}{path}", base_url.trim_end_matches('/'))
}

pub(crate) fn compat_value<'a>(model: &'a Model, key: &str) -> Option<&'a Value> {
    model.compat.as_ref()?.get(key)
}

pub(crate) fn compat_bool(model: &Model, key: &str) -> Option<bool> {
    compat_value(model, key)?.as_bool()
}

pub(crate) fn compat_str<'a>(model: &'a Model, key: &str) -> Option<&'a str> {
    compat_value(model, key)?.as_str()
}

/// `PI_CACHE_RETENTION=long` requests long prompt cache retention.
pub(crate) fn long_cache_retention() -> bool {
    std::env::var("PI_CACHE_RETENTION").is_ok_and(|value| value == "long")
}

/// The requested reasoning level clamped to what the model supports. Without
/// per-model level maps, `xhigh` falls back to `high` as upstream does.
pub(crate) fn reasoning_level(model: &Model, options: &StreamOptions) -> Option<ThinkingLevel> {
    if !model.reasoning {
        return None;
    }
    match options.reasoning? {
        ThinkingLevel::Off => None,
        ThinkingLevel::Xhigh => Some(ThinkingLevel::High),
        level => Some(level),
    }
}

/// Tokens always left for the answer when thinking shares the output ceiling.
pub(crate) const MIN_ANSWER_TOKENS: u64 = 1024;

/// Default thinking token budget for a level.
pub(crate) fn thinking_budget(level: ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        ThinkingLevel::High | ThinkingLevel::Xhigh => 16384,
    }
}

/// Replace characters outside `[A-Za-z0-9_-]` with `_`.
pub(crate) fn sanitize_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

pub(crate) fn take_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

pub(crate) fn data_url(mime_type: &str, data: &str) -> String {
    format!("data:{mime_type};base64,{data}")
}

/// Text blocks of a tool result joined with newlines.
pub(crate) fn tool_result_text(content: &[ContentBlock]) -> String {
    content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
}

pub(crate) fn has_images(content: &[ContentBlock]) -> bool {
    content.iter().any(|block| matches!(block, ContentBlock::Image { .. }))
}

pub(crate) fn as_u64(value: Option<&Value>) -> u64 {
    value.and_then(Value::as_u64).unwrap_or_default()
}

/// Deterministic short hash, ported from upstream `shortHash`.
pub(crate) fn short_hash(input: &str) -> String {
    let mut h1: u32 = 0xdead_beef;
    let mut h2: u32 = 0x41c6_ce57;
    for unit in input.encode_utf16() {
        let unit = u32::from(unit);
        h1 = (h1 ^ unit).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ unit).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", radix36(h2), radix36(h1))
}

fn radix36(mut value: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut digits = Vec::new();
    loop {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).unwrap_or_default()
}

const USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";

fn replace_images(content: &[ContentBlock], placeholder: &str) -> Vec<ContentBlock> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        if matches!(block, ContentBlock::Image { .. }) {
            if !previous_was_placeholder {
                result.push(ContentBlock::text(placeholder));
            }
            previous_was_placeholder = true;
            continue;
        }
        previous_was_placeholder = block.as_text() == Some(placeholder);
        result.push(block.clone());
    }
    result
}

/// Prepare history for `model`: downgrade unsupported images, convert foreign
/// thinking to text, normalize foreign tool call ids, drop failed turns, and
/// add error results for tool calls that never got one.
pub(crate) fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_id: &dyn Fn(&str, &AssistantMessage) -> String,
) -> Vec<Message> {
    let images = model.supports_images();
    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut transformed = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            Message::User(user) => {
                let mut user = user.clone();
                if let (false, UserContent::Blocks(blocks)) = (images, &user.content) {
                    user.content = UserContent::Blocks(replace_images(blocks, USER_IMAGE_PLACEHOLDER));
                }
                transformed.push(Message::User(user));
            }
            Message::ToolResult(result) => {
                let mut result = result.clone();
                if !images {
                    result.content = replace_images(&result.content, TOOL_IMAGE_PLACEHOLDER);
                }
                if let Some(id) = id_map.get(&result.tool_call_id) {
                    result.tool_call_id = id.clone();
                }
                transformed.push(Message::ToolResult(result));
            }
            Message::Assistant(assistant) => {
                let same_model =
                    assistant.provider == model.provider && assistant.api == model.api && assistant.model == model.id;
                let mut converted = assistant.clone();
                converted.content = assistant
                    .content
                    .iter()
                    .filter_map(|block| transform_block(block, assistant, same_model, normalize_id, &mut id_map))
                    .collect();
                transformed.push(Message::Assistant(converted));
            }
        }
    }
    close_orphaned_tool_calls(transformed)
}

fn transform_block(
    block: &ContentBlock,
    source: &AssistantMessage,
    same_model: bool,
    normalize_id: &dyn Fn(&str, &AssistantMessage) -> String,
    id_map: &mut HashMap<String, String>,
) -> Option<ContentBlock> {
    match block {
        ContentBlock::Thinking { thinking, thinking_signature, redacted } => {
            let signed = thinking_signature.as_deref().is_some_and(|signature| !signature.is_empty());
            if *redacted {
                return same_model.then(|| block.clone());
            }
            if same_model && signed {
                return Some(block.clone());
            }
            if thinking.trim().is_empty() {
                return None;
            }
            Some(if same_model { block.clone() } else { ContentBlock::text(thinking.clone()) })
        }
        ContentBlock::Text { text, .. } if !same_model => Some(ContentBlock::text(text.clone())),
        ContentBlock::ToolCall(call) if !same_model => {
            let mut call = call.clone();
            call.thought_signature = None;
            let id = normalize_id(&call.id, source);
            if id != call.id {
                id_map.insert(call.id.clone(), id.clone());
                call.id = id;
            }
            Some(ContentBlock::ToolCall(call))
        }
        _ => Some(block.clone()),
    }
}

fn close_orphaned_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    let mut result = Vec::with_capacity(messages.len());
    let mut pending: Vec<(String, String)> = Vec::new();
    let flush = |pending: &mut Vec<(String, String)>, result: &mut Vec<Message>| {
        for (id, name) in pending.drain(..) {
            result.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: id,
                tool_name: name,
                content: vec![ContentBlock::text("No result provided")],
                details: None,
                is_error: true,
                timestamp: now_ms(),
                duration_ms: None,
            }));
        }
    };
    for message in messages {
        match &message {
            Message::Assistant(assistant) => {
                flush(&mut pending, &mut result);
                if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                    continue;
                }
                pending = assistant.tool_calls().map(|call| (call.id.clone(), call.name.clone())).collect();
            }
            Message::ToolResult(tool_result) => pending.retain(|(id, _)| *id != tool_result.tool_call_id),
            Message::User(_) => flush(&mut pending, &mut result),
        }
        result.push(message);
    }
    flush(&mut pending, &mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, Usage};

    #[test]
    fn http_errors_prefer_json_messages() {
        assert_eq!(
            http_error_message(429, r#"{"error":{"message":"Rate limited","type":"x"}}"#),
            "Provider HTTP 429: Rate limited"
        );
        assert_eq!(http_error_message(502, "bad gateway"), "Provider HTTP 502: bad gateway");
        assert_eq!(http_error_message(500, ""), "Provider HTTP 500");
    }

    #[test]
    fn short_hash_matches_upstream() {
        assert_eq!(short_hash(""), "k4n83c7h0j2b");
        assert_eq!(short_hash("call_abc|fc_123é"), "ii5wx01p3is0h");
    }

    #[test]
    fn transform_adds_missing_tool_results_and_drops_failed_turns() {
        let assistant = |stop_reason, id: &str| {
            Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: id.into(),
                    name: "read".into(),
                    arguments: Default::default(),
                    thought_signature: None,
                })],
                api: "other".into(),
                provider: "other".into(),
                model: "x".into(),
                response_id: None,
                usage: Usage::default(),
                stop_reason,
                error_message: None,
                timestamp: 0,
                duration_ms: None,
            })
        };
        let model = crate::providers::faux::FauxProvider::model();
        let messages =
            vec![assistant(StopReason::Aborted, "a|1"), assistant(StopReason::ToolUse, "b|2"), Message::user("next")];
        let normalize = |id: &str, _: &AssistantMessage| id.replace('|', "_");
        let result = transform_messages(&messages, &model, &normalize);
        assert_eq!(result.len(), 3);
        let Message::ToolResult(synthetic) = &result[1] else { panic!("expected tool result") };
        assert_eq!(synthetic.tool_call_id, "b_2");
        assert!(synthetic.is_error);
    }
}
