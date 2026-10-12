//! Errors of the MCP client, its transports, and OAuth.

use serde_json::Value;

/// JSON-RPC error codes.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum McpError {
    /// A JSON-RPC error response, or an invalid message.
    #[error("{message}")]
    Rpc { code: i64, message: String, data: Option<Value> },
    #[error("{0}")]
    ConnectionClosed(String),
    #[error("MCP request timed out after {0}ms")]
    Timeout(u64),
    #[error("MCP request aborted")]
    Aborted,
    #[error("{message}")]
    Http { status: u16, message: String, body: String },
    /// HTTP 401.
    #[error("MCP server requires authentication")]
    AuthRequired { body: String, www_authenticate: Option<String> },
    /// HTTP 404 for an established session.
    #[error("MCP session expired")]
    SessionExpired { body: String },
    /// A request that never got an HTTP response, like a refused connection or a dropped body.
    #[error("{0}")]
    Network(String),
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error("{0}")]
    Other(String),
}

impl McpError {
    pub fn rpc(code: i64, message: impl Into<String>) -> Self {
        Self::Rpc { code, message: message.into(), data: None }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::rpc(codes::INVALID_REQUEST, message)
    }

    pub fn closed() -> Self {
        Self::ConnectionClosed("MCP connection closed".into())
    }

    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    /// HTTP status of HTTP errors.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            Self::AuthRequired { .. } => Some(401),
            Self::SessionExpired { .. } => Some(404),
            _ => None,
        }
    }

    /// Response body of HTTP errors.
    pub fn body(&self) -> Option<&str> {
        match self {
            Self::Http { body, .. } | Self::AuthRequired { body, .. } | Self::SessionExpired { body } => Some(body),
            _ => None,
        }
    }

    pub fn is_method_not_found(&self) -> bool {
        matches!(self, Self::Rpc { code, .. } if *code == codes::METHOD_NOT_FOUND)
    }

    /// Network failures and overloaded or restarting servers, worth another try.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) => true,
            Self::Http { status, .. } => *status == 408 || *status == 429 || (*status >= 500 && *status != 501),
            _ => false,
        }
    }

    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        Self::Network(network_message(&error))
    }
}

/// The message of a request error, with its causes.
pub(crate) fn network_message(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum OAuthError {
    /// An OAuth error response (`error`, `error_description`, `error_uri`).
    #[error("{message}")]
    Server { code: String, message: String, error_uri: Option<String> },
    #[error("OAuth issuer mismatch: expected {expected:?}, received {}", received.as_ref().map(|value| format!("{value:?}")).unwrap_or_else(|| "none".into()))]
    IssuerMismatch { expected: String, received: Option<String> },
    #[error("Refusing to send OAuth credentials to non-HTTPS endpoint {0}")]
    InsecureEndpoint(String),
    #[error("OAuth dynamic client registration failed with status {status}: {body}")]
    Registration { status: u16, body: String },
    /// The user has to authorize in the browser.
    #[error("MCP OAuth authorization requires user interaction")]
    AuthorizationRequired,
    #[error("{0}")]
    Network(String),
    #[error("OAuth request aborted")]
    Aborted,
    #[error("{0}")]
    Other(String),
}

impl OAuthError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    pub fn server(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let message = message.into();
        Self::Server { message: if message.is_empty() { code.clone() } else { message }, code, error_uri: None }
    }

    /// The OAuth error code of error responses.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Server { code, .. } => Some(code),
            _ => None,
        }
    }

    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        Self::Network(network_message(&error))
    }
}
