//! Request and parsing helpers shared by the classifier APIs (upstream `classifier-shared.ts`).

use serde_json::Value;

use crate::request::{
    HttpRequest, RequestOptions, SendOptions, apply_payload_hook, build_headers, format_provider_error, post_json,
};
use crate::retry::ProviderRequestError;
use crate::types::{Model, Usage};

/// Classifier requests retry twice unless the caller says otherwise (`maxRetries ?? 2`).
pub(crate) const DEFAULT_RETRIES: u32 = 2;

/// A failure without HTTP details; it displays as its message.
pub(crate) fn failure(message: impl Into<String>) -> ProviderRequestError {
    ProviderRequestError::transport(message)
}

/// The display message of a failed classification or image request, as upstream catch blocks build it.
pub(crate) fn error_message(error: &ProviderRequestError, prefix: Option<&str>) -> String {
    if error.aborted {
        // Upstream retry wrappers replace any failure after cancellation with this error.
        return "Request aborted".into();
    }
    if error.message.starts_with("Server requested ") {
        return error.message.clone();
    }
    format_provider_error(error, prefix)
}

/// `{base_url}/{path}` with trailing slashes of the base removed.
pub(crate) fn endpoint(base_url: &str, path: &str) -> String {
    format!("{}/{path}", base_url.trim_end_matches('/'))
}

/// JavaScript `String(value)` for scalar JSON values; a missing value reads `undefined`.
pub(crate) fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Object(_)) => "[object Object]".into(),
        Some(other) => other.to_string(),
    }
}

/// A finite JSON number, or `<label> returned an invalid <field>`.
pub(crate) fn required_number(label: &str, value: Option<&Value>, field: &str) -> Result<f64, ProviderRequestError> {
    value.and_then(Value::as_f64).ok_or_else(|| failure(format!("{label} returned an invalid {field}")))
}

/// POST one JSON classifier request with bearer auth, the payload and response hooks, and retries.
/// Statuses in `no_retry_statuses` fail at once although they are normally retried.
pub(crate) async fn post_classifier_request(
    label: &str,
    url: String,
    model: &Model,
    body: Value,
    options: &RequestOptions,
    no_retry_statuses: &[u16],
) -> Result<Value, ProviderRequestError> {
    let Some(api_key) = options.api_key.as_deref().filter(|key| !key.is_empty()) else {
        return Err(failure(format!("No API key for provider: {}", model.provider)));
    };
    let headers = build_headers(
        vec![("authorization", format!("Bearer {api_key}")), ("content-type", "application/json".into())],
        &model.headers,
        &options.headers,
        &options.suppressed_headers,
    );
    let request = HttpRequest { url, headers, body: apply_payload_hook(body, options.on_payload.as_ref(), model) };
    let send =
        SendOptions { label: Some(label), no_retry_statuses, ..SendOptions::from_request(options, DEFAULT_RETRIES) };
    let (response, json) = post_json(&request, &send).await?;
    if let Some(on_response) = &options.on_response {
        on_response(&response, model);
    }
    Ok(json)
}

/// Usage from `{ input_tokens, output_tokens }`, priced from the model catalog. A missing or
/// malformed usage object leaves the result without usage instead of failing it.
pub(crate) fn parse_classifier_usage(value: Option<&Value>, model: &Model) -> Option<Usage> {
    let usage = value?.as_object()?;
    if !usage.contains_key("input_tokens") && !usage.contains_key("output_tokens") {
        return None;
    }
    let count = |key: &str| usage.get(key).and_then(Value::as_f64).filter(|count| *count > 0.0).map_or(0, |n| n as u64);
    let (input, output) = (count("input_tokens"), count("output_tokens"));
    let mut usage = Usage { input, output, total_tokens: input + output, ..Usage::default() };
    usage.apply_cost(&model.cost);
    Some(usage)
}
