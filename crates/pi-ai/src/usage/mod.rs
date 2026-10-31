//! Subscription usage limits of pi's logins, cached per provider. Endpoints and sources are in
//! `docs/providers/usage.md`.

mod anthropic;
mod codex;
mod cursor;
mod grok;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_slots;
mod time;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::oauth::ANTIGRAVITY_ACP_MARKER;
use crate::auth::{AuthOverrides, AuthResolver, Credential};

pub use self::time::{iso_from_unix, unix_from_iso};

/// Providers with a usage fetcher, in display order.
pub const USAGE_PROVIDERS: &[&str] = &["anthropic", "openai-codex", "cursor", "xai", "google-antigravity"];

const TTL: Duration = Duration::from_secs(60);
/// Each Antigravity fetch starts the ACP server.
const ANTIGRAVITY_TTL: Duration = Duration::from_secs(300);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether a window limits the whole login or only some models.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowScope {
    #[default]
    Provider,
    Model,
}

/// One limit window. `used_percent` is 0 to 100; `resets_at` is ISO 8601 UTC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
    pub window_minutes: Option<u64>,
    /// The provider's name of the window, such as `five_hour` or `primary_window`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Always sent, so a reader can tell this format from older hosts.
    #[serde(default)]
    pub scope: WindowScope,
    /// Model families a `model` window limits, such as `opus`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub families: Vec<String>,
    /// `resets_at` in Unix milliseconds, as exact as the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
}

impl UsageWindow {
    pub fn new(label: impl Into<String>, used_percent: f64, resets_at: Option<String>, minutes: Option<u64>) -> Self {
        let used_percent = if used_percent.is_finite() { used_percent.clamp(0.0, 100.0) } else { 0.0 };
        let resets_at_ms = resets_at.as_deref().and_then(unix_from_iso).map(|seconds| seconds * 1000);
        Self {
            label: label.into(),
            used_percent,
            resets_at,
            window_minutes: minutes,
            id: None,
            scope: WindowScope::Provider,
            families: Vec::new(),
            resets_at_ms,
        }
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// A window of one model family.
    pub fn for_family(mut self, family: impl Into<String>) -> Self {
        self.scope = WindowScope::Model;
        self.families = vec![family.into()];
        self
    }

    pub fn with_reset_ms(mut self, ms: Option<i64>) -> Self {
        if ms.is_some() {
            self.resets_at_ms = ms;
        }
        self
    }
}

/// Codex credits of a login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCredits {
    pub available: bool,
    pub unlimited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

/// Why a usage read failed, for callers that back off or ask for a login.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageErrorKind {
    /// No login, or the login cannot be refreshed.
    Login,
    /// pi reads no usage for this provider.
    Unsupported,
    /// HTTP 401 or 403.
    Rejected,
    /// HTTP 429.
    RateLimited,
    /// Another HTTP status.
    Http,
    /// The request did not complete.
    Network,
    /// The body is not the expected JSON.
    Response,
}

/// A failed read: the message for the user, its kind, and the `retry-after` delay.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UsageError {
    pub message: String,
    pub kind: UsageErrorKind,
    pub retry_after_ms: Option<i64>,
}

impl UsageError {
    pub(crate) fn new(kind: UsageErrorKind, message: impl Into<String>) -> Self {
        Self { message: message.into(), kind, retry_after_ms: None }
    }
}

impl From<&str> for UsageError {
    fn from(message: &str) -> Self {
        Self::new(UsageErrorKind::Response, message)
    }
}

impl From<String> for UsageError {
    fn from(message: String) -> Self {
        Self::new(UsageErrorKind::Response, message)
    }
}

/// The usage of one provider login. A failure has `error` and no windows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsage {
    pub provider: String,
    pub plan: Option<String>,
    pub windows: Vec<UsageWindow>,
    pub error: Option<String>,
    /// Unix milliseconds.
    pub fetched_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<UsageCredits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<UsageErrorKind>,
    /// The `retry-after` of a failed request, in milliseconds from `fetched_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<i64>,
}

impl ProviderUsage {
    fn ok(provider: &str, plan: Option<String>, windows: Vec<UsageWindow>, credits: Option<UsageCredits>) -> Self {
        let mut usage = Self::empty(provider);
        (usage.plan, usage.windows, usage.credits) = (plan, windows, credits);
        usage
    }

    fn empty(provider: &str) -> Self {
        let (provider, fetched_at) = (provider.into(), crate::now_ms());
        let (plan, windows, error, credits, error_kind, retry_after_ms) = Default::default();
        Self { provider, plan, windows, error, fetched_at, credits, error_kind, retry_after_ms }
    }

    fn failed(provider: &str, error: impl Into<UsageError>) -> Self {
        let error = error.into();
        let mut usage = Self::empty(provider);
        (usage.error, usage.error_kind, usage.retry_after_ms) =
            (Some(error.message), Some(error.kind), error.retry_after_ms);
        usage
    }
}

/// Base URLs of the usage endpoints; tests point them at a local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageEndpoints {
    pub anthropic: String,
    pub chatgpt: String,
    pub cursor: String,
    /// The Grok CLI proxy, for the `xai` login.
    pub grok: String,
}

impl Default for UsageEndpoints {
    fn default() -> Self {
        Self {
            anthropic: "https://api.anthropic.com".into(),
            chatgpt: crate::providers::openai_codex::DEFAULT_CODEX_BASE_URL.into(),
            cursor: crate::providers::cursor::CURSOR_BASE_URL.into(),
            grok: "https://cli-chat-proxy.grok.com".into(),
        }
    }
}

type Slot = Arc<tokio::sync::Mutex<Option<(Instant, ProviderUsage)>>>;

/// Fetches and caches usage with the credentials of an [`AuthResolver`].
pub struct UsageService {
    resolver: Arc<AuthResolver>,
    endpoints: Mutex<UsageEndpoints>,
    slots: Mutex<HashMap<String, Slot>>,
    /// Extension providers that log in with a built-in flow, by the provider of that flow.
    aliases: Mutex<HashMap<String, String>>,
}

impl UsageService {
    pub fn new(resolver: Arc<AuthResolver>) -> Self {
        Self::with_endpoints(resolver, UsageEndpoints::default())
    }

    pub fn with_endpoints(resolver: Arc<AuthResolver>, endpoints: UsageEndpoints) -> Self {
        Self { resolver, endpoints: Mutex::new(endpoints), slots: Mutex::default(), aliases: Mutex::default() }
    }

    /// Point the fetchers at other endpoints, such as a local test server; drops cached results.
    pub fn set_endpoints(&self, endpoints: UsageEndpoints) {
        *self.endpoints.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = endpoints;
        self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
    }

    /// Reads `provider` (an extension provider such as `anthropic-2`) with the fetcher of `base`.
    /// The login, the token refresh, and the cache stay under `provider`. `None` removes the alias.
    /// A built-in `provider` is refused; a `base` without a fetcher removes the alias. `false`
    /// when no alias is set.
    pub fn set_alias(&self, provider: &str, base: Option<&str>) -> bool {
        if USAGE_PROVIDERS.contains(&provider) {
            return false;
        }
        let base = base.filter(|base| USAGE_PROVIDERS.contains(base));
        let mut aliases = self.aliases.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = match base {
            Some(base) => aliases.insert(provider.to_string(), base.to_string()).as_deref() != Some(base),
            None => aliases.remove(provider).is_some(),
        };
        drop(aliases);
        if changed {
            self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(provider);
        }
        base.is_some()
    }

    /// The fetcher of `provider`: its own, or that of its alias.
    fn kind_of(&self, provider: &str) -> Option<String> {
        if USAGE_PROVIDERS.contains(&provider) {
            return Some(provider.to_string());
        }
        self.aliases.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(provider).cloned()
    }

    /// Providers of [`USAGE_PROVIDERS`], then aliased providers, with a stored login.
    pub async fn logged_in(&self) -> Vec<String> {
        let mut aliased: Vec<String> =
            self.aliases.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).keys().cloned().collect();
        aliased.sort();
        let mut providers = Vec::new();
        for provider in USAGE_PROVIDERS.iter().map(|provider| provider.to_string()).chain(aliased) {
            if let Ok(Some(Credential::OAuth(_))) = self.resolver.store().read(&provider).await {
                providers.push(provider);
            }
        }
        providers
    }

    /// Usage of every logged-in provider.
    pub async fn all(&self, refresh: bool) -> Vec<ProviderUsage> {
        let providers = self.logged_in().await;
        futures::future::join_all(providers.iter().map(|provider| self.get(provider, refresh))).await
    }

    /// Usage of `provider`, from the cache when it is fresh and `refresh` is false.
    pub async fn get(&self, provider: &str, refresh: bool) -> ProviderUsage {
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            slots.entry(provider.to_string()).or_default().clone()
        };
        let mut cached = slot.lock().await;
        let ttl = if self.kind_of(provider).as_deref() == Some("google-antigravity") { ANTIGRAVITY_TTL } else { TTL };
        if let Some((at, usage)) = cached.as_ref()
            && !refresh
            && at.elapsed() < ttl
        {
            return usage.clone();
        }
        let usage = self.fetch(provider).await;
        *cached = Some((Instant::now(), usage.clone()));
        usage
    }

    async fn fetch(&self, provider: &str) -> ProviderUsage {
        let Some(kind) = self.kind_of(provider) else {
            let message = format!("pi has no usage report for {provider}");
            return ProviderUsage::failed(provider, UsageError::new(UsageErrorKind::Unsupported, message));
        };
        let login = || UsageError::new(UsageErrorKind::Login, format!("Not logged in. Run /login {provider}."));
        let credential = match self.resolver.store().read(provider).await {
            Ok(Some(Credential::OAuth(credential))) => credential,
            Ok(_) => return ProviderUsage::failed(provider, login()),
            Err(error) => {
                return ProviderUsage::failed(provider, UsageError::new(UsageErrorKind::Login, error.to_string()));
            }
        };
        if kind == "google-antigravity" {
            return match crate::providers::antigravity_acp::usage().await {
                Ok(windows) => ProviderUsage::ok(provider, None, windows, None),
                Err(error) => ProviderUsage::failed(provider, error),
            };
        }
        let token = match self.token(provider).await {
            Ok(token) => token,
            Err(error) => return ProviderUsage::failed(provider, error),
        };
        let plain = |fetched: Fetched| fetched.map(|(plan, windows)| (plan, windows, None));
        let endpoints = self.endpoints.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        let result = match kind.as_str() {
            "anthropic" => plain(anthropic::fetch(&endpoints.anthropic, &token, provider).await),
            "cursor" => plain(cursor::fetch(&endpoints.cursor, &token).await),
            "xai" => plain(grok::fetch(&endpoints.grok, &token).await),
            _ => codex::fetch(&endpoints.chatgpt, &token, credential.extra_str("accountId"), provider).await,
        };
        match result {
            Ok((plan, windows, credits)) => ProviderUsage::ok(provider, plan, windows, credits),
            Err(error) => ProviderUsage::failed(provider, error),
        }
    }

    /// A valid access token; an expiring one is refreshed under the store lock.
    async fn token(&self, provider: &str) -> Result<String, UsageError> {
        let resolved = self.resolver.resolve(provider, &AuthOverrides::default()).await;
        let refresh_failed = |error| format!("Cannot refresh the {provider} login: {error}");
        let resolved = resolved.map_err(|error| UsageError::new(UsageErrorKind::Login, refresh_failed(error)))?;
        resolved
            .and_then(|resolved| resolved.auth.api_key)
            .filter(|token| !token.is_empty() && token != ANTIGRAVITY_ACP_MARKER)
            .ok_or_else(|| UsageError::new(UsageErrorKind::Login, format!("Not logged in. Run /login {provider}.")))
    }
}

pub(crate) type Fetched = Result<(Option<String>, Vec<UsageWindow>), UsageError>;

/// Sends `request` and reads a JSON body; HTTP errors become messages for the user.
pub(crate) async fn json_response(request: reqwest::RequestBuilder, provider: &str) -> Result<Value, UsageError> {
    let failed =
        |error: reqwest::Error| UsageError::new(UsageErrorKind::Network, format!("Usage request failed: {error}"));
    let response = request.timeout(REQUEST_TIMEOUT).send().await.map_err(failed)?;
    let status = response.status().as_u16();
    let header = response.headers().get("retry-after").and_then(|value| value.to_str().ok()).map(str::to_string);
    let text = response.text().await.map_err(failed)?;
    let error = |kind, message: String| {
        let retry_after_ms = header.as_deref().and_then(|value| retry_after_ms(value, crate::now_ms()));
        Err(UsageError { message, kind, retry_after_ms })
    };
    match status {
        200..=299 if text.trim_start().starts_with('<') => Err("The usage endpoint returned HTML".into()),
        200..=299 => serde_json::from_str(&text).map_err(|error| format!("Invalid usage response: {error}").into()),
        401 | 403 => {
            error(UsageErrorKind::Rejected, format!("The login was rejected (HTTP {status}). Run /login {provider}."))
        }
        429 => {
            error(UsageErrorKind::RateLimited, "The usage endpoint is rate limited (HTTP 429). Try again later.".into())
        }
        _ => error(UsageErrorKind::Http, format!("The usage endpoint returned HTTP {status}")),
    }
}

/// A `retry-after` value in milliseconds from `now`: delta seconds or an HTTP date.
pub(crate) fn retry_after_ms(header: &str, now: i64) -> Option<i64> {
    let header = header.trim();
    let ms = match header.parse::<i64>() {
        Ok(seconds) => seconds.saturating_mul(1000),
        Err(_) => time::unix_from_http_date(header)? * 1000 - now,
    };
    (ms > 0).then_some(ms)
}

/// A JSON number, or a number in a string.
pub(crate) fn number(value: &Value) -> Option<f64> {
    let parsed = value.as_f64().or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()));
    parsed.filter(|number: &f64| number.is_finite())
}
