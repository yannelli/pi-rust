//! Shared plumbing for the HTTP streaming providers: the request driver (payload and response
//! hooks, retries, timeouts, proxies), header merging, compat flags, and small helpers.

use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub(crate) use super::transform::transform_messages;
use crate::registry::StreamOptions;
#[allow(unused_imports)]
pub(crate) use crate::request::{HttpRequest, http_error_message, truncate_chars};
use crate::request::{SendOptions, apply_payload_hook, build_headers, error_chain, provider_response, send_request};
use crate::sse::{SseEvent, sse_events};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, event_channel};
use crate::types::{ContentBlock, Model, StopReason, ThinkingLevel};

/// Consumes the SSE events of one response.
pub(crate) trait SseHandler: Send + 'static {
    /// Called once the response arrived, before `Start` is emitted.
    fn start(&mut self, _builder: &mut MessageBuilder) {}

    /// Handle one event. Return `Ok(false)` to stop reading the body.
    fn event(&mut self, builder: &mut MessageBuilder, event: SseEvent) -> Result<bool, String>;

    /// Called after the last event; returns the stop reason for `Done`.
    fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, String>;

    /// Rewrite a failure message before the `Error` event.
    fn error_message(&self, message: String) -> String {
        message
    }
}

/// Send a prepared `request` on a background task and stream its SSE body through `handler`.
/// Prefer [`spawn_provider_stream`], which also honors the request options.
#[allow(dead_code)]
pub(crate) fn spawn_sse_request<H: SseHandler>(
    model: &Model,
    request: reqwest::RequestBuilder,
    cancel: CancellationToken,
    mut handler: H,
) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let mut builder = MessageBuilder::new(model, sender);
    tokio::spawn(async move {
        let drive = async {
            let response = request.send().await.map_err(|error| format!("Request failed: {}", error_chain(&error)))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(http_error_message(status.as_u16(), &body));
            }
            read_events(&mut builder, response, &mut handler, None, None).await
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(String::new()),
            result = drive => result,
        };
        settle(builder, &handler, result, cancel.is_cancelled());
    });
    stream
}

/// POST `request` with the hooks, retries, timeout, and proxy settings of `options`, and
/// stream the SSE response through `handler`. `on_payload` runs before this returns.
pub(crate) fn spawn_provider_stream<H: SseHandler>(
    model: &Model,
    options: &StreamOptions,
    mut request: HttpRequest,
    mut handler: H,
) -> AssistantMessageEventStream {
    request.body = apply_payload_hook(request.body, options.on_payload.as_ref(), model);
    let (sender, stream) = event_channel();
    let mut builder = MessageBuilder::new(model, sender);
    let (model, options) = (model.clone(), options.clone());
    tokio::spawn(async move {
        let send = SendOptions {
            env: &options.env,
            cancel: &options.cancel,
            timeout_ms: options.timeout_ms,
            max_retries: options.max_retries.unwrap_or(0),
            max_retry_delay_ms: options.max_retry_delay_ms,
            no_retry_statuses: &[],
            label: None,
        };
        let drive = async {
            let response = send_request(&request, &send).await.map_err(|error| error.message)?;
            if let Some(hook) = &options.on_response {
                hook(&provider_response(&response), &model);
            }
            let hook = options.on_provider_stream_event.as_ref().map(|hook| (hook, &model));
            read_events(&mut builder, response, &mut handler, hook, Some(&options.cancel)).await
        };
        let result = tokio::select! {
            biased;
            _ = options.cancel.cancelled() => Err(String::new()),
            result = drive => result,
        };
        settle(builder, &handler, result, options.cancel.is_cancelled());
    });
    stream
}

type StreamEventHook<'a> = (&'a crate::registry::OnProviderStreamEventFn, &'a Model);

async fn read_events<H: SseHandler>(
    builder: &mut MessageBuilder,
    response: reqwest::Response,
    handler: &mut H,
    hook: Option<StreamEventHook<'_>>,
    cancel: Option<&CancellationToken>,
) -> Result<StopReason, String> {
    handler.start(builder);
    builder.start();
    let mut events = std::pin::pin!(sse_events(Box::pin(response.bytes_stream())));
    while let Some(event) = events.next().await {
        let event = event?;
        if let Some((hook, model)) = hook
            && let Ok(data) = serde_json::from_str::<Value>(&event.data)
        {
            hook(&data, model);
        }
        if !handler.event(builder, event)? {
            break;
        }
    }
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Err("Request was aborted".into());
    }
    handler.end(builder)
}

fn settle<H: SseHandler>(builder: MessageBuilder, handler: &H, result: Result<StopReason, String>, cancelled: bool) {
    if cancelled {
        return builder.fail(StopReason::Aborted, "Request was aborted");
    }
    match result {
        Ok(reason) => builder.finish(reason),
        Err(error) => builder.fail(StopReason::Error, handler.error_message(error)),
    }
}

/// Merge `defaults`, then `model.headers`, then `options.headers` (later names replace earlier
/// ones, case-insensitively), then remove `options.suppressed_headers`.
pub(crate) fn header_map(defaults: Vec<(&str, String)>, model: &Model, options: &StreamOptions) -> HeaderMap {
    build_headers(defaults, &model.headers, &options.headers, &options.suppressed_headers)
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

/// The requested reasoning level for budget-style APIs: `None` when off or unsupported,
/// with `xhigh` and `max` clamped to `high` as upstream `clampReasoning` does.
pub(crate) fn reasoning_level(model: &Model, options: &StreamOptions) -> Option<ThinkingLevel> {
    if !model.reasoning {
        return None;
    }
    match options.reasoning? {
        ThinkingLevel::Off => None,
        ThinkingLevel::Xhigh | ThinkingLevel::Max => Some(ThinkingLevel::High),
        level => Some(level),
    }
}

/// Tokens always left for the answer when thinking shares the output ceiling.
pub(crate) const MIN_ANSWER_TOKENS: u64 = crate::simple_options::MIN_ANSWER_TOKENS;

/// Default thinking token budget for a level.
pub(crate) fn thinking_budget(level: ThinkingLevel) -> u64 {
    crate::simple_options::thinking_budget_for_level(level, None)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hash_matches_upstream() {
        assert_eq!(short_hash(""), "k4n83c7h0j2b");
        assert_eq!(short_hash("call_abc|fc_123é"), "ii5wx01p3is0h");
    }
}
