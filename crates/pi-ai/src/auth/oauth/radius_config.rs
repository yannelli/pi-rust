//! Radius gateway catalog: the `/v1/config` model list and the legacy copy that older
//! credentials kept in `gatewayConfig`.

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::http::{Hooks, send};
use crate::auth::{AuthError, AuthResult, OAuthCredential};
use crate::types::Model;

pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

/// A gateway's base URL and the models it serves, kept as raw JSON objects.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RadiusGatewayConfig {
    pub base_url: String,
    pub models: Vec<Map<String, Value>>,
}

/// Adds `https://` when no http(s) scheme is present and drops trailing slashes.
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

fn is_gateway_model(value: &Value) -> bool {
    let Some(model) = value.as_object() else { return false };
    let is = |key: &str, check: fn(&Value) -> bool| model.get(key).is_some_and(check);
    is("id", Value::is_string)
        && is("name", Value::is_string)
        && is("reasoning", Value::is_boolean)
        && is("input", Value::is_array)
        && is("cost", Value::is_object)
        && is("contextWindow", Value::is_number)
        && is("maxTokens", Value::is_number)
}

fn sanitize_config(config: &Value) -> Option<RadiusGatewayConfig> {
    let config = config.as_object()?;
    let base_url = config.get("baseUrl")?.as_str()?.to_string();
    let models = config.get("models")?.as_array()?;
    let models = models.iter().filter(|model| is_gateway_model(model)).filter_map(|model| model.as_object().cloned());
    Some(RadiusGatewayConfig { base_url, models: models.collect() })
}

/// The catalog a pre-models-store Radius login saved in the credential.
pub fn radius_credential_config(credential: &OAuthCredential) -> Option<RadiusGatewayConfig> {
    sanitize_config(credential.extra.get("gatewayConfig")?)
}

/// Models of a gateway catalog with `api`, `provider`, and `baseUrl` filled in.
pub fn radius_models_from_config(provider_id: &str, config: &RadiusGatewayConfig) -> Vec<Model> {
    config
        .models
        .iter()
        .filter_map(|model| {
            let mut model = model.clone();
            model.insert("api".into(), Value::String("pi-messages".into()));
            model.insert("provider".into(), Value::String(provider_id.into()));
            model.insert("baseUrl".into(), Value::String(config.base_url.clone()));
            serde_json::from_value(Value::Object(model)).ok()
        })
        .collect()
}

/// Models from a credential's legacy catalog; empty when it has none.
pub fn radius_models(provider_id: &str, credential: Option<&OAuthCredential>) -> Vec<Model> {
    credential
        .and_then(radius_credential_config)
        .map(|config| radius_models_from_config(provider_id, &config))
        .unwrap_or_default()
}

fn truncate_http_body(body: &str) -> String {
    let trimmed = body.trim();
    // JavaScript slices UTF-16 code units; counting chars is the closest stable equivalent.
    match trimmed.char_indices().nth(512) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_string(),
    }
}

/// Fetches the gateway's `/v1/config` catalog, authenticated when `api_key` is given.
pub async fn load_radius_gateway_config(
    gateway: &str,
    api_key: Option<&str>,
    cancel: &CancellationToken,
) -> AuthResult<RadiusGatewayConfig> {
    load_config(&Hooks::default(), gateway, api_key, cancel).await
}

pub(crate) async fn load_config(
    hooks: &Hooks,
    gateway: &str,
    api_key: Option<&str>,
    cancel: &CancellationToken,
) -> AuthResult<RadiusGatewayConfig> {
    let url = gateway_url(gateway, "/v1/config")?;
    let mut request = hooks.get(&url).header("accept", "application/json");
    if let Some(key) = api_key.filter(|key| !key.is_empty()) {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let reply = send(request, cancel, None).await?;
    if !reply.ok() {
        return Err(AuthError::other(format!(
            "Could not load Radius config from {gateway}: {}: {}",
            reply.status,
            truncate_http_body(&reply.body)
        )));
    }
    let json = reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))?;
    sanitize_config(&json).ok_or_else(|| AuthError::other(format!("Invalid Radius config from {gateway}")))
}

/// `new URL(path, gateway)`: an absolute path replaces any path on the gateway URL.
pub(crate) fn gateway_url(gateway: &str, path: &str) -> AuthResult<String> {
    let base =
        reqwest::Url::parse(gateway).map_err(|error| AuthError::other(format!("Invalid URL {gateway}: {error}")))?;
    let url = base.join(path).map_err(|error| AuthError::other(format!("Invalid URL {gateway}: {error}")))?;
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_urls_normalize_like_upstream() {
        assert_eq!(normalize_radius_gateway_url("radius.example.com/"), "https://radius.example.com");
        assert_eq!(normalize_radius_gateway_url("HTTP://localhost:8080//"), "HTTP://localhost:8080");
        assert_eq!(
            gateway_url("https://radius.example.com/prefix", "/v1/oauth").unwrap(),
            "https://radius.example.com/v1/oauth"
        );
    }

    #[test]
    fn credential_catalogs_keep_valid_models_only() {
        let mut credential = OAuthCredential::default();
        let model = serde_json::json!({"id": "m", "name": "M", "reasoning": false, "input": ["text"],
            "cost": {"input": 1, "output": 2}, "contextWindow": 1000, "maxTokens": 100, "extra": true});
        let config = serde_json::json!({"baseUrl": "https://gw/v1", "models": [model, {"id": "broken"}]});
        credential.extra.insert("gatewayConfig".into(), config);
        let models = radius_models("radius", Some(&credential));
        assert_eq!(models.len(), 1);
        assert_eq!((models[0].api.as_str(), models[0].provider.as_str()), ("pi-messages", "radius"));
        assert_eq!(models[0].base_url, "https://gw/v1");
        assert!(radius_models("radius", None).is_empty());
    }
}
