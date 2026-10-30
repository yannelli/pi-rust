//! Subscription usage limits of pi's logins, cached per provider. Endpoints and sources are in
//! `docs/providers/usage.md`.

mod anthropic;
mod codex;
mod cursor;
#[cfg(test)]
mod tests;
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
pub const USAGE_PROVIDERS: &[&str] = &["anthropic", "openai-codex", "cursor", "google-antigravity"];

const TTL: Duration = Duration::from_secs(60);
/// Each Antigravity fetch starts the ACP server.
const ANTIGRAVITY_TTL: Duration = Duration::from_secs(300);
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// One limit window. `used_percent` is 0 to 100; `resets_at` is ISO 8601 UTC.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<String>,
    pub window_minutes: Option<u64>,
}

impl UsageWindow {
    pub fn new(label: impl Into<String>, used_percent: f64, resets_at: Option<String>, minutes: Option<u64>) -> Self {
        let used_percent = if used_percent.is_finite() { used_percent.clamp(0.0, 100.0) } else { 0.0 };
        Self { label: label.into(), used_percent, resets_at, window_minutes: minutes }
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
}

impl ProviderUsage {
    fn ok(provider: &str, plan: Option<String>, windows: Vec<UsageWindow>) -> Self {
        Self { provider: provider.into(), plan, windows, error: None, fetched_at: crate::now_ms() }
    }

    fn failed(provider: &str, error: impl Into<String>) -> Self {
        let error = Some(error.into());
        Self { provider: provider.into(), plan: None, windows: Vec::new(), error, fetched_at: crate::now_ms() }
    }
}

/// Base URLs of the usage endpoints; tests point them at a local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageEndpoints {
    pub anthropic: String,
    pub chatgpt: String,
    pub cursor: String,
}

impl Default for UsageEndpoints {
    fn default() -> Self {
        Self {
            anthropic: "https://api.anthropic.com".into(),
            chatgpt: crate::providers::openai_codex::DEFAULT_CODEX_BASE_URL.into(),
            cursor: crate::providers::cursor::CURSOR_BASE_URL.into(),
        }
    }
}

type Slot = Arc<tokio::sync::Mutex<Option<(Instant, ProviderUsage)>>>;

/// Fetches and caches usage with the credentials of an [`AuthResolver`].
pub struct UsageService {
    resolver: Arc<AuthResolver>,
    endpoints: UsageEndpoints,
    slots: Mutex<HashMap<String, Slot>>,
}

impl UsageService {
    pub fn new(resolver: Arc<AuthResolver>) -> Self {
        Self::with_endpoints(resolver, UsageEndpoints::default())
    }

    pub fn with_endpoints(resolver: Arc<AuthResolver>, endpoints: UsageEndpoints) -> Self {
        Self { resolver, endpoints, slots: Mutex::default() }
    }

    /// Providers of [`USAGE_PROVIDERS`] with a stored login.
    pub async fn logged_in(&self) -> Vec<String> {
        let mut providers = Vec::new();
        for provider in USAGE_PROVIDERS {
            if let Ok(Some(Credential::OAuth(_))) = self.resolver.store().read(provider).await {
                providers.push((*provider).to_string());
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
        let ttl = if provider == "google-antigravity" { ANTIGRAVITY_TTL } else { TTL };
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
        if !USAGE_PROVIDERS.contains(&provider) {
            return ProviderUsage::failed(provider, format!("pi has no usage report for {provider}"));
        }
        let credential = match self.resolver.store().read(provider).await {
            Ok(Some(Credential::OAuth(credential))) => credential,
            Ok(_) => return ProviderUsage::failed(provider, format!("Not logged in. Run /login {provider}.")),
            Err(error) => return ProviderUsage::failed(provider, error.to_string()),
        };
        if provider == "google-antigravity" {
            return match crate::providers::antigravity_acp::usage().await {
                Ok(windows) => ProviderUsage::ok(provider, None, windows),
                Err(error) => ProviderUsage::failed(provider, error),
            };
        }
        let token = match self.token(provider).await {
            Ok(token) => token,
            Err(error) => return ProviderUsage::failed(provider, error),
        };
        let result = match provider {
            "anthropic" => anthropic::fetch(&self.endpoints.anthropic, &token).await,
            "cursor" => cursor::fetch(&self.endpoints.cursor, &token).await,
            _ => codex::fetch(&self.endpoints.chatgpt, &token, credential.extra_str("accountId"), provider).await,
        };
        match result {
            Ok((plan, windows)) => ProviderUsage::ok(provider, plan, windows),
            Err(error) => ProviderUsage::failed(provider, error),
        }
    }

    /// A valid access token; an expiring one is refreshed under the store lock.
    async fn token(&self, provider: &str) -> Result<String, String> {
        let resolved = self.resolver.resolve(provider, &AuthOverrides::default()).await;
        let resolved = resolved.map_err(|error| format!("Cannot refresh the {provider} login: {error}"))?;
        resolved
            .and_then(|resolved| resolved.auth.api_key)
            .filter(|token| !token.is_empty() && token != ANTIGRAVITY_ACP_MARKER)
            .ok_or_else(|| format!("Not logged in. Run /login {provider}."))
    }
}

pub(crate) type Fetched = Result<(Option<String>, Vec<UsageWindow>), String>;

/// Sends `request` and reads a JSON body; HTTP errors become messages for the user.
pub(crate) async fn json_response(request: reqwest::RequestBuilder, provider: &str) -> Result<Value, String> {
    let failed = |error: reqwest::Error| format!("Usage request failed: {error}");
    let response = request.timeout(REQUEST_TIMEOUT).send().await.map_err(failed)?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(failed)?;
    match status {
        200..=299 if text.trim_start().starts_with('<') => Err("The usage endpoint returned HTML".into()),
        200..=299 => serde_json::from_str(&text).map_err(|error| format!("Invalid usage response: {error}")),
        401 | 403 => Err(format!("The login was rejected (HTTP {status}). Run /login {provider}.")),
        429 => Err("The usage endpoint is rate limited (HTTP 429). Try again later.".into()),
        _ => Err(format!("The usage endpoint returned HTTP {status}")),
    }
}

/// A JSON number, or a number in a string.
pub(crate) fn number(value: &Value) -> Option<f64> {
    let parsed = value.as_f64().or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()));
    parsed.filter(|number: &f64| number.is_finite())
}
