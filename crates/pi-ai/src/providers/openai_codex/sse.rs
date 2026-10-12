//! Codex SSE transport: zstd request bodies, Codex retry rules, and friendly usage-limit errors.

use std::time::Duration;

use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value;

use super::Prepared;
use super::events::{CodexError, CodexEvents};
use super::request::{compress_zstd, resolve_codex_url};
use crate::http::client_for_env;
use crate::registry::StreamOptions;
use crate::request::{error_chain, provider_response};
use crate::retry::parse_http_date_ms;
use crate::sse::sse_events;
use crate::stream::MessageBuilder;
use crate::types::{StopReason, now_ms};

const BASE_DELAY_MS: u64 = 1000;
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

/// True when `text` contains `left`, at most one character, then `right` (regex `left.?right`).
fn contains_with_gap(text: &str, left: &str, right: &str) -> bool {
    text.match_indices(left).any(|(index, _)| {
        let rest = &text[index + left.len()..];
        rest.starts_with(right) || rest.chars().next().is_some_and(|c| rest[c.len_utf8()..].starts_with(right))
    })
}

fn is_terminal_rate_limit(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "gousagelimiterror",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn is_retryable(status: u16, text: &str) -> bool {
    if status == 429 && is_terminal_rate_limit(text) {
        return false;
    }
    if matches!(status, 429 | 500 | 502 | 503 | 504) {
        return true;
    }
    let lower = text.to_lowercase();
    contains_with_gap(&lower, "rate", "limit")
        || lower.contains("overloaded")
        || contains_with_gap(&lower, "service", "unavailable")
        || contains_with_gap(&lower, "upstream", "connect")
        || contains_with_gap(&lower, "connection", "refused")
}

fn retry_after_ms(headers: &HeaderMap) -> Option<f64> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::trim);
    if let Some(millis) = header("retry-after-ms").and_then(|value| value.parse::<f64>().ok()).filter(|v| v.is_finite())
    {
        return Some(millis.max(0.0));
    }
    let value = header("retry-after")?;
    match value.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() => Some((seconds * 1000.0).max(0.0)),
        _ => parse_http_date_ms(value).map(|at| (at - now_ms()).max(0) as f64),
    }
}

/// Upstream `parseErrorResponse`: the ChatGPT usage-limit message when it applies.
pub(super) fn error_message(status: u16, reason: &str, text: &str) -> String {
    let mut message = if text.is_empty() { reason.to_string() } else { text.to_string() };
    if message.is_empty() {
        message = "Request failed".into();
    }
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let Some(error) = parsed.as_ref().and_then(|value| value.get("error")).filter(|error| error.is_object()) else {
        return message;
    };
    let code = error["code"].as_str().or(error["type"].as_str()).unwrap_or_default().to_lowercase();
    let mut friendly = None;
    if ["usage_limit_reached", "usage_not_included", "rate_limit_exceeded"].iter().any(|c| code.contains(c))
        || status == 429
    {
        let plan =
            error["plan_type"].as_str().map(|plan| format!(" ({} plan)", plan.to_lowercase())).unwrap_or_default();
        let when = error["resets_at"].as_f64().map(|resets| {
            let minutes = ((resets * 1000.0 - now_ms() as f64) / 60_000.0).round().max(0.0);
            format!(" Try again in ~{minutes} min.")
        });
        friendly = Some(
            format!("You have hit your ChatGPT usage limit{plan}.{}", when.unwrap_or_default()).trim().to_string(),
        );
    }
    if let Some(text) = error["message"].as_str().filter(|text| !text.is_empty()) {
        message = text.to_string();
    } else if let Some(friendly) = &friendly {
        message = friendly.clone();
    }
    friendly.unwrap_or(message)
}

enum SendFailure {
    Failed { message: String, delay_ms: Option<u64> },
    DelayTooLong(String),
}

async fn send_once(
    prepared: &Prepared,
    body: &[u8],
    options: &StreamOptions,
) -> Result<reqwest::Response, SendFailure> {
    let mut request =
        client_for_env(&options.env).post(resolve_codex_url(&prepared.model.base_url)).body(body.to_vec());
    for (name, value) in &prepared.sse_headers.0 {
        request = request.header(name.as_str(), value.as_str());
    }
    request = request.header("content-encoding", "zstd");
    let sent = match options.timeout_ms.filter(|timeout| *timeout > 0) {
        Some(timeout) => tokio::time::timeout(Duration::from_millis(timeout), request.send()).await.map_err(|_| {
            SendFailure::Failed {
                message: format!("Codex SSE response headers timed out after {timeout}ms"),
                delay_ms: None,
            }
        })?,
        None => request.send().await,
    };
    let response = sent.map_err(|error| SendFailure::Failed { message: error_chain(&error), delay_ms: None })?;
    if let Some(hook) = &options.on_response {
        hook(&provider_response(&response), &prepared.model);
    }
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let headers = response.headers().clone();
    let reason = status.canonical_reason().unwrap_or_default().to_string();
    let text = response.text().await.unwrap_or_default();
    let message = error_message(status.as_u16(), &reason, &text);
    if !is_retryable(status.as_u16(), &text) {
        return Err(SendFailure::Failed { message, delay_ms: None });
    }
    let max = options.max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    match retry_after_ms(&headers) {
        Some(delay) if max > 0 && delay > max as f64 => Err(SendFailure::DelayTooLong(format!(
            "Server requested {}s retry delay (max: {}s)",
            (delay / 1000.0).ceil(),
            (max as f64 / 1000.0).ceil()
        ))),
        delay => Err(SendFailure::Failed { message, delay_ms: delay.map(|delay| delay as u64) }),
    }
}

/// POST the request and stream its events. Every failure is retried while retries remain,
/// except usage limits and over-long server delays, as upstream does.
pub(super) async fn request(
    prepared: &Prepared,
    events: &mut CodexEvents,
    builder: &mut MessageBuilder,
    options: &StreamOptions,
) -> Result<StopReason, CodexError> {
    let body = compress_zstd(prepared.body_json.as_bytes());
    let max_retries = options.max_retries.unwrap_or(0);
    let mut attempt = 0;
    let response = loop {
        let failure = match send_once(prepared, &body, options).await {
            Ok(response) => break response,
            Err(failure) => failure,
        };
        let (message, delay_ms) = match failure {
            SendFailure::DelayTooLong(message) => return Err(CodexError::Transport(message)),
            SendFailure::Failed { message, delay_ms } => (message, delay_ms),
        };
        if attempt >= max_retries || message.contains("usage limit") {
            return Err(CodexError::Transport(message));
        }
        let delay = delay_ms.unwrap_or(BASE_DELAY_MS * 2u64.pow(attempt));
        tokio::time::sleep(Duration::from_millis(delay)).await;
        attempt += 1;
    };
    if !events.started {
        events.started = true;
        builder.start();
    }
    let mut stream = std::pin::pin!(sse_events(Box::pin(response.bytes_stream())));
    while let Some(event) = stream.next().await {
        let event = event.map_err(CodexError::Transport)?;
        let data = event.data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if events.event(builder, data, "SSE")? {
            break;
        }
    }
    events.end(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_retryable_codex_errors() {
        assert!(is_retryable(503, ""));
        assert!(!is_retryable(429, "insufficient_quota"));
        assert!(is_retryable(400, "Rate-limit hit"));
        assert!(is_retryable(400, "upstream connect error"));
        assert!(!is_retryable(400, "bad request"));
    }

    #[test]
    fn formats_usage_limit_errors() {
        let body = r#"{"error":{"code":"usage_limit_reached","message":"","plan_type":"PLUS"}}"#;
        assert_eq!(error_message(429, "Too Many Requests", body), "You have hit your ChatGPT usage limit (plus plan).");
        assert_eq!(error_message(500, "Internal Server Error", r#"{"error":{"message":"boom"}}"#), "boom");
        assert_eq!(error_message(502, "Bad Gateway", ""), "Bad Gateway");
    }
}
