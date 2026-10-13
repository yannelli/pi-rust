//! HTTP requests of the OAuth flow, bounded by a timeout and cancellable.

use std::time::Duration;

use reqwest::RequestBuilder;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::OAuthError;

/// The HTTP client of an OAuth flow. Every request stops when `cancel` fires.
#[derive(Clone)]
pub struct OAuthHttp {
    pub client: reqwest::Client,
    /// Limit of each request, including its body.
    pub timeout: Option<Duration>,
    pub cancel: Option<CancellationToken>,
}

impl Default for OAuthHttp {
    fn default() -> Self {
        Self::new(pi_ai::http_client().clone())
    }
}

/// A response with its whole body.
pub(crate) struct OAuthResponse {
    pub status: u16,
    pub text: String,
}

impl OAuthResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Result<Value, OAuthError> {
        serde_json::from_str(&self.text).map_err(|error| OAuthError::other(format!("Invalid JSON response: {error}")))
    }
}

impl OAuthHttp {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client, timeout: None, cancel: None }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancellationToken::is_cancelled)
    }

    pub(crate) async fn execute(&self, mut request: RequestBuilder) -> Result<OAuthResponse, OAuthError> {
        if let Some(timeout) = self.timeout {
            request = request.timeout(timeout);
        }
        let work = async {
            let response = request.send().await.map_err(map_error)?;
            let status = response.status().as_u16();
            let text = response.text().await.map_err(map_error)?;
            Ok(OAuthResponse { status, text })
        };
        match &self.cancel {
            None => work.await,
            Some(cancel) => tokio::select! {
                result = work => result,
                _ = cancel.cancelled() => Err(OAuthError::Aborted),
            },
        }
    }
}

fn map_error(error: reqwest::Error) -> OAuthError {
    if error.is_timeout() {
        OAuthError::other("The operation was aborted due to timeout")
    } else {
        OAuthError::from_reqwest(error)
    }
}
