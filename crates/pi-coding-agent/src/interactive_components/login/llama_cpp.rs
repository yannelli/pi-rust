//! The llama.cpp server login (upstream `extensions/llama/provider.ts` `auth.apiKey.login`): ask
//! for the server URL and an optional key, check that the server answers in router mode, and
//! return the credential with `LLAMA_BASE_URL`.

use std::time::Duration;

use pi_ai::auth::{ApiKeyCredential, AuthError, AuthInteraction, AuthPrompt, AuthResult, ProviderEnv};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub const LLAMA_CPP_PROVIDER_ID: &str = "llama.cpp";
pub const DEFAULT_LLAMA_SERVER_URL: &str = "http://127.0.0.1:8080";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Upstream `normalizeLlamaServerUrl`: http(s) only, no query or fragment, no trailing slash
/// or `/v1` suffix.
pub fn normalize_llama_server_url(value: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(value.trim()).map_err(|_| "Invalid URL".to_string())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Server URL must use http or https".into());
    }
    url.set_fragment(None);
    url.set_query(None);
    let trimmed = url.path().trim_end_matches('/');
    let path = trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string();
    url.set_path(if path.is_empty() { "/" } else { &path });
    let text = url.to_string();
    Ok(text.strip_suffix('/').unwrap_or(&text).to_string())
}

/// The status text after a llama.cpp login without a selected model.
pub fn llama_cpp_post_login_guidance(action_label: &str, loaded_model_count: usize) -> String {
    if loaded_model_count == 0 {
        format!("{action_label}. No llama.cpp models are loaded. Use /llama to load a model, then /model to select it.")
    } else {
        format!("{action_label}. Use /model to select a loaded llama.cpp model, or /llama to manage models.")
    }
}

fn error_message(payload: &Value, fallback: String) -> String {
    payload["error"]["message"].as_str().filter(|message| !message.is_empty()).map_or(fallback, str::to_string)
}

fn is_model_info(value: &Value) -> bool {
    value["id"].is_string() && value["status"]["value"].is_string()
}

/// Lists the server's models (upstream `LlamaClient.list`) to check the URL and key.
pub async fn check_llama_server(server_url: &str, api_key: Option<&str>) -> Result<usize, String> {
    let client = pi_ai::http::shared_client();
    let mut request = client.get(format!("{server_url}/models")).timeout(REQUEST_TIMEOUT);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let payload: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(error_message(&payload, format!("llama.cpp returned HTTP {}", status.as_u16())));
    }
    let Some(data) = payload["data"].as_array() else {
        return Err("llama.cpp returned an invalid model catalog".into());
    };
    if !data.iter().all(is_model_info) {
        return Err("Server is not running in llama.cpp router mode".into());
    }
    Ok(data.len())
}

/// Prompts for the server URL (default `$LLAMA_BASE_URL` or [`DEFAULT_LLAMA_SERVER_URL`]) and an
/// optional API key, then checks the server.
pub async fn llama_cpp_login(
    interaction: &dyn AuthInteraction,
    cancel: CancellationToken,
) -> AuthResult<ApiKeyCredential> {
    let env_url = std::env::var("LLAMA_BASE_URL").ok().filter(|url| !url.is_empty());
    let placeholder = env_url.clone().unwrap_or_else(|| DEFAULT_LLAMA_SERVER_URL.to_string());
    let prompt = AuthPrompt::Text { message: "llama.cpp server URL".into(), placeholder: Some(placeholder) };
    let entered = interaction.prompt(prompt, cancel.clone()).await?;
    let entered = entered.trim();
    let raw = if entered.is_empty() { env_url.as_deref().unwrap_or(DEFAULT_LLAMA_SERVER_URL) } else { entered };
    let server_url = normalize_llama_server_url(raw).map_err(AuthError::other)?;
    let prompt = AuthPrompt::Secret { message: "API key (optional)".into(), placeholder: None };
    let api_key = interaction.prompt(prompt, cancel.clone()).await?.trim().to_string();
    let api_key = (!api_key.is_empty()).then_some(api_key);
    tokio::select! {
        result = check_llama_server(&server_url, api_key.as_deref()) => result.map_err(AuthError::other)?,
        () = cancel.cancelled() => return Err(AuthError::Cancelled),
    };
    let env: ProviderEnv = [("LLAMA_BASE_URL".to_string(), server_url)].into_iter().collect();
    Ok(ApiKeyCredential { key: api_key, env: Some(env) })
}
