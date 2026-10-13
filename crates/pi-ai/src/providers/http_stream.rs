//! Request plumbing on top of [`crate::request`] for adapters that format errors from the raw
//! response, report failed responses to `on_response`, or read SSE events themselves.

use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::providers::common::SseHandler;
use crate::registry::{ProviderResponse, StreamOptions};
use crate::request::{HttpRequest, SendOptions, provider_response, send_request};
use crate::retry::ProviderRequestError;
use crate::sse::{SseEvent, sse_events};
use crate::stream::{AssistantMessageEventStream, MessageBuilder, event_channel};
use crate::types::{Model, StopReason};

/// When `on_response` runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseReport {
    Never,
    /// Successful and failed responses, as adapters that call `fetch` directly do.
    Always,
    SuccessOnly,
}

pub(crate) struct SendConfig {
    pub max_retries: u32,
    /// Deadline for response headers, with the message used when it passes.
    pub timeout: Option<(u64, String)>,
    pub report: ResponseReport,
}

/// POST `request` with retries, proxies, and cancellation from `options`.
pub(crate) async fn send(
    request: &HttpRequest,
    model: &Model,
    options: &StreamOptions,
    config: &SendConfig,
) -> Result<reqwest::Response, ProviderRequestError> {
    let send_options = SendOptions {
        env: &options.env,
        cancel: &options.cancel,
        timeout_ms: config.timeout.as_ref().map(|(ms, _)| *ms),
        max_retries: config.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        no_retry_statuses: &[],
        label: None,
    };
    let result = send_request(request, &send_options).await;
    let report = |response: ProviderResponse| {
        let wanted = match config.report {
            ResponseReport::Never => false,
            ResponseReport::Always => true,
            ResponseReport::SuccessOnly => (200..300).contains(&response.status),
        };
        if let (true, Some(hook)) = (wanted, &options.on_response) {
            hook(&response, model);
        }
    };
    match result {
        Ok(response) => {
            report(provider_response(&response));
            Ok(response)
        }
        Err(mut error) => {
            if let Some(status) = error.status {
                report(ProviderResponse { status, headers: error.headers.clone() });
            }
            if let (Some((ms, message)), None) = (&config.timeout, error.status)
                && error.message == format!("Request timed out after {ms}ms")
            {
                error.message = message.clone();
            }
            Err(error)
        }
    }
}

/// The reason phrase of an HTTP status, such as `Bad Gateway`.
pub(crate) fn status_text(status: u16) -> &'static str {
    reqwest::StatusCode::from_u16(status).ok().and_then(|status| status.canonical_reason()).unwrap_or_default()
}

/// SSE events of a response body. When `on_provider_stream_event` is set, each JSON payload
/// is passed to it before the adapter sees the event.
pub(crate) fn hooked_events<'a>(
    response: reqwest::Response,
    model: &'a Model,
    options: &'a StreamOptions,
) -> impl Stream<Item = Result<SseEvent, String>> + 'a {
    sse_events(Box::pin(response.bytes_stream())).map(move |event| {
        if let (Ok(event), Some(hook)) = (&event, &options.on_provider_stream_event)
            && let Ok(value) = serde_json::from_str::<Value>(&event.data)
        {
            hook(&value, model);
        }
        event
    })
}

/// Send `request` and feed its SSE body to `handler` through a `MessageBuilder`. `format`
/// turns a failed request into the adapter's error message.
pub(crate) fn spawn_builder_stream<H: SseHandler>(
    model: &Model,
    options: StreamOptions,
    request: HttpRequest,
    config: SendConfig,
    format: fn(ProviderRequestError) -> String,
    mut handler: H,
) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let model = model.clone();
    tokio::spawn(async move {
        let mut builder = MessageBuilder::new(&model, sender);
        let drive = async {
            let response = send(&request, &model, &options, &config).await.map_err(format)?;
            handler.start(&mut builder);
            builder.start();
            let mut events = std::pin::pin!(hooked_events(response, &model, &options));
            while let Some(event) = events.next().await {
                if !handler.event(&mut builder, event?)? {
                    break;
                }
            }
            handler.end(&mut builder)
        };
        let cancel = options.cancel.clone();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(String::new()),
            result = drive => result,
        };
        match result {
            _ if cancel.is_cancelled() => builder.fail(StopReason::Aborted, "Request was aborted"),
            Ok(reason) => builder.finish(reason),
            Err(error) => builder.fail(StopReason::Error, handler.error_message(error)),
        }
    });
    stream
}
