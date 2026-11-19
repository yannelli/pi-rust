//! JSON-RPC errors in the shapes of the TypeScript SDK's `RequestError` helpers.

use agent_client_protocol_schema::Error;
use serde_json::{Value, json};

use super::auth::{auth_methods_json, default_launch};

pub type AcpResult<T> = Result<T, Error>;

const AUTH_REQUIRED_MESSAGE: &str = "Configure an API key or log in with an OAuth provider.";

/// `RequestError.invalidParams(data)`, with a text detail also in the message: clients such as
/// Paseo show only the message when `data` is no object.
pub fn invalid_params(data: impl Into<Value>) -> Error {
    let data = data.into();
    let message = match data.as_str().map(str::trim) {
        Some(detail) if !detail.is_empty() => format!("Invalid params: {detail}"),
        _ => "Invalid params".to_string(),
    };
    Error::new(-32602, message).data(data)
}

/// `RequestError.internalError(data, message)`.
pub fn internal_error(data: Value, message: &str) -> Error {
    Error::new(-32603, format!("Internal error: {message}")).data(data)
}

/// An unexpected failure, as the SDK reports a thrown non-`RequestError`.
pub fn internal_failure(error: &anyhow::Error) -> Error {
    Error::new(-32603, "Internal error").data(json!({"details": format!("{error:#}")}))
}

/// A prompt that failed, with pi's message as the whole message so clients show it as is.
pub fn prompt_failed(message: &str) -> Error {
    let message = message.trim();
    Error::new(-32603, if message.is_empty() { "The prompt failed" } else { message })
}

/// `RequestError.methodNotFound(method)`.
pub fn method_not_found(method: &str) -> Error {
    Error::new(-32601, format!("\"Method not found\": {method}")).data(json!({"method": method}))
}

/// `RequestError.authRequired` with the terminal login method, so clients can offer it.
pub fn auth_required() -> Error {
    let data = json!({"authMethods": auth_methods_json(true, &default_launch())});
    Error::new(-32000, format!("Authentication required: {AUTH_REQUIRED_MESSAGE}")).data(data)
}

/// Whether an error from pi looks like missing credentials (upstream `maybeAuthRequiredError`).
pub fn is_auth_failure(message: &str) -> bool {
    const PATTERNS: [&str; 11] = [
        "api key",
        "apikey",
        "missing key",
        "no key",
        "not configured",
        "unauthorized",
        "authentication",
        "permission denied",
        "forbidden",
        "401",
        "403",
    ];
    let lower = message.to_lowercase();
    PATTERNS.iter().any(|pattern| lower.contains(pattern))
}

/// AUTH_REQUIRED for credential failures, `None` for anything else.
pub fn maybe_auth_required(error: &anyhow::Error) -> Option<Error> {
    is_auth_failure(&format!("{error:#}")).then(auth_required)
}

/// AUTH_REQUIRED for credential failures, otherwise an internal error with the message.
pub fn auth_or_internal(error: &anyhow::Error) -> Error {
    maybe_auth_required(error).unwrap_or_else(|| internal_error(json!({}), &format!("{error:#}")))
}

/// The numeric code of an error.
pub fn code(error: &Error) -> i32 {
    error.code.into()
}
