//! Meta Model API OAuth flow. The device grant yields an identity token that inference rejects,
//! so it is exchanged for a day-long Model API key. `refresh` holds the identity token (which
//! cannot be renewed) and `access` the key, so a refresh re-mints the key.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::device_code::{DeviceCodePollOptions, DevicePoll, poll_device_code_flow};
use super::http::{Hooks, Reply, json_text, non_empty_str, now_ms, positive_number, send, trusted_http_url};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

// Muse Code CLI client id.
const CLIENT_ID: &str = "1031625952748946";
const DEVICE_AUTHORIZATION_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
const DEVICE_TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
const API_KEY_MINT_URL: &str = "https://api.meta.ai/muse-code/key";
const API_KEY_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Meta (Muse subscription) login.
#[derive(Default)]
pub struct MetaOAuth {
    pub(crate) hooks: Hooks,
}

struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: Option<f64>,
    expires_in_seconds: Option<f64>,
}

fn read_json(reply: &Reply) -> Option<Value> {
    reply.json().ok().filter(|json| json.is_object() || json.is_array())
}

fn field<'a>(json: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    json.and_then(|json| json.get(key))
}

fn error_detail(json: Option<&Value>) -> String {
    ["error_description", "detail", "message", "error"]
        .iter()
        .find_map(|key| field(json, key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()))
        .map(|text| format!(": {text}"))
        .unwrap_or_default()
}

impl MetaOAuth {
    async fn post_form(&self, url: &str, form: &[(&str, &str)], cancel: &CancellationToken) -> AuthResult<Reply> {
        let request = self.hooks.post(url).header("Accept", "application/json").form(form);
        Ok(send(request, cancel, self.hooks.timeout(REQUEST_TIMEOUT)).await?)
    }

    async fn start_device_authorization(&self, cancel: &CancellationToken) -> AuthResult<DeviceAuthorization> {
        let reply = self.post_form(DEVICE_AUTHORIZATION_URL, &[("client_id", CLIENT_ID)], cancel).await?;
        let json = read_json(&reply);
        if !reply.ok() {
            let detail = error_detail(json.as_ref());
            return Err(AuthError::other(format!(
                "Meta device authorization failed with status {}{detail}",
                reply.status
            )));
        }
        let verification_uri = trusted_http_url(field(json.as_ref(), "verification_uri_complete"))
            .or_else(|| trusted_http_url(field(json.as_ref(), "verification_uri")));
        let device_code = non_empty_str(field(json.as_ref(), "device_code"));
        let user_code = non_empty_str(field(json.as_ref(), "user_code"));
        let (Some(device_code), Some(user_code), Some(verification_uri)) = (device_code, user_code, verification_uri)
        else {
            let json = json_text(json.as_ref());
            return Err(AuthError::other(format!("Invalid Meta device authorization response: {json}")));
        };
        Ok(DeviceAuthorization {
            device_code: device_code.into(),
            user_code: user_code.into(),
            verification_uri,
            interval_seconds: positive_number(field(json.as_ref(), "interval")),
            expires_in_seconds: positive_number(field(json.as_ref(), "expires_in")),
        })
    }

    async fn poll_for_identity_token(
        &self,
        device: &DeviceAuthorization,
        cancel: &CancellationToken,
    ) -> AuthResult<String> {
        let options = DeviceCodePollOptions {
            interval_seconds: device.interval_seconds,
            expires_in_seconds: device.expires_in_seconds,
            wait_before_first_poll: true,
        };
        poll_device_code_flow(options, cancel, || async {
            let form = [
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device.device_code.as_str()),
                ("client_id", CLIENT_ID),
            ];
            let reply = self.post_form(DEVICE_TOKEN_URL, &form, cancel).await?;
            let json = read_json(&reply);
            if reply.ok()
                && let Some(token) = non_empty_str(field(json.as_ref(), "access_token"))
            {
                return Ok(DevicePoll::Complete(token.to_string()));
            }
            Ok(match field(json.as_ref(), "error").and_then(Value::as_str) {
                Some("authorization_pending") => DevicePoll::Pending,
                Some("slow_down") => {
                    DevicePoll::SlowDown { interval_seconds: positive_number(field(json.as_ref(), "interval")) }
                }
                Some("access_denied") => DevicePoll::Failed("Meta login was denied.".into()),
                Some("expired_token") => {
                    DevicePoll::Failed("Meta device authorization expired. Please restart login.".into())
                }
                _ => DevicePoll::Failed(format!(
                    "Meta device token request failed with status {}{}",
                    reply.status,
                    error_detail(json.as_ref())
                )),
            })
        })
        .await
    }

    /// Exchanges an identity token for a Model API key.
    async fn mint_api_key(&self, identity_token: &str, cancel: &CancellationToken) -> AuthResult<OAuthCredential> {
        let request = self
            .hooks
            .post(API_KEY_MINT_URL)
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {identity_token}"))
            .header("Content-Type", "application/json")
            .header("x-api-version", "1.0.0")
            .body("{}");
        let reply = send(request, cancel, self.hooks.timeout(REQUEST_TIMEOUT)).await?;
        let json = read_json(&reply);
        let detail = error_detail(json.as_ref());
        if reply.status == 401 || reply.status == 403 {
            // The identity token cannot be renewed; only a fresh device login helps.
            return Err(AuthError::other(format!(
                "Meta session expired (status {}). Run `/login meta` to sign in again.{detail}",
                reply.status
            )));
        }
        if !reply.ok() {
            return Err(AuthError::other(format!("Meta API key mint failed with status {}{detail}", reply.status)));
        }
        let Some(api_key) = non_empty_str(field(json.as_ref(), "api_key")) else {
            let setup = trusted_http_url(field(json.as_ref(), "action_url"))
                .map(|url| format!(" Complete setup at {url}"))
                .unwrap_or_default();
            return Err(AuthError::other(format!("Meta did not issue an API key.{setup}")));
        };
        Ok(OAuthCredential {
            refresh: identity_token.into(),
            access: api_key.into(),
            expires: now_ms() + API_KEY_LIFETIME_MS,
            extra: Map::new(),
        })
    }

    async fn login_meta(
        &self,
        interaction: &dyn AuthInteraction,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let device = self.start_device_authorization(cancel).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri.clone(),
            interval_seconds: device.interval_seconds.map(|seconds| seconds as u64),
            expires_in_seconds: device.expires_in_seconds.map(|seconds| seconds as u64),
        });
        let identity_token = self.poll_for_identity_token(&device, cancel).await?;
        interaction.notify(AuthEvent::Progress { message: "Enabling Meta Model API access...".into() });
        self.mint_api_key(&identity_token, cancel).await
    }
}

#[async_trait]
impl OAuthAuth for MetaOAuth {
    fn name(&self) -> &str {
        "Meta (Muse subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Meta")
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let result = self.login_meta(interaction, &cancel).await;
        result.map_err(|error| if cancel.is_cancelled() { AuthError::Cancelled } else { error })
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        self.mint_api_key(&credential.refresh, &cancel).await
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
