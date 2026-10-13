//! llama.cpp provider auth: the router URL and optional key come from a stored credential
//! (`/login llama.cpp`) or `LLAMA_BASE_URL` and `LLAMA_API_KEY`.

use pi_ai::auth::{
    ApiKeyCredential, AuthCheck, AuthError, AuthInteraction, AuthPrompt, AuthResult, AuthType, ModelAuth, ProviderEnv,
    ResolvedAuth,
};
use tokio_util::sync::CancellationToken;

use super::client::{LlamaClient, llama_inference_url, normalize_llama_server_url};
use super::provider::DEFAULT_LLAMA_SERVER_URL;

pub const ENV_LLAMA_BASE_URL: &str = "LLAMA_BASE_URL";
pub const ENV_LLAMA_API_KEY: &str = "LLAMA_API_KEY";
const STORED_SOURCE: &str = "stored credential";

/// The normalized `LLAMA_BASE_URL` a stored credential carries.
pub fn credential_server_url(credential: Option<&ApiKeyCredential>) -> Result<Option<String>, String> {
    let url = credential.and_then(|credential| credential.env.as_ref()?.get(ENV_LLAMA_BASE_URL));
    match url.map(|url| url.trim()).filter(|url| !url.is_empty()) {
        Some(url) => normalize_llama_server_url(url).map(Some),
        None => Ok(None),
    }
}

/// The credential's router URL, else `LLAMA_BASE_URL`.
pub fn resolve_server_url(
    credential: Option<&ApiKeyCredential>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    if let Some(url) = credential_server_url(credential)? {
        return Ok(Some(url));
    }
    match env(ENV_LLAMA_BASE_URL).map(|url| url.trim().to_string()).filter(|url| !url.is_empty()) {
        Some(url) => normalize_llama_server_url(&url).map(Some),
        None => Ok(None),
    }
}

fn source(credential: Option<&ApiKeyCredential>) -> String {
    if credential.is_some() { STORED_SOURCE.into() } else { ENV_LLAMA_BASE_URL.into() }
}

/// Configured when a router URL is known; nothing else is checked.
pub fn check_llama_auth(
    credential: Option<&ApiKeyCredential>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<AuthCheck>, String> {
    let configured = resolve_server_url(credential, env)?.is_some();
    Ok(configured.then(|| AuthCheck { auth_type: AuthType::ApiKey, source: source(credential) }))
}

/// Request auth: the inference URL, the key (`LLAMA_API_KEY`, else `local`), and the router URL
/// in the provider environment.
pub fn resolve_llama_auth(
    credential: Option<&ApiKeyCredential>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ResolvedAuth>, String> {
    let Some(server_url) = resolve_server_url(credential, env)? else { return Ok(None) };
    let api_key = credential
        .and_then(|credential| credential.key.clone())
        .or_else(|| env(ENV_LLAMA_API_KEY))
        .unwrap_or_else(|| "local".into());
    let mut provider_env: ProviderEnv = credential.and_then(|credential| credential.env.clone()).unwrap_or_default();
    provider_env.insert(ENV_LLAMA_BASE_URL.into(), server_url.clone());
    Ok(Some(ResolvedAuth {
        auth: ModelAuth {
            api_key: Some(api_key),
            base_url: Some(llama_inference_url(&server_url)?),
            ..ModelAuth::default()
        },
        env: Some(provider_env),
        source: Some(source(credential)),
    }))
}

/// `/login llama.cpp`: ask for the router URL and an optional key, make sure the router
/// answers in router mode, and return the credential to store.
pub async fn login_llama(
    interaction: &dyn AuthInteraction,
    env_base_url: Option<&str>,
    http: Option<reqwest::Client>,
    cancel: &CancellationToken,
) -> AuthResult<ApiKeyCredential> {
    let default_url = env_base_url.filter(|url| !url.is_empty()).unwrap_or(DEFAULT_LLAMA_SERVER_URL);
    let prompt = AuthPrompt::Text { message: "llama.cpp server URL".into(), placeholder: Some(default_url.into()) };
    let entered = interaction.prompt(prompt, cancel.child_token()).await?;
    let entered = entered.trim();
    let server_url =
        normalize_llama_server_url(if entered.is_empty() { default_url } else { entered }).map_err(AuthError::other)?;
    let secret = AuthPrompt::Secret { message: "API key (optional)".into(), placeholder: None };
    let api_key = interaction.prompt(secret, cancel.child_token()).await?.trim().to_string();
    let api_key = (!api_key.is_empty()).then_some(api_key);
    let mut client = LlamaClient::new(&server_url, api_key.clone()).map_err(AuthError::other)?;
    if let Some(http) = http {
        client = client.with_http(http);
    }
    client.list(false, cancel).await.map_err(|error| match error {
        super::client::LlamaError::Cancelled => AuthError::Cancelled,
        other => AuthError::other(other.to_string()),
    })?;
    let env = ProviderEnv::from([(ENV_LLAMA_BASE_URL.to_string(), server_url)]);
    Ok(ApiKeyCredential { key: api_key, env: Some(env) })
}
