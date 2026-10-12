//! Application Default Credentials for Vertex AI, following `google-auth-library`:
//! `GOOGLE_APPLICATION_CREDENTIALS`, the gcloud well-known file, then the metadata server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Value, json};

use crate::registry::{StreamOptions, http_client};
use crate::request::error_chain;
use crate::simple_options::provider_env_value;
use crate::types::now_ms;

pub(crate) const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const NO_ADC_FOUND: &str = "Could not load the default credentials. Browse to https://cloud.google.com/docs/authentication/getting-started for more information.";
/// Tokens are refreshed this long before they expire.
const EAGER_REFRESH_MS: i64 = 5 * 60 * 1000;

/// An OAuth access token and the quota project to bill, if any.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AccessToken {
    pub token: String,
    pub quota_project: Option<String>,
}

static TOKEN_CACHE: LazyLock<Mutex<HashMap<String, (AccessToken, i64)>>> = LazyLock::new(Default::default);

enum Source {
    File(PathBuf),
    Metadata(String),
}

/// An access token from Application Default Credentials, cached until shortly before expiry.
pub(crate) async fn application_default_token(options: &StreamOptions) -> Result<AccessToken, String> {
    let source = discover(options)?;
    let key = match &source {
        Source::File(path) => format!("file:{}", path.display()),
        Source::Metadata(host) => format!("metadata:{host}"),
    };
    let cached = TOKEN_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&key).cloned();
    if let Some((token, expires_at)) = cached
        && expires_at - EAGER_REFRESH_MS > now_ms()
    {
        return Ok(token);
    }
    let (mut token, expires_in) = match &source {
        Source::File(path) => file_token(path, options).await?,
        Source::Metadata(host) => metadata_token(host).await?,
    };
    if let Some(quota) = provider_env_value("GOOGLE_CLOUD_QUOTA_PROJECT", &options.env) {
        token.quota_project = Some(quota);
    }
    let expires_at = now_ms() + expires_in.saturating_mul(1000);
    TOKEN_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(key, (token.clone(), expires_at));
    Ok(token)
}

fn discover(options: &StreamOptions) -> Result<Source, String> {
    let explicit = provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", &options.env)
        .or_else(|| std::env::var("google_application_credentials").ok().filter(|path| !path.is_empty()));
    if let Some(path) = explicit {
        return Ok(Source::File(PathBuf::from(path)));
    }
    if let Some(path) = well_known_file(options).filter(|path| path.is_file()) {
        return Ok(Source::File(path));
    }
    if on_google_cloud(options) {
        let host = provider_env_value("GCE_METADATA_HOST", &options.env)
            .or_else(|| provider_env_value("GCE_METADATA_IP", &options.env))
            .unwrap_or_else(|| "metadata.google.internal".into());
        return Ok(Source::Metadata(host));
    }
    Err(NO_ADC_FOUND.into())
}

/// gcloud's `application_default_credentials.json`: under `%APPDATA%\gcloud` on Windows,
/// `$HOME/.config/gcloud` elsewhere.
pub(crate) fn well_known_file(options: &StreamOptions) -> Option<PathBuf> {
    let root = if cfg!(windows) {
        PathBuf::from(provider_env_value("APPDATA", &options.env)?)
    } else {
        PathBuf::from(provider_env_value("HOME", &options.env)?).join(".config")
    };
    Some(root.join("gcloud").join("application_default_credentials.json"))
}

/// Cloud Run, Cloud Functions, or a Compute Engine BIOS, as `gcp-metadata` detects them.
fn on_google_cloud(options: &StreamOptions) -> bool {
    match provider_env_value("METADATA_SERVER_DETECTION", &options.env).as_deref() {
        Some("assume-present") => return true,
        Some("none") => return false,
        _ => {}
    }
    if provider_env_value("GCE_METADATA_HOST", &options.env).is_some() {
        return true;
    }
    let serverless = ["CLOUD_RUN_JOB", "FUNCTION_NAME", "K_SERVICE"];
    if serverless.iter().any(|name| provider_env_value(name, &options.env).is_some()) {
        return true;
    }
    cfg!(target_os = "linux")
        && std::path::Path::new("/sys/class/dmi/id/bios_date").exists()
        && std::fs::read_to_string("/sys/class/dmi/id/bios_vendor").is_ok_and(|vendor| vendor.contains("Google"))
}

async fn file_token(path: &PathBuf, options: &StreamOptions) -> Result<(AccessToken, i64), String> {
    let read_error = |error: String| {
        let explicit = provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", &options.env).is_some();
        if explicit {
            format!(
                "Unable to read the credential file specified by the GOOGLE_APPLICATION_CREDENTIALS environment variable: {error}"
            )
        } else {
            error
        }
    };
    let text =
        tokio::fs::read_to_string(path).await.map_err(|error| read_error(format!("{}: {error}", path.display())))?;
    let credentials: Value = serde_json::from_str(&text).map_err(|error| read_error(error.to_string()))?;
    credentials_token(&credentials, &crate::http::client_for_env(&options.env)).await
}

/// Exchange a parsed credentials file for an access token and its lifetime in seconds.
pub(crate) async fn credentials_token(
    credentials: &Value,
    client: &reqwest::Client,
) -> Result<(AccessToken, i64), String> {
    let field = |key: &str| credentials[key].as_str().filter(|value| !value.is_empty());
    let quota_project = field("quota_project_id").map(str::to_string);
    let form: Vec<(&str, String)> = match field("type") {
        Some("authorized_user") => {
            let client_id = field("client_id").ok_or("The incoming JSON object does not contain a client_id field")?;
            let secret =
                field("client_secret").ok_or("The incoming JSON object does not contain a client_secret field")?;
            let refresh =
                field("refresh_token").ok_or("The incoming JSON object does not contain a refresh_token field")?;
            vec![
                ("client_id", client_id.to_string()),
                ("client_secret", secret.to_string()),
                ("refresh_token", refresh.to_string()),
                ("grant_type", "refresh_token".to_string()),
            ]
        }
        Some(kind @ ("external_account" | "external_account_authorized_user" | "impersonated_service_account")) => {
            return Err(format!("Unsupported Google credential type for Vertex AI: {kind}"));
        }
        _ => {
            let email =
                field("client_email").ok_or("The incoming JSON object does not contain a client_email field")?;
            let key = field("private_key").ok_or("The incoming JSON object does not contain a private_key field")?;
            let audience = field("token_uri").unwrap_or(GOOGLE_TOKEN_URL);
            let assertion = sign_assertion(email, key, audience, now_ms() / 1000)?;
            vec![("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string()), ("assertion", assertion)]
        }
    };
    let token_url = field("token_uri").unwrap_or(GOOGLE_TOKEN_URL);
    let response = client
        .post(token_url)
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("Google token request failed: {}", error_chain(&error)))?;
    let (token, expires_in) = parse_token_response(response).await?;
    Ok((AccessToken { token, quota_project }, expires_in))
}

async fn metadata_token(host: &str) -> Result<(AccessToken, i64), String> {
    let url = format!("http://{host}/computeMetadata/v1/instance/service-accounts/default/token");
    let response = http_client()
        .get(url)
        .header("Metadata-Flavor", "Google")
        .query(&[("scopes", CLOUD_PLATFORM_SCOPE)])
        .send()
        .await
        .map_err(|error| format!("{NO_ADC_FOUND} Metadata server request failed: {}", error_chain(&error)))?;
    let (token, expires_in) = parse_token_response(response).await?;
    Ok((AccessToken { token, quota_project: None }, expires_in))
}

async fn parse_token_response(response: reqwest::Response) -> Result<(String, i64), String> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if !status.is_success() {
        let detail = match (parsed["error"].as_str(), parsed["error_description"].as_str()) {
            (Some(error), Some(description)) => format!("{error}: {description}"),
            (Some(error), None) => error.to_string(),
            _ => body.trim().to_string(),
        };
        return Err(format!("Failed to refresh Google access token ({}): {detail}", status.as_u16()));
    }
    let token = parsed["access_token"].as_str().filter(|token| !token.is_empty());
    let token = token.ok_or("Google token response did not include an access_token")?;
    Ok((token.to_string(), parsed["expires_in"].as_i64().unwrap_or(3600)))
}

/// DER bytes of the first PEM block in `pem`.
fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .skip_while(|line| !line.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|line| !line.starts_with("-----END"))
        .collect();
    if body.is_empty() {
        return Err("private_key is not a PEM encoded key".into());
    }
    STANDARD.decode(body).map_err(|error| format!("private_key is not valid base64: {error}"))
}

/// An RS256-signed JWT bearer assertion for the service account token exchange.
pub(crate) fn sign_assertion(email: &str, private_key: &str, audience: &str, issued_at: i64) -> Result<String, String> {
    let key = RsaKeyPair::from_pkcs8(&pem_to_der(private_key)?)
        .map_err(|error| format!("private_key is not a PKCS#8 RSA key: {error}"))?;
    let header = json!({"alg": "RS256"});
    let claims = json!({
        "iss": email, "scope": CLOUD_PLATFORM_SCOPE, "aud": audience, "exp": issued_at + 3600, "iat": issued_at,
    });
    let encode = |value: &Value| URL_SAFE_NO_PAD.encode(value.to_string());
    let signing_input = format!("{}.{}", encode(&header), encode(&claims));
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), signing_input.as_bytes(), &mut signature)
        .map_err(|_| "Failed to sign the service account assertion".to_string())?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature)))
}
