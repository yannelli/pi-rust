//! Shared plumbing for the HTTP streaming providers: the request driver (payload and response
//! hooks, retries, timeouts, proxies), header merging, compat flags, and small helpers.

use std::sync::Arc;

use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub(crate) use super::transform::{split_error_tool_result_images, transform_messages};
use crate::registry::StreamOptions;
pub(crate) use crate::request::HttpRequest;
use crate::request::{SendOptions, apply_payload_hook, build_headers, provider_response, send_request};
use crate::retry::ProviderRequestError;
use crate::sse::{SseEvent, sse_events};
use crate::stream::{AssistantMessageEventStream, EventSender, MessageBuilder, event_channel};
use crate::types::{ContentBlock, Model, StopReason};

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

/// A body for one more attempt, with the diagnostic that records the recovery.
pub(crate) struct RequestRewrite {
    pub body: Value,
    pub diagnostic: Value,
}

/// Decides, from a rejection that arrived before any stream started, whether to send once more
/// with a different body. It runs at most once per stream and never after cancellation.
pub(crate) type RequestFallback = Arc<dyn Fn(&ProviderRequestError) -> Option<RequestRewrite> + Send + Sync>;

/// POST `request` with the hooks, retries, timeout, and proxy settings of `options`, and
/// stream the SSE response through `handler`. `on_payload` runs before this returns.
pub(crate) fn spawn_provider_stream<H: SseHandler>(
    model: &Model,
    options: &StreamOptions,
    request: HttpRequest,
    handler: H,
) -> AssistantMessageEventStream {
    spawn_provider_stream_with(model, options, request, |_| handler)
}

/// [`spawn_provider_stream`] for handlers that emit some block events themselves through a
/// clone of the event sender, for example to append to a block that is no longer the last one.
pub(crate) fn spawn_provider_stream_with<H: SseHandler>(
    model: &Model,
    options: &StreamOptions,
    request: HttpRequest,
    make_handler: impl FnOnce(EventSender) -> H,
) -> AssistantMessageEventStream {
    spawn_provider_stream_with_fallback(model, options, request, None, make_handler)
}

/// [`spawn_provider_stream`] that can send a rewritten body once when the server rejects the
/// first request; `on_payload` runs again for the rewritten body.
pub(crate) fn spawn_provider_stream_with_fallback<H: SseHandler>(
    model: &Model,
    options: &StreamOptions,
    mut request: HttpRequest,
    fallback: Option<RequestFallback>,
    make_handler: impl FnOnce(EventSender) -> H,
) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let mut handler = make_handler(sender.clone());
    let mut builder = MessageBuilder::new(model, sender);
    let (model, options) = (model.clone(), options.clone());
    tokio::spawn(async move {
        let body = std::mem::take(&mut request.body);
        request.body = apply_payload_hook(body, options.on_payload.as_ref(), &model).await;
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
            let mut fallback = fallback;
            let response = loop {
                let error = match send_request(&request, &send).await {
                    Ok(response) => break response,
                    Err(error) => error,
                };
                let rewrite = fallback.take().filter(|_| !options.cancel.is_cancelled()).and_then(|f| f(&error));
                let Some(rewrite) = rewrite else { return Err(error.message) };
                builder.add_diagnostic(rewrite.diagnostic);
                request.body = apply_payload_hook(rewrite.body, options.on_payload.as_ref(), &model).await;
            };
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

/// Like [`header_map`], with `overrides` applied after `model.headers` and before `options.headers`.
pub(crate) fn header_map_with(
    defaults: Vec<(&str, String)>,
    overrides: Vec<(&str, String)>,
    model: &Model,
    options: &StreamOptions,
) -> HeaderMap {
    let mut model_headers = model.headers.clone();
    for (name, value) in overrides {
        model_headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
        model_headers.insert(name.to_string(), value);
    }
    build_headers(defaults, &model_headers, &options.headers, &options.suppressed_headers)
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
