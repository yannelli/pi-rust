//! Workload identity federation for the `anthropic` provider: upstream hands the Anthropic SDK an
//! `oidc_federation` config from `ANTHROPIC_*` variables, and the SDK exchanges the identity token
//! (`oidcFederationProvider`) and caches the access token (`TokenCache`). This ports both.

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};

use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::http::pi_user_agent;
use crate::providers::common::endpoint;
use crate::request::{HttpRequest, SendOptions, send_request};
use crate::simple_options::provider_env_value;

const RULE_ID_ENV: &str = "ANTHROPIC_FEDERATION_RULE_ID";
const ORGANIZATION_ID_ENV: &str = "ANTHROPIC_ORGANIZATION_ID";
const SERVICE_ACCOUNT_ID_ENV: &str = "ANTHROPIC_SERVICE_ACCOUNT_ID";
const IDENTITY_TOKEN_FILE_ENV: &str = "ANTHROPIC_IDENTITY_TOKEN_FILE";
const WORKSPACE_ID_ENV: &str = "ANTHROPIC_WORKSPACE_ID";
const TOKEN_ENDPOINT: &str = "/v1/oauth/token";
const GRANT_TYPE_JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// `anthropic-beta` value that API requests with an OAuth bearer token carry.
pub(super) const OAUTH_API_BETA: &str = "oauth-2025-04-20";
const FEDERATION_BETA: &str = "oidc-federation-2026-04-01";
const ADVISORY_REFRESH_SECONDS: i64 = 120;
const MANDATORY_REFRESH_SECONDS: i64 = 30;
const ADVISORY_BACKOFF_SECONDS: i64 = 5;
const MAX_ASSERTION_BYTES: usize = 16 * 1024;
const MAX_ERROR_BODY_CHARS: usize = 2000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct FederationConfig {
    pub organization_id: String,
    pub workspace_id: Option<String>,
    pub federation_rule_id: String,
    pub service_account_id: Option<String>,
    pub identity_token_file: String,
}

/// The federation config when the rule, organization, and identity token file are all set.
pub(super) fn federation_config(env: &BTreeMap<String, String>) -> Option<FederationConfig> {
    Some(FederationConfig {
        federation_rule_id: provider_env_value(RULE_ID_ENV, env)?,
        organization_id: provider_env_value(ORGANIZATION_ID_ENV, env)?,
        identity_token_file: provider_env_value(IDENTITY_TOKEN_FILE_ENV, env)?,
        workspace_id: provider_env_value(WORKSPACE_ID_ENV, env),
        service_account_id: provider_env_value(SERVICE_ACCOUNT_ID_ENV, env),
    })
}

#[derive(Clone, Default)]
struct CachedToken {
    token: Option<(String, i64)>,
    refreshing: bool,
    last_advisory_error: i64,
}

static TOKENS: LazyLock<Mutex<HashMap<String, CachedToken>>> = LazyLock::new(Mutex::default);

fn now_seconds() -> i64 {
    crate::types::now_ms() / 1000
}

fn cache_key(base_url: &str, config: &FederationConfig) -> String {
    serde_json::to_string(&(base_url, config)).unwrap_or_default()
}

/// Drop the cached token, after the API rejected it.
pub(super) fn invalidate(base_url: &str, config: &FederationConfig) {
    TOKENS.lock().unwrap().remove(&cache_key(base_url, config));
}

/// A bearer token: cached while more than 120s remain, refreshed in the background within
/// 120s, and exchanged before use within 30s of expiry.
pub(super) async fn access_token(
    base_url: &str,
    config: &FederationConfig,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let key = cache_key(base_url, config);
    let now = now_seconds();
    let stale = {
        let mut tokens = TOKENS.lock().unwrap();
        let entry = tokens.entry(key.clone()).or_default();
        match entry.token.clone() {
            Some((token, expires_at)) if expires_at - now > ADVISORY_REFRESH_SECONDS => return Ok(token),
            Some((token, expires_at)) if expires_at - now > MANDATORY_REFRESH_SECONDS => {
                let backoff = now - entry.last_advisory_error < ADVISORY_BACKOFF_SECONDS;
                if !entry.refreshing && !backoff {
                    entry.refreshing = true;
                    let (base_url, config, env) = (base_url.to_string(), config.clone(), env.clone());
                    tokio::spawn(async move {
                        let result = exchange(&base_url, &config, &env, &CancellationToken::new()).await;
                        let mut tokens = TOKENS.lock().unwrap();
                        let entry = tokens.entry(cache_key(&base_url, &config)).or_default();
                        entry.refreshing = false;
                        match result {
                            Ok(token) => entry.token = Some(token),
                            Err(_) => entry.last_advisory_error = now_seconds(),
                        }
                    });
                }
                Some(token)
            }
            _ => None,
        }
    };
    if let Some(token) = stale {
        return Ok(token);
    }
    let (token, expires_at) = exchange(base_url, config, env, cancel).await?;
    TOKENS.lock().unwrap().entry(key).or_default().token = Some((token.clone(), expires_at));
    Ok(token)
}

/// Reject token endpoints that would send the assertion over cleartext HTTP, except loopback.
fn require_secure_token_endpoint(base_url: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(base_url)
        .map_err(|error| format!("Invalid token endpoint base URL \"{base_url}\": {error}"))?;
    let host = url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_lowercase();
    match url.scheme() {
        "https" => Ok(()),
        "http" if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") => Ok(()),
        _ => Err(format!("Refusing to send credential over non-https token endpoint \"{base_url}\"")),
    }
}

/// A token endpoint body that is safe to show: only RFC 6749 error fields of JSON objects,
/// and other text truncated.
fn redact_sensitive(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(parsed) => redact_value(&parsed).to_string(),
        Err(_) if body.chars().count() <= MAX_ERROR_BODY_CHARS => body.to_string(),
        Err(_) => {
            let total = body.chars().count();
            let kept: String = body.chars().take(MAX_ERROR_BODY_CHARS).collect();
            format!("{kept}... <{} more chars>", total - MAX_ERROR_BODY_CHARS)
        }
    }
}

fn redact_value(value: &Value) -> Value {
    let Some(object) = value.as_object() else { return Value::Null };
    let safe = ["error", "error_description", "error_uri"];
    Value::Object(
        object.iter().filter(|(key, _)| safe.contains(&key.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect(),
    )
}

async fn exchange(
    base_url: &str,
    config: &FederationConfig,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<(String, i64), String> {
    require_secure_token_endpoint(base_url)?;
    let path = &config.identity_token_file;
    let content = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| format!("Failed to read identity token file at {path}: {error}"))?;
    let assertion = content.trim();
    if assertion.is_empty() {
        return Err(format!("Identity token file at {path} is empty"));
    }
    if assertion.len() > MAX_ASSERTION_BYTES {
        let kib = assertion.len().div_ceil(1024);
        return Err(format!("Identity token is {kib} KiB, exceeds the 16 KiB assertion limit"));
    }
    let mut body = Map::new();
    body.insert("grant_type".into(), json!(GRANT_TYPE_JWT_BEARER));
    body.insert("assertion".into(), json!(assertion));
    body.insert("federation_rule_id".into(), json!(config.federation_rule_id));
    body.insert("organization_id".into(), json!(config.organization_id));
    if let Some(service_account) = &config.service_account_id {
        body.insert("service_account_id".into(), json!(service_account));
    }
    if let Some(workspace) = &config.workspace_id {
        body.insert("workspace_id".into(), json!(workspace));
    }
    let url = endpoint(base_url, TOKEN_ENDPOINT);
    let mut headers = reqwest::header::HeaderMap::new();
    let beta = format!("{OAUTH_API_BETA},{FEDERATION_BETA}");
    for (name, value) in [("anthropic-beta", beta), ("user-agent", pi_user_agent())] {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    let request = HttpRequest { url: url.clone(), headers, body: Value::Object(body) };
    let send = SendOptions {
        env,
        cancel,
        timeout_ms: None,
        max_retries: 0,
        max_retry_delay_ms: None,
        no_retry_statuses: &[],
        label: Some("Token exchange"),
    };
    let response = match send_request(&request, &send).await {
        Ok(response) => response,
        Err(error) if error.aborted => return Err("Request was aborted".into()),
        Err(error) => return Err(exchange_error(&url, config, error)),
    };
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|error| format!("Failed to reach token endpoint {url}: {error}"))?;
    let data: Value = serde_json::from_str(&text)
        .map_err(|_| format!("Token endpoint returned non-JSON response (status {status})"))?;
    let Some(token) = data["access_token"].as_str().filter(|token| !token.is_empty()) else {
        return Err(format!("Token endpoint response missing access_token: {}", redact_value(&data)));
    };
    if let Some(kind) =
        data["token_type"].as_str().filter(|kind| !kind.is_empty() && !kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(format!("Token endpoint response: unsupported token_type \"{kind}\" (want Bearer)"));
    }
    let Some(expires_in) = data["expires_in"].as_f64().or_else(|| data["expires_in"].as_str()?.parse().ok()) else {
        return Err(format!("Token endpoint response missing required fields: {}", redact_value(&data)));
    };
    Ok((token.to_string(), now_seconds() + expires_in as i64))
}

fn exchange_error(url: &str, config: &FederationConfig, error: crate::retry::ProviderRequestError) -> String {
    let Some(status) = error.status else {
        return format!("Failed to reach token endpoint {url}: {}", error.message);
    };
    let request_id = error.headers.get("request-id").map(|id| format!(" (request-id {id})")).unwrap_or_default();
    let redacted = redact_sensitive(error.body.as_deref().unwrap_or_default());
    let hint = if status == 401 {
        let workspace = if config.workspace_id.is_some() {
            ""
        } else {
            "If your federation rule is scoped to multiple workspaces, set the ANTHROPIC_WORKSPACE_ID environment variable, the 'workspace_id' config key, or the `workspaceId` option. "
        };
        format!(
            " Ensure your federation rule matches your identity token. {workspace}View your authentication events in the Workload identity page of Claude Console for more details."
        )
    } else {
        String::new()
    };
    format!("Token exchange failed with status {status}{request_id}: {redacted}{hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_token_endpoint_bodies_and_rejects_cleartext_endpoints() {
        let body = r#"{"error":"invalid_grant","assertion":"secret","error_description":"bad"}"#;
        assert_eq!(redact_sensitive(body), r#"{"error":"invalid_grant","error_description":"bad"}"#);
        assert_eq!(redact_sensitive("[1]"), "null");
        let long = "x".repeat(2005);
        assert!(redact_sensitive(&long).ends_with("... <5 more chars>"));
        assert!(require_secure_token_endpoint("https://api.anthropic.com").is_ok());
        assert!(require_secure_token_endpoint("http://[::1]:8080").is_ok());
        let error = require_secure_token_endpoint("http://example.com").unwrap_err();
        assert_eq!(error, "Refusing to send credential over non-https token endpoint \"http://example.com\"");
    }

    #[test]
    fn config_needs_rule_organization_and_token_file() {
        let mut env: BTreeMap<String, String> = [
            (RULE_ID_ENV, "fdrl_test"),
            (ORGANIZATION_ID_ENV, "org-test"),
            (IDENTITY_TOKEN_FILE_ENV, "/tmp/identity.jwt"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        let config = federation_config(&env).unwrap();
        assert_eq!((config.service_account_id, config.workspace_id), (None, None));
        env.insert(IDENTITY_TOKEN_FILE_ENV.into(), String::new());
        if std::env::var(IDENTITY_TOKEN_FILE_ENV).is_err() {
            assert!(federation_config(&env).is_none());
        }
    }
}
