//! Bearer tokens for HTTP transports.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use url::Url;

use crate::error::McpError;

/// A 401 response, or a 403 whose challenge reports `insufficient_scope`.
#[derive(Clone, Debug)]
pub struct UnauthorizedContext {
    pub status: u16,
    pub headers: HeaderMap,
    pub server_url: Url,
    pub client: reqwest::Client,
    /// Access token the rejected request carried. A different current token means another request
    /// already refreshed it.
    pub token: Option<String>,
}

impl UnauthorizedContext {
    pub fn www_authenticate(&self) -> Option<&str> {
        self.headers.get(reqwest::header::WWW_AUTHENTICATE).and_then(|value| value.to_str().ok())
    }
}

/// Supplies bearer tokens to an HTTP transport and may refresh them after a 401.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Token for the next request. May refresh it over the network first.
    async fn token(&self) -> Result<Option<String>, McpError>;

    /// Handle a rejected request. `Ok(true)` retries it once with the token then current;
    /// `Ok(false)` leaves the response as it is.
    async fn on_unauthorized(&self, _context: UnauthorizedContext) -> Result<bool, McpError> {
        Ok(false)
    }
}

/// Sends a fixed token, or none.
pub struct StaticToken(pub Option<String>);

#[async_trait]
impl AuthProvider for StaticToken {
    async fn token(&self) -> Result<Option<String>, McpError> {
        Ok(self.0.clone())
    }
}
