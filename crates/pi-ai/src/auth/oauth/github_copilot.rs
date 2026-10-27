//! GitHub Copilot OAuth flow: GitHub device code, then a Copilot token exchange.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::device_code::{DeviceCodePollOptions, DevicePoll, poll_device_code_flow};
use super::github_copilot_models::{
    COPILOT_HEADERS, RetryPolicy, enable_models, fetch_models, github_copilot_base_url,
};
use super::http::{Hooks, Reply, send, trusted_http_url};
use super::interact;
use super::pkce::decode_constant;
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

const CLIENT_ID_ENCODED: &str = "SXYxLmI1MDdhMDhjODdlY2ZlOTg=";
const USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

pub(crate) type KnownModel = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// GitHub Copilot login for github.com or a GitHub Enterprise domain.
pub struct GitHubCopilotOAuth {
    pub(crate) hooks: Hooks,
    /// Whether a model id is in the Copilot catalog; only those get their policy enabled.
    pub(crate) known_model: KnownModel,
}

impl Default for GitHubCopilotOAuth {
    fn default() -> Self {
        let known_model: KnownModel = Arc::new(|id| {
            crate::models::builtin_models().iter().any(|model| model.provider == "github-copilot" && model.id == id)
        });
        Self { hooks: Hooks::default(), known_model }
    }
}

struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<f64>,
    expires_in: f64,
}

/// The host of a GitHub Enterprise URL or bare domain.
pub(crate) fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let url = if trimmed.contains("://") { Url::parse(trimmed) } else { Url::parse(&format!("https://{trimmed}")) };
    url.ok()?.host_str().filter(|host| !host.is_empty()).map(str::to_string)
}

fn device_code_url(domain: &str) -> String {
    format!("https://{domain}/login/device/code")
}

fn access_token_url(domain: &str) -> String {
    format!("https://{domain}/login/oauth/access_token")
}

fn copilot_token_url(domain: &str) -> String {
    format!("https://api.{domain}/copilot_internal/v2/token")
}

fn enterprise_domain(credential: &OAuthCredential) -> Option<String> {
    credential.extra_str("enterpriseUrl").filter(|url| !url.is_empty()).and_then(normalize_domain)
}

/// `fetchJson`: the parsed body of a successful response.
fn json_body(reply: Reply) -> AuthResult<Value> {
    if !reply.ok() {
        return Err(AuthError::other(format!("{} {}: {}", reply.status, reply.status_text(), reply.body)));
    }
    reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))
}

impl GitHubCopilotOAuth {
    async fn post_form(&self, url: &str, form: &[(&str, &str)], cancel: &CancellationToken) -> AuthResult<Value> {
        let request =
            self.hooks.post(url).header("Accept", "application/json").header("User-Agent", USER_AGENT).form(form);
        json_body(send(request, cancel, None).await?)
    }

    async fn start_device_flow(&self, domain: &str, cancel: &CancellationToken) -> AuthResult<DeviceCode> {
        let client_id = decode_constant(CLIENT_ID_ENCODED);
        let data = self
            .post_form(&device_code_url(domain), &[("client_id", &client_id), ("scope", "read:user")], cancel)
            .await?;
        let Value::Object(data) = data else { return Err(AuthError::other("Invalid device code response")) };
        let string = |key: &str| data.get(key).and_then(Value::as_str).map(str::to_string);
        let interval = data.get("interval");
        let fields = (string("device_code"), string("user_code"), string("verification_uri"));
        let expires_in = data.get("expires_in").and_then(Value::as_f64);
        let (Some(device_code), Some(user_code), Some(verification_uri), Some(expires_in)) =
            (fields.0, fields.1, fields.2, expires_in)
        else {
            return Err(AuthError::other("Invalid device code response fields"));
        };
        if interval.is_some_and(|interval| !interval.is_number()) {
            return Err(AuthError::other("Invalid device code response fields"));
        }
        let Some(verification_uri) = trusted_http_url(Some(&Value::String(verification_uri))) else {
            return Err(AuthError::other("Untrusted verification_uri in device code response"));
        };
        Ok(DeviceCode {
            device_code,
            user_code,
            verification_uri,
            interval: interval.and_then(Value::as_f64),
            expires_in,
        })
    }

    async fn poll_access_token(
        &self,
        domain: &str,
        device: &DeviceCode,
        cancel: &CancellationToken,
    ) -> AuthResult<String> {
        let options = DeviceCodePollOptions {
            interval_seconds: device.interval,
            expires_in_seconds: Some(device.expires_in),
            wait_before_first_poll: true,
        };
        let client_id = decode_constant(CLIENT_ID_ENCODED);
        let url = access_token_url(domain);
        poll_device_code_flow(options, cancel, || async {
            let form = [
                ("client_id", client_id.as_str()),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ];
            let raw = self.post_form(&url, &form, cancel).await?;
            if let Some(token) = raw.get("access_token").and_then(Value::as_str) {
                return Ok(DevicePoll::Complete(token.to_string()));
            }
            let Some(error) = raw.get("error").and_then(Value::as_str) else {
                return Ok(DevicePoll::Failed("Invalid device token response".into()));
            };
            Ok(match error {
                "authorization_pending" => DevicePoll::Pending,
                "slow_down" => DevicePoll::SlowDown { interval_seconds: raw.get("interval").and_then(Value::as_f64) },
                _ => {
                    let description =
                        raw.get("error_description").and_then(Value::as_str).filter(|text| !text.is_empty());
                    let suffix = description.map(|text| format!(": {text}")).unwrap_or_default();
                    DevicePoll::Failed(format!("Device flow failed: {error}{suffix}"))
                }
            })
        })
        .await
    }

    /// Exchanges a GitHub token for a Copilot token.
    async fn copilot_token(
        &self,
        github_token: &str,
        enterprise_domain: Option<&str>,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let url = copilot_token_url(enterprise_domain.unwrap_or("github.com"));
        let request = COPILOT_HEADERS
            .iter()
            .fold(self.hooks.get(&url).header("Accept", "application/json"), |request, (name, value)| {
                request.header(*name, *value)
            })
            .header("Authorization", format!("Bearer {github_token}"));
        let Value::Object(raw) = json_body(send(request, cancel, None).await?)? else {
            return Err(AuthError::other("Invalid Copilot token response"));
        };
        let token = raw.get("token").and_then(Value::as_str);
        let expires_at = raw.get("expires_at").and_then(Value::as_f64);
        let (Some(token), Some(expires_at)) = (token, expires_at) else {
            return Err(AuthError::other("Invalid Copilot token response fields"));
        };
        let mut extra = Map::new();
        if let Some(domain) = enterprise_domain {
            extra.insert("enterpriseUrl".into(), Value::String(domain.into()));
        }
        Ok(OAuthCredential {
            refresh: github_token.into(),
            access: token.into(),
            expires: (expires_at * 1000.0).round() as i64 - EXPIRY_MARGIN_MS,
            extra,
        })
    }

    async fn login_copilot(
        &self,
        interaction: &dyn AuthInteraction,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let prompt = AuthPrompt::Text {
            message: "GitHub Enterprise URL/domain (blank for github.com)".into(),
            placeholder: Some("company.ghe.com".into()),
        };
        let input = interact::prompt(interaction, prompt, cancel).await?;
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let enterprise = normalize_domain(&input);
        if !input.trim().is_empty() && enterprise.is_none() {
            return Err(AuthError::other("Invalid GitHub Enterprise URL/domain"));
        }
        let domain = enterprise.as_deref().unwrap_or("github.com");

        let device = self.start_device_flow(domain, cancel).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval.map(|seconds| seconds as u64),
            expires_in_seconds: Some(device.expires_in as u64),
        });
        let github_token = self.poll_access_token(domain, &device, cancel).await?;
        let mut credential = self.copilot_token(&github_token, enterprise.as_deref(), cancel).await?;
        let policy = RetryPolicy { max_retries: 2, max_elapsed: Duration::from_millis(5000) };
        let known = self.known_model.as_ref();
        let catalog =
            fetch_models(&self.hooks, &credential.access, enterprise.as_deref(), known, cancel, policy).await?;
        let mut enabled = Vec::new();
        if !catalog.policy_model_ids.is_empty() {
            interaction.notify(AuthEvent::Progress { message: "Enabling models...".into() });
            let ids = &catalog.policy_model_ids;
            enabled = enable_models(&self.hooks, &credential.access, ids, enterprise.as_deref(), cancel).await?;
        }
        let mut available: Vec<String> = Vec::new();
        for id in catalog.available_model_ids.into_iter().chain(enabled) {
            if !available.contains(&id) {
                available.push(id);
            }
        }
        set_available_models(&mut credential, available);
        Ok(credential)
    }
}

fn set_available_models(credential: &mut OAuthCredential, ids: Vec<String>) {
    let ids = ids.into_iter().map(Value::String).collect();
    credential.extra.insert("availableModelIds".into(), Value::Array(ids));
}

#[async_trait]
impl OAuthAuth for GitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        self.login_copilot(interaction, &cancel).await
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        let enterprise = enterprise_domain(credential);
        let mut refreshed = self.copilot_token(&credential.refresh, enterprise.as_deref(), &cancel).await?;
        let policy = RetryPolicy { max_retries: 0, max_elapsed: Duration::ZERO };
        let known = self.known_model.as_ref();
        let catalog =
            fetch_models(&self.hooks, &refreshed.access, enterprise.as_deref(), known, &cancel, policy).await?;
        set_available_models(&mut refreshed, catalog.available_model_ids);
        Ok(refreshed)
    }

    /// Request auth with the credential's own proxy endpoint as the base URL.
    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        let enterprise = enterprise_domain(credential);
        ModelAuth {
            api_key: Some(credential.access.clone()),
            base_url: Some(github_copilot_base_url(Some(&credential.access), enterprise.as_deref())),
            ..ModelAuth::default()
        }
    }
}
