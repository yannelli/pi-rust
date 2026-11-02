use serde_json::Value;

use super::http::Hooks;
use crate::auth::OAuthCredential;

const GLOBAL_OAUTH_HOST: &str = "https://auth.kimi.ai";
const MAINLAND_OAUTH_HOST: &str = "https://auth.kimi.com";

pub(super) struct Endpoints {
    pub oauth_host: String,
    pub base_url: Option<String>,
}

impl Endpoints {
    pub fn login(hooks: &Hooks) -> Self {
        Self::resolve(hooks, None)
    }

    pub fn stored(hooks: &Hooks, credential: &OAuthCredential) -> Self {
        Self::resolve(hooks, Some(credential))
    }

    fn resolve(hooks: &Hooks, credential: Option<&OAuthCredential>) -> Self {
        let saved = |key| credential.and_then(|credential| credential.extra_str(key)).map(str::to_string);
        // Credentials without endpoint metadata were issued by the original mainland flow.
        let fallback = if credential.is_some() { MAINLAND_OAUTH_HOST } else { GLOBAL_OAUTH_HOST };
        let oauth_host = hooks
            .env("KIMI_CODE_OAUTH_HOST")
            .or_else(|| hooks.env("KIMI_OAUTH_HOST"))
            .or_else(|| saved("oauthHost"))
            .unwrap_or_else(|| fallback.into())
            .trim_end_matches('/')
            .to_string();
        let default_base = match oauth_host.as_str() {
            MAINLAND_OAUTH_HOST => Some("https://api.kimi.com/coding/v1"),
            GLOBAL_OAUTH_HOST => Some("https://api.kimi.ai/coding/v1"),
            _ => None,
        };
        let base_url = hooks
            .env("KIMI_CODE_BASE_URL")
            .or_else(|| saved("baseUrl"))
            .or_else(|| default_base.map(str::to_string))
            .map(|url| url.trim_end_matches('/').to_string());
        Self { oauth_host, base_url }
    }

    pub fn remember(&self, mut credential: OAuthCredential) -> OAuthCredential {
        credential.extra.insert("oauthHost".into(), Value::String(self.oauth_host.clone()));
        if let Some(base_url) = &self.base_url {
            credential.extra.insert("baseUrl".into(), Value::String(base_url.clone()));
        }
        credential
    }
}
