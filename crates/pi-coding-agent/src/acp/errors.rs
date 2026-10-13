//! JSON-RPC errors in the shapes of the TypeScript SDK's `RequestError` helpers.

use agent_client_protocol_schema::Error;
use serde_json::{Value, json};

use super::auth::{auth_methods_json, default_launch};

pub type AcpResult<T> = Result<T, Error>;

const AUTH_REQUIRED_MESSAGE: &str = "Configure an API key or log in with an OAuth provider.";

/// `RequestError.invalidParams(data)`: the detail travels as `data`.
pub fn invalid_params(data: impl Into<Value>) -> Error {
    Error::new(-32602, "Invalid params").data(data.into())
}

/// `RequestError.internalError(data, message)`.
pub fn internal_error(data: Value, message: &str) -> Error {
    Error::new(-32603, format!("Internal error: {message}")).data(data)
}

/// An unexpected failure, as the SDK reports a thrown non-`RequestError`.
pub fn internal_failure(error: &anyhow::Error) -> Error {
    Error::new(-32603, "Internal error").data(json!({"details": format!("{error:#}")}))
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
