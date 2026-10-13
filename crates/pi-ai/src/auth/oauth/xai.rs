//! xAI OAuth device-code flow (SuperGrok or X Premium).

use async_trait::async_trait;
use reqwest::Url;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::device_code::{DeviceCodePollOptions, DevicePoll, poll_device_code_flow};
use super::http::{Hooks, SendError, expires_at, send};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
// Refresh slightly before the reported expiry so a token does not die mid-request.
const REFRESH_SKEW_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TOKEN_LIFETIME_SECONDS: f64 = 3600.0;

/// xAI login for Grok and X subscriptions.
#[derive(Default)]
pub struct XaiOAuth {
    pub(crate) hooks: Hooks,
}

struct OAuthReply {
    ok: bool,
    status: u16,
    body: Map<String, Value>,
}

struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    interval_seconds: Option<f64>,
    expires_in_seconds: f64,
}

fn required_string(body: &Map<String, Value>, field: &str) -> AuthResult<String> {
    match body.get(field).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        _ => Err(AuthError::other(format!("Invalid xAI OAuth response field: {field}"))),
    }
}

fn positive_number(body: &Map<String, Value>, field: &str) -> AuthResult<f64> {
    super::http::positive_number(body.get(field))
        .ok_or_else(|| AuthError::other(format!("Invalid xAI OAuth response field: {field}")))
}

// The verification URI is opened in the browser; only https URLs are allowed.
fn validate_verification_uri(raw: &str) -> AuthResult<String> {
    let untrusted = || AuthError::other("Untrusted verification URI in xAI OAuth response");
    let url = Url::parse(raw).map_err(|_| untrusted())?;
    if url.scheme() != "https" {
        return Err(untrusted());
    }
    Ok(url.to_string())
}

fn request_failure(action: &str, reply: &OAuthReply) -> AuthError {
    let detail: Vec<&str> = ["error", "error_description"]
        .iter()
        .filter_map(|key| reply.body.get(*key).and_then(Value::as_str).filter(|text| !text.is_empty()))
        .collect();
    let detail = if detail.is_empty() { String::new() } else { format!(": {}", detail.join(": ")) };
    AuthError::other(format!("xAI OAuth {action} failed (HTTP {}){detail}", reply.status))
}

fn parse_device_code(body: &Map<String, Value>) -> AuthResult<DeviceCode> {
    // RFC 8628 allows interval 0; fall back to the poller default for non-positive values.
    let interval_seconds = super::http::positive_number(body.get("interval"));
    let verification_uri_complete = match body.get("verification_uri_complete").and_then(Value::as_str) {
        Some(uri) if !uri.is_empty() => Some(validate_verification_uri(uri)?),
        _ => None,
    };
    Ok(DeviceCode {
        device_code: required_string(body, "device_code")?,
        user_code: required_string(body, "user_code")?,
        verification_uri: validate_verification_uri(&required_string(body, "verification_uri")?)?,
        verification_uri_complete,
        interval_seconds,
        expires_in_seconds: positive_number(body, "expires_in")?,
    })
}

fn credential_from_token(body: &Map<String, Value>, previous_refresh: Option<&str>) -> AuthResult<OAuthCredential> {
    let access = required_string(body, "access_token")?;
    // xAI may omit refresh_token on refresh when the token is not rotated.
    let refresh = match (body.get("refresh_token"), previous_refresh.filter(|token| !token.is_empty())) {
        (None, Some(previous)) => previous.to_string(),
        _ => required_string(body, "refresh_token")?,
    };
    let expires_in = match body.get("expires_in") {
        None => DEFAULT_TOKEN_LIFETIME_SECONDS,
        Some(_) => positive_number(body, "expires_in")?,
    };
    Ok(OAuthCredential { access, refresh, expires: expires_at(expires_in, REFRESH_SKEW_MS), extra: Map::new() })
}

impl XaiOAuth {
    async fn post_form(&self, url: &str, form: &[(&str, &str)], cancel: &CancellationToken) -> AuthResult<OAuthReply> {
        let request = self.hooks.post(url).header("Accept", "application/json").form(form);
        let reply = match send(request, cancel, None).await {
            Ok(reply) => reply,
            Err(SendError::Cancelled) => return Err(AuthError::Cancelled),
            Err(error) => return Err(AuthError::Http(error.to_string())),
        };
        let body = match reply.json() {
            Ok(Value::Object(body)) => body,
            Ok(_) => Map::new(),
            Err(_) => return Err(AuthError::other(format!("xAI OAuth returned invalid JSON (HTTP {})", reply.status))),
        };
        Ok(OAuthReply { ok: reply.ok(), status: reply.status, body })
    }

    async fn request_device_code(&self, cancel: &CancellationToken) -> AuthResult<DeviceCode> {
        let form = [("client_id", CLIENT_ID), ("scope", SCOPE), ("referrer", "pi")];
        let reply = self.post_form(DEVICE_CODE_URL, &form, cancel).await?;
        if !reply.ok {
            return Err(request_failure("device authorization", &reply));
        }
        parse_device_code(&reply.body)
    }

    async fn poll_for_tokens(&self, device: &DeviceCode, cancel: &CancellationToken) -> AuthResult<OAuthCredential> {
        let options = DeviceCodePollOptions {
            interval_seconds: device.interval_seconds,
            expires_in_seconds: Some(device.expires_in_seconds),
            wait_before_first_poll: true,
        };
        poll_device_code_flow(options, cancel, || async {
            let form = [
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
            ];
            let reply = self.post_form(TOKEN_URL, &form, cancel).await?;
            if reply.ok {
                return Ok(DevicePoll::Complete(credential_from_token(&reply.body, None)?));
            }
            Ok(match reply.body.get("error").and_then(Value::as_str) {
                Some("authorization_pending") => DevicePoll::Pending,
                Some("slow_down") => {
                    DevicePoll::SlowDown { interval_seconds: reply.body.get("interval").and_then(Value::as_f64) }
                }
                Some("access_denied" | "authorization_denied") => {
                    DevicePoll::Failed("xAI device authorization was denied".into())
                }
                Some("expired_token") => DevicePoll::Failed("xAI device code expired".into()),
                _ => DevicePoll::Failed(request_failure("device token polling", &reply).to_string()),
            })
        })
        .await
    }
}

#[async_trait]
impl OAuthAuth for XaiOAuth {
    fn name(&self) -> &str {
        "xAI (Grok/X subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with SuperGrok or X Premium")
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let device = self.request_device_code(&cancel).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device
                .verification_uri_complete
                .clone()
                .unwrap_or_else(|| device.verification_uri.clone()),
            interval_seconds: device.interval_seconds.map(|seconds| seconds as u64),
            expires_in_seconds: Some(device.expires_in_seconds as u64),
        });
        self.poll_for_tokens(&device, &cancel).await
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        let form =
            [("grant_type", "refresh_token"), ("client_id", CLIENT_ID), ("refresh_token", credential.refresh.as_str())];
        let reply = self.post_form(TOKEN_URL, &form, &cancel).await?;
        if !reply.ok {
            return Err(request_failure("token refresh", &reply));
        }
        credential_from_token(&reply.body, Some(&credential.refresh))
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
