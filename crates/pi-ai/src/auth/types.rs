//! Credential and login contracts shared by credential stores, auth resolution, and OAuth flows.
//! JSON shapes match the TypeScript pi `auth.json`, so both can share one file.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// Provider-scoped environment values, for example Cloudflare account ids.
pub type ProviderEnv = BTreeMap<String, String>;

/// Request auth for one model request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Stored API key. `key` may be a literal, `$VAR`, `${VAR}`, or `!command`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<ProviderEnv>,
}

/// Stored OAuth tokens. `expires` is a Unix time in milliseconds; provider-specific
/// fields (for example Copilot's `enterpriseUrl` or Codex's `accountId`) live in `extra`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    pub expires: i64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl OAuthCredential {
    /// True when the access token expires within `margin_ms` of `now_ms`.
    pub fn expires_within(&self, now_ms: i64, margin_ms: i64) -> bool {
        self.expires <= now_ms + margin_ms
    }

    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }
}

/// One type-tagged credential per provider, the shape of `auth.json` values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

impl Credential {
    pub fn auth_type(&self) -> AuthType {
        match self {
            Self::ApiKey(_) => AuthType::ApiKey,
            Self::OAuth(_) => AuthType::OAuth,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthType {
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

/// Non-secret credential metadata for status listings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub auth_type: AuthType,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("login cancelled")]
    Cancelled,
    #[error("credential storage failed: {0}")]
    Storage(String),
    #[error("{0}")]
    Http(String),
    #[error("{0}")]
    Other(String),
}

impl AuthError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

impl From<reqwest::Error> for AuthError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error.to_string())
    }
}

pub type AuthResult<T> = Result<T, AuthError>;

/// The transform passed to [`CredentialStore::update`]: it sees the current
/// credential and returns the new one, or `None` to leave the entry unchanged.
pub type UpdateFn =
    Box<dyn FnOnce(Option<Credential>) -> BoxFuture<'static, AuthResult<Option<Credential>>> + Send + 'static>;

/// App-owned credential storage keyed by provider id. `update` is the only write
/// path and is serialized per provider (and across processes where the store can lock),
/// so a token refresh cannot race a concurrent refresh or login.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// The stored credential, possibly expired.
    async fn read(&self, provider_id: &str) -> AuthResult<Option<Credential>>;

    /// Stored credential metadata. Must not run `!command` keys.
    async fn list(&self) -> AuthResult<Vec<CredentialInfo>>;

    /// Serialized read, transform, and write. Returns the credential stored afterwards.
    async fn update(&self, provider_id: &str, transform: UpdateFn) -> AuthResult<Option<Credential>>;

    /// Remove a credential (logout).
    async fn delete(&self, provider_id: &str) -> AuthResult<()>;
}

/// Where a resolved credential came from, for status display.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedAuth {
    pub auth: ModelAuth,
    pub env: Option<ProviderEnv>,
    /// Label such as `ANTHROPIC_API_KEY`, `OAuth`, or `~/.aws/credentials`.
    pub source: Option<String>,
}

/// One option of a `select` prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A question shown to the user during login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthPrompt {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<PromptOption>,
    },
    /// Paste of a redirect URL or code, raced against a callback server.
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthInfoLink {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Progress shown to the user during login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthEvent {
    Info {
        message: String,
        #[serde(default)]
        links: Vec<AuthInfoLink>,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    DeviceCode {
        user_code: String,
        verification_uri: String,
        interval_seconds: Option<u64>,
        expires_in_seconds: Option<u64>,
    },
    Progress {
        message: String,
    },
}

/// Login UI callbacks. `prompt` returns the entered text or the selected option id;
/// `cancel` is the per-prompt token a flow fires when another path (a callback) wins.
#[async_trait]
pub trait AuthInteraction: Send + Sync {
    async fn prompt(&self, prompt: AuthPrompt, cancel: CancellationToken) -> AuthResult<String>;
    fn notify(&self, event: AuthEvent);
}

/// App identity passed to login flows that need it.
#[derive(Clone, Default)]
pub struct LoginOptions {
    /// Stable id of this installation; flows that need one call it.
    pub device_id: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    /// Name the app introduces itself with, for example the Codex originator. Defaults to `pi`.
    pub agent_name: Option<String>,
}

/// An OAuth login method. `refresh` produces a new credential and `to_auth`
/// derives request auth from whatever credential ends up stored.
#[async_trait]
pub trait OAuthAuth: Send + Sync {
    /// Display name, for example `Anthropic (Claude Pro/Max)`.
    fn name(&self) -> &str;

    /// Whether access is backed by a provider subscription.
    fn is_subscription(&self) -> bool {
        false
    }

    /// Selector label for the login option.
    fn login_label(&self) -> Option<&str> {
        None
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential>;

    /// Exchange the refresh token. Runs under the credential store lock.
    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential>;

    /// Request auth for a valid credential (may set a per-credential base URL).
    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_auth_json_shapes() {
        let text = r#"{"anthropic":{"type":"oauth","refresh":"r","access":"a","expires":5,"scopes":["x"]},
            "openai":{"type":"api_key","key":"sk"},"cloudflare":{"type":"api_key","env":{"CLOUDFLARE_ACCOUNT_ID":"id"}}}"#;
        let parsed: BTreeMap<String, Credential> = serde_json::from_str(text).unwrap();
        let Credential::OAuth(oauth) = &parsed["anthropic"] else { panic!("expected oauth") };
        assert_eq!((oauth.access.as_str(), oauth.expires), ("a", 5));
        assert!(oauth.extra.contains_key("scopes"));
        assert_eq!(parsed["openai"].auth_type(), AuthType::ApiKey);
        let back = serde_json::to_value(&parsed).unwrap();
        assert_eq!(back["anthropic"]["type"], "oauth");
        assert_eq!(back["anthropic"]["scopes"][0], "x");
        assert_eq!(back["cloudflare"]["env"]["CLOUDFLARE_ACCOUNT_ID"], "id");
    }
}
