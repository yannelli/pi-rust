//! OpenAI Codex OAuth flow (ChatGPT Plus/Pro) with browser and device-code login.

use std::sync::Arc;

use async_trait::async_trait;
use futures::FutureExt;
use reqwest::Url;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::callback_server::{
    CallbackOrManual, CallbackServerOptions, start_callback_server, wait_for_callback_or_manual_input,
};
use super::device_code::{DeviceCodePollOptions, DevicePoll, poll_device_code_flow};
use super::http::{Hooks, Reply, SendError, decode_jwt_payload, expires_at, parse_authorization_input, send};
use super::interact;
use super::pkce::{generate_pkce, random_hex};
use crate::auth::{
    AuthError, AuthEvent, AuthInteraction, AuthResult, LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
const BROWSER_LOGIN_METHOD: &str = "browser";
const DEVICE_CODE_LOGIN_METHOD: &str = "device_code";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

/// OpenAI Codex login for ChatGPT Plus/Pro subscriptions.
pub struct OpenAICodexOAuth {
    pub(crate) hooks: Hooks,
    /// Port 1455 is registered with OpenAI and shared with the Codex CLI.
    pub(crate) callback_port: u16,
}

impl Default for OpenAICodexOAuth {
    fn default() -> Self {
        Self { hooks: Hooks::default(), callback_port: CALLBACK_PORT }
    }
}

struct Token {
    access: String,
    refresh: String,
    expires: i64,
}

struct DeviceAuth {
    device_auth_id: String,
    user_code: String,
    interval_seconds: f64,
}

fn login_error(error: SendError) -> AuthError {
    error.into()
}

fn read_token_response(reply: Reply, operation: &str) -> AuthResult<Token> {
    if !reply.ok() {
        let text = if reply.body.is_empty() { reply.status_text().to_string() } else { reply.body.clone() };
        return Err(AuthError::other(format!("OpenAI Codex token {operation} failed ({}): {text}", reply.status)));
    }
    let json = reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))?;
    let access = json.get("access_token").and_then(Value::as_str).filter(|value| !value.is_empty());
    let refresh = json.get("refresh_token").and_then(Value::as_str).filter(|value| !value.is_empty());
    let expires_in = json.get("expires_in").and_then(Value::as_f64);
    match (access, refresh, expires_in) {
        (Some(access), Some(refresh), Some(expires_in)) => {
            Ok(Token { access: access.into(), refresh: refresh.into(), expires: expires_at(expires_in, 0) })
        }
        _ => Err(AuthError::other(format!("OpenAI Codex token {operation} response missing fields: {json}"))),
    }
}

/// `chatgpt_account_id` from the access token's OpenAI auth claim.
pub(crate) fn account_id(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token)?;
    let id = payload.get(JWT_CLAIM_PATH)?.get("chatgpt_account_id")?.as_str()?;
    (!id.is_empty()).then(|| id.to_string())
}

fn credential_from_token(token: Token) -> AuthResult<OAuthCredential> {
    let account_id =
        account_id(&token.access).ok_or_else(|| AuthError::other("Failed to extract accountId from token"))?;
    let mut extra = Map::new();
    extra.insert("accountId".into(), Value::String(account_id));
    Ok(OAuthCredential { access: token.access, refresh: token.refresh, expires: token.expires, extra })
}

/// The device auth poll interval: a number or a numeric string.
fn interval_value(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if text.trim().is_empty() => Some(0.0),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

impl OpenAICodexOAuth {
    async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ];
        let reply = send(self.hooks.post(TOKEN_URL).form(&form), cancel, None).await.map_err(login_error)?;
        credential_from_token(read_token_response(reply, "exchange")?)
    }

    async fn start_device_auth(&self, cancel: &CancellationToken) -> AuthResult<DeviceAuth> {
        let request = self.hooks.post(DEVICE_USER_CODE_URL).json(&json!({ "client_id": CLIENT_ID }));
        let reply = send(request, cancel, None).await.map_err(login_error)?;
        if !reply.ok() {
            if reply.status == 404 {
                return Err(AuthError::other(
                    "OpenAI Codex device code login is not enabled for this server. Use browser login or verify the server URL.",
                ));
            }
            let suffix = if reply.body.is_empty() { String::new() } else { format!(": {}", reply.body) };
            return Err(AuthError::other(format!(
                "OpenAI Codex device code request failed with status {}{suffix}",
                reply.status
            )));
        }
        let json = reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))?;
        let device_auth_id = json.get("device_auth_id").and_then(Value::as_str).filter(|value| !value.is_empty());
        let user_code = json.get("user_code").and_then(Value::as_str).filter(|value| !value.is_empty());
        let interval = interval_value(json.get("interval")).filter(|value| value.is_finite() && *value >= 0.0);
        match (device_auth_id, user_code, interval) {
            (Some(device_auth_id), Some(user_code), Some(interval_seconds)) => {
                Ok(DeviceAuth { device_auth_id: device_auth_id.into(), user_code: user_code.into(), interval_seconds })
            }
            _ => Err(AuthError::other(format!("Invalid OpenAI Codex device code response: {json}"))),
        }
    }

    async fn poll_device_auth(&self, device: &DeviceAuth, cancel: &CancellationToken) -> AuthResult<(String, String)> {
        let options = DeviceCodePollOptions {
            interval_seconds: Some(device.interval_seconds),
            expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS as f64),
            wait_before_first_poll: false,
        };
        poll_device_code_flow(options, cancel, || async {
            let body = json!({ "device_auth_id": device.device_auth_id, "user_code": device.user_code });
            let reply = send(self.hooks.post(DEVICE_TOKEN_URL).json(&body), cancel, None).await.map_err(login_error)?;
            if reply.ok() {
                let json = reply.json().map_err(|error| AuthError::other(format!("SyntaxError: {error}")))?;
                let code = json.get("authorization_code").and_then(Value::as_str).filter(|value| !value.is_empty());
                let verifier = json.get("code_verifier").and_then(Value::as_str).filter(|value| !value.is_empty());
                return Ok(match (code, verifier) {
                    (Some(code), Some(verifier)) => DevicePoll::Complete((code.to_string(), verifier.to_string())),
                    _ => DevicePoll::Failed(format!("Invalid OpenAI Codex device auth token response: {json}")),
                });
            }
            if reply.status == 403 || reply.status == 404 {
                return Ok(DevicePoll::Pending);
            }
            let error = reply.json().ok().and_then(|json| json.get("error").cloned());
            let code = match &error {
                Some(Value::Object(object)) => object.get("code").and_then(Value::as_str).map(str::to_string),
                Some(Value::String(code)) => Some(code.clone()),
                _ => None,
            };
            Ok(match code.as_deref() {
                Some("deviceauth_authorization_pending") => DevicePoll::Pending,
                Some("slow_down") => DevicePoll::SlowDown { interval_seconds: None },
                _ => {
                    let suffix = if reply.body.is_empty() { String::new() } else { format!(": {}", reply.body) };
                    DevicePoll::Failed(format!("OpenAI Codex device auth failed with status {}{suffix}", reply.status))
                }
            })
        })
        .await
    }

    async fn login_device_code(
        &self,
        interaction: &dyn AuthInteraction,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let device = self.start_device_auth(cancel).await?;
        interaction.notify(AuthEvent::DeviceCode {
            user_code: device.user_code.clone(),
            verification_uri: DEVICE_VERIFICATION_URI.into(),
            interval_seconds: Some(device.interval_seconds as u64),
            expires_in_seconds: Some(DEVICE_CODE_TIMEOUT_SECONDS),
        });
        let (code, verifier) = self.poll_device_auth(&device, cancel).await?;
        self.exchange_code(&code, &verifier, DEVICE_REDIRECT_URI, cancel).await
    }

    async fn login_browser(
        &self,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
        cancel: &CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let pkce = generate_pkce();
        let state = random_hex::<16>();
        let mut url = Url::parse(AUTHORIZE_URL).expect("static URL parses");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("scope", SCOPE)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", options.agent_name.as_deref().unwrap_or("pi"));
        // When the port is taken (for example by the Codex CLI), fall back to the pasted redirect URL.
        let callback = start_callback_server(CallbackServerOptions {
            provider_name: "OpenAI".into(),
            host: self.hooks.callback_host(),
            port: self.callback_port,
            path: CALLBACK_PATH.into(),
            redirect_host: None,
            state: Some(state.clone()),
            complete: Arc::new(|code| async move { Ok(code) }.boxed()),
            cancel: Some(cancel.clone()),
            timeout: None,
        })
        .await
        .ok();
        interaction.notify(AuthEvent::AuthUrl {
            url: url.to_string(),
            instructions: Some("A browser window should open. Complete login to finish.".into()),
        });

        let result = wait_for_callback_or_manual_input(
            interaction,
            callback.as_ref(),
            "Complete login in your browser, or paste the authorization code / redirect URL here:",
            REDIRECT_URI,
            cancel,
        )
        .await?;
        let code = match result {
            CallbackOrManual::Callback(code) => Some(code),
            CallbackOrManual::Manual(input) => {
                let parsed = parse_authorization_input(&input);
                if parsed.state.as_deref().is_some_and(|parsed| !parsed.is_empty() && parsed != state) {
                    return Err(AuthError::other("State mismatch"));
                }
                parsed.code
            }
        };
        let code =
            code.filter(|code| !code.is_empty()).ok_or_else(|| AuthError::other("Missing authorization code"))?;
        self.exchange_code(&code, &pkce.verifier, REDIRECT_URI, cancel).await
    }
}

#[async_trait]
impl OAuthAuth for OpenAICodexOAuth {
    fn name(&self) -> &str {
        "OpenAI (ChatGPT Plus/Pro)"
    }

    fn is_subscription(&self) -> bool {
        true
    }

    async fn login(
        &self,
        interaction: &dyn AuthInteraction,
        options: &LoginOptions,
        cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        let methods = [
            (BROWSER_LOGIN_METHOD, "Browser login (default)"),
            (DEVICE_CODE_LOGIN_METHOD, "Device code login (headless)"),
        ];
        let method = interact::select(interaction, "Select OpenAI Codex login method:", &methods, &cancel).await?;
        match method.as_str() {
            DEVICE_CODE_LOGIN_METHOD => self.login_device_code(interaction, &cancel).await,
            BROWSER_LOGIN_METHOD => self.login_browser(interaction, options, &cancel).await,
            other => Err(AuthError::other(format!("Unknown OpenAI Codex login method: {other}"))),
        }
    }

    async fn refresh(&self, credential: &OAuthCredential, cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        let form =
            [("grant_type", "refresh_token"), ("refresh_token", credential.refresh.as_str()), ("client_id", CLIENT_ID)];
        let reply = match send(self.hooks.post(TOKEN_URL).form(&form), &cancel, None).await {
            Ok(reply) => reply,
            Err(SendError::Cancelled) => return Err(AuthError::Cancelled),
            Err(error) => return Err(AuthError::other(format!("OpenAI Codex token refresh error: {error}"))),
        };
        credential_from_token(read_token_response(reply, "refresh")?)
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}
