//! Credential resolution (upstream `ai/src/auth/resolve.ts` and the API-key composition in
//! `coding-agent/src/core/provider-composer.ts`). Priority: a per-run key (`--api-key`),
//! the stored `auth.json` credential, the `models.json` `apiKey`, then the environment.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::FutureExt;
use tokio_util::sync::CancellationToken;

use super::resolve_value::{
    config_value_env_var_names, is_command_config_value, resolve_config_value_or_err, resolve_headers_or_err,
};
use super::{
    ApiKeyCredential, AuthError, AuthResult, AuthType, Credential, CredentialStore, ModelAuth, OAuthAuth,
    OAuthCredential, ProviderEnv, ResolvedAuth,
};
use crate::env::{self, EnvLookup, ProcessEnv, api_key_env_vars, env_value_in};

/// Finds the OAuth login method for a provider id, for example `builtin_oauth`.
pub type OAuthLookup = Arc<dyn Fn(&str) -> Option<Arc<dyn OAuthAuth>> + Send + Sync>;

/// Tokens closer than this to expiry are refreshed before use.
pub const DEFAULT_OAUTH_MIN_VALIDITY_MS: i64 = 5 * 60 * 1000;
const OAUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

/// Auth settings a provider has in `models.json`. Values may use `$VAR` and `!command`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderAuthConfig {
    pub api_key: Option<String>,
    pub headers: BTreeMap<String, String>,
    /// Send the resolved key as `Authorization: Bearer <key>`.
    pub auth_header: bool,
    /// Per-model headers from `models` entries and `modelOverrides`, by model id.
    pub model_headers: BTreeMap<String, BTreeMap<String, String>>,
}

/// Per-request overrides.
#[derive(Clone, Debug, Default)]
pub struct AuthOverrides {
    pub api_key: Option<String>,
    pub env: Option<ProviderEnv>,
    /// Required remaining OAuth validity; a refreshed token below it is an error.
    pub min_oauth_validity_ms: Option<i64>,
}

/// Whether a provider is configured, without side effects beyond reading `auth.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthCheck {
    pub auth_type: AuthType,
    pub source: String,
}

/// Resolves request auth for providers from a credential store, `models.json`, and the environment.
pub struct AuthResolver {
    store: Arc<dyn CredentialStore>,
    oauth: OAuthLookup,
    configs: RwLock<HashMap<String, ProviderAuthConfig>>,
    env: Arc<dyn EnvLookup>,
}

fn has_builtin_api_key_auth(provider_id: &str) -> bool {
    !api_key_env_vars(provider_id).is_empty() || matches!(provider_id, "amazon-bedrock")
}

fn stored(key: String, env: Option<ProviderEnv>) -> ResolvedAuth {
    ResolvedAuth {
        auth: ModelAuth { api_key: Some(key), ..ModelAuth::default() },
        env,
        source: Some("stored credential".into()),
    }
}

/// Built-in API-key resolution: a stored key wins, then the provider's environment variables.
fn builtin_resolve(
    provider_id: &str,
    credential: Option<&ApiKeyCredential>,
    env: Option<&ProviderEnv>,
    base: &dyn EnvLookup,
) -> Option<ResolvedAuth> {
    if let Some(key) = credential.and_then(|credential| credential.key.clone()) {
        return Some(stored(key, credential.and_then(|credential| credential.env.clone())));
    }
    if provider_id == "anthropic" {
        return anthropic_env_auth(env, base);
    }
    for name in api_key_env_vars(provider_id) {
        if let Some(value) = env_value_in(name, env, base) {
            let auth = ModelAuth { api_key: Some(value), ..ModelAuth::default() };
            return Some(ResolvedAuth { auth, env: None, source: Some(name.to_string()) });
        }
    }
    env::get_env_api_key_in(provider_id, env, base).map(|value| ResolvedAuth {
        auth: ModelAuth { api_key: Some(value), ..ModelAuth::default() },
        env: None,
        source: Some("ambient credentials".into()),
    })
}

/// `ANTHROPIC_AUTH_TOKEN` as a bearer header, then OAuth token and API key variables, then
/// workload identity federation ids passed on as provider env.
fn anthropic_env_auth(env: Option<&ProviderEnv>, base: &dyn EnvLookup) -> Option<ResolvedAuth> {
    let value = |name: &str| env_value_in(name, env, base);
    if let Some(token) = value(env::ANTHROPIC_AUTH_TOKEN_ENV) {
        let mut auth = ModelAuth::default();
        auth.headers.insert("Authorization".into(), format!("Bearer {token}"));
        return Some(ResolvedAuth { auth, env: None, source: Some(env::ANTHROPIC_AUTH_TOKEN_ENV.into()) });
    }
    for name in [env::ANTHROPIC_OAUTH_TOKEN_ENV, env::ANTHROPIC_API_KEY_ENV] {
        if let Some(value) = value(name) {
            let auth = ModelAuth { api_key: Some(value), ..ModelAuth::default() };
            return Some(ResolvedAuth { auth, env: None, source: Some(name.into()) });
        }
    }
    let mut federation = ProviderEnv::new();
    for name in [
        env::ANTHROPIC_FEDERATION_RULE_ID_ENV,
        env::ANTHROPIC_ORGANIZATION_ID_ENV,
        env::ANTHROPIC_IDENTITY_TOKEN_FILE_ENV,
    ] {
        federation.insert(name.into(), value(name)?);
    }
    for name in [env::ANTHROPIC_SERVICE_ACCOUNT_ID_ENV, env::ANTHROPIC_WORKSPACE_ID_ENV] {
        if let Some(value) = value(name) {
            federation.insert(name.into(), value);
        }
    }
    Some(ResolvedAuth {
        auth: ModelAuth::default(),
        env: Some(federation),
        source: Some("workload identity federation".into()),
    })
}

fn merged_env(base: Option<&ProviderEnv>, extra: Option<&ProviderEnv>) -> Option<ProviderEnv> {
    match (base, extra) {
        (None, None) => None,
        _ => Some(base.into_iter().chain(extra).flat_map(|env| env.clone()).collect()),
    }
}

fn now_ms() -> i64 {
    crate::types::now_ms()
}

impl AuthResolver {
    pub fn new(store: Arc<dyn CredentialStore>, oauth: OAuthLookup) -> Self {
        Self { store, oauth, configs: RwLock::default(), env: Arc::new(ProcessEnv) }
    }

    /// Read environment variables (provider keys, ambient credentials, `$VAR` values) from `env`
    /// in place of the process environment.
    pub fn with_env(self, env: Arc<dyn EnvLookup>) -> Self {
        Self { env, ..self }
    }

    /// The environment credential lookups read.
    pub fn env(&self) -> &Arc<dyn EnvLookup> {
        &self.env
    }

    /// A resolver without OAuth methods.
    pub fn without_oauth(store: Arc<dyn CredentialStore>) -> Self {
        Self::new(store, Arc::new(|_| None))
    }

    pub fn store(&self) -> &Arc<dyn CredentialStore> {
        &self.store
    }

    pub fn oauth(&self, provider_id: &str) -> Option<Arc<dyn OAuthAuth>> {
        (self.oauth)(provider_id)
    }

    /// Replace the `models.json` auth settings, for example after a reload.
    pub fn set_provider_configs(&self, configs: HashMap<String, ProviderAuthConfig>) {
        *self.configs.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = configs;
    }

    fn config(&self, provider_id: &str) -> ProviderAuthConfig {
        self.configs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(provider_id)
            .cloned()
            .unwrap_or_default()
    }

    fn has_api_key_auth(&self, provider_id: &str, config: &ProviderAuthConfig) -> bool {
        has_builtin_api_key_auth(provider_id) || config.api_key.is_some() || self.oauth(provider_id).is_none()
    }

    /// Request auth for `provider_id`, refreshing an expiring OAuth token under the store lock.
    pub async fn resolve(&self, provider_id: &str, overrides: &AuthOverrides) -> AuthResult<Option<ResolvedAuth>> {
        let config = self.config(provider_id);
        let api_key_auth = self.has_api_key_auth(provider_id, &config);
        if let Some(key) = overrides.api_key.clone().filter(|_| api_key_auth) {
            let credential = ApiKeyCredential { key: Some(key), env: overrides.env.clone() };
            return self.resolve_api_key(provider_id, &config, Some(&credential), overrides.env.as_ref());
        }
        let stored = self.store.read(provider_id).await?;
        match stored {
            Some(Credential::OAuth(credential)) => match self.oauth(provider_id) {
                Some(oauth) => self.resolve_stored_oauth(provider_id, &config, oauth, credential, overrides).await,
                None => Ok(None),
            },
            Some(Credential::ApiKey(mut credential)) if api_key_auth => {
                credential.env = merged_env(credential.env.as_ref(), overrides.env.as_ref());
                self.resolve_api_key(provider_id, &config, Some(&credential), overrides.env.as_ref())
            }
            Some(_) => Ok(None),
            None if api_key_auth => self.resolve_api_key(provider_id, &config, None, overrides.env.as_ref()),
            None => Ok(None),
        }
    }

    /// Like [`Self::resolve`] for one model: adds its `models.json` headers.
    pub async fn resolve_for_model(
        &self,
        provider_id: &str,
        model_id: &str,
        overrides: &AuthOverrides,
    ) -> AuthResult<Option<ResolvedAuth>> {
        let Some(mut resolved) = self.resolve(provider_id, overrides).await? else { return Ok(None) };
        let config = self.config(provider_id);
        if let Some(headers) = config.model_headers.get(model_id) {
            let env = merged_env(resolved.env.as_ref(), overrides.env.as_ref());
            let description = format!("model \"{provider_id}/{model_id}\"");
            let headers = resolve_headers_or_err(headers, &description, env.as_ref(), &*self.env);
            resolved.auth.headers.extend(headers.map_err(AuthError::Other)?);
        }
        Ok(Some(resolved))
    }

    fn resolve_api_key(
        &self,
        provider_id: &str,
        config: &ProviderAuthConfig,
        credential: Option<&ApiKeyCredential>,
        env: Option<&ProviderEnv>,
    ) -> AuthResult<Option<ResolvedAuth>> {
        let builtin = has_builtin_api_key_auth(provider_id);
        let base = &*self.env;
        let result = match (credential, &config.api_key) {
            (Some(credential), _) if builtin => builtin_resolve(provider_id, Some(credential), env, base),
            (Some(credential), _) => credential.key.clone().map(|key| stored(key, credential.env.clone())),
            (None, Some(raw)) => {
                let description = format!("API key for provider \"{provider_id}\"");
                let key = resolve_config_value_or_err(raw, &description, env, base).map_err(AuthError::Other)?;
                let mut resolved =
                    builtin_resolve(provider_id, Some(&ApiKeyCredential { key: Some(key), env: None }), env, base);
                if let Some(resolved) = resolved.as_mut() {
                    resolved.source = Some("configured API key".into());
                }
                resolved
            }
            (None, None) if builtin => builtin_resolve(provider_id, None, env, base),
            (None, None) => None,
        };
        let Some(mut result) = result else { return Ok(None) };
        let header_env = merged_env(credential.and_then(|credential| credential.env.as_ref()), result.env.as_ref());
        self.apply_config_headers(provider_id, config, &mut result.auth, header_env.as_ref().or(env))?;
        Ok(Some(result))
    }

    fn apply_config_headers(
        &self,
        provider_id: &str,
        config: &ProviderAuthConfig,
        auth: &mut ModelAuth,
        env: Option<&ProviderEnv>,
    ) -> AuthResult<()> {
        let description = format!("provider \"{provider_id}\"");
        let headers = resolve_headers_or_err(&config.headers, &description, env, &*self.env);
        auth.headers.extend(headers.map_err(AuthError::Other)?);
        if config.auth_header {
            let key = auth.api_key.clone().ok_or_else(|| AuthError::other("authHeader requires a resolved API key"))?;
            auth.headers.insert("Authorization".into(), format!("Bearer {key}"));
        }
        Ok(())
    }

    async fn resolve_stored_oauth(
        &self,
        provider_id: &str,
        config: &ProviderAuthConfig,
        oauth: Arc<dyn OAuthAuth>,
        stored: OAuthCredential,
        overrides: &AuthOverrides,
    ) -> AuthResult<Option<ResolvedAuth>> {
        let minimum = DEFAULT_OAUTH_MIN_VALIDITY_MS.max(overrides.min_oauth_validity_ms.unwrap_or(0));
        let mut credential = stored;
        if credential.expires_within(now_ms(), minimum) {
            let refreshed = refresh_stored_oauth(&self.store, provider_id, oauth.clone(), minimum).await?;
            let Some(refreshed) = refreshed else { return Ok(None) };
            credential = refreshed;
            if overrides.min_oauth_validity_ms.is_some() && credential.expires_within(now_ms(), minimum) {
                return Err(AuthError::other(format!(
                    "OAuth refresh returned a token that expires too soon for {provider_id}"
                )));
            }
        }
        let mut auth = oauth.to_auth(&credential);
        let env: Option<ProviderEnv> =
            credential.extra.get("env").and_then(|env| serde_json::from_value(env.clone()).ok());
        self.apply_config_headers(provider_id, config, &mut auth, env.as_ref())?;
        Ok(Some(ResolvedAuth { auth, env: None, source: Some("OAuth".into()) }))
    }

    /// Whether `provider_id` has usable auth. Does not run `models.json` commands or refresh tokens.
    pub async fn check(&self, provider_id: &str) -> AuthResult<Option<AuthCheck>> {
        let config = self.config(provider_id);
        let credential = self.store.read(provider_id).await?;
        let found = |source: &str| Some(AuthCheck { auth_type: AuthType::ApiKey, source: source.to_string() });
        if let Some(Credential::OAuth(_)) = credential {
            return Ok(self
                .oauth(provider_id)
                .map(|_| AuthCheck { auth_type: AuthType::OAuth, source: "OAuth".into() }));
        }
        if !self.has_api_key_auth(provider_id, &config) {
            return Ok(None);
        }
        let (builtin, base) = (has_builtin_api_key_auth(provider_id), &*self.env);
        if let Some(Credential::ApiKey(credential)) = &credential {
            if credential.key.is_some() {
                return Ok(found("stored credential"));
            }
            let resolved = if builtin { builtin_resolve(provider_id, Some(credential), None, base) } else { None };
            return Ok(resolved.and_then(|resolved| found(resolved.source.as_deref().unwrap_or("environment"))));
        }
        if let Some(raw) = &config.api_key {
            let configured = is_command_config_value(raw)
                || config_value_env_var_names(raw).iter().all(|name| env_value_in(name, None, base).is_some());
            return Ok(if configured { found("configured API key") } else { None });
        }
        let resolved = if builtin { builtin_resolve(provider_id, None, None, base) } else { None };
        Ok(resolved.and_then(|resolved| found(resolved.source.as_deref().unwrap_or("environment"))))
    }
}

/// Refresh a stored OAuth credential under the store lock and persist it before the lock is
/// released. Expiry is checked again under the lock, so concurrent callers refresh once.
/// Returns the stored OAuth credential afterwards, or `None` after a logout.
pub async fn refresh_stored_oauth(
    store: &Arc<dyn CredentialStore>,
    provider_id: &str,
    oauth: Arc<dyn OAuthAuth>,
    min_validity_ms: i64,
) -> AuthResult<Option<OAuthCredential>> {
    let provider = provider_id.to_string();
    let post = store
        .update(
            provider_id,
            Box::new(move |current| {
                async move {
                    let Some(Credential::OAuth(current)) = current else { return Ok(None) };
                    if !current.expires_within(now_ms(), min_validity_ms) {
                        return Ok(None);
                    }
                    let cancel = CancellationToken::new();
                    let refresh = oauth.refresh(&current, cancel.clone());
                    match tokio::time::timeout(OAUTH_REFRESH_TIMEOUT, refresh).await {
                        Ok(Ok(credential)) => Ok(Some(Credential::OAuth(credential))),
                        Ok(Err(error)) => {
                            Err(AuthError::other(format!("OAuth refresh failed for {provider}: {error}")))
                        }
                        Err(_) => {
                            cancel.cancel();
                            Err(AuthError::other(format!("OAuth refresh failed for {provider}: timed out")))
                        }
                    }
                }
                .boxed()
            }),
        )
        .await?;
    Ok(match post {
        Some(Credential::OAuth(credential)) => Some(credential),
        _ => None,
    })
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
