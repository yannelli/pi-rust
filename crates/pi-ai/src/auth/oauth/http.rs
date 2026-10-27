//! HTTP, environment, and parsing helpers shared by the OAuth flows.

use std::error::Error as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Method, RequestBuilder, Url};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::auth::AuthError;

pub(crate) type UrlRewrite = Arc<dyn Fn(&str) -> String + Send + Sync>;
pub(crate) type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Test seams: tests route provider URLs to a local mock server and supply env values.
/// `no_timeouts` drops request timeouts, which a paused test clock would fire during IO.
#[derive(Clone, Default)]
pub(crate) struct Hooks {
    pub rewrite: Option<UrlRewrite>,
    pub env: Option<EnvLookup>,
    pub no_timeouts: bool,
}

impl Hooks {
    pub fn timeout(&self, limit: Duration) -> Option<Duration> {
        (!self.no_timeouts).then_some(limit)
    }

    pub fn url(&self, url: &str) -> String {
        match &self.rewrite {
            Some(rewrite) => rewrite(url),
            None => url.to_string(),
        }
    }

    /// A non-empty environment value, as upstream's `getProviderEnvValue(name) || fallback`.
    pub fn env(&self, name: &str) -> Option<String> {
        let value = match &self.env {
            Some(lookup) => lookup(name),
            None => std::env::var(name).ok(),
        };
        value.filter(|value| !value.is_empty())
    }

    /// Callback listen host: `PI_OAUTH_CALLBACK_HOST` or `127.0.0.1`.
    pub fn callback_host(&self) -> String {
        self.env("PI_OAUTH_CALLBACK_HOST").unwrap_or_else(|| "127.0.0.1".into())
    }

    pub fn request(&self, method: Method, url: &str) -> RequestBuilder {
        // The shared client's idle and keepalive timers would move a paused test clock.
        static PLAIN: std::sync::LazyLock<reqwest::Client> =
            std::sync::LazyLock::new(|| reqwest::Client::builder().no_proxy().build().expect("TLS backend"));
        let client = if self.no_timeouts { PLAIN.clone() } else { crate::registry::http_client() };
        client.request(method, self.url(url))
    }

    pub fn get(&self, url: &str) -> RequestBuilder {
        self.request(Method::GET, url)
    }

    pub fn post(&self, url: &str) -> RequestBuilder {
        self.request(Method::POST, url)
    }
}

/// A fully read HTTP response.
#[derive(Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn status_text(&self) -> &'static str {
        reqwest::StatusCode::from_u16(self.status).ok().and_then(|status| status.canonical_reason()).unwrap_or("")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.body)
    }
}

/// Why a request produced no response.
#[derive(Debug)]
pub(crate) enum SendError {
    Cancelled,
    TimedOut,
    Http(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("This operation was aborted"),
            Self::TimedOut => f.write_str("The operation was aborted due to timeout"),
            Self::Http(message) => f.write_str(message),
        }
    }
}

impl From<SendError> for AuthError {
    fn from(error: SendError) -> Self {
        match error {
            SendError::Cancelled => AuthError::Cancelled,
            other => AuthError::Http(other.to_string()),
        }
    }
}

/// A transport error with its source chain, for example `error sending request: connection refused`.
pub(crate) fn describe_error(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// Sends a request and reads the whole body, racing cancellation and an optional timeout.
pub(crate) async fn send(
    request: RequestBuilder,
    cancel: &CancellationToken,
    timeout: Option<Duration>,
) -> Result<Reply, SendError> {
    let exchange = async {
        let response = request.send().await.map_err(|error| SendError::Http(describe_error(&error)))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.map_err(|error| SendError::Http(describe_error(&error)))?;
        Ok(Reply { status, headers, body })
    };
    let timed = async {
        match timeout {
            Some(limit) => tokio::time::timeout(limit, exchange).await.unwrap_or(Err(SendError::TimedOut)),
            None => exchange.await,
        }
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(SendError::Cancelled),
        result = timed => result,
    }
}

/// Sleeps unless cancelled first.
pub(crate) async fn sleep(duration: Duration, cancel: &CancellationToken) -> Result<(), AuthError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AuthError::Cancelled),
        _ = tokio::time::sleep(duration) => Ok(()),
    }
}

/// Unix time in milliseconds.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as i64).unwrap_or(0)
}

/// `Date.now() + seconds * 1000 - margin_ms`, rounded like JavaScript number math.
pub(crate) fn expires_at(seconds: f64, margin_ms: i64) -> i64 {
    now_ms() + (seconds * 1000.0).round() as i64 - margin_ms
}

/// A positive finite JSON number.
pub(crate) fn positive_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|number| number.is_finite() && *number > 0.0)
}

/// A non-empty JSON string.
pub(crate) fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|text| !text.is_empty())
}

/// The URL's normalized form when it parses with an http(s) scheme. Login flows open server-supplied URLs
/// with the platform URL handler, which would also launch a `file:` URL or a custom protocol handler.
pub(crate) fn trusted_http_url(value: Option<&Value>) -> Option<String> {
    let url = Url::parse(non_empty_str(value)?).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// JSON text of an optional value, `null` when absent, as `JSON.stringify`.
pub(crate) fn json_text(value: Option<&Value>) -> String {
    value.map_or_else(|| "null".into(), Value::to_string)
}

/// `new URLSearchParams(text).get(name)`.
pub(crate) fn search_param(text: &str, name: &str) -> Option<String> {
    let mut url = Url::parse("http://localhost/").expect("static URL parses");
    url.set_query(Some(text.strip_prefix('?').unwrap_or(text)));
    url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
}

/// `new URL(text).searchParams.get(name)`, or `None` when `text` is not an absolute URL.
pub(crate) fn url_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
}

/// A pasted redirect URL, `code#state`, query string, or bare code.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AuthorizationInput {
    pub code: Option<String>,
    pub state: Option<String>,
}

pub(crate) fn parse_authorization_input(input: &str) -> AuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return AuthorizationInput::default();
    }
    if let Ok(url) = Url::parse(value) {
        return AuthorizationInput { code: url_param(&url, "code"), state: url_param(&url, "state") };
    }
    if let Some((code, rest)) = value.split_once('#') {
        let state = rest.split('#').next().unwrap_or_default();
        return AuthorizationInput { code: Some(code.into()), state: Some(state.into()) };
    }
    if value.contains("code=") {
        return AuthorizationInput { code: search_param(value, "code"), state: search_param(value, "state") };
    }
    AuthorizationInput { code: Some(value.into()), state: None }
}

/// `JSON.parse(atob(segment))` of a JWT payload; `None` for malformed tokens.
pub(crate) fn decode_jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = super::pkce::base64_decode_lenient(parts[1])?;
    serde_json::from_slice(&bytes).ok()
}
