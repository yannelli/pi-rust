//! Radius gateway OAuth flow. OAuth client APIs live on the configured gateway; only the
//! interactive browser authorization endpoint is discovered.

use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use reqwest::Url;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use super::callback_server::{CallbackServerOptions, start_callback_server};
use super::device_code::{DeviceCodePollOptions, DevicePoll, poll_device_code_flow};
use super::http::{Hooks, SendError, expires_at, send};
use super::interact;
use super::pkce::{generate_pkce, random_uuid};
use super::radius_config::{gateway_url, normalize_radius_gateway_url};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 1456;
const CALLBACK_PATH: &str = "/oauth/callback";
const TOKEN_EXPIRY_SKEW_MS: i64 = 60_000;
const LOGIN_METHOD_BROWSER: &str = "browser";
const LOGIN_METHOD_DEVICE_CODE: &str = "device-code";
const OAUTH_CLIENT_ID: &str = "pi-gateway";
const OAUTH_SCOPE: &str = "gateway offline_access";
const OAUTH_DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Radius gateway login, created per gateway as upstream's `createRadiusOAuth`.
pub struct RadiusOAuth {
    name: String,
    gateway: String,
    pub(crate) hooks: Hooks,
    pub(crate) callback_port: u16,
}

impl RadiusOAuth {
    /// A login named `name` (for example `Radius`) against `gateway`, with or without a scheme.
    pub fn new(name: impl Into<String>, gateway: &str) -> Self {
        Self {
            name: name.into(),
            gateway: normalize_radius_gateway_url(gateway),
            hooks: Hooks::default(),
            callback_port: CALLBACK_PORT,
        }
    }

    pub fn gateway(&self) -> &str {
        &self.gateway
    }

    fn redirect_uri(&self) -> String {
        format!("http://{CALLBACK_HOST}:{}{CALLBACK_PATH}", self.callback_port)
    }
}

/// A token endpoint failure that keeps the OAuth `error` code for device polling.
enum TokenError {
    OAuth { oauth_error: Option<String>, message: String },
    Auth(AuthError),
}

impl From<TokenError> for AuthError {
    fn from(error: TokenError) -> Self {
        match error {
            TokenError::OAuth { message, .. } => AuthError::Other(message),
            TokenError::Auth(error) => error,
        }
    }
}

fn oauth_response_error(status: u16, body: &str, message: &str) -> TokenError {
    let (mut oauth_error, mut description) = (None, None);
    if !body.is_empty() {
        match serde_json::from_str::<Value>(body) {
            Ok(data) => {
                oauth_error = data.get("error").and_then(Value::as_str).map(str::to_string);
                description = data.get("error_description").and_then(Value::as_str).map(str::to_string);
            }
            Err(_) => description = Some(body.to_string()),
        }
    }
    let detail = match (&oauth_error, description.filter(|text| !text.is_empty())) {
        (Some(error), Some(description)) if !error.is_empty() => format!("{error}: {description}"),
        (Some(error), _) if !error.is_empty() => error.clone(),
        (_, Some(description)) => description,
        _ => status.to_string(),
    };
    TokenError::OAuth { oauth_error, message: format!("{message}: {detail}") }
}

fn send_error(error: SendError) -> AuthError {
    error.into()
}

impl RadiusOAuth {
    async fn request_token(
        &self,
        form: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<OAuthCredential, TokenError> {
        let url = gateway_url(&self.gateway, "/v1/oauth/token").map_err(TokenError::Auth)?;
        let request = self.hooks.post(&url).header("accept", "application/json").form(form);
        let reply = send(request, cancel, None).await.map_err(|error| TokenError::Auth(send_error(error)))?;
        if !reply.ok() {
            return Err(oauth_response_error(reply.status, &reply.body, "Radius OAuth token request failed"));
        }
        let data = reply.json().map_err(|error| TokenError::Auth(AuthError::other(format!("SyntaxError: {error}"))))?;
        let access = data.get("access_token").and_then(Value::as_str);
        let expires_in = data.get("expires_in").and_then(Value::as_f64);
        let (Some(access), Some(expires_in)) = (access, expires_in) else {
            return Err(TokenError::Auth(AuthError::other("Radius OAuth token response is missing required fields")));
        };
        let mut extra = Map::new();
        if let Some(scope) = data.get("scope").filter(|scope| !scope.is_null()) {
            extra.insert("scope".into(), scope.clone());
        }
        Ok(OAuthCredential {
            access: access.into(),
            refresh: data.get("refresh_token").and_then(Value::as_str).unwrap_or_default().into(),
            expires: expires_at(expires_in, TOKEN_EXPIRY_SKEW_MS),
            extra,
        })
    }

    async fn discover_authorization_endpoint(&self, cancel: &CancellationToken) -> AuthResult<String> {
        let url = gateway_url(&self.gateway, "/v1/oauth")?;
        let reply = send(self.hooks.get(&url).header("accept", "application/json"), cancel, None).await?;
        let gateway = &self.gateway;
        if !reply.ok() {
            return Err(AuthError::other(format!(
                "Could not load Radius OAuth config from {gateway}: {} {}",
                reply.status, reply.body
            )));
        }
        let discovery = reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))?;
        let endpoint = discovery.get("authorizationEndpoint").and_then(Value::as_str);
        endpoint
            .map(str::to_string)
            .ok_or_else(|| AuthError::other(format!("Invalid Radius OAuth config from {gateway}")))
    }

    async fn login_with_browser(
        &self,
        authorization_endpoint: &str,
        interaction: &dyn AuthInteraction,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let pkce = generate_pkce();
        let state = random_uuid();
        let redirect_uri = self.redirect_uri();
        let mut authorize_url = Url::parse(authorization_endpoint)
            .map_err(|error| AuthError::other(format!("Invalid URL {authorization_endpoint}: {error}")))?;
        authorize_url
            .query_pairs_mut()
            .clear()
            .append_pair("response_type", "code")
            .append_pair("client_id", OAUTH_CLIENT_ID)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", OAUTH_SCOPE)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("handoff", "url")
            .append_pair("state", &state);

        let flow = Arc::new(RadiusOAuth {
            name: self.name.clone(),
            gateway: self.gateway.clone(),
            hooks: self.hooks.clone(),
            callback_port: self.callback_port,
        });
        let (verifier, exchange_redirect, exchange_cancel) =
            (pkce.verifier.clone(), redirect_uri.clone(), cancel.clone());
        let callback = start_callback_server(CallbackServerOptions {
            provider_name: "Radius".into(),
            host: CALLBACK_HOST.into(),
            port: self.callback_port,
            path: CALLBACK_PATH.into(),
            redirect_host: None,
            state: Some(state),
            complete: Arc::new(move |code| {
                let (flow, verifier, redirect, cancel) =
                    (flow.clone(), verifier.clone(), exchange_redirect.clone(), exchange_cancel.clone());
                async move {
                    let form = [
                        ("grant_type", "authorization_code"),
                        ("client_id", OAUTH_CLIENT_ID),
                        ("redirect_uri", redirect.as_str()),
                        ("code", code.as_str()),
                        ("code_verifier", verifier.as_str()),
                    ];
                    flow.request_token(&form, &cancel).await.map_err(AuthError::from)
                }
                .boxed()
            }),
            cancel: Some(cancel.clone()),
            timeout: None,
        })
        .await?;
        interaction.notify(AuthEvent::Progress { message: format!("Listening for OAuth callback on {redirect_uri}") });
        interaction.notify(AuthEvent::AuthUrl {
            url: authorize_url.to_string(),
            instructions: Some("Continue in your browser.".into()),
        });
        let credential = callback.wait().await?;
        credential.ok_or_else(|| AuthError::other("OAuth callback did not complete."))
    }

    async fn login_with_device_code(
        &self,
        interaction: &dyn AuthInteraction,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let url = gateway_url(&self.gateway, "/v1/oauth/device")?;
        let form = [("client_id", OAUTH_CLIENT_ID), ("scope", OAUTH_SCOPE)];
        let request = self.hooks.post(&url).header("accept", "application/json").form(&form);
        let reply = send(request, cancel, None).await?;
        if !reply.ok() {
            return Err(
                oauth_response_error(reply.status, &reply.body, "Radius OAuth device authorization failed").into()
            );
        }
        let data = reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))?;
        let text =
            |key: &str| data.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string);
        let expires_in = data.get("expires_in").and_then(Value::as_f64).filter(|value| *value != 0.0);
        let (Some(device_code), Some(user_code), Some(verification_uri), Some(expires_in)) =
            (text("device_code"), text("user_code"), text("verification_uri"), expires_in)
        else {
            return Err(AuthError::other("Radius OAuth device authorization response is missing required fields"));
        };
        let interval = data.get("interval").and_then(Value::as_f64);
        interaction.notify(AuthEvent::DeviceCode {
            user_code,
            verification_uri,
            interval_seconds: interval.map(|seconds| seconds as u64),
            expires_in_seconds: Some(expires_in as u64),
        });

        let options = DeviceCodePollOptions {
            interval_seconds: interval,
            expires_in_seconds: Some(expires_in),
            wait_before_first_poll: false,
        };
        poll_device_code_flow(options, cancel, || async {
            let form = [
                ("grant_type", OAUTH_DEVICE_CODE_GRANT_TYPE),
                ("client_id", OAUTH_CLIENT_ID),
                ("device_code", device_code.as_str()),
            ];
            match self.request_token(&form, cancel).await {
                Ok(credential) => Ok(DevicePoll::Complete(credential)),
                Err(TokenError::OAuth { oauth_error, message }) => match oauth_error.as_deref() {
                    Some("authorization_pending") => Ok(DevicePoll::Pending),
                    Some("slow_down") => Ok(DevicePoll::SlowDown { interval_seconds: None }),
                    Some("expired_token") => Ok(DevicePoll::Failed("Device authorization expired.".into())),
                    Some("access_denied") => Ok(DevicePoll::Failed("Device authorization was denied.".into())),
                    _ => Err(AuthError::Other(message)),
                },
                Err(TokenError::Auth(error)) => Err(error),
            }
        })
        .await
    }
}

#[async_trait]
impl OAuthAuth for RadiusOAuth {
    fn name(&self) -> &str {
        &self.name
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let methods = [
            (LOGIN_METHOD_BROWSER, "Sign in with browser (recommended)"),
            (LOGIN_METHOD_DEVICE_CODE, "Sign in with device code (when signing in from another device)"),
        ];
        let message = format!("Sign in to {}:", self.name);
        let method = interact::select(interaction, &message, &methods, &cancel).await?;
        match method.as_str() {
            LOGIN_METHOD_DEVICE_CODE => self.login_with_device_code(interaction, &cancel).await,
            LOGIN_METHOD_BROWSER => {
                let endpoint = self.discover_authorization_endpoint(&cancel).await?;
                self.login_with_browser(&endpoint, interaction, &cancel).await
            }
            other => Err(AuthError::other(format!("Unknown {} sign-in method: {other}", self.name))),
        }
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", OAUTH_CLIENT_ID),
            ("refresh_token", credential.refresh.as_str()),
        ];
        Ok(self.request_token(&form, &cancel).await?)
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
