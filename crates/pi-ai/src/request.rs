//! One provider HTTP request: header merging, the proxy-aware pooled client, a timeout
//! until response headers, retries with server delays, and bounded error bodies.

use std::collections::BTreeMap;
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::http::{client_for_env, resolve_proxy_url};
use crate::registry::{OnPayloadFn, OnResponseFn, ProviderResponse};
use crate::retry::{ProviderRequestError, ProviderRetryOptions, retry_provider_request};
use crate::types::Model;

/// Largest error body read from a failed response.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Longest response body kept in a formatted provider error.
pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;
const MAX_ERROR_MESSAGE_CHARS: usize = 500;

/// Request options shared by every provider operation (upstream `ProviderRequestOptions`).
#[derive(Clone, Default)]
pub struct RequestOptions {
    pub api_key: Option<String>,
    pub cancel: CancellationToken,
    /// Explicit parent span for telemetry produced by this logical request.
    pub telemetry: Option<std::sync::Arc<dyn pi_telemetry::TelemetrySpan>>,
    /// Provider-scoped environment values, including proxy variables.
    pub env: BTreeMap<String, String>,
    /// Extra headers; later sources replace earlier ones by case-insensitive name.
    pub headers: BTreeMap<String, String>,
    /// Header names removed from the merged request headers.
    pub suppressed_headers: Vec<String>,
    /// Time until the response arrives, per request.
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    /// Longest server-requested retry delay to honor; 0 disables the cap. Default 60000.
    pub max_retry_delay_ms: Option<u64>,
    pub on_payload: Option<OnPayloadFn>,
    pub on_response: Option<OnResponseFn>,
}

impl std::fmt::Debug for RequestOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestOptions")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout_ms", &self.timeout_ms)
            .field("max_retries", &self.max_retries)
            .finish_non_exhaustive()
    }
}

/// Merge `defaults`, then `model`, then `options` headers by case-insensitive name, then drop `suppressed`.
pub(crate) fn build_headers(
    defaults: Vec<(&str, String)>,
    model: &BTreeMap<String, String>,
    options: &BTreeMap<String, String>,
    suppressed: &[String],
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let custom = model.iter().chain(options.iter()).map(|(name, value)| (name.as_str(), value.clone()));
    for (name, value) in defaults.into_iter().chain(custom) {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value)) {
            headers.insert(name, value);
        }
    }
    for name in suppressed {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    headers
}

/// A JSON POST request, before the payload hook runs.
#[derive(Clone, Debug)]
pub(crate) struct HttpRequest {
    pub url: String,
    pub headers: HeaderMap,
    pub body: Value,
}

/// How one request is sent and retried.
#[derive(Clone, Copy)]
pub(crate) struct SendOptions<'a> {
    pub env: &'a BTreeMap<String, String>,
    pub cancel: &'a CancellationToken,
    pub timeout_ms: Option<u64>,
    pub max_retries: u32,
    pub max_retry_delay_ms: Option<u64>,
    pub no_retry_statuses: &'a [u16],
    /// Service name for `"<label> returned <status>"` messages; `None` uses `Provider HTTP <status>`.
    pub label: Option<&'a str>,
}

impl<'a> SendOptions<'a> {
    pub fn from_request(options: &'a RequestOptions, default_retries: u32) -> Self {
        Self {
            env: &options.env,
            cancel: &options.cancel,
            timeout_ms: options.timeout_ms,
            max_retries: options.max_retries.unwrap_or(default_retries),
            max_retry_delay_ms: options.max_retry_delay_ms,
            no_retry_statuses: &[],
            label: None,
        }
    }
}

/// Apply `on_payload`: a returned value replaces the body.
pub(crate) async fn apply_payload_hook(body: Value, hook: Option<&OnPayloadFn>, model: &Model) -> Value {
    match hook {
        Some(hook) => hook(&body, model).await.unwrap_or(body),
        None => body,
    }
}

pub(crate) fn provider_response(response: &reqwest::Response) -> ProviderResponse {
    ProviderResponse { status: response.status().as_u16(), headers: header_record(response.headers()) }
}

fn header_record(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.to_string())))
        .collect()
}

pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
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
    let parsed = serde_json::from_str::<Value>(trimmed).ok();
    let json_message = parsed.as_ref().and_then(|value| {
        [value.pointer("/error/message"), value.get("message"), value.get("error"), value.get("detail")]
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .find(|message| !message.is_empty())
            .map(str::to_string)
    });
    let code = parsed.as_ref().and_then(|value| {
        value.pointer("/error/code").or_else(|| value.get("code")).and_then(Value::as_str).map(str::to_string)
    });
    let mut message = truncate_chars(&json_message.unwrap_or_else(|| trimmed.to_string()), MAX_ERROR_MESSAGE_CHARS);
    // Keep the error code: retry classification and usage hints match on it.
    if let Some(code) = code.filter(|code| !code.is_empty() && !message.contains(code.as_str())) {
        message.push_str(&format!(" ({code})"));
    }
    // OpenRouter puts the upstream provider's own error in `error.metadata.raw`.
    let raw = parsed.as_ref().and_then(|value| value.pointer("/error/metadata/raw")).and_then(Value::as_str);
    if let Some(raw) = raw.filter(|raw| !raw.is_empty() && !message.contains(raw)) {
        message.push_str(&format!("\n{raw}"));
    }
    format!("Provider HTTP {status}: {message}")
}

pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_string(),
    }
}

/// Upstream `truncateErrorText`: keep `max` UTF-16 units and note how many were dropped.
pub fn truncate_error_text(text: &str, max: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return text.to_string();
    }
    format!("{}... [truncated {} chars]", String::from_utf16_lossy(&units[..max]), units.len() - max)
}

/// Upstream `formatProviderError`: `<prefix> (<status>): <body>` when a body was read, else the message.
pub fn format_provider_error(error: &ProviderRequestError, prefix: Option<&str>) -> String {
    let body = error.body.as_deref().map(str::trim).filter(|body| !body.is_empty() && !error.message.contains(body));
    match (error.status, body, prefix) {
        (Some(status), Some(body), Some(prefix)) => {
            format!("{prefix} ({status}): {}", truncate_error_text(body, MAX_PROVIDER_ERROR_BODY_CHARS))
        }
        (Some(status), Some(body), None) => {
            format!("{status}: {}", truncate_error_text(body, MAX_PROVIDER_ERROR_BODY_CHARS))
        }
        (Some(status), None, Some(prefix)) => format!("{prefix} ({status}): {}", error.message),
        _ => error.message.clone(),
    }
}

/// Send `request` once: apply the timeout and classify the response status.
async fn send_once(
    client: &reqwest::Client,
    request: &HttpRequest,
    options: &SendOptions<'_>,
) -> Result<reqwest::Response, ProviderRequestError> {
    let send = client.post(&request.url).headers(request.headers.clone()).json(&request.body).send();
    let result = match options.timeout_ms {
        Some(timeout) => tokio::time::timeout(Duration::from_millis(timeout), send)
            .await
            .map_err(|_| ProviderRequestError::transport(format!("Request timed out after {timeout}ms")))?,
        None => send.await,
    };
    let response =
        result.map_err(|error| ProviderRequestError::transport(format!("Request failed: {}", error_chain(&error))))?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let headers = header_record(response.headers());
    let body = read_bounded(response).await;
    let message = match options.label {
        Some(label) => format!("{label} returned {}", status.as_u16()),
        None => http_error_message(status.as_u16(), &body),
    };
    Err(ProviderRequestError { status: Some(status.as_u16()), headers, message, body: Some(body), aborted: false })
}

fn retry_options<'a>(options: &SendOptions<'a>) -> ProviderRetryOptions<'a> {
    ProviderRetryOptions {
        max_retries: options.max_retries,
        max_retry_delay_ms: options.max_retry_delay_ms,
        cancel: Some(options.cancel),
        no_retry_statuses: options.no_retry_statuses,
    }
}

/// Send `request` with retries and return the successful response once its headers arrive.
pub(crate) async fn send_request(
    request: &HttpRequest,
    options: &SendOptions<'_>,
) -> Result<reqwest::Response, ProviderRequestError> {
    resolve_proxy_url(&request.url, options.env).map_err(ProviderRequestError::transport)?;
    let client = client_for_env(options.env);
    tokio::select! {
        biased;
        _ = options.cancel.cancelled() => Err(ProviderRequestError::aborted()),
        result = retry_provider_request(|| send_once(&client, request, options), retry_options(options)) => result,
    }
}

/// Send `request` and parse a JSON response body. The timeout and retries cover the body too.
pub(crate) async fn post_json(
    request: &HttpRequest,
    options: &SendOptions<'_>,
) -> Result<(ProviderResponse, Value), ProviderRequestError> {
    resolve_proxy_url(&request.url, options.env).map_err(ProviderRequestError::transport)?;
    let client = client_for_env(options.env);
    let untimed = SendOptions { timeout_ms: None, ..*options };
    let once = || async {
        let read = async {
            let response = send_once(&client, request, &untimed).await?;
            let meta = provider_response(&response);
            let text = response
                .text()
                .await
                .map_err(|error| ProviderRequestError::transport(format!("Request failed: {}", error_chain(&error))))?;
            let json = serde_json::from_str(&text)
                .map_err(|error| ProviderRequestError::transport(format!("Invalid JSON response: {error}")))?;
            Ok((meta, json))
        };
        match options.timeout_ms {
            Some(timeout) => tokio::time::timeout(Duration::from_millis(timeout), read).await.unwrap_or_else(|_| {
                Err(ProviderRequestError::transport(format!("Request timed out after {timeout}ms")))
            }),
            None => read.await,
        }
    };
    tokio::select! {
        biased;
        _ = options.cancel.cancelled() => Err(ProviderRequestError::aborted()),
        result = retry_provider_request(once, retry_options(options)) => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_errors_prefer_json_messages() {
        assert_eq!(
            http_error_message(429, r#"{"error":{"message":"Rate limited","type":"x"}}"#),
            "Provider HTTP 429: Rate limited"
        );
        assert_eq!(http_error_message(502, "bad gateway"), "Provider HTTP 502: bad gateway");
        assert_eq!(http_error_message(500, ""), "Provider HTTP 500");
        assert_eq!(
            http_error_message(429, r#"{"error":{"message":"Quota","code":"insufficient_quota"}}"#),
            "Provider HTTP 429: Quota (insufficient_quota)"
        );
        assert_eq!(
            http_error_message(400, r#"{"error":{"message":"Provider returned error","metadata":{"raw":"bad tool"}}}"#),
            "Provider HTTP 400: Provider returned error\nbad tool"
        );
    }

    #[test]
    fn provider_errors_format_like_upstream() {
        let http = |body: Option<&str>| ProviderRequestError {
            status: Some(401),
            message: "System One API returned 401".into(),
            body: body.map(String::from),
            ..Default::default()
        };
        assert_eq!(format_provider_error(&http(Some(" bad key ")), Some("API error")), "API error (401): bad key");
        assert_eq!(
            format_provider_error(&http(Some("")), Some("API error")),
            "API error (401): System One API returned 401"
        );
        assert_eq!(format_provider_error(&http(Some("bad key")), None), "401: bad key");
        let plain = ProviderRequestError::transport("Request timed out after 5ms");
        assert_eq!(format_provider_error(&plain, Some("API error")), "Request timed out after 5ms");
        assert_eq!(truncate_error_text("abcdef", 4), "abcd... [truncated 2 chars]");
    }

    #[test]
    fn headers_merge_case_insensitively_and_drop_suppressed_names() {
        let model = BTreeMap::from([("X-Model".to_string(), "m".to_string())]);
        let options = BTreeMap::from([("user-agent".to_string(), "custom".to_string())]);
        let headers = build_headers(
            vec![("User-Agent", "pi".into()), ("x-default", "d".into())],
            &model,
            &options,
            &["X-Default".to_string()],
        );
        assert_eq!(headers["user-agent"], "custom");
        assert_eq!(headers["x-model"], "m");
        assert!(!headers.contains_key("x-default"));
    }
}
