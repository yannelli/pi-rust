//! Kimi Code (subscription) OAuth flow: an RFC 8628 device grant against auth.kimi.com.
//! The access token authenticates requests to api.kimi.com/coding as a Bearer header.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::device_code::{DeviceCodePollOptions, DevicePoll, poll_device_code_flow};
use super::http::{Hooks, Reply, SendError, expires_at, json_text, positive_number, send, trusted_http_url};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_OAUTH_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_MAX_RETRIES: u32 = 3;

/// Kimi Code subscription login.
#[derive(Default)]
pub struct KimiCodingOAuth {
    pub(crate) hooks: Hooks,
}

struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    interval_seconds: f64,
    expires_in_seconds: f64,
}

/// A JSON body that is an object or array, as upstream's `readJson`.
fn read_json(reply: &Reply) -> Option<Value> {
    reply.json().ok().filter(|json| json.is_object() || json.is_array())
}

fn field<'a>(json: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    json.and_then(|json| json.get(key))
}

fn parse_token(json: Option<&Value>, operation: &str) -> AuthResult<OAuthCredential> {
    let access = field(json, "access_token").and_then(Value::as_str).filter(|token| !token.is_empty());
    let refresh = field(json, "refresh_token").and_then(Value::as_str).filter(|token| !token.is_empty());
    match (access, refresh, positive_number(field(json, "expires_in"))) {
        (Some(access), Some(refresh), Some(expires_in)) => Ok(OAuthCredential {
            access: access.into(),
            refresh: refresh.into(),
            expires: expires_at(expires_in, 0),
            extra: Map::new(),
        }),
        _ => Err(AuthError::other(format!("Kimi Code token {operation} response missing fields: {}", json_text(json)))),
    }
}

fn with_body(prefix: String, body: &str) -> String {
    if body.is_empty() { prefix } else { format!("{prefix}: {body}") }
}

impl KimiCodingOAuth {
    fn oauth_host(&self) -> String {
        let host = self.hooks.env("KIMI_CODE_OAUTH_HOST").or_else(|| self.hooks.env("KIMI_OAUTH_HOST"));
        host.unwrap_or_else(|| DEFAULT_OAUTH_HOST.into()).trim_end_matches('/').to_string()
    }

    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<Reply, SendError> {
        let request = self.hooks.post(url).header("Accept", "application/json").form(form);
        send(request, cancel, self.hooks.timeout(REQUEST_TIMEOUT)).await
    }

    async fn start_device_authorization(
        &self,
        host: &str,
        cancel: &CancellationToken,
    ) -> AuthResult<DeviceAuthorization> {
        let url = format!("{host}/api/oauth/device_authorization");
        let reply = self.post_form(&url, &[("client_id", CLIENT_ID)], cancel).await?;
        if !reply.ok() {
            let message = format!("Kimi Code device authorization failed with status {}", reply.status);
            return Err(AuthError::other(with_body(message, &reply.body)));
        }
        let json = read_json(&reply);
        let string = |key: &str| field(json.as_ref(), key).and_then(Value::as_str).map(str::to_string);
        let (device_code, user_code) = (string("device_code"), string("user_code"));
        let uri = trusted_http_url(field(json.as_ref(), "verification_uri"));
        let complete = trusted_http_url(field(json.as_ref(), "verification_uri_complete"));
        let (Some(device_code), Some(user_code), Some(_), Some(complete)) = (device_code, user_code, uri, complete)
        else {
            let json = json_text(json.as_ref());
            return Err(AuthError::other(format!("Invalid Kimi Code device authorization response: {json}")));
        };
        Ok(DeviceAuthorization {
            device_code,
            user_code,
            verification_uri_complete: complete,
            interval_seconds: positive_number(field(json.as_ref(), "interval"))
                .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
            expires_in_seconds: positive_number(field(json.as_ref(), "expires_in"))
                .unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS),
        })
    }

    async fn poll_for_token(
        &self,
        host: &str,
        device: &DeviceAuthorization,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let options = DeviceCodePollOptions {
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(device.expires_in_seconds),
            wait_before_first_poll: true,
        };
        let url = format!("{host}/api/oauth/token");
        poll_device_code_flow(options, cancel, || async {
            let form = [
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ];
            let reply = self.post_form(&url, &form, cancel).await?;
            if reply.status >= 500 {
                let message = format!("Kimi Code device token request failed with status {}", reply.status);
                return Ok(DevicePoll::Failed(with_body(message, &reply.body)));
            }
            let json = read_json(&reply);
            if reply.ok() && field(json.as_ref(), "access_token").is_some_and(Value::is_string) {
                return Ok(match parse_token(json.as_ref(), "poll") {
                    Ok(credential) => DevicePoll::Complete(credential),
                    Err(error) => DevicePoll::Failed(error.to_string()),
                });
            }
            let error = field(json.as_ref(), "error").and_then(Value::as_str);
            Ok(match error {
                Some("authorization_pending") => DevicePoll::Pending,
                Some("slow_down") => {
                    DevicePoll::SlowDown { interval_seconds: positive_number(field(json.as_ref(), "interval")) }
                }
                Some("expired_token") => {
                    DevicePoll::Failed("Kimi Code device authorization expired. Please restart login.".into())
                }
                Some("access_denied") => DevicePoll::Failed("Kimi Code login was denied.".into()),
                _ => {
                    let description = field(json.as_ref(), "error_description").and_then(Value::as_str);
                    let detail = match error {
                        Some(error) => {
                            format!(": {error}{}", description.map(|text| format!(": {text}")).unwrap_or_default())
                        }
                        None => String::new(),
                    };
                    DevicePoll::Failed(format!(
                        "Kimi Code device token request failed (status {}){detail}",
                        reply.status
                    ))
                }
            })
        })
        .await
    }

    async fn refresh_token(
        &self,
        host: &str,
        refresh: &str,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let url = format!("{host}/api/oauth/token");
        let mut last_error: Option<AuthError> = None;
        for attempt in 0..=REFRESH_MAX_RETRIES {
            if attempt > 0 {
                super::http::sleep(Duration::from_millis(1000 * 2u64.pow(attempt - 1)), cancel).await?;
            }
            if cancel.is_cancelled() {
                return Err(AuthError::other("Kimi Code token refresh aborted"));
            }
            let form = [("client_id", CLIENT_ID), ("grant_type", "refresh_token"), ("refresh_token", refresh)];
            let reply = match self.post_form(&url, &form, cancel).await {
                Ok(reply) => reply,
                Err(SendError::Cancelled) => return Err(AuthError::Cancelled),
                Err(error) => {
                    last_error = Some(AuthError::Http(error.to_string()));
                    continue;
                }
            };
            let json = read_json(&reply);
            if reply.ok() {
                return parse_token(json.as_ref(), "refresh");
            }
            // Unauthorized: the stored credential is dead and the user must sign in again.
            let error = field(json.as_ref(), "error").and_then(Value::as_str);
            if reply.status == 401 || reply.status == 403 || error == Some("invalid_grant") {
                let description = field(json.as_ref(), "error_description").and_then(Value::as_str);
                let description = description.map(|text| format!(": {text}")).unwrap_or_default();
                return Err(AuthError::other(format!(
                    "Kimi Code token refresh unauthorized (status {}){description}",
                    reply.status
                )));
            }
            if (reply.status == 429 || reply.status >= 500) && attempt < REFRESH_MAX_RETRIES {
                last_error =
                    Some(AuthError::other(format!("Kimi Code token refresh failed with status {}", reply.status)));
                continue;
            }
            let text = json_text(json.as_ref());
            return Err(AuthError::other(format!(
                "Kimi Code token refresh failed with status {}: {text}",
                reply.status
            )));
        }
        Err(last_error.unwrap_or_else(|| AuthError::other("Kimi Code token refresh failed")))
    }
}

#[async_trait]
impl OAuthAuth for KimiCodingOAuth {
    fn name(&self) -> &str {
        "Kimi Code (subscription)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login_label(&self) -> Option<&str> {
        Some("Sign in with Kimi Code")
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let host = self.oauth_host();
        let device = self.start_device_authorization(&host, &cancel).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: device.verification_uri_complete.clone(),
            interval_seconds: Some(device.interval_seconds as u64),
            expires_in_seconds: Some(device.expires_in_seconds as u64),
        });
        self.poll_for_token(&host, &device, &cancel).await
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        self.refresh_token(&self.oauth_host(), &credential.refresh, &cancel).await
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        let headers = BTreeMap::from([("Authorization".to_string(), format!("Bearer {}", credential.access))]);
        ModelAuth { headers, ..ModelAuth::default() }
    }
}
